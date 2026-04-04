use std::io;
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result};
use async_channel::Receiver;
use crossterm::event::{
    self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_lite::future;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs, Wrap};
use ratatui::Terminal;

use crate::rpc::RpcPane;
use crate::serial::{RpcClient, SerialIo};
use crate::system::{self, StatsPane, TracingPane};
use helios_shell_protocol::system::tracing;

pub async fn run(io: SerialIo) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let guard = TerminalGuard;
    let mut client = io.into_client();
    let mut app = App::new();
    let events = spawn_events();

    app.refresh_current(&mut client).await;

    loop {
        terminal.draw(|frame| draw(frame, &app))?;
        match future::or(async { events.recv().await.ok() }, async {
            async_io::Timer::after(Duration::from_millis(250)).await;
            Some(UiEvent::Tick)
        })
        .await
        {
            Some(UiEvent::Key(key)) => {
                if app.handle_key(&mut client, key).await? {
                    break;
                }
            }
            Some(UiEvent::Resize) => {}
            Some(UiEvent::Tick) => {
                app.tick(&mut client).await;
            }
            None => break,
        }
    }

    restore_terminal(&mut terminal)?;
    std::mem::forget(guard);
    Ok(())
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}

struct App {
    tab: Tab,
    input: String,
    status: String,
    stats: StatsPane,
    tracing: TracingPane,
    rpc: RpcPane,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Stats,
    Tracing,
    Rpc,
}

enum UiEvent {
    Key(KeyEvent),
    Resize,
    Tick,
}

impl App {
    fn new() -> Self {
        Self {
            tab: Tab::Stats,
            input: String::new(),
            status: "type `help` for commands".to_owned(),
            stats: StatsPane::new(),
            tracing: TracingPane::new(),
            rpc: RpcPane::new(),
        }
    }

    async fn handle_key(&mut self, client: &mut RpcClient, key: KeyEvent) -> Result<bool> {
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            KeyCode::Tab => {
                self.cycle_tab();
                self.refresh_current(client).await;
            }
            KeyCode::F(5) => self.refresh_current(client).await,
            KeyCode::Esc => self.input.clear(),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => {
                let command = self.input.trim().to_owned();
                self.input.clear();
                if !command.is_empty() && self.execute_command(client, &command).await? {
                    return Ok(true);
                }
            }
            KeyCode::Char(ch) => {
                self.input.push(ch);
            }
            _ => {}
        }
        Ok(false)
    }

    async fn execute_command(&mut self, client: &mut RpcClient, command: &str) -> Result<bool> {
        match parse_command(command)? {
            Command::Help => {
                self.status = help_text();
            }
            Command::Quit => return Ok(true),
            Command::SwitchTab(tab) => {
                self.tab = tab;
                self.refresh_current(client).await;
            }
            Command::Refresh => self.refresh_current(client).await,
            Command::StatsPeriod(period_ms) => {
                self.stats.period_ms = period_ms;
                self.status = format!("stats refresh period set to {period_ms}ms");
                self.refresh_current(client).await;
            }
            Command::TracingLimit(limit) => {
                self.tracing.limit = limit;
                self.status = format!("tracing history limit set to {limit}");
                self.refresh_current(client).await;
            }
            Command::TracingLevel(level) => {
                self.tracing.min_level = level;
                self.status = format!("tracing minimum level set to {}", level_name(level));
                self.refresh_current(client).await;
            }
            Command::TracingTargets(prefixes) => {
                self.tracing.target_prefixes = prefixes;
                self.status = format!(
                    "tracing targets set to {}",
                    system::format_targets(&self.tracing.target_prefixes)
                );
                self.refresh_current(client).await;
            }
            Command::RpcInstance(instance) => {
                self.rpc.instance = instance;
                self.status = "rpc instance updated".to_owned();
            }
            Command::RpcFunc(func) => {
                self.rpc.func = func;
                self.status = "rpc function updated".to_owned();
            }
            Command::RpcPayload(payload) => {
                self.rpc.request_hex = payload;
                self.status = "rpc payload updated".to_owned();
            }
            Command::RpcCall => match self.rpc.call(client).await {
                Ok(()) => {
                    self.tab = Tab::Rpc;
                    self.status = format!(
                        "rpc call completed for {}.{}",
                        self.rpc.instance, self.rpc.func
                    );
                }
                Err(error) => {
                    self.status = error.to_string();
                }
            },
        }
        Ok(false)
    }

    async fn tick(&mut self, client: &mut RpcClient) {
        match self.tab {
            Tab::Stats if should_refresh(self.stats.last_updated, self.stats.period_ms) => {
                self.refresh_current(client).await;
            }
            Tab::Tracing if should_refresh(self.tracing.last_updated, 1_500) => {
                self.refresh_current(client).await;
            }
            Tab::Rpc => {}
            _ => {}
        }
    }

    async fn refresh_current(&mut self, client: &mut RpcClient) {
        let outcome = match self.tab {
            Tab::Stats => self.stats.refresh(client).await,
            Tab::Tracing => self.tracing.refresh(client).await,
            Tab::Rpc => Ok(()),
        };
        if let Err(error) = outcome {
            self.status = error.to_string();
        }
    }

    fn cycle_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Stats => Tab::Tracing,
            Tab::Tracing => Tab::Rpc,
            Tab::Rpc => Tab::Stats,
        };
    }
}

enum Command {
    Help,
    Quit,
    SwitchTab(Tab),
    Refresh,
    StatsPeriod(u64),
    TracingLimit(u32),
    TracingLevel(Option<tracing::Level>),
    TracingTargets(Vec<String>),
    RpcInstance(String),
    RpcFunc(String),
    RpcPayload(String),
    RpcCall,
}

fn parse_command(input: &str) -> Result<Command> {
    let mut parts = input.splitn(3, ' ');
    let head = parts.next().context("empty command")?;
    let middle = parts.next().unwrap_or_default();
    let tail = parts.next().unwrap_or_default().trim();

    match (head, middle) {
        ("help", _) => Ok(Command::Help),
        ("quit", _) | ("exit", _) => Ok(Command::Quit),
        ("refresh", _) => Ok(Command::Refresh),
        ("tab", "stats") => Ok(Command::SwitchTab(Tab::Stats)),
        ("tab", "tracing") => Ok(Command::SwitchTab(Tab::Tracing)),
        ("tab", "rpc") => Ok(Command::SwitchTab(Tab::Rpc)),
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

fn help_text() -> String {
    "tab stats|tracing|rpc | refresh | stats period <ms> | tracing limit <n> | tracing level <none|error|warn|info|debug|trace> | tracing targets <a,b> | rpc instance <name> | rpc func <name> | rpc payload <hex> | rpc call | quit".to_owned()
}

fn should_refresh(last_updated: Option<std::time::Instant>, period_ms: u64) -> bool {
    match last_updated {
        None => true,
        Some(last) => last.elapsed() >= Duration::from_millis(period_ms),
    }
}

fn level_name(level: Option<tracing::Level>) -> &'static str {
    use tracing::Level;

    match level {
        None => "none",
        Some(Level::Error) => "error",
        Some(Level::Warn) => "warn",
        Some(Level::Info) => "info",
        Some(Level::Debug) => "debug",
        Some(Level::Trace) => "trace",
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("failed to create terminal")
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to restore cursor")
}

fn spawn_events() -> Receiver<UiEvent> {
    let (tx, rx) = async_channel::unbounded();
    thread::spawn(move || loop {
        if event::poll(Duration::from_millis(100)).unwrap_or(false) {
            match event::read() {
                Ok(CrosstermEvent::Key(key)) => {
                    if tx.send_blocking(UiEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(CrosstermEvent::Resize(_, _)) => {
                    if tx.send_blocking(UiEvent::Resize).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
    rx
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
            Constraint::Length(4),
        ])
        .split(frame.area());

    draw_tabs(frame, layout[0], app);
    draw_body(frame, layout[1], app);
    draw_status(frame, layout[2], app);
    draw_input(frame, layout[3], app);
}

fn draw_tabs(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let titles = ["Stats", "Tracing", "RPC"]
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    let selected = match app.tab {
        Tab::Stats => 0,
        Tab::Tracing => 1,
        Tab::Rpc => 2,
    };
    let tabs = Tabs::new(titles)
        .block(Block::default().title("Helios Shell").borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .select(selected);
    frame.render_widget(tabs, area);
}

fn draw_body(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let (title, body) = match app.tab {
        Tab::Stats => (
            format!(
                "Stats  period={}ms  last={}",
                app.stats.period_ms,
                format_updated(app.stats.last_updated)
            ),
            app.stats.last_rendered.as_str(),
        ),
        Tab::Tracing => (
            format!(
                "Tracing  limit={}  level={}  targets={}  last={}",
                app.tracing.limit,
                level_name(app.tracing.min_level),
                system::format_targets(&app.tracing.target_prefixes),
                format_updated(app.tracing.last_updated)
            ),
            app.tracing.last_rendered.as_str(),
        ),
        Tab::Rpc => (
            format!("RPC  instance={}  func={}", app.rpc.instance, app.rpc.func),
            app.rpc.response_hex.as_str(),
        ),
    };
    let paragraph = Paragraph::new(Text::from(body.to_owned()))
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let status = Paragraph::new(Text::from(app.status.clone()))
        .block(
            Block::default()
                .title("Status  Ctrl-C quit  Tab switch  F5 refresh")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: true })
        .style(Style::default().fg(Color::Yellow));
    frame.render_widget(status, area);
}

fn draw_input(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let text = vec![
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Green)),
            Span::raw(app.input.as_str()),
        ]),
        Line::from("examples: `refresh`, `tab tracing`, `rpc call`, `help`"),
    ];
    let input = Paragraph::new(text)
        .block(Block::default().title("Command").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, area);
    frame.set_cursor_position((area.x + 3 + app.input.len() as u16, area.y + 1));
}

fn format_updated(updated: Option<std::time::Instant>) -> String {
    match updated {
        None => "never".to_owned(),
        Some(instant) => format!("{}ms ago", instant.elapsed().as_millis()),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_command, Command, Tab};
    use helios_shell_protocol::system::tracing::Level;

    #[test]
    fn parses_tab_command() {
        let command = parse_command("tab tracing")
            .unwrap_or_else(|error| panic!("failed to parse tab command: {error}"));
        assert!(matches!(command, Command::SwitchTab(Tab::Tracing)));
    }

    #[test]
    fn parses_tracing_level_none() {
        let command = parse_command("tracing level none")
            .unwrap_or_else(|error| panic!("failed to parse level command: {error}"));
        assert!(matches!(command, Command::TracingLevel(None)));
    }

    #[test]
    fn parses_tracing_level_info() {
        let command = parse_command("tracing level info")
            .unwrap_or_else(|error| panic!("failed to parse level command: {error}"));
        assert!(matches!(command, Command::TracingLevel(Some(Level::Info))));
    }
}
