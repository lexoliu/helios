use std::collections::{HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context as _, Result};
use helios_shell_protocol::system::{stats, tracing};

use crate::runtime;
use crate::serial::RpcClient;

const INITIAL_REMOTE_TIMEOUT: Duration = Duration::from_secs(180);
const LIVE_TRACING_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub struct TracingConfig {
    pub limit: u32,
    pub min_level: Option<tracing::Level>,
    pub target_prefixes: Vec<String>,
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

    stream_tracing(&mut client, &config).await
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

pub async fn stream_tracing(client: &mut RpcClient, config: &TracingConfig) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    let mut emitted = EmittedEvents::new(config.limit);

    loop {
        let events = fetch_tracing(client, config).await?;
        for event in events {
            let line = render_tracing_event(&event)?;
            if emitted.insert(line.clone()) {
                stdout.write_all(line.as_bytes())?;
                stdout.write_all(b"\n")?;
            }
        }
        stdout.flush()?;
        async_io::Timer::after(LIVE_TRACING_POLL_INTERVAL).await;
    }
}

fn render_tracing_event(event: &tracing::Event) -> Result<String> {
    let mut text = String::new();
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

struct EmittedEvents {
    capacity: usize,
    lines: VecDeque<String>,
    seen: HashSet<String>,
}

impl EmittedEvents {
    fn new(limit: u32) -> Self {
        let capacity = usize::try_from(limit.max(1))
            .expect("tracing limit does not fit into usize")
            .saturating_mul(4);
        Self {
            capacity,
            lines: VecDeque::with_capacity(capacity),
            seen: HashSet::with_capacity(capacity),
        }
    }

    fn insert(&mut self, line: String) -> bool {
        if !self.seen.insert(line.clone()) {
            return false;
        }

        self.lines.push_back(line);
        while self.lines.len() > self.capacity {
            let removed = self
                .lines
                .pop_front()
                .expect("emitted tracing event ring must not underflow");
            self.seen.remove(&removed);
        }
        true
    }
}
