use std::io::Write as _;

use anyhow::{Context as _, Result};
use helios_shell_protocol::system::tracing;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use crate::rpc::RpcPane;
use crate::runtime;
use crate::serial::RpcClient;
use crate::system::{self, StatsPane, TracingPane};

pub fn run(mut client: RpcClient) -> Result<()> {
    let mut editor = DefaultEditor::new().context("failed to initialize shell line editor")?;
    let mut shell = Shell::new();

    shell.print_banner()?;

    loop {
        let line = match editor.readline("helios> ") {
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

        let command = match parse_command(line) {
            Ok(command) => command,
            Err(error) => {
                shell.print_error(&error.to_string())?;
                continue;
            }
        };

        if runtime::block_on(shell.execute(&mut client, command))? {
            return Ok(());
        }
    }
}

struct Shell {
    current: View,
    stats: StatsPane,
    tracing: TracingPane,
    rpc: RpcPane,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HelpTopic {
    Overview,
    Stats,
    Tracing,
    Rpc,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Stats,
    Tracing,
    Rpc,
}

enum Command {
    Help(HelpTopic),
    Quit,
    Refresh,
    ShowStats,
    ShowTracing,
    Use(View),
    StatsPeriod(u64),
    TracingLimit(u32),
    TracingLevel(Option<tracing::Level>),
    TracingTargets(Vec<String>),
    RpcInstance(String),
    RpcFunc(String),
    RpcPayload(String),
    RpcCall,
}

impl Shell {
    fn new() -> Self {
        Self {
            current: View::Stats,
            stats: StatsPane::new(),
            tracing: TracingPane::new(),
            rpc: RpcPane::new(),
        }
    }

    fn print_banner(&self) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(b"Helios shell ready. Type `help` for commands.\n")?;
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

    fn print_error(&self, error: &str) -> Result<()> {
        self.print_line(&format!("error: {error}"))
    }

    async fn execute(&mut self, client: &mut RpcClient, command: Command) -> Result<bool> {
        match command {
            Command::Help(topic) => self.print_block(help_text(topic))?,
            Command::Quit => return Ok(true),
            Command::Refresh => match self.current {
                View::Stats => self.show_stats(client).await?,
                View::Tracing => self.show_tracing(client).await?,
                View::Rpc => self.call_rpc(client).await?,
            },
            Command::ShowStats => {
                self.current = View::Stats;
                self.show_stats(client).await?;
            }
            Command::ShowTracing => {
                self.current = View::Tracing;
                self.show_tracing(client).await?;
            }
            Command::Use(view) => {
                self.current = view;
                self.print_line(match view {
                    View::Stats => "current view: stats",
                    View::Tracing => "current view: tracing",
                    View::Rpc => "current view: rpc",
                })?;
            }
            Command::StatsPeriod(period_ms) => {
                self.stats.period_ms = period_ms;
                self.print_line(&format!("stats period set to {period_ms}ms"))?;
            }
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
                self.current = View::Rpc;
                self.rpc.instance = instance;
                self.print_line("rpc instance updated")?;
            }
            Command::RpcFunc(func) => {
                self.current = View::Rpc;
                self.rpc.func = func;
                self.print_line("rpc function updated")?;
            }
            Command::RpcPayload(payload) => {
                self.current = View::Rpc;
                self.rpc.request_hex = payload;
                self.print_line("rpc payload updated")?;
            }
            Command::RpcCall => {
                self.current = View::Rpc;
                self.call_rpc(client).await?;
            }
        }

        Ok(false)
    }

    async fn show_stats(&mut self, client: &mut RpcClient) -> Result<()> {
        self.stats.refresh(client).await?;
        self.print_block(&self.stats.last_rendered)
    }

    async fn show_tracing(&mut self, client: &mut RpcClient) -> Result<()> {
        self.tracing.refresh(client).await?;
        self.print_block(&self.tracing.last_rendered)
    }

    async fn call_rpc(&mut self, client: &mut RpcClient) -> Result<()> {
        self.rpc.call(client).await?;
        self.print_block(&self.rpc.response_hex)
    }
}

fn parse_command(input: &str) -> Result<Command> {
    let mut parts = input.splitn(3, ' ');
    let head = parts.next().context("empty command")?;
    let middle = parts.next().unwrap_or_default();
    let tail = parts.next().unwrap_or_default().trim();

    match (head, middle) {
        ("help", "") => Ok(Command::Help(HelpTopic::Overview)),
        ("help", "overview") => Ok(Command::Help(HelpTopic::Overview)),
        ("help", "stats") => Ok(Command::Help(HelpTopic::Stats)),
        ("help", "tracing") => Ok(Command::Help(HelpTopic::Tracing)),
        ("help", "rpc") => Ok(Command::Help(HelpTopic::Rpc)),
        ("quit", _) | ("exit", _) => Ok(Command::Quit),
        ("refresh", _) => Ok(Command::Refresh),
        ("stats", "") => Ok(Command::ShowStats),
        ("tracing", "") => Ok(Command::ShowTracing),
        ("use", "stats") => Ok(Command::Use(View::Stats)),
        ("use", "tracing") => Ok(Command::Use(View::Tracing)),
        ("use", "rpc") => Ok(Command::Use(View::Rpc)),
        ("stats", "period") => Ok(Command::StatsPeriod(
            tail.parse()
                .context("stats period must be an integer in milliseconds")?,
        )),
        ("tracing", "limit") => Ok(Command::TracingLimit(
            tail.parse().context("tracing limit must be an integer")?,
        )),
        ("tracing", "level") => Ok(Command::TracingLevel(system::parse_level(tail)?)),
        ("tracing", "targets") => Ok(Command::TracingTargets(parse_targets(tail))),
        ("rpc", "instance") => Ok(Command::RpcInstance(tail.to_owned())),
        ("rpc", "func") => Ok(Command::RpcFunc(tail.to_owned())),
        ("rpc", "payload") => Ok(Command::RpcPayload(tail.to_owned())),
        ("rpc", "call") => Ok(Command::RpcCall),
        _ => anyhow::bail!("unknown command `{input}`"),
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

fn help_text(topic: HelpTopic) -> &'static str {
    match topic {
        HelpTopic::Overview => include_str!("help_overview.txt"),
        HelpTopic::Stats => include_str!("help_stats.txt"),
        HelpTopic::Tracing => include_str!("help_tracing.txt"),
        HelpTopic::Rpc => include_str!("help_rpc.txt"),
    }
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

#[cfg(test)]
mod tests {
    use super::{parse_command, Command, HelpTopic, View};
    use helios_shell_protocol::system::tracing::Level;

    #[test]
    fn parses_stats_command() {
        let command = parse_command("stats")
            .unwrap_or_else(|error| panic!("failed to parse stats command: {error}"));
        assert!(matches!(command, Command::ShowStats));
    }

    #[test]
    fn parses_use_command() {
        let command = parse_command("use rpc")
            .unwrap_or_else(|error| panic!("failed to parse use command: {error}"));
        assert!(matches!(command, Command::Use(View::Rpc)));
    }

    #[test]
    fn parses_help_topic() {
        let command = parse_command("help tracing")
            .unwrap_or_else(|error| panic!("failed to parse help command: {error}"));
        assert!(matches!(command, Command::Help(HelpTopic::Tracing)));
    }

    #[test]
    fn parses_tracing_level_info() {
        let command = parse_command("tracing level info")
            .unwrap_or_else(|error| panic!("failed to parse level command: {error}"));
        assert!(matches!(command, Command::TracingLevel(Some(Level::Info))));
    }
}
