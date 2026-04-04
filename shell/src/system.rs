use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context as _, Result};
use helios_shell_protocol::system::{stats, tracing};

use crate::runtime;
use crate::serial::{RpcClient, SerialIo};

const INITIAL_REMOTE_TIMEOUT: Duration = Duration::from_secs(180);

pub struct StatsPane {
    pub period_ms: u64,
    pub last_rendered: String,
    pub last_updated: Option<Instant>,
}

pub struct TracingPane {
    pub limit: u32,
    pub min_level: Option<tracing::Level>,
    pub target_prefixes: Vec<String>,
    pub last_rendered: String,
    pub last_updated: Option<Instant>,
}

impl StatsPane {
    pub fn new() -> Self {
        Self {
            period_ms: 1_000,
            last_rendered: "waiting for first stats sample".to_owned(),
            last_updated: None,
        }
    }

    pub async fn refresh(&mut self, client: &mut RpcClient) -> Result<()> {
        let sample = runtime::timeout(INITIAL_REMOTE_TIMEOUT, stats::snapshot(client))
            .await
            .context("timed out waiting for remote stats snapshot")?
            .context("failed to fetch remote stats snapshot")?;
        self.last_rendered = render_sample(&sample)?;
        self.last_updated = Some(Instant::now());
        Ok(())
    }
}

pub async fn run_stats(io: SerialIo, period_ms: u64) -> Result<()> {
    let mut client = io.into_client();
    let period = duration_to_nanos(Duration::from_millis(period_ms));
    let mut stream = runtime::timeout(
        INITIAL_REMOTE_TIMEOUT,
        stats::subscribe(&mut client, period),
    )
    .await
    .context("timed out waiting for remote stats subscription")?
    .context("failed to subscribe to remote stats stream")?;
    let mut stdout = std::io::stdout();

    while let Some(sample) = runtime::timeout(INITIAL_REMOTE_TIMEOUT, stream.next())
        .await
        .context("timed out waiting for streamed stats sample")?
        .context("failed to read streamed stats sample")?
    {
        stdout.write_all(render_sample(&sample)?.as_bytes())?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }

    Ok(())
}

pub async fn run_tracing(
    io: SerialIo,
    limit: u32,
    min_level: Option<&str>,
    target_prefixes: Vec<String>,
) -> Result<()> {
    let mut client = io.into_client();
    let mut pane = TracingPane::new();
    pane.limit = limit;
    pane.min_level = match min_level {
        Some(level) => parse_level(level)?,
        None => pane.min_level,
    };
    pane.target_prefixes = target_prefixes;
    pane.refresh(&mut client).await?;
    std::io::stdout().write_all(pane.last_rendered.as_bytes())?;
    Ok(())
}

impl TracingPane {
    pub fn new() -> Self {
        Self {
            limit: 64,
            min_level: Some(tracing::Level::Info),
            target_prefixes: Vec::new(),
            last_rendered: "waiting for tracing history".to_owned(),
            last_updated: None,
        }
    }

    pub fn filter(&self) -> tracing::Filter {
        tracing::Filter {
            min_level: self.min_level,
            target_prefixes: self.target_prefixes.clone(),
        }
    }

    pub async fn refresh(&mut self, client: &mut RpcClient) -> Result<()> {
        let events = runtime::timeout(
            INITIAL_REMOTE_TIMEOUT,
            tracing::recent(client, &self.filter(), self.limit),
        )
        .await
        .context("timed out waiting for remote tracing events")?
        .context("failed to fetch remote tracing events")?;
        self.last_rendered = render_events(&events)?;
        self.last_updated = Some(Instant::now());
        Ok(())
    }
}

pub fn parse_level(value: &str) -> Result<Option<tracing::Level>> {
    use tracing::Level;

    let level = match value {
        "none" => return Ok(None),
        "error" => Level::Error,
        "warn" => Level::Warn,
        "info" => Level::Info,
        "debug" => Level::Debug,
        "trace" => Level::Trace,
        _ => anyhow::bail!("unknown tracing level {value}"),
    };
    Ok(Some(level))
}

pub fn format_targets(prefixes: &[String]) -> String {
    if prefixes.is_empty() {
        "any".to_owned()
    } else {
        prefixes.join(", ")
    }
}

fn render_sample(sample: &stats::Sample) -> Result<String> {
    let mut text = String::new();
    writeln!(&mut text, "timestamp: {}", sample.timestamp)?;
    writeln!(&mut text, "uptime: {}", sample.uptime)?;
    writeln!(
        &mut text,
        "processors: configured={} online={}",
        sample.processors.configured, sample.processors.online
    )?;
    for processor in &sample.processors.utilization {
        writeln!(
            &mut text,
            "  cpu{} busy={} permille",
            processor.id, processor.busy
        )?;
    }
    writeln!(
        &mut text,
        "memory: total={} available={} pressure={}",
        sample.memory.total_bytes,
        sample.memory.available_bytes,
        memory_pressure_name(sample.memory.pressure),
    )?;
    Ok(text)
}

fn render_events(events: &[tracing::Event]) -> Result<String> {
    if events.is_empty() {
        return Ok("no tracing events matched the current filter".to_owned());
    }

    let mut text = String::new();
    for event in events {
        write!(
            &mut text,
            "[{}] {} {}",
            event.timestamp,
            level_name(event.level),
            event.target
        )?;
        for field in &event.fields {
            write!(&mut text, " {}=", field.key)?;
            write_value(&mut text, &field.value)?;
        }
        writeln!(&mut text)?;
    }
    Ok(text)
}

fn level_name(level: tracing::Level) -> &'static str {
    use tracing::Level;

    match level {
        Level::Error => "ERROR",
        Level::Warn => "WARN",
        Level::Info => "INFO",
        Level::Debug => "DEBUG",
        Level::Trace => "TRACE",
    }
}

fn memory_pressure_name(pressure: stats::MemoryPressure) -> &'static str {
    use stats::MemoryPressure;

    match pressure {
        MemoryPressure::Nominal => "nominal",
        MemoryPressure::Elevated => "elevated",
        MemoryPressure::High => "high",
        MemoryPressure::Critical => "critical",
    }
}

fn write_value(output: &mut String, value: &tracing::Value) -> Result<()> {
    use tracing::Value;

    match value {
        Value::Boolean(value) => write!(output, "{value}")?,
        Value::Signed64(value) => write!(output, "{value}")?,
        Value::Unsigned64(value) => write!(output, "{value}")?,
        Value::Float64(value) => write!(output, "{value}")?,
        Value::Text(value) => write!(output, "{value}")?,
        Value::Blob(value) => write!(output, "{value:?}")?,
    }
    Ok(())
}

fn duration_to_nanos(duration: Duration) -> u64 {
    duration
        .as_nanos()
        .try_into()
        .expect("duration does not fit into u64 nanoseconds")
}
