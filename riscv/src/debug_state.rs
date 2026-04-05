extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use helios_kernel::{InstanceRegistry, Notify};
use spin::Mutex;

const HISTORY_CAPACITY: usize = 512;

#[derive(Clone)]
pub(crate) struct RuntimeState {
    inner: Arc<RuntimeStateInner>,
}

struct RuntimeStateInner {
    boot_ticks: u64,
    timebase_frequency: u64,
    processor_count: u32,
    instance_registry: InstanceRegistry,
    program_service: Mutex<Option<crate::program_host::UserProgramService>>,
    program_service_ready: Notify,
    network_service: Mutex<Option<crate::net::NetworkService>>,
    tracing: Mutex<TraceHistory>,
}

struct TraceHistory {
    next_seq: u64,
    events: VecDeque<(u64, TraceEvent)>,
}

#[derive(Clone)]
pub(crate) struct TraceFilter {
    pub min_level: Option<TraceLevel>,
    pub target_prefixes: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct TraceEvent {
    pub timestamp: u64,
    pub level: TraceLevel,
    pub target: String,
    pub fields: Vec<TraceField>,
}

#[derive(Clone)]
pub(crate) struct TraceField {
    pub key: String,
    pub value: TraceValue,
}

#[derive(Clone)]
pub(crate) enum TraceValue {
    Boolean(bool),
    Signed64(i64),
    Unsigned64(u64),
    Float64(f64),
    Text(String),
    Blob(Vec<u8>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TraceLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

pub(crate) struct StatsSample {
    pub timestamp: u64,
    pub uptime: u64,
    pub configured_processors: u32,
    pub online_processors: u32,
}

impl RuntimeState {
    pub(crate) fn new(timebase_frequency: u64, processor_count: usize, boot_ticks: u64) -> Self {
        Self {
            inner: Arc::new(RuntimeStateInner {
                boot_ticks,
                timebase_frequency,
                processor_count: processor_count as u32,
                instance_registry: InstanceRegistry::new(),
                program_service: Mutex::new(None),
                program_service_ready: Notify::new(),
                network_service: Mutex::new(None),
                tracing: Mutex::new(TraceHistory {
                    next_seq: 1,
                    events: VecDeque::with_capacity(HISTORY_CAPACITY),
                }),
            }),
        }
    }

    pub(crate) fn snapshot(&self, current_ticks: u64) -> StatsSample {
        let uptime = self.ticks_to_nanos(current_ticks.saturating_sub(self.inner.boot_ticks));
        StatsSample {
            timestamp: uptime,
            uptime,
            configured_processors: self.inner.processor_count,
            online_processors: self.inner.processor_count,
        }
    }

    pub(crate) fn record_console_text(&self, current_ticks: u64, text: &str) {
        let timestamp = self.ticks_to_nanos(current_ticks.saturating_sub(self.inner.boot_ticks));
        let stripped = strip_ansi(text);
        for raw_line in stripped.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            self.push_event(parse_console_line(timestamp, line));
        }
    }

    pub(crate) fn recent(&self, filter: &TraceFilter, limit: u32) -> Vec<TraceEvent> {
        let history = self.inner.tracing.lock();
        let mut events = history
            .events
            .iter()
            .map(|(_, event)| event)
            .filter(|event| matches_filter(event, filter))
            .cloned()
            .collect::<Vec<_>>();
        let keep = limit as usize;
        if events.len() > keep {
            events.drain(..events.len() - keep);
        }
        events
    }

    pub(crate) fn next_after(
        &self,
        cursor: u64,
        filter: &TraceFilter,
    ) -> Option<(u64, TraceEvent)> {
        let history = self.inner.tracing.lock();
        history
            .events
            .iter()
            .find(|(seq, event)| *seq > cursor && matches_filter(event, filter))
            .map(|(seq, event)| (*seq, event.clone()))
    }

    pub(crate) fn ticks_to_nanos(&self, ticks: u64) -> u64 {
        ticks.saturating_mul(1_000_000_000) / self.inner.timebase_frequency
    }

    pub(crate) fn uptime_nanos(&self, current_ticks: u64) -> u64 {
        self.ticks_to_nanos(current_ticks.saturating_sub(self.inner.boot_ticks))
    }

    pub(crate) fn instance_registry(&self) -> InstanceRegistry {
        self.inner.instance_registry.clone()
    }

    pub(crate) fn install_program_service(&self, service: crate::program_host::UserProgramService) {
        let mut slot = self.inner.program_service.lock();
        assert!(
            slot.is_none(),
            "program service was installed more than once"
        );
        *slot = Some(service);
        self.inner.program_service_ready.notify_all();
    }

    pub(crate) fn program_service(&self) -> Option<crate::program_host::UserProgramService> {
        self.inner.program_service.lock().clone()
    }

    pub(crate) async fn wait_for_program_service(&self) -> crate::program_host::UserProgramService {
        loop {
            if let Some(service) = self.program_service() {
                return service;
            }

            self.inner.program_service_ready.notified().await;
        }
    }

    pub(crate) fn install_network_service(&self, service: crate::net::NetworkService) {
        let mut slot = self.inner.network_service.lock();
        assert!(
            slot.is_none(),
            "network service was installed more than once"
        );
        *slot = Some(service);
    }

    pub(crate) fn network_service(&self) -> Option<crate::net::NetworkService> {
        self.inner.network_service.lock().clone()
    }
}

fn push_frontier(history: &mut TraceHistory, event: TraceEvent) {
    if history.events.len() == HISTORY_CAPACITY {
        let dropped = history.events.pop_front();
        assert!(dropped.is_some(), "trace history underflowed");
    }
    let seq = history.next_seq;
    history.next_seq = history.next_seq.saturating_add(1);
    history.events.push_back((seq, event));
}

impl RuntimeState {
    fn push_event(&self, event: TraceEvent) {
        let mut history = self.inner.tracing.lock();
        push_frontier(&mut history, event);
    }
}

fn parse_console_line(timestamp: u64, line: &str) -> TraceEvent {
    if let Some((level, target, message)) = split_prefixed_line(line) {
        return TraceEvent {
            timestamp,
            level,
            target,
            fields: vec![TraceField {
                key: "message".to_owned(),
                value: TraceValue::Text(message),
            }],
        };
    }

    TraceEvent {
        timestamp,
        level: TraceLevel::Info,
        target: "console".to_owned(),
        fields: vec![TraceField {
            key: "message".to_owned(),
            value: TraceValue::Text(line.to_owned()),
        }],
    }
}

fn split_prefixed_line(line: &str) -> Option<(TraceLevel, String, String)> {
    let open = line.find('[')?;
    let close = line[open..].find(']')? + open;
    let level = parse_level(line[..open].trim())?;
    let target = line[open + 1..close].trim();
    let message = line[close + 1..].trim();
    if target.is_empty() || message.is_empty() {
        return None;
    }
    Some((level, target.to_owned(), message.to_owned()))
}

fn parse_level(level: &str) -> Option<TraceLevel> {
    Some(match level {
        "ERROR" => TraceLevel::Error,
        "WARN" => TraceLevel::Warn,
        "INFO" => TraceLevel::Info,
        "DEBUG" => TraceLevel::Debug,
        "TRACE" => TraceLevel::Trace,
        _ => return None,
    })
}

fn matches_filter(event: &TraceEvent, filter: &TraceFilter) -> bool {
    if let Some(min_level) = filter.min_level {
        if level_priority(event.level) > level_priority(min_level) {
            return false;
        }
    }

    if filter.target_prefixes.is_empty() {
        return true;
    }

    filter
        .target_prefixes
        .iter()
        .any(|prefix| event.target.starts_with(prefix))
}

fn level_priority(level: TraceLevel) -> u8 {
    match level {
        TraceLevel::Error => 0,
        TraceLevel::Warn => 1,
        TraceLevel::Info => 2,
        TraceLevel::Debug => 3,
        TraceLevel::Trace => 4,
    }
}

fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index += 1;
            if index < bytes.len() && bytes[index] == b'[' {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if byte.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
        }

        output.push(bytes[index] as char);
        index += 1;
    }

    output
}
