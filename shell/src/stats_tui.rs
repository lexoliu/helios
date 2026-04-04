use std::io;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use async_channel::Receiver;
use crossterm::event::{self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_lite::future;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Text;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Terminal;

use crate::serial::RpcClient;
use crate::system;

pub async fn run(client: &mut RpcClient, period_ms: u64) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let guard = TerminalGuard;
    let events = spawn_events();
    let mut app = App::new(period_ms);

    app.refresh(client).await;

    loop {
        terminal.draw(|frame| draw(frame, &app))?;
        match future::or(async { events.recv().await.ok() }, async {
            async_io::Timer::after(Duration::from_millis(100)).await;
            Some(UiEvent::Tick)
        })
        .await
        {
            Some(UiEvent::Key(key)) => {
                if app.handle_key(key) {
                    break;
                }
            }
            Some(UiEvent::Resize) => {}
            Some(UiEvent::Tick) => {
                if app.should_refresh() {
                    app.refresh(client).await;
                }
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
    period_ms: u64,
    body: String,
    last_updated: Option<Instant>,
    status: String,
}

enum UiEvent {
    Key(KeyEvent),
    Resize,
    Tick,
}

impl App {
    fn new(period_ms: u64) -> Self {
        Self {
            period_ms,
            body: "waiting for first stats snapshot".to_owned(),
            last_updated: None,
            status: "live stats view; press q to return to the shell".to_owned(),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            || matches!(
                key.code,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL)
            )
    }

    fn should_refresh(&self) -> bool {
        match self.last_updated {
            None => true,
            Some(last_updated) => last_updated.elapsed() >= Duration::from_millis(self.period_ms),
        }
    }

    async fn refresh(&mut self, client: &mut RpcClient) {
        match system::fetch_stats(client).await {
            Ok(sample) => match system::render_stats_sample(&sample) {
                Ok(body) => {
                    self.body = body;
                    self.last_updated = Some(Instant::now());
                    self.status = "live stats view; press q to return to the shell".to_owned();
                }
                Err(error) => {
                    self.status = error.to_string();
                }
            },
            Err(error) => {
                self.status = error.to_string();
            }
        }
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode().context("failed to enable raw mode for stats view")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter stats screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("failed to create stats terminal")
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode for stats view")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("failed to leave stats screen")?;
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
        .constraints([Constraint::Min(8), Constraint::Length(3)])
        .split(frame.area());

    draw_body(frame, layout[0], app);
    draw_status(frame, layout[1], app);
}

fn draw_body(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let title = format!(
        "Stats  period={}ms  last={}",
        app.period_ms,
        format_updated(app.last_updated)
    );
    let paragraph = Paragraph::new(Text::from(app.body.clone()))
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let status = Paragraph::new(Text::from(app.status.clone()))
        .block(
            Block::default()
                .title("Controls")
                .borders(Borders::ALL)
                .border_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(status, area);
}

fn format_updated(updated: Option<Instant>) -> String {
    match updated {
        None => "never".to_owned(),
        Some(updated) => format!("{}ms ago", updated.elapsed().as_millis()),
    }
}
