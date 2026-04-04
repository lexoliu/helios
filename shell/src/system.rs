use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context as _, Result};
use helios_shell_protocol::system::{stats, tracing};

use crate::runtime;
use crate::serial::RpcClient;

const INITIAL_REMOTE_TIMEOUT: Duration = Duration::from_secs(180);

pub struct StatsConfig {
    pub period_ms: u64,
}

pub struct TracingConfig {
    pub limit: u32,
    pub min_level: Option<tracing::Level>,
    pub target_prefixes: Vec<String>,
}

impl StatsConfig {
    pub fn new() -> Self {
        Self { period_ms: 1_000 }
    }
}

impl TracingConfig {
    pub fn new() -> Self {
        Self {
            limit: 64,
            min_level: Some(tracing::Level::Info),
            target_prefixes: Vec::new(),
        }
    }

    pub fn filter(&self) -> tracing::Filter {
        tracing::Filter {
            min_level: self.min_level,
            target_prefixes: self.target_prefixes.clone(),
        }
    }
}

pub async fn fetch_stats(client: &mut RpcClient) -> Result<stats::Sample> {
    runtime::timeout(INITIAL_REMOTE_TIMEOUT, stats::snapshot(client))
        .await
        .context("timed out waiting for remote stats snapshot")?
        .context("failed to fetch remote stats snapshot")
}

pub async fn fetch_tracing(
    client: &mut RpcClient,
    config: &TracingConfig,
) -> Result<Vec<tracing::Event>> {
    runtime::timeout(
        INITIAL_REMOTE_TIMEOUT,
        tracing::recent(client, &config.filter(), config.limit),
    )
    .await
    .context("timed out waiting for remote tracing events")?
    .context("failed to fetch remote tracing events")
}

pub async fn run_tracing(
    mut client: RpcClient,
    limit: u32,
    min_level: Option<&str>,
    target_prefixes: Vec<String>,
) -> Result<()> {
    let mut config = TracingConfig::new();
    config.limit = limit;
    config.min_level = match min_level {
        Some(level) => parse_level(level)?,
        None => config.min_level,
    };
    config.target_prefixes = target_prefixes;

    let events = fetch_tracing(&mut client, &config).await?;
    std::io::stdout().write_all(render_tracing_events(&events)?.as_bytes())?;
    std::io::stdout().write_all(b"\n")?;
    Ok(())
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

pub fn render_stats_sample(sample: &stats::Sample) -> Result<String> {
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

pub fn render_tracing_events(events: &[tracing::Event]) -> Result<String> {
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
