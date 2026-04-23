extern crate alloc;

use alloc::collections::BinaryHeap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::cmp::Ordering;
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
    sleepers: BinaryHeap<TimerEntry>,
}

struct TimerShared {
    // This heap is owned by the kernel event loop running on the timer's home
    // processor. Interrupt handlers never touch it; they only re-arm the next
    // periodic/preemption tick so normal async/task context can finish the work.
    state: UnsafeCell<TimerState>,
    inbox: ConcurrentQueue<TimerEntry>,
    next_id: AtomicU64,
    next_sleep_deadline: AtomicU64,
    armed_deadline: AtomicU64,
    interrupt_interval_ticks: u64,
    ready_pool: Pool<Vec<Arc<SleepState>>>,
}

struct TimerEntry {
    deadline: Instant,
    id: u64,
    state: Arc<SleepState>,
}

struct SleepState {
    deadline: Instant,
    queued: AtomicBool,
    fired: AtomicBool,
    cancelled: AtomicBool,
    waker: AtomicWaker,
}

impl<CpuImpl: Cpu + Clone> Timer<CpuImpl> {
    pub fn new(cpu: CpuImpl) -> Self {
        let interrupt_interval_ticks =
            duration_to_ticks(SCHEDULER_INTERRUPT_INTERVAL, cpu.timer_frequency());
        assert!(
            interrupt_interval_ticks != 0,
            "scheduler interrupt interval {SCHEDULER_INTERRUPT_INTERVAL:?} converted to zero timer ticks"
        );
        let initial_deadline = cpu.now().saturating_add(interrupt_interval_ticks);
        cpu.set_deadline(initial_deadline);
        Self {
            cpu,
            shared: Arc::new(TimerShared {
                state: UnsafeCell::new(TimerState {
                    sleepers: BinaryHeap::new(),
                }),
                inbox: ConcurrentQueue::unbounded(),
                next_id: AtomicU64::new(0),
                next_sleep_deadline: AtomicU64::new(u64::MAX),
                armed_deadline: AtomicU64::new(initial_deadline.ticks()),
                interrupt_interval_ticks,
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
        // Timer wakeups are frequent, so keep the temporary ready list out of
        // the allocator fast path once the pool has warmed up.
        let mut ready = self.shared.ready_pool.get_owned();
        // SAFETY: the timer heap is owned by the kernel event loop on this
        // processor. Sleep futures only enqueue through the lock-free inbox, and
        // interrupt handlers only disarm the hardware timer.
        let state = unsafe { &mut *self.shared.state.get() };
        self.drain_inbox(state);

        while let Some(entry) = state.sleepers.peek() {
            if entry.state.cancelled.load(AtomicOrdering::Acquire)
                || entry.state.fired.load(AtomicOrdering::Acquire)
            {
                state.sleepers.pop();
                continue;
            }

            if entry.deadline > now {
                break;
            }

            let entry = state
                .sleepers
                .pop()
                .expect("timer heap peek succeeded but pop failed");
            ready.push(entry.state);
        }

        let next_deadline = next_live_deadline(state);

        for state in ready.iter() {
            state.fire();
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
        let id = self.shared.next_id.fetch_add(1, AtomicOrdering::AcqRel);
        let entry = TimerEntry {
            deadline,
            id,
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
                Ok(entry) => state.sleepers.push(entry),
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

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.id == other.id
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.id.cmp(&self.id))
    }
}

fn next_live_deadline(state: &mut TimerState) -> Option<Instant> {
    while let Some(entry) = state.sleepers.peek() {
        if entry.state.cancelled.load(AtomicOrdering::Acquire)
            || entry.state.fired.load(AtomicOrdering::Acquire)
        {
            state.sleepers.pop();
            continue;
        }

        return Some(entry.deadline);
    }

    None
}

unsafe impl Send for TimerShared {}
unsafe impl Sync for TimerShared {}
