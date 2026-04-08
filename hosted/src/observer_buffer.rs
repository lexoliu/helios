use std::sync::{Arc, Mutex};
use std::time::Instant as StdInstant;

use helios_kernel::{
    DEFAULT_TRACE_HISTORY_CAPACITY, TraceEvent, TraceFilter, TraceHistory, matches_trace_filter,
    parse_console_text,
};
use tokio::sync::broadcast;

pub(crate) type SharedObserverBuffer = Arc<ObserverBuffer>;

pub(crate) struct ObserverBuffer {
    started_at: StdInstant,
    history: Mutex<TraceHistory>,
    tx: broadcast::Sender<TraceEvent>,
}

impl ObserverBuffer {
    pub(crate) fn new(started_at: StdInstant) -> SharedObserverBuffer {
        let (tx, _) = broadcast::channel(DEFAULT_TRACE_HISTORY_CAPACITY);
        Arc::new(Self {
            started_at,
            history: Mutex::new(TraceHistory::new(DEFAULT_TRACE_HISTORY_CAPACITY)),
            tx,
        })
    }

    pub(crate) fn record_console_text(&self, text: &str) {
        let timestamp = self.now_nanos();
        let events = parse_console_text(timestamp, text);
        if events.is_empty() {
            return;
        }

        {
            let mut history = self
                .history
                .lock()
                .unwrap_or_else(|_| panic!("observer history mutex poisoned"));
            for event in &events {
                history.push(event.clone());
            }
        }

        for event in events {
            let _ = self.tx.send(event);
        }
    }

    pub(crate) fn recent(&self, filter: &TraceFilter, limit: u32) -> Vec<TraceEvent> {
        self.history
            .lock()
            .unwrap_or_else(|_| panic!("observer history mutex poisoned"))
            .recent(filter, limit)
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<TraceEvent> {
        self.tx.subscribe()
    }

    fn now_nanos(&self) -> u64 {
        self.started_at.elapsed().as_nanos() as u64
    }
}

pub(crate) fn matches_filter(event: &TraceEvent, filter: &TraceFilter) -> bool {
    matches_trace_filter(event, filter)
}
