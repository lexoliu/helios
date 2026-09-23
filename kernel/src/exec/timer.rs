extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering as AtomicOrdering, fence};
use core::task::{Context, Poll};
use core::time::Duration;

use atomic_waker::AtomicWaker;
use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use helios_hal::cpu::{Cpu, Instant};
use objectpool::Pool;
use triomphe::Arc;

use crate::exec::time::duration_to_ticks;

const SCHEDULER_INTERRUPT_INTERVAL: Duration = Duration::from_millis(100);
const TIMER_WHEEL_QUANTUM: Duration = Duration::from_micros(50);
const TIMER_WHEEL_LEVELS: usize = 4;
const TIMER_WHEEL_SLOTS: usize = 256;
const TIMER_INBOX_CAPACITY: usize = TIMER_WHEEL_SLOTS * 16;
const TIMER_READY_POOL_SLOTS: usize = 8;
const TIMER_READY_RETAINED_ENTRIES: usize = TIMER_WHEEL_SLOTS;
/// Slots reserved for the per-Timer SleepState pool. A slot is held
/// exactly as long as its `Sleep` future is alive — the wheel holds
/// generation-validated weak handles and never pins a slot — so this
/// bounds the number of concurrently live pending sleeps on one timer,
/// matching the `TIMER_INBOX_CAPACITY` capacity contract.
const SLEEP_STATE_POOL_SLOTS: usize = TIMER_INBOX_CAPACITY;

#[derive(Clone)]
pub struct Timer<CpuImpl: Cpu + Clone> {
    cpu: CpuImpl,
    shared: Arc<TimerShared>,
}

pub struct Sleep<CpuImpl: Cpu + Clone> {
    timer: Timer<CpuImpl>,
    deadline: Option<Instant>,
    state: Option<SleepRef>,
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
    sleep_pool: SleepStatePool,
}

struct TimerEntry {
    deadline: Instant,
    deadline_tick: u64,
    state: SleepHandle,
}

struct SleepState {
    queued: AtomicBool,
    fired: AtomicBool,
    waker: AtomicWaker,
}

/// Bounded slab pool that hands out reference-counted handles to a
/// fixed array of `SleepState` cells. Slots are pinned only by live
/// `Sleep` futures, so exhaustion means more concurrently pending
/// sleeps than the kernel capacity contract allows and fails
/// immediately instead of reintroducing per-Sleep heap allocation.
struct SleepStatePool {
    slots: Box<[SleepStateSlot]>,
    free_indices: ConcurrentQueue<u32>,
}

#[repr(C)]
struct SleepStateSlot {
    /// Live ref count when occupied. Zero means the slot is free
    /// and the cell is uninitialised; readers must consult the
    /// freelist before touching the cell.
    refcount: AtomicU32,
    /// Bumped on every final release, before the slot returns to the
    /// freelist. Weak handles snapshot it and validate against it so a
    /// recycled slot can never satisfy a stale wheel entry.
    generation: AtomicU64,
    /// Slot storage. Initialised between `acquire` and the final
    /// `Drop` of every `SleepRef` referencing this slot.
    cell: UnsafeCell<MaybeUninit<SleepState>>,
}

// SAFETY: the pool synchronises slot ownership through `refcount`
// (Acquire/Release for the cell publication) and through the
// `ConcurrentQueue<u32>` freelist (which provides its own
// happens-before for the slot index itself). Writes to `cell` only
// happen with exclusive ownership: either the acquiring thread
// before publishing the slot, or the releasing thread after
// observing refcount==0.
unsafe impl Send for SleepStateSlot {}
unsafe impl Sync for SleepStateSlot {}

/// Pool-backed strong handle. The `shared` Arc keeps the pool alive
/// for the lifetime of every clone of this handle; the slot is
/// reclaimed once the last ref drops.
struct SleepRef {
    shared: Arc<TimerShared>,
    idx: u32,
}

/// Weak, generation-validated handle held by timer inbox and wheel
/// entries. It pins nothing: the slot is reclaimed the moment the
/// owning `Sleep` drops, and a stale handle just fails validation.
/// All accesses go through the pool the handle was minted from.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SleepHandle {
    idx: u32,
    generation: u64,
}

struct TimingWheel {
    levels: [WheelLevel; TIMER_WHEEL_LEVELS],
    current_tick: u64,
    next_deadline: Option<Instant>,
    next_deadline_tick: Option<u64>,
    next_deadline_dirty: bool,
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
                inbox: ConcurrentQueue::bounded(TIMER_INBOX_CAPACITY),
                next_sleep_deadline: AtomicU64::new(u64::MAX),
                armed_deadline: AtomicU64::new(initial_deadline.ticks()),
                interrupt_interval_ticks,
                wheel_quantum_ticks,
                ready_pool: Pool::bounded(
                    TIMER_READY_POOL_SLOTS,
                    Vec::new,
                    reset_timer_ready_entries,
                ),
                sleep_pool: SleepStatePool::new(SLEEP_STATE_POOL_SLOTS),
            }),
        }
    }

    pub fn now(&self) -> Instant {
        self.cpu.now()
    }

    pub fn sleep_until(&self, deadline: Instant) -> Sleep<CpuImpl> {
        if deadline <= self.now() {
            return Sleep {
                timer: self.clone(),
                deadline: None,
                state: None,
            };
        }

        Sleep {
            timer: self.clone(),
            deadline: Some(deadline),
            state: None,
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
        let pool = &self.shared.sleep_pool;
        self.drain_inbox(state);

        let now_tick = wheel_tick_floor(now, self.shared.wheel_quantum_ticks);
        state.wheel.drain_expired(pool, now_tick, &mut ready);
        let next_deadline = state.wheel.next_live_deadline(pool);

        for entry in ready.iter() {
            entry.state.fire(pool);
        }

        self.commit_deadline(next_deadline, now);
        ready.len()
    }

    pub fn handle_interrupt(&self) -> usize {
        let next_deadline = self.next_interrupt_deadline_after(self.now());
        self.arm_deadline(next_deadline);
        0
    }

    fn enqueue(&self, deadline: Instant, state: SleepHandle) {
        let entry = TimerEntry {
            deadline,
            deadline_tick: wheel_tick_ceil(deadline, self.shared.wheel_quantum_ticks),
            state,
        };

        match self.shared.inbox.push(entry) {
            Ok(()) => self.publish_deadline(deadline),
            Err(PushError::Full(_)) => {
                panic!("timer inbox capacity {TIMER_INBOX_CAPACITY} exhausted")
            }
            Err(PushError::Closed(_)) => panic!("timer inbox was closed unexpectedly"),
        }
    }

    fn drain_inbox(&self, state: &mut TimerState) {
        loop {
            match self.shared.inbox.pop() {
                Ok(entry) => state.wheel.insert(&self.shared.sleep_pool, entry),
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
        let this = self.get_mut();
        let Some(deadline) = this.deadline else {
            return Poll::Ready(());
        };
        if deadline <= this.timer.now() {
            this.deadline = None;
            if let Some(state) = this.state.as_ref() {
                state.fire();
            }
            return Poll::Ready(());
        }

        if this.state.is_none() {
            this.state = Some(SleepRef::new(&this.timer.shared));
        }
        let state = this
            .state
            .as_ref()
            .expect("sleep state must exist after first pending poll");

        if state.fired.load(AtomicOrdering::Acquire) {
            return Poll::Ready(());
        }

        state.waker.register(cx.waker());

        if !state.queued.swap(true, AtomicOrdering::AcqRel) {
            this.timer.enqueue(deadline, state.downgrade());
        }

        if state.fired.load(AtomicOrdering::Acquire) {
            return Poll::Ready(());
        }

        Poll::Pending
    }
}

impl<CpuImpl: Cpu + Clone> Unpin for Sleep<CpuImpl> {}

impl SleepState {
    fn new() -> Self {
        Self {
            queued: AtomicBool::new(false),
            fired: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        }
    }

    fn fire(&self) {
        if self.fired.swap(true, AtomicOrdering::AcqRel) {
            return;
        }

        self.waker.wake();
    }
}

impl SleepStatePool {
    fn new(capacity: usize) -> Self {
        let slots: Box<[SleepStateSlot]> = (0..capacity)
            .map(|_| SleepStateSlot {
                refcount: AtomicU32::new(0),
                generation: AtomicU64::new(0),
                cell: UnsafeCell::new(MaybeUninit::uninit()),
            })
            .collect();
        let free_indices = ConcurrentQueue::bounded(capacity);
        for index in 0..capacity {
            free_indices
                .push(index as u32)
                .unwrap_or_else(|_| panic!("sleep state pool freelist overrun"));
        }
        Self {
            slots,
            free_indices,
        }
    }

    /// Acquires a fresh slot from the pool, initialising it with
    /// `state` and a refcount of 1.
    fn acquire(&self, state: SleepState) -> u32 {
        let idx = self
            .free_indices
            .pop()
            .unwrap_or_else(|_| panic!("sleep state pool capacity exhausted"));
        let slot = &self.slots[idx as usize];
        debug_assert_eq!(
            slot.refcount.load(AtomicOrdering::Relaxed),
            0,
            "sleep slot popped from freelist must be free"
        );
        // SAFETY: the freelist had exclusive ownership of `idx` until
        // the pop above; nothing else can observe `cell` until we
        // publish the refcount below.
        unsafe {
            (*slot.cell.get()).write(state);
        }
        // Release ordering pairs with the Acquire load on `slot()`
        // so cell initialisation happens-before any clone reads it.
        slot.refcount.store(1, AtomicOrdering::Release);
        idx
    }

    fn slot(&self, idx: u32) -> &SleepStateSlot {
        &self.slots[idx as usize]
    }

    /// Drops one reference to `idx`, tearing down and recycling the
    /// slot when it was the last.
    fn release(&self, idx: u32) {
        let slot = self.slot(idx);
        // AcqRel: pair Acquire on the last-drop branch with all
        // Release stores on prior clones so we observe the most
        // recent state writes before tearing down the cell.
        let prev = slot.refcount.fetch_sub(1, AtomicOrdering::AcqRel);
        if prev != 1 {
            return;
        }
        // Last reference. Synchronise with any concurrent pre-drop
        // reads from other threads.
        fence(AtomicOrdering::Acquire);
        // SAFETY: we are the last observer of the slot; the refcount
        // is now zero, no other thread can touch the cell until
        // acquire republishes it. Drop-in-place releases the
        // SleepState's `AtomicWaker` registration and any other
        // resources before the slot is recycled.
        unsafe {
            (*slot.cell.get()).assume_init_drop();
        }
        // Invalidate outstanding weak handles before the index becomes
        // acquirable again.
        slot.generation.fetch_add(1, AtomicOrdering::Release);
        self.free_indices
            .push(idx)
            .unwrap_or_else(|_| panic!("sleep state pool freelist overflow"));
    }
}

impl SleepRef {
    fn new(shared: &Arc<TimerShared>) -> Self {
        let idx = shared.sleep_pool.acquire(SleepState::new());
        Self {
            shared: shared.clone(),
            idx,
        }
    }

    fn state(&self) -> &SleepState {
        let slot = self.shared.sleep_pool.slot(self.idx);
        debug_assert!(
            slot.refcount.load(AtomicOrdering::Relaxed) > 0,
            "sleep slot dereferenced after final drop"
        );
        // SAFETY: the live refcount above guarantees the cell
        // is still initialised, and Release on the publishing
        // store paired with our Acquire on clone provides the
        // happens-before for the contents.
        unsafe { (*slot.cell.get()).assume_init_ref() }
    }

    /// Weak handle for the timer inbox and wheel. Snapshotting the
    /// generation under a live strong ref is race-free: it only moves
    /// when the refcount reaches zero.
    fn downgrade(&self) -> SleepHandle {
        let slot = self.shared.sleep_pool.slot(self.idx);
        SleepHandle {
            idx: self.idx,
            generation: slot.generation.load(AtomicOrdering::Relaxed),
        }
    }
}

impl SleepHandle {
    /// True once the owning `Sleep` released the slot (or the slot has
    /// been recycled for a newer sleeper). Only touches the slot
    /// header, never the cell, so it is safe on stale handles.
    fn is_dead(&self, pool: &SleepStatePool) -> bool {
        let slot = pool.slot(self.idx);
        slot.refcount.load(AtomicOrdering::Acquire) == 0
            || slot.generation.load(AtomicOrdering::Acquire) != self.generation
    }

    /// Wakes the sleeper if this handle is still current. Takes a
    /// temporary strong reference so the cell cannot be torn down
    /// mid-wake by a concurrent `Sleep` drop on another processor.
    fn fire(&self, pool: &SleepStatePool) {
        let slot = pool.slot(self.idx);
        let mut refcount = slot.refcount.load(AtomicOrdering::Acquire);
        loop {
            if refcount == 0 {
                return;
            }
            match slot.refcount.compare_exchange_weak(
                refcount,
                refcount + 1,
                AtomicOrdering::Acquire,
                AtomicOrdering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => refcount = current,
            }
        }
        if slot.generation.load(AtomicOrdering::Acquire) == self.generation {
            // SAFETY: the reference taken above keeps the cell
            // initialised, and the matching generation proves it still
            // holds this handle's sleeper.
            unsafe { (*slot.cell.get()).assume_init_ref() }.fire();
        }
        pool.release(self.idx);
    }
}

impl Deref for SleepRef {
    type Target = SleepState;

    fn deref(&self) -> &SleepState {
        self.state()
    }
}

impl Drop for SleepRef {
    fn drop(&mut self) {
        self.shared.sleep_pool.release(self.idx);
    }
}

impl TimingWheel {
    fn new(current_tick: u64) -> Self {
        Self {
            levels: core::array::from_fn(|_| WheelLevel::new()),
            current_tick,
            next_deadline: None,
            next_deadline_tick: None,
            next_deadline_dirty: false,
        }
    }

    fn insert(&mut self, pool: &SleepStatePool, entry: TimerEntry) {
        // A sleep dropped between enqueue and drain arrives already dead;
        // dropping it here keeps wheel buckets bounded by live sleeps.
        if entry.is_dead(pool) {
            return;
        }
        self.observe_deadline(entry.deadline, entry.deadline_tick);
        let target_tick = entry.deadline_tick.max(self.current_tick);
        let delay = target_tick.saturating_sub(self.current_tick);
        let level = wheel_level(delay);
        let slot = wheel_slot(target_tick, level);
        self.levels[level].buckets[slot].push(entry);
    }

    fn drain_expired(&mut self, pool: &SleepStatePool, now_tick: u64, ready: &mut Vec<TimerEntry>) {
        while self.current_tick < now_tick {
            self.current_tick = self.current_tick.saturating_add(1);
            self.cascade(pool);
            self.drain_current_slot(pool, ready);
        }
        self.drain_current_slot(pool, ready);
    }

    fn next_live_deadline(&mut self, pool: &SleepStatePool) -> Option<Instant> {
        if self.next_deadline_dirty
            || self
                .next_deadline_tick
                .is_some_and(|deadline_tick| deadline_tick <= self.current_tick)
        {
            self.rebuild_next_deadline(pool);
        }
        self.next_deadline
    }

    fn observe_deadline(&mut self, deadline: Instant, deadline_tick: u64) {
        if self
            .next_deadline
            .is_none_or(|current_deadline| deadline < current_deadline)
        {
            self.next_deadline = Some(deadline);
            self.next_deadline_tick = Some(deadline_tick);
        }
    }

    fn rebuild_next_deadline(&mut self, pool: &SleepStatePool) {
        let next = self
            .levels
            .iter()
            .flat_map(|level| level.buckets.iter())
            .flat_map(|bucket| bucket.iter())
            .filter(|entry| !entry.is_dead(pool))
            .min_by_key(|entry| entry.deadline);
        self.next_deadline = next.map(|entry| entry.deadline);
        self.next_deadline_tick = next.map(|entry| entry.deadline_tick);
        self.next_deadline_dirty = false;
    }

    fn cascade(&mut self, pool: &SleepStatePool) {
        for level in (1..TIMER_WHEEL_LEVELS).rev() {
            let lower_mask = (1_u64 << (level * 8)) - 1;
            if self.current_tick & lower_mask == 0 {
                self.cascade_level(pool, level);
            }
        }
    }

    fn cascade_level(&mut self, pool: &SleepStatePool, level: usize) {
        let slot = wheel_slot(self.current_tick, level);
        while let Some(entry) = self.levels[level].buckets[slot].pop() {
            self.next_deadline_dirty = true;
            if entry.is_dead(pool) {
                continue;
            }
            self.insert(pool, entry);
        }
    }

    fn drain_current_slot(&mut self, pool: &SleepStatePool, ready: &mut Vec<TimerEntry>) {
        let slot = wheel_slot(self.current_tick, 0);
        while let Some(entry) = self.levels[0].buckets[slot].pop() {
            self.next_deadline_dirty = true;
            if entry.is_dead(pool) {
                continue;
            }
            if entry.deadline_tick <= self.current_tick {
                ready.push(entry);
            } else {
                self.insert(pool, entry);
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
    fn is_dead(&self, pool: &SleepStatePool) -> bool {
        self.state.is_dead(pool)
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

fn reset_timer_ready_entries(ready: &mut Vec<TimerEntry>) {
    if ready.capacity() > TIMER_READY_RETAINED_ENTRIES {
        *ready = Vec::with_capacity(TIMER_READY_RETAINED_ENTRIES);
    } else {
        ready.clear();
    }
}

unsafe impl Send for TimerShared {}
unsafe impl Sync for TimerShared {}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::future::Future as _;
    use core::pin::Pin;
    use core::task::Context;

    use futures::task::noop_waker_ref;
    use helios_hal::cpu::{Cpu, Instant, ProcessorId};

    use super::{
        TIMER_INBOX_CAPACITY, TIMER_READY_RETAINED_ENTRIES, TimerEntry, TimingWheel,
        reset_timer_ready_entries, wheel_tick_ceil, wheel_tick_floor,
    };

    #[derive(Clone)]
    struct TestCpu {
        now: u64,
    }

    impl Cpu for TestCpu {
        fn current_processor(&self) -> ProcessorId {
            ProcessorId::new(0)
        }

        fn processor_count(&self) -> usize {
            1
        }

        fn bootstrap_processor(&self) -> ProcessorId {
            ProcessorId::new(0)
        }

        fn park_current(&self) {}

        fn start_processor(&self, _processor: ProcessorId) {}

        fn wake_processor(&self, _processor: ProcessorId) {}

        fn now(&self) -> Instant {
            Instant::new(self.now)
        }

        fn timer_frequency(&self) -> u64 {
            1_000_000
        }

        fn set_deadline(&self, _deadline: Instant) {}

        fn publish_executable(&self, _ptr: *const u8, _len: usize) {}

        fn unpublish_executable(&self, _ptr: *const u8, _len: usize) {}

        fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
            None
        }

        fn shutdown(&self) -> ! {
            panic!("test CPU cannot shut down")
        }

        fn reboot(&self) -> ! {
            panic!("test CPU cannot reboot")
        }
    }

    fn test_timer() -> super::Timer<TestCpu> {
        super::Timer::new(TestCpu { now: 0 })
    }

    /// Builds a wheel entry plus the strong ref that keeps its slot
    /// live; dropping the ref cancels the sleep.
    fn timer_entry(
        timer: &super::Timer<TestCpu>,
        deadline: u64,
        quantum_ticks: u64,
    ) -> (super::SleepRef, TimerEntry) {
        let state = super::SleepRef::new(&timer.shared);
        let entry = TimerEntry {
            deadline: Instant::new(deadline),
            deadline_tick: wheel_tick_ceil(Instant::new(deadline), quantum_ticks),
            state: state.downgrade(),
        };
        (state, entry)
    }

    #[test]
    fn wheel_tick_rounding_preserves_deadline_order() {
        assert_eq!(wheel_tick_floor(Instant::new(100), 50), 2);
        assert_eq!(wheel_tick_ceil(Instant::new(101), 50), 3);
        assert_eq!(wheel_tick_ceil(Instant::new(150), 50), 3);
    }

    #[test]
    fn timing_wheel_expires_entries_after_deadline_tick() {
        let timer = test_timer();
        let pool = &timer.shared.sleep_pool;
        let mut wheel = TimingWheel::new(0);
        let (state, entry) = timer_entry(&timer, 150, 50);
        let handle = entry.state;
        wheel.insert(pool, entry);

        let mut ready = Vec::new();
        wheel.drain_expired(pool, 2, &mut ready);
        assert!(ready.is_empty());
        wheel.drain_expired(pool, 3, &mut ready);
        assert_eq!(ready.len(), 1);
        assert!(ready[0].state == handle);
        drop(state);
    }

    #[test]
    fn timing_wheel_cascades_higher_level_entries() {
        let timer = test_timer();
        let pool = &timer.shared.sleep_pool;
        let mut wheel = TimingWheel::new(0);
        let (state, entry) = timer_entry(&timer, 300 * 50, 50);
        let handle = entry.state;
        wheel.insert(pool, entry);

        let mut ready = Vec::new();
        wheel.drain_expired(pool, 255, &mut ready);
        assert!(ready.is_empty());
        wheel.drain_expired(pool, 300, &mut ready);
        assert_eq!(ready.len(), 1);
        assert!(ready[0].state == handle);
        drop(state);
    }

    #[test]
    fn timing_wheel_skips_cancelled_entries() {
        let timer = test_timer();
        let pool = &timer.shared.sleep_pool;
        let mut wheel = TimingWheel::new(0);
        let (state, entry) = timer_entry(&timer, 100, 50);
        drop(state);
        wheel.insert(pool, entry);

        let mut ready = Vec::new();
        wheel.drain_expired(pool, 2, &mut ready);
        assert!(ready.is_empty());
    }

    #[test]
    fn cancelled_sleep_releases_slot_and_stale_handles_fail_validation() {
        let timer = test_timer();
        let pool = &timer.shared.sleep_pool;
        let (state, entry) = timer_entry(&timer, 300 * 50, 50);
        let handle = entry.state;

        // Cancelling releases the slot immediately even though a wheel
        // entry could still reference it until its distant deadline.
        drop(state);
        assert!(handle.is_dead(pool));

        // Recycle slots until the same index is handed out again and
        // prove the stale handle never validates against the new
        // occupant while the fresh handle does.
        let mut held = Vec::new();
        let reused = loop {
            let sleep_ref = super::SleepRef::new(&timer.shared);
            if sleep_ref.idx == handle.idx {
                break sleep_ref;
            }
            held.push(sleep_ref);
        };
        let fresh = reused.downgrade();
        assert_eq!(fresh.idx, handle.idx);
        assert_ne!(fresh.generation, handle.generation);
        assert!(handle.is_dead(pool));
        assert!(!fresh.is_dead(pool));
        drop(entry);
    }

    #[test]
    fn timing_wheel_drain_retains_bucket_capacity() {
        let timer = test_timer();
        let pool = &timer.shared.sleep_pool;
        let mut wheel = TimingWheel::new(0);
        let (state, entry) = timer_entry(&timer, 100, 50);
        wheel.insert(pool, entry);
        let slot = super::wheel_slot(2, 0);
        let retained_capacity = wheel.levels[0].buckets[slot].capacity();
        assert_ne!(retained_capacity, 0);

        let mut ready = Vec::new();
        wheel.drain_expired(pool, 2, &mut ready);

        assert_eq!(ready.len(), 1);
        assert_eq!(wheel.levels[0].buckets[slot].capacity(), retained_capacity);
        drop(state);
    }

    #[test]
    fn timing_wheel_cascade_retains_bucket_capacity() {
        let timer = test_timer();
        let pool = &timer.shared.sleep_pool;
        let mut wheel = TimingWheel::new(0);
        let (state, entry) = timer_entry(&timer, 300 * 50, 50);
        wheel.insert(pool, entry);
        let slot = super::wheel_slot(256, 1);
        let retained_capacity = wheel.levels[1].buckets[slot].capacity();
        assert_ne!(retained_capacity, 0);

        let mut ready = Vec::new();
        wheel.drain_expired(pool, 256, &mut ready);

        assert!(ready.is_empty());
        assert_eq!(wheel.levels[1].buckets[slot].capacity(), retained_capacity);
        drop(state);
    }

    #[test]
    fn timing_wheel_rebuilds_cached_deadline_after_cancelled_front_entry() {
        let timer = test_timer();
        let pool = &timer.shared.sleep_pool;
        let mut wheel = TimingWheel::new(0);
        let (cancelled, entry) = timer_entry(&timer, 100, 50);
        drop(cancelled);
        wheel.insert(pool, entry);
        let (state, entry) = timer_entry(&timer, 200, 50);
        wheel.insert(pool, entry);

        let mut ready = Vec::new();
        wheel.drain_expired(pool, 2, &mut ready);
        assert!(ready.is_empty());
        assert_eq!(
            wheel
                .next_live_deadline(pool)
                .map(|deadline| deadline.ticks()),
            Some(200)
        );
        drop(state);
    }

    #[test]
    fn elapsed_sleep_does_not_allocate_state() {
        let timer = super::Timer::new(TestCpu { now: 100 });
        assert!(timer.sleep_until(Instant::new(101)).state.is_none());
        assert!(timer.sleep_until(Instant::new(100)).state.is_none());
        assert!(timer.sleep_until(Instant::new(99)).state.is_none());
        assert!(timer.sleep_for(core::time::Duration::ZERO).state.is_none());
    }

    #[test]
    fn pending_sleep_allocates_state_on_first_poll() {
        let timer = super::Timer::new(TestCpu { now: 100 });
        let mut sleep = timer.sleep_until(Instant::new(101));
        assert!(sleep.state.is_none());

        let mut context = Context::from_waker(noop_waker_ref());
        assert!(Pin::new(&mut sleep).poll(&mut context).is_pending());

        assert!(sleep.state.is_some());
    }

    #[test]
    fn timer_inbox_is_bounded_to_kernel_capacity() {
        let timer = super::Timer::new(TestCpu { now: 100 });

        assert_eq!(timer.shared.inbox.capacity(), Some(TIMER_INBOX_CAPACITY));
    }

    #[test]
    fn timer_ready_pool_reset_drops_oversized_capacity() {
        let timer = test_timer();
        let mut ready = Vec::with_capacity(TIMER_READY_RETAINED_ENTRIES + 1);
        let (state, entry) = timer_entry(&timer, 100, 50);
        ready.push(entry);

        reset_timer_ready_entries(&mut ready);

        assert!(ready.is_empty());
        assert_eq!(ready.capacity(), TIMER_READY_RETAINED_ENTRIES);
        drop(state);
    }
}
