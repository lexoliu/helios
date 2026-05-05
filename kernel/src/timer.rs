extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use core::task::{Context, Poll};
use core::time::Duration;

use atomic_waker::AtomicWaker;
use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use helios_hal::cpu::{Cpu, Instant};
use objectpool::Pool;

use crate::time::duration_to_ticks;

const SCHEDULER_INTERRUPT_INTERVAL: Duration = Duration::from_millis(100);
const TIMER_WHEEL_QUANTUM: Duration = Duration::from_micros(50);
const TIMER_WHEEL_LEVELS: usize = 4;
const TIMER_WHEEL_SLOTS: usize = 256;

#[derive(Clone)]
pub struct Timer<CpuImpl: Cpu + Clone> {
    cpu: CpuImpl,
    shared: Arc<TimerShared>,
}

pub struct Sleep<CpuImpl: Cpu + Clone> {
    timer: Timer<CpuImpl>,
    state: Arc<SleepState>,
}

struct TimerState {
    wheel: TimingWheel,
}

struct TimerShared {
    // This timing wheel is owned by the kernel event loop running on the
    // timer's home processor. Interrupt handlers never touch it; they only
    // re-arm the next periodic/preemption tick so normal async/task context
    // can finish the work.
    state: UnsafeCell<TimerState>,
    inbox: ConcurrentQueue<TimerEntry>,
    next_sleep_deadline: AtomicU64,
    armed_deadline: AtomicU64,
    interrupt_interval_ticks: u64,
    wheel_quantum_ticks: u64,
    ready_pool: Pool<Vec<TimerEntry>>,
}

struct TimerEntry {
    deadline: Instant,
    deadline_tick: u64,
    state: Arc<SleepState>,
}

struct SleepState {
    deadline: Instant,
    queued: AtomicBool,
    fired: AtomicBool,
    cancelled: AtomicBool,
    waker: AtomicWaker,
}

struct TimingWheel {
    levels: [WheelLevel; TIMER_WHEEL_LEVELS],
    current_tick: u64,
}

struct WheelLevel {
    buckets: Box<[Vec<TimerEntry>]>,
}

impl<CpuImpl: Cpu + Clone> Timer<CpuImpl> {
    pub fn new(cpu: CpuImpl) -> Self {
        let interrupt_interval_ticks =
            duration_to_ticks(SCHEDULER_INTERRUPT_INTERVAL, cpu.timer_frequency());
        let wheel_quantum_ticks = duration_to_ticks(TIMER_WHEEL_QUANTUM, cpu.timer_frequency());
        assert!(
            interrupt_interval_ticks != 0,
            "scheduler interrupt interval {SCHEDULER_INTERRUPT_INTERVAL:?} converted to zero timer ticks"
        );
        assert!(
            wheel_quantum_ticks != 0,
            "timer wheel quantum {TIMER_WHEEL_QUANTUM:?} converted to zero timer ticks"
        );
        let initial_deadline = cpu.now().saturating_add(interrupt_interval_ticks);
        cpu.set_deadline(initial_deadline);
        let current_tick = wheel_tick_floor(cpu.now(), wheel_quantum_ticks);
        Self {
            cpu,
            shared: Arc::new(TimerShared {
                state: UnsafeCell::new(TimerState {
                    wheel: TimingWheel::new(current_tick),
                }),
                inbox: ConcurrentQueue::unbounded(),
                next_sleep_deadline: AtomicU64::new(u64::MAX),
                armed_deadline: AtomicU64::new(initial_deadline.ticks()),
                interrupt_interval_ticks,
                wheel_quantum_ticks,
                ready_pool: Pool::bounded(8, Vec::new, Vec::clear),
            }),
        }
    }

    pub fn now(&self) -> Instant {
        self.cpu.now()
    }

    pub fn sleep_until(&self, deadline: Instant) -> Sleep<CpuImpl> {
        Sleep {
            timer: self.clone(),
            state: Arc::new(SleepState {
                deadline,
                queued: AtomicBool::new(false),
                fired: AtomicBool::new(false),
                cancelled: AtomicBool::new(false),
                waker: AtomicWaker::new(),
            }),
        }
    }

    pub fn sleep_for(&self, duration: Duration) -> Sleep<CpuImpl> {
        let ticks = duration_to_ticks(duration, self.cpu.timer_frequency());
        self.sleep_until(self.now().saturating_add(ticks))
    }

    pub fn fire_expired(&self) -> usize {
        let now = self.now();
        let mut ready = self.shared.ready_pool.get_owned();
        // SAFETY: the timing wheel is owned by the kernel event loop on this
        // processor. Sleep futures only enqueue through the lock-free inbox, and
        // interrupt handlers only disarm the hardware timer.
        let state = unsafe { &mut *self.shared.state.get() };
        self.drain_inbox(state);

        let now_tick = wheel_tick_floor(now, self.shared.wheel_quantum_ticks);
        state.wheel.drain_expired(now_tick, &mut ready);
        let next_deadline = state.wheel.next_live_deadline();

        for entry in ready.iter() {
            entry.state.fire();
        }

        self.commit_deadline(next_deadline, now);
        ready.len()
    }

    pub fn handle_interrupt(&self) -> usize {
        let next_deadline = self.next_interrupt_deadline_after(self.now());
        self.arm_deadline(next_deadline);
        0
    }

    fn enqueue(&self, state: Arc<SleepState>) {
        let deadline = state.deadline;
        let entry = TimerEntry {
            deadline,
            deadline_tick: wheel_tick_ceil(deadline, self.shared.wheel_quantum_ticks),
            state,
        };

        match self.shared.inbox.push(entry) {
            Ok(()) => self.publish_deadline(deadline),
            Err(PushError::Full(_)) => unreachable!("unbounded timer inbox reported full"),
            Err(PushError::Closed(_)) => panic!("timer inbox was closed unexpectedly"),
        }
    }

    fn drain_inbox(&self, state: &mut TimerState) {
        loop {
            match self.shared.inbox.pop() {
                Ok(entry) => state.wheel.insert(entry),
                Err(PopError::Empty | PopError::Closed) => return,
            }
        }
    }

    fn publish_deadline(&self, deadline: Instant) {
        let deadline = deadline.ticks();
        loop {
            let published = self
                .shared
                .next_sleep_deadline
                .load(AtomicOrdering::Acquire);
            if deadline >= published {
                return;
            }

            match self.shared.next_sleep_deadline.compare_exchange_weak(
                published,
                deadline,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }

        let candidate = self.next_interrupt_deadline_after(self.now());
        self.arm_deadline(candidate);
    }

    fn commit_deadline(&self, deadline: Option<Instant>, now: Instant) {
        let next_sleep_deadline = deadline.map_or(u64::MAX, |deadline| deadline.ticks());
        self.shared
            .next_sleep_deadline
            .store(next_sleep_deadline, AtomicOrdering::Release);
        let candidate = self.next_interrupt_deadline_after(now);
        self.arm_deadline(candidate);
    }

    fn next_interrupt_deadline_after(&self, now: Instant) -> Instant {
        let periodic_deadline = now.saturating_add(self.shared.interrupt_interval_ticks);
        let next_sleep_deadline = self
            .shared
            .next_sleep_deadline
            .load(AtomicOrdering::Acquire);

        if next_sleep_deadline <= now.ticks() {
            return periodic_deadline;
        }

        Instant::new(periodic_deadline.ticks().min(next_sleep_deadline))
    }

    fn arm_deadline(&self, deadline: Instant) {
        let deadline_ticks = deadline.ticks();
        let previous = self
            .shared
            .armed_deadline
            .swap(deadline_ticks, AtomicOrdering::AcqRel);
        if previous == deadline_ticks {
            return;
        }

        self.cpu.set_deadline(deadline);
    }
}

impl<CpuImpl: Cpu + Clone> Future for Sleep<CpuImpl> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_ref().get_ref();

        if this.state.fired.load(AtomicOrdering::Acquire) {
            return Poll::Ready(());
        }

        if this.state.deadline <= this.timer.now() {
            this.state.fire();
            return Poll::Ready(());
        }

        this.state.waker.register(cx.waker());

        if !this.state.queued.swap(true, AtomicOrdering::AcqRel) {
            this.timer.enqueue(this.state.clone());
        }

        if this.state.fired.load(AtomicOrdering::Acquire) {
            return Poll::Ready(());
        }

        Poll::Pending
    }
}

impl<CpuImpl: Cpu + Clone> Drop for Sleep<CpuImpl> {
    fn drop(&mut self) {
        self.state.cancelled.store(true, AtomicOrdering::Release);
    }
}

impl SleepState {
    fn fire(&self) {
        if self.fired.swap(true, AtomicOrdering::AcqRel) {
            return;
        }

        self.waker.wake();
    }
}

impl TimingWheel {
    fn new(current_tick: u64) -> Self {
        Self {
            levels: core::array::from_fn(|_| WheelLevel::new()),
            current_tick,
        }
    }

    fn insert(&mut self, entry: TimerEntry) {
        let target_tick = entry.deadline_tick.max(self.current_tick);
        let delay = target_tick.saturating_sub(self.current_tick);
        let level = wheel_level(delay);
        let slot = wheel_slot(target_tick, level);
        self.levels[level].buckets[slot].push(entry);
    }

    fn drain_expired(&mut self, now_tick: u64, ready: &mut Vec<TimerEntry>) {
        while self.current_tick < now_tick {
            self.current_tick = self.current_tick.saturating_add(1);
            self.cascade();
            self.drain_current_slot(ready);
        }
        self.drain_current_slot(ready);
    }

    fn next_live_deadline(&self) -> Option<Instant> {
        self.levels
            .iter()
            .flat_map(|level| level.buckets.iter())
            .flat_map(|bucket| bucket.iter())
            .filter(|entry| !entry.is_dead())
            .map(|entry| entry.deadline)
            .min()
    }

    fn cascade(&mut self) {
        for level in (1..TIMER_WHEEL_LEVELS).rev() {
            let lower_mask = (1_u64 << (level * 8)) - 1;
            if self.current_tick & lower_mask == 0 {
                self.cascade_level(level);
            }
        }
    }

    fn cascade_level(&mut self, level: usize) {
        let slot = wheel_slot(self.current_tick, level);
        let entries = core::mem::take(&mut self.levels[level].buckets[slot]);
        for entry in entries {
            self.insert(entry);
        }
    }

    fn drain_current_slot(&mut self, ready: &mut Vec<TimerEntry>) {
        let slot = wheel_slot(self.current_tick, 0);
        let entries = core::mem::take(&mut self.levels[0].buckets[slot]);
        for entry in entries {
            if entry.is_dead() {
                continue;
            }
            if entry.deadline_tick <= self.current_tick {
                ready.push(entry);
            } else {
                self.insert(entry);
            }
        }
    }
}

impl WheelLevel {
    fn new() -> Self {
        let buckets = (0..TIMER_WHEEL_SLOTS)
            .map(|_| Vec::new())
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { buckets }
    }
}

impl TimerEntry {
    fn is_dead(&self) -> bool {
        self.state.cancelled.load(AtomicOrdering::Acquire)
            || self.state.fired.load(AtomicOrdering::Acquire)
    }
}

fn wheel_level(delay: u64) -> usize {
    if delay < TIMER_WHEEL_SLOTS as u64 {
        0
    } else if delay < (TIMER_WHEEL_SLOTS as u64).pow(2) {
        1
    } else if delay < (TIMER_WHEEL_SLOTS as u64).pow(3) {
        2
    } else {
        3
    }
}

fn wheel_slot(tick: u64, level: usize) -> usize {
    ((tick >> (level * 8)) & (TIMER_WHEEL_SLOTS as u64 - 1)) as usize
}

fn wheel_tick_floor(deadline: Instant, quantum_ticks: u64) -> u64 {
    deadline.ticks() / quantum_ticks
}

fn wheel_tick_ceil(deadline: Instant, quantum_ticks: u64) -> u64 {
    deadline
        .ticks()
        .saturating_add(quantum_ticks.saturating_sub(1))
        / quantum_ticks
}

unsafe impl Send for TimerShared {}
unsafe impl Sync for TimerShared {}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::AtomicBool;

    use atomic_waker::AtomicWaker;
    use helios_hal::cpu::Instant;

    use super::{
        AtomicOrdering, SleepState, TimerEntry, TimingWheel, wheel_tick_ceil, wheel_tick_floor,
    };

    fn sleep_state(deadline: u64) -> Arc<SleepState> {
        Arc::new(SleepState {
            deadline: Instant::new(deadline),
            queued: AtomicBool::new(false),
            fired: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        })
    }

    fn timer_entry(deadline: u64, quantum_ticks: u64) -> TimerEntry {
        TimerEntry {
            deadline: Instant::new(deadline),
            deadline_tick: wheel_tick_ceil(Instant::new(deadline), quantum_ticks),
            state: sleep_state(deadline),
        }
    }

    #[test]
    fn wheel_tick_rounding_preserves_deadline_order() {
        assert_eq!(wheel_tick_floor(Instant::new(100), 50), 2);
        assert_eq!(wheel_tick_ceil(Instant::new(101), 50), 3);
        assert_eq!(wheel_tick_ceil(Instant::new(150), 50), 3);
    }

    #[test]
    fn timing_wheel_expires_entries_after_deadline_tick() {
        let mut wheel = TimingWheel::new(0);
        let entry = timer_entry(150, 50);
        let state = entry.state.clone();
        wheel.insert(entry);

        let mut ready = Vec::new();
        wheel.drain_expired(2, &mut ready);
        assert!(ready.is_empty());
        wheel.drain_expired(3, &mut ready);
        assert_eq!(ready.len(), 1);
        assert!(Arc::ptr_eq(&ready[0].state, &state));
    }

    #[test]
    fn timing_wheel_cascades_higher_level_entries() {
        let mut wheel = TimingWheel::new(0);
        let entry = timer_entry(300 * 50, 50);
        let state = entry.state.clone();
        wheel.insert(entry);

        let mut ready = Vec::new();
        wheel.drain_expired(255, &mut ready);
        assert!(ready.is_empty());
        wheel.drain_expired(300, &mut ready);
        assert_eq!(ready.len(), 1);
        assert!(Arc::ptr_eq(&ready[0].state, &state));
    }

    #[test]
    fn timing_wheel_skips_cancelled_entries() {
        let mut wheel = TimingWheel::new(0);
        let entry = timer_entry(100, 50);
        entry.state.cancelled.store(true, AtomicOrdering::Release);
        wheel.insert(entry);

        let mut ready = Vec::new();
        wheel.drain_expired(2, &mut ready);
        assert!(ready.is_empty());
    }
}
