use async_signal::{Signal, Signals};
use futures_lite::{StreamExt as _, future};
use std::collections::{HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;

use helios_inspector_protocol::system::swap::{self, PolicyError, SwapPolicy};
use helios_inspector_protocol::system::{instances, stats, tracing};
use nu_ansi_term::{Color, Style as AnsiStyle};

use crate::remote::{self, RemoteError};
use crate::serial::RpcClient;
use crate::{SwapCommand, Switch, TracingCommand};

const LIVE_TRACING_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Why the inspector could not read or render what the guest reports.
///
/// The fetches and the rendering are separate variants because they fail
/// for unrelated reasons: a fetch fails at the guest, and a render fails
/// at this process's own formatter, which is a defect here rather than
/// there.
#[derive(Debug, thiserror::Error)]
pub enum SystemError {
    #[error("failed to fetch {what}: {source}")]
    Fetch {
        /// What was being read, spelled the way the message reads it.
        what: &'static str,
        #[source]
        source: RemoteError,
    },
    #[error("unknown tracing level {level}")]
    UnknownLevel { level: String },
    #[error("no live instance is named {name}")]
    NoInstanceNamed { name: String },
    #[error("the guest refused to stop instance {id}: {reason}")]
    KillRefused { id: u64, reason: &'static str },
    #[error(
        "the guest has no swap: its stats report no swap record or the kernel refused the policy as unavailable"
    )]
    NoSwap,
    #[error("the guest refused the swap policy: {error}")]
    PolicyRefused { error: swap::PolicyError },
    #[error("the guest refused to enable the tracing target {target}: {detail}")]
    TargetRefused { target: String, detail: String },
    #[error("failed to render a guest tracing event: {source}")]
    Render {
        #[from]
        source: core::fmt::Error,
    },
    #[error("failed to listen for SIGINT and SIGTERM: {source}")]
    Signals {
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write the guest tracing stream: {source}")]
    Write {
        #[source]
        source: std::io::Error,
    },
}

impl SystemError {
    /// The guest's panic report when a fetch failed because the guest
    /// kernel died.
    pub fn guest_panic(&self) -> Option<&str> {
        match self {
            Self::Fetch { source, .. } => source.guest_panic(),
            Self::UnknownLevel { .. }
            | Self::NoInstanceNamed { .. }
            | Self::KillRefused { .. }
            | Self::NoSwap
            | Self::PolicyRefused { .. }
            | Self::TargetRefused { .. }
            | Self::Render { .. }
            | Self::Signals { .. }
            | Self::Write { .. } => None,
        }
    }
}

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

pub async fn fetch_stats(client: &RpcClient) -> Result<stats::Sample, SystemError> {
    remote::call(stats::snapshot(client), "remote stats snapshot")
        .await
        .map_err(|source| SystemError::Fetch {
            what: "remote stats snapshot",
            source,
        })
}

pub async fn fetch_instances(client: &RpcClient) -> Result<Vec<instances::Instance>, SystemError> {
    remote::call(instances::snapshot(client), "remote instances snapshot")
        .await
        .map_err(|source| SystemError::Fetch {
            what: "remote instances snapshot",
            source,
        })
}

/// Prints the live instances, and stops one when asked to.
///
/// The listing is printed whichever way the call goes, because what an
/// operator wants to see after asking for a kill is the registry the
/// kill was aimed at.
pub async fn run_instances(
    client: &RpcClient,
    kill_id: Option<u64>,
    kill_name: Option<&str>,
) -> Result<(), SystemError> {
    let live = fetch_instances(client).await?;
    let target = match (kill_id, kill_name) {
        (Some(id), _) => Some(id),
        (None, Some(name)) => Some(named_instance(&live, name)?),
        (None, None) => None,
    };
    let mut out = std::io::stdout();
    for instance in &live {
        writeln!(
            out,
            "{:>6}  {:<24} up {:>8} ms  {:>9} KiB  {:>4}‰ cpu",
            instance.id,
            instance.name,
            instance.uptime / 1_000_000,
            instance.memory_bytes / 1024,
            instance.cpu_busy,
        )
        .map_err(|source| SystemError::Write { source })?;
    }
    let Some(id) = target else {
        return Ok(());
    };
    kill(client, id).await?;
    writeln!(out, "killed instance {id}").map_err(|source| SystemError::Write { source })?;
    Ok(())
}

impl SwapCommand {
    /// The policy that results from laying the given settings over
    /// `current`; a setting left out keeps its value.
    pub(crate) fn overlay(&self, current: SwapPolicy) -> SwapPolicy {
        SwapPolicy {
            // The guest counts in nanoseconds, the flag in milliseconds.
            idle_after: self
                .idle_after_ms
                .map_or(current.idle_after, |ms| ms * 1_000_000),
            store_limit_percent: self
                .store_limit_percent
                .unwrap_or(current.store_limit_percent),
            store_accept_percent: self
                .store_accept_percent
                .unwrap_or(current.store_accept_percent),
            accept_granules: self.accept_granules.unwrap_or(current.accept_granules),
            writeback: self
                .writeback
                .map_or(current.writeback, |switch| switch == Switch::On),
        }
    }

    /// Whether no setting was given, which asks only for the policy in
    /// force.
    pub(crate) fn is_query(&self) -> bool {
        self.idle_after_ms.is_none()
            && self.store_limit_percent.is_none()
            && self.store_accept_percent.is_none()
            && self.accept_granules.is_none()
            && self.writeback.is_none()
    }
}

/// The policy as the command prints it, one setting per line.
fn render_swap_policy(policy: &SwapPolicy) -> String {
    format!(
        "idle-after: {} ms\nstore-limit-percent: {}\nstore-accept-percent: {}\naccept-granules: {}\nwriteback: {}\n",
        policy.idle_after / 1_000_000,
        policy.store_limit_percent,
        policy.store_accept_percent,
        policy.accept_granules,
        if policy.writeback { "on" } else { "off" },
    )
}

/// The policy in force, from a stats sample.
async fn fetch_swap_policy(client: &RpcClient) -> Result<SwapPolicy, SystemError> {
    let sample = fetch_stats(client).await?;
    sample
        .swap
        .map(|swap| swap.policy)
        .ok_or(SystemError::NoSwap)
}

/// Prints the swap policy in force, after applying the settings given.
///
/// With none given nothing is sent: the command is a read.
pub async fn run_swap(client: &RpcClient, command: &SwapCommand) -> Result<(), SystemError> {
    let mut policy = fetch_swap_policy(client).await?;
    if !command.is_query() {
        let wanted = command.overlay(policy);
        let answer = remote::call(swap::set_policy(client, &wanted), "remote swap policy")
            .await
            .map_err(|source| SystemError::Fetch {
                what: "remote swap policy",
                source,
            })?;
        match answer {
            Ok(()) => {}
            Err(PolicyError::Unavailable) => return Err(SystemError::NoSwap),
            Err(error) => return Err(SystemError::PolicyRefused { error }),
        }
        policy = wanted;
    }
    std::io::stdout()
        .write_all(render_swap_policy(&policy).as_bytes())
        .map_err(|source| SystemError::Write { source })
}

/// The identifier the instance registered under `name` holds.
fn named_instance(live: &[instances::Instance], name: &str) -> Result<u64, SystemError> {
    live.iter()
        .find(|instance| instance.name == name)
        .map(|instance| instance.id)
        .ok_or_else(|| SystemError::NoInstanceNamed {
            name: name.to_owned(),
        })
}

/// Stops the instance registered under `name`, and reports which one it
/// was.
///
/// The kill is a flag the instance observes at its next yield point, so
/// this returns when the guest has been told, not when the instance is
/// gone. What proves it went is what happens next: the instance leaving a
/// later snapshot, or its supervisor's own line on the way back up.
pub async fn kill_named_instance(client: &RpcClient, name: &str) -> Result<u64, SystemError> {
    let id = named_instance(&fetch_instances(client).await?, name)?;
    kill(client, id).await?;
    Ok(id)
}

async fn kill(client: &RpcClient, id: u64) -> Result<(), SystemError> {
    let answer = remote::call(instances::kill(client, id), "remote instance kill")
        .await
        .map_err(|source| SystemError::Fetch {
            what: "remote instance kill",
            source,
        })?;
    match answer {
        Ok(()) => Ok(()),
        Err(instances::KillError::NoSuchInstance) => Err(SystemError::KillRefused {
            id,
            reason: "no live instance carries that identifier",
        }),
        Err(instances::KillError::AlreadyStopping) => Err(SystemError::KillRefused {
            id,
            reason: "it is already stopping",
        }),
    }
}

pub async fn fetch_tracing(
    client: &mut RpcClient,
    config: &TracingConfig,
) -> Result<Vec<tracing::Event>, SystemError> {
    remote::call(
        tracing::recent(client, &config.filter(), config.limit),
        "remote tracing events",
    )
    .await
    .map_err(|source| SystemError::Fetch {
        what: "remote tracing events",
        source,
    })
}

pub async fn run_tracing(
    mut client: RpcClient,
    command: &TracingCommand,
) -> Result<(), SystemError> {
    enable_tracing_targets(&client, &command.enable_target).await?;
    let config = tracing_config(
        command.limit,
        command.min_level.as_deref(),
        command.target_prefix.clone(),
    )?;
    stream_tracing(&mut client, &config).await
}

pub async fn stream_tracing_command(
    client: &mut RpcClient,
    command: &TracingCommand,
) -> Result<(), SystemError> {
    enable_tracing_targets(client, &command.enable_target).await?;
    let config = tracing_config(
        command.limit,
        command.min_level.as_deref(),
        command.target_prefix.clone(),
    )?;
    stream_tracing(client, &config).await
}

/// Turns each kernel diagnostic target in `targets` on for the rest of
/// the boot. The guest refuses a name no kernel module registered, and
/// that refusal is the error — a session that asked for a target it
/// misspelled fails here rather than streaming an empty log.
pub async fn enable_tracing_targets(
    client: &RpcClient,
    targets: &[String],
) -> Result<(), SystemError> {
    for target in targets {
        let refused = remote::call(
            tracing::set_target_enabled(client, target, true),
            "remote tracing target enablement",
        )
        .await
        .map_err(|source| SystemError::Fetch {
            what: "tracing target enablement",
            source,
        })?;
        refused.map_err(|error| SystemError::TargetRefused {
            target: target.clone(),
            detail: match error {
                tracing::TargetError::UnknownTarget(name) => name,
            },
        })?;
    }
    Ok(())
}

pub fn tracing_config(
    limit: u32,
    min_level: Option<&str>,
    target_prefixes: Vec<String>,
) -> Result<TracingConfig, SystemError> {
    let mut config = TracingConfig::new();
    config.limit = limit;
    config.min_level = match min_level {
        Some(level) => parse_level(level)?,
        None => config.min_level,
    };
    config.target_prefixes = target_prefixes;
    Ok(config)
}

pub fn parse_level(value: &str) -> Result<Option<tracing::Level>, SystemError> {
    use tracing::Level;

    let level = match value {
        "none" => return Ok(None),
        "error" => Level::Error,
        "warn" => Level::Warn,
        "info" => Level::Info,
        "debug" => Level::Debug,
        "trace" => Level::Trace,
        _ => {
            return Err(SystemError::UnknownLevel {
                level: value.to_owned(),
            });
        }
    };
    Ok(Some(level))
}

pub async fn stream_tracing(
    client: &mut RpcClient,
    config: &TracingConfig,
) -> Result<(), SystemError> {
    let mut stdout = std::io::stdout().lock();
    let mut emitted = EmittedEvents::new(config.limit);
    let mut signals =
        Signals::new([Signal::Int]).map_err(|source| SystemError::Signals { source })?;

    loop {
        let events = fetch_tracing(client, config).await?;
        for event in events {
            let key = tracing_event_key(&event)?;
            let line = render_tracing_event(&event)?;
            if emitted.insert(key) {
                write_line(&mut stdout, line.as_bytes())?;
                write_line(&mut stdout, b"\n")?;
            }
        }
        stdout
            .flush()
            .map_err(|source| SystemError::Write { source })?;
        if wait_for_tracing_tick_or_interrupt(&mut signals)
            .await
            .map_err(|source| SystemError::Signals { source })?
        {
            write_line(&mut stdout, b"interrupted\n")?;
            stdout
                .flush()
                .map_err(|source| SystemError::Write { source })?;
            return Ok(());
        }
    }
}

fn write_line(sink: &mut impl std::io::Write, bytes: &[u8]) -> Result<(), SystemError> {
    sink.write_all(bytes)
        .map_err(|source| SystemError::Write { source })
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

pub fn render_tracing_event(event: &tracing::Event) -> Result<String, SystemError> {
    render_tracing_event_with(event, TracingPalette::Terminal)
}

pub fn render_tracing_event_with(
    event: &tracing::Event,
    palette: TracingPalette,
) -> Result<String, SystemError> {
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
    pub async fn start(client: &mut RpcClient, config: TracingConfig) -> Result<Self, SystemError> {
        let mut capture = Self {
            emitted: EmittedEvents::new(config.limit),
            config,
        };
        capture.take_new_events(client).await?;
        Ok(capture)
    }

    /// Returns the events that reached the ring since the last call, rendered
    /// without escape sequences.
    pub async fn drain(&mut self, client: &mut RpcClient) -> Result<Vec<String>, SystemError> {
        let events = self.take_new_events(client).await?;
        events
            .iter()
            .map(|event| render_tracing_event_with(event, TracingPalette::Plain))
            .collect()
    }

    async fn take_new_events(
        &mut self,
        client: &mut RpcClient,
    ) -> Result<Vec<tracing::Event>, SystemError> {
        let mut fresh = Vec::new();
        for event in fetch_tracing(client, &self.config).await? {
            if self.emitted.insert(tracing_event_key(&event)?) {
                fresh.push(event);
            }
        }
        Ok(fresh)
    }
}

fn tracing_event_key(event: &tracing::Event) -> Result<String, SystemError> {
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

fn render_value(value: &tracing::Value) -> Result<String, SystemError> {
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

    use clap::Parser as _;
    use helios_inspector_protocol::system::swap::SwapPolicy;

    use super::{EmittedEvents, render_swap_policy, render_tracing_event, tracing_event_key};
    use crate::{SwapCommand, Switch};

    /// The command as its own command line parses it.
    #[derive(clap::Parser)]
    struct SwapLine {
        #[command(flatten)]
        command: SwapCommand,
    }

    fn swap_command(args: &[&str]) -> SwapCommand {
        SwapLine::try_parse_from(std::iter::once("swap").chain(args.iter().copied()))
            .expect("the swap flags parse")
            .command
    }

    fn policy_in_force() -> SwapPolicy {
        SwapPolicy {
            idle_after: 30_000_000_000,
            store_limit_percent: 25,
            store_accept_percent: 80,
            accept_granules: 48,
            writeback: true,
        }
    }

    #[test]
    fn swap_without_flags_is_a_read() {
        let command = swap_command(&[]);
        assert!(command.is_query());
        let current = policy_in_force();
        assert_eq!(
            render_swap_policy(&command.overlay(current)),
            render_swap_policy(&current)
        );
    }

    #[test]
    fn swap_overlay_changes_only_the_given_settings() {
        let command = swap_command(&["--idle-after-ms", "1500", "--writeback", "off"]);
        assert!(!command.is_query());
        let policy = command.overlay(policy_in_force());
        assert_eq!(policy.idle_after, 1_500_000_000);
        assert!(!policy.writeback);
        assert_eq!(policy.store_limit_percent, 25);
        assert_eq!(policy.store_accept_percent, 80);
        assert_eq!(policy.accept_granules, 48);
    }

    #[test]
    fn an_idle_delay_past_the_guests_nanosecond_clock_is_refused() {
        let longest = crate::MAX_IDLE_AFTER_MS;
        let command = swap_command(&["--idle-after-ms", &longest.to_string()]);
        assert_eq!(
            command.overlay(policy_in_force()).idle_after,
            longest * 1_000_000
        );
        let too_long = (longest + 1).to_string();
        assert!(
            SwapLine::try_parse_from(["swap", "--idle-after-ms", too_long.as_str()]).is_err(),
            "a delay past u64 nanoseconds must not parse"
        );
    }

    #[test]
    fn swap_overlay_takes_every_setting() {
        let command = swap_command(&[
            "--idle-after-ms",
            "0",
            "--store-limit-percent",
            "100",
            "--store-accept-percent",
            "0",
            "--accept-granules",
            "63",
            "--writeback",
            "on",
        ]);
        assert_eq!(command.writeback, Some(Switch::On));
        let policy = command.overlay(SwapPolicy {
            writeback: false,
            ..policy_in_force()
        });
        assert_eq!(
            render_swap_policy(&policy),
            "idle-after: 0 ms\nstore-limit-percent: 100\nstore-accept-percent: 0\naccept-granules: 63\nwriteback: on\n"
        );
    }

    #[test]
    fn swap_rejects_malformed_flags() {
        for args in [
            &["--writeback", "maybe"][..],
            &["--accept-granules", "300"],
            &["--idle-after-ms", "-1"],
        ] {
            assert!(
                SwapLine::try_parse_from(std::iter::once("swap").chain(args.iter().copied()))
                    .is_err(),
                "{args:?} must not parse"
            );
        }
    }

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
