use std::borrow::Cow::{self, Borrowed, Owned};
use std::cell::RefCell;
use std::fs;
use std::io::Write as _;
use std::rc::Rc;

use anyhow::{Context as _, Result, bail};
use clap::{ColorChoice, Parser};
use helios_inspector_protocol::system::programs::{ExecOutput, ExecResult};
use nu_ansi_term::{Color, Style as AnsiStyle};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{Highlighter, MatchingBracketHighlighter};
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::history::DefaultHistory;
use rustyline::validate::{MatchingBracketValidator, ValidationContext, ValidationResult, Validator};
use rustyline::Cmd;
use rustyline::{CompletionType, Config, Context as RustyContext, EditMode, Editor, Event as RustyEvent, Helper, KeyCode as RustyKeyCode, KeyEvent as RustyKeyEvent, Modifiers as RustyModifiers};

use crate::{DashCommand, TracingCommand};
use crate::programs;
use crate::runtime;
use crate::serial::RpcClient;
use crate::stats_tui;
use crate::system;

const PROMPT: &str = "helios> ";
const CONTINUATION_PROMPT: &str = "....> ";
const REMOTE_DASH_PATH: &str = "/bin/dash";
const ROOT_CANDIDATES: &[&str] = &["stats", "tracing", "clear", "exit", "quit"];
const TRACING_CANDIDATES: &[&str] = &["--min-level", "--target-prefix", "--limit"];
const LEVEL_CANDIDATES: &[&str] = &["none", "error", "warn", "info", "debug", "trace"];

type SharedEditor = Rc<RefCell<Editor<ShellHelper, DefaultHistory>>>;

#[derive(Debug, Parser)]
#[command(name = "repl", no_binary_name = true, disable_version_flag = true, color = ColorChoice::Always)]
struct ReplBuiltinCli {
    #[command(subcommand)]
    command: ReplBuiltin,
}

#[derive(Debug, clap::Subcommand)]
enum ReplBuiltin {
    /// Open the live system monitor.
    Stats,
    /// Stream tracing events until interrupted.
    Tracing(TracingCommand),
    /// Clear the screen and move the cursor to the top-left corner.
    Clear,
    /// Leave the inspector.
    Exit,
    /// Leave the inspector.
    Quit,
}

pub fn run(mut client: RpcClient) -> Result<()> {
    let editor = Rc::new(RefCell::new(build_editor()?));
    let mut terminal = InteractiveTerminal::new(Rc::clone(&editor));

    terminal.print(
        "Helios inspector ready. Builtins: stats, tracing, clear, exit. Everything else runs in remote dash.\n",
    )?;

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
            .context("failed to record inspector history entry")?;

        match BuiltinCommand::parse(input) {
            BuiltinParse::Builtin(BuiltinCommand::Stats) => {
                runtime::block_on(stats_tui::run(&mut client))
                    .context("failed to open stats view")?;
            }
            BuiltinParse::Builtin(BuiltinCommand::Tracing(command)) => {
                match runtime::block_on(runtime::interruptible(system::stream_tracing_command(
                    &mut client,
                    &command,
                )))
                .context("failed to listen for Ctrl+C during tracing stream")?
                {
                    runtime::CommandRun::Completed(result) => result?,
                    runtime::CommandRun::Interrupted => terminal.print_line("interrupted")?,
                }
            }
            BuiltinParse::Builtin(BuiltinCommand::Clear) => terminal.clear_screen()?,
            BuiltinParse::Builtin(BuiltinCommand::Exit) => return Ok(()),
            BuiltinParse::Remote(script) => {
                match runtime::block_on(runtime::interruptible(run_dash_script(
                    &mut client,
                    script,
                )))
                .context("failed to listen for Ctrl+C during remote dash execution")?
                {
                    runtime::CommandRun::Completed(result) => {
                        let output = result?;
                        terminal.write_output(&output.output)?;
                    }
                    runtime::CommandRun::Interrupted => terminal.print_line("interrupted")?,
                }
            }
        }
    }
}

pub async fn run_dash_command(client: &mut RpcClient, command: &DashCommand) -> Result<ExecResult> {
    let script = match (&command.command, &command.script) {
        (Some(command), None) => command.clone(),
        (None, Some(path)) => fs::read_to_string(path)
            .with_context(|| format!("failed to read local dash script {path}"))?,
        (None, None) => bail!("dash requires either -c <script> or <script-file>"),
        (Some(_), Some(_)) => unreachable!("clap must reject conflicting dash inputs"),
    };
    run_dash_script(client, script).await
}

async fn run_dash_script(client: &mut RpcClient, script: String) -> Result<ExecResult> {
    programs::exec(client, REMOTE_DASH_PATH, &["-c".to_owned(), script]).await
}

fn read_program(editor: &SharedEditor) -> Result<Option<String>> {
    let mut input = String::new();
    let mut prompt = PROMPT;
    loop {
        let line = match editor.borrow_mut().readline(prompt) {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => return Ok(Some(String::new())),
            Err(ReadlineError::Eof) => return Ok(None),
            Err(error) => return Err(error).context("failed to read inspector input"),
        };
        if !input.is_empty() {
            input.push('\n');
        }
        input.push_str(&line);
        if !needs_more_input(&input) {
            return Ok(Some(input));
        }
        prompt = CONTINUATION_PROMPT;
    }
}

struct InteractiveTerminal {
    editor: SharedEditor,
}

impl InteractiveTerminal {
    fn new(editor: SharedEditor) -> Self {
        Self { editor }
    }

    fn print(&mut self, text: &str) -> Result<()> {
        write_stdout(text.as_bytes())
    }

    fn print_line(&mut self, line: &str) -> Result<()> {
        let mut output = String::with_capacity(line.len() + 1);
        output.push_str(line);
        output.push('\n');
        self.print(&output)
    }

    fn write_output(&mut self, output: &ExecOutput) -> Result<()> {
        if !output.stdout.is_empty() {
            write_stdout(&output.stdout)?;
        }
        if !output.stderr.is_empty() {
            write_stderr(&output.stderr)?;
        }
        Ok(())
    }

    fn clear_screen(&mut self) -> Result<()> {
        self.editor
            .borrow_mut()
            .clear_screen()
            .context("failed to clear inspector screen")
    }
}

enum BuiltinParse {
    Builtin(BuiltinCommand),
    Remote(String),
}

enum BuiltinCommand {
    Stats,
    Tracing(TracingCommand),
    Clear,
    Exit,
}

impl BuiltinCommand {
    fn parse(input: &str) -> BuiltinParse {
        let Ok(tokens) = shell_words::split(input) else {
            return BuiltinParse::Remote(input.to_owned());
        };
        if tokens.is_empty() {
            return BuiltinParse::Remote(input.to_owned());
        }

        match ReplBuiltinCli::try_parse_from(tokens.iter().map(String::as_str)) {
            Ok(cli) => BuiltinParse::Builtin(match cli.command {
                ReplBuiltin::Stats => BuiltinCommand::Stats,
                ReplBuiltin::Tracing(command) => BuiltinCommand::Tracing(command),
                ReplBuiltin::Clear => BuiltinCommand::Clear,
                ReplBuiltin::Exit | ReplBuiltin::Quit => BuiltinCommand::Exit,
            }),
            Err(_) => BuiltinParse::Remote(input.to_owned()),
        }
    }
}

struct ShellHelper {
    highlighter: MatchingBracketHighlighter,
    hinter: HistoryHinter,
    validator: MatchingBracketValidator,
    colored_prompt: String,
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
                display: candidate.to_owned(),
                replacement: candidate.to_owned(),
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
    editor.bind_sequence(
        RustyEvent::from(RustyKeyEvent(
            RustyKeyCode::Char('F'),
            RustyModifiers::CTRL,
        )),
        Cmd::CompleteHint,
    );
    Ok(editor)
}

fn completion_context(line: &str, pos: usize) -> (usize, &str, Vec<&str>) {
    let prefix = &line[..pos];
    let start = prefix
        .char_indices()
        .rev()
        .find_map(|(index, ch)| ch.is_whitespace().then_some(index + ch.len_utf8()))
        .unwrap_or(0);
    let current = &prefix[start..];
    let tokens = prefix[..start].split_whitespace().collect::<Vec<_>>();
    (start, current, tokens)
}

fn completion_candidates<'a>(tokens: &[&'a str], _current: &'a str) -> &'static [&'static str] {
    match tokens.first().copied() {
        Some("tracing") => {
            if tokens.last().is_some_and(|token| *token == "--min-level") {
                LEVEL_CANDIDATES
            } else {
                TRACING_CANDIDATES
            }
        }
        _ => ROOT_CANDIDATES,
    }
}

fn write_stdout(bytes: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()?;
    Ok(())
}

fn write_stderr(bytes: &[u8]) -> Result<()> {
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(bytes)?;
    stderr.flush()?;
    Ok(())
}

fn needs_more_input(input: &str) -> bool {
    let mut depth = 0usize;
    for line in input.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "end" {
            depth = depth.saturating_sub(1);
            continue;
        }
        if line == "else" || line.starts_with("else if ") {
            continue;
        }
        if line.starts_with("if ") {
            depth = depth.saturating_add(1);
        }
    }
    depth != 0
}


#[cfg(test)]
mod tests {
    use super::{BuiltinCommand, BuiltinParse};

    #[test]
    fn repl_intercepts_stats_builtin() {
        match BuiltinCommand::parse("stats") {
            BuiltinParse::Builtin(BuiltinCommand::Stats) => {}
            _ => panic!("stats must stay a local builtin"),
        }
    }

    #[test]
    fn repl_intercepts_tracing_builtin() {
        match BuiltinCommand::parse("tracing --min-level debug") {
            BuiltinParse::Builtin(BuiltinCommand::Tracing(command)) => {
                assert_eq!(command.min_level.as_deref(), Some("debug"));
            }
            _ => panic!("tracing must stay a local builtin"),
        }
    }

    #[test]
    fn repl_forwards_regular_shell_commands_to_remote_dash() {
        match BuiltinCommand::parse("ls /tmp") {
            BuiltinParse::Remote(script) => assert_eq!(script, "ls /tmp"),
            _ => panic!("ls must be forwarded to remote dash"),
        }
    }

    #[test]
    fn repl_forwards_stats_with_extra_arguments_to_remote_dash() {
        match BuiltinCommand::parse("stats --help") {
            BuiltinParse::Remote(script) => assert_eq!(script, "stats --help"),
            _ => panic!("stats with arguments must not be treated as a local builtin"),
        }
    }
}
