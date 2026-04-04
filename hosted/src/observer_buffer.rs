use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant as StdInstant;

use strip_ansi_escapes::strip_str;
use tokio::sync::broadcast;

use crate::program_bindings::bindings;

const HISTORY_CAPACITY: usize = 512;

pub(crate) type SharedObserverBuffer = Arc<ObserverBuffer>;

pub(crate) struct ObserverBuffer {
    started_at: StdInstant,
    history: Mutex<VecDeque<bindings::helios::system::tracing::Event>>,
    tx: broadcast::Sender<bindings::helios::system::tracing::Event>,
}

impl ObserverBuffer {
    pub(crate) fn new(started_at: StdInstant) -> SharedObserverBuffer {
        let (tx, _) = broadcast::channel(HISTORY_CAPACITY);
        Arc::new(Self {
            started_at,
            history: Mutex::new(VecDeque::with_capacity(HISTORY_CAPACITY)),
            tx,
        })
    }

    pub(crate) fn record_console_text(&self, text: &str) {
        for raw_line in text.lines() {
            let line = strip_str(raw_line).trim().to_owned();
            if line.is_empty() {
                continue;
            }
            self.push_event(parse_console_line(self.now_nanos(), line));
        }
    }

    pub(crate) fn recent(
        &self,
        filter: &bindings::helios::system::tracing::Filter,
        limit: u32,
    ) -> Vec<bindings::helios::system::tracing::Event> {
        let history = self
            .history
            .lock()
            .unwrap_or_else(|_| panic!("observer history mutex poisoned"));
        let mut events = history
            .iter()
            .filter(|event| matches_filter(event, filter))
            .cloned()
            .collect::<Vec<_>>();
        let keep = limit as usize;
        if events.len() > keep {
            events.drain(..events.len() - keep);
        }
        events
    }

    pub(crate) fn subscribe(
        &self,
    ) -> broadcast::Receiver<bindings::helios::system::tracing::Event> {
        self.tx.subscribe()
    }

    fn now_nanos(&self) -> u64 {
        self.started_at.elapsed().as_nanos() as u64
    }

    fn push_event(&self, event: bindings::helios::system::tracing::Event) {
        {
            let mut history = self
                .history
                .lock()
                .unwrap_or_else(|_| panic!("observer history mutex poisoned"));
            if history.len() == HISTORY_CAPACITY {
                let dropped = history.pop_front();
                assert!(dropped.is_some(), "observer history underflowed");
            }
            history.push_back(event.clone());
        }
        let _ = self.tx.send(event);
    }
}

fn parse_console_line(timestamp: u64, line: String) -> bindings::helios::system::tracing::Event {
    if let Some((level, target, message)) = split_prefixed_line(&line) {
        return bindings::helios::system::tracing::Event {
            timestamp,
            level,
            target,
            fields: vec![bindings::helios::system::tracing::Field {
                key: "message".to_owned(),
                value: bindings::helios::system::tracing::Value::Text(message),
            }],
        };
    }

    bindings::helios::system::tracing::Event {
        timestamp,
        level: bindings::helios::system::tracing::Level::Info,
        target: "console".to_owned(),
        fields: vec![bindings::helios::system::tracing::Field {
            key: "message".to_owned(),
            value: bindings::helios::system::tracing::Value::Text(line),
        }],
    }
}

fn split_prefixed_line(
    line: &str,
) -> Option<(bindings::helios::system::tracing::Level, String, String)> {
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

fn parse_level(level: &str) -> Option<bindings::helios::system::tracing::Level> {
    Some(match level {
        "ERROR" => bindings::helios::system::tracing::Level::Error,
        "WARN" => bindings::helios::system::tracing::Level::Warn,
        "INFO" => bindings::helios::system::tracing::Level::Info,
        "DEBUG" => bindings::helios::system::tracing::Level::Debug,
        "TRACE" => bindings::helios::system::tracing::Level::Trace,
        _ => return None,
    })
}

pub(crate) fn matches_filter(
    event: &bindings::helios::system::tracing::Event,
    filter: &bindings::helios::system::tracing::Filter,
) -> bool {
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

fn level_priority(level: bindings::helios::system::tracing::Level) -> u8 {
    match level {
        bindings::helios::system::tracing::Level::Error => 0,
        bindings::helios::system::tracing::Level::Warn => 1,
        bindings::helios::system::tracing::Level::Info => 2,
        bindings::helios::system::tracing::Level::Debug => 3,
        bindings::helios::system::tracing::Level::Trace => 4,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant as StdInstant};

    use super::bindings::helios::system::tracing::{Event, Field, Filter, Level, Value};
    use super::{ObserverBuffer, matches_filter, parse_console_line};

    #[test]
    fn parses_kernel_console_prefixes() {
        let event = parse_console_line(42, "INFO [helios_kernel] Kernel initialized".to_owned());

        assert_eq!(event.timestamp, 42);
        assert_eq!(event.level, Level::Info);
        assert_eq!(event.target, "helios_kernel");
        assert_eq!(event.fields.len(), 1);
        assert_eq!(event.fields[0].key, "message");
        assert!(
            matches!(&event.fields[0].value, Value::Text(text) if text == "Kernel initialized")
        );
    }

    #[test]
    fn recent_applies_filter_and_limit() {
        let started_at = StdInstant::now() - Duration::from_secs(1);
        let buffer = ObserverBuffer::new(started_at);
        buffer.record_console_text("INFO [helios_kernel] one\nWARN [helios_shell] two\n");

        let filter = Filter {
            min_level: Some(Level::Warn),
            target_prefixes: vec!["helios_shell".to_owned()],
        };
        let events = buffer.recent(&filter, 8);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, Level::Warn);
        assert_eq!(events[0].target, "helios_shell");
    }

    #[test]
    fn filter_accepts_matching_event() {
        let event = Event {
            timestamp: 7,
            level: Level::Error,
            target: "helios_kernel".to_owned(),
            fields: vec![Field {
                key: "message".to_owned(),
                value: Value::Text("panic".to_owned()),
            }],
        };
        let filter = Filter {
            min_level: Some(Level::Warn),
            target_prefixes: vec!["helios_".to_owned()],
        };

        assert!(matches_filter(&event, &filter));
    }
}
