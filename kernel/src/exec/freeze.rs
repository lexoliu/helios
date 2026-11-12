//! Holding the whole machine still at a scheduler boundary.
//!
//! A freeze stops every processor at a point where no task is being
//! polled, no timer is firing and no interrupt can reach a driver, and
//! lets one processor, the freezer's, work alone from there. Hibernation
//! takes its image at that point, and the virtio drivers' `quiesce` and
//! `reinitialize` run at it: their contract is that they run on one
//! processor while every other is parked and no task is polled.
//!
//! # Concurrency contract
//!
//! One requester, the *freezer*, runs [`Executor::freeze`] on its
//! processor. Every other processor is a *secondary*. The freezer never
//! waits on a lock: it publishes a word, wakes the secondaries and awaits
//! their arrival, and a secondary parks on the word.
//!
//! State, each word with one writer:
//!
//! * `phase` (`Running`, `Freezing`, `Frozen`, `Thawing`), one cache
//!   line, written only by the freezer. The freezer takes it from
//!   `Running` with a compare-and-swap, so a second requester sees
//!   [`FreezeError::AlreadyFreezing`] and writes nothing.
//! * `hold`, in the same line, written only by the freezer: bit 0 is set
//!   while a freeze holds the secondaries and the other seven bits count
//!   freezes. The count makes every held value differ from the one
//!   before it, so a secondary that parked against one freeze never
//!   mistakes the next for the same.
//! * `freezer`, in the same line, written only by the freezer before it
//!   sets `hold`.
//! * one arrival word per processor, each on a line of its own and
//!   written only by its processor: the `hold` value the processor parked
//!   against, or zero (`hold` has bit 0 set whenever it is held, so a
//!   held value is never zero).
//!
//! The freezer:
//!
//! 1. takes `phase` from `Running` to `Freezing`, publishes `freezer`,
//!    then `hold` (`Release`);
//! 2. asks the guests that are running to stop, which ends each at its
//!    next function entry or loop header and returns it to the executor
//!    (the caller supplies that, because the scheduler knows nothing of
//!    the runtime that runs guests);
//! 3. wakes every secondary, whatever it is doing: a secondary that is
//!    running reads `hold` at its next boundary, and one that is idle,
//!    polling or parked is brought back to its run loop. The wake is
//!    unconditional, and does not consult the idle state, because a
//!    processor that is polling has not published `Parked` and a wake it
//!    is owed would otherwise be skipped;
//! 4. awaits, with the wait armed before each test of the arrival words,
//!    until every secondary's word equals `hold`, or until the arrival
//!    deadline the caller gave passes. The freeze does not decide how
//!    long a processor may take: a wall-clock bound cannot tell a
//!    descheduled virtual processor from a task that does not yield, and
//!    neither can a progress counter, so the caller's policy (what it
//!    knows of its host, and what a failed freeze costs it) sets the
//!    bound. A secondary that has not arrived by then fails the freeze,
//!    which names the lowest such processor: `hold` is cleared, the
//!    secondaries that did arrive are woken, `phase` returns to
//!    `Running`, and nothing stays held;
//! 5. after its last await, and from there with no await until it
//!    thaws, narrows the interrupt mask of its own processor to the wake
//!    and shootdown IPIs ([`Cpu::mask_to_wake_only`], the mask the
//!    secondaries park under), sets `phase` to `Frozen` and returns a
//!    [`FrozenMachine`]. The freezer's processor polls no task but the
//!    one that holds that value, because that task does not yield; the
//!    value is not `Send`, so it cannot be carried to another processor,
//!    and a task that yields while holding it breaks the contract (the
//!    processor's next boundary reports it). The freezer does not clear
//!    its interrupt flag: a device interrupt or timer tick is held off by
//!    the narrowed mask, and the flag stays set so that the freezer can
//!    change a mapping while it holds the machine, which broadcasts a TLB
//!    shootdown and must be able to take the one another processor would
//!    send (the x86 backend refuses to broadcast with the flag clear).
//!
//! A secondary reaches a boundary before every attempt to pop a task in
//! `Executor::run_until_stalled_with_stats` (so also on a pass that finds
//! its queues empty, and between the tasks of a batch) and before the
//! root future is polled in `Kernel::run_local_future`
//! (`FreezeBarrier::checkpoint`). A pass that fires a timer before it
//! reaches the boundary does so while the freeze is still `Freezing`:
//! the freezer counts the processor as arrived only once it has parked,
//! so nothing fires after the machine is `Frozen`. At a boundary it reads
//! `hold` with one `Relaxed` load and, when it is set,
//!
//! 1. masks interrupts on its own processor, so that from here to the
//!    end of the freeze no handler can run (a handler is what a driver's
//!    interrupt path is);
//! 2. publishes its arrival word and broadcasts the freezer's wait, then
//!    wakes the freezer's processor. That wake is as unconditional as the
//!    freezer's: the freezer may be parked, and the notification wakes
//!    its task through the scheduler, which wakes a processor only when
//!    it has published `Parked`;
//! 3. parks in [`Cpu::park_wake_only`], which narrows the interrupt mask
//!    to the wake IPI and the TLB-shootdown IPI before it waits on
//!    `hold`.
//!
//! The arrival is published before the processor parks but after its
//! interrupts are masked, so there is no instant at which the freezer
//! counts it and a handler can still run on it; and the wait is entered
//! with the mask already narrowed, so no device interrupt is taken while
//! the processor waits. A secondary leaves only when `hold` changes. It
//! then clears its arrival word, so a later freeze never counts it as
//! arrived before it has parked.
//!
//! The freezer thaws (`FrozenMachine` dropped, or a failed freeze) in
//! order: `phase` to `Thawing`, `hold` to its released value, a wake to
//! every secondary, and then a wait, a hardware-synchronisation spin
//! bounded by IPI latency, until no secondary's arrival word still equals
//! the held value, so that each has left its park. Last, `phase` returns
//! to `Running` and the freezer's own mask is lifted.
//!
//! No wake is lost:
//!
//! * freezer to secondary: `hold` is stored `Release` before the wake,
//!   and every secondary tests it `Acquire` after its masks are in
//!   place, so a secondary either sees the new value or has the wake
//!   pending when it waits (each backend's `park_wake_only` states how);
//!   an IPI that finds the secondary running is taken as an interrupt and
//!   changes nothing, because the boundary reads `hold` itself;
//! * secondary to freezer: the arrival word is stored `Release` before the
//!   broadcast, the freezer's wait is armed before it reads the words,
//!   and the wake to the freezer's processor is sent after, so the
//!   freezer is either woken or finds the word on its next read;
//! * release: `hold` changes before the wake, the park tests it after
//!   every wake, so a secondary that has not yet parked returns at once.
//!
//! Work queued while the machine is frozen is queued and nothing more: a
//! processor whose run loop has published nothing but `Running` is never
//! sent a scheduler wake for it, and every processor drains its queues
//! when it leaves the freeze.

use alloc::boxed::Box;
use core::future::poll_fn;
use core::marker::PhantomData;
use core::pin::pin;
use core::sync::atomic::{AtomicU8, AtomicU16, Ordering};
use core::task::Poll;
use core::time::Duration;

use crossbeam_utils::CachePadded;
use helios_hal::cpu::{Cpu, ProcessorId, current_processor};
use helios_hal::critical_section::LocalInterruptsMasked;
use helios_hal::watchdog::{NoWatchdog, Watchdog};
use triomphe::Arc as NoWeakArc;

use crate::exec::executor::{Executor, ExecutorGroup};
use crate::exec::sync::Notify;
use crate::exec::timer::Timer;

/// `hold` is held while this bit is set.
const HELD: u8 = 1;

/// The arrival word of a processor that is not parked against a freeze.
const NOT_ARRIVED: u8 = 0;

/// Freezes in `hold`'s high seven bits.
const GENERATIONS: u8 = 1 << 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Phase {
    Running = 0,
    Freezing = 1,
    Frozen = 2,
    Thawing = 3,
}

impl Phase {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Running,
            1 => Self::Freezing,
            2 => Self::Frozen,
            3 => Self::Thawing,
            _ => panic!("freeze phase {value} was never published"),
        }
    }
}

/// Why a freeze did not happen. The machine is running, every processor
/// released, in both cases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FreezeError {
    /// Another freeze holds the machine, or is releasing it.
    #[error("the machine is already being frozen")]
    AlreadyFreezing,
    /// A processor had not reached a scheduler boundary within the
    /// arrival deadline the caller gave. It may be inside a task that does
    /// not yield or simply not scheduled by its host; the freeze cannot
    /// tell the two apart, and the caller's deadline is its policy for
    /// both.
    #[error(
        "processor {} did not reach a scheduler boundary within the caller's arrival deadline",
        processor.id()
    )]
    ProcessorDidNotArrive {
        /// The lowest-numbered processor that had not arrived.
        processor: ProcessorId,
    },
}

/// The words every processor shares for a freeze. The contract is in the
/// module documentation.
pub(crate) struct FreezeBarrier {
    control: CachePadded<Control>,
    arrivals: Box<[CachePadded<AtomicU8>]>,
    /// Broadcast by a secondary after it publishes its arrival; the
    /// freezer is the only waiter.
    arrived: Notify,
}

/// The freezer's words, on one line: read by every processor at every
/// boundary, written only by the freezer.
struct Control {
    phase: AtomicU8,
    hold: AtomicU8,
    freezer: AtomicU16,
}

impl FreezeBarrier {
    pub(crate) fn new(processors: usize) -> Self {
        Self {
            control: CachePadded::new(Control {
                phase: AtomicU8::new(Phase::Running as u8),
                hold: AtomicU8::new(0),
                freezer: AtomicU16::new(0),
            }),
            arrivals: (0..processors)
                .map(|_| CachePadded::new(AtomicU8::new(NOT_ARRIVED)))
                .collect(),
            arrived: Notify::new(),
        }
    }

    /// Whether a freeze holds the machine. The run loop's whole cost: one
    /// `Relaxed` load. A boundary that misses a freeze published a moment
    /// ago takes it at the next one, which is within a task, and the
    /// freezer's wake brings an idle processor back to its loop.
    #[inline(always)]
    pub(crate) fn requested(&self) -> bool {
        self.control.hold.load(Ordering::Relaxed) & HELD != 0
    }

    /// Whether a freeze holds the machine and `processor` is not the one
    /// that asked: the idle loop's reason to return to its run loop.
    pub(crate) fn requested_of(&self, processor: ProcessorId) -> bool {
        // `hold` is loaded `Acquire` and `freezer` after it, as `arrive`
        // does: the freezer stores `freezer` before it releases `hold`,
        // so a `hold` that is seen is paired with its own `freezer`, not
        // with the previous freeze's.
        self.control.hold.load(Ordering::Acquire) & HELD != 0
            && self.control.freezer.load(Ordering::Relaxed) != processor.id()
    }

    /// A scheduler boundary: the processor polls no task, fires no timer
    /// and holds nothing a handler needs. A secondary parks here for as
    /// long as a freeze holds it.
    #[inline(always)]
    pub(crate) fn checkpoint<CpuImpl: Cpu>(&self, cpu: &CpuImpl) {
        if self.requested() {
            self.arrive(cpu);
        }
    }

    #[cold]
    #[inline(never)]
    fn arrive<CpuImpl: Cpu>(&self, cpu: &CpuImpl) {
        let processor = current_processor();
        let hold = &self.control.hold;
        let mut held = hold.load(Ordering::Acquire);
        if held & HELD == 0 {
            return;
        }
        if self.control.freezer.load(Ordering::Relaxed) == processor.id() {
            self.freezer_boundary();
            return;
        }
        // From the arrival to the end of the freeze no handler runs here.
        let _masked = LocalInterruptsMasked::new();
        let arrival = &self.arrivals[usize::from(processor.id())];
        loop {
            let freezer = ProcessorId::new(self.control.freezer.load(Ordering::Relaxed));
            arrival.store(held, Ordering::Release);
            self.arrived.notify_all();
            cpu.wake_processor(freezer);
            cpu.park_wake_only(hold, held);
            arrival.store(NOT_ARRIVED, Ordering::Release);
            held = hold.load(Ordering::Acquire);
            if held & HELD == 0 {
                return;
            }
        }
    }

    /// The freezer's own processor reached a boundary. While it awaits
    /// its secondaries it runs ordinary tasks, and passes. Once
    /// `Frozen` the task that holds the machine has yielded, which the
    /// contract forbids.
    #[cold]
    fn freezer_boundary(&self) {
        if Phase::from_u8(self.control.phase.load(Ordering::Acquire)) == Phase::Frozen {
            tracing::error!(
                processor = current_processor().id(),
                "a task yielded while the machine was frozen; the freezer's processor is \
                 polling tasks again with every other processor parked"
            );
            debug_assert!(
                false,
                "a task yielded while holding the frozen machine; the machine must be thawed \
                 before the freezer's task awaits"
            );
        }
    }

    /// The lowest-numbered secondary that has not parked against `held`.
    fn first_missing(&self, freezer: ProcessorId, held: u8) -> Option<ProcessorId> {
        self.arrivals
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != usize::from(freezer.id()))
            .find(|(_, arrival)| arrival.load(Ordering::Acquire) != held)
            .map(|(index, _)| {
                ProcessorId::new(u16::try_from(index).expect("processor index fits a ProcessorId"))
            })
    }

    /// Releases the secondaries, in the order of the module
    /// documentation, and returns once every one has left its park.
    fn release<CpuImpl: Cpu>(&self, cpu: &CpuImpl, freezer: ProcessorId, held: u8) {
        self.control
            .phase
            .store(Phase::Thawing as u8, Ordering::Release);
        self.control.hold.store(held & !HELD, Ordering::Release);
        for index in 0..self.arrivals.len() {
            if index != usize::from(freezer.id()) {
                cpu.wake_processor(processor_at(index));
            }
        }
        for (index, arrival) in self.arrivals.iter().enumerate() {
            if index == usize::from(freezer.id()) {
                continue;
            }
            // A secondary that parked is a few instructions from leaving
            // once `hold` changes; one that never arrived is not waited
            // for, and finds `hold` released when it does.
            while arrival.load(Ordering::Acquire) == held {
                core::hint::spin_loop();
            }
        }
        self.control
            .phase
            .store(Phase::Running as u8, Ordering::Release);
    }
}

fn processor_at(index: usize) -> ProcessorId {
    ProcessorId::new(u16::try_from(index).expect("processor index fits a ProcessorId"))
}

/// A freeze that has set `hold` and has not released it: what a dropped
/// future or a returned guard releases.
struct Hold<CpuImpl: Cpu + Clone> {
    group: NoWeakArc<ExecutorGroup>,
    cpu: CpuImpl,
    freezer: ProcessorId,
    held: u8,
}

impl<CpuImpl: Cpu + Clone> Drop for Hold<CpuImpl> {
    fn drop(&mut self) {
        self.group
            .freeze
            .release(&self.cpu, self.freezer, self.held);
    }
}

/// The machine, held still. Every processor but this one is parked at a
/// scheduler boundary with its interrupts held off, no task is polled on
/// any of them, no timer fires, and this processor takes only the wake
/// and TLB-shootdown IPIs ([`Cpu::mask_to_wake_only`]): the caller works
/// alone until it drops the value, and may change a mapping, which
/// broadcasts a shootdown the parked processors acknowledge.
///
/// Dropping it, or [`FrozenMachine::thaw`], releases the machine. The
/// value is `!Send` and the work done while it is held is synchronous:
/// the freezer's processor polls no task but the one that holds it, so
/// that task must not yield.
///
/// # The hardware watchdog
///
/// The kernel's watchdog supervisor is a task, and no task but the
/// holder's runs while the machine is frozen, so the watchdog is not
/// petted for as long as the machine is held. A holder that keeps the
/// machine frozen for longer than the watchdog's timeout (writing a
/// hibernation image of a large guest is the case) must pet it with
/// [`FrozenMachine::pet_watchdog`], the operation the supervisor
/// performs, at least once per timeout, or the hardware resets the
/// machine.
#[must_use = "the machine stays frozen until this is dropped"]
pub struct FrozenMachine<CpuImpl: Cpu + Clone, WatchdogImpl: Watchdog = NoWatchdog> {
    // Field order is the thaw order: the secondaries are released first,
    // and this processor's mask is lifted last.
    _hold: Hold<CpuImpl>,
    _mask: CpuImpl::WakeOnlyMask,
    /// The mask is this processor's register state, whatever a backend's
    /// own type for it says.
    _processor_local: PhantomData<*mut ()>,
    watchdog: WatchdogImpl,
}

impl<CpuImpl: Cpu + Clone, WatchdogImpl: Watchdog> FrozenMachine<CpuImpl, WatchdogImpl> {
    /// Releases every processor and lifts this processor's mask.
    pub fn thaw(self) {
        drop(self);
    }

    /// Pets the hardware watchdog, as the kernel's watchdog supervisor
    /// does while the machine runs. See the type's documentation.
    pub fn pet_watchdog(&self) {
        self.watchdog.pet();
    }
}

impl Executor {
    /// Freezes every processor at a scheduler boundary.
    ///
    /// `timer` is this processor's, `watchdog` is the one the answer lets
    /// its holder pet, `arrival_deadline` is how long the caller lets the
    /// other processors take to reach a boundary, and
    /// `interrupt_guests` asks the runtime that runs guests to end the
    /// time slice of each guest that is running now (it is how a
    /// processor inside wasm reaches a boundary before its slice ends).
    /// The contract is in the module documentation.
    ///
    /// # Errors
    ///
    /// [`FreezeError::AlreadyFreezing`] when another freeze holds the
    /// machine, and [`FreezeError::ProcessorDidNotArrive`] when a
    /// processor has not reached a boundary within `arrival_deadline`.
    /// The machine is running in both cases.
    ///
    /// The future is `!Send`: it records the processor it first runs on
    /// as the freezer, and runs only there.
    pub async fn freeze<CpuImpl: Cpu + Clone, WatchdogImpl: Watchdog>(
        &self,
        timer: &Timer<CpuImpl>,
        watchdog: WatchdogImpl,
        arrival_deadline: Duration,
        interrupt_guests: impl FnOnce(),
    ) -> Result<FrozenMachine<CpuImpl, WatchdogImpl>, FreezeError> {
        // Held for the whole future, and moved into the answer at its
        // end, so that the future is `!Send` (a value that nothing uses
        // later is not kept across an await, and would not count): the
        // freezer is the processor that polls this first, and its task
        // must stay on that processor's own queue (`spawn_local`), never
        // in the global queue another processor could pop it from.
        let processor_local = PhantomData::<*mut ()>;
        let barrier = &self.group.freeze;
        let cpu = timer.cpu();
        let freezer = current_processor();

        barrier
            .control
            .phase
            .compare_exchange(
                Phase::Running as u8,
                Phase::Freezing as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| FreezeError::AlreadyFreezing)?;
        let generation = ((barrier.control.hold.load(Ordering::Relaxed) >> 1) + 1) % GENERATIONS;
        let held = (generation << 1) | HELD;
        barrier
            .control
            .freezer
            .store(freezer.id(), Ordering::Relaxed);
        barrier.control.hold.store(held, Ordering::Release);
        // From here a drop, whether of the guard or of this future,
        // releases the machine.
        let hold = Hold {
            group: self.group.clone(),
            cpu: cpu.clone(),
            freezer,
            held,
        };

        interrupt_guests();
        for index in 0..barrier.arrivals.len() {
            if index != usize::from(freezer.id()) {
                cpu.wake_processor(processor_at(index));
            }
        }

        let deadline = cpu
            .now()
            .saturating_add(cpu.timebase().duration_to_ticks(arrival_deadline));
        let mut expiry = pin!(timer.sleep_until(deadline));
        let mut arrival = barrier.arrived.waiter();
        let arrived = poll_fn(|context| {
            loop {
                let Some(missing) = barrier.first_missing(freezer, held) else {
                    return Poll::Ready(Ok(()));
                };
                if expiry.as_mut().poll(context).is_ready() {
                    return Poll::Ready(Err(FreezeError::ProcessorDidNotArrive {
                        processor: missing,
                    }));
                }
                // The waiter is armed before `first_missing` reads, and
                // re-armed each time it completes.
                match barrier.arrived.poll_notified(context, &mut arrival) {
                    Poll::Ready(()) => {}
                    Poll::Pending => return Poll::Pending,
                }
            }
        })
        .await;
        arrived?;

        // No await from here: this processor polls nothing else until the
        // guard is dropped.
        let mask = cpu.mask_to_wake_only();
        barrier
            .control
            .phase
            .store(Phase::Frozen as u8, Ordering::Release);
        Ok(FrozenMachine {
            _hold: hold,
            _mask: mask,
            _processor_local: processor_local,
            watchdog,
        })
    }

    /// A scheduler boundary of this processor's run loop; see
    /// [`FreezeBarrier::checkpoint`].
    #[inline(always)]
    pub(crate) fn freeze_checkpoint<CpuImpl: Cpu + Clone>(&self, cpu: &CpuImpl) {
        self.group.freeze.checkpoint(cpu);
    }

    /// Whether a freeze that this processor did not ask for is holding
    /// the machine.
    pub(crate) fn freeze_requested_of_this_processor(&self) -> bool {
        self.group.freeze.requested_of(self.owner_processor)
    }
}
