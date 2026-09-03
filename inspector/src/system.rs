use async_signal::{Signal, Signals};
use futures_lite::{StreamExt as _, future};
use std::collections::{HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context as _, Result};
use helios_inspector_protocol::system::{instances, stats, tracing};
use nu_ansi_term::{Color, Style as AnsiStyle};

use crate::TracingCommand;
use crate::remote;
use crate::serial::RpcClient;

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
    remote::call(stats::snapshot(client), "remote stats snapshot")
        .await
        .context("failed to fetch remote stats snapshot")
}

pub async fn fetch_instances(client: &mut RpcClient) -> Result<Vec<instances::Instance>> {
    remote::call(instances::snapshot(client), "remote instances snapshot")
        .await
        .context("failed to fetch remote instances snapshot")
}

pub async fn fetch_tracing(
    client: &mut RpcClient,
    config: &TracingConfig,
) -> Result<Vec<tracing::Event>> {
    remote::call(
        tracing::recent(client, &config.filter(), config.limit),
        "remote tracing events",
    )
    .await
    .context("failed to fetch remote tracing events")
}

pub async fn run_tracing(
    mut client: RpcClient,
    limit: u32,
    min_level: Option<&str>,
    target_prefixes: Vec<String>,
) -> Result<()> {
    let config = tracing_config(limit, min_level, target_prefixes)?;
    stream_tracing(&mut client, &config).await
}

pub async fn stream_tracing_command(
    client: &mut RpcClient,
    command: &TracingCommand,
) -> Result<()> {
    let config = tracing_config(
        command.limit,
        command.min_level.as_deref(),
        command.target_prefix.clone(),
    )?;
    stream_tracing(client, &config).await
}

pub fn tracing_config(
    limit: u32,
    min_level: Option<&str>,
    target_prefixes: Vec<String>,
) -> Result<TracingConfig> {
    let mut config = TracingConfig::new();
    config.limit = limit;
    config.min_level = match min_level {
        Some(level) => parse_level(level)?,
        None => config.min_level,
    };
    config.target_prefixes = target_prefixes;
    Ok(config)
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

pub async fn stream_tracing(client: &mut RpcClient, config: &TracingConfig) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    let mut emitted = EmittedEvents::new(config.limit);
    let mut signals = Signals::new([Signal::Int]).context("failed to listen for SIGINT")?;

    loop {
        let events = fetch_tracing(client, config).await?;
        for event in events {
            let key = tracing_event_key(&event)?;
            let line = render_tracing_event(&event)?;
            if emitted.insert(key) {
                stdout.write_all(line.as_bytes())?;
                stdout.write_all(b"\n")?;
            }
        }
        stdout.flush()?;
        if wait_for_tracing_tick_or_interrupt(&mut signals).await? {
            stdout.write_all(b"interrupted\n")?;
            stdout.flush()?;
            return Ok(());
        }
    }
}

async fn wait_for_tracing_tick_or_interrupt(signals: &mut Signals) -> std::io::Result<bool> {
    future::or(
        async {
            async_io::Timer::after(LIVE_TRACING_POLL_INTERVAL).await;
            Ok(false)
        },
        async {
            match signals.next().await {
                Some(Ok(Signal::Int)) => Ok(true),
                Some(Ok(signal)) => {
                    panic!("unexpected signal while waiting for tracing cancellation: {signal:?}")
                }
                Some(Err(error)) => Err(error),
                None => panic!("signal stream ended while waiting for tracing cancellation"),
            }
        },
    )
    .await
}

/// How a rendered tracing event is coloured.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TracingPalette {
    /// ANSI colour, for a terminal.
    Terminal,
    /// No escape sequences, for a log file the inspector retains — escape
    /// sequences in an artifact are noise a reader has to strip back out.
    Plain,
}

impl TracingPalette {
    fn apply(self, style: AnsiStyle) -> AnsiStyle {
        match self {
            Self::Terminal => style,
            Self::Plain => AnsiStyle::new(),
        }
    }
}

pub fn render_tracing_event(event: &tracing::Event) -> Result<String> {
    render_tracing_event_with(event, TracingPalette::Terminal)
}

pub fn render_tracing_event_with(
    event: &tracing::Event,
    palette: TracingPalette,
) -> Result<String> {
    let mut text = String::new();
    write!(
        &mut text,
        "{}",
        palette
            .apply(level_style(event.level))
            .paint(level_name(event.level))
    )?;
    write!(
        &mut text,
        " {}",
        palette
            .apply(AnsiStyle::new().fg(Color::Fixed(244)))
            .paint(&event.target)
    )?;
    for field in &event.fields {
        if field.key == "message" {
            write!(&mut text, " ")?;
            write!(
                &mut text,
                "{}",
                palette
                    .apply(AnsiStyle::new().fg(Color::White))
                    .paint(render_value(&field.value)?)
            )?;
            continue;
        }
        write!(
            &mut text,
            " {}={}",
            palette
                .apply(AnsiStyle::new().fg(Color::Fixed(109)))
                .paint(&field.key),
            palette
                .apply(AnsiStyle::new().fg(Color::Fixed(252)))
                .paint(render_value(&field.value)?)
        )?;
    }
    Ok(text)
}

/// Captures the guest's tracing around a command that runs on the same RPC
/// client.
///
/// The guest keeps a bounded ring of recent events and `recent` always answers
/// with its tail, so the events a command produced are exactly the ones that
/// were not already in the ring when it started. One connection cannot poll
/// and run a command at the same time, so the capture primes its seen-set
/// before the command and drains after it; what comes back is the command's
/// own account, in order.
pub struct TracingCapture {
    config: TracingConfig,
    emitted: EmittedEvents,
}

impl TracingCapture {
    /// Primes the capture with whatever the ring already holds.
    pub async fn start(client: &mut RpcClient, config: TracingConfig) -> Result<Self> {
        let mut capture = Self {
            emitted: EmittedEvents::new(config.limit),
            config,
        };
        capture.take_new_events(client).await?;
        Ok(capture)
    }

    /// Returns the events that reached the ring since the last call, rendered
    /// without escape sequences.
    pub async fn drain(&mut self, client: &mut RpcClient) -> Result<Vec<String>> {
        let events = self.take_new_events(client).await?;
        events
            .iter()
            .map(|event| render_tracing_event_with(event, TracingPalette::Plain))
            .collect()
    }

    async fn take_new_events(&mut self, client: &mut RpcClient) -> Result<Vec<tracing::Event>> {
        let mut fresh = Vec::new();
        for event in fetch_tracing(client, &self.config).await? {
            if self.emitted.insert(tracing_event_key(&event)?) {
                fresh.push(event);
            }
        }
        Ok(fresh)
    }
}

fn tracing_event_key(event: &tracing::Event) -> Result<String> {
    let mut text = String::new();
    write!(
        &mut text,
        "{}|{}|{}",
        event.timestamp,
        level_name(event.level),
        event.target
    )?;
    for field in &event.fields {
        write!(&mut text, "|{}=", field.key)?;
        text.push_str(&render_value(&field.value)?);
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

fn level_style(level: tracing::Level) -> AnsiStyle {
    use tracing::Level;

    match level {
        Level::Error => AnsiStyle::new().fg(Color::Red).bold(),
        Level::Warn => AnsiStyle::new().fg(Color::Yellow).bold(),
        Level::Info => AnsiStyle::new().fg(Color::Green).bold(),
        Level::Debug => AnsiStyle::new().fg(Color::Blue).bold(),
        Level::Trace => AnsiStyle::new().fg(Color::Purple).bold(),
    }
}

fn render_value(value: &tracing::Value) -> Result<String> {
    use tracing::Value;

    let mut output = String::new();
    match value {
        Value::Boolean(value) => write!(output, "{value}")?,
        Value::Signed64(value) => write!(output, "{value}")?,
        Value::Unsigned64(value) => write!(output, "{value}")?,
        Value::Float64(value) => write!(output, "{value}")?,
        Value::Text(value) => write!(output, "{value}")?,
        Value::Blob(value) => write!(output, "{value:?}")?,
    }
    Ok(output)
}

struct EmittedEvents {
    capacity: usize,
    keys: VecDeque<String>,
    seen: HashSet<String>,
}

impl EmittedEvents {
    fn new(limit: u32) -> Self {
        let capacity = usize::try_from(limit.max(1))
            .expect("tracing limit does not fit into usize")
            .saturating_mul(4);
        Self {
            capacity,
            keys: VecDeque::with_capacity(capacity),
            seen: HashSet::with_capacity(capacity),
        }
    }

    fn insert(&mut self, key: String) -> bool {
        if !self.seen.insert(key.clone()) {
            return false;
        }

        self.keys.push_back(key);
        while self.keys.len() > self.capacity {
            let removed = self
                .keys
                .pop_front()
                .expect("emitted tracing event ring must not underflow");
            self.seen.remove(&removed);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use helios_inspector_protocol::system::tracing::{Event, Field, Level, Value};

    use super::{EmittedEvents, render_tracing_event, tracing_event_key};

    #[test]
    fn tracing_render_omits_timestamp_and_message_prefix() {
        let event = Event {
            timestamp: 8_059_000,
            level: Level::Info,
            target: "helios_kernel".to_owned(),
            fields: vec![
                Field {
                    key: "message".to_owned(),
                    value: Value::Text("Kernel initialized".to_owned()),
                },
                Field {
                    key: "processor".to_owned(),
                    value: Value::Unsigned64(1),
                },
            ],
        };

        let rendered = render_tracing_event(&event).expect("tracing render must succeed");

        assert!(!rendered.contains("[8059000]"));
        assert!(!rendered.contains("message="));
        assert!(rendered.contains("Kernel initialized"));
        assert!(rendered.contains("processor"));
    }

    #[test]
    fn tracing_dedup_key_keeps_distinct_timestamps() {
        let first = Event {
            timestamp: 1,
            level: Level::Info,
            target: "helios_kernel".to_owned(),
            fields: vec![Field {
                key: "message".to_owned(),
                value: Value::Text("tick".to_owned()),
            }],
        };
        let second = Event {
            timestamp: 2,
            level: Level::Info,
            target: "helios_kernel".to_owned(),
            fields: vec![Field {
                key: "message".to_owned(),
                value: Value::Text("tick".to_owned()),
            }],
        };
        let mut emitted = EmittedEvents::new(4);

        assert!(emitted.insert(tracing_event_key(&first).expect("key generation must succeed")));
        assert!(emitted.insert(tracing_event_key(&second).expect("key generation must succeed")));
    }
}
