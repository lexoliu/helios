use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use async_recursion::async_recursion;
use brush_parser::ast::{
    AndOr, Command, CommandPrefixOrSuffixItem, CompoundCommand, CompoundList, CompoundListItem,
    ElseClause, ForClauseCommand, FunctionBody, FunctionDefinition, IfClauseCommand, IoFd,
    IoFileRedirectKind, IoFileRedirectTarget, IoHereDocument, IoRedirect, Pipeline,
    PipelineOperator, Program, SeparatorOperator, SimpleCommand, WhileOrUntilClauseCommand, Word,
};
use futures::channel::oneshot;

use crate::builtin;
use crate::error::{Result, ShellError};
use crate::platform::{ResolvedProgram, RunningProcess, ShellPlatform, SpawnRequest, WriteMode};
use crate::streams::{InputStream, OutputStream};

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

#[derive(Clone)]
pub struct Shell<P> {
    pub(crate) platform: P,
    pub(crate) variables: HashMap<String, Variable>,
    pub(crate) functions: HashMap<String, FunctionDefinition>,
    pub(crate) shell_name: String,
    pub(crate) positional_parameters: Vec<String>,
    pub(crate) working_dir: PathBuf,
    pub(crate) last_status: u8,
    pub(crate) last_pipeline_statuses: Vec<u8>,
}

pub(crate) struct ExecutionIo {
    pub(crate) stdin: InputStream,
    pub(crate) stdout: OutputStream,
    pub(crate) stderr: OutputStream,
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
            positional_parameters: bootstrap.positional_parameters,
            working_dir: bootstrap.working_dir,
            last_status: 0,
            last_pipeline_statuses: vec![0],
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
        ExecutionIo {
            stdin: InputStream::empty(),
            stdout: self.platform.stdout(),
            stderr: self.platform.stderr(),
        }
    }

    fn fork(&self) -> Self {
        self.clone()
    }

    #[async_recursion(?Send)]
    pub async fn run_program(&mut self, program: &Program) -> Result<CommandStatus> {
        let mut last = CommandStatus::SUCCESS;
        for complete_command in &program.complete_commands {
            last = self.run_compound_list(complete_command).await?;
            if last.is_exit() {
                self.last_status = last.code();
                return Ok(last);
            }
        }
        self.last_status = last.code();
        Ok(last)
    }

    #[async_recursion(?Send)]
    async fn run_compound_list(&mut self, list: &CompoundList) -> Result<CommandStatus> {
        let mut last = CommandStatus::SUCCESS;
        for CompoundListItem(and_or, separator) in &list.0 {
            if matches!(separator, SeparatorOperator::Async) {
                return Err(ShellError::unsupported("background command execution"));
            }
            last = self.run_and_or(and_or).await?;
            if last.is_exit() {
                return Ok(last);
            }
        }
        Ok(last)
    }

    #[async_recursion(?Send)]
    async fn run_and_or(&mut self, and_or: &brush_parser::ast::AndOrList) -> Result<CommandStatus> {
        let mut last = self.run_pipeline(&and_or.first).await?;
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

            last = self.run_pipeline(pipeline).await?;
            if last.is_exit() {
                return Ok(last);
            }
        }

        Ok(last)
    }

    #[async_recursion(?Send)]
    async fn run_pipeline(&mut self, pipeline: &Pipeline) -> Result<CommandStatus> {
        if pipeline.timed.is_some() {
            return Err(ShellError::unsupported("pipeline timing"));
        }

        if pipeline.seq.len() == 1 {
            let status = self.run_command(&pipeline.seq[0], self.new_io()).await?;
            self.last_pipeline_statuses = vec![status.code()];
            self.last_status = status.code();
            return Ok(apply_pipeline_bang(status, pipeline.bang));
        }

        let mut stages = Vec::with_capacity(pipeline.seq.len());
        let mut next_input = None;
        let last_index = pipeline.seq.len() - 1;

        for (index, command) in pipeline.seq.iter().enumerate() {
            let stdin = next_input.take().unwrap_or_else(InputStream::empty);
            let stdout = if index == last_index {
                self.platform.stdout()
            } else {
                let (reader, writer) = self.platform.pipe();
                next_input = Some(reader);
                writer
            };
            let stderr = self.platform.stderr();
            stages.push(self.spawn_pipeline_stage(command.clone(), stdin, stdout, stderr));
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

    fn spawn_pipeline_stage(
        &self,
        command: Command,
        stdin: InputStream,
        stdout: OutputStream,
        stderr: OutputStream,
    ) -> StageHandle {
        let mut shell = self.fork();
        let (sender, receiver) = oneshot::channel();
        self.platform.spawn_task(Box::pin(async move {
            let result = shell
                .run_command(
                    &command,
                    ExecutionIo {
                        stdin,
                        stdout,
                        stderr,
                    },
                )
                .await;
            let _ = sender.send(result);
        }));
        StageHandle { result: receiver }
    }

    #[async_recursion(?Send)]
    async fn run_command(&mut self, command: &Command, io: ExecutionIo) -> Result<CommandStatus> {
        match command {
            Command::Simple(simple) => self.run_simple(simple, io).await,
            Command::Compound(compound, redirects) => {
                if redirects.is_some() {
                    return Err(ShellError::unsupported("redirected compound commands"));
                }
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
        _io: ExecutionIo,
    ) -> Result<CommandStatus> {
        match compound {
            CompoundCommand::BraceGroup(group) => self.run_compound_list(&group.list).await,
            CompoundCommand::Subshell(subshell) => {
                let mut shell = self.fork();
                shell.run_compound_list(&subshell.list).await
            }
            CompoundCommand::IfClause(if_clause) => self.run_if(if_clause).await,
            CompoundCommand::WhileClause(while_clause) => {
                self.run_while_like(while_clause, false).await
            }
            CompoundCommand::UntilClause(until_clause) => {
                self.run_while_like(until_clause, true).await
            }
            CompoundCommand::ForClause(for_clause) => self.run_for(for_clause).await,
            CompoundCommand::Arithmetic(_) => Err(ShellError::unsupported("arithmetic commands")),
            CompoundCommand::ArithmeticForClause(_) => {
                Err(ShellError::unsupported("arithmetic for loops"))
            }
            CompoundCommand::CaseClause(_) => Err(ShellError::unsupported("case clauses")),
        }
    }

    async fn run_if(&mut self, if_clause: &IfClauseCommand) -> Result<CommandStatus> {
        let condition = self.run_compound_list(&if_clause.condition).await?;
        if condition.is_exit() {
            return Ok(condition);
        }

        if condition.is_success() {
            return self.run_compound_list(&if_clause.then).await;
        }

        let Some(elses) = &if_clause.elses else {
            return Ok(condition);
        };
        for else_clause in elses {
            let status = self.run_else_clause(else_clause).await?;
            if status.is_exit() || status.is_success() || else_clause.condition.is_none() {
                return Ok(status);
            }
        }

        Ok(condition)
    }

    async fn run_else_clause(&mut self, else_clause: &ElseClause) -> Result<CommandStatus> {
        if let Some(condition) = &else_clause.condition {
            let status = self.run_compound_list(condition).await?;
            if status.is_exit() || !status.is_success() {
                return Ok(status);
            }
        }

        self.run_compound_list(&else_clause.body).await
    }

    async fn run_while_like(
        &mut self,
        clause: &WhileOrUntilClauseCommand,
        invert: bool,
    ) -> Result<CommandStatus> {
        let mut last = CommandStatus::SUCCESS;
        loop {
            let condition = self.run_compound_list(&clause.0).await?;
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
            last = self.run_compound_list(&clause.1.list).await?;
            if last.is_exit() {
                return Ok(last);
            }
        }
    }

    async fn run_for(&mut self, for_clause: &ForClauseCommand) -> Result<CommandStatus> {
        let values = match &for_clause.values {
            Some(values) => {
                let mut expanded = Vec::with_capacity(values.len());
                for value in values {
                    expanded.push(self.expand_word(value)?);
                }
                expanded
            }
            None => self.positional_parameters.clone(),
        };

        let mut last = CommandStatus::SUCCESS;
        for value in values {
            self.set_variable(for_clause.variable_name.clone(), value, Some(false));
            last = self.run_compound_list(&for_clause.body.list).await?;
            if last.is_exit() {
                return Ok(last);
            }
        }

        Ok(last)
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
                            self.expand_assignment_value(&assign.value)?,
                        ));
                    }
                    CommandPrefixOrSuffixItem::Word(word) => args.push(self.expand_word(word)?),
                    CommandPrefixOrSuffixItem::IoRedirect(redirect) => redirects.push(redirect),
                    CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {
                        return Err(ShellError::unsupported("process substitution"));
                    }
                }
            }
        }

        if let Some(word) = &command.word_or_name {
            args.push(self.expand_word(word)?);
        }

        if let Some(suffix) = &command.suffix {
            for item in &suffix.0 {
                match item {
                    CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                        args.push(self.expand_word(word)?);
                    }
                    CommandPrefixOrSuffixItem::Word(word) => args.push(self.expand_word(word)?),
                    CommandPrefixOrSuffixItem::IoRedirect(redirect) => redirects.push(redirect),
                    CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {
                        return Err(ShellError::unsupported("process substitution"));
                    }
                }
            }
        }

        io = self.apply_redirects(io, &redirects).await?;

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
            let status = self.run_function(&function, arguments).await?;
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
    ) -> Result<CommandStatus> {
        let saved = self.positional_parameters.clone();
        self.positional_parameters = arguments;
        let status = self.run_function_body(&function.body).await;
        self.positional_parameters = saved;
        status
    }

    async fn run_function_body(&mut self, body: &FunctionBody) -> Result<CommandStatus> {
        if body.1.is_some() {
            return Err(ShellError::unsupported("redirected function bodies"));
        }
        self.run_compound(&body.0, self.new_io()).await
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
                stdin: io.stdin,
                stdout: io.stdout,
                stderr: io.stderr,
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

    async fn apply_redirects(
        &self,
        mut io: ExecutionIo,
        redirects: &[&IoRedirect],
    ) -> Result<ExecutionIo> {
        for redirect in redirects {
            match redirect {
                IoRedirect::File(fd, kind, target) => {
                    let fd = effective_fd(*fd, kind);
                    match kind {
                        IoFileRedirectKind::Read => {
                            if fd != 0 {
                                return Err(ShellError::message(format!(
                                    "input redirection for fd {fd} is not supported"
                                )));
                            }
                            let path = self.resolve_redirect_target(target)?;
                            io.stdin = self.platform.open_input(&path).await?;
                        }
                        IoFileRedirectKind::Write | IoFileRedirectKind::Clobber => {
                            let path = self.resolve_redirect_target(target)?;
                            match fd {
                                1 => {
                                    io.stdout = self
                                        .platform
                                        .open_output(&path, WriteMode::Truncate)
                                        .await?
                                }
                                2 => {
                                    io.stderr = self
                                        .platform
                                        .open_output(&path, WriteMode::Truncate)
                                        .await?
                                }
                                _ => {
                                    return Err(ShellError::message(format!(
                                        "output redirection for fd {fd} is not supported"
                                    )));
                                }
                            }
                        }
                        IoFileRedirectKind::Append => {
                            let path = self.resolve_redirect_target(target)?;
                            match fd {
                                1 => {
                                    io.stdout =
                                        self.platform.open_output(&path, WriteMode::Append).await?
                                }
                                2 => {
                                    io.stderr =
                                        self.platform.open_output(&path, WriteMode::Append).await?
                                }
                                _ => {
                                    return Err(ShellError::message(format!(
                                        "output redirection for fd {fd} is not supported"
                                    )));
                                }
                            }
                        }
                        IoFileRedirectKind::ReadAndWrite => {
                            return Err(ShellError::unsupported("<> redirection"));
                        }
                        IoFileRedirectKind::DuplicateInput
                        | IoFileRedirectKind::DuplicateOutput => {
                            return Err(ShellError::unsupported("file descriptor duplication"));
                        }
                    }
                }
                IoRedirect::HereDocument(fd, document) => {
                    if fd.unwrap_or(0) != 0 {
                        return Err(ShellError::unsupported("here-documents for non-stdin fds"));
                    }
                    io.stdin = InputStream::from_bytes(self.render_here_document(document)?);
                }
                IoRedirect::HereString(fd, word) => {
                    if fd.unwrap_or(0) != 0 {
                        return Err(ShellError::unsupported("here-strings for non-stdin fds"));
                    }
                    let mut bytes = self.expand_word(word)?.into_bytes();
                    bytes.push(b'\n');
                    io.stdin = InputStream::from_bytes(bytes);
                }
                IoRedirect::OutputAndError(_, _) => {
                    return Err(ShellError::unsupported("&> redirection"));
                }
            }
        }
        Ok(io)
    }

    fn resolve_redirect_target(&self, target: &IoFileRedirectTarget) -> Result<PathBuf> {
        match target {
            IoFileRedirectTarget::Filename(word) => Ok(resolve_path(
                &self.working_dir,
                &PathBuf::from(self.expand_word(word)?),
            )),
            IoFileRedirectTarget::Fd(fd) => Err(ShellError::message(format!(
                "file descriptor target {fd} is not supported here"
            ))),
            IoFileRedirectTarget::ProcessSubstitution(_, _) => {
                Err(ShellError::unsupported("process substitution redirection"))
            }
            IoFileRedirectTarget::Duplicate(_) => {
                Err(ShellError::unsupported("duplicate redirection targets"))
            }
        }
    }

    fn render_here_document(&self, document: &IoHereDocument) -> Result<Vec<u8>> {
        let mut text = if document.requires_expansion {
            self.expand_word(&document.doc)?
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

    pub(crate) fn expand_word(&self, word: &Word) -> Result<String> {
        self.expand_text(&word.value)
    }

    fn expand_assignment_value(
        &self,
        value: &brush_parser::ast::AssignmentValue,
    ) -> Result<String> {
        use brush_parser::ast::AssignmentValue;
        match value {
            AssignmentValue::Scalar(word) => self.expand_word(word),
            AssignmentValue::Array(_) => Err(ShellError::unsupported("array assignments")),
        }
    }

    fn expand_text(&self, raw: &str) -> Result<String> {
        let mut out = String::with_capacity(raw.len());
        let bytes = raw.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\'' => {
                    index += 1;
                    while index < bytes.len() && bytes[index] != b'\'' {
                        out.push(bytes[index] as char);
                        index += 1;
                    }
                    index += 1;
                }
                b'"' => {
                    index += 1;
                    while index < bytes.len() && bytes[index] != b'"' {
                        if bytes[index] == b'$' {
                            let (value, next) = self.expand_dollar(raw, index)?;
                            out.push_str(&value);
                            index = next;
                            continue;
                        }
                        if bytes[index] == b'\\' && index + 1 < bytes.len() {
                            out.push(bytes[index + 1] as char);
                            index += 2;
                            continue;
                        }
                        out.push(bytes[index] as char);
                        index += 1;
                    }
                    index += 1;
                }
                b'\\' if index + 1 < bytes.len() => {
                    out.push(bytes[index + 1] as char);
                    index += 2;
                }
                b'$' => {
                    let (value, next) = self.expand_dollar(raw, index)?;
                    out.push_str(&value);
                    index = next;
                }
                byte => {
                    out.push(byte as char);
                    index += 1;
                }
            }
        }
        Ok(out)
    }

    fn expand_dollar(&self, raw: &str, start: usize) -> Result<(String, usize)> {
        let bytes = raw.as_bytes();
        if start + 1 >= bytes.len() {
            return Ok(("$".to_owned(), start + 1));
        }

        match bytes[start + 1] {
            b'?' => Ok((self.last_status.to_string(), start + 2)),
            b'#' => Ok((self.positional_parameters.len().to_string(), start + 2)),
            b'0' => Ok((self.shell_name.clone(), start + 2)),
            b'1'..=b'9' => {
                let mut end = start + 1;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                let index = raw[start + 1..end].parse::<usize>().map_err(|error| {
                    ShellError::message(format!("invalid positional parameter: {error}"))
                })?;
                Ok((
                    self.positional_parameters
                        .get(index.saturating_sub(1))
                        .cloned()
                        .unwrap_or_default(),
                    end,
                ))
            }
            b'@' | b'*' => Ok((self.positional_parameters.join(" "), start + 2)),
            b'{' => {
                let close = raw[start + 2..]
                    .find('}')
                    .ok_or_else(|| ShellError::message("unterminated parameter expansion"))?;
                let name = &raw[start + 2..start + 2 + close];
                let value = self
                    .variable(name)
                    .map(|variable| variable.value.clone())
                    .unwrap_or_default();
                Ok((value, start + 2 + close + 1))
            }
            _ => {
                let mut end = start + 1;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                if end == start + 1 {
                    return Ok(("$".to_owned(), start + 1));
                }
                let name = &raw[start + 1..end];
                let value = self
                    .variable(name)
                    .map(|variable| variable.value.clone())
                    .unwrap_or_default();
                Ok((value, end))
            }
        }
    }
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
    use futures::future::LocalBoxFuture;
    use futures::io::AsyncWrite;
    use futures::task::LocalSpawnExt;

    use super::*;
    use crate::parser;
    use crate::platform::{RunningProcess, ShellPlatform, SpawnRequest, WriteMode};
    use crate::streams::{InputStream, OutputStream, close, read_all, write_all};

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
    fn stderr_redirection_respects_requested_fd() {
        let state = run_script("errcmd 2>/err.log\n", [("errcmd", errcmd_command)]);
        assert_eq!(
            state.files.get(Path::new("/err.log")).map(Vec::as_slice),
            Some("bad\n".as_bytes())
        );
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
