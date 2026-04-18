use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use async_recursion::async_recursion;
use brush_parser::ast::{
    AndOr, ArithmeticExpr, ArithmeticTarget, BinaryOperator, CaseClauseCommand, CaseItemPostAction,
    Command, CommandPrefixOrSuffixItem, CompoundCommand, CompoundList, CompoundListItem,
    ElseClause, ForClauseCommand, FunctionBody, FunctionDefinition, IfClauseCommand, IoFd,
    IoFileRedirectKind, IoFileRedirectTarget, IoHereDocument, IoRedirect, Pipeline,
    PipelineOperator, Program, SeparatorOperator, SimpleCommand, UnaryAssignmentOperator,
    UnaryOperator, WhileOrUntilClauseCommand, Word,
};
use brush_parser::word::{
    Parameter, ParameterExpr, ParameterTestType, SpecialParameter, WordPiece, WordPieceWithSource,
};
use futures::channel::oneshot;
use glob::{MatchOptions, Pattern};

use crate::builtin;
use crate::error::{Result, ShellError};
use crate::parser;
use crate::platform::{ResolvedProgram, RunningProcess, ShellPlatform, SpawnRequest, WriteMode};
use crate::streams::{InputStream, OutputStream, capture_output};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variable {
    pub value: String,
    pub exported: bool,
}

#[derive(Clone, Debug)]
pub struct Bootstrap {
    pub shell_name: String,
    pub positional_parameters: Vec<String>,
    pub working_dir: PathBuf,
    pub environment: Vec<(String, String)>,
    pub process_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandStatus {
    code: u8,
    exiting: bool,
}

impl CommandStatus {
    pub const SUCCESS: Self = Self {
        code: 0,
        exiting: false,
    };

    pub const fn new(code: u8) -> Self {
        Self {
            code,
            exiting: false,
        }
    }

    pub const fn exit(code: u8) -> Self {
        Self {
            code,
            exiting: true,
        }
    }

    pub const fn code(self) -> u8 {
        self.code
    }

    pub const fn is_success(self) -> bool {
        self.code == 0
    }

    pub const fn is_exit(self) -> bool {
        self.exiting
    }
}

pub struct Shell<P> {
    pub(crate) platform: P,
    pub(crate) variables: HashMap<String, Variable>,
    pub(crate) functions: HashMap<String, FunctionDefinition>,
    pub(crate) shell_name: String,
    pub(crate) process_id: String,
    pub(crate) positional_parameters: Vec<String>,
    pub(crate) working_dir: PathBuf,
    pub(crate) last_status: u8,
    pub(crate) last_pipeline_statuses: Vec<u8>,
    pub(crate) last_background_job: Option<u64>,
    pub(crate) next_background_job: u64,
    pub(crate) background_jobs: Rc<RefCell<HashMap<u64, oneshot::Receiver<Result<CommandStatus>>>>>,
}

#[derive(Clone)]
pub(crate) struct ExecutionIo {
    pub(crate) inputs: HashMap<IoFd, InputStream>,
    pub(crate) outputs: HashMap<IoFd, OutputStream>,
}

impl ExecutionIo {
    fn new(stdin: InputStream, stdout: OutputStream, stderr: OutputStream) -> Self {
        let mut inputs = HashMap::new();
        inputs.insert(0, stdin);

        let mut outputs = HashMap::new();
        outputs.insert(1, stdout);
        outputs.insert(2, stderr);

        Self { inputs, outputs }
    }

    pub(crate) fn input(&self, fd: IoFd) -> InputStream {
        self.inputs
            .get(&fd)
            .cloned()
            .unwrap_or_else(|| InputStream::closed(fd))
    }

    pub(crate) fn output(&self, fd: IoFd) -> OutputStream {
        self.outputs
            .get(&fd)
            .cloned()
            .unwrap_or_else(|| OutputStream::closed(fd))
    }

    pub(crate) fn set_input(&mut self, fd: IoFd, input: InputStream) {
        self.inputs.insert(fd, input);
    }

    pub(crate) fn set_output(&mut self, fd: IoFd, output: OutputStream) {
        self.outputs.insert(fd, output);
    }

    pub(crate) fn close_input(&mut self, fd: IoFd) {
        self.inputs.remove(&fd);
    }

    pub(crate) fn close_output(&mut self, fd: IoFd) {
        self.outputs.remove(&fd);
    }
}

struct StageHandle {
    result: oneshot::Receiver<Result<CommandStatus>>,
}

impl StageHandle {
    async fn wait(self) -> Result<CommandStatus> {
        self.result
            .await
            .map_err(|_| ShellError::message("pipeline stage dropped"))?
    }
}

impl<P> Shell<P>
where
    P: ShellPlatform,
{
    pub fn new(platform: P, bootstrap: Bootstrap) -> Self {
        let mut variables = HashMap::new();
        for (name, value) in bootstrap.environment {
            variables.insert(
                name,
                Variable {
                    value,
                    exported: true,
                },
            );
        }
        variables
            .entry("PATH".to_owned())
            .or_insert_with(|| Variable {
                value: "/bin".to_owned(),
                exported: true,
            });
        variables.insert(
            "PWD".to_owned(),
            Variable {
                value: display_path(&bootstrap.working_dir),
                exported: true,
            },
        );

        Self {
            platform,
            variables,
            functions: HashMap::new(),
            shell_name: bootstrap.shell_name,
            process_id: bootstrap.process_id,
            positional_parameters: bootstrap.positional_parameters,
            working_dir: bootstrap.working_dir,
            last_status: 0,
            last_pipeline_statuses: vec![0],
            last_background_job: None,
            next_background_job: 1,
            background_jobs: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn set_variable(&mut self, name: String, value: String, exported: Option<bool>) {
        match self.variables.get_mut(&name) {
            Some(existing) => {
                existing.value = value;
                if let Some(exported) = exported {
                    existing.exported = exported;
                }
            }
            None => {
                self.variables.insert(
                    name,
                    Variable {
                        value,
                        exported: exported.unwrap_or(false),
                    },
                );
            }
        }
    }

    pub fn mark_exported(&mut self, name: &str) {
        if let Some(variable) = self.variables.get_mut(name) {
            variable.exported = true;
            return;
        }

        self.variables.insert(
            name.to_owned(),
            Variable {
                value: String::new(),
                exported: true,
            },
        );
    }

    pub fn unset(&mut self, name: &str) {
        self.variables.remove(name);
    }

    pub fn variable(&self, name: &str) -> Option<&Variable> {
        self.variables.get(name)
    }

    pub fn exported_environment(&self) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        for (name, variable) in &self.variables {
            if variable.exported {
                pairs.push((name.clone(), variable.value.clone()));
            }
        }
        pairs
    }

    pub fn set_working_dir(&mut self, working_dir: PathBuf) {
        let oldpwd = self.working_dir.clone();
        self.working_dir = working_dir.clone();
        self.set_variable("PWD".to_owned(), display_path(&working_dir), Some(true));
        self.set_variable("OLDPWD".to_owned(), display_path(&oldpwd), Some(true));
    }

    pub fn current_dir(&self) -> &Path {
        &self.working_dir
    }

    pub(crate) fn resolve_user_path(&self, path: &str) -> PathBuf {
        resolve_path(&self.working_dir, Path::new(path))
    }

    fn new_io(&self) -> ExecutionIo {
        ExecutionIo::new(
            InputStream::empty(),
            self.platform.stdout(),
            self.platform.stderr(),
        )
    }

    fn fork(&self) -> Self {
        Self {
            platform: self.platform.clone(),
            variables: self.variables.clone(),
            functions: self.functions.clone(),
            shell_name: self.shell_name.clone(),
            process_id: self.process_id.clone(),
            positional_parameters: self.positional_parameters.clone(),
            working_dir: self.working_dir.clone(),
            last_status: self.last_status,
            last_pipeline_statuses: self.last_pipeline_statuses.clone(),
            last_background_job: self.last_background_job,
            next_background_job: self.next_background_job,
            background_jobs: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    #[async_recursion(?Send)]
    pub async fn run_program(&mut self, program: &Program) -> Result<CommandStatus> {
        self.run_program_with_io(program, self.new_io()).await
    }

    #[async_recursion(?Send)]
    async fn run_program_with_io(
        &mut self,
        program: &Program,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        let mut last = CommandStatus::SUCCESS;
        for complete_command in &program.complete_commands {
            last = self.run_compound_list(complete_command, io.clone()).await?;
            if last.is_exit() {
                self.last_status = last.code();
                return Ok(last);
            }
        }
        self.last_status = last.code();
        Ok(last)
    }

    #[async_recursion(?Send)]
    async fn run_compound_list(
        &mut self,
        list: &CompoundList,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        let mut last = CommandStatus::SUCCESS;
        for CompoundListItem(and_or, separator) in &list.0 {
            if matches!(separator, SeparatorOperator::Async) {
                self.spawn_background(and_or.clone(), io.clone());
                last = CommandStatus::SUCCESS;
                continue;
            }
            last = self.run_and_or(and_or, io.clone()).await?;
            if last.is_exit() {
                return Ok(last);
            }
        }
        Ok(last)
    }

    #[async_recursion(?Send)]
    async fn run_and_or(
        &mut self,
        and_or: &brush_parser::ast::AndOrList,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        let mut last = self.run_pipeline(&and_or.first, io.clone()).await?;
        if last.is_exit() {
            return Ok(last);
        }

        for link in &and_or.additional {
            let (operator, pipeline) = match link {
                AndOr::And(pipeline) => (PipelineOperator::And, pipeline),
                AndOr::Or(pipeline) => (PipelineOperator::Or, pipeline),
            };
            let should_run = match operator {
                PipelineOperator::And => last.is_success(),
                PipelineOperator::Or => !last.is_success(),
            };
            if !should_run {
                continue;
            }

            last = self.run_pipeline(pipeline, io.clone()).await?;
            if last.is_exit() {
                return Ok(last);
            }
        }

        Ok(last)
    }

    #[async_recursion(?Send)]
    async fn run_pipeline(
        &mut self,
        pipeline: &Pipeline,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        if pipeline.timed.is_some() {
            return Err(ShellError::unsupported("pipeline timing"));
        }

        if pipeline.seq.len() == 1 {
            let status = self.run_command(&pipeline.seq[0], io).await?;
            self.last_pipeline_statuses = vec![status.code()];
            self.last_status = status.code();
            return Ok(apply_pipeline_bang(status, pipeline.bang));
        }

        let mut stages = Vec::with_capacity(pipeline.seq.len());
        let mut next_input = None;
        let last_index = pipeline.seq.len() - 1;

        for (index, command) in pipeline.seq.iter().enumerate() {
            let stdin = if index == 0 {
                io.input(0)
            } else {
                next_input
                    .take()
                    .expect("pipeline stage must have upstream input")
            };
            let stdout = if index == last_index {
                io.output(1)
            } else {
                let (reader, writer) = self.platform.pipe();
                next_input = Some(reader);
                writer
            };
            let mut stage_io = io.clone();
            stage_io.set_input(0, stdin);
            stage_io.set_output(1, stdout);
            stage_io.set_output(2, io.output(2));
            stages.push(self.spawn_pipeline_stage(command.clone(), stage_io));
        }

        let mut statuses = Vec::with_capacity(stages.len());
        for stage in stages {
            statuses.push(stage.wait().await?);
        }

        self.last_pipeline_statuses = statuses.iter().map(|status| status.code()).collect();
        let status = statuses
            .last()
            .copied()
            .ok_or_else(|| ShellError::message("pipeline unexpectedly had no stages"))?;
        self.last_status = status.code();
        Ok(apply_pipeline_bang(status, pipeline.bang))
    }

    fn spawn_pipeline_stage(&self, command: Command, io: ExecutionIo) -> StageHandle {
        let mut shell = self.fork();
        let (sender, receiver) = oneshot::channel();
        self.platform.spawn_task(Box::pin(async move {
            let result = shell.run_command(&command, io).await;
            let _ = sender.send(result);
        }));
        StageHandle { result: receiver }
    }

    fn spawn_background(&mut self, and_or: brush_parser::ast::AndOrList, io: ExecutionIo) {
        let mut shell = self.fork();
        let background_job = self.next_background_job;
        self.next_background_job += 1;
        self.last_background_job = Some(background_job);
        let (sender, receiver) = oneshot::channel();
        self.background_jobs
            .borrow_mut()
            .insert(background_job, receiver);

        let mut background_io = io;
        background_io.set_input(0, InputStream::empty());
        self.platform.spawn_task(Box::pin(async move {
            let result = shell.run_and_or(&and_or, background_io).await;
            let _ = sender.send(result);
        }));
    }

    pub(crate) fn take_background_job(
        &mut self,
        id: u64,
    ) -> Option<oneshot::Receiver<Result<CommandStatus>>> {
        self.background_jobs.borrow_mut().remove(&id)
    }

    pub(crate) fn take_all_background_jobs(
        &mut self,
    ) -> Vec<(u64, oneshot::Receiver<Result<CommandStatus>>)> {
        let mut jobs = self
            .background_jobs
            .borrow_mut()
            .drain()
            .collect::<Vec<_>>();
        jobs.sort_by_key(|(id, _)| *id);
        jobs
    }

    #[async_recursion(?Send)]
    async fn run_command(&mut self, command: &Command, io: ExecutionIo) -> Result<CommandStatus> {
        match command {
            Command::Simple(simple) => self.run_simple(simple, io).await,
            Command::Compound(compound, redirects) => {
                let io = if let Some(redirects) = redirects {
                    let expansion_io = io.clone();
                    self.apply_redirects(io, &redirects.0, &expansion_io)
                        .await?
                } else {
                    io
                };
                self.run_compound(compound, io).await
            }
            Command::Function(function) => {
                self.functions
                    .insert(function.fname.value.clone(), function.clone());
                Ok(CommandStatus::SUCCESS)
            }
            Command::ExtendedTest(_) => Err(ShellError::unsupported("[[ ... ]] expressions")),
        }
    }

    #[async_recursion(?Send)]
    async fn run_compound(
        &mut self,
        compound: &CompoundCommand,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        match compound {
            CompoundCommand::BraceGroup(group) => self.run_compound_list(&group.list, io).await,
            CompoundCommand::Subshell(subshell) => {
                let mut shell = self.fork();
                shell.run_compound_list(&subshell.list, io).await
            }
            CompoundCommand::IfClause(if_clause) => self.run_if(if_clause, io).await,
            CompoundCommand::WhileClause(while_clause) => {
                self.run_while_like(while_clause, false, io).await
            }
            CompoundCommand::UntilClause(until_clause) => {
                self.run_while_like(until_clause, true, io).await
            }
            CompoundCommand::ForClause(for_clause) => self.run_for(for_clause, io).await,
            CompoundCommand::Arithmetic(_) => Err(ShellError::unsupported("arithmetic commands")),
            CompoundCommand::ArithmeticForClause(_) => {
                Err(ShellError::unsupported("arithmetic for loops"))
            }
            CompoundCommand::CaseClause(case_clause) => self.run_case(case_clause, io).await,
        }
    }

    async fn run_if(
        &mut self,
        if_clause: &IfClauseCommand,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        let condition = self
            .run_compound_list(&if_clause.condition, io.clone())
            .await?;
        if condition.is_exit() {
            return Ok(condition);
        }

        if condition.is_success() {
            return self.run_compound_list(&if_clause.then, io).await;
        }

        let Some(elses) = &if_clause.elses else {
            return Ok(condition);
        };
        for else_clause in elses {
            let status = self.run_else_clause(else_clause, io.clone()).await?;
            if status.is_exit() || status.is_success() || else_clause.condition.is_none() {
                return Ok(status);
            }
        }

        Ok(condition)
    }

    async fn run_else_clause(
        &mut self,
        else_clause: &ElseClause,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        if let Some(condition) = &else_clause.condition {
            let status = self.run_compound_list(condition, io.clone()).await?;
            if status.is_exit() || !status.is_success() {
                return Ok(status);
            }
        }

        self.run_compound_list(&else_clause.body, io).await
    }

    async fn run_while_like(
        &mut self,
        clause: &WhileOrUntilClauseCommand,
        invert: bool,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        let mut last = CommandStatus::SUCCESS;
        loop {
            let condition = self.run_compound_list(&clause.0, io.clone()).await?;
            if condition.is_exit() {
                return Ok(condition);
            }
            let should_run = if invert {
                !condition.is_success()
            } else {
                condition.is_success()
            };
            if !should_run {
                return Ok(last);
            }
            last = self.run_compound_list(&clause.1.list, io.clone()).await?;
            if last.is_exit() {
                return Ok(last);
            }
        }
    }

    async fn run_for(
        &mut self,
        for_clause: &ForClauseCommand,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        let values = match &for_clause.values {
            Some(values) => {
                let mut expanded = Vec::with_capacity(values.len());
                for value in values {
                    expanded.push(self.expand_word(value, &io).await?);
                }
                expanded
            }
            None => self.positional_parameters.clone(),
        };

        let mut last = CommandStatus::SUCCESS;
        for value in values {
            self.set_variable(for_clause.variable_name.clone(), value, Some(false));
            last = self
                .run_compound_list(&for_clause.body.list, io.clone())
                .await?;
            if last.is_exit() {
                return Ok(last);
            }
        }

        Ok(last)
    }

    async fn run_case(
        &mut self,
        case_clause: &CaseClauseCommand,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        let value = self.expand_word(&case_clause.value, &io).await?;
        let mut result = CommandStatus::SUCCESS;
        let mut force_execute_next_case = false;

        for case in &case_clause.cases {
            let matched = if force_execute_next_case {
                force_execute_next_case = false;
                true
            } else {
                let mut matched = false;
                for pattern in &case.patterns {
                    let expanded_pattern = self.expand_pattern(pattern, &io).await?;
                    if shell_pattern_matches(&expanded_pattern, &value)? {
                        matched = true;
                        break;
                    }
                }
                matched
            };

            if !matched {
                continue;
            }

            if let Some(command) = &case.cmd {
                result = self.run_compound_list(command, io.clone()).await?;
                if result.is_exit() {
                    return Ok(result);
                }
            }

            match case.post_action {
                CaseItemPostAction::ExitCase => break,
                CaseItemPostAction::UnconditionallyExecuteNextCaseItem => {
                    force_execute_next_case = true;
                }
                CaseItemPostAction::ContinueEvaluatingCases => {}
            }
        }

        Ok(result)
    }

    async fn run_simple(
        &mut self,
        command: &SimpleCommand,
        mut io: ExecutionIo,
    ) -> Result<CommandStatus> {
        let mut assignments = Vec::new();
        let mut args = Vec::new();
        let mut redirects = Vec::new();

        if let Some(prefix) = &command.prefix {
            for item in &prefix.0 {
                match item {
                    CommandPrefixOrSuffixItem::AssignmentWord(assign, _) => {
                        assignments.push((
                            assign.name.to_string(),
                            self.expand_assignment_value(&assign.value, &io).await?,
                        ));
                    }
                    CommandPrefixOrSuffixItem::Word(word) => {
                        args.push(self.expand_word(word, &io).await?)
                    }
                    CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                        redirects.push(redirect.clone())
                    }
                    CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {
                        return Err(ShellError::unsupported("process substitution"));
                    }
                }
            }
        }

        if let Some(word) = &command.word_or_name {
            args.push(self.expand_word(word, &io).await?);
        }

        if let Some(suffix) = &command.suffix {
            for item in &suffix.0 {
                match item {
                    CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                        args.push(self.expand_word(word, &io).await?);
                    }
                    CommandPrefixOrSuffixItem::Word(word) => {
                        args.push(self.expand_word(word, &io).await?)
                    }
                    CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                        redirects.push(redirect.clone())
                    }
                    CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {
                        return Err(ShellError::unsupported("process substitution"));
                    }
                }
            }
        }

        let expansion_io = io.clone();
        io = self.apply_redirects(io, &redirects, &expansion_io).await?;

        if args.is_empty() {
            for (name, value) in assignments {
                self.set_variable(name, value, None);
            }
            self.last_status = 0;
            return Ok(CommandStatus::SUCCESS);
        }

        let mut program = args.remove(0);
        let mut arguments = args;
        let exec_requested = program == "exec";
        if exec_requested {
            if arguments.is_empty() {
                return Ok(CommandStatus::SUCCESS);
            }
            program = arguments.remove(0);
        }

        let special_builtin = is_special_builtin(&program);
        if special_builtin {
            for (name, value) in &assignments {
                self.set_variable(name.clone(), value.clone(), None);
            }
        }

        if builtin::is_builtin(&program) {
            let status = builtin::dispatch(self, &program, &arguments, io)
                .await?
                .ok_or_else(|| {
                    ShellError::message(format!("builtin {program:?} disappeared during dispatch"))
                })?;
            self.last_status = status.code();
            return Ok(if exec_requested {
                CommandStatus::exit(status.code())
            } else {
                status
            });
        }

        if let Some(function) = self.functions.get(&program).cloned() {
            let status = self.run_function(&function, arguments, io).await?;
            self.last_status = status.code();
            return Ok(if exec_requested {
                CommandStatus::exit(status.code())
            } else {
                status
            });
        }

        let status = self
            .run_external(program, arguments, assignments, io)
            .await?;
        self.last_status = status.code();
        Ok(if exec_requested {
            CommandStatus::exit(status.code())
        } else {
            status
        })
    }

    async fn run_function(
        &mut self,
        function: &FunctionDefinition,
        arguments: Vec<String>,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        let saved = self.positional_parameters.clone();
        self.positional_parameters = arguments;
        let status = self.run_function_body(&function.body, io).await;
        self.positional_parameters = saved;
        status
    }

    async fn run_function_body(
        &mut self,
        body: &FunctionBody,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        let io = if let Some(redirects) = &body.1 {
            let expansion_io = io.clone();
            self.apply_redirects(io, &redirects.0, &expansion_io)
                .await?
        } else {
            io
        };
        self.run_compound(&body.0, io).await
    }

    async fn run_external(
        &mut self,
        program: String,
        arguments: Vec<String>,
        assignments: Vec<(String, String)>,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        let resolved = self.resolve_program(&program).await?;
        let child = self
            .platform
            .spawn(SpawnRequest {
                resolved,
                args: arguments,
                env: overlay_environment(self.exported_environment(), assignments),
                stdin: io.input(0),
                stdout: io.output(1),
                stderr: io.output(2),
            })
            .await?;
        Ok(CommandStatus::new(child.wait().await?))
    }

    async fn resolve_program(&self, input: &str) -> Result<ResolvedProgram> {
        let search_path = self
            .variable("PATH")
            .map(|variable| variable.value.as_str())
            .unwrap_or("/bin");
        let mut errors = Vec::new();
        for candidate in candidate_paths(&self.working_dir, input, search_path) {
            match self.platform.read_file(&candidate).await {
                Ok(wasm) => {
                    return Ok(ResolvedProgram {
                        path: display_path(&candidate),
                        wasm,
                    });
                }
                Err(error) => errors.push(format!("{}: {error}", candidate.display())),
            }
        }

        Err(ShellError::message(format!(
            "failed to locate executable program {input:?}:\n{}",
            errors.join("\n")
        )))
    }

    async fn apply_redirects<'a, I>(
        &mut self,
        mut io: ExecutionIo,
        redirects: I,
        expansion_io: &ExecutionIo,
    ) -> Result<ExecutionIo>
    where
        I: IntoIterator<Item = &'a IoRedirect>,
    {
        for redirect in redirects {
            match redirect {
                IoRedirect::File(fd, kind, target) => {
                    let fd = effective_fd(*fd, kind);
                    match kind {
                        IoFileRedirectKind::Read => {
                            let path = self.resolve_redirect_path(target, expansion_io).await?;
                            io.set_input(fd, self.platform.open_input(&path).await?);
                        }
                        IoFileRedirectKind::Write | IoFileRedirectKind::Clobber => {
                            let path = self.resolve_redirect_path(target, expansion_io).await?;
                            io.set_output(
                                fd,
                                self.platform
                                    .open_output(&path, WriteMode::Truncate)
                                    .await?,
                            );
                        }
                        IoFileRedirectKind::Append => {
                            let path = self.resolve_redirect_path(target, expansion_io).await?;
                            io.set_output(
                                fd,
                                self.platform.open_output(&path, WriteMode::Append).await?,
                            );
                        }
                        IoFileRedirectKind::ReadAndWrite => {
                            let path = self.resolve_redirect_path(target, expansion_io).await?;
                            let (input, output) = self.platform.open_read_write(&path).await?;
                            io.set_input(fd, input);
                            io.set_output(fd, output);
                        }
                        IoFileRedirectKind::DuplicateInput => {
                            match self.resolve_duplicate_target(target, expansion_io).await? {
                                DuplicateTarget::Close => io.close_input(fd),
                                DuplicateTarget::Fd(source) => io.set_input(fd, io.input(source)),
                            }
                        }
                        IoFileRedirectKind::DuplicateOutput => {
                            match self.resolve_duplicate_target(target, expansion_io).await? {
                                DuplicateTarget::Close => io.close_output(fd),
                                DuplicateTarget::Fd(source) => {
                                    io.set_output(fd, io.output(source));
                                }
                            }
                        }
                    }
                }
                IoRedirect::HereDocument(fd, document) => {
                    let fd = fd.unwrap_or(0);
                    io.set_input(
                        fd,
                        InputStream::from_bytes(
                            self.render_here_document(document, expansion_io).await?,
                        ),
                    );
                }
                IoRedirect::HereString(fd, word) => {
                    let fd = fd.unwrap_or(0);
                    let mut bytes = self.expand_word(word, expansion_io).await?.into_bytes();
                    bytes.push(b'\n');
                    io.set_input(fd, InputStream::from_bytes(bytes));
                }
                IoRedirect::OutputAndError(word, append) => {
                    let expanded = self.expand_word(word, expansion_io).await?;
                    let path = resolve_path(&self.working_dir, &PathBuf::from(expanded));
                    let output = self
                        .platform
                        .open_output(
                            &path,
                            if *append {
                                WriteMode::Append
                            } else {
                                WriteMode::Truncate
                            },
                        )
                        .await?;
                    io.set_output(1, output.clone());
                    io.set_output(2, output);
                }
            }
        }
        Ok(io)
    }

    async fn resolve_redirect_path(
        &mut self,
        target: &IoFileRedirectTarget,
        io: &ExecutionIo,
    ) -> Result<PathBuf> {
        match target {
            IoFileRedirectTarget::Filename(word) => {
                let expanded = self.expand_word(word, io).await?;
                Ok(resolve_path(&self.working_dir, &PathBuf::from(expanded)))
            }
            IoFileRedirectTarget::ProcessSubstitution(_, _) => {
                Err(ShellError::unsupported("process substitution redirection"))
            }
            IoFileRedirectTarget::Fd(fd) => Err(ShellError::message(format!(
                "file descriptor target {fd} cannot be used as a filename"
            ))),
            IoFileRedirectTarget::Duplicate(word) => Err(ShellError::message(format!(
                "redirect target {:?} is not a filename",
                self.expand_word(word, io).await?
            ))),
        }
    }

    async fn resolve_duplicate_target(
        &mut self,
        target: &IoFileRedirectTarget,
        io: &ExecutionIo,
    ) -> Result<DuplicateTarget> {
        match target {
            IoFileRedirectTarget::Fd(fd) => Ok(DuplicateTarget::Fd(*fd)),
            IoFileRedirectTarget::Filename(word) | IoFileRedirectTarget::Duplicate(word) => {
                let value = self.expand_word(word, io).await?;
                if value == "-" {
                    return Ok(DuplicateTarget::Close);
                }
                let fd = value.parse::<IoFd>().map_err(|error| {
                    ShellError::message(format!(
                        "invalid file descriptor target {value:?}: {error}"
                    ))
                })?;
                Ok(DuplicateTarget::Fd(fd))
            }
            IoFileRedirectTarget::ProcessSubstitution(_, _) => {
                Err(ShellError::unsupported("process substitution redirection"))
            }
        }
    }

    async fn render_here_document(
        &mut self,
        document: &IoHereDocument,
        io: &ExecutionIo,
    ) -> Result<Vec<u8>> {
        let mut text = if document.requires_expansion {
            self.expand_word(&document.doc, io).await?
        } else {
            document.doc.value.clone()
        };
        if document.remove_tabs {
            text = text
                .lines()
                .map(|line| line.trim_start_matches('\t'))
                .collect::<Vec<_>>()
                .join("\n");
        }
        Ok(text.into_bytes())
    }

    async fn expand_word(&mut self, word: &Word, io: &ExecutionIo) -> Result<String> {
        self.expand_text(&word.value, io, ExpansionMode::Text, false)
            .await
    }

    async fn expand_pattern(&mut self, word: &Word, io: &ExecutionIo) -> Result<String> {
        self.expand_text(&word.value, io, ExpansionMode::Pattern, false)
            .await
    }

    async fn expand_assignment_value(
        &mut self,
        value: &brush_parser::ast::AssignmentValue,
        io: &ExecutionIo,
    ) -> Result<String> {
        use brush_parser::ast::AssignmentValue;
        match value {
            AssignmentValue::Scalar(word) => self.expand_word(word, io).await,
            AssignmentValue::Array(_) => Err(ShellError::unsupported("array assignments")),
        }
    }

    #[async_recursion(?Send)]
    async fn expand_text(
        &mut self,
        raw: &str,
        io: &ExecutionIo,
        mode: ExpansionMode,
        quoted: bool,
    ) -> Result<String> {
        let pieces = parser::parse_word(raw)?;
        self.expand_pieces(&pieces, io, mode, quoted).await
    }

    #[async_recursion(?Send)]
    async fn expand_pieces(
        &mut self,
        pieces: &[WordPieceWithSource],
        io: &ExecutionIo,
        mode: ExpansionMode,
        quoted: bool,
    ) -> Result<String> {
        let mut out = String::new();
        for piece in pieces {
            let expanded = self.expand_piece(&piece.piece, io, mode, quoted).await?;
            out.push_str(&expanded);
        }
        Ok(out)
    }

    #[async_recursion(?Send)]
    async fn expand_piece(
        &mut self,
        piece: &WordPiece,
        io: &ExecutionIo,
        mode: ExpansionMode,
        quoted: bool,
    ) -> Result<String> {
        match piece {
            WordPiece::Text(text) | WordPiece::TildePrefix(text) => {
                Ok(append_expanded_value(String::new(), text, mode, quoted))
            }
            WordPiece::SingleQuotedText(text) | WordPiece::AnsiCQuotedText(text) => {
                Ok(append_expanded_value(String::new(), text, mode, true))
            }
            WordPiece::EscapeSequence(text) => Ok(append_expanded_value(
                String::new(),
                unescape_sequence(text),
                mode,
                true,
            )),
            WordPiece::DoubleQuotedSequence(pieces)
            | WordPiece::GettextDoubleQuotedSequence(pieces) => {
                self.expand_pieces(pieces, io, mode, true).await
            }
            WordPiece::ParameterExpansion(expr) => {
                self.expand_parameter_expr(expr, io, mode, quoted).await
            }
            WordPiece::CommandSubstitution(command)
            | WordPiece::BackquotedCommandSubstitution(command) => Ok(append_expanded_value(
                String::new(),
                &self.expand_command_substitution(command, io).await?,
                mode,
                quoted,
            )),
            WordPiece::ArithmeticExpression(expr) => Ok(append_expanded_value(
                String::new(),
                &self.expand_arithmetic_expression(expr)?,
                mode,
                quoted,
            )),
        }
    }

    async fn expand_command_substitution(
        &mut self,
        command: &str,
        io: &ExecutionIo,
    ) -> Result<String> {
        let program = parser::parse(command)?;
        let (stdout, captured) = capture_output();
        let mut shell = self.fork();
        let mut substitution_io = io.clone();
        substitution_io.set_output(1, stdout);
        let _ = shell.run_program_with_io(&program, substitution_io).await?;
        let mut bytes = captured.take();
        while matches!(bytes.last(), Some(b'\n')) {
            bytes.pop();
        }
        String::from_utf8(bytes).map_err(|error| {
            ShellError::message(format!(
                "command substitution did not produce valid utf-8: {error}"
            ))
        })
    }

    #[async_recursion(?Send)]
    async fn expand_parameter_expr(
        &mut self,
        expr: &ParameterExpr,
        io: &ExecutionIo,
        mode: ExpansionMode,
        quoted: bool,
    ) -> Result<String> {
        match expr {
            ParameterExpr::Parameter { parameter, .. } => Ok(self
                .parameter_state(parameter)?
                .value_for_expansion(mode, quoted)),
            ParameterExpr::UseDefaultValues {
                parameter,
                test_type,
                default_value,
                ..
            } => {
                let state = self.parameter_state(parameter)?;
                if parameter_satisfies_test(&state, test_type) {
                    Ok(state.value_for_expansion(mode, quoted))
                } else {
                    self.expand_default_value(default_value.as_deref(), io, mode, quoted)
                        .await
                }
            }
            ParameterExpr::AssignDefaultValues {
                parameter,
                test_type,
                default_value,
                ..
            } => {
                let state = self.parameter_state(parameter)?;
                if parameter_satisfies_test(&state, test_type) {
                    Ok(state.value_for_expansion(mode, quoted))
                } else {
                    let value = self
                        .expand_default_value(default_value.as_deref(), io, mode, quoted)
                        .await?;
                    self.assign_parameter(parameter, value.clone())?;
                    Ok(value)
                }
            }
            ParameterExpr::IndicateErrorIfNullOrUnset {
                parameter,
                test_type,
                error_message,
                ..
            } => {
                let state = self.parameter_state(parameter)?;
                if parameter_satisfies_test(&state, test_type) {
                    Ok(state.value_for_expansion(mode, quoted))
                } else {
                    let message = if let Some(error_message) = error_message {
                        self.expand_text(error_message, io, ExpansionMode::Text, false)
                            .await?
                    } else {
                        format!("{}: parameter not set", parameter_display_name(parameter))
                    };
                    Err(ShellError::message(message))
                }
            }
            ParameterExpr::UseAlternativeValue {
                parameter,
                test_type,
                alternative_value,
                ..
            } => {
                let state = self.parameter_state(parameter)?;
                if parameter_satisfies_test(&state, test_type) {
                    self.expand_default_value(alternative_value.as_deref(), io, mode, quoted)
                        .await
                } else {
                    Ok(String::new())
                }
            }
            ParameterExpr::ParameterLength { parameter, .. } => {
                let state = self.parameter_state(parameter)?;
                Ok(state.value.chars().count().to_string())
            }
            ParameterExpr::RemoveSmallestSuffixPattern {
                parameter, pattern, ..
            } => {
                self.remove_parameter_pattern(
                    parameter,
                    pattern.as_deref(),
                    io,
                    PatternRemoval::SmallestSuffix,
                )
                .await
            }
            ParameterExpr::RemoveLargestSuffixPattern {
                parameter, pattern, ..
            } => {
                self.remove_parameter_pattern(
                    parameter,
                    pattern.as_deref(),
                    io,
                    PatternRemoval::LargestSuffix,
                )
                .await
            }
            ParameterExpr::RemoveSmallestPrefixPattern {
                parameter, pattern, ..
            } => {
                self.remove_parameter_pattern(
                    parameter,
                    pattern.as_deref(),
                    io,
                    PatternRemoval::SmallestPrefix,
                )
                .await
            }
            ParameterExpr::RemoveLargestPrefixPattern {
                parameter, pattern, ..
            } => {
                self.remove_parameter_pattern(
                    parameter,
                    pattern.as_deref(),
                    io,
                    PatternRemoval::LargestPrefix,
                )
                .await
            }
            ParameterExpr::Substring { .. } => Err(ShellError::unsupported("substring expansion")),
            ParameterExpr::Transform { .. }
            | ParameterExpr::UppercaseFirstChar { .. }
            | ParameterExpr::UppercasePattern { .. }
            | ParameterExpr::LowercaseFirstChar { .. }
            | ParameterExpr::LowercasePattern { .. }
            | ParameterExpr::ReplaceSubstring { .. }
            | ParameterExpr::VariableNames { .. }
            | ParameterExpr::MemberKeys { .. } => {
                Err(ShellError::unsupported("non-POSIX parameter transforms"))
            }
        }
    }

    async fn expand_default_value(
        &mut self,
        raw: Option<&str>,
        io: &ExecutionIo,
        mode: ExpansionMode,
        quoted: bool,
    ) -> Result<String> {
        match raw {
            Some(raw) => self.expand_text(raw, io, mode, quoted).await,
            None => Ok(String::new()),
        }
    }

    async fn remove_parameter_pattern(
        &mut self,
        parameter: &Parameter,
        pattern: Option<&str>,
        io: &ExecutionIo,
        removal: PatternRemoval,
    ) -> Result<String> {
        let value = self.parameter_state(parameter)?.value;
        let pattern = match pattern {
            Some(pattern) => {
                self.expand_text(pattern, io, ExpansionMode::Pattern, false)
                    .await?
            }
            None => String::new(),
        };
        remove_shell_pattern(&value, &pattern, removal)
    }

    fn parameter_state(&self, parameter: &Parameter) -> Result<ParameterState> {
        match parameter {
            Parameter::Positional(index) => {
                let index = usize::try_from(*index).map_err(|_| {
                    ShellError::message(format!("invalid positional parameter index {index}"))
                })?;
                Ok(ParameterState {
                    value: self
                        .positional_parameters
                        .get(index.saturating_sub(1))
                        .cloned()
                        .unwrap_or_default(),
                    is_set: index > 0 && index <= self.positional_parameters.len(),
                })
            }
            Parameter::Special(special) => Ok(self.special_parameter_state(special)),
            Parameter::Named(name) => Ok(ParameterState {
                value: self
                    .variable(name)
                    .map(|variable| variable.value.clone())
                    .unwrap_or_default(),
                is_set: self.variable(name).is_some(),
            }),
            Parameter::NamedWithIndex { .. } | Parameter::NamedWithAllIndices { .. } => {
                Err(ShellError::unsupported("array parameter expansion"))
            }
        }
    }

    fn special_parameter_state(&self, parameter: &SpecialParameter) -> ParameterState {
        match parameter {
            SpecialParameter::AllPositionalParameters { .. } => ParameterState {
                value: self.positional_parameters.join(" "),
                is_set: !self.positional_parameters.is_empty(),
            },
            SpecialParameter::PositionalParameterCount => ParameterState {
                value: self.positional_parameters.len().to_string(),
                is_set: true,
            },
            SpecialParameter::LastExitStatus => ParameterState {
                value: self.last_status.to_string(),
                is_set: true,
            },
            SpecialParameter::CurrentOptionFlags => ParameterState {
                value: String::new(),
                is_set: true,
            },
            SpecialParameter::ProcessId => ParameterState {
                value: self.process_id.clone(),
                is_set: true,
            },
            SpecialParameter::LastBackgroundProcessId => ParameterState {
                value: self
                    .last_background_job
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                is_set: self.last_background_job.is_some(),
            },
            SpecialParameter::ShellName => ParameterState {
                value: self.shell_name.clone(),
                is_set: true,
            },
        }
    }

    fn expand_arithmetic_expression(
        &mut self,
        expr: &brush_parser::ast::UnexpandedArithmeticExpr,
    ) -> Result<String> {
        let parsed = brush_parser::arithmetic::parse(&expr.value).map_err(|error| {
            ShellError::message(format!(
                "failed to parse arithmetic expression {:?}: {error}",
                expr.value
            ))
        })?;
        Ok(self.eval_arithmetic_expr(&parsed)?.to_string())
    }

    fn eval_arithmetic_expr(&mut self, expr: &ArithmeticExpr) -> Result<i64> {
        match expr {
            ArithmeticExpr::Literal(value) => Ok(*value),
            ArithmeticExpr::Reference(target) => self.read_arithmetic_target(target),
            ArithmeticExpr::UnaryOp(operator, operand) => {
                let value = self.eval_arithmetic_expr(operand)?;
                match operator {
                    UnaryOperator::UnaryPlus => Ok(value),
                    UnaryOperator::UnaryMinus => value
                        .checked_neg()
                        .ok_or_else(|| ShellError::message("arithmetic overflow: unary -")),
                    UnaryOperator::BitwiseNot => Ok(!value),
                    UnaryOperator::LogicalNot => Ok(bool_to_arithmetic(value == 0)),
                }
            }
            ArithmeticExpr::BinaryOp(operator, left, right) => {
                self.eval_binary_arithmetic(*operator, left, right)
            }
            ArithmeticExpr::Conditional(condition, then_branch, else_branch) => {
                if self.eval_arithmetic_expr(condition)? != 0 {
                    self.eval_arithmetic_expr(then_branch)
                } else {
                    self.eval_arithmetic_expr(else_branch)
                }
            }
            ArithmeticExpr::Assignment(target, value) => {
                let value = self.eval_arithmetic_expr(value)?;
                self.write_arithmetic_target(target, value)?;
                Ok(value)
            }
            ArithmeticExpr::BinaryAssignment(operator, target, operand) => {
                let left = self.read_arithmetic_target(target)?;
                let right = self.eval_arithmetic_expr(operand)?;
                let value = self.apply_binary_operator(*operator, left, right)?;
                self.write_arithmetic_target(target, value)?;
                Ok(value)
            }
            ArithmeticExpr::UnaryAssignment(operator, target) => {
                let current = self.read_arithmetic_target(target)?;
                let updated = match operator {
                    UnaryAssignmentOperator::PrefixIncrement
                    | UnaryAssignmentOperator::PostfixIncrement => current
                        .checked_add(1)
                        .ok_or_else(|| ShellError::message("arithmetic overflow: increment"))?,
                    UnaryAssignmentOperator::PrefixDecrement
                    | UnaryAssignmentOperator::PostfixDecrement => current
                        .checked_sub(1)
                        .ok_or_else(|| ShellError::message("arithmetic overflow: decrement"))?,
                };
                self.write_arithmetic_target(target, updated)?;
                match operator {
                    UnaryAssignmentOperator::PrefixIncrement
                    | UnaryAssignmentOperator::PrefixDecrement => Ok(updated),
                    UnaryAssignmentOperator::PostfixIncrement
                    | UnaryAssignmentOperator::PostfixDecrement => Ok(current),
                }
            }
        }
    }

    fn eval_binary_arithmetic(
        &mut self,
        operator: BinaryOperator,
        left: &ArithmeticExpr,
        right: &ArithmeticExpr,
    ) -> Result<i64> {
        match operator {
            BinaryOperator::LogicalAnd => {
                let left = self.eval_arithmetic_expr(left)?;
                if left == 0 {
                    return Ok(0);
                }
                Ok(bool_to_arithmetic(self.eval_arithmetic_expr(right)? != 0))
            }
            BinaryOperator::LogicalOr => {
                let left = self.eval_arithmetic_expr(left)?;
                if left != 0 {
                    return Ok(1);
                }
                Ok(bool_to_arithmetic(self.eval_arithmetic_expr(right)? != 0))
            }
            BinaryOperator::Comma => {
                let _ = self.eval_arithmetic_expr(left)?;
                self.eval_arithmetic_expr(right)
            }
            _ => {
                let left = self.eval_arithmetic_expr(left)?;
                let right = self.eval_arithmetic_expr(right)?;
                self.apply_binary_operator(operator, left, right)
            }
        }
    }

    fn apply_binary_operator(
        &self,
        operator: BinaryOperator,
        left: i64,
        right: i64,
    ) -> Result<i64> {
        match operator {
            BinaryOperator::Power => {
                let exponent = u32::try_from(right).map_err(|_| {
                    ShellError::message("arithmetic exponent must be a non-negative integer")
                })?;
                left.checked_pow(exponent)
                    .ok_or_else(|| ShellError::message("arithmetic overflow: exponentiation"))
            }
            BinaryOperator::Multiply => left
                .checked_mul(right)
                .ok_or_else(|| ShellError::message("arithmetic overflow: multiplication")),
            BinaryOperator::Divide => {
                if right == 0 {
                    return Err(ShellError::message("division by zero"));
                }
                left.checked_div(right)
                    .ok_or_else(|| ShellError::message("arithmetic overflow: division"))
            }
            BinaryOperator::Modulo => {
                if right == 0 {
                    return Err(ShellError::message("division by zero"));
                }
                left.checked_rem(right)
                    .ok_or_else(|| ShellError::message("arithmetic overflow: modulo"))
            }
            BinaryOperator::Comma => Ok(right),
            BinaryOperator::Add => left
                .checked_add(right)
                .ok_or_else(|| ShellError::message("arithmetic overflow: addition")),
            BinaryOperator::Subtract => left
                .checked_sub(right)
                .ok_or_else(|| ShellError::message("arithmetic overflow: subtraction")),
            BinaryOperator::ShiftLeft => {
                let amount = u32::try_from(right).map_err(|_| {
                    ShellError::message("shift amount must be a non-negative integer")
                })?;
                left.checked_shl(amount)
                    .ok_or_else(|| ShellError::message("arithmetic overflow: shift left"))
            }
            BinaryOperator::ShiftRight => {
                let amount = u32::try_from(right).map_err(|_| {
                    ShellError::message("shift amount must be a non-negative integer")
                })?;
                left.checked_shr(amount)
                    .ok_or_else(|| ShellError::message("arithmetic overflow: shift right"))
            }
            BinaryOperator::LessThan => Ok(bool_to_arithmetic(left < right)),
            BinaryOperator::LessThanOrEqualTo => Ok(bool_to_arithmetic(left <= right)),
            BinaryOperator::GreaterThan => Ok(bool_to_arithmetic(left > right)),
            BinaryOperator::GreaterThanOrEqualTo => Ok(bool_to_arithmetic(left >= right)),
            BinaryOperator::Equals => Ok(bool_to_arithmetic(left == right)),
            BinaryOperator::NotEquals => Ok(bool_to_arithmetic(left != right)),
            BinaryOperator::BitwiseAnd => Ok(left & right),
            BinaryOperator::BitwiseXor => Ok(left ^ right),
            BinaryOperator::BitwiseOr => Ok(left | right),
            BinaryOperator::LogicalAnd | BinaryOperator::LogicalOr => unreachable!(),
        }
    }

    fn read_arithmetic_target(&mut self, target: &ArithmeticTarget) -> Result<i64> {
        match target {
            ArithmeticTarget::Variable(name) => {
                let Some(variable) = self.variable(name) else {
                    return Ok(0);
                };
                if variable.value.is_empty() {
                    return Ok(0);
                }
                let parsed = brush_parser::arithmetic::parse(&variable.value).map_err(|error| {
                    ShellError::message(format!(
                        "failed to parse arithmetic variable {name:?}: {error}"
                    ))
                })?;
                self.eval_arithmetic_expr(&parsed)
            }
            ArithmeticTarget::ArrayElement(_, _) => {
                Err(ShellError::unsupported("arithmetic array references"))
            }
        }
    }

    fn write_arithmetic_target(&mut self, target: &ArithmeticTarget, value: i64) -> Result<()> {
        match target {
            ArithmeticTarget::Variable(name) => {
                self.set_variable(name.clone(), value.to_string(), None);
                Ok(())
            }
            ArithmeticTarget::ArrayElement(_, _) => {
                Err(ShellError::unsupported("arithmetic array assignments"))
            }
        }
    }

    fn assign_parameter(&mut self, parameter: &Parameter, value: String) -> Result<()> {
        match parameter {
            Parameter::Named(name) => {
                self.set_variable(name.clone(), value, None);
                Ok(())
            }
            _ => Err(ShellError::message(format!(
                "cannot assign through parameter {}",
                parameter_display_name(parameter)
            ))),
        }
    }
}

#[derive(Clone, Copy)]
enum ExpansionMode {
    Text,
    Pattern,
}

enum DuplicateTarget {
    Fd(IoFd),
    Close,
}

#[derive(Clone, Copy)]
enum PatternRemoval {
    SmallestPrefix,
    LargestPrefix,
    SmallestSuffix,
    LargestSuffix,
}

struct ParameterState {
    value: String,
    is_set: bool,
}

impl ParameterState {
    fn value_for_expansion(&self, mode: ExpansionMode, quoted: bool) -> String {
        append_expanded_value(String::new(), &self.value, mode, quoted)
    }
}

fn parameter_satisfies_test(state: &ParameterState, test_type: &ParameterTestType) -> bool {
    match test_type {
        ParameterTestType::UnsetOrNull => state.is_set && !state.value.is_empty(),
        ParameterTestType::Unset => state.is_set,
    }
}

fn parameter_display_name(parameter: &Parameter) -> String {
    match parameter {
        Parameter::Positional(index) => index.to_string(),
        Parameter::Special(SpecialParameter::AllPositionalParameters { concatenate }) => {
            if *concatenate {
                "*".to_owned()
            } else {
                "@".to_owned()
            }
        }
        Parameter::Special(SpecialParameter::PositionalParameterCount) => "#".to_owned(),
        Parameter::Special(SpecialParameter::LastExitStatus) => "?".to_owned(),
        Parameter::Special(SpecialParameter::CurrentOptionFlags) => "-".to_owned(),
        Parameter::Special(SpecialParameter::ProcessId) => "$".to_owned(),
        Parameter::Special(SpecialParameter::LastBackgroundProcessId) => "!".to_owned(),
        Parameter::Special(SpecialParameter::ShellName) => "0".to_owned(),
        Parameter::Named(name) => name.clone(),
        Parameter::NamedWithIndex { name, index } => format!("{name}[{index}]"),
        Parameter::NamedWithAllIndices { name, .. } => format!("{name}[@]"),
    }
}

fn append_expanded_value(
    mut buffer: String,
    value: &str,
    mode: ExpansionMode,
    quoted: bool,
) -> String {
    match mode {
        ExpansionMode::Text => buffer.push_str(value),
        ExpansionMode::Pattern => {
            if quoted {
                buffer.push_str(&Pattern::escape(value));
            } else {
                buffer.push_str(value);
            }
        }
    }
    buffer
}

fn unescape_sequence(value: &str) -> &str {
    value.strip_prefix('\\').unwrap_or(value)
}

fn shell_pattern_matches(pattern: &str, value: &str) -> Result<bool> {
    let pattern = Pattern::new(pattern).map_err(|error| {
        ShellError::message(format!("invalid shell pattern {pattern:?}: {error}"))
    })?;
    Ok(pattern.matches_with(
        value,
        MatchOptions {
            case_sensitive: true,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        },
    ))
}

fn remove_shell_pattern(value: &str, pattern: &str, removal: PatternRemoval) -> Result<String> {
    let boundaries = char_boundaries(value);
    match removal {
        PatternRemoval::SmallestPrefix => {
            for end in boundaries {
                if shell_pattern_matches(pattern, &value[..end])? {
                    return Ok(value[end..].to_owned());
                }
            }
        }
        PatternRemoval::LargestPrefix => {
            for end in boundaries.into_iter().rev() {
                if shell_pattern_matches(pattern, &value[..end])? {
                    return Ok(value[end..].to_owned());
                }
            }
        }
        PatternRemoval::SmallestSuffix => {
            for start in boundaries.into_iter().rev() {
                if shell_pattern_matches(pattern, &value[start..])? {
                    return Ok(value[..start].to_owned());
                }
            }
        }
        PatternRemoval::LargestSuffix => {
            for start in boundaries {
                if shell_pattern_matches(pattern, &value[start..])? {
                    return Ok(value[..start].to_owned());
                }
            }
        }
    }
    Ok(value.to_owned())
}

fn char_boundaries(value: &str) -> Vec<usize> {
    let mut boundaries = value
        .char_indices()
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    boundaries.push(value.len());
    boundaries
}

fn bool_to_arithmetic(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn overlay_environment(
    mut base: Vec<(String, String)>,
    assignments: Vec<(String, String)>,
) -> Vec<(String, String)> {
    for (name, value) in assignments {
        if let Some(existing) = base.iter_mut().find(|(key, _)| key == &name) {
            existing.1 = value;
        } else {
            base.push((name, value));
        }
    }
    base
}

fn apply_pipeline_bang(status: CommandStatus, bang: bool) -> CommandStatus {
    if bang {
        CommandStatus::new(if status.is_success() { 1 } else { 0 })
    } else {
        status
    }
}

fn is_special_builtin(name: &str) -> bool {
    matches!(name, ":" | "exec" | "exit" | "export" | "unset")
}

fn effective_fd(fd: Option<IoFd>, kind: &IoFileRedirectKind) -> IoFd {
    match fd {
        Some(fd) => fd,
        None => match kind {
            IoFileRedirectKind::Read
            | IoFileRedirectKind::ReadAndWrite
            | IoFileRedirectKind::DuplicateInput => 0,
            IoFileRedirectKind::Write
            | IoFileRedirectKind::Append
            | IoFileRedirectKind::Clobber
            | IoFileRedirectKind::DuplicateOutput => 1,
        },
    }
}

fn candidate_paths(cwd: &Path, input: &str, search_path: &str) -> Vec<PathBuf> {
    if input.contains('/') || input.starts_with('.') {
        return explicit_program_candidates(cwd, input);
    }

    let mut candidates = Vec::new();
    for directory in search_path.split(':').filter(|segment| !segment.is_empty()) {
        let base = resolve_path(cwd, Path::new(directory));
        candidates.push(base.join(input));
        if !input.ends_with(".wasm") {
            candidates.push(base.join(format!("{input}.wasm")));
        }
    }
    candidates
}

fn explicit_program_candidates(cwd: &Path, input: &str) -> Vec<PathBuf> {
    let path = resolve_path(cwd, Path::new(input));
    let mut candidates = vec![path.clone()];
    if !input.ends_with(".wasm") {
        candidates.push(PathBuf::from(format!("{}.wasm", path.display())));
    }
    candidates
}

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    let mut resolved = if path.is_absolute() {
        PathBuf::from("/")
    } else {
        cwd.to_path_buf()
    };

    for component in path.components() {
        match component {
            Component::RootDir => resolved = PathBuf::from("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
                if resolved.as_os_str().is_empty() {
                    resolved.push("/");
                }
            }
            Component::Normal(part) => resolved.push(part),
            Component::Prefix(_) => unreachable!("unix shell paths must not have prefixes"),
        }
    }

    if resolved.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        resolved
    }
}

fn display_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        "/".to_owned()
    } else {
        path.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll};

    use async_trait::async_trait;
    use futures::channel::oneshot;
    use futures::executor::LocalPool;
    use futures::future::{FutureExt, LocalBoxFuture};
    use futures::io::AsyncWrite;
    use futures::task::LocalSpawnExt;

    use super::*;
    use crate::parser;
    use crate::platform::{RunningProcess, ShellPlatform, SpawnRequest, WriteMode};
    use crate::streams::{
        InputStream, OutputStream, close, read_all, shared_buffer_file, write_all,
    };

    #[test]
    fn shell_variables_are_not_exported_without_export() {
        let state = run_script(
            "foo=bar\nprintenv foo\nexport foo\nprintenv foo\n",
            [("printenv", printenv_command)],
        );
        assert_eq!(state.stdout, b"bar\n");
    }

    #[test]
    fn builtin_cat_reads_pipeline_input() {
        let state = run_script("producer | cat\n", [("producer", producer_command)]);
        assert_eq!(state.stdout, b"pipe-data\n");
    }

    #[test]
    fn if_clause_executes_then_branch() {
        let state = run_script("if true; then echo ok; else echo no; fi\n", []);
        assert_eq!(state.stdout, b"ok\n");
    }

    #[test]
    fn case_clause_matches_pattern() {
        let state = run_script("case foo in f*) echo yes;; *) echo no;; esac\n", []);
        assert_eq!(state.stdout, b"yes\n");
    }

    #[test]
    fn stderr_redirection_respects_requested_fd() {
        let state = run_script("errcmd 2>/err.log\n", [("errcmd", errcmd_command)]);
        assert_eq!(
            state.files.get(Path::new("/err.log")).map(Vec::as_slice),
            Some("bad\n".as_bytes())
        );
    }

    #[test]
    fn redirected_brace_group_preserves_fd_ordering() {
        let state = run_script("{ echo out; echo err >&2; } 2>&1 >/out.log\n", []);
        assert_eq!(state.stdout, b"err\n");
        assert_eq!(
            state.files.get(Path::new("/out.log")).map(Vec::as_slice),
            Some("out\n".as_bytes())
        );
    }

    #[test]
    fn redirected_function_body_uses_call_site_redirection() {
        let state = run_script("f(){ echo hi; }\nf >/fn.log\n", []);
        assert_eq!(
            state.files.get(Path::new("/fn.log")).map(Vec::as_slice),
            Some("hi\n".as_bytes())
        );
    }

    #[test]
    fn command_substitution_trims_trailing_newlines() {
        let state = run_script("echo $(newlinecmd)\n", [("newlinecmd", newline_command)]);
        assert_eq!(state.stdout, b"a\n");
    }

    #[test]
    fn parameter_expansion_supports_default_and_pattern_removal() {
        let state = run_script(
            "echo ${unset_var:-fallback}\nfoo=abcxyz\necho ${foo%z} ${foo%%x*} ${foo#abc} ${foo##*c}\n",
            [],
        );
        assert_eq!(state.stdout, b"fallback\nabcxy abc xyz xyz\n");
    }

    #[test]
    fn background_commands_can_be_waited_on() {
        let state = run_script("false & wait $! || echo fail\n", []);
        assert_eq!(state.stdout, b"fail\n");
    }

    #[test]
    fn arithmetic_expansion_evaluates_and_assigns() {
        let state = run_script("x=5\necho $((x += 2))\necho \"$x\"\n", []);
        assert_eq!(state.stdout, b"7\n7\n");
    }

    fn run_script<const N: usize>(
        script: &str,
        commands: [(&'static str, TestCommandHandler); N],
    ) -> TestState {
        let mut pool = LocalPool::new();
        let platform = TestPlatform::new(pool.spawner());
        for (name, command) in commands {
            platform.register_command(name, command);
        }

        let platform_for_run = platform.clone();
        pool.run_until(async move {
            let mut shell = Shell::new(
                platform_for_run.clone(),
                Bootstrap {
                    shell_name: "dash".to_owned(),
                    process_id: "1".to_owned(),
                    positional_parameters: Vec::new(),
                    working_dir: PathBuf::from("/"),
                    environment: Vec::new(),
                },
            );
            let program = parser::parse(script).expect("script should parse");
            let status = shell
                .run_program(&program)
                .await
                .expect("script should run");
            assert!(status.is_success(), "script exited with {}", status.code());
        });

        platform.snapshot()
    }

    fn printenv_command(invocation: TestInvocation) -> TestCommandResult {
        let name = invocation.args.first().cloned().unwrap_or_default();
        let value = invocation
            .env
            .into_iter()
            .find_map(|(key, value)| (key == name).then_some(value))
            .unwrap_or_default();
        let stdout = if value.is_empty() {
            Vec::new()
        } else {
            format!("{value}\n").into_bytes()
        };
        TestCommandResult {
            exit_code: 0,
            stdout,
            stderr: Vec::new(),
        }
    }

    fn producer_command(_invocation: TestInvocation) -> TestCommandResult {
        TestCommandResult {
            exit_code: 0,
            stdout: b"pipe-data\n".to_vec(),
            stderr: Vec::new(),
        }
    }

    fn errcmd_command(_invocation: TestInvocation) -> TestCommandResult {
        TestCommandResult {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: b"bad\n".to_vec(),
        }
    }

    fn newline_command(_invocation: TestInvocation) -> TestCommandResult {
        TestCommandResult {
            exit_code: 0,
            stdout: b"a\n\n".to_vec(),
            stderr: Vec::new(),
        }
    }

    type TestCommandHandler = fn(TestInvocation) -> TestCommandResult;

    #[derive(Clone)]
    struct TestPlatform {
        spawner: futures::executor::LocalSpawner,
        state: Rc<RefCell<TestState>>,
    }

    #[derive(Default, Clone)]
    struct TestState {
        files: HashMap<PathBuf, Vec<u8>>,
        directories: HashSet<PathBuf>,
        commands: HashMap<String, TestCommandHandler>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }

    struct TestInvocation {
        args: Vec<String>,
        env: Vec<(String, String)>,
    }

    struct TestCommandResult {
        exit_code: u8,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }

    impl TestPlatform {
        fn new(spawner: futures::executor::LocalSpawner) -> Self {
            let mut state = TestState::default();
            state.directories.insert(PathBuf::from("/"));
            Self {
                spawner,
                state: Rc::new(RefCell::new(state)),
            }
        }

        fn register_command(&self, name: &str, handler: TestCommandHandler) {
            let mut state = self.state.borrow_mut();
            state.commands.insert(name.to_owned(), handler);
            state
                .files
                .insert(PathBuf::from(format!("/bin/{name}")), vec![0]);
            state
                .files
                .insert(PathBuf::from(format!("/bin/{name}.wasm")), vec![0]);
        }

        fn snapshot(&self) -> TestState {
            self.state.borrow().clone()
        }
    }

    struct TestChild {
        exit: oneshot::Receiver<Result<u8>>,
    }

    #[async_trait]
    impl RunningProcess for TestChild {
        async fn wait(self) -> Result<u8> {
            self.exit
                .await
                .map_err(|_| ShellError::message("test child dropped"))?
        }
    }

    impl ShellPlatform for TestPlatform {
        type Child = TestChild;

        fn spawn_task(&self, task: LocalBoxFuture<'static, ()>) {
            self.spawner
                .spawn_local(task)
                .expect("local task spawn must succeed");
        }

        fn stdout(&self) -> OutputStream {
            OutputStream::new(CaptureWriter {
                state: self.state.clone(),
                target: CaptureTarget::Stdout,
            })
        }

        fn stderr(&self) -> OutputStream {
            OutputStream::new(CaptureWriter {
                state: self.state.clone(),
                target: CaptureTarget::Stderr,
            })
        }

        async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
            self.state
                .borrow()
                .files
                .get(path)
                .cloned()
                .ok_or_else(|| ShellError::message(format!("missing file {}", path.display())))
        }

        async fn open_input(&self, path: &Path) -> Result<InputStream> {
            Ok(InputStream::from_bytes(self.read_file(path).await?))
        }

        async fn open_output(&self, path: &Path, mode: WriteMode) -> Result<OutputStream> {
            Ok(OutputStream::new(TestFileWriter {
                state: self.state.clone(),
                path: path.to_path_buf(),
                append: mode == WriteMode::Append,
                initialized: false,
            }))
        }

        async fn open_read_write(&self, path: &Path) -> Result<(InputStream, OutputStream)> {
            let initial = self
                .state
                .borrow()
                .files
                .get(path)
                .cloned()
                .unwrap_or_default();
            let state = self.state.clone();
            let path = path.to_path_buf();
            Ok(shared_buffer_file(initial, move |bytes| {
                let state = state.clone();
                let path = path.clone();
                async move {
                    state.borrow_mut().files.insert(path, bytes);
                    Ok(())
                }
                .boxed_local()
            }))
        }

        async fn exists(&self, path: &Path) -> bool {
            let state = self.state.borrow();
            state.files.contains_key(path) || state.directories.contains(path)
        }

        async fn is_file(&self, path: &Path) -> bool {
            self.state.borrow().files.contains_key(path)
        }

        async fn is_dir(&self, path: &Path) -> bool {
            self.state.borrow().directories.contains(path)
        }

        async fn spawn(&self, request: SpawnRequest) -> Result<Self::Child> {
            let command_name = Path::new(&request.resolved.path)
                .file_name()
                .and_then(|name| name.to_str())
                .expect("resolved command path must have a file name")
                .trim_end_matches(".wasm")
                .to_owned();
            let handler = *self
                .state
                .borrow()
                .commands
                .get(&command_name)
                .ok_or_else(|| ShellError::message(format!("unknown command {command_name}")))?;

            let (sender, receiver) = oneshot::channel();
            self.spawn_task(Box::pin(async move {
                let result = async move {
                    let mut stdin = request.stdin;
                    let invocation = TestInvocation {
                        args: request.args,
                        env: request.env,
                    };
                    let _stdin = read_all(&mut stdin).await?;
                    let output = handler(invocation);
                    let mut stdout = request.stdout;
                    write_all(&mut stdout, &output.stdout).await?;
                    close(&mut stdout).await?;
                    let mut stderr = request.stderr;
                    write_all(&mut stderr, &output.stderr).await?;
                    close(&mut stderr).await?;
                    Ok(output.exit_code)
                }
                .await;
                let _ = sender.send(result);
            }));

            Ok(TestChild { exit: receiver })
        }
    }

    struct CaptureWriter {
        state: Rc<RefCell<TestState>>,
        target: CaptureTarget,
    }

    impl AsyncWrite for CaptureWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let mut state = self.state.borrow_mut();
            match self.target {
                CaptureTarget::Stdout => state.stdout.extend_from_slice(buf),
                CaptureTarget::Stderr => state.stderr.extend_from_slice(buf),
            }
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct TestFileWriter {
        state: Rc<RefCell<TestState>>,
        path: PathBuf,
        append: bool,
        initialized: bool,
    }

    impl AsyncWrite for TestFileWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let should_truncate = !self.append && !self.initialized;
            if should_truncate {
                self.initialized = true;
            }
            let mut state = self.state.borrow_mut();
            let file = state.files.entry(self.path.clone()).or_default();
            if should_truncate {
                file.clear();
            }
            file.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Clone, Copy)]
    enum CaptureTarget {
        Stdout,
        Stderr,
    }
}
