use std::borrow::Cow::{self, Borrowed, Owned};
use std::fmt::Write as _;
use std::io::Write as _;

use anyhow::{Context as _, Result};
use clap::error::ErrorKind;
use clap::{ColorChoice, CommandFactory, Parser, Subcommand, ValueEnum};
use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};
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
use rustyline::{CompletionType, Config, Context as RustyContext, EditMode, Editor, Helper};
use strsim::normalized_damerau_levenshtein;

use crate::filesystem;
use crate::rpc::RpcPane;
use crate::runtime;
use crate::serial::RpcClient;
use crate::stats_tui;
use crate::system::{self, TracingConfig};

pub fn run(mut client: RpcClient) -> Result<()> {
    let mut editor = build_editor()?;
    let mut shell = Shell::new();

    shell.print_banner()?;

    loop {
        let line = match editor.readline(PROMPT) {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => return Ok(()),
            Err(error) => return Err(error).context("failed to read shell input"),
        };

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        editor
            .add_history_entry(line)
            .context("failed to record shell history entry")?;

        match parse_line(line) {
            ParsedLine::Command(command) => {
                if runtime::block_on(shell.execute(&mut client, command))? {
                    return Ok(());
                }
            }
            ParsedLine::Output(output) => shell.print_block(&output)?,
        }
    }
}

const PROMPT: &str = "helios> ";
const LIVE_STATS_PERIOD_MS: u64 = 1_000;
const ROOT_CANDIDATES: &[&str] = &[
    "help", "clear", "exit", "ls", "rm", "touch", "stats", "tracing", "rpc", "--help",
];
const HELP_CANDIDATES: &[&str] = &["overview", "stats", "tracing", "rpc", "--help"];
const STATS_CANDIDATES: &[&str] = &["--help"];
const TRACING_CANDIDATES: &[&str] = &["limit", "level", "targets", "--help"];
const TRACING_LEVEL_CANDIDATES: &[&str] = &["none", "error", "warn", "info", "debug", "trace"];
const RPC_CANDIDATES: &[&str] = &["instance", "func", "payload", "call", "--help"];
const RPC_INSTANCE_CANDIDATES: &[&str] = &[
    "helios:system/stats@0.1.0",
    "helios:system/tracing@0.1.0",
    "helios:system/serial@0.1.0",
    "helios:system/sync@0.1.0",
];
const RPC_FUNC_CANDIDATES: &[&str] = &[
    "snapshot",
    "recent",
    "debug-port",
    "raw-mutex",
    "raw-rw-lock",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum HelpTopic {
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
struct ReplCli {
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
    /// List files in the remote debugger filesystem.
    Ls { path: Option<String> },
    /// Remove a file or an empty directory from the remote debugger filesystem.
    Rm { path: String },
    /// Create a file if it does not exist in the remote debugger filesystem.
    Touch { path: String },
    /// Open the live stats view.
    Stats,
    /// Fetch tracing events or configure tracing filters.
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

struct Shell {
    tracing: TracingConfig,
    rpc: RpcPane,
}

enum Command {
    Help(HelpTopic),
    Clear,
    Exit,
    List(Option<String>),
    Remove(String),
    Touch(String),
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
    Command(Command),
    Output(String),
}

struct ShellHelper {
    highlighter: MatchingBracketHighlighter,
    hinter: HistoryHinter,
    validator: MatchingBracketValidator,
    colored_prompt: String,
}

impl Shell {
    fn new() -> Self {
        Self {
            tracing: TracingConfig::new(),
            rpc: RpcPane::new(),
        }
    }

    fn print_banner(&self) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(b"Helios shell ready. Type `help` or use `--help` on any command.\n")?;
        stdout.flush()?;
        Ok(())
    }

    fn print_line(&self, line: &str) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(line.as_bytes())?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
        Ok(())
    }

    fn print_block(&self, text: &str) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(text.as_bytes())?;
        if !text.ends_with('\n') {
            stdout.write_all(b"\n")?;
        }
        stdout.flush()?;
        Ok(())
    }

    fn clear_screen(&self) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))
            .context("failed to clear shell screen")?;
        stdout.flush()?;
        Ok(())
    }

    async fn execute(&mut self, client: &mut RpcClient, command: Command) -> Result<bool> {
        match command {
            Command::Help(topic) => self.print_block(&render_help(topic))?,
            Command::Clear => self.clear_screen()?,
            Command::Exit => return Ok(true),
            Command::List(path) => {
                let output = filesystem::list(client, path.as_deref()).await?;
                self.print_block(&output)?;
            }
            Command::Remove(path) => filesystem::remove(client, &path).await?,
            Command::Touch(path) => filesystem::touch(client, &path).await?,
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
        }

        Ok(false)
    }

    async fn show_tracing(&mut self, client: &mut RpcClient) -> Result<()> {
        let events = system::fetch_tracing(client, &self.tracing).await?;
        self.print_block(&system::render_tracing_events(&events)?)
    }

    async fn call_rpc(&mut self, client: &mut RpcClient) -> Result<()> {
        self.rpc.call(client).await?;
        self.print_block(&self.rpc.response_hex)
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
            .into_iter()
            .filter(|candidate| candidate.starts_with(current))
            .map(|candidate| Pair {
                display: candidate.to_string(),
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
            .into_iter()
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
        self.highlighter.highlight(line, pos)
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
    Ok(editor)
}

fn parse_line(input: &str) -> ParsedLine {
    let tokens = match shell_words::split(input) {
        Ok(tokens) => tokens,
        Err(error) => {
            return ParsedLine::Output(format!("error: failed to parse shell input: {error}"));
        }
    };

    match ReplCli::try_parse_from(tokens.iter().map(String::as_str)) {
        Ok(cli) => ParsedLine::Command(cli.command.into_runtime_command()),
        Err(error) => ParsedLine::Output(render_clap_error(error, &tokens)),
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
        .filter_map(|candidate| {
            let score = normalized_damerau_levenshtein(current, candidate);
            (score >= 0.5).then_some((candidate, score))
        })
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(candidate, _)| candidate)
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
    let mut root = ReplCli::command().color(ColorChoice::Always);
    let mut output = match topic {
        HelpTopic::Overview => root.render_long_help().ansi().to_string(),
        HelpTopic::Stats => root
            .find_subcommand_mut("stats")
            .expect("stats help command must exist")
            .render_long_help()
            .ansi()
            .to_string(),
        HelpTopic::Tracing => root
            .find_subcommand_mut("tracing")
            .expect("tracing help command must exist")
            .render_long_help()
            .ansi()
            .to_string(),
        HelpTopic::Rpc => root
            .find_subcommand_mut("rpc")
            .expect("rpc help command must exist")
            .render_long_help()
            .ansi()
            .to_string(),
    };

    let appendix = match topic {
        HelpTopic::Overview => include_str!("help_overview.txt"),
        HelpTopic::Stats => include_str!("help_stats.txt"),
        HelpTopic::Tracing => include_str!("help_tracing.txt"),
        HelpTopic::Rpc => include_str!("help_rpc.txt"),
    };

    if !appendix.trim().is_empty() {
        output.push('\n');
        output.push_str(appendix);
        if !output.ends_with('\n') {
            output.push('\n');
        }
    }

    output
}

impl CliCommand {
    fn into_runtime_command(self) -> Command {
        match self {
            Self::Help { topic } => Command::Help(topic.unwrap_or(HelpTopic::Overview)),
            Self::Clear => Command::Clear,
            Self::Exit => Command::Exit,
            Self::Ls { path } => Command::List(path),
            Self::Rm { path } => Command::Remove(path),
            Self::Touch { path } => Command::Touch(path),
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
        }
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

fn completion_context<'a>(line: &'a str, pos: usize) -> (usize, &'a str, Vec<&'a str>) {
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

#[cfg(test)]
mod tests {
    use super::{parse_line, Command, HelpTopic, ParsedLine};

    #[test]
    fn parses_stats_command() {
        match parse_line("stats") {
            ParsedLine::Command(Command::ShowStats) => {}
            _ => panic!("stats command must parse"),
        }
    }

    #[test]
    fn parses_clear_command() {
        match parse_line("clear") {
            ParsedLine::Command(Command::Clear) => {}
            _ => panic!("clear command must parse"),
        }
    }

    #[test]
    fn parses_ls_command() {
        match parse_line("ls /tmp") {
            ParsedLine::Command(Command::List(Some(path))) => assert_eq!(path, "/tmp"),
            _ => panic!("ls command must parse"),
        }
    }

    #[test]
    fn parses_rm_command() {
        match parse_line("rm /tmp/file") {
            ParsedLine::Command(Command::Remove(path)) => assert_eq!(path, "/tmp/file"),
            _ => panic!("rm command must parse"),
        }
    }

    #[test]
    fn parses_touch_command() {
        match parse_line("touch /tmp/file") {
            ParsedLine::Command(Command::Touch(path)) => assert_eq!(path, "/tmp/file"),
            _ => panic!("touch command must parse"),
        }
    }

    #[test]
    fn rejects_removed_stats_period_command() {
        match parse_line("stats period 250") {
            ParsedLine::Output(text) => assert!(text.contains("unknown command")),
            _ => panic!("removed stats period command must stay rejected"),
        }
    }

    #[test]
    fn parses_rpc_payload_without_argument() {
        match parse_line("rpc payload") {
            ParsedLine::Command(Command::RpcPayload(value)) => assert!(value.is_empty()),
            _ => panic!("rpc payload without argument must parse"),
        }
    }

    #[test]
    fn keeps_help_command() {
        match parse_line("help tracing") {
            ParsedLine::Command(Command::Help(HelpTopic::Tracing)) => {}
            _ => panic!("help tracing must parse"),
        }
    }

    #[test]
    fn clap_suggests_similar_command() {
        match parse_line("staats") {
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
        match parse_line("stats --help") {
            ParsedLine::Output(text) => assert!(text.contains("live stats view")),
            _ => panic!("stats --help must print help text"),
        }
    }
}
