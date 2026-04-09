extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

pub const DEFAULT_TRACE_HISTORY_CAPACITY: usize = 512;

#[derive(Clone, Debug)]
pub struct TraceFilter {
    pub min_level: Option<TraceLevel>,
    pub target_prefixes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct TraceEvent {
    pub timestamp: u64,
    pub level: TraceLevel,
    pub target: String,
    pub fields: Vec<TraceField>,
}

#[derive(Clone, Debug)]
pub struct TraceField {
    pub key: String,
    pub value: TraceValue,
}

#[derive(Clone, Debug)]
pub enum TraceValue {
    Boolean(bool),
    Signed64(i64),
    Unsigned64(u64),
    Float64(f64),
    Text(String),
    Blob(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatsSample {
    pub timestamp: u64,
    pub uptime: u64,
    pub configured_processors: u32,
    pub online_processors: u32,
}

#[derive(Debug)]
pub struct TraceHistory {
    next_seq: u64,
    capacity: usize,
    events: VecDeque<(u64, TraceEvent)>,
}

impl TraceHistory {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity != 0, "trace history capacity must be non-zero");
        Self {
            next_seq: 1,
            capacity,
            events: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, event: TraceEvent) {
        if self.events.len() == self.capacity {
            let dropped = self.events.pop_front();
            assert!(dropped.is_some(), "trace history underflowed");
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.events.push_back((seq, event));
    }

    pub fn record_console_text(&mut self, timestamp: u64, text: &str) {
        for event in parse_console_text(timestamp, text) {
            self.push(event);
        }
    }

    pub fn recent(&self, filter: &TraceFilter, limit: u32) -> Vec<TraceEvent> {
        let mut events = self
            .events
            .iter()
            .map(|(_, event)| event)
            .filter(|event| matches_trace_filter(event, filter))
            .cloned()
            .collect::<Vec<_>>();
        let keep = limit as usize;
        if events.len() > keep {
            events.drain(..events.len() - keep);
        }
        events
    }

    pub fn next_after(&self, cursor: u64, filter: &TraceFilter) -> Option<(u64, TraceEvent)> {
        self.events
            .iter()
            .find(|(seq, event)| *seq > cursor && matches_trace_filter(event, filter))
            .map(|(seq, event)| (*seq, event.clone()))
    }
}

pub fn parse_console_text(timestamp: u64, text: &str) -> Vec<TraceEvent> {
    let stripped = strip_ansi(text);
    stripped
        .lines()
        .filter_map(|raw_line| {
            let line = raw_line.trim();
            (!line.is_empty()).then(|| parse_console_line(timestamp, line))
        })
        .collect()
}

pub fn matches_trace_filter(event: &TraceEvent, filter: &TraceFilter) -> bool {
    if let Some(min_level) = filter.min_level
        && level_priority(event.level) > level_priority(min_level)
    {
        return false;
    }

    if filter.target_prefixes.is_empty() {
        return true;
    }

    filter
        .target_prefixes
        .iter()
        .any(|prefix| event.target.starts_with(prefix))
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

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned;
    use alloc::vec;

    use super::{
        TraceEvent, TraceField, TraceFilter, TraceHistory, TraceLevel, TraceValue, matches_trace_filter,
        parse_console_text,
    };

    #[test]
    fn parses_kernel_console_prefixes() {
        let event = parse_console_text(42, "INFO [helios_kernel] Kernel initialized")
            .into_iter()
            .next()
            .expect("console line should produce one event");

        assert_eq!(event.timestamp, 42);
        assert_eq!(event.level, TraceLevel::Info);
        assert_eq!(event.target, "helios_kernel");
        assert_eq!(event.fields.len(), 1);
        assert_eq!(event.fields[0].key, "message");
        assert!(
            matches!(&event.fields[0].value, TraceValue::Text(text) if text == "Kernel initialized")
        );
    }

    #[test]
    fn trace_history_applies_filter_and_limit() {
        let mut history = TraceHistory::new(8);
        history.record_console_text(1, "INFO [helios_kernel] one\nWARN [helios_shell] two\n");

        let filter = TraceFilter {
            min_level: Some(TraceLevel::Warn),
            target_prefixes: vec!["helios_shell".to_owned()],
        };
        let events = history.recent(&filter, 8);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, TraceLevel::Warn);
        assert_eq!(events[0].target, "helios_shell");
    }

    #[test]
    fn filter_accepts_matching_event() {
        let event = TraceEvent {
            timestamp: 7,
            level: TraceLevel::Error,
            target: "helios_kernel".to_owned(),
            fields: vec![TraceField {
                key: "message".to_owned(),
                value: TraceValue::Text("panic".to_owned()),
            }],
        };
        let filter = TraceFilter {
            min_level: Some(TraceLevel::Warn),
            target_prefixes: vec!["helios_".to_owned()],
        };

        assert!(matches_trace_filter(&event, &filter));
    }
}
