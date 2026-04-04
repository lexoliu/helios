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
use helios_shell_protocol::system::stats::{self, MemoryPressure};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Bar, BarChart, BarGroup, Block, Borders, Gauge, Paragraph, Wrap};
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
    sample: Option<stats::Sample>,
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
            sample: None,
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
            Ok(sample) => {
                self.sample = Some(sample);
                self.last_updated = Some(Instant::now());
                self.status = "live stats view; press q to return to the shell".to_owned();
            }
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
        .constraints([
            Constraint::Length(5),
            Constraint::Length(7),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(frame.area());

    match &app.sample {
        Some(sample) => {
            draw_summary(frame, layout[0], app, sample);
            draw_main_panels(frame, layout[1], sample);
            draw_details(frame, layout[2], app, sample);
        }
        None => draw_empty(frame, layout[0], layout[1], layout[2], app),
    }

    draw_status(frame, layout[3], app);
}

fn draw_summary(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App, sample: &stats::Sample) {
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    render_card(
        frame,
        cards[0],
        "Uptime",
        &format_duration(sample.uptime),
        &format!("refresh {}ms", app.period_ms),
        Color::Cyan,
    );
    render_card(
        frame,
        cards[1],
        "Processors",
        &format!(
            "{}/{} online",
            sample.processors.online, sample.processors.configured
        ),
        &format!("{} cores reporting", sample.processors.utilization.len()),
        Color::Green,
    );
    render_card(
        frame,
        cards[2],
        "Busiest Core",
        &busiest_core_text(sample),
        &format!("peak {}", peak_cpu_percent(sample)),
        cpu_color(peak_cpu_busy(sample)),
    );
    render_card(
        frame,
        cards[3],
        "Memory Pressure",
        memory_pressure_label(sample.memory.pressure),
        &format!("{} free", format_bytes(sample.memory.available_bytes)),
        pressure_color(sample.memory.pressure),
    );
}

fn draw_main_panels(frame: &mut ratatui::Frame<'_>, area: Rect, sample: &stats::Sample) {
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);

    draw_cpu_chart(frame, panels[0], sample);
    draw_memory_panel(frame, panels[1], sample);
}

fn draw_details(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App, sample: &stats::Sample) {
    let details = vec![
        Line::from(vec![
            Span::styled("timestamp ", Style::default().fg(Color::Cyan)),
            Span::raw(sample.timestamp.to_string()),
        ]),
        Line::from(vec![
            Span::styled("refresh  ", Style::default().fg(Color::Cyan)),
            Span::raw(format!("every {}ms", app.period_ms)),
        ]),
        Line::from(vec![
            Span::styled("memory   ", Style::default().fg(Color::Cyan)),
            Span::raw(format!(
                "{} total, {} available, {} used",
                format_bytes(sample.memory.total_bytes),
                format_bytes(sample.memory.available_bytes),
                format_bytes(
                    sample
                        .memory
                        .total_bytes
                        .saturating_sub(sample.memory.available_bytes)
                )
            )),
        ]),
        Line::from(vec![
            Span::styled("cpu load ", Style::default().fg(Color::Cyan)),
            Span::raw(cpu_load_line(sample)),
        ]),
    ];

    let details = Paragraph::new(Text::from(details))
        .block(Block::default().title("Details").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(details, area);
}

fn draw_empty(frame: &mut ratatui::Frame<'_>, top: Rect, middle: Rect, bottom: Rect, app: &App) {
    let placeholder = Paragraph::new("waiting for first stats snapshot")
        .block(Block::default().title("Stats").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(placeholder, top);
    frame.render_widget(
        Paragraph::new(app.status.clone())
            .block(Block::default().title("State").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        middle,
    );
    frame.render_widget(
        Paragraph::new("no sample available yet")
            .block(Block::default().title("Details").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        bottom,
    );
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

fn draw_cpu_chart(frame: &mut ratatui::Frame<'_>, area: Rect, sample: &stats::Sample) {
    let bars: Vec<Bar<'_>> = sample
        .processors
        .utilization
        .iter()
        .map(|processor| {
            let label = Line::from(format!("cpu{}", processor.id));
            let style = Style::default().fg(cpu_color(processor.busy));
            Bar::default()
                .value(u64::from(processor.busy))
                .label(label)
                .text_value(format!("{:>5.1}%", f64::from(processor.busy) / 10.0))
                .style(style)
                .value_style(Style::default().add_modifier(Modifier::BOLD))
        })
        .collect();

    let chart = BarChart::default()
        .block(
            Block::default()
                .title("CPU Utilization")
                .borders(Borders::ALL),
        )
        .data(BarGroup::default().bars(&bars))
        .max(1_000)
        .direction(Direction::Horizontal)
        .bar_width(1)
        .bar_gap(0);

    frame.render_widget(chart, area);
}

fn draw_memory_panel(frame: &mut ratatui::Frame<'_>, area: Rect, sample: &stats::Sample) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(area);

    let total = sample.memory.total_bytes;
    let available = sample.memory.available_bytes.min(total);
    let used = total.saturating_sub(available);
    let used_ratio = if total == 0 {
        0.0
    } else {
        used as f64 / total as f64
    };
    let free_ratio = if total == 0 {
        0.0
    } else {
        available as f64 / total as f64
    };

    let used_gauge = Gauge::default()
        .block(Block::default().title("Used Memory").borders(Borders::ALL))
        .gauge_style(pressure_color(sample.memory.pressure))
        .ratio(used_ratio)
        .use_unicode(true)
        .label(if total == 0 {
            "unreported".to_owned()
        } else {
            format!("{} / {}", format_bytes(used), format_bytes(total))
        });
    frame.render_widget(used_gauge, layout[0]);

    let free_gauge = Gauge::default()
        .block(
            Block::default()
                .title("Available Memory")
                .borders(Borders::ALL),
        )
        .gauge_style(Color::Green)
        .ratio(free_ratio)
        .use_unicode(true)
        .label(if total == 0 {
            "unreported".to_owned()
        } else {
            format!("{} free", format_bytes(available))
        });
    frame.render_widget(free_gauge, layout[1]);

    let details = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled("pressure ", Style::default().fg(Color::Cyan)),
            Span::styled(
                memory_pressure_label(sample.memory.pressure),
                Style::default().fg(pressure_color(sample.memory.pressure)),
            ),
        ]),
        Line::from(vec![
            Span::styled("usage    ", Style::default().fg(Color::Cyan)),
            Span::raw(if total == 0 {
                "unreported".to_owned()
            } else {
                format!("{:.1}%", used_ratio * 100.0)
            }),
        ]),
    ]))
    .block(Block::default().title("Memory").borders(Borders::ALL))
    .wrap(Wrap { trim: true });
    frame.render_widget(details, layout[2]);
}

fn render_card(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    value: &str,
    detail: &str,
    accent: Color,
) {
    let card = Paragraph::new(Text::from(vec![
        Line::from(Span::styled(
            value.to_owned(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(detail.to_owned()),
    ]))
    .block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(accent)),
    )
    .wrap(Wrap { trim: true });
    frame.render_widget(card, area);
}

fn format_duration(nanos: u64) -> String {
    let seconds = nanos / 1_000_000_000;
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn peak_cpu_busy(sample: &stats::Sample) -> u16 {
    sample
        .processors
        .utilization
        .iter()
        .map(|processor| processor.busy)
        .max()
        .unwrap_or(0)
}

fn peak_cpu_percent(sample: &stats::Sample) -> String {
    format!("{:.1}%", f64::from(peak_cpu_busy(sample)) / 10.0)
}

fn busiest_core_text(sample: &stats::Sample) -> String {
    sample
        .processors
        .utilization
        .iter()
        .max_by_key(|processor| processor.busy)
        .map(|processor| format!("cpu{}", processor.id))
        .unwrap_or_else(|| "n/a".to_owned())
}

fn cpu_load_line(sample: &stats::Sample) -> String {
    sample
        .processors
        .utilization
        .iter()
        .map(|processor| {
            format!(
                "cpu{} {:>5.1}%",
                processor.id,
                f64::from(processor.busy) / 10.0
            )
        })
        .collect::<Vec<_>>()
        .join("   ")
}

fn cpu_color(busy: u16) -> Color {
    match busy {
        0..=249 => Color::Green,
        250..=599 => Color::Yellow,
        600..=849 => Color::LightRed,
        _ => Color::Red,
    }
}

fn pressure_color(pressure: MemoryPressure) -> Color {
    match pressure {
        MemoryPressure::Nominal => Color::Green,
        MemoryPressure::Elevated => Color::Yellow,
        MemoryPressure::High => Color::LightRed,
        MemoryPressure::Critical => Color::Red,
    }
}

fn memory_pressure_label(pressure: MemoryPressure) -> &'static str {
    match pressure {
        MemoryPressure::Nominal => "nominal",
        MemoryPressure::Elevated => "elevated",
        MemoryPressure::High => "high",
        MemoryPressure::Critical => "critical",
    }
}
