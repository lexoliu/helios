use std::borrow::Cow::{self, Borrowed, Owned};
use std::cell::RefCell;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::rc::Rc;

use anyhow::{Context as _, Result};
use clap::error::ErrorKind;
use clap::{ColorChoice, Parser, Subcommand, ValueEnum};
use helios_shell_protocol::system::tracing;
use nu_ansi_term::{Color, Style as AnsiStyle};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{Highlighter, MatchingBracketHighlighter};
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::history::DefaultHistory;
use rustyline::validate::{
    MatchingBracketValidator, ValidationContext, ValidationResult, Validator,
};
use rustyline::Cmd;
use rustyline::{
    CompletionType, Config, Context as RustyContext, EditMode, Editor, Event as RustyEvent, Helper,
    KeyCode as RustyKeyCode, KeyEvent as RustyKeyEvent, Modifiers as RustyModifiers,
};
use strsim::normalized_damerau_levenshtein;

use crate::edit_tui;
use crate::filesystem;
use crate::guest_exec;
use crate::help;
use crate::rpc::RpcPane;
use crate::runtime;
use shell_core::guest_commands::{self, GuestCommand, GuestCommandSource};
use shell_core::{self, CommandStatus, ScriptHost, Statement};
use crate::serial::RpcClient;
use crate::stats_tui;
use crate::system::{self, TracingConfig};

type SharedEditor = Rc<RefCell<Editor<ShellHelper, DefaultHistory>>>;

pub fn run(mut client: RpcClient) -> Result<()> {
    let editor = Rc::new(RefCell::new(build_editor()?));
    let mut shell = Shell::new(InteractiveTerminal::new(Rc::clone(&editor))?);

    shell.print_banner()?;

    loop {
        let Some(input) = read_program(&editor)? else {
            return Ok(());
        };
        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        editor
            .borrow_mut()
            .add_history_entry(input)
            .context("failed to record shell history entry")?;

        match parse_input(input) {
            ParsedLine::Program(program) => {
                match runtime::block_on(runtime::interruptible(
                    shell.execute_program(&mut client, &program),
                ))
                .context("failed to listen for Ctrl+C during shell command execution")?
                {
                    runtime::CommandRun::Completed(result) => {
                        match result {
                            Ok(status) => {
                                if status.should_exit() {
                                    return Ok(());
                                }
                            }
                            Err(error) => shell.print_line(&format!("error: {error:#}"))?,
                        }
                    }
                    runtime::CommandRun::Interrupted => shell.print_line("interrupted")?,
                }
            }
            ParsedLine::Output(output) => shell.print_block(&output)?,
        }
    }
}

fn read_program(editor: &SharedEditor) -> Result<Option<String>> {
    let mut input = String::new();
    let mut prompt = PROMPT;
    loop {
        let line = match editor.borrow_mut().readline(prompt) {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => return Ok(Some(String::new())),
            Err(ReadlineError::Eof) => return Ok(None),
            Err(error) => return Err(error).context("failed to read shell input"),
        };
        if !input.is_empty() {
            input.push('\n');
        }
        input.push_str(&line);
        if !shell_core::needs_more_input(&input)? {
            return Ok(Some(input));
        }
        prompt = CONTINUATION_PROMPT;
    }
}

const PROMPT: &str = "helios> ";
const CONTINUATION_PROMPT: &str = "....> ";
const LIVE_STATS_PERIOD_MS: u64 = 1_000;
const ROOT_CANDIDATES: &[&str] = &[
    "help", "clear", "exit", "pwd", "ls", "cat", "edit", "mkdir", "rm", "cp", "mv", "touch", "echo", "test",
    "source", "stats", "exec", "tracing", "rpc", "if", "else", "end", "--help",
];
const HELP_CANDIDATES: &[&str] = &["overview", "stats", "tracing", "rpc", "--help"];
const STATS_CANDIDATES: &[&str] = &["--help"];
const TRACING_CANDIDATES: &[&str] = &["limit", "level", "targets", "--help"];
const TRACING_LEVEL_CANDIDATES: &[&str] = &["none", "error", "warn", "info", "debug", "trace"];
const RPC_CANDIDATES: &[&str] = &["instance", "func", "payload", "call", "--help"];
const RPC_INSTANCE_CANDIDATES: &[&str] = &[
    "helios:system/programs@0.1.0",
    "helios:system/stats@0.1.0",
    "helios:system/tracing@0.1.0",
    "helios:system/serial@0.1.0",
    "helios:system/sync@0.1.0",
];
const RPC_FUNC_CANDIDATES: &[&str] = &[
    "exec",
    "snapshot",
    "recent",
    "debug-port",
    "raw-mutex",
    "raw-rw-lock",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum HelpTopic {
    Overview,
    Stats,
    Tracing,
    Rpc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum TracingLevelArg {
    None,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Parser)]
#[command(
    name = "helios",
    no_binary_name = true,
    disable_version_flag = true,
    disable_help_subcommand = true,
    color = ColorChoice::Always,
    subcommand_required = true
)]
pub(crate) struct ReplCli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Show help for the shell or a specific command.
    Help {
        #[arg(value_enum)]
        topic: Option<HelpTopic>,
    },
    /// Clear the screen and move the cursor to the top-left corner.
    Clear,
    /// Leave the shell.
    Exit,
    /// Print the current remote working directory.
    Pwd,
    /// List files in the remote debugger filesystem.
    Ls { path: Option<String> },
    /// Print remote file contents.
    Cat { path: String },
    /// Edit a remote text file inside a full-screen terminal editor.
    Edit { path: String },
    /// Evaluate a remote filesystem predicate for scripts and conditionals.
    Test {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        expression: Vec<String>,
    },
    /// Execute a host-local shell script that controls the remote debugger session.
    Source { path: String },
    /// Create a directory in the remote debugger filesystem.
    Mkdir { path: String },
    /// Remove a file or an empty directory from the remote debugger filesystem.
    Rm { path: String },
    /// Copy a remote file inside the debugger filesystem.
    Cp { source: String, destination: String },
    /// Move or rename a remote path inside the debugger filesystem.
    Mv { source: String, destination: String },
    /// Create a file if it does not exist in the remote debugger filesystem.
    Touch { path: String },
    /// Print text or write it into a remote file with `>` or `>>`.
    Echo {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },
    /// Exec a wasm guest program from the remote filesystem with the default minimal rights set.
    Exec {
        path: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Open the live stats view.
    Stats,
    /// Stream tracing events until interrupted or configure tracing filters.
    Tracing {
        #[command(subcommand)]
        action: Option<TracingAction>,
    },
    /// Configure or invoke a raw remote system call.
    Rpc {
        #[command(subcommand)]
        action: RpcAction,
    },
}

#[derive(Debug, Subcommand)]
enum TracingAction {
    /// Set the maximum number of events fetched by `tracing`.
    Limit { count: u32 },
    /// Set the minimum tracing level.
    Level { level: TracingLevelArg },
    /// Set comma-separated tracing target prefixes. Empty clears the filter.
    Targets {
        #[arg(default_value = "")]
        prefixes: String,
    },
}

#[derive(Debug, Subcommand)]
enum RpcAction {
    /// Set the remote WIT instance identifier.
    Instance { name: String },
    /// Set the remote function name.
    Func { name: String },
    /// Set the request payload as hexadecimal bytes.
    Payload {
        #[arg(default_value = "")]
        hex: String,
    },
    /// Invoke the configured remote call.
    Call,
}

trait TerminalIo {
    fn print(&mut self, text: &str) -> Result<()>;
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()>;
    fn clear_screen(&mut self) -> Result<()>;
}

struct InteractiveTerminal {
    editor: SharedEditor,
    printer: Box<dyn rustyline::ExternalPrinter>,
}

struct Shell<T> {
    terminal: T,
    tracing: TracingConfig,
    rpc: RpcPane,
}

enum Command {
    Help(HelpTopic),
    Clear,
    Exit,
    Pwd,
    List(Option<String>),
    Cat(String),
    Edit(String),
    Test(Vec<String>),
    Source(String),
    Mkdir(String),
    Remove(String),
    Copy { source: String, destination: String },
    Move { source: String, destination: String },
    Touch(String),
    Echo(filesystem::EchoTarget),
    Exec { path: String, args: Vec<String> },
    ShowStats,
    ShowTracing,
    TracingLimit(u32),
    TracingLevel(Option<tracing::Level>),
    TracingTargets(Vec<String>),
    RpcInstance(String),
    RpcFunc(String),
    RpcPayload(String),
    RpcCall,
}

enum ParsedLine {
    Program(Vec<Statement>),
    Output(String),
}

enum ParsedCommandLine {
    Command(Command),
    Output(String),
}

struct ShellHelper {
    highlighter: MatchingBracketHighlighter,
    hinter: HistoryHinter,
    validator: MatchingBracketValidator,
    colored_prompt: String,
}

impl InteractiveTerminal {
    fn new(editor: SharedEditor) -> Result<Self> {
        let printer = editor
            .borrow_mut()
            .create_external_printer()
            .context("failed to create shell external printer")?;
        Ok(Self {
            editor,
            printer: Box::new(printer),
        })
    }
}

impl TerminalIo for InteractiveTerminal {
    fn print(&mut self, text: &str) -> Result<()> {
        self.printer
            .print(text.to_owned())
            .context("failed to print shell output")
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        match std::str::from_utf8(bytes) {
            Ok(text) => self.print(text),
            Err(_) => write_stdout(bytes),
        }
    }

    fn clear_screen(&mut self) -> Result<()> {
        self.editor
            .borrow_mut()
            .clear_screen()
            .context("failed to clear shell screen")
    }
}

struct ScriptRuntime<'a, T> {
    shell: &'a mut Shell<T>,
    client: &'a mut RpcClient,
}

#[async_trait::async_trait(?Send)]
impl<T: TerminalIo> ScriptHost for ScriptRuntime<'_, T> {
    async fn execute_line(&mut self, line: &str) -> Result<CommandStatus> {
        match parse_command_line(line)? {
            ParsedCommandLine::Command(command) => self.shell.execute_command(self.client, command).await,
            ParsedCommandLine::Output(output) => {
                self.shell.print_block(&output)?;
                Ok(CommandStatus::SUCCESS)
            }
        }
    }
}

impl GuestCommandSource for Command {
    fn pwd(&self) -> bool {
        matches!(self, Self::Pwd)
    }

    fn list_path(&self) -> Option<Option<&str>> {
        match self {
            Self::List(path) => Some(path.as_deref()),
            _ => None,
        }
    }

    fn cat_path(&self) -> Option<&str> {
        match self {
            Self::Cat(path) => Some(path),
            _ => None,
        }
    }

    fn test_expression(&self) -> Option<&[String]> {
        match self {
            Self::Test(expression) => Some(expression),
            _ => None,
        }
    }

    fn mkdir_path(&self) -> Option<&str> {
        match self {
            Self::Mkdir(path) => Some(path),
            _ => None,
        }
    }

    fn remove_path(&self) -> Option<&str> {
        match self {
            Self::Remove(path) => Some(path),
            _ => None,
        }
    }

    fn copy_paths(&self) -> Option<(&str, &str)> {
        match self {
            Self::Copy { source, destination } => Some((source, destination)),
            _ => None,
        }
    }

    fn move_paths(&self) -> Option<(&str, &str)> {
        match self {
            Self::Move { source, destination } => Some((source, destination)),
            _ => None,
        }
    }

    fn touch_path(&self) -> Option<&str> {
        match self {
            Self::Touch(path) => Some(path),
            _ => None,
        }
    }

    fn echo_target(&self) -> Option<&shell_core::guest_commands::EchoTarget> {
        match self {
            Self::Echo(target) => Some(target),
            _ => None,
        }
    }

    fn exec_program(&self) -> Option<(&str, &[String])> {
        match self {
            Self::Exec { path, args } => Some((path, args)),
            _ => None,
        }
    }
}

impl Command {
    fn as_guest_command(&self) -> Result<GuestCommand> {
        guest_commands::from_source(self)
            .ok_or_else(|| anyhow::anyhow!("command is not delegated to guest shell"))
    }
}

impl<T: TerminalIo> Shell<T> {
    fn new(terminal: T) -> Self {
        Self {
            terminal,
            tracing: TracingConfig::new(),
            rpc: RpcPane::new(),
        }
    }

    fn print_banner(&mut self) -> Result<()> {
        self.terminal
            .print("Helios shell ready. Type `help` or use `--help` on any command.\n")
    }

    fn print_line(&mut self, line: &str) -> Result<()> {
        let mut output = String::with_capacity(line.len() + 1);
        output.push_str(line);
        output.push('\n');
        self.terminal.print(&output)
    }

    fn print_block(&mut self, text: &str) -> Result<()> {
        let mut output = String::with_capacity(text.len() + 1);
        output.push_str(text);
        if !text.ends_with('\n') {
            output.push('\n');
        }
        self.terminal.print(&output)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.terminal.write_bytes(bytes)
    }

    async fn execute_program(
        &mut self,
        client: &mut RpcClient,
        program: &[Statement],
    ) -> Result<CommandStatus> {
        let mut host = ScriptRuntime { shell: self, client };
        shell_core::execute_script(&mut host, program).await
    }

    async fn execute_command(
        &mut self,
        client: &mut RpcClient,
        command: Command,
    ) -> Result<CommandStatus> {
        if let Some(status) = self.execute_immediate_command(&command)? {
            return Ok(status);
        }

        match command {
            Command::Edit(path) => edit_tui::run(client, &path).await?,
            Command::Source(path) => {
                let script = std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read shell script {path}"))?;
                let program = match parse_input(&script) {
                    ParsedLine::Program(program) => program,
                    ParsedLine::Output(output) => anyhow::bail!("{output}"),
                };
                return self.execute_program(client, &program).await;
            }
            Command::ShowStats => stats_tui::run(client, LIVE_STATS_PERIOD_MS).await?,
            Command::ShowTracing => self.show_tracing(client).await?,
            Command::TracingLimit(limit) => {
                self.tracing.limit = limit;
                self.print_line(&format!("tracing limit set to {limit}"))?;
            }
            Command::TracingLevel(level) => {
                self.tracing.min_level = level;
                self.print_line(&format!(
                    "tracing level set to {}",
                    tracing_level_name(level)
                ))?;
            }
            Command::TracingTargets(prefixes) => {
                self.tracing.target_prefixes = prefixes;
                self.print_line(&format!(
                    "tracing targets set to {}",
                    system::format_targets(&self.tracing.target_prefixes)
                ))?;
            }
            Command::RpcInstance(instance) => {
                self.rpc.instance = instance;
                self.print_line("rpc instance updated")?;
            }
            Command::RpcFunc(func) => {
                self.rpc.func = func;
                self.print_line("rpc function updated")?;
            }
            Command::RpcPayload(payload) => {
                self.rpc.request_hex = payload;
                self.print_line("rpc payload updated")?;
            }
            Command::RpcCall => self.call_rpc(client).await?,
            Command::Pwd
            | Command::List(_)
            | Command::Cat(_)
            | Command::Test(_)
            | Command::Mkdir(_)
            | Command::Remove(_)
            | Command::Copy { .. }
            | Command::Move { .. }
            | Command::Touch(_)
            | Command::Echo(_)
            | Command::Exec { .. } => return self.execute_guest_command(client, &command).await,
            Command::Help(_) | Command::Clear | Command::Exit => unreachable!(),
        }

        Ok(CommandStatus::SUCCESS)
    }

    fn execute_immediate_command(&mut self, command: &Command) -> Result<Option<CommandStatus>> {
        let status = match command {
            Command::Help(topic) => {
                self.print_block(&render_help(*topic))?;
                CommandStatus::SUCCESS
            }
            Command::Clear => {
                self.terminal.clear_screen()?;
                CommandStatus::SUCCESS
            }
            Command::Exit => CommandStatus::exiting(0),
            _ => return Ok(None),
        };
        Ok(Some(status))
    }


    async fn execute_guest_command(
        &mut self,
        client: &mut RpcClient,
        command: &Command,
    ) -> Result<CommandStatus> {
        let output = guest_exec::run(client, &command.as_guest_command()?).await?;
        if !output.stdout.is_empty() {
            self.write_bytes(&output.stdout)?;
        }
        if !output.stderr.is_empty() {
            self.write_bytes(&output.stderr)?;
        }
        Ok(CommandStatus::new(output.exit_code))
    }

    async fn show_tracing(&mut self, client: &mut RpcClient) -> Result<()> {
        system::stream_tracing(client, &self.tracing).await
    }

    async fn call_rpc(&mut self, client: &mut RpcClient) -> Result<()> {
        self.rpc.call(client).await?;
        let response = self.rpc.response_hex.clone();
        self.print_block(&response)
    }
}

impl ShellHelper {
    fn new() -> Self {
        Self {
            highlighter: MatchingBracketHighlighter::new(),
            hinter: HistoryHinter::new(),
            validator: MatchingBracketValidator::new(),
            colored_prompt: format!(
                "{}{} ",
                AnsiStyle::new().fg(Color::Green).bold().paint("helios"),
                AnsiStyle::new().fg(Color::Fixed(244)).paint(">")
            ),
        }
    }
}

impl Helper for ShellHelper {}

impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &RustyContext<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        let (start, current, tokens) = completion_context(line, pos);
        let candidates = completion_candidates(&tokens, current)
            .iter()
            .copied()
            .filter(|candidate| candidate.starts_with(current))
            .map(|candidate| Pair {
                display: completion_display(&tokens, candidate),
                replacement: candidate.to_string(),
            })
            .collect();
        Ok((start, candidates))
    }
}

impl Hinter for ShellHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, ctx: &RustyContext<'_>) -> Option<Self::Hint> {
        if let Some(hint) = self.hinter.hint(line, pos, ctx) {
            return Some(hint);
        }

        if pos != line.len() {
            return None;
        }

        let (_, current, tokens) = completion_context(line, pos);
        if current.is_empty() {
            return None;
        }

        let matches = completion_candidates(&tokens, current)
            .iter()
            .copied()
            .filter(|candidate| candidate.starts_with(current))
            .collect::<Vec<_>>();

        if matches.len() == 1 {
            let candidate = matches[0];
            if candidate.len() > current.len() {
                return Some(candidate[current.len()..].to_owned());
            }
        }

        None
    }
}

impl Highlighter for ShellHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        if default {
            Borrowed(&self.colored_prompt)
        } else {
            Borrowed(prompt)
        }
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Owned(
            AnsiStyle::new()
                .fg(Color::Fixed(244))
                .italic()
                .paint(hint)
                .to_string(),
        )
    }

    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        let _ = pos;
        highlight_shell_line(line)
            .unwrap_or_else(|| self.highlighter.highlight(line, pos).into_owned())
            .into()
    }

    fn highlight_char(&self, line: &str, pos: usize, forced: bool) -> bool {
        self.highlighter.highlight_char(line, pos, forced)
    }
}

impl Validator for ShellHelper {
    fn validate(&self, ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        self.validator.validate(ctx)
    }
}

fn build_editor() -> Result<Editor<ShellHelper, DefaultHistory>> {
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .edit_mode(EditMode::Emacs)
        .history_ignore_space(true)
        .build();
    let mut editor = Editor::with_config(config).context("failed to create line editor")?;
    editor.set_helper(Some(ShellHelper::new()));
    editor.bind_sequence(
        RustyEvent::from(RustyKeyEvent(RustyKeyCode::Char('F'), RustyModifiers::CTRL)),
        Cmd::CompleteHint,
    );
    Ok(editor)
}

fn write_stdout(bytes: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()?;
    Ok(())
}

fn parse_input(input: &str) -> ParsedLine {
    match shell_core::parse(input) {
        Ok(shell_core::ParseState::Complete(program)) => {
            let mut commands = Vec::with_capacity(program.len());
            for statement in program {
                match validate_statement(statement) {
                    Ok(statement) => commands.push(statement),
                    Err(error) => return ParsedLine::Output(format_parse_error(&error.to_string())),
                }
            }
            ParsedLine::Program(commands)
        }
        Ok(shell_core::ParseState::Incomplete) => {
            ParsedLine::Output("error: shell block is incomplete".to_owned())
        }
        Err(error) => ParsedLine::Output(format_parse_error(&error.to_string())),
    }
}

fn format_parse_error(error: &str) -> String {
    if error.starts_with("error:") {
        error.to_owned()
    } else {
        format!("error: {error}")
    }
}

fn validate_statement(statement: Statement) -> Result<Statement> {
    match statement {
        Statement::Command(line) => {
            parse_command_line(&line)?;
            Ok(Statement::Command(line))
        }
        Statement::If {
            condition,
            then_branch,
            else_branch,
        } => {
            parse_command_line(&condition)?;
            let then_branch = then_branch
                .into_iter()
                .map(validate_statement)
                .collect::<Result<Vec<_>>>()?;
            let else_branch = else_branch
                .into_iter()
                .map(validate_statement)
                .collect::<Result<Vec<_>>>()?;
            Ok(Statement::If {
                condition,
                then_branch,
                else_branch,
            })
        }
    }
}

fn parse_command_line(input: &str) -> Result<ParsedCommandLine> {
    let tokens = match shell_words::split(input) {
        Ok(tokens) => tokens,
        Err(error) => {
            anyhow::bail!("failed to parse shell input: {error}");
        }
    };

    match ReplCli::try_parse_from(tokens.iter().map(String::as_str)) {
        Ok(cli) => Ok(ParsedCommandLine::Command(cli.command.into_runtime_command()?)),
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            Ok(ParsedCommandLine::Output(error.render().ansi().to_string()))
        }
        Err(error) => anyhow::bail!(render_clap_error(error, &tokens)),
    }
}

fn render_clap_error(error: clap::Error, tokens: &[String]) -> String {
    if matches!(
        error.kind(),
        ErrorKind::InvalidSubcommand | ErrorKind::UnknownArgument
    ) {
        if let Some(output) = render_unknown_command(tokens) {
            return output;
        }
    }

    error.use_stderr();
    error.render().ansi().to_string()
}

fn render_unknown_command(tokens: &[String]) -> Option<String> {
    let current = tokens.last()?;
    if current.starts_with('-') {
        return None;
    }

    if let Some(output) = render_reserved_block_keyword_error(tokens) {
        return Some(output);
    }

    let prefix = tokens[..tokens.len().saturating_sub(1)]
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let candidates = completion_candidates(&prefix, current);
    let suggestion = best_suggestion(current, candidates);
    let help_hint = render_help_hint(&prefix);

    let mut output = String::new();
    write!(
        &mut output,
        "{}: unknown command `{}`",
        AnsiStyle::new().fg(Color::Red).bold().paint("error"),
        AnsiStyle::new().fg(Color::Yellow).paint(current),
    )
    .expect("writing into String must succeed");

    if !prefix.is_empty() {
        write!(
            &mut output,
            "\n  {}: {}",
            AnsiStyle::new().fg(Color::Fixed(244)).paint("context"),
            prefix.join(" "),
        )
        .expect("writing into String must succeed");
    }

    if let Some(suggestion) = suggestion {
        write!(
            &mut output,
            "\n\n  {}: did you mean `{}`?",
            AnsiStyle::new().fg(Color::Green).bold().paint("tip"),
            AnsiStyle::new().fg(Color::Green).paint(suggestion),
        )
        .expect("writing into String must succeed");
    }

    write!(
        &mut output,
        "\n\n{} {}",
        AnsiStyle::new().fg(Color::Fixed(244)).paint("try:"),
        help_hint,
    )
    .expect("writing into String must succeed");

    Some(output)
}

fn best_suggestion<'a>(current: &str, candidates: &'a [&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .filter(|candidate| !candidate.starts_with("--"))
        .filter(|candidate| *candidate != current)
        .filter_map(|candidate| {
            let score = normalized_damerau_levenshtein(current, candidate);
            (score >= 0.5).then_some((candidate, score))
        })
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(candidate, _)| candidate)
}

fn render_reserved_block_keyword_error(tokens: &[String]) -> Option<String> {
    let keyword = tokens.last()?.as_str();
    let message = match keyword {
        "if" => "`if` starts a shell block and requires a condition command, for example `if test -e /tmp/file`",
        "else" => "`else` is only valid inside an `if ... end` block",
        "end" => "`end` is only valid when closing an open shell block",
        _ => return None,
    };

    let mut output = String::new();
    write!(
        &mut output,
        "{}: {}",
        AnsiStyle::new().fg(Color::Red).bold().paint("error"),
        message,
    )
    .expect("writing into String must succeed");
    write!(
        &mut output,
        "\n\n{} `if test -e /tmp/file`",
        AnsiStyle::new().fg(Color::Fixed(244)).paint("try:"),
    )
    .expect("writing into String must succeed");
    Some(output)
}

fn render_help_hint(prefix: &[&str]) -> String {
    match prefix.first().copied() {
        None => format!(
            "`{}` or `{}`",
            AnsiStyle::new().fg(Color::Green).paint("help"),
            AnsiStyle::new().fg(Color::Green).paint("--help"),
        ),
        Some(command) => format!(
            "`help {}` or `{} --help`",
            AnsiStyle::new().fg(Color::Green).paint(command),
            AnsiStyle::new().fg(Color::Green).paint(command),
        ),
    }
}


fn render_help(topic: HelpTopic) -> String {
    help::render(topic)
}

impl CliCommand {
    fn into_runtime_command(self) -> Result<Command> {
        Ok(match self {
            Self::Help { topic } => Command::Help(topic.unwrap_or(HelpTopic::Overview)),
            Self::Clear => Command::Clear,
            Self::Exit => Command::Exit,
            Self::Pwd => Command::Pwd,
            Self::Ls { path } => Command::List(path),
            Self::Cat { path } => Command::Cat(path),
            Self::Edit { path } => Command::Edit(path),
            Self::Test { expression } => Command::Test(expression),
            Self::Source { path } => Command::Source(path),
            Self::Mkdir { path } => Command::Mkdir(path),
            Self::Rm { path } => Command::Remove(path),
            Self::Cp { source, destination } => Command::Copy { source, destination },
            Self::Mv { source, destination } => Command::Move { source, destination },
            Self::Touch { path } => Command::Touch(path),
            Self::Echo { words } => Command::Echo(filesystem::parse_echo(&words)?),
            Self::Exec { path, args } => Command::Exec { path, args },
            Self::Stats => Command::ShowStats,
            Self::Tracing { action } => match action {
                None => Command::ShowTracing,
                Some(TracingAction::Limit { count }) => Command::TracingLimit(count),
                Some(TracingAction::Level { level }) => Command::TracingLevel(level.into_runtime()),
                Some(TracingAction::Targets { prefixes }) => {
                    Command::TracingTargets(parse_targets(&prefixes))
                }
            },
            Self::Rpc { action } => match action {
                RpcAction::Instance { name } => Command::RpcInstance(name),
                RpcAction::Func { name } => Command::RpcFunc(name),
                RpcAction::Payload { hex } => Command::RpcPayload(hex),
                RpcAction::Call => Command::RpcCall,
            },
        })
    }
}

impl TracingLevelArg {
    fn into_runtime(self) -> Option<tracing::Level> {
        match self {
            Self::None => None,
            Self::Error => Some(tracing::Level::Error),
            Self::Warn => Some(tracing::Level::Warn),
            Self::Info => Some(tracing::Level::Info),
            Self::Debug => Some(tracing::Level::Debug),
            Self::Trace => Some(tracing::Level::Trace),
        }
    }
}

fn parse_targets(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn tracing_level_name(level: Option<tracing::Level>) -> &'static str {
    match level {
        None => "none",
        Some(tracing::Level::Error) => "error",
        Some(tracing::Level::Warn) => "warn",
        Some(tracing::Level::Info) => "info",
        Some(tracing::Level::Debug) => "debug",
        Some(tracing::Level::Trace) => "trace",
    }
}

fn completion_context(line: &str, pos: usize) -> (usize, &str, Vec<&str>) {
    let prefix = &line[..pos];
    let ends_with_space = prefix.chars().next_back().is_some_and(char::is_whitespace);
    let mut tokens = prefix.split_whitespace().collect::<Vec<_>>();
    let current = if ends_with_space {
        ""
    } else {
        tokens.pop().unwrap_or("")
    };
    let start = pos.saturating_sub(current.len());
    (start, current, tokens)
}

fn completion_candidates<'a>(tokens: &[&str], current: &str) -> &'a [&'a str] {
    if current.starts_with("--") || tokens.last().is_some_and(|token| token.starts_with("--")) {
        return &["--help"];
    }

    match tokens {
        [] => ROOT_CANDIDATES,
        ["if"] => ROOT_CANDIDATES,
        ["help"] => HELP_CANDIDATES,
        ["stats"] => STATS_CANDIDATES,
        ["tracing"] => TRACING_CANDIDATES,
        ["tracing", "level"] => TRACING_LEVEL_CANDIDATES,
        ["rpc"] => RPC_CANDIDATES,
        ["rpc", "instance"] => RPC_INSTANCE_CANDIDATES,
        ["rpc", "func"] => RPC_FUNC_CANDIDATES,
        _ => &[],
    }
}

fn completion_display(tokens: &[&str], candidate: &str) -> String {
    let description = match (tokens.first().copied(), candidate) {
        (_, "help") => "show shell help",
        (_, "clear") => "clear the screen",
        (_, "exit") => "leave the shell",
        (_, "pwd") => "print the remote working directory",
        (_, "ls") => "list remote debugger files",
        (_, "cat") => "print a remote file",
        (_, "edit") => "open the full-screen editor",
        (_, "mkdir") => "create a remote directory",
        (_, "rm") => "remove a remote path",
        (_, "cp") => "copy a remote file",
        (_, "mv") => "move or rename a remote path",
        (_, "touch") => "create an empty remote file",
        (_, "echo") => "print or redirect text",
        (_, "test") => "evaluate a filesystem predicate",
        (_, "source") => "run a host-local shell script",
        (Some("help"), "stats") => "help for the stats view",
        (_, "exec") => "exec a guest program from the remote filesystem",
        (Some("help"), "tracing") => "help for tracing controls",
        (Some("help"), "rpc") => "help for raw RPC calls",
        (_, "stats") => "open the live stats view",
        (_, "tracing") => "stream kernel tracing events",
        (_, "rpc") => "invoke a raw remote WIT method",
        (_, "if") => "start a shell conditional block",
        (_, "else") => "alternate branch inside `if`",
        (_, "end") => "close the current shell block",
        (_, "--help") => "show command help",
        (Some("help"), "overview") => "shell command overview",
        (Some("tracing"), "limit") => "set the fetch window size",
        (Some("tracing"), "level") => "set the minimum tracing level",
        (Some("tracing"), "targets") => "set tracing target prefixes",
        (Some("rpc"), "instance") => "set the remote WIT instance id",
        (Some("rpc"), "func") => "set the remote function name",
        (Some("rpc"), "payload") => "set the hex request payload",
        (Some("rpc"), "call") => "invoke the configured remote method",
        _ => "",
    };

    if description.is_empty() {
        candidate.to_owned()
    } else {
        format!("{candidate:<16} {description}")
    }
}

fn highlight_shell_line(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "else" || trimmed == "end" || trimmed.starts_with("if ") {
        return Some(highlight_block_header(line));
    }

    let tokens = shell_words::split(line).ok()?;
    let command = tokens.first()?;
    let mut output = String::new();
    for (index, token) in tokens.iter().enumerate() {
        if index != 0 {
            output.push(' ');
        }
        if index == 0 {
            output.push_str(&highlight_command_token(command));
            continue;
        }
        output.push_str(&highlight_argument_token(command, token));
    }
    Some(output)
}

fn highlight_block_header(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len().saturating_sub(trimmed.len())];
    let style = AnsiStyle::new().fg(Color::Purple).bold();
    if let Some(condition) = trimmed.strip_prefix("if ") {
        format!(
            "{indent}{} {}",
            style.paint("if"),
            AnsiStyle::new().fg(Color::White).paint(condition)
        )
    } else {
        format!("{indent}{}", style.paint(trimmed))
    }
}

fn highlight_command_token(command: &str) -> String {
    if ROOT_CANDIDATES.contains(&command) {
        return AnsiStyle::new()
            .fg(Color::Blue)
            .bold()
            .paint(command)
            .to_string();
    }
    AnsiStyle::new()
        .fg(Color::Red)
        .bold()
        .paint(command)
        .to_string()
}

fn highlight_argument_token(command: &str, token: &str) -> String {
    if matches!(command, "exec" | "source") {
        let style = if Path::new(token).exists() {
            AnsiStyle::new().fg(Color::Cyan).underline()
        } else {
            AnsiStyle::new().fg(Color::Red).underline()
        };
        return style.paint(token).to_string();
    }

    if matches!(
        command,
        "ls" | "cat" | "edit" | "mkdir" | "rm" | "cp" | "mv" | "touch" | "test"
    ) {
        let style = match filesystem::normalize_path(token) {
            Ok(_) => AnsiStyle::new().fg(Color::Cyan).underline(),
            Err(_) => AnsiStyle::new().fg(Color::Red).underline(),
        };
        return style.paint(token).to_string();
    }

    AnsiStyle::new().fg(Color::White).paint(token).to_string()
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use crate::filesystem::EchoTarget;
    use shell_core::Statement;

    use super::{parse_input, Command, HelpTopic, ParsedLine};

    #[derive(Debug, PartialEq, Eq)]
    enum TerminalEvent {
        Print(String),
        Bytes(Vec<u8>),
        Clear,
    }

    #[derive(Default)]
    struct MockTerminal {
        events: Vec<TerminalEvent>,
    }

    impl super::TerminalIo for MockTerminal {
        fn print(&mut self, text: &str) -> Result<()> {
            self.events.push(TerminalEvent::Print(text.to_owned()));
            Ok(())
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            self.events.push(TerminalEvent::Bytes(bytes.to_vec()));
            Ok(())
        }

        fn clear_screen(&mut self) -> Result<()> {
            self.events.push(TerminalEvent::Clear);
            Ok(())
        }
    }

    fn single_command(input: &str) -> Command {
        match parse_input(input) {
            ParsedLine::Program(program) => match program.as_slice() {
                [Statement::Command(line)] => match super::parse_command_line(line)
                    .expect("single command program must parse successfully")
                {
                    super::ParsedCommandLine::Command(command) => command,
                    super::ParsedCommandLine::Output(output) => {
                        panic!("expected command parse, got output: {output}")
                    }
                },
                _ => panic!("expected a single command statement"),
            },
            ParsedLine::Output(output) => panic!("unexpected parse output: {output}"),
        }
    }

    #[test]
    fn parses_stats_command() {
        match single_command("stats") {
            Command::ShowStats => {}
            _ => panic!("stats command must parse"),
        }
    }

    #[test]
    fn parses_edit_command() {
        match single_command("edit /tmp/debug.txt") {
            Command::Edit(path) => assert_eq!(path, "/tmp/debug.txt"),
            _ => panic!("edit command must parse"),
        }
    }

    #[test]
    fn parses_clear_command() {
        match single_command("clear") {
            Command::Clear => {}
            _ => panic!("clear command must parse"),
        }
    }

    #[test]
    fn parses_pwd_command() {
        match single_command("pwd") {
            Command::Pwd => {}
            _ => panic!("pwd command must parse"),
        }
    }

    #[test]
    fn parses_ls_command() {
        match single_command("ls /tmp") {
            Command::List(Some(path)) => assert_eq!(path, "/tmp"),
            _ => panic!("ls command must parse"),
        }
    }

    #[test]
    fn parses_rm_command() {
        match single_command("rm /tmp/file") {
            Command::Remove(path) => assert_eq!(path, "/tmp/file"),
            _ => panic!("rm command must parse"),
        }
    }

    #[test]
    fn parses_cp_command() {
        match single_command("cp /tmp/from /tmp/to") {
            Command::Copy { source, destination } => {
                assert_eq!(source, "/tmp/from");
                assert_eq!(destination, "/tmp/to");
            }
            _ => panic!("cp command must parse"),
        }
    }

    #[test]
    fn parses_mv_command() {
        match single_command("mv /tmp/from /tmp/to") {
            Command::Move { source, destination } => {
                assert_eq!(source, "/tmp/from");
                assert_eq!(destination, "/tmp/to");
            }
            _ => panic!("mv command must parse"),
        }
    }

    #[test]
    fn parses_touch_command() {
        match single_command("touch /tmp/file") {
            Command::Touch(path) => assert_eq!(path, "/tmp/file"),
            _ => panic!("touch command must parse"),
        }
    }

    #[test]
    fn parses_cat_command() {
        match single_command("cat /tmp/file") {
            Command::Cat(path) => assert_eq!(path, "/tmp/file"),
            _ => panic!("cat command must parse"),
        }
    }

    #[test]
    fn parses_mkdir_command() {
        match single_command("mkdir /tmp/dir") {
            Command::Mkdir(path) => assert_eq!(path, "/tmp/dir"),
            _ => panic!("mkdir command must parse"),
        }
    }

    #[test]
    fn parses_echo_redirection() {
        match single_command("echo hello > /tmp/file") {
            Command::Echo(EchoTarget::File {
                path,
                bytes,
                append,
            }) => {
                assert_eq!(path, "/tmp/file");
                assert_eq!(bytes, b"hello\n");
                assert!(!append);
            }
            _ => panic!("echo redirection must parse"),
        }
    }

    #[test]
    fn parses_exec_command() {
        match single_command("exec ./target/ping.wasm google.com 1500") {
            Command::Exec { path, args } => {
                assert_eq!(path, "./target/ping.wasm");
                assert_eq!(args, vec!["google.com", "1500"]);
            }
            _ => panic!("exec command must parse"),
        }
    }

    #[test]
    fn rejects_removed_stats_period_command() {
        match parse_input("stats period 250") {
            ParsedLine::Output(text) => assert!(text.contains("unknown command")),
            _ => panic!("removed stats period command must stay rejected"),
        }
    }

    #[test]
    fn parses_rpc_payload_without_argument() {
        match single_command("rpc payload") {
            Command::RpcPayload(value) => assert!(value.is_empty()),
            _ => panic!("rpc payload without argument must parse"),
        }
    }

    #[test]
    fn keeps_help_command() {
        match single_command("help tracing") {
            Command::Help(HelpTopic::Tracing) => {}
            _ => panic!("help tracing must parse"),
        }
    }

    #[test]
    fn clap_suggests_similar_command() {
        match parse_input("staats") {
            ParsedLine::Output(text) => {
                assert!(text.contains("unknown command"));
                assert!(text.contains("stats"));
                assert!(!text.contains("subcommand"));
            }
            _ => panic!("invalid command must produce clap output"),
        }
    }

    #[test]
    fn clap_supports_help_flag() {
        match super::parse_command_line("stats --help")
            .expect("stats --help must parse successfully")
        {
            super::ParsedCommandLine::Output(text) => {
                assert!(text.contains("Open the live stats view"));
                assert!(!text.starts_with("error:"));
            }
            super::ParsedCommandLine::Command(_) => {
                panic!("stats --help must render help output")
            }
        }
    }

    #[test]
    fn reserved_if_keyword_reports_block_specific_error() {
        match parse_input("if") {
            ParsedLine::Output(text) => {
                assert!(text.contains("`if` starts a shell block"));
                assert!(text.contains("if test -e /tmp/file"));
                assert!(!text.contains("did you mean `if`"));
                assert!(!text.contains("error: error:"));
            }
            _ => panic!("bare if must produce a parse error"),
        }
    }

    #[test]
    fn parses_if_program() {
        match parse_input("if test -e /tmp/file\n echo yes\nelse\n echo no\nend") {
            ParsedLine::Program(program) => {
                assert!(matches!(program[0], Statement::If { .. }));
            }
            ParsedLine::Output(text) => panic!("unexpected parse output: {text}"),
        }
    }

    #[test]
    fn clear_command_uses_terminal_clear_path() {
        let mut shell = super::Shell::new(MockTerminal::default());
        let status = shell
            .execute_immediate_command(&Command::Clear)
            .expect("clear must execute successfully")
            .expect("clear must complete immediately");

        assert!(!status.should_exit());
        assert!(status.is_success());
        assert_eq!(shell.terminal.events, vec![TerminalEvent::Clear]);
    }

    #[test]
    fn print_block_adds_only_one_trailing_newline() {
        let mut shell = super::Shell::new(MockTerminal::default());
        shell
            .print_block("kernel ready")
            .expect("printing a block must succeed");
        shell
            .print_block("already terminated\n")
            .expect("printing a terminated block must succeed");

        assert_eq!(
            shell.terminal.events,
            vec![
                TerminalEvent::Print("kernel ready\n".to_owned()),
                TerminalEvent::Print("already terminated\n".to_owned()),
            ]
        );
    }
}
