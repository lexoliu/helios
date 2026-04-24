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

use crate::HELIOS_PROCESS_ID_ENV;
use crate::builtin;
use crate::error::{Result, ShellError};
use crate::ifs::{DEFAULT_IFS, IfsConfig};
use crate::options::ShellOptions;
use crate::parser;
use crate::platform::{ResolvedProgram, RunningProcess, ShellPlatform, SpawnRequest, WriteMode};
use crate::streams::{InputStream, OutputStream, capture_output, close, write_all};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variable {
    pub value: Option<String>,
    pub exported: bool,
    pub readonly: bool,
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
    code: i32,
    flow: ControlFlow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum ControlFlow {
    #[default]
    None,
    ExitShell,
    Return,
    Break(usize),
    Continue(usize),
}

impl CommandStatus {
    pub const SUCCESS: Self = Self {
        code: 0,
        flow: ControlFlow::None,
    };

    pub const fn new(code: i32) -> Self {
        Self {
            code,
            flow: ControlFlow::None,
        }
    }

    pub const fn exit(code: i32) -> Self {
        Self {
            code,
            flow: ControlFlow::ExitShell,
        }
    }

    pub const fn shell_return(code: i32) -> Self {
        Self {
            code,
            flow: ControlFlow::Return,
        }
    }

    pub const fn break_loop(code: i32, depth: usize) -> Self {
        Self {
            code,
            flow: ControlFlow::Break(depth),
        }
    }

    pub const fn continue_loop(code: i32, depth: usize) -> Self {
        Self {
            code,
            flow: ControlFlow::Continue(depth),
        }
    }

    pub const fn code(self) -> i32 {
        self.code
    }

    pub const fn is_success(self) -> bool {
        self.code == 0
    }

    pub const fn is_exit(self) -> bool {
        matches!(self.flow, ControlFlow::ExitShell)
    }

    pub const fn is_return(self) -> bool {
        matches!(self.flow, ControlFlow::Return)
    }

    pub const fn is_interrupting(self) -> bool {
        !matches!(self.flow, ControlFlow::None)
    }

    pub const fn as_plain(self) -> Self {
        Self::new(self.code)
    }

    const fn flow(self) -> ControlFlow {
        self.flow
    }

    fn normalize_fork_boundary(self) -> Self {
        Self::new(self.code)
    }
}

pub struct Shell<P> {
    pub(crate) platform: P,
    pub(crate) variables: HashMap<String, Variable>,
    pub(crate) functions: HashMap<String, FunctionDefinition>,
    pub(crate) shell_name: String,
    pub(crate) process_id: String,
    pub(crate) positional_parameters: Vec<String>,
    pub(crate) options: ShellOptions,
    pub(crate) working_dir: PathBuf,
    pub(crate) last_status: i32,
    pub(crate) last_pipeline_statuses: Vec<i32>,
    pub(crate) last_background_job: Option<u64>,
    pub(crate) next_background_job: u64,
    pub(crate) function_depth: usize,
    pub(crate) loop_depth: usize,
    pub(crate) background_jobs: Rc<RefCell<HashMap<u64, oneshot::Receiver<Result<CommandStatus>>>>>,
}

#[derive(Default)]
struct TemporaryAssignments {
    originals: HashMap<String, Option<Variable>>,
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
                    value: Some(value),
                    exported: true,
                    readonly: false,
                },
            );
        }
        variables
            .entry("PATH".to_owned())
            .or_insert_with(|| Variable {
                value: Some("/bin".to_owned()),
                exported: true,
                readonly: false,
            });
        variables.insert(
            "PWD".to_owned(),
            Variable {
                value: Some(display_path(&bootstrap.working_dir)),
                exported: true,
                readonly: false,
            },
        );

        Self {
            platform,
            variables,
            functions: HashMap::new(),
            shell_name: bootstrap.shell_name,
            process_id: bootstrap.process_id,
            positional_parameters: bootstrap.positional_parameters,
            options: ShellOptions::default(),
            working_dir: bootstrap.working_dir,
            last_status: 0,
            last_pipeline_statuses: vec![0],
            last_background_job: None,
            next_background_job: 1,
            function_depth: 0,
            loop_depth: 0,
            background_jobs: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn ensure_assignable(&self, name: &str) -> Result<()> {
        if matches!(self.variables.get(name), Some(variable) if variable.readonly) {
            return Err(ShellError::readonly_variable(name));
        }
        Ok(())
    }

    pub fn assign_variable(&mut self, name: String, value: String) -> Result<()> {
        self.ensure_assignable(&name)?;
        match self.variables.get_mut(&name) {
            Some(existing) => {
                existing.value = Some(value);
                if self.options.allexport() {
                    existing.exported = true;
                }
            }
            None => {
                self.variables.insert(
                    name,
                    Variable {
                        value: Some(value),
                        exported: self.options.allexport(),
                        readonly: false,
                    },
                );
            }
        }
        Ok(())
    }

    pub fn assign_exported_variable(&mut self, name: String, value: String) -> Result<()> {
        self.ensure_assignable(&name)?;
        match self.variables.get_mut(&name) {
            Some(existing) => {
                existing.value = Some(value);
                existing.exported = true;
            }
            None => {
                self.variables.insert(
                    name,
                    Variable {
                        value: Some(value),
                        exported: true,
                        readonly: false,
                    },
                );
            }
        }
        Ok(())
    }

    pub fn mark_exported(&mut self, name: &str) -> Result<()> {
        if let Some(variable) = self.variables.get_mut(name) {
            variable.exported = true;
            return Ok(());
        }

        self.variables.insert(
            name.to_owned(),
            Variable {
                value: None,
                exported: true,
                readonly: false,
            },
        );
        Ok(())
    }

    pub fn mark_readonly(&mut self, name: &str, value: Option<String>) -> Result<()> {
        match self.variables.get_mut(name) {
            Some(variable) => {
                if variable.readonly {
                    if value.is_some() {
                        return Err(ShellError::readonly_variable(name));
                    }
                    return Ok(());
                }
                if let Some(value) = value {
                    variable.value = Some(value);
                }
                variable.readonly = true;
                Ok(())
            }
            None => {
                self.variables.insert(
                    name.to_owned(),
                    Variable {
                        value,
                        exported: self.options.allexport(),
                        readonly: true,
                    },
                );
                Ok(())
            }
        }
    }

    pub fn unset(&mut self, name: &str) -> Result<()> {
        self.ensure_assignable(name)?;
        self.variables.remove(name);
        Ok(())
    }

    pub fn variable(&self, name: &str) -> Option<&Variable> {
        self.variables.get(name)
    }

    pub fn exported_environment(&self) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        for (name, variable) in &self.variables {
            if name != HELIOS_PROCESS_ID_ENV
                && variable.exported
                && let Some(value) = &variable.value
            {
                pairs.push((name.clone(), value.clone()));
            }
        }
        pairs
    }

    pub fn set_working_dir(&mut self, working_dir: PathBuf) -> Vec<ShellError> {
        let oldpwd = self.working_dir.clone();
        self.working_dir = working_dir.clone();
        let mut errors = Vec::new();
        if let Err(error) =
            self.assign_exported_variable("PWD".to_owned(), display_path(&working_dir))
        {
            errors.push(error);
        }
        if let Err(error) =
            self.assign_exported_variable("OLDPWD".to_owned(), display_path(&oldpwd))
        {
            errors.push(error);
        }
        errors
    }

    pub fn current_dir(&self) -> &Path {
        &self.working_dir
    }

    pub(crate) fn resolve_user_path(&self, path: &str) -> PathBuf {
        resolve_path(&self.working_dir, Path::new(path))
    }

    fn ifs_value(&self) -> &str {
        self.variable("IFS")
            .and_then(|variable| variable.value.as_deref())
            .unwrap_or(DEFAULT_IFS)
    }

    fn apply_temporary_assignments(
        &mut self,
        assignments: &[(String, String)],
    ) -> Result<TemporaryAssignments> {
        let mut originals = HashMap::new();
        for (name, value) in assignments {
            originals
                .entry(name.clone())
                .or_insert_with(|| self.variables.get(name).cloned());
            self.assign_variable(name.clone(), value.clone())?;
        }
        Ok(TemporaryAssignments { originals })
    }

    fn restore_temporary_assignments(&mut self, assignments: TemporaryAssignments) {
        for (name, original) in assignments.originals {
            match original {
                Some(variable) => {
                    self.variables.insert(name, variable);
                }
                None => {
                    self.variables.remove(&name);
                }
            }
        }
    }

    fn ifs_joiner(&self) -> String {
        let ifs = self.ifs_value();
        if let Some((index, _)) = ifs.char_indices().nth(1) {
            ifs[..index].to_owned()
        } else {
            ifs.to_owned()
        }
    }

    pub(crate) fn ifs_config(&self) -> IfsConfig<'_> {
        IfsConfig::new(self.ifs_value())
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
            options: self.options,
            working_dir: self.working_dir.clone(),
            last_status: self.last_status,
            last_pipeline_statuses: self.last_pipeline_statuses.clone(),
            last_background_job: self.last_background_job,
            next_background_job: self.next_background_job,
            function_depth: self.function_depth,
            loop_depth: self.loop_depth,
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

    pub(crate) async fn eval_source_with_io(
        &mut self,
        source: &str,
        io: ExecutionIo,
    ) -> Result<CommandStatus> {
        let program = parser::parse(source)?;
        self.run_program_with_io(&program, io).await
    }

    pub(crate) async fn load_shell_source(&self, path: &Path) -> Result<String> {
        let bytes = self.platform.read_file(path).await?;
        String::from_utf8(bytes).map_err(|error| {
            ShellError::message(format!(
                "shell source {} is not valid utf-8: {error}",
                path.display()
            ))
        })
    }

    pub(crate) async fn resolve_source_path(&self, input: &str) -> Result<PathBuf> {
        let search_path = self
            .variable("PATH")
            .and_then(|variable| variable.value.as_deref())
            .unwrap_or("/bin");

        for candidate in source_candidate_paths(&self.working_dir, input, search_path) {
            if self.platform.is_file(&candidate).await {
                return Ok(candidate);
            }
        }

        Err(ShellError::message(format!("{input}: not found")))
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
            if last.is_interrupting() {
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
        if last.is_interrupting() {
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
            if last.is_interrupting() {
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
            let result = shell
                .run_command(&command, io)
                .await
                .map(CommandStatus::normalize_fork_boundary);
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
            let result = shell
                .run_and_or(&and_or, background_io)
                .await
                .map(CommandStatus::normalize_fork_boundary);
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
        let error_io = io.clone();
        let result = match command {
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
        };

        match result {
            Err(error @ (ShellError::ReadonlyVariable(_) | ShellError::ShellDiagnostic(_))) => {
                emit_command_error(error, error_io, true).await
            }
            Err(error) => emit_command_error(error, error_io, false).await,
            Ok(status) => Ok(status),
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
                Ok(shell
                    .run_compound_list(&subshell.list, io)
                    .await?
                    .normalize_fork_boundary())
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
        if condition.is_interrupting() {
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
            if status.is_interrupting() || status.is_success() || else_clause.condition.is_none() {
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
            if status.is_interrupting() || !status.is_success() {
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
            self.loop_depth += 1;
            let condition = self.run_compound_list(&clause.0, io.clone()).await;
            self.loop_depth -= 1;
            let condition = condition?;
            match condition.flow() {
                ControlFlow::ExitShell | ControlFlow::Return => return Ok(condition),
                ControlFlow::Break(depth) => {
                    return Ok(if depth > 1 {
                        CommandStatus::break_loop(condition.code(), depth - 1)
                    } else {
                        condition.as_plain()
                    });
                }
                ControlFlow::Continue(depth) => {
                    if depth > 1 {
                        return Ok(CommandStatus::continue_loop(condition.code(), depth - 1));
                    }
                    last = condition.as_plain();
                    continue;
                }
                ControlFlow::None => {}
            }
            let should_run = if invert {
                !condition.is_success()
            } else {
                condition.is_success()
            };
            if !should_run {
                return Ok(last);
            }
            self.loop_depth += 1;
            let body = self.run_compound_list(&clause.1.list, io.clone()).await;
            self.loop_depth -= 1;
            last = body?;
            match last.flow() {
                ControlFlow::ExitShell | ControlFlow::Return => return Ok(last),
                ControlFlow::Break(depth) => {
                    return Ok(if depth > 1 {
                        CommandStatus::break_loop(last.code(), depth - 1)
                    } else {
                        last.as_plain()
                    });
                }
                ControlFlow::Continue(depth) => {
                    if depth > 1 {
                        return Ok(CommandStatus::continue_loop(last.code(), depth - 1));
                    }
                    last = last.as_plain();
                    continue;
                }
                ControlFlow::None => {}
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
                let mut expanded = Vec::new();
                for value in values {
                    expanded.extend(self.expand_command_word(value, &io).await?);
                }
                expanded
            }
            None => self.positional_parameters.clone(),
        };

        let mut last = CommandStatus::SUCCESS;
        for value in values {
            self.assign_variable(for_clause.variable_name.clone(), value)?;
            self.loop_depth += 1;
            let body = self
                .run_compound_list(&for_clause.body.list, io.clone())
                .await;
            self.loop_depth -= 1;
            last = body?;
            match last.flow() {
                ControlFlow::ExitShell | ControlFlow::Return => return Ok(last),
                ControlFlow::Break(depth) => {
                    return Ok(if depth > 1 {
                        CommandStatus::break_loop(last.code(), depth - 1)
                    } else {
                        last.as_plain()
                    });
                }
                ControlFlow::Continue(depth) => {
                    if depth > 1 {
                        return Ok(CommandStatus::continue_loop(last.code(), depth - 1));
                    }
                    last = last.as_plain();
                    continue;
                }
                ControlFlow::None => {}
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
                if result.is_interrupting() {
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
                        args.extend(self.expand_command_word(word, &io).await?)
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
            args.extend(self.expand_command_word(word, &io).await?);
        }

        if let Some(suffix) = &command.suffix {
            for item in &suffix.0 {
                match item {
                    CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                        args.extend(self.expand_command_word(word, &io).await?);
                    }
                    CommandPrefixOrSuffixItem::Word(word) => {
                        args.extend(self.expand_command_word(word, &io).await?)
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
        let redirect_error_exits_shell = first_word_is_special_builtin(&args);
        io = match self.apply_redirects(io, &redirects, &expansion_io).await {
            Ok(io) => io,
            Err(error @ (ShellError::ReadonlyVariable(_) | ShellError::ShellDiagnostic(_))) => {
                return Err(error);
            }
            Err(error) => {
                return emit_command_error(error, expansion_io, redirect_error_exits_shell).await;
            }
        };

        if args.is_empty() {
            for (name, value) in assignments {
                self.assign_variable(name, value)?;
            }
            for fd in redirected_output_fds(&redirects) {
                let mut output = io.output(fd);
                close(&mut output).await?;
            }
            self.last_status = 0;
            return Ok(CommandStatus::SUCCESS);
        }

        for (name, _) in &assignments {
            self.ensure_assignable(name)?;
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
                self.assign_variable(name.clone(), value.clone())?;
            }
        }

        if builtin::is_builtin(&program) {
            let temporary_assignments = if special_builtin || assignments.is_empty() {
                None
            } else {
                Some(self.apply_temporary_assignments(&assignments)?)
            };
            let error_io = io.clone();
            let dispatch_result = builtin::dispatch(self, &program, &arguments, io).await;
            if let Some(temporary_assignments) = temporary_assignments {
                self.restore_temporary_assignments(temporary_assignments);
            }
            let status = match dispatch_result {
                Ok(Some(status)) => status,
                Ok(None) => {
                    return Err(ShellError::message(format!(
                        "builtin {program:?} disappeared during dispatch"
                    )));
                }
                Err(error @ (ShellError::ReadonlyVariable(_) | ShellError::ShellDiagnostic(_))) => {
                    return Err(error);
                }
                Err(error) => return emit_command_error(error, error_io, special_builtin).await,
            };
            self.last_status = status.code();
            return Ok(if exec_requested {
                CommandStatus::exit(status.code())
            } else {
                status
            });
        }

        if let Some(function) = self.functions.get(&program).cloned() {
            let temporary_assignments = if assignments.is_empty() {
                None
            } else {
                Some(self.apply_temporary_assignments(&assignments)?)
            };
            let error_io = io.clone();
            let status_result = self.run_function(&function, arguments, io).await;
            if let Some(temporary_assignments) = temporary_assignments {
                self.restore_temporary_assignments(temporary_assignments);
            }
            let status = match status_result {
                Ok(status) => status,
                Err(error @ (ShellError::ReadonlyVariable(_) | ShellError::ShellDiagnostic(_))) => {
                    return Err(error);
                }
                Err(error) => return emit_command_error(error, error_io, exec_requested).await,
            };
            self.last_status = status.code();
            return Ok(if exec_requested {
                CommandStatus::exit(status.code())
            } else {
                status
            });
        }

        let error_io = io.clone();
        let status = match self.run_external(program, arguments, assignments, io).await {
            Ok(status) => status,
            Err(error @ (ShellError::ReadonlyVariable(_) | ShellError::ShellDiagnostic(_))) => {
                return Err(error);
            }
            Err(error) if exec_requested => {
                return emit_prefixed_command_error("exec", error, error_io, true).await;
            }
            Err(error) => return emit_command_error(error, error_io, false).await,
        };
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
        self.function_depth += 1;
        self.positional_parameters = arguments;
        let status = self.run_function_body(&function.body, io).await;
        self.function_depth -= 1;
        self.positional_parameters = saved;
        Ok(match status? {
            CommandStatus {
                code,
                flow: ControlFlow::Return | ControlFlow::Break(_) | ControlFlow::Continue(_),
            } => CommandStatus::new(code),
            status => status,
        })
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
        Ok(CommandStatus::new(i32::from(child.wait().await?)))
    }

    async fn resolve_program(&self, input: &str) -> Result<ResolvedProgram> {
        let search_path = self
            .variable("PATH")
            .and_then(|variable| variable.value.as_deref())
            .unwrap_or("/bin");
        for candidate in candidate_paths(&self.working_dir, input, search_path) {
            if self.platform.is_file(&candidate).await {
                return Ok(ResolvedProgram {
                    path: display_path(&candidate),
                });
            }
        }

        Err(ShellError::command_not_found(input))
    }

    fn default_output_write_mode(&self) -> WriteMode {
        if self.options.noclobber() {
            WriteMode::NoClobber
        } else {
            WriteMode::Truncate
        }
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
                        IoFileRedirectKind::Write => {
                            let path = self.resolve_redirect_path(target, expansion_io).await?;
                            io.set_output(
                                fd,
                                self.platform
                                    .open_output(&path, self.default_output_write_mode())
                                    .await?,
                            );
                        }
                        IoFileRedirectKind::Clobber => {
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
                                self.default_output_write_mode()
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
        Ok(self
            .expand_word_expansion(word, io)
            .await?
            .scalarize(&self.ifs_joiner()))
    }

    async fn expand_command_word(&mut self, word: &Word, io: &ExecutionIo) -> Result<Vec<String>> {
        let expansion = self.expand_word_expansion(word, io).await?;
        let fields = split_fields(expansion.fields, &self.ifs_config());
        self.expand_pathnames(fields).await
    }

    async fn expand_pattern(&mut self, word: &Word, io: &ExecutionIo) -> Result<String> {
        Ok(self
            .expand_word_expansion(word, io)
            .await?
            .render_pattern(&self.ifs_joiner()))
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

    async fn expand_word_expansion(&mut self, word: &Word, io: &ExecutionIo) -> Result<Expansion> {
        self.expand_text(&word.value, io, false).await
    }

    #[async_recursion(?Send)]
    async fn expand_text(
        &mut self,
        raw: &str,
        io: &ExecutionIo,
        quoted: bool,
    ) -> Result<Expansion> {
        let pieces = parser::parse_word(raw)?;
        self.expand_pieces(&pieces, io, quoted).await
    }

    #[async_recursion(?Send)]
    async fn expand_pieces(
        &mut self,
        pieces: &[WordPieceWithSource],
        io: &ExecutionIo,
        quoted: bool,
    ) -> Result<Expansion> {
        let mut fields = Vec::new();
        for piece in pieces {
            let expanded = self.expand_piece(&piece.piece, io, quoted).await?;
            let next_fields = if quoted && expanded.quote_behavior == QuoteBehavior::JoinFields {
                vec![join_quoted_fields(expanded.fields, &self.ifs_joiner())]
            } else {
                expanded.fields
            };
            append_fields(&mut fields, next_fields);
        }
        Ok(Expansion::from_fields(fields))
    }

    #[async_recursion(?Send)]
    async fn expand_piece(
        &mut self,
        piece: &WordPiece,
        io: &ExecutionIo,
        quoted: bool,
    ) -> Result<Expansion> {
        match piece {
            WordPiece::Text(text) | WordPiece::TildePrefix(text) => {
                Ok(Expansion::from_text(text.clone(), quoted))
            }
            WordPiece::SingleQuotedText(text) | WordPiece::AnsiCQuotedText(text) => {
                Ok(Expansion::from_text(text.clone(), true))
            }
            WordPiece::EscapeSequence(text) => Ok(Expansion::from_text(
                unescape_sequence(text).to_owned(),
                true,
            )),
            WordPiece::DoubleQuotedSequence(pieces)
            | WordPiece::GettextDoubleQuotedSequence(pieces) => {
                if pieces.is_empty() {
                    return Ok(Expansion::from_text(String::new(), true));
                }
                self.expand_pieces(pieces, io, true).await
            }
            WordPiece::ParameterExpansion(expr) => {
                self.expand_parameter_expr(expr, io, quoted).await
            }
            WordPiece::CommandSubstitution(command)
            | WordPiece::BackquotedCommandSubstitution(command) => Ok(Expansion::from_text(
                self.expand_command_substitution(command, io).await?,
                quoted,
            )),
            WordPiece::ArithmeticExpression(expr) => Ok(Expansion::from_text(
                self.expand_arithmetic_expression(expr)?,
                quoted,
            )),
        }
    }

    async fn expand_pathnames(&self, fields: Vec<WordField>) -> Result<Vec<String>> {
        let mut expanded = Vec::new();
        for field in fields {
            if self.options.noglob() || !field.has_glob_metacharacters() {
                expanded.push(field.render_text());
                continue;
            }

            let pattern = field.render_pattern();
            if pattern.is_empty() {
                expanded.push(String::new());
                continue;
            }

            let matches = self
                .platform
                .expand_glob(&self.working_dir, &pattern)
                .await?;
            if matches.is_empty() {
                expanded.push(field.render_text());
            } else {
                expanded.extend(matches);
            }
        }
        Ok(expanded)
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
        quoted: bool,
    ) -> Result<Expansion> {
        match expr {
            ParameterExpr::Parameter { parameter, .. } => self.expand_parameter(parameter, quoted),
            ParameterExpr::UseDefaultValues {
                parameter,
                test_type,
                default_value,
                ..
            } => {
                let state = self.parameter_state(parameter)?;
                if parameter_satisfies_test(&state, test_type) {
                    self.expand_parameter(parameter, quoted)
                } else {
                    self.expand_default_value(default_value.as_deref(), io, quoted)
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
                    self.expand_parameter(parameter, quoted)
                } else {
                    let value = self
                        .expand_default_value(default_value.as_deref(), io, quoted)
                        .await?;
                    self.assign_parameter(parameter, value.scalarize(&self.ifs_joiner()))?;
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
                    self.expand_parameter(parameter, quoted)
                } else {
                    Err(ShellError::shell_diagnostic(
                        self.parameter_error_message(parameter, test_type, error_message, io)
                            .await?,
                    ))
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
                    self.expand_default_value(alternative_value.as_deref(), io, quoted)
                        .await
                } else {
                    Ok(Expansion::default())
                }
            }
            ParameterExpr::ParameterLength { parameter, .. } => {
                let state = self.parameter_state(parameter)?;
                self.ensure_parameter_is_set_for_expansion(parameter, &state)?;
                Ok(Expansion::from_text(
                    state.value.chars().count().to_string(),
                    quoted,
                ))
            }
            ParameterExpr::RemoveSmallestSuffixPattern {
                parameter, pattern, ..
            } => {
                self.remove_parameter_pattern(
                    parameter,
                    pattern.as_deref(),
                    io,
                    PatternRemoval::SmallestSuffix,
                    quoted,
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
                    quoted,
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
                    quoted,
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
                    quoted,
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
        quoted: bool,
    ) -> Result<Expansion> {
        match raw {
            Some(raw) => self.expand_text(raw, io, quoted).await,
            None => Ok(Expansion::from_text(String::new(), quoted)),
        }
    }

    async fn remove_parameter_pattern(
        &mut self,
        parameter: &Parameter,
        pattern: Option<&str>,
        io: &ExecutionIo,
        removal: PatternRemoval,
        quoted: bool,
    ) -> Result<Expansion> {
        let state = self.parameter_state(parameter)?;
        self.ensure_parameter_is_set_for_expansion(parameter, &state)?;
        let value = state.value;
        let pattern = match pattern {
            Some(pattern) => self
                .expand_text(pattern, io, false)
                .await?
                .render_pattern(&self.ifs_joiner()),
            None => String::new(),
        };
        Ok(Expansion::from_text(
            remove_shell_pattern(&value, &pattern, removal)?,
            quoted,
        ))
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
                    .and_then(|variable| variable.value.clone())
                    .unwrap_or_default(),
                is_set: matches!(self.variable(name), Some(variable) if variable.value.is_some()),
            }),
            Parameter::NamedWithIndex { .. } | Parameter::NamedWithAllIndices { .. } => {
                Err(ShellError::unsupported("array parameter expansion"))
            }
        }
    }

    fn expand_parameter(&self, parameter: &Parameter, quoted: bool) -> Result<Expansion> {
        let state = self.parameter_state(parameter)?;
        self.ensure_parameter_is_set_for_expansion(parameter, &state)?;
        match parameter {
            Parameter::Positional(index) => {
                let index = usize::try_from(*index).map_err(|_| {
                    ShellError::message(format!("invalid positional parameter index {index}"))
                })?;
                Ok(Expansion::from_text(
                    self.positional_parameters
                        .get(index.saturating_sub(1))
                        .cloned()
                        .unwrap_or_default(),
                    quoted,
                ))
            }
            Parameter::Special(SpecialParameter::AllPositionalParameters { concatenate }) => {
                let fields = self
                    .positional_parameters
                    .iter()
                    .cloned()
                    .map(|value| WordField::from_text(value, quoted))
                    .collect();
                Ok(Expansion {
                    fields,
                    quote_behavior: if quoted && *concatenate {
                        QuoteBehavior::JoinFields
                    } else {
                        QuoteBehavior::PreserveFields
                    },
                })
            }
            Parameter::Special(special) => Ok(Expansion::from_text(
                self.special_parameter_state(special).value,
                quoted,
            )),
            Parameter::Named(name) => Ok(Expansion::from_text(
                self.variable(name)
                    .and_then(|variable| variable.value.clone())
                    .unwrap_or_default(),
                quoted,
            )),
            Parameter::NamedWithIndex { .. } | Parameter::NamedWithAllIndices { .. } => {
                Err(ShellError::unsupported("array parameter expansion"))
            }
        }
    }

    fn ensure_parameter_is_set_for_expansion(
        &self,
        parameter: &Parameter,
        state: &ParameterState,
    ) -> Result<()> {
        if !self.options.nounset() || state.is_set || allows_unset_with_nounset(parameter) {
            return Ok(());
        }

        Err(ShellError::shell_diagnostic(format!(
            "{}: parameter not set",
            parameter_display_name(parameter)
        )))
    }

    async fn parameter_error_message(
        &mut self,
        parameter: &Parameter,
        test_type: &ParameterTestType,
        error_message: &Option<String>,
        io: &ExecutionIo,
    ) -> Result<String> {
        if let Some(error_message) = error_message {
            if error_message.is_empty() {
                return Ok(format!(
                    "{}: {}",
                    parameter_display_name(parameter),
                    default_parameter_error_detail(test_type)
                ));
            }
            let detail = self
                .expand_text(error_message, io, false)
                .await?
                .scalarize(&self.ifs_joiner());
            return Ok(format!("{}: {detail}", parameter_display_name(parameter)));
        }

        Ok(format!(
            "{}: {}",
            parameter_display_name(parameter),
            default_parameter_error_detail(test_type)
        ))
    }

    fn special_parameter_state(&self, parameter: &SpecialParameter) -> ParameterState {
        match parameter {
            SpecialParameter::AllPositionalParameters { .. } => ParameterState {
                value: self.positional_parameters.join(&self.ifs_joiner()),
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
                value: self.options.current_option_flags(),
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
                let Some(value) = &variable.value else {
                    return Ok(0);
                };
                if value.is_empty() {
                    return Ok(0);
                }
                let parsed = brush_parser::arithmetic::parse(value).map_err(|error| {
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
                self.assign_variable(name.clone(), value.to_string())
            }
            ArithmeticTarget::ArrayElement(_, _) => {
                Err(ShellError::unsupported("arithmetic array assignments"))
            }
        }
    }

    fn assign_parameter(&mut self, parameter: &Parameter, value: String) -> Result<()> {
        match parameter {
            Parameter::Named(name) => self.assign_variable(name.clone(), value),
            _ => Err(ShellError::message(format!(
                "cannot assign through parameter {}",
                parameter_display_name(parameter)
            ))),
        }
    }
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum QuoteBehavior {
    #[default]
    PreserveFields,
    JoinFields,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Expansion {
    fields: Vec<WordField>,
    quote_behavior: QuoteBehavior,
}

impl Expansion {
    fn from_fields(fields: Vec<WordField>) -> Self {
        Self {
            fields,
            quote_behavior: QuoteBehavior::PreserveFields,
        }
    }

    fn from_text(value: String, quoted: bool) -> Self {
        Self::from_fields(vec![WordField::from_text(value, quoted)])
    }

    fn scalarize(&self, joiner: &str) -> String {
        self.fields
            .iter()
            .map(WordField::render_text)
            .collect::<Vec<_>>()
            .join(joiner)
    }

    fn render_pattern(&self, joiner: &str) -> String {
        self.fields
            .iter()
            .map(WordField::render_pattern)
            .collect::<Vec<_>>()
            .join(joiner)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WordField(Vec<ExpansionPiece>);

impl WordField {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn from_text(value: String, quoted: bool) -> Self {
        Self(vec![ExpansionPiece::from_text(value, quoted)])
    }

    fn append(&mut self, other: WordField) {
        self.0.extend(other.0);
    }

    fn push_char(&mut self, ch: char, splittable: bool) {
        match self.0.last_mut() {
            Some(ExpansionPiece::Splittable(existing)) if splittable => existing.push(ch),
            Some(ExpansionPiece::Unsplittable(existing)) if !splittable => existing.push(ch),
            _ => self
                .0
                .push(ExpansionPiece::from_text(ch.to_string(), !splittable)),
        }
    }

    fn has_unsplittable_piece(&self) -> bool {
        self.0.iter().any(ExpansionPiece::is_unsplittable)
    }

    fn render_text(&self) -> String {
        self.0.iter().map(ExpansionPiece::as_str).collect()
    }

    fn render_pattern(&self) -> String {
        let mut rendered = String::new();
        for piece in &self.0 {
            rendered.push_str(&piece.render_pattern());
        }
        rendered
    }

    fn has_glob_metacharacters(&self) -> bool {
        self.0.iter().any(ExpansionPiece::has_glob_metacharacters)
    }

    fn chars(&self) -> Vec<FieldChar> {
        let mut chars = Vec::new();
        for piece in &self.0 {
            let splittable = piece.is_splittable();
            for ch in piece.as_str().chars() {
                chars.push(FieldChar { ch, splittable });
            }
        }
        chars
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ExpansionPiece {
    Splittable(String),
    Unsplittable(String),
}

impl ExpansionPiece {
    fn from_text(value: String, quoted: bool) -> Self {
        if quoted {
            Self::Unsplittable(value)
        } else {
            Self::Splittable(value)
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Splittable(value) | Self::Unsplittable(value) => value,
        }
    }

    fn is_splittable(&self) -> bool {
        matches!(self, Self::Splittable(_))
    }

    fn is_unsplittable(&self) -> bool {
        matches!(self, Self::Unsplittable(_))
    }

    fn render_pattern(&self) -> String {
        match self {
            Self::Splittable(value) => value.clone(),
            Self::Unsplittable(value) => Pattern::escape(value),
        }
    }

    fn has_glob_metacharacters(&self) -> bool {
        self.is_splittable() && self.as_str().chars().any(is_glob_metacharacter)
    }
}

#[derive(Clone, Copy)]
struct FieldChar {
    ch: char,
    splittable: bool,
}

#[derive(Clone, Copy)]
enum DelimiterKind {
    Whitespace,
    NonWhitespace,
}

fn parameter_satisfies_test(state: &ParameterState, test_type: &ParameterTestType) -> bool {
    match test_type {
        ParameterTestType::UnsetOrNull => state.is_set && !state.value.is_empty(),
        ParameterTestType::Unset => state.is_set,
    }
}

fn default_parameter_error_detail(test_type: &ParameterTestType) -> &'static str {
    match test_type {
        ParameterTestType::UnsetOrNull => "parameter not set or null",
        ParameterTestType::Unset => "parameter not set",
    }
}

fn allows_unset_with_nounset(parameter: &Parameter) -> bool {
    matches!(
        parameter,
        Parameter::Special(SpecialParameter::AllPositionalParameters { .. })
    )
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

fn append_fields(target: &mut Vec<WordField>, mut next_fields: Vec<WordField>) {
    if next_fields.is_empty() {
        return;
    }

    if let Some(last) = target.last_mut() {
        let first = next_fields.remove(0);
        last.append(first);
    }

    target.extend(next_fields);
}

fn join_quoted_fields(fields: Vec<WordField>, joiner: &str) -> WordField {
    let mut joined = WordField::new();
    if fields.is_empty() {
        joined.0.push(ExpansionPiece::Unsplittable(String::new()));
        return joined;
    }

    for (index, field) in fields.into_iter().enumerate() {
        if index > 0 && !joiner.is_empty() {
            joined
                .0
                .push(ExpansionPiece::Unsplittable(joiner.to_owned()));
        }
        for piece in field.0 {
            joined.0.push(match piece {
                ExpansionPiece::Splittable(value) | ExpansionPiece::Unsplittable(value) => {
                    ExpansionPiece::Unsplittable(value)
                }
            });
        }
    }

    joined
}

fn split_fields(fields: Vec<WordField>, ifs: &IfsConfig<'_>) -> Vec<WordField> {
    let mut split = Vec::new();
    for field in fields {
        split.extend(split_field(field, ifs));
    }
    split
}

fn split_field(field: WordField, ifs: &IfsConfig<'_>) -> Vec<WordField> {
    let chars = field.chars();
    if chars.is_empty() {
        return if field.has_unsplittable_piece() {
            vec![WordField::new()]
        } else {
            Vec::new()
        };
    }

    if ifs.is_empty() {
        return vec![field];
    }

    let mut fields = Vec::new();
    let mut current = WordField::new();
    let mut index = 0;
    while index < chars.len() {
        if let Some((kind, next_index)) = parse_delimiter(&chars, index, *ifs) {
            match kind {
                DelimiterKind::Whitespace => {
                    if !current.0.is_empty() {
                        fields.push(std::mem::take(&mut current));
                    }
                }
                DelimiterKind::NonWhitespace => {
                    fields.push(std::mem::take(&mut current));
                }
            }
            index = next_index;
            continue;
        }

        let symbol = chars[index];
        current.push_char(symbol.ch, symbol.splittable);
        index += 1;
    }

    if !current.0.is_empty() {
        fields.push(current);
    }
    fields
}

fn parse_delimiter(
    chars: &[FieldChar],
    start: usize,
    ifs: IfsConfig<'_>,
) -> Option<(DelimiterKind, usize)> {
    if !chars.get(start)?.splittable || !ifs.contains(chars.get(start)?.ch) {
        return None;
    }

    let mut index = start;
    while index < chars.len() && chars[index].splittable && ifs.is_whitespace(chars[index].ch) {
        index += 1;
    }

    if index < chars.len() && chars[index].splittable && ifs.is_non_whitespace(chars[index].ch) {
        index += 1;
        while index < chars.len() && chars[index].splittable && ifs.is_whitespace(chars[index].ch) {
            index += 1;
        }
        return Some((DelimiterKind::NonWhitespace, index));
    }

    if index > start {
        return Some((DelimiterKind::Whitespace, index));
    }

    None
}

fn is_glob_metacharacter(ch: char) -> bool {
    matches!(ch, '*' | '?' | '[')
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
    if status.is_interrupting() {
        return status;
    }
    if bang {
        CommandStatus::new(if status.is_success() { 1 } else { 0 })
    } else {
        status
    }
}

async fn emit_command_error(
    error: ShellError,
    io: ExecutionIo,
    shell_exit: bool,
) -> Result<CommandStatus> {
    emit_formatted_command_error(format!("{error}"), error, io, shell_exit).await
}

async fn emit_prefixed_command_error(
    prefix: &str,
    error: ShellError,
    io: ExecutionIo,
    shell_exit: bool,
) -> Result<CommandStatus> {
    emit_formatted_command_error(format!("{prefix}: {error}"), error, io, shell_exit).await
}

async fn emit_formatted_command_error(
    message: String,
    error: ShellError,
    io: ExecutionIo,
    shell_exit: bool,
) -> Result<CommandStatus> {
    let mut stderr = io.output(2);
    let line = format!("{message}\n");
    write_all(&mut stderr, line.as_bytes()).await?;
    let code = command_error_status(&error);
    Ok(if shell_exit {
        CommandStatus::exit(code)
    } else {
        CommandStatus::new(code)
    })
}

fn command_error_status(error: &ShellError) -> i32 {
    match error {
        ShellError::CommandNotFound(_) => 127,
        _ => 2,
    }
}

fn first_word_is_special_builtin(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some(name) if is_special_builtin(name))
}

fn is_special_builtin(name: &str) -> bool {
    matches!(
        name,
        "." | ":"
            | "break"
            | "continue"
            | "eval"
            | "exec"
            | "exit"
            | "export"
            | "readonly"
            | "return"
            | "set"
            | "shift"
            | "unset"
    )
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

fn redirected_output_fds(redirects: &[IoRedirect]) -> Vec<IoFd> {
    let mut fds = Vec::new();
    for redirect in redirects {
        match redirect {
            IoRedirect::File(fd, kind, _)
                if matches!(
                    kind,
                    IoFileRedirectKind::Write
                        | IoFileRedirectKind::Clobber
                        | IoFileRedirectKind::Append
                        | IoFileRedirectKind::ReadAndWrite
                        | IoFileRedirectKind::DuplicateOutput
                ) =>
            {
                let fd = effective_fd(*fd, kind);
                if !fds.contains(&fd) {
                    fds.push(fd);
                }
            }
            IoRedirect::OutputAndError(_, _) => {
                for fd in [1, 2] {
                    if !fds.contains(&fd) {
                        fds.push(fd);
                    }
                }
            }
            _ => {}
        }
    }
    fds
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

fn source_candidate_paths(cwd: &Path, input: &str, search_path: &str) -> Vec<PathBuf> {
    if input.contains('/') || input.starts_with('.') {
        return vec![resolve_path(cwd, Path::new(input))];
    }

    search_path
        .split(':')
        .filter(|segment| !segment.is_empty())
        .map(|directory| resolve_path(cwd, Path::new(directory)).join(input))
        .collect()
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
    fn reserved_kernel_process_id_is_not_exported_to_external_commands() {
        let (status, state) = run_script_with_bootstrap(
            "printenv HELIOS_PROCESS_ID\n",
            [("printenv", printenv_command)],
            Bootstrap {
                shell_name: "dash".to_owned(),
                process_id: "42".to_owned(),
                positional_parameters: Vec::new(),
                working_dir: PathBuf::from("/"),
                environment: vec![(HELIOS_PROCESS_ID_ENV.to_owned(), "42".to_owned())],
            },
        );
        assert!(status.is_success(), "script exited with {}", status.code());
        assert!(state.stdout.is_empty());
    }

    #[test]
    fn builtin_cat_reads_pipeline_input() {
        let state = run_script("producer | cat\n", [("producer", producer_command)]);
        assert_eq!(state.stdout, b"pipe-data\n");
    }

    #[test]
    fn read_builtin_consumes_input_and_obeys_ifs_rules() {
        let state = run_script(
            "words | { read first rest; echo \"<$first>|<$rest>\"; }\ncolon | { IFS=: read x y z; echo \"[$x]|[$y]|[$z]\"; }\n",
            [("words", words_command), ("colon", colon_command)],
        );
        assert_eq!(state.stdout, b"<a>|<b c>\n[]|[]|[a::b::]\n");
    }

    #[test]
    fn read_builtin_supports_backslash_processing_and_raw_mode() {
        let state = run_script(
            "escaped | { read cooked; echo \"<$cooked>\"; }\nescaped | { read -r raw_left raw_right; echo \"[$raw_left]|[$raw_right]\"; }\n",
            [("escaped", escaped_command)],
        );
        assert_eq!(state.stdout, b"<a b>\n[a\\]|[b]\n");
    }

    #[test]
    fn read_builtin_returns_eof_status_after_assigning_partial_line() {
        let state = run_script(
            "nonewline | { read value; echo \"status=$? value=<$value>\"; }\n",
            [("nonewline", nonewline_command)],
        );
        assert_eq!(state.stdout, b"status=1 value=<abc>\n");
    }

    #[test]
    fn read_builtin_errors_do_not_exit_shell_and_keep_partial_assignments() {
        let state = run_script(
            "words | { readonly second; read first second; echo \"status=$? first=<$first> second=<$second>\"; }\nempty | { read; echo \"arg=$?\"; }\nempty | { read -z value; echo \"opt=$?\"; }\nwords | { read first foo-bar; echo \"name=$? first=<$first>\"; }\n",
            [("words", words_command), ("empty", empty_command)],
        );
        assert_eq!(
            state.stdout,
            b"status=2 first=<a> second=<>\narg=2\nopt=2\nname=2 first=<a>\n"
        );
        let stderr = String::from_utf8(state.stderr).expect("stderr should be utf-8");
        assert!(stderr.contains("read: second: is read only\n"));
        assert!(stderr.contains("read: arg count\n"));
        assert!(stderr.contains("read: Illegal option -z\n"));
        assert!(stderr.contains("read: foo-bar: bad variable name\n"));
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

    #[test]
    fn field_splitting_honors_ifs_rules() {
        let state = run_script(
            "x=\"a b\"\nfor value in $x; do echo \"<$value>\"; done\nIFS=,:\ny=\",a::b,\"\nfor value in $y; do echo \"<$value>\"; done\n",
            [],
        );
        assert_eq!(state.stdout, b"<a>\n<b>\n<>\n<a>\n<>\n<b>\n");
    }

    #[test]
    fn quoted_dollar_at_preserves_field_boundaries() {
        let state = run_script(
            "f(){ for value in \"x$@y\"; do echo \"<$value>\"; done; }\nf a b\nf\n",
            [],
        );
        assert_eq!(state.stdout, b"<xa>\n<by>\n<xy>\n");
    }

    #[test]
    fn command_words_expand_globs_and_retain_non_matches() {
        let state = run_script(
            ": >/a.rs\n: >/b.rs\nfor value in *.rs; do echo \"<$value>\"; done\nfor value in *.txt; do echo \"[$value]\"; done\nfor value in [; do echo \"{$value}\"; done\n",
            [],
        );
        assert_eq!(state.stdout, b"<a.rs>\n<b.rs>\n[*.txt]\n{[}\n");
    }

    #[test]
    fn empty_redirection_creates_truncated_file() {
        let state = run_script(": >/empty.log\n", []);
        assert_eq!(
            state.files.get(Path::new("/empty.log")).map(Vec::as_slice),
            Some(&[][..])
        );
    }

    #[test]
    fn loop_control_builtins_follow_dash_semantics() {
        let state = run_script(
            "for value in a b c; do echo \"$value\"; break; done\nfor value in a b c; do [ \"$value\" = a ] && continue; echo \"$value\"; done\nbreak\ncontinue\n",
            [],
        );
        assert_eq!(state.stdout, b"a\nb\nc\n");
    }

    #[test]
    fn return_exits_functions_and_top_level_shell() {
        let (status, state) = run_script_with_status(
            "f(){ return 9; echo no; }\nf\necho after-function\nreturn 7\necho no\n",
            [],
        );
        assert_eq!(status.code(), 7);
        assert!(status.is_exit());
        assert_eq!(state.stdout, b"after-function\n");
    }

    #[test]
    fn function_return_preserves_shell_status_width() {
        let state = run_script("f(){ return 256; }\nf\necho \"$?\"\n", []);
        assert_eq!(state.stdout, b"256\n");
    }

    #[test]
    fn set_and_shift_manage_positional_parameters() {
        let state = run_script(
            "set foo bar\nfor value in \"$@\"; do echo \"<$value>\"; done\nshift\nfor value in \"$@\"; do echo \"[$value]\"; done\nset --\necho \"$#\"\n",
            [],
        );
        assert_eq!(state.stdout, b"<foo>\n<bar>\n[bar]\n0\n");
    }

    #[test]
    fn set_supports_allexport_and_tracks_current_flags() {
        let state = run_script(
            "set -a\nfoo=bar\nreadonly ro\nprintenv foo\nprintenv ro\necho \"$-\"\nset +a\nbar=baz\nprintenv bar\n",
            [("printenv", printenv_command)],
        );
        assert_eq!(state.stdout, b"bar\na\n");
    }

    #[test]
    fn set_supports_nounset_and_parameter_errors_match_dash() {
        let (status, state) = run_script_with_status("set -u\necho \"$-\"\necho \"$foo\"\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert_eq!(state.stdout, b"u\n");
        assert_eq!(state.stderr, b"foo: parameter not set\n");

        let (status, state) = run_script_with_status("set -u\necho \"$1\"\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert_eq!(state.stderr, b"1: parameter not set\n");

        let (status, state) = run_script_with_status("set -u\necho \"$!\"\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert_eq!(state.stderr, b"!: parameter not set\n");
    }

    #[test]
    fn nounset_respects_dash_exceptions_and_parameter_operators() {
        let state = run_script(
            "set -u\necho \"<$@>|<$*>\"\necho $((foo + 1))\necho \"${foo-bar}\" \"${foo:-bar}\"\n: \"${foo:=baz}\"\necho \"$foo\"\n",
            [],
        );
        assert_eq!(state.stdout, b"<>|<>\n1\nbar bar\nbaz\n");
    }

    #[test]
    fn parameter_error_operator_uses_dash_messages() {
        let (status, state) = run_script_with_status("echo ${foo?custom msg}\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert_eq!(state.stderr, b"foo: custom msg\n");

        let (status, state) = run_script_with_status("foo=\necho ${foo:?}\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert_eq!(state.stderr, b"foo: parameter not set or null\n");
    }

    #[test]
    fn nounset_in_command_substitution_does_not_abort_parent_shell() {
        let state = run_script("set -u\necho \"$(echo $foo)\"\necho after\n", []);
        assert_eq!(state.stdout, b"\nafter\n");
        assert_eq!(state.stderr, b"foo: parameter not set\n");
    }

    #[test]
    fn set_supports_dash_option_ordering_for_flags_and_named_listings() {
        let state = run_script(
            ": >/a.rs\nset -a -C -u -f\necho \"$-\"\necho *.rs\nset -o\nset +o\nset +o noglob\necho *.rs\n",
            [],
        );
        let stdout = String::from_utf8(state.stdout).expect("stdout should be utf-8");
        assert!(stdout.starts_with("uaCf\n*.rs\n"));
        assert!(stdout.contains("Current option settings\n"));
        assert!(stdout.contains("noglob          on\n"));
        assert!(stdout.contains("noclobber       on\n"));
        assert!(stdout.contains("allexport       on\n"));
        assert!(stdout.contains("nounset         on\n"));
        assert!(stdout.contains("set -o noglob\n"));
        assert!(stdout.contains("set -o noclobber\n"));
        assert!(stdout.contains("set -o allexport\n"));
        assert!(stdout.contains("set -o nounset\n"));
        let noglob = stdout
            .find("noglob          on\n")
            .expect("noglob should be listed");
        let noclobber = stdout
            .find("noclobber       on\n")
            .expect("noclobber should be listed");
        let allexport = stdout
            .find("allexport       on\n")
            .expect("allexport should be listed");
        let nounset = stdout
            .find("nounset         on\n")
            .expect("nounset should be listed");
        assert!(noglob < noclobber && noclobber < allexport && allexport < nounset);
        let reusable_noglob = stdout
            .find("set -o noglob\n")
            .expect("noglob reusable command should be listed");
        let reusable_noclobber = stdout
            .find("set -o noclobber\n")
            .expect("noclobber reusable command should be listed");
        let reusable_allexport = stdout
            .find("set -o allexport\n")
            .expect("allexport reusable command should be listed");
        let reusable_nounset = stdout
            .find("set -o nounset\n")
            .expect("nounset reusable command should be listed");
        assert!(
            reusable_noglob < reusable_noclobber
                && reusable_noclobber < reusable_allexport
                && reusable_allexport < reusable_nounset
        );
        assert!(stdout.ends_with("a.rs\n"));
    }

    #[test]
    fn noclobber_rejects_plain_overwrite_but_allows_clobber_and_append() {
        let state = run_script(
            "echo seed >/log\nset -C\necho blocked >/log\necho \"status:$?\"\n>|/log\necho \"clobber:$?\"\necho ok >>/log\necho \"append:$?\"\ncat /log\n",
            [],
        );
        assert_eq!(state.stdout, b"status:2\nclobber:0\nappend:0\nok\n");
        assert_eq!(state.stderr, b"cannot create /log: File exists\n");
    }

    #[test]
    fn noclobber_redirect_errors_exit_special_builtins() {
        let (status, state) = run_script_with_status(": >/log\nset -C\n: >/log\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert!(state.stdout.is_empty());
        assert_eq!(state.stderr, b"cannot create /log: File exists\n");
    }

    #[test]
    fn command_not_found_uses_dash_status_codes_without_aborting_shell() {
        let state = run_script("missing\necho \"$?\"\necho after\n", []);
        assert_eq!(state.stdout, b"127\nafter\n");
        assert_eq!(state.stderr, b"missing: not found\n");
    }

    #[test]
    fn exec_missing_exits_shell_with_127() {
        let (status, state) = run_script_with_status("exec missing\necho no\n", []);
        assert_eq!(status.code(), 127);
        assert!(status.is_exit());
        assert!(state.stdout.is_empty());
        assert_eq!(state.stderr, b"exec: missing: not found\n");
    }

    #[test]
    fn invalid_set_options_exit_noninteractive_shell() {
        let (status, state) = run_script_with_status("set -z\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert_eq!(state.stderr, b"set: Illegal option -z\n");

        let (status, state) = run_script_with_status("set -o nope\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert_eq!(state.stderr, b"set: Illegal option -o nope\n");
    }

    #[test]
    fn set_lists_variables_with_shell_quoting() {
        let state = run_script("foo=bar\nbaz='a b'\nempty=\nset\n", []);
        let stdout = String::from_utf8(state.stdout).expect("stdout should be utf-8");
        assert!(stdout.contains("foo='bar'\n"));
        assert!(stdout.contains("baz='a b'\n"));
        assert!(stdout.contains("empty=''\n"));
    }

    #[test]
    fn readonly_builtin_lists_unset_and_empty_values() {
        let state = run_script(
            "readonly foo\nreadonly bar=\nreadonly baz='a b'\nreadonly\n",
            [],
        );
        assert_eq!(
            state.stdout,
            b"readonly bar=''\nreadonly baz='a b'\nreadonly foo\n"
        );
    }

    #[test]
    fn readonly_assignments_exit_noninteractive_shell() {
        let (status, state) = run_script_with_status("readonly foo=bar\nfoo=baz\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert!(state.stdout.is_empty());
        assert_eq!(state.stderr, b"foo: is read only\n");

        let (status, state) =
            run_script_with_status("readonly foo=bar\nfoo=zzz true\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert!(state.stdout.is_empty());
        assert_eq!(state.stderr, b"foo: is read only\n");
    }

    #[test]
    fn readonly_special_builtins_report_builtin_name() {
        let (status, state) =
            run_script_with_status("readonly foo=bar\nreadonly foo=baz\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert!(state.stdout.is_empty());
        assert_eq!(state.stderr, b"readonly: foo: is read only\n");

        let (status, state) = run_script_with_status("readonly foo\nunset foo\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert!(state.stdout.is_empty());
        assert_eq!(state.stderr, b"unset: foo: is read only\n");

        let (status, state) = run_script_with_status(
            "readonly foo\nexport foo\nexport\nexport foo=baz\necho no\n",
            [],
        );
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        let stdout = String::from_utf8(state.stdout).expect("stdout should be utf-8");
        assert!(stdout.contains("export foo\n"));
        assert_eq!(state.stderr, b"export: foo: is read only\n");
    }

    #[test]
    fn special_builtins_reject_bad_variable_names() {
        let (status, state) = run_script_with_status("export foo-bar=1\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert_eq!(state.stderr, b"export: foo-bar: bad variable name\n");

        let (status, state) = run_script_with_status("readonly foo-bar\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert_eq!(state.stderr, b"readonly: foo-bar: bad variable name\n");

        let (status, state) = run_script_with_status("unset foo-bar\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert_eq!(state.stderr, b"unset: foo-bar: bad variable name\n");
    }

    #[test]
    fn readonly_blocks_parameter_and_arithmetic_assignments() {
        let (status, state) = run_script_with_status("readonly foo\n: ${foo:=bar}\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert!(state.stdout.is_empty());
        assert_eq!(state.stderr, b"foo: is read only\n");

        let (status, state) = run_script_with_status("readonly x=1\necho $((x=2))\necho no\n", []);
        assert_eq!(status.code(), 2);
        assert!(status.is_exit());
        assert!(state.stdout.is_empty());
        assert_eq!(state.stderr, b"x: is read only\n");
    }

    #[test]
    fn cd_warns_for_readonly_pwd_and_oldpwd_but_still_changes_directory() {
        let state = run_script("readonly PWD\nreadonly OLDPWD\ncd /tmp\npwd\n", []);
        assert_eq!(state.stdout, b"/tmp\n");
        let stderr = String::from_utf8(state.stderr).expect("stderr should be utf-8");
        assert!(stderr.contains("cd: PWD: is read only\n"));
        assert!(stderr.contains("cd: OLDPWD: is read only\n"));
    }

    #[test]
    fn temporary_assignments_apply_to_non_special_builtins_without_leaking() {
        let state = run_script("HOME=/tmp cd\npwd\necho \"${HOME+set}\"\n", []);
        assert_eq!(state.stdout, b"/tmp\n\n");
    }

    #[test]
    fn temporary_assignments_scope_shell_functions_like_dash() {
        let state = run_script(
            "f(){ echo \"in:$foo\"; foo=inner; }\nfoo=outer\nfoo=temp f\necho \"out:$foo\"\n",
            [],
        );
        assert_eq!(state.stdout, b"in:temp\nout:outer\n");
    }

    #[test]
    fn eval_runs_in_current_shell_context() {
        let state = run_script(
            "foo=bar\neval echo '$foo'\neval set -- c d\nfor value in \"$@\"; do echo \"<$value>\"; done\n",
            [],
        );
        assert_eq!(state.stdout, b"bar\n<c>\n<d>\n");
    }

    #[test]
    fn dot_sources_file_and_plainifies_return_status() {
        let state = run_script(
            "emit_source >/bin/source-me\n. source-me\necho \"$foo\"\nf(){ . /bin/return-me; echo \"dot=$?\"; echo after; }\nemit_return >/bin/return-me\nf\n",
            [
                ("emit_source", emit_source_command),
                ("emit_return", emit_return_command),
            ],
        );
        assert_eq!(state.stdout, b"sourced\nbar\ndot=4\nafter\n");
    }

    #[test]
    fn special_builtin_errors_exit_noninteractive_shell() {
        let (shift_status, shift_state) = run_script_with_status("shift x\necho no\n", []);
        assert_eq!(shift_status.code(), 2);
        assert!(shift_status.is_exit());
        assert!(shift_state.stdout.is_empty());

        let (eval_status, eval_state) = run_script_with_status("eval if\necho no\n", []);
        assert_eq!(eval_status.code(), 2);
        assert!(eval_status.is_exit());
        assert!(eval_state.stdout.is_empty());

        let (dot_status, dot_state) = run_script_with_status(". missing\necho no\n", []);
        assert_eq!(dot_status.code(), 2);
        assert!(dot_status.is_exit());
        assert!(dot_state.stdout.is_empty());
    }

    #[test]
    fn subshell_and_pipeline_exit_do_not_abort_parent_shell() {
        let state = run_script(
            "(exit 7)\necho after-subshell\nexit 3 | cat\necho after-pipeline\n",
            [],
        );
        assert_eq!(state.stdout, b"after-subshell\nafter-pipeline\n");
    }

    fn run_script<const N: usize>(
        script: &str,
        commands: [(&'static str, TestCommandHandler); N],
    ) -> TestState {
        let (status, state) = run_script_with_status(script, commands);
        assert!(status.is_success(), "script exited with {}", status.code());
        state
    }

    fn run_script_with_status<const N: usize>(
        script: &str,
        commands: [(&'static str, TestCommandHandler); N],
    ) -> (CommandStatus, TestState) {
        run_script_with_bootstrap(
            script,
            commands,
            Bootstrap {
                shell_name: "dash".to_owned(),
                process_id: "1".to_owned(),
                positional_parameters: Vec::new(),
                working_dir: PathBuf::from("/"),
                environment: Vec::new(),
            },
        )
    }

    fn run_script_with_bootstrap<const N: usize>(
        script: &str,
        commands: [(&'static str, TestCommandHandler); N],
        bootstrap: Bootstrap,
    ) -> (CommandStatus, TestState) {
        let mut pool = LocalPool::new();
        let platform = TestPlatform::new(pool.spawner());
        for (name, command) in commands {
            platform.register_command(name, command);
        }

        let platform_for_run = platform.clone();
        let status = pool.run_until(async move {
            let mut shell = Shell::new(platform_for_run.clone(), bootstrap);
            let program = parser::parse(script).expect("script should parse");
            shell
                .run_program(&program)
                .await
                .expect("script should run")
        });

        (status, platform.snapshot())
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

    fn words_command(_invocation: TestInvocation) -> TestCommandResult {
        TestCommandResult {
            exit_code: 0,
            stdout: b"a b c\n".to_vec(),
            stderr: Vec::new(),
        }
    }

    fn colon_command(_invocation: TestInvocation) -> TestCommandResult {
        TestCommandResult {
            exit_code: 0,
            stdout: b"::a::b::\n".to_vec(),
            stderr: Vec::new(),
        }
    }

    fn escaped_command(_invocation: TestInvocation) -> TestCommandResult {
        TestCommandResult {
            exit_code: 0,
            stdout: b"a\\ b\n".to_vec(),
            stderr: Vec::new(),
        }
    }

    fn nonewline_command(_invocation: TestInvocation) -> TestCommandResult {
        TestCommandResult {
            exit_code: 0,
            stdout: b"abc".to_vec(),
            stderr: Vec::new(),
        }
    }

    fn empty_command(_invocation: TestInvocation) -> TestCommandResult {
        TestCommandResult {
            exit_code: 0,
            stdout: Vec::new(),
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

    fn emit_source_command(_invocation: TestInvocation) -> TestCommandResult {
        TestCommandResult {
            exit_code: 0,
            stdout: b"echo sourced\nfoo=bar\n".to_vec(),
            stderr: Vec::new(),
        }
    }

    fn emit_return_command(_invocation: TestInvocation) -> TestCommandResult {
        TestCommandResult {
            exit_code: 0,
            stdout: b"return 4\n".to_vec(),
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
            state.directories.insert(PathBuf::from("/tmp"));
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
            if matches!(mode, WriteMode::NoClobber) && self.state.borrow().files.contains_key(path)
            {
                return Err(crate::platform::existing_output_error(path));
            }
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

        async fn expand_glob(&self, cwd: &Path, pattern: &str) -> Result<Vec<String>> {
            let raw_path = Path::new(pattern);
            let absolute_pattern = if raw_path.is_absolute() {
                raw_path.to_path_buf()
            } else {
                cwd.join(raw_path)
            };
            let relative = !raw_path.is_absolute();
            let rendered_pattern = absolute_pattern.display().to_string();
            let pattern = match Pattern::new(&rendered_pattern) {
                Ok(pattern) => pattern,
                Err(_) => return Ok(Vec::new()),
            };

            let options = MatchOptions {
                case_sensitive: true,
                require_literal_separator: true,
                require_literal_leading_dot: true,
            };

            let state = self.state.borrow();
            let mut matches = state
                .files
                .keys()
                .chain(state.directories.iter())
                .filter(|path| pattern.matches_path_with(path, options))
                .cloned()
                .collect::<Vec<_>>();
            matches.sort();
            matches.dedup();
            Ok(matches
                .into_iter()
                .map(|path| crate::platform::render_glob_match(path, cwd, relative))
                .collect())
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

        fn poll_close(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            if !self.initialized {
                self.initialized = true;
                let mut state = self.state.borrow_mut();
                let file = state.files.entry(self.path.clone()).or_default();
                if !self.append {
                    file.clear();
                }
            }
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Clone, Copy)]
    enum CaptureTarget {
        Stdout,
        Stderr,
    }
}
