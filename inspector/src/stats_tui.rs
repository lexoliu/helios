use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use futures_lite::future;
use helios_inspector_protocol::system::instances;
use helios_inspector_protocol::system::stats::{self, MemoryPressure};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, Borders, Cell, Gauge, Paragraph, Row, Table, Wrap,
};

use crate::serial::RpcClient;
use crate::system;
use crate::tui;

const LIVE_STATS_PERIOD_MS: u64 = 1_000;

pub async fn run(client: &mut RpcClient) -> Result<()> {
    let mut session = tui::Session::open(false, "stats view")?;
    let events = tui::spawn_events();
    let mut app = App::new(LIVE_STATS_PERIOD_MS);

    app.refresh(client).await;

    loop {
        session.terminal().draw(|frame| draw(frame, &app))?;
        match future::or(
            async { events.recv().await.ok().and_then(UiEvent::from_crossterm) },
            async {
                async_io::Timer::after(Duration::from_millis(100)).await;
                Some(UiEvent::Tick)
            },
        )
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

    session.close()
}

struct App {
    period_ms: u64,
    sample: Option<stats::Sample>,
    instances: Vec<instances::Instance>,
    last_updated: Option<Instant>,
    status: String,
}

enum UiEvent {
    Key(KeyEvent),
    Resize,
    Tick,
}

impl UiEvent {
    fn from_crossterm(event: CrosstermEvent) -> Option<Self> {
        match event {
            CrosstermEvent::Key(key) => Some(Self::Key(key)),
            CrosstermEvent::Resize(_, _) => Some(Self::Resize),
            _ => None,
        }
    }
}

impl App {
    fn new(period_ms: u64) -> Self {
        Self {
            period_ms,
            sample: None,
            instances: Vec::new(),
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
        match (
            system::fetch_stats(client).await,
            system::fetch_instances(client).await,
        ) {
            (Ok(sample), Ok(instances)) => {
                self.sample = Some(sample);
                self.instances = instances;
                self.last_updated = Some(Instant::now());
                self.status = "live stats view; press q to return to the shell".to_owned();
            }
            (Err(error), _) | (_, Err(error)) => {
                self.status = error.to_string();
            }
        }
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(14),
            Constraint::Length(1),
        ])
        .split(frame.area());

    match &app.sample {
        Some(sample) => {
            draw_summary(frame, layout[0], sample);
            draw_main_panels(frame, layout[1], sample, &app.instances);
        }
        None => draw_empty(frame, layout[0], layout[1], app),
    }

    draw_status(frame, layout[2], app);
}

fn draw_summary(frame: &mut ratatui::Frame<'_>, area: Rect, sample: &stats::Sample) {
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    let memory = MemoryView::new(sample);
    render_card(
        frame,
        cards[0],
        "Uptime",
        &format_duration(sample.uptime),
        None,
        Color::Cyan,
    );
    render_card(
        frame,
        cards[1],
        "Cores",
        &core_count_value(sample),
        core_count_detail(sample).as_deref(),
        Color::Green,
    );
    render_card(
        frame,
        cards[2],
        "Busiest Core",
        &busiest_core_text(sample),
        Some(&peak_cpu_percent(sample)),
        cpu_color(peak_cpu_busy(sample)),
    );
    render_card(
        frame,
        cards[3],
        "Memory",
        &memory.card_value(),
        Some(&memory.card_detail()),
        memory.accent(sample.memory.pressure),
    );
}

fn draw_main_panels(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    sample: &stats::Sample,
    instances: &[instances::Instance],
) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Min(6)])
        .split(area);

    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(19),
            Constraint::Percentage(23),
            Constraint::Percentage(24),
        ])
        .split(sections[0]);

    draw_cpu_chart(frame, panels[0], sample);
    draw_memory_panel(frame, panels[1], sample);
    draw_block_panel(frame, panels[2], sample);
    draw_iommu_panel(frame, panels[3], sample);
    draw_instances_panel(frame, sections[1], instances);
}

fn draw_empty(frame: &mut ratatui::Frame<'_>, top: Rect, body: Rect, app: &App) {
    let placeholder = Paragraph::new("waiting for first stats snapshot")
        .block(Block::default().title("Stats").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(placeholder, top);
    frame.render_widget(
        Paragraph::new(app.status.clone())
            .block(Block::default().title("Stats").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        body,
    );
}

fn draw_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let status = Paragraph::new(Text::from(app.status.clone()))
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true });
    frame.render_widget(status, area);
}

fn draw_instances_panel(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    instances: &[instances::Instance],
) {
    if instances.is_empty() {
        frame.render_widget(
            Paragraph::new("no live wasm instances reported")
                .block(Block::default().title("Instances").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let rows = instances.iter().map(|instance| {
        Row::new([
            Cell::from(instance.id.to_string()),
            Cell::from(instance.name.clone()),
            Cell::from(format_duration(instance.started_at)),
            Cell::from(format_duration(instance.uptime)),
            Cell::from(format!("{:>5.1}%", f64::from(instance.cpu_busy) / 10.0)),
            Cell::from(format_bytes(instance.memory_bytes)),
        ])
    });
    let header = Row::new(["id", "name", "started", "uptime", "cpu", "memory"])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(1);
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Min(16),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .block(Block::default().title("Instances").borders(Borders::ALL))
    .column_spacing(1);
    frame.render_widget(table, area);
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

    let memory = MemoryView::new(sample);

    let used_gauge = Gauge::default()
        .block(Block::default().title("Used Memory").borders(Borders::ALL))
        .gauge_style(pressure_color(sample.memory.pressure))
        .ratio(memory.used_ratio())
        .use_unicode(true)
        .label(memory.used_label());
    frame.render_widget(used_gauge, layout[0]);

    let free_gauge = Gauge::default()
        .block(
            Block::default()
                .title("Available Memory")
                .borders(Borders::ALL),
        )
        .gauge_style(Color::Green)
        .ratio(memory.free_ratio())
        .use_unicode(true)
        .label(memory.available_label());
    frame.render_widget(free_gauge, layout[1]);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("pressure ", Style::default().fg(Color::Cyan)),
            Span::styled(
                memory_pressure_label(sample.memory.pressure),
                Style::default().fg(pressure_color(sample.memory.pressure)),
            ),
        ]),
        Line::from(vec![
            Span::styled("used     ", Style::default().fg(Color::Cyan)),
            Span::raw(memory.used_detail()),
        ]),
        Line::from(vec![
            Span::styled("free     ", Style::default().fg(Color::Cyan)),
            Span::raw(memory.free_detail()),
        ]),
    ];
    lines.extend(balloon_lines(sample));
    let details = Paragraph::new(Text::from(lines))
        .block(Block::default().title("Memory").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(details, layout[2]);
}

/// What the host is holding through the memory balloon, on a machine
/// that has one.
///
/// A balloon that is following its target reads as one number; a
/// balloon that stopped short of what the host asked for is the case
/// worth seeing, so the target is shown alongside whenever the two
/// differ.
fn balloon_lines(sample: &stats::Sample) -> Vec<Line<'static>> {
    let Some(balloon) = &sample.balloon else {
        return Vec::new();
    };
    let held = if balloon.actual_bytes == balloon.target_bytes {
        format_bytes(balloon.actual_bytes)
    } else {
        format!(
            "{} of {} asked",
            format_bytes(balloon.actual_bytes),
            format_bytes(balloon.target_bytes)
        )
    };
    vec![Line::from(vec![
        Span::styled("balloon  ", Style::default().fg(Color::Cyan)),
        Span::raw(format!(
            "{held}, {} reported free",
            format_bytes(balloon.reported_bytes)
        )),
    ])]
}

/// The block device the kernel identified at boot, or the fact that it
/// has none.
fn draw_block_panel(frame: &mut ratatui::Frame<'_>, area: Rect, sample: &stats::Sample) {
    let lines = match &sample.block {
        Some(block) => {
            let mut features = Vec::new();
            if block.flush {
                features.push("flush");
            }
            if block.discard {
                features.push("discard");
            }
            if block.write_zeroes {
                features.push("write-zeroes");
            }
            if features.is_empty() {
                features.push("none");
            }
            vec![
                block_line("capacity", format_bytes(block.capacity_bytes)),
                block_line(
                    "block",
                    format!(
                        "{} logical / {} physical",
                        format_bytes(u64::from(block.block_bytes)),
                        format_bytes(u64::from(block.physical_block_bytes))
                    ),
                ),
                block_line(
                    "queues",
                    format!("{} x {} deep", block.queues, block.queue_depth),
                ),
                block_line("offloads", features.join(" ")),
                block_line(
                    "requests",
                    format!(
                        "{} read  {} write  {} flush  {} discard  {} zero",
                        block.reads,
                        block.writes,
                        block.flushes,
                        block.discards,
                        block.write_zeroes_requests
                    ),
                ),
            ]
        }
        None => vec![Line::from(Span::styled(
            "no block device",
            Style::default().fg(Color::DarkGray),
        ))],
    };

    let panel = Paragraph::new(Text::from(lines))
        .block(Block::default().title("Disk").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(panel, area);
}

/// What the platform's translation unit confines, or the fact that the
/// machine has none and its devices reach all of memory.
fn draw_iommu_panel(frame: &mut ratatui::Frame<'_>, area: Rect, sample: &stats::Sample) {
    let lines = match &sample.iommu {
        Some(iommu) => {
            let mut lines = vec![
                block_line("granule", format_bytes(iommu.granule_bytes)),
                block_line(
                    "bypass",
                    match iommu.global_bypass {
                        true => "unattached endpoints pass through".to_owned(),
                        false => "off".to_owned(),
                    },
                ),
                block_line("faults", iommu.faults.to_string()),
            ];
            lines.extend(iommu.endpoints.iter().map(|endpoint| {
                Line::from(vec![
                    Span::styled(
                        format!("{:<9}", format!("{:#06x}", endpoint.endpoint)),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw(format!(
                        "domain {}  {}",
                        endpoint.domain,
                        format_bytes(endpoint.mapped_bytes)
                    )),
                ])
            }));
            lines
        }
        None => vec![Line::from(Span::styled(
            "devices reach all of memory",
            Style::default().fg(Color::DarkGray),
        ))],
    };

    let panel = Paragraph::new(Text::from(lines))
        .block(Block::default().title("IOMMU").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(panel, area);
}

fn block_line(label: &'static str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), Style::default().fg(Color::Cyan)),
        Span::raw(value),
    ])
}

fn render_card(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    value: &str,
    detail: Option<&str>,
    accent: Color,
) {
    let mut lines = vec![Line::from(Span::styled(
        value.to_owned(),
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    ))];
    if let Some(detail) = detail {
        lines.push(Line::from(detail.to_owned()));
    }

    let card = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent)),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(card, area);
}

struct MemoryView {
    total_bytes: Option<u64>,
    available_bytes: u64,
}

impl MemoryView {
    fn new(sample: &stats::Sample) -> Self {
        let total_bytes = (sample.memory.total_bytes != 0).then_some(sample.memory.total_bytes);
        let available_bytes = total_bytes
            .map(|total| sample.memory.available_bytes.min(total))
            .unwrap_or(0);

        Self {
            total_bytes,
            available_bytes,
        }
    }

    fn used_bytes(&self) -> Option<u64> {
        self.total_bytes
            .map(|total_bytes| total_bytes.saturating_sub(self.available_bytes))
    }

    fn used_ratio(&self) -> f64 {
        match (self.used_bytes(), self.total_bytes) {
            (Some(used_bytes), Some(total_bytes)) if total_bytes != 0 => {
                used_bytes as f64 / total_bytes as f64
            }
            _ => 0.0,
        }
    }

    fn free_ratio(&self) -> f64 {
        match self.total_bytes {
            Some(total_bytes) if total_bytes != 0 => {
                self.available_bytes as f64 / total_bytes as f64
            }
            _ => 0.0,
        }
    }

    fn card_value(&self) -> String {
        match self.available_bytes() {
            Some(available_bytes) => format!("{} free", format_bytes(available_bytes)),
            None => "unreported".to_owned(),
        }
    }

    fn card_detail(&self) -> String {
        match self.total_bytes {
            Some(total_bytes) => format!("{} total", format_bytes(total_bytes)),
            None => "stats unavailable".to_owned(),
        }
    }

    fn accent(&self, pressure: MemoryPressure) -> Color {
        match self.total_bytes {
            Some(_) => pressure_color(pressure),
            None => Color::DarkGray,
        }
    }

    fn used_label(&self) -> String {
        match (self.used_bytes(), self.total_bytes) {
            (Some(used_bytes), Some(total_bytes)) => {
                format!(
                    "{} / {}",
                    format_bytes(used_bytes),
                    format_bytes(total_bytes)
                )
            }
            _ => "unreported".to_owned(),
        }
    }

    fn available_label(&self) -> String {
        match self.available_bytes() {
            Some(available_bytes) => format!("{} free", format_bytes(available_bytes)),
            None => "unreported".to_owned(),
        }
    }

    fn used_detail(&self) -> String {
        self.used_bytes()
            .map(format_bytes)
            .unwrap_or_else(|| "unreported".to_owned())
    }

    fn free_detail(&self) -> String {
        self.available_bytes()
            .map(format_bytes)
            .unwrap_or_else(|| "unreported".to_owned())
    }

    fn available_bytes(&self) -> Option<u64> {
        self.total_bytes.map(|_| self.available_bytes)
    }
}

fn format_duration(nanos: u64) -> String {
    let seconds = nanos / 1_000_000_000;
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

pub(crate) fn format_bytes(bytes: u64) -> String {
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

fn core_count_value(sample: &stats::Sample) -> String {
    sample.processors.configured.to_string()
}

fn core_count_detail(sample: &stats::Sample) -> Option<String> {
    (sample.processors.online != sample.processors.configured)
        .then(|| format!("{} online", sample.processors.online))
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
