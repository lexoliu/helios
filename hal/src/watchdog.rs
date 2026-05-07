use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use triomphe::Arc;

#[derive(Clone, Default)]
pub struct ProgressCounter {
    epoch: Arc<AtomicU64>,
}

impl ProgressCounter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_progress(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }

    pub fn snapshot(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
}

pub trait Watchdog: Clone + Send + Sync + 'static {
    fn progress(&self) -> ProgressCounter;

    fn is_enabled(&self) -> bool;

    fn timeout(&self) -> Duration;

    fn arm(&self);

    fn pet(&self);

    fn disarm(&self);
}

#[derive(Clone, Copy, Default)]
pub struct NoWatchdog;

impl Watchdog for NoWatchdog {
    fn progress(&self) -> ProgressCounter {
        ProgressCounter::new()
    }

    fn is_enabled(&self) -> bool {
        false
    }

    fn timeout(&self) -> Duration {
        Duration::ZERO
    }

    fn arm(&self) {}

    fn pet(&self) {}

    fn disarm(&self) {}
}
