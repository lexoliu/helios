extern crate alloc;

use alloc::collections::BinaryHeap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use core::task::{Context, Poll};
use core::time::Duration;

use atomic_waker::AtomicWaker;
use helios_hal::cpu::{Cpu, Instant};
use spinning_top::Spinlock;

#[derive(Clone)]
pub struct Timer<CpuImpl: Cpu + Clone> {
    cpu: CpuImpl,
    state: Arc<Spinlock<TimerState>>,
}

pub struct Sleep<CpuImpl: Cpu + Clone> {
    timer: Timer<CpuImpl>,
    state: Arc<SleepState>,
}

struct TimerState {
    next_id: u64,
    sleepers: BinaryHeap<TimerEntry>,
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
        Self {
            cpu,
            state: Arc::new(Spinlock::new(TimerState {
                next_id: 0,
                sleepers: BinaryHeap::new(),
            })),
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
        self.fire_expired_inner(false)
    }

    pub fn handle_interrupt(&self) -> usize {
        self.fire_expired_inner(true)
    }

    fn fire_expired_inner(&self, disarm_when_empty: bool) -> usize {
        let now = self.now();
        let mut ready = Vec::new();
        let mut queue_changed = false;
        let next_deadline = {
            critical_section::with(|_| {
                let mut state = self.state.lock();

                while let Some(entry) = state.sleepers.peek() {
                    if entry.state.cancelled.load(AtomicOrdering::Acquire)
                        || entry.state.fired.load(AtomicOrdering::Acquire)
                    {
                        queue_changed = true;
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
                    queue_changed = true;
                    ready.push(entry.state);
                }

                next_live_deadline(&mut state)
            })
        };

        for state in &ready {
            state.fire();
        }

        match next_deadline {
            Some(deadline) => self.cpu.set_deadline(deadline),
            None if disarm_when_empty => self.cpu.set_deadline(Instant::new(u64::MAX)),
            None if queue_changed => self.cpu.set_deadline(Instant::new(u64::MAX)),
            None => {}
        }

        ready.len()
    }

    fn enqueue(&self, state: Arc<SleepState>) {
        let deadline = state.deadline;
        let earliest_deadline = {
            critical_section::with(|_| {
                let mut timer_state = self.state.lock();
                let id = timer_state.next_id;
                timer_state.next_id = timer_state
                    .next_id
                    .checked_add(1)
                    .expect("timer entry id overflow");
                timer_state.sleepers.push(TimerEntry {
                    deadline,
                    id,
                    state,
                });

                next_live_deadline(&mut timer_state)
            })
        };

        if let Some(deadline) = earliest_deadline {
            self.cpu.set_deadline(deadline);
        }
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

fn duration_to_ticks(duration: Duration, frequency: u64) -> u64 {
    let seconds = (duration.as_secs() as u128) * (frequency as u128);
    let subsec = ((duration.subsec_nanos() as u128) * (frequency as u128)) / 1_000_000_000u128;
    let ticks = seconds
        .checked_add(subsec)
        .expect("timer duration overflows tick conversion");

    u64::try_from(ticks).expect("timer duration does not fit into u64 ticks")
}
