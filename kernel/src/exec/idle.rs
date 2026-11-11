//! Per-processor idle state and the haltpoll handshake behind
//! [`Executor::park_until_work`](crate::exec::Executor::park_until_work).
//!
//! A processor whose run loop found nothing to do publishes
//! [`IdleState::Polling`] and spins on its ready queues and its timer
//! for a bounded, adaptive window before it parks; a wake that lands
//! inside the window costs neither an IPI nor a halt exit. A scheduler
//! that queues work for another processor sends `wake_processor` only
//! when that processor has published [`IdleState::Parked`] — a
//! `Polling` or `Running` one observes the ready count itself.
//!
//! Correctness: the parker stores `Parked` then loads the ready count;
//! the sender increments the ready count then loads the state; all
//! four are `SeqCst`. In the single total order, if the parker's load
//! saw zero it precedes the sender's increment, so the parker's store
//! precedes the sender's load and the sender sees `Parked` and sends
//! the IPI. If the sender saw `Polling` or `Running`, its increment
//! precedes the parker's store, so the parker's re-test sees the
//! count. No wake is lost. The backend's own `wake_pending` latch
//! (`x86/src/smp.rs`, `aarch64/src/lib.rs`) still covers the
//! interrupt-masked window inside `park_current`.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use core::time::Duration;

/// The longest an idle processor spends polling before it parks.
///
/// These are the Linux `cpuidle-haltpoll` defaults: the window starts
/// at zero — no polling until a short park has been observed — grows
/// from [`IDLE_POLL_GROW_START`] by [`IDLE_POLL_GROW`] per short park
/// up to this cap, and is divided by [`IDLE_POLL_SHRINK`] per park
/// that ran longer than the cap.
pub(crate) const IDLE_POLL_MAX: Duration = Duration::from_micros(200);

/// The poll window a processor earns from its first short park.
pub(crate) const IDLE_POLL_GROW_START: Duration = Duration::from_micros(50);

/// Per-short-park multiplier of the poll window.
pub(crate) const IDLE_POLL_GROW: u64 = 2;

/// Per-long-park divisor of the poll window.
pub(crate) const IDLE_POLL_SHRINK: u64 = 2;

/// What one processor's run loop is doing about work, published for
/// every other processor that might queue some for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum IdleState {
    /// In `run_until_stalled` or a task; drains its queues itself.
    Running = 0,
    /// Out of work, spinning on the ready counts before parking.
    Polling = 1,
    /// Inside `park_current`; only an interrupt or a wake IPI frees it.
    Parked = 2,
}

impl IdleState {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Running,
            1 => Self::Polling,
            2 => Self::Parked,
            _ => panic!("idle state {value} was never published"),
        }
    }
}

/// One processor's idle state, read by every processor that queues
/// work for it and written only by the owner. Lives in its own cache
/// line inside `ExecutorGroup` (`CachePadded`).
pub(crate) struct ProcessorIdle {
    state: AtomicU8,
    /// The owner's adaptive poll window in timer ticks; zero means
    /// "park without polling" and is the start state and the shrink
    /// floor.
    poll_limit_ticks: AtomicU64,
}

impl ProcessorIdle {
    pub(crate) const fn new() -> Self {
        Self {
            state: AtomicU8::new(IdleState::Running as u8),
            poll_limit_ticks: AtomicU64::new(0),
        }
    }

    /// The published state. `SeqCst`: this load is the sender half of
    /// the handshake in the module docs.
    pub(crate) fn state(&self) -> IdleState {
        IdleState::from_u8(self.state.load(Ordering::SeqCst))
    }

    /// Publishes a state transition. `SeqCst`: this store is the
    /// parker half of the handshake in the module docs. Owner-only.
    pub(crate) fn store(&self, state: IdleState) {
        self.state.store(state as u8, Ordering::SeqCst);
    }

    /// The owner's current poll window in timer ticks.
    pub(crate) fn poll_limit_ticks(&self) -> u64 {
        self.poll_limit_ticks.load(Ordering::Relaxed)
    }

    fn set_poll_limit_ticks(&self, limit: u64) {
        self.poll_limit_ticks.store(limit, Ordering::Relaxed);
    }

    /// The haltpoll adaptation, run by the owner after a park: a park
    /// no longer than the maximum window means polling would have
    /// caught the wake, so the window grows — from zero to
    /// `grow_start_ticks`, by `IDLE_POLL_GROW` otherwise — while a
    /// longer park halves it toward the zero "no polling" floor.
    pub(crate) fn adapt_poll_limit(
        &self,
        parked_ticks: u64,
        grow_start_ticks: u64,
        max_ticks: u64,
    ) {
        let limit = self.poll_limit_ticks();
        let limit = if parked_ticks <= max_ticks {
            if limit == 0 {
                grow_start_ticks
            } else {
                (limit * IDLE_POLL_GROW).min(max_ticks)
            }
        } else {
            limit / IDLE_POLL_SHRINK
        };
        self.set_poll_limit_ticks(limit);
    }
}

/// How `Executor::park_until_work` left the idle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleOutcome {
    /// Work arrived while polling; `polled_ticks` were spent polling.
    Polled { polled_ticks: u64 },
    /// The poll window expired and the processor parked;
    /// `polled_ticks` spent polling, `parked_ticks` spent in
    /// `park_current`.
    Parked {
        polled_ticks: u64,
        parked_ticks: u64,
    },
}
