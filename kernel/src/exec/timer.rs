//! Each processor's kernel timer, and the [`Sleep`] future.
//!
//! A pending sleep's timer state lives in the sleep. [`Sleep`] carries a
//! [`SleepNode`]: its links in the owner's [`TimingWheel`], the quantum
//! tick it is filed under, the list it is on, a state word and an
//! `AtomicWaker`. Registering links memory the sleep already is, so it
//! cannot fail for capacity, and nothing kernel-owned grows with the
//! number of sleeps: the wheel's slot heads are fixed and built once in
//! [`Timer::new`]. A dropped sleep unlinks itself at once and leaves
//! nothing behind for a later drain. The shape is Tokio's `TimerEntry`
//! (`runtime/time/entry.rs`) on a per-processor base, as Linux's
//! `hrtimer` and `timer_list` embed their queue nodes.
//!
//! # Concurrency contract
//!
//! A `Timer` belongs to the processor that built it, its owner
//! ([`TimerOwner`]). Its wheel is guarded by one `spin::Mutex`. A `Sleep`
//! registers on the `Timer` it was made from, whichever processor polls
//! it.
//!
//! | Operation | Who | Context | Synchronisation | Allocates |
//! | --- | --- | --- | --- | --- |
//! | `Sleep::poll`, first pending poll (`IDLE → REGISTERED`) | the holder of `Pin<&mut Sleep>`, any processor | task | `AtomicWaker::register` (lock-free), then the wheel lock for an O(1) link and to lower `next_sleep_deadline`; after unlocking, the owner arms the deadline itself, and any other processor wakes the owner if it is parked on a deadline platform | never |
//! | `Sleep::poll` while `REGISTERED` | any processor | task | lock-free: a state load and `AtomicWaker::register`. Takes the lock only when the deadline has already passed, to deregister and complete, so a completed sleep holds nothing even before it is dropped | never |
//! | `Sleep::drop` | any processor | task only: a future is dropped only inside async-task's `run` or `Runnable::drop`, never in an interrupt handler | an `Acquire` state load: `IDLE` or `DONE` returns with no lock (Linux `hrtimer_try_to_cancel`'s lock-free test). `REGISTERED` or `FIRING` takes the lock, re-tests, unlinks in O(1) and stores `DONE`. The `AtomicWaker` is dropped by drop glue after the lock is released | never |
//! | [`Timer::fire_expired`] | owner only (asserted) | task | lock-free fast path: `next_sleep_deadline > now` returns 0. Otherwise the lock; the wheel jumps by its occupancy words; each due node is unlinked and fired. Every [`TIMER_WAKE_BATCH`] wakers the lock is released, the batch is woken and the lock is retaken (Tokio `process_at_time`). Then `next_sleep_deadline` is stored, the lock released, the rest woken and the deadline armed | never: the batch is a stack array |
//! | [`Timer::handle_interrupt`] | owner | interrupt | atomics only (`armed_deadline`, `next_sleep_deadline`); never the wheel lock | never |
//! | [`Timer::is_due`], [`Timer::arm_published_deadline`], [`Timer::arm_scheduler_tick`] | owner | task (park path, run loop) | atomics only | never |
//!
//! The lock is a plain `spin::Mutex`, as `ready`'s global consumer lock
//! is. No interrupt handler can reach it: [`Timer::handle_interrupt`]
//! reads atomics, wakers run outside it, and no `Sleep` is polled or
//! dropped in interrupt context. So the `IrqSafeMutex` rule for locks an
//! interrupt handler can reach does not apply. Nothing inside it
//! allocates, calls or drops a waker, takes another lock, or faults: a
//! node lives in kernel heap, the task arena or a fiber stack, none of
//! which is swappable, and its page is resident from the registration's
//! first write. Register and cancel hold it for O(1); firing holds it for
//! at most [`TIMER_WAKE_BATCH`] fires; a cascade holds it for one slot's
//! nodes.
//!
//! # Node states
//!
//! - `IDLE`: on no list. Initial. No processor but the holder has the
//!   node's address.
//! - `REGISTERED`: linked in a slot list or in `pending`. Entered only
//!   under the lock.
//! - `FIRING`: taken off the wheel by a firer that still holds the lock
//!   and is taking the waker. A poll reads it as complete; a drop that
//!   reads it waits for the lock.
//! - `DONE`: off the wheel for good, fired or deregistered. Stored only
//!   under the lock.
//!
//! # Invariants the `unsafe` code rests on
//!
//! - *Lifetime*: a node is linked in a wheel list only while it is
//!   `REGISTERED`, and its sleep's `Drop` unlinks it under the lock before
//!   the memory can be reused. That is the `Pin` drop guarantee, which
//!   Tokio's `TimerEntry` rests on: `Sleep` is `!Unpin`, so once polled it
//!   is never moved, and its memory is not reused until it is dropped.
//! - *Lock*: a node's links, filed tick and list are read and written only
//!   under the wheel lock, and its state changes to `REGISTERED`,
//!   `FIRING` and `DONE` only under it.
//! - *Last access*: whoever moves a node to `DONE` does it with a
//!   `Release` store, under the lock, as its last access to the node.
//!
//! # Memory ordering
//!
//! - **Registrant.** Writes the links, tick and list, then stores
//!   `REGISTERED`, under the lock. A later holder sees them through the
//!   lock's acquire.
//! - **Firer.** Under the lock: unlinks the node (writing it and its
//!   neighbours), stores `FIRING`, takes the waker, stores `DONE` with
//!   `Release`. It never touches the node again. `FIRING` comes before
//!   the take because `AtomicWaker` requires the state a waiter tests to
//!   change before its waker is taken: a poll that registers a new waker
//!   between a take and a `DONE` store would otherwise test `REGISTERED`,
//!   return `Pending`, and never be woken.
//! - **Deregisterer.** Under the lock: unlinks the node and stores `DONE`
//!   with `Release`.
//! - **Dropper.** An `Acquire` load that reads `DONE` synchronizes with
//!   that `Release`, so every access the remover made happens before the
//!   memory is freed. Reading `IDLE` means no other processor ever had the
//!   address. Reading `REGISTERED` or `FIRING` goes through the lock,
//!   which orders the dropper after every earlier holder; under it the
//!   node is `REGISTERED` (unlink it) or `DONE` (a firer won; nothing to
//!   do), because `FIRING` exists only while a firer holds the lock.
//! - **Poll racing a fire.** `AtomicWaker`'s protocol covers it: either
//!   the firer takes the new waker, or the poller's `register` sees the
//!   firer's take and wakes the new waker itself, and either way its
//!   re-test reads `FIRING` or `DONE` and returns `Ready`.
//! - **The deadline handshake with a parked owner.** The registrant's
//!   `SeqCst` store to `next_sleep_deadline` and its `SeqCst` read of the
//!   owner's idle state pair with the owner's `SeqCst` `Parked` store and
//!   its [`Timer::is_due`] and [`Timer::arm_published_deadline`] loads:
//!   either the registrant sees `Parked` and wakes the owner, or the
//!   owner's load comes after the lowering and arms it before it parks.
//!   The store is made under the lock, after a `min` test against the
//!   value there, so it needs no compare-and-swap: every writer of
//!   `next_sleep_deadline` holds the lock.
//! - **`next_sleep_deadline` is never later than the wheel's earliest
//!   expiration.** A registration lowers it under the lock;
//!   [`Timer::fire_expired`] stores the wheel's next expiration under the
//!   lock; a cancel never raises it, and a stale low value costs one slow
//!   path. So the lock-free fast path never skips a due sleep.
//!
//! # Cache lines
//!
//! `TimerShared`, one per processor, in its `triomphe::Arc`:
//!
//! | Block | Contents | Written by |
//! | --- | --- | --- |
//! | read-only | `owner`, `tick_ticks`, `idle_interval_ticks`, `wheel_quantum_ticks` | nobody after [`Timer::new`] |
//! | `CachePadded<TimerDeadlines>` | `next_sleep_deadline`, `armed_deadline` | `armed_deadline`: the owner only, from task context with interrupts masked and from its timer interrupt. `next_sleep_deadline`: wheel-lock holders. Both read by the owner on its run and park paths and by its interrupt |
//! | `CachePadded<spin::Mutex<TimingWheel>>` | the lock word, `elapsed`, `pending`, the six occupancy words; the slot heads follow | every lock holder: the owner's fire, any registrant or canceller |
//! | each [`SleepNode`] | in its holder's memory | links, tick and list: lock holders. `state`: lock holders. `waker`: the poller and the firer |

use core::future::Future;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use core::task::{Context, Poll, Waker};
use core::time::Duration;

use crossbeam_utils::CachePadded;
use helios_hal::cpu::{Cpu, Instant, TimerInterrupt, current_processor};
use helios_hal::critical_section::with_local_interrupts_masked;
use spin::Mutex;
use triomphe::Arc;

use super::executor::TimerOwner;
use super::wheel::{NodeState, SleepNode, TimingWheel};

/// How long a guest holds its processor before it is preempted and
/// yields to the executor.
///
/// This is a scheduling policy, the same on every backend and
/// independent of the processor count. 2.5 ms is the yield interval the
/// x86-64 bench lane's guests had when each of its four processors'
/// 100 Hz ticks interrupted every running guest, and sits at Linux's
/// EEVDF base slice on a four-processor machine (0.75 ms scaled by
/// `1 + log2(4)`, 2.25 ms; the CFS minimum granularity it replaced
/// scaled the same way to 3 ms). A longer slice delays every instance
/// that shares a processor with a CPU-bound one by that much more before
/// its next turn; a shorter one spends more processor time on yields.
///
/// No hardware timer runs at the slice. Every processor's timer
/// interrupt walks every processor's running guest and interrupts each
/// one that has run for at least a slice
/// (`wasmtime_adapter::preemption`), so the slice is enforced by
/// whichever processor's interrupt comes first after it ends: a guest
/// yields at the first timer interrupt anywhere on the machine at or
/// after one slice into its run. The effective slice is this constant
/// rounded up to that interrupt:
///
/// - x86-64, where each processor's local APIC ticks every 10 ms with
///   an unsynchronised phase: between one slice and one slice plus the
///   largest gap between consecutive ticks of all processors. On four
///   processors with evenly staggered ticks that is 2.5–5 ms; on one
///   processor it is 2.5–12.5 ms, because a run that starts less than a
///   slice before a tick is not yet due at that tick.
/// - aarch64 and riscv64, whose timer interrupt is this [`Timer`]'s
///   deadline: a processor that is running work keeps its deadline
///   within one [`SCHEDULER_TICK`], re-armed one tick on from each of
///   its interrupts and armed a tick out by the first executor pass
///   that runs work after a park, so the bound is the x86-64 one, with
///   each processor's phase set by when it last left idle. A parked
///   processor stretches its deadline to [`IDLE_INTERRUPT_INTERVAL`],
///   and runs no guest.
pub const GUEST_TIME_SLICE: Duration = Duration::from_micros(2_500);
/// The longest a processor that is running work goes without a timer
/// interrupt on a deadline-timer platform: the scheduler tick, the
/// period x86-64's local APIC runs at. Guest preemption rides on it.
const SCHEDULER_TICK: Duration = Duration::from_millis(10);
/// The longest a parked processor with no sleep due sooner goes without
/// a timer interrupt.
const IDLE_INTERRUPT_INTERVAL: Duration = Duration::from_millis(100);
/// The wheel's resolution. A sleep is due at the first quantum boundary
/// at or after its deadline, and that boundary is the deadline the timer
/// publishes and arms.
pub(super) const TIMER_WHEEL_QUANTUM: Duration = Duration::from_micros(50);
/// How many wakers a fire collects before it releases the wheel lock to
/// wake them: Tokio's `WakeList` capacity, `NUM_WAKERS`
/// (`util/wake_list.rs`).
const TIMER_WAKE_BATCH: usize = 32;

#[derive(Clone)]
pub struct Timer<CpuImpl: Cpu + Clone> {
    cpu: CpuImpl,
    shared: Arc<TimerShared>,
}

/// A future that completes at a deadline on a [`Timer`].
///
/// `!Unpin`: once polled, the node it carries may be linked in the
/// timer's wheel, and its `Drop` unlinks it.
pub struct Sleep<CpuImpl: Cpu + Clone> {
    timer: Timer<CpuImpl>,
    deadline: Option<Instant>,
    node: SleepNode,
}

/// One processor's timer state. The module docs give its concurrency
/// contract and which processor writes each block.
struct TimerShared {
    owner: TimerOwner,
    tick_ticks: u64,
    idle_interval_ticks: u64,
    wheel_quantum_ticks: u64,
    deadlines: CachePadded<TimerDeadlines>,
    wheel: CachePadded<Mutex<TimingWheel>>,
}

struct TimerDeadlines {
    /// The earliest instant the wheel has work, in timer ticks: a quantum
    /// boundary, never later than the earliest registered sleep's.
    /// `u64::MAX` when nothing is registered.
    next_sleep_deadline: AtomicU64,
    /// The deadline programmed into the owner's hardware. Only the owner
    /// writes it, from task context with local interrupts masked and from
    /// its timer interrupt, so the record and the comparator never
    /// disagree. Task context only ever moves it earlier; the timer
    /// interrupt, which runs once it has passed, picks the next one: the
    /// earlier of the next sleep and one [`SCHEDULER_TICK`] on, or one
    /// [`IDLE_INTERRUPT_INTERVAL`] on when the owner is parked. An
    /// executor pass about to run work pulls it to within a tick
    /// ([`Timer::arm_scheduler_tick`]), so a processor running a guest
    /// takes an interrupt at least once a tick and an idle one keeps its
    /// long deadline, with no arm per guest poll. A processor that is not
    /// the owner never programs a deadline: [`Cpu::set_deadline`] reaches
    /// the executing processor's comparator, not the owner's. It wakes the
    /// owner instead, when the owner is parked, and the owner programs the
    /// new deadline before it parks again.
    armed_deadline: AtomicU64,
}

/// The wakers one fire collects under the lock, woken after it is
/// released. A stack array: firing allocates nothing.
struct WakeBatch {
    wakers: [Option<Waker>; TIMER_WAKE_BATCH],
    len: usize,
}

impl WakeBatch {
    const fn new() -> Self {
        Self {
            wakers: [const { None }; TIMER_WAKE_BATCH],
            len: 0,
        }
    }

    fn push(&mut self, waker: Waker) {
        self.wakers[self.len] = Some(waker);
        self.len += 1;
    }

    fn is_full(&self) -> bool {
        self.len == TIMER_WAKE_BATCH
    }

    /// Wakes every collected waker. Called with the wheel lock released.
    fn wake_all(&mut self) {
        for waker in self.wakers[..self.len].iter_mut().filter_map(Option::take) {
            waker.wake();
        }
        self.len = 0;
    }
}

impl<CpuImpl: Cpu + Clone> Timer<CpuImpl> {
    /// The timer of `owner`, built on `owner` itself: the deadline it
    /// programs here is the owner's.
    pub(crate) fn new(cpu: CpuImpl, owner: TimerOwner) -> Self {
        assert_eq!(
            current_processor(),
            owner.processor(),
            "a timer is built on the processor that owns it"
        );
        let timebase = cpu.timebase();
        let tick_ticks = timebase.duration_to_ticks(SCHEDULER_TICK);
        let idle_interval_ticks = timebase.duration_to_ticks(IDLE_INTERRUPT_INTERVAL);
        let wheel_quantum_ticks = timebase.duration_to_ticks(TIMER_WHEEL_QUANTUM);
        assert!(
            tick_ticks != 0,
            "scheduler tick {SCHEDULER_TICK:?} converted to zero timer ticks"
        );
        assert!(
            wheel_quantum_ticks != 0,
            "timer wheel quantum {TIMER_WHEEL_QUANTUM:?} converted to zero timer ticks"
        );
        // The owner is running when it builds its timer. On a periodic
        // platform this call is what starts the interrupt.
        let initial_deadline = cpu.now().saturating_add(tick_ticks);
        cpu.set_deadline(initial_deadline);
        let elapsed = wheel_tick_floor(cpu.now(), wheel_quantum_ticks);
        Self {
            cpu,
            shared: Arc::new(TimerShared {
                owner,
                tick_ticks,
                idle_interval_ticks,
                wheel_quantum_ticks,
                deadlines: CachePadded::new(TimerDeadlines {
                    next_sleep_deadline: AtomicU64::new(u64::MAX),
                    armed_deadline: AtomicU64::new(initial_deadline.ticks()),
                }),
                wheel: CachePadded::new(Mutex::new(TimingWheel::new(elapsed))),
            }),
        }
    }

    pub fn now(&self) -> Instant {
        self.cpu.now()
    }

    /// The platform `Cpu` this timer reads the time from.
    pub(crate) fn cpu(&self) -> &CpuImpl {
        &self.cpu
    }

    /// Whether a sleep deadline is already due as of `now` — the idle
    /// loop's reason to stop polling without parking.
    ///
    /// `SeqCst`: after the owner has published `Parked`, this is the
    /// parker half of the wake handshake [`Self::publish_lowered_deadline`]
    /// describes.
    pub(crate) fn is_due(&self, now: Instant) -> bool {
        self.shared
            .deadlines
            .next_sleep_deadline
            .load(AtomicOrdering::SeqCst)
            <= now.ticks()
    }

    /// Programs the published sleep deadline into the owner's hardware
    /// if it is earlier than the deadline armed there. The owner calls
    /// this after publishing `Parked` and before it parks, so a deadline
    /// another processor lowered while the owner was running or polling,
    /// and so did not wake it for, is armed before the owner stops
    /// looking.
    pub(crate) fn arm_published_deadline(&self) {
        // `SeqCst`: the parker half of the wake handshake.
        let published = self
            .shared
            .deadlines
            .next_sleep_deadline
            .load(AtomicOrdering::SeqCst);
        self.arm_earlier(published);
    }

    /// Pulls the owner's deadline to within one [`SCHEDULER_TICK`]. The
    /// executor calls this before it runs the first task of a pass, so
    /// work that starts after a park, where the deadline may be an idle
    /// interval out, is interrupted within a tick. Owner only.
    ///
    /// The deadline already sits within a tick on every later pass, so
    /// this is a clock read and a compare; it programs the hardware at
    /// most once per tick.
    pub(crate) fn arm_scheduler_tick(&self) {
        if CpuImpl::TIMER_INTERRUPT == TimerInterrupt::Periodic {
            return;
        }
        self.arm_earlier(self.now().ticks().saturating_add(self.shared.tick_ticks));
    }

    pub fn sleep_until(&self, deadline: Instant) -> Sleep<CpuImpl> {
        Sleep {
            timer: self.clone(),
            deadline: (deadline > self.now()).then_some(deadline),
            node: SleepNode::new(),
        }
    }

    pub fn sleep_for(&self, duration: Duration) -> Sleep<CpuImpl> {
        let ticks = self.cpu.timebase().duration_to_ticks(duration);
        self.sleep_until(self.now().saturating_add(ticks))
    }

    /// Fires every sleep due by now and returns how many fired. Owner
    /// only.
    ///
    /// When nothing is due this is one clock read and one load, and takes
    /// no lock (Go's `timers.check` early-out on `wakeTime`).
    pub fn fire_expired(&self) -> usize {
        assert_eq!(
            current_processor(),
            self.shared.owner.processor(),
            "only the processor that owns a timer fires it"
        );
        let now = self.now();
        let deadlines = &self.shared.deadlines;
        if deadlines.next_sleep_deadline.load(AtomicOrdering::Acquire) > now.ticks() {
            return 0;
        }

        let quantum = self.shared.wheel_quantum_ticks;
        let now_tick = wheel_tick_floor(now, quantum);
        let mut batch = WakeBatch::new();
        let mut fired = 0;
        let mut wheel = self.shared.wheel.lock();
        while let Some(waker) = wheel.fire_next(now_tick) {
            fired += 1;
            let Some(waker) = waker else {
                continue;
            };
            batch.push(waker);
            if batch.is_full() {
                // Wakers run outside the lock: a waker may reach this
                // timer, and a long fire must not hold off registrants.
                drop(wheel);
                batch.wake_all();
                wheel = self.shared.wheel.lock();
            }
        }
        let next = wheel
            .next_expiration_tick()
            .map_or(u64::MAX, |tick| tick.saturating_mul(quantum));
        deadlines
            .next_sleep_deadline
            .store(next, AtomicOrdering::SeqCst);
        drop(wheel);
        batch.wake_all();
        self.arm_earlier(next);
        fired
    }

    /// How many sleeps are registered on this timer's wheel. It walks
    /// every list under the wheel lock, so it is for tests and diagnosis,
    /// never a hot path.
    pub fn registered_sleeps(&self) -> usize {
        self.shared.wheel.lock().registered()
    }

    /// The owner's timer interrupt: arms the next one, at the earlier of
    /// the next sleep and one [`SCHEDULER_TICK`] on, or one
    /// [`IDLE_INTERRUPT_INTERVAL`] on when the owner is parked.
    ///
    /// A sleep already due is left to the event loop, which this
    /// interrupt has just woken or will reach after the running task.
    /// Arming it would re-raise the interrupt at once.
    pub fn handle_interrupt(&self) -> usize {
        let now = self.now();
        let interval = if self.shared.owner.is_parked() {
            self.shared.idle_interval_ticks
        } else {
            self.shared.tick_ticks
        };
        let periodic = now.ticks().saturating_add(interval);
        let sleep = self
            .shared
            .deadlines
            .next_sleep_deadline
            .load(AtomicOrdering::Acquire);
        let deadline = if sleep > now.ticks() {
            periodic.min(sleep)
        } else {
            periodic
        };
        self.arm_deadline(Instant::new(deadline));
        0
    }

    /// Makes sure the owner's hardware will interrupt by `deadline`, which
    /// the caller has just stored as the lower `next_sleep_deadline` under
    /// the wheel lock.
    ///
    /// On the owner that is a reprogramming of its own comparator. On
    /// any other processor it is not, because [`Cpu::set_deadline`]
    /// would program the executing processor's comparator, replacing
    /// that processor's own deadline with this one. The owner is
    /// reached through the executor's idle handshake instead:
    ///
    /// - this side lowers `next_sleep_deadline`, then reads the owner's
    ///   idle state, and wakes it only if it has published `Parked`;
    /// - the owner publishes `Parked`, then reads `next_sleep_deadline`
    ///   ([`Self::is_due`], [`Self::arm_published_deadline`]) and arms
    ///   it before it parks.
    ///
    /// All four accesses are `SeqCst`, so in their single total order
    /// either this side sees `Parked` and sends the wake, or the owner's
    /// read comes after the lowering and arms it. A running owner arms
    /// it on its next executor pass or before its next park. A periodic
    /// timer interrupt needs neither: no deadline is programmed, and the
    /// owner's next tick finds the sleep.
    fn publish_lowered_deadline(&self, deadline: u64) {
        let owner = self.shared.owner.processor();
        if current_processor() == owner {
            self.arm_earlier(deadline);
        } else if CpuImpl::TIMER_INTERRUPT == TimerInterrupt::Deadline
            && self.shared.owner.is_parked()
        {
            self.cpu.wake_processor(owner);
        }
    }

    /// Programs `deadline` into the owner's hardware if it is earlier
    /// than the one armed there. Owner task context only.
    ///
    /// Task context never moves the deadline later. A deadline that
    /// turns out early (a cancelled sleep, a tick armed for work that
    /// has since finished) costs one interrupt, whose handler picks the
    /// next deadline. Moving it later on every pass would reprogram the
    /// comparator on every pass, and would undo the tick a running
    /// guest needs. `u64::MAX`, no sleep pending, is never earlier.
    fn arm_earlier(&self, deadline: u64) {
        let armed = &self.shared.deadlines.armed_deadline;
        if CpuImpl::TIMER_INTERRUPT == TimerInterrupt::Periodic
            || deadline >= armed.load(AtomicOrdering::Relaxed)
        {
            return;
        }
        // Re-checked with interrupts masked: the owner's timer interrupt
        // may have re-armed between the test above and the mask.
        with_local_interrupts_masked(|| {
            if deadline < armed.load(AtomicOrdering::Relaxed) {
                self.arm_deadline(Instant::new(deadline));
            }
        });
    }

    /// Programs `deadline` into the owner's hardware and records it.
    ///
    /// Owner only: [`Cpu::set_deadline`] reaches the executing
    /// processor. Local interrupts are masked across the record and the
    /// programming, so the owner's timer interrupt, which re-arms
    /// through here too, cannot land between them and leave the record
    /// naming a deadline the comparator does not hold.
    fn arm_deadline(&self, deadline: Instant) {
        // A periodic interrupt has no deadline to program; the owner
        // started it when it built the timer.
        if CpuImpl::TIMER_INTERRUPT == TimerInterrupt::Periodic {
            return;
        }
        assert_eq!(
            current_processor(),
            self.shared.owner.processor(),
            "only the processor that owns a timer programs its deadline"
        );
        let deadline_ticks = deadline.ticks();
        with_local_interrupts_masked(|| {
            let previous = self
                .shared
                .deadlines
                .armed_deadline
                .swap(deadline_ticks, AtomicOrdering::Relaxed);
            if previous != deadline_ticks {
                self.cpu.set_deadline(deadline);
            }
        });
    }
}

impl<CpuImpl: Cpu + Clone> Sleep<CpuImpl> {
    /// Links this sleep's node into its timer's wheel and lowers the
    /// published deadline if this sleep is the earliest.
    ///
    /// The node is `IDLE` and its waker already registered, so a fire that
    /// follows the unlock finds it.
    fn register(self: Pin<&Self>, deadline: Instant) {
        let shared = &self.timer.shared;
        let quantum = shared.wheel_quantum_ticks;
        let deadline_tick = wheel_tick_ceil(deadline, quantum);
        // The instant the wheel counts the sleep due: the quantum boundary
        // at or after its deadline.
        let due = deadline_tick.saturating_mul(quantum);
        let lowered = {
            let mut wheel = shared.wheel.lock();
            // SAFETY: the node is pinned with this sleep (`self` is
            // `Pin<&Self>` and `Sleep` is `!Unpin`) and `IDLE`, and
            // `Sleep::drop` deregisters it under this lock before its
            // memory can be reused: the lifetime invariant.
            unsafe { wheel.register(NonNull::from(&self.node), deadline_tick) };
            // Every writer of `next_sleep_deadline` holds the lock, so a
            // load and a store under it are a `min`.
            let next = &shared.deadlines.next_sleep_deadline;
            let lowered = due < next.load(AtomicOrdering::Relaxed);
            if lowered {
                next.store(due, AtomicOrdering::SeqCst);
            }
            lowered
        };
        if lowered {
            self.timer.publish_lowered_deadline(due);
        }
    }

    /// Takes this sleep's node off its timer's wheel, unless a firer
    /// already has, and leaves it `DONE`.
    fn deregister(&self) {
        let mut wheel = self.timer.shared.wheel.lock();
        // SAFETY: a sleep registers only on its own timer's wheel, and the
        // node is alive for as long as `self` is: the lifetime invariant.
        unsafe { wheel.deregister(NonNull::from(&self.node)) };
    }
}

impl<CpuImpl: Cpu + Clone> Future for Sleep<CpuImpl> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Every field is read through a shared reference; the node's own
        // cells and atomics carry its changes, so nothing is moved.
        let this = self.into_ref();
        let Some(deadline) = this.deadline else {
            return Poll::Ready(());
        };
        match this.node.state() {
            NodeState::Firing | NodeState::Done => return Poll::Ready(()),
            NodeState::Idle => {
                if deadline <= this.timer.now() {
                    return Poll::Ready(());
                }
                this.node.register_waker(cx.waker());
                this.register(deadline);
            }
            NodeState::Registered => {
                if deadline <= this.timer.now() {
                    // Complete now rather than wait for the owner's next
                    // fire, so a completed sleep holds nothing.
                    this.deregister();
                    return Poll::Ready(());
                }
                this.node.register_waker(cx.waker());
            }
        }
        // Re-tested after the waker is registered: a fire that took the
        // previous waker has stored `FIRING` first.
        if this.node.state().is_complete() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl<CpuImpl: Cpu + Clone> Drop for Sleep<CpuImpl> {
    fn drop(&mut self) {
        match self.node.state() {
            // Never linked, or a remover has finished with it: no lock.
            NodeState::Idle | NodeState::Done => {}
            NodeState::Registered | NodeState::Firing => self.deregister(),
        }
    }
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

#[cfg(test)]
mod tests {
    use core::future::Future as _;
    use core::pin::pin;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context, Poll, Waker};
    use core::time::Duration;

    use std::boxed::Box;
    use std::sync::Arc;
    use std::vec::Vec;

    use helios_hal::cpu::{Cpu, Instant, ProcessorId, Timebase, TimerInterrupt};

    use super::{wheel_tick_ceil, wheel_tick_floor};
    use crate::test_support::{CountingWaker, ManualClockCpu, PAST_THE_OLD_TIMER_CAPACITY};

    #[derive(Clone)]
    struct TestCpu {
        now: u64,
        timebase: Timebase,
    }

    impl TestCpu {
        fn at(now: u64) -> Self {
            Self {
                now,
                timebase: Timebase::new(1_000_000),
            }
        }
    }

    impl Cpu for TestCpu {
        const TIMER_INTERRUPT: TimerInterrupt = TimerInterrupt::Deadline;

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

        fn timebase(&self) -> Timebase {
            self.timebase
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

    #[test]
    fn wheel_tick_rounding_preserves_deadline_order() {
        assert_eq!(wheel_tick_floor(Instant::new(100), 50), 2);
        assert_eq!(wheel_tick_ceil(Instant::new(101), 50), 3);
        assert_eq!(wheel_tick_ceil(Instant::new(150), 50), 3);
    }

    /// A sleep whose deadline has passed is ready at once and never
    /// touches the wheel; one that is never polled never registers.
    #[test]
    fn an_elapsed_or_unpolled_sleep_never_registers() {
        let timer = crate::test_support::test_timer(TestCpu::at(100));
        let mut context = Context::from_waker(Waker::noop());

        for deadline in [100, 99] {
            let sleep = pin!(timer.sleep_until(Instant::new(deadline)));
            assert!(sleep.poll(&mut context).is_ready());
        }
        let sleep = pin!(timer.sleep_for(Duration::ZERO));
        assert!(sleep.poll(&mut context).is_ready());
        let unpolled = timer.sleep_until(Instant::new(101));

        assert_eq!(timer.registered_sleeps(), 0);
        drop(unpolled);
    }

    /// The shape of `wasi:io/poll`'s `ready` on a timer pollable: a sleep
    /// polled once and dropped. Before #705 each left an inbox entry that
    /// only `fire_expired` drained, and the 4097th panicked.
    #[test]
    fn a_sleep_polled_once_and_dropped_leaves_nothing_registered() {
        let timer = crate::test_support::test_timer(TestCpu::at(100));
        let mut context = Context::from_waker(Waker::noop());

        for _ in 0..PAST_THE_OLD_TIMER_CAPACITY {
            {
                let sleep = pin!(timer.sleep_for(Duration::from_secs(1)));
                assert!(sleep.poll(&mut context).is_pending());
                assert_eq!(timer.registered_sleeps(), 1);
            }
            assert_eq!(timer.registered_sleeps(), 0);
        }
    }

    /// More sleeps pending at once than the old pool held, all of which
    /// fire, and every one of whose wakers is woken once.
    #[test]
    fn more_pending_sleeps_than_the_old_pool_held_all_fire() {
        let clock = ManualClockCpu::new();
        let timer = crate::test_support::test_timer(clock.clone());
        let waker = Arc::new(CountingWaker::default());
        let waker_handle = Waker::from(waker.clone());
        let mut context = Context::from_waker(&waker_handle);

        let mut sleeps: Vec<_> = (0..PAST_THE_OLD_TIMER_CAPACITY)
            .map(|_| Box::pin(timer.sleep_for(Duration::from_millis(1))))
            .collect();
        for sleep in &mut sleeps {
            assert!(sleep.as_mut().poll(&mut context).is_pending());
        }
        assert_eq!(timer.registered_sleeps(), PAST_THE_OLD_TIMER_CAPACITY);

        clock.advance(2_000_000);
        assert_eq!(timer.fire_expired(), PAST_THE_OLD_TIMER_CAPACITY);
        assert_eq!(waker.wakes(), PAST_THE_OLD_TIMER_CAPACITY);
        assert_eq!(timer.registered_sleeps(), 0);
        for sleep in &mut sleeps {
            assert!(sleep.as_mut().poll(&mut context).is_ready());
        }
    }

    /// A sleep registered on processor 0's timer and dropped on processor
    /// 1 takes itself off processor 0's wheel, with no fire in between.
    #[test]
    fn a_sleep_dropped_on_another_processor_unlinks_itself() {
        crate::test_processor_identity::set(ProcessorId::new(0));
        let timer = crate::test_support::test_timer(TestCpu::at(100));
        let mut context = Context::from_waker(Waker::noop());
        let mut sleep = Box::pin(timer.sleep_for(Duration::from_secs(1)));
        assert!(sleep.as_mut().poll(&mut context).is_pending());
        assert_eq!(timer.registered_sleeps(), 1);

        std::thread::spawn(move || {
            crate::test_processor_identity::set(ProcessorId::new(1));
            drop(sleep);
        })
        .join()
        .expect("the dropping processor panicked");

        assert_eq!(timer.registered_sleeps(), 0);
    }

    /// Four processors register, cancel and wait on sleeps on processor
    /// 0's timer while processor 0 advances the clock and fires. No waker
    /// is woken twice, every sleep whose waker fired is ready, and the
    /// wheel ends empty. With no loom or shuttle in the tree, this and the
    /// module docs' written argument are the evidence for the protocol.
    #[test]
    fn register_cancel_and_fire_race_from_many_processors() {
        const PROCESSORS: u16 = 4;
        const ITERATIONS: u64 = 100_000;
        /// The furthest a sleep is set out, in nanoseconds: 200 µs.
        const FURTHEST: u64 = 200_000;

        crate::test_processor_identity::set(ProcessorId::new(0));
        let clock = ManualClockCpu::new();
        let timer = crate::test_support::test_timer(clock.clone());
        let finished = AtomicUsize::new(0);
        let fired_but_pending = AtomicUsize::new(0);

        let wakers: Vec<Arc<CountingWaker>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (1..=PROCESSORS)
                .map(|processor| {
                    let (timer, finished, fired_but_pending) =
                        (&timer, &finished, &fired_but_pending);
                    scope.spawn(move || {
                        crate::test_processor_identity::set(ProcessorId::new(processor));
                        let mut wakers = Vec::with_capacity(ITERATIONS as usize);
                        for iteration in 0..ITERATIONS {
                            let counter = Arc::new(CountingWaker::default());
                            let waker = Waker::from(counter.clone());
                            let mut context = Context::from_waker(&waker);
                            let out = (iteration * 7_919 + u64::from(processor)) % (FURTHEST + 1);
                            let mut sleep = pin!(timer.sleep_for(Duration::from_nanos(out)));
                            let registered = sleep.as_mut().poll(&mut context).is_pending();
                            // Every other sleep waits for its fire; the rest
                            // are dropped at once, racing it.
                            if registered && iteration % 2 == 1 {
                                while counter.wakes() == 0 {
                                    core::hint::spin_loop();
                                }
                                if sleep.as_mut().poll(&mut context).is_pending() {
                                    fired_but_pending.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            wakers.push(counter);
                        }
                        finished.fetch_add(1, Ordering::SeqCst);
                        wakers
                    })
                })
                .collect();

            // Processor 0 owns the timer: one quantum of clock per pass.
            while finished.load(Ordering::SeqCst) < usize::from(PROCESSORS) {
                clock.advance(50_000);
                timer.fire_expired();
            }
            workers
                .into_iter()
                .flat_map(|worker| worker.join().expect("a racing processor panicked"))
                .collect()
        });

        assert_eq!(fired_but_pending.load(Ordering::SeqCst), 0);
        let woken_twice = wakers.iter().filter(|waker| waker.wakes() > 1).count();
        assert_eq!(woken_twice, 0, "a sleep's waker was woken more than once");
        assert_eq!(timer.registered_sleeps(), 0);
    }

    /// A registered sleep re-polled with a new waker each time, while
    /// processor 0 fires, is owed a wake on the waker of its last pending
    /// poll. A lost wake hangs this test on the waker it waits for. The
    /// narrowest race, a register between the firer's take and its `DONE`
    /// store, is too short to hit from here without injecting a delay;
    /// the module docs' ordering argument (`FIRING` before the take) is
    /// what covers it.
    #[test]
    fn a_sleep_repolled_with_a_new_waker_while_it_fires_is_woken() {
        const ITERATIONS: u64 = 20_000;
        /// The furthest a sleep is set out, in nanoseconds: 200 µs.
        const FURTHEST: u64 = 200_000;
        /// The most polls a sleep takes, each with a new waker and none
        /// waiting for a wake, before one waits.
        const REPOLLS: u64 = 64;

        crate::test_processor_identity::set(ProcessorId::new(0));
        let clock = ManualClockCpu::new();
        let timer = crate::test_support::test_timer(clock.clone());
        let finished = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            let (timer, finished) = (&timer, &finished);
            scope.spawn(move || {
                crate::test_processor_identity::set(ProcessorId::new(1));
                for iteration in 0..ITERATIONS {
                    let out = 1 + iteration * 7_919 % FURTHEST;
                    let mut sleep = pin!(timer.sleep_for(Duration::from_nanos(out)));
                    let mut repolls = iteration % REPOLLS;
                    loop {
                        let counter = Arc::new(CountingWaker::default());
                        let waker = Waker::from(counter.clone());
                        let mut context = Context::from_waker(&waker);
                        if sleep.as_mut().poll(&mut context).is_ready() {
                            break;
                        }
                        if repolls != 0 {
                            repolls -= 1;
                            continue;
                        }
                        while counter.wakes() == 0 {
                            core::hint::spin_loop();
                        }
                    }
                }
                finished.fetch_add(1, Ordering::SeqCst);
            });

            while finished.load(Ordering::SeqCst) == 0 {
                clock.advance(50_000);
                timer.fire_expired();
            }
        });

        assert_eq!(timer.registered_sleeps(), 0);
    }

    /// The fast path: with nothing due, `fire_expired` answers without the
    /// wheel lock, here held by the test itself.
    #[test]
    fn fire_expired_with_nothing_due_takes_no_lock() {
        let timer = crate::test_support::test_timer(TestCpu::at(100));
        let mut context = Context::from_waker(Waker::noop());
        let sleep = pin!(timer.sleep_for(Duration::from_secs(1)));
        assert!(sleep.poll(&mut context).is_pending());

        let wheel = timer.shared.wheel.lock();
        assert_eq!(timer.fire_expired(), 0);
        drop(wheel);
    }

    /// A sleep polled again after its deadline passed completes and
    /// leaves the wheel without waiting for the owner's fire.
    #[test]
    fn a_registered_sleep_polled_after_its_deadline_completes_and_deregisters() {
        let clock = ManualClockCpu::new();
        let timer = crate::test_support::test_timer(clock.clone());
        let mut context = Context::from_waker(Waker::noop());
        let mut sleep = pin!(timer.sleep_for(Duration::from_millis(1)));
        assert!(sleep.as_mut().poll(&mut context).is_pending());
        assert_eq!(timer.registered_sleeps(), 1);

        clock.advance(1_000_000);

        assert_eq!(sleep.as_mut().poll(&mut context), Poll::Ready(()));
        assert_eq!(timer.registered_sleeps(), 0);
        assert_eq!(timer.fire_expired(), 0);
    }
}
