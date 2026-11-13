//! Which processors are online, and the ordered steps that bring one up
//! and take one down.
//!
//! # The sets
//!
//! The possible set is every processor the machine can ever run,
//! [`Cpu::processor_count`]: a platform fact, fixed at boot, and the
//! size of every per-processor structure in the kernel, so bringing a
//! processor online never grows one. The online set ([`OnlineSet`]) is
//! the kernel's: a processor joins it once its own per-processor owners
//! are up, and leaves it before it stops. Every cross-processor wake and
//! fan-out addresses the online set only.
//!
//! # The lifecycle
//!
//! Each per-processor owner implements a [`ProcessorStep`]: its half of
//! bringing a processor up and of taking it down. The steps run in a
//! fixed order on bring-up — memory, timer base, executor, watchdog pet
//! — and in the reverse order on teardown, the way Linux's cpuhp states
//! do. The order is the type of [`STEPS`]: a nested pair runs its first
//! step before its second on the way up and after it on the way down,
//! so the reverse order is not something a caller can get wrong.
//!
//! A teardown step may refuse, and refuses before it changes anything.
//! The steps already torn down are then brought back up in order, and
//! the processor stays online: an offline either completes or leaves the
//! machine as it was.
//!
//! # Concurrency contract
//!
//! - Transitions are serialised. The initiator holds
//!   `ProcessorLifecycles::transition`, the kernel's async mutex, for
//!   the whole of one offline or online; at most one processor is ever
//!   between states, so the processor a departing one hands its timers
//!   to cannot itself be departing. Nothing on a hot path takes it.
//! - The initiator is a task on any online processor other than the
//!   target ([`ProcessorControl`]). It publishes the request in the
//!   target's lifecycle cell, wakes the target if it has parked, and
//!   awaits the cell's `changed` signal, armed before every test of
//!   the state.
//! - Every step runs on the target itself, at the top of its run loop
//!   (`Kernel::run_until_stalled_with_stats`), holding no lock and with
//!   no task mid-poll. A step therefore touches its own processor's
//!   owner-only state without synchronisation. Teardown ends in
//!   [`Cpu::stop_current`]; when that returns, bring-up runs in order
//!   and the processor rejoins the online set last.
//! - The online set and the lifecycle cells are atomics and lock-free.
//!   The target reads its own cell once per run-loop pass and in the
//!   park handshake; the request store and that read are `SeqCst`, as
//!   in `idle`'s handshake, so a parked target is either woken or sees
//!   the request before it parks.
//! - An offline fans out to one other processor: the successor, the
//!   processor the initiator ran on when it took the transition, which
//!   takes over the departing processor's pending timers and is woken
//!   for any global work already queued.
//! - A freeze (`freeze`'s module docs) and a transition exclude each
//!   other through the same lock: the freezer takes it before it holds
//!   any processor and keeps it until the thaw is done, so a freeze
//!   counts a settled online set and a transition never runs a step
//!   against a parked processor. A transition requested during a freeze
//!   waits for the thaw; the holder of a [`crate::FrozenMachine`] cannot
//!   request one, since it must not await. A processor's first bring-up
//!   at boot is the one join outside the lock, and it ends at the
//!   freeze's join checkpoint, which holds it when a freeze has already
//!   counted the online set without it.

extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU8, AtomicU16, AtomicU64, AtomicUsize, Ordering};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::{Cpu, ProcessorId, current_processor};
use helios_hal::measured_boot::MeasuredBoot;
use helios_hal::watchdog::Watchdog;

use super::executor::{ExecutorGroup, PinnedTasks, machine_executor_group};
use super::sync::{Notify, RawMutex, RawMutexLease};
use crate::Kernel;

/// The kernel's online set: one bit per possible processor.
///
/// Written only by a processor joining or leaving it, from its own
/// lifecycle step; read by every cross-processor wake. All accesses are
/// `SeqCst`, because the executor's global-work handoff rests on a
/// departing processor's removal and a waker's read of the set falling
/// in one total order with the global ready count (see
/// `Executor::take_offline`).
pub struct OnlineSet {
    words: Box<[AtomicU64]>,
    count: AtomicUsize,
}

impl OnlineSet {
    pub(crate) fn new(possible_processors: usize) -> Self {
        Self {
            words: (0..possible_processors.div_ceil(64))
                .map(|_| AtomicU64::new(0))
                .collect(),
            count: AtomicUsize::new(0),
        }
    }

    fn word_and_bit(&self, processor: ProcessorId) -> (&AtomicU64, u64) {
        let index = usize::from(processor.id());
        let word = self.words.get(index / 64).unwrap_or_else(|| {
            panic!(
                "processor {} is outside the online set's {} possible processors",
                processor.id(),
                self.words.len() * 64
            )
        });
        (word, 1 << (index % 64))
    }

    pub(crate) fn insert(&self, processor: ProcessorId) {
        let (word, bit) = self.word_and_bit(processor);
        let previous = word.fetch_or(bit, Ordering::SeqCst);
        assert!(
            previous & bit == 0,
            "processor {} joined the online set while already in it",
            processor.id()
        );
        self.count.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn remove(&self, processor: ProcessorId) {
        let (word, bit) = self.word_and_bit(processor);
        let previous = word.fetch_and(!bit, Ordering::SeqCst);
        assert!(
            previous & bit != 0,
            "processor {} left the online set without being in it",
            processor.id()
        );
        self.count.fetch_sub(1, Ordering::SeqCst);
    }

    /// Whether `processor` is online.
    pub fn contains(&self, processor: ProcessorId) -> bool {
        let (word, bit) = self.word_and_bit(processor);
        word.load(Ordering::SeqCst) & bit != 0
    }

    /// How many processors are online.
    pub fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// The online processor `start` places among the online processors
    /// other than `exclude`, or `None` when there is none.
    ///
    /// The words are read twice — once to count, once to select — so a
    /// transition between the reads can make the selection come up
    /// empty. That is a transition's own window, and the departing
    /// processor's handoff wake covers the work a lost pick was for.
    pub(crate) fn nth_after(&self, start: usize, exclude: ProcessorId) -> Option<ProcessorId> {
        let (exclude_word, exclude_bit) = self.word_and_bit(exclude);
        let candidates = |word: &AtomicU64| {
            let bits = word.load(Ordering::SeqCst);
            if core::ptr::eq(word, exclude_word) {
                bits & !exclude_bit
            } else {
                bits
            }
        };
        let total: usize = self
            .words
            .iter()
            .map(|word| candidates(word).count_ones() as usize)
            .sum();
        if total == 0 {
            return None;
        }
        let mut target = start % total;
        for (index, word) in self.words.iter().enumerate() {
            let mut bits = candidates(word);
            let ones = bits.count_ones() as usize;
            if target >= ones {
                target -= ones;
                continue;
            }
            for _ in 0..target {
                bits &= bits - 1;
            }
            let processor = index * 64 + bits.trailing_zeros() as usize;
            return Some(ProcessorId::new(u16::try_from(processor).unwrap_or_else(
                |_| panic!("online processor {processor} does not fit a processor identifier"),
            )));
        }
        None
    }
}

/// Where one processor stands in the lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum LifecycleState {
    /// Never brought up: a possible processor that has not run yet.
    Absent = 0,
    /// Running, in the online set.
    Online = 1,
    /// Asked to go offline; the processor runs its teardown next.
    Departing = 2,
    /// Torn down and stopped, or stopping.
    Offline = 3,
    /// Started or restarted; the processor runs its bring-up next.
    Arriving = 4,
}

impl LifecycleState {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Absent,
            1 => Self::Online,
            2 => Self::Departing,
            3 => Self::Offline,
            4 => Self::Arriving,
            _ => panic!("lifecycle state {value} was never published"),
        }
    }
}

/// One processor's lifecycle cell.
///
/// `state` is written by the initiator (a request) and by the processor
/// itself (a completion), and `changed` is raised on every completion.
/// `successor` and `refused_live` are written before the `state` store
/// that publishes them and read after the load that observes it.
struct LifecycleCell {
    state: AtomicU8,
    successor: AtomicU16,
    refused_live: AtomicUsize,
    changed: Notify,
}

impl LifecycleCell {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(LifecycleState::Absent as u8),
            successor: AtomicU16::new(0),
            refused_live: AtomicUsize::new(0),
            changed: Notify::new(),
        }
    }

    fn state(&self) -> LifecycleState {
        LifecycleState::from_u8(self.state.load(Ordering::SeqCst))
    }

    fn publish(&self, state: LifecycleState) {
        self.state.store(state as u8, Ordering::SeqCst);
        self.changed.notify_all();
    }

    /// Waits until the state is no longer `transient`, and returns it.
    async fn settled(&self, transient: LifecycleState) -> LifecycleState {
        loop {
            // Armed before the state is read: the processor publishes
            // and then broadcasts, and a broadcast reaches only waits
            // that already exist.
            let changed = self.changed.notified();
            let state = self.state();
            if state != transient {
                return state;
            }
            changed.await;
        }
    }
}

/// Every processor's lifecycle cell, and the lock that serialises
/// transitions.
pub(crate) struct ProcessorLifecycles {
    cells: Box<[CachePadded<LifecycleCell>]>,
    transition: RawMutex,
}

impl ProcessorLifecycles {
    pub(crate) fn new(possible_processors: usize) -> Self {
        Self {
            cells: (0..possible_processors)
                .map(|_| CachePadded::new(LifecycleCell::new()))
                .collect(),
            transition: RawMutex::new(),
        }
    }

    fn cell(&self, processor: ProcessorId) -> &LifecycleCell {
        self.cells
            .get(usize::from(processor.id()))
            .unwrap_or_else(|| {
                panic!(
                    "processor {} is outside the {} possible processors' lifecycle cells",
                    processor.id(),
                    self.cells.len()
                )
            })
    }

    /// Takes the lock that serialises transitions. An offline or online
    /// holds it from its request to the target's settled state, and a
    /// freeze holds it from before it sets `hold` until its thaw is done.
    pub(crate) async fn lock_transitions(&self) -> RawMutexLease<'_> {
        self.transition.lock().await
    }

    /// Whether `processor` has been asked to go offline. `SeqCst`: the
    /// parker half of the request handshake, read after the processor
    /// has published `Parked`.
    pub(crate) fn departure_requested(&self, processor: ProcessorId) -> bool {
        self.cell(processor).state() == LifecycleState::Departing
    }
}

/// Why a processor refused to go offline. The processor stays online
/// and nothing it owns has changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OfflineRefusal {
    /// Local tasks still run on it: their futures are not `Send`, so
    /// they cannot move to another processor.
    #[error(transparent)]
    PinnedTasks(#[from] PinnedTasks),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OfflineError {
    #[error(
        "processor {} is not one of the {possible} possible processors",
        processor.id()
    )]
    NotPossible {
        processor: ProcessorId,
        possible: usize,
    },
    #[error("processor {} is the bootstrap processor, which stays online", processor.id())]
    Bootstrap { processor: ProcessorId },
    #[error(
        "processor {} cannot take itself offline: another online processor initiates it",
        processor.id()
    )]
    CurrentProcessor { processor: ProcessorId },
    #[error("processor {} is not online", processor.id())]
    NotOnline { processor: ProcessorId },
    #[error(transparent)]
    Refused(#[from] OfflineRefusal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OnlineError {
    #[error(
        "processor {} is not one of the {possible} possible processors",
        processor.id()
    )]
    NotPossible {
        processor: ProcessorId,
        possible: usize,
    },
    #[error("processor {} is not offline", processor.id())]
    NotOffline { processor: ProcessorId },
}

/// The processor lifecycle as a system capability: the possible and
/// online sets, and the two transitions between them.
///
/// The kernel's services reach the lifecycle through this trait, and
/// `helios:system/processors` is its WIT face.
///
/// # Concurrency contract
///
/// The set reads are lock-free atomic loads, callable from any
/// processor; a read concurrent with a transition sees the set before
/// or after it. `offline` and `online` take the kernel's async
/// transition mutex for the whole transition and fan out to exactly the
/// target processor (and, for an offline, the successor the call runs
/// on), so concurrent requests from any number of processors are
/// serialised and each one's result describes its own transition.
pub trait ProcessorLifecycle {
    /// How many processors the machine can ever run: the possible set
    /// is `0..possible_processors()`.
    fn possible_processors(&self) -> usize;

    /// The processor that booted the machine, which stays online.
    fn bootstrap_processor(&self) -> ProcessorId;

    /// Whether `processor`, one of the possible processors, is online.
    fn is_online(&self, processor: ProcessorId) -> bool;

    /// Takes `processor` offline: it runs its teardown steps, hands its
    /// pending timers and queued global work to the processor the call
    /// runs on, and stops. Resolves once it has stopped, or with why it
    /// did not; a refusal leaves it online and unchanged.
    fn offline(
        &self,
        processor: ProcessorId,
    ) -> impl Future<Output = Result<(), OfflineError>> + Send + '_;

    /// Brings `processor` online, starting it when it has never run.
    /// Resolves once it has run its bring-up and joined the online set.
    fn online(
        &self,
        processor: ProcessorId,
    ) -> impl Future<Output = Result<(), OnlineError>> + Send + '_;
}

/// The initiator's side of the lifecycle: takes processors offline and
/// brings them online from a task on any online processor.
#[derive(Clone)]
pub struct ProcessorControl<CpuImpl: Cpu + Clone> {
    cpu: CpuImpl,
    group: &'static ExecutorGroup,
}

impl<CpuImpl: Cpu + Clone> ProcessorControl<CpuImpl> {
    pub(crate) fn new(cpu: CpuImpl, group: &'static ExecutorGroup) -> Self {
        Self { cpu, group }
    }

    /// The online set, live.
    pub fn online_set(&self) -> &OnlineSet {
        self.group.online()
    }

    fn possible(&self, processor: ProcessorId) -> bool {
        usize::from(processor.id()) < self.group.possible_processors()
    }
}

impl<CpuImpl: Cpu + Clone> ProcessorLifecycle for ProcessorControl<CpuImpl> {
    fn possible_processors(&self) -> usize {
        self.group.possible_processors()
    }

    fn bootstrap_processor(&self) -> ProcessorId {
        self.cpu.bootstrap_processor()
    }

    fn is_online(&self, processor: ProcessorId) -> bool {
        self.online_set().contains(processor)
    }

    /// Takes `processor` offline through [`Cpu::stop_current`].
    ///
    /// Refused with [`OfflineError::Refused`] when the processor still
    /// runs work pinned to it, which leaves it online and unchanged.
    async fn offline(&self, processor: ProcessorId) -> Result<(), OfflineError> {
        if !self.possible(processor) {
            return Err(OfflineError::NotPossible {
                processor,
                possible: self.group.possible_processors(),
            });
        }
        if processor == self.cpu.bootstrap_processor() {
            return Err(OfflineError::Bootstrap { processor });
        }
        let _transition = self.group.lifecycle().lock_transitions().await;
        // Read after the lock: the task may have resumed elsewhere.
        let successor = current_processor();
        if successor == processor {
            return Err(OfflineError::CurrentProcessor { processor });
        }
        let cell = self.group.lifecycle().cell(processor);
        if cell.state() != LifecycleState::Online {
            return Err(OfflineError::NotOnline { processor });
        }
        cell.successor.store(successor.id(), Ordering::Relaxed);
        // `SeqCst` store, then the target's idle state: the requester
        // half of the handshake `departure_requested` describes.
        cell.publish(LifecycleState::Departing);
        if self.group.needs_wake(usize::from(processor.id())) {
            self.cpu.wake_processor(processor);
        }
        match cell.settled(LifecycleState::Departing).await {
            LifecycleState::Offline => Ok(()),
            LifecycleState::Online => Err(OfflineError::Refused(OfflineRefusal::PinnedTasks(
                PinnedTasks {
                    processor,
                    live: cell.refused_live.load(Ordering::Relaxed),
                },
            ))),
            state => panic!(
                "processor {} left Departing for {state:?}, which no teardown publishes",
                processor.id()
            ),
        }
    }

    /// Restarts `processor` when it was taken offline and starts it
    /// when it has never run.
    async fn online(&self, processor: ProcessorId) -> Result<(), OnlineError> {
        if !self.possible(processor) {
            return Err(OnlineError::NotPossible {
                processor,
                possible: self.group.possible_processors(),
            });
        }
        let _transition = self.group.lifecycle().lock_transitions().await;
        let cell = self.group.lifecycle().cell(processor);
        match cell.state() {
            LifecycleState::Absent => {
                cell.publish(LifecycleState::Arriving);
                self.cpu.start_processor(processor);
            }
            LifecycleState::Offline => {
                cell.publish(LifecycleState::Arriving);
                self.cpu.restart_processor(processor);
            }
            LifecycleState::Online | LifecycleState::Departing | LifecycleState::Arriving => {
                return Err(OnlineError::NotOffline { processor });
            }
        }
        match cell.settled(LifecycleState::Arriving).await {
            LifecycleState::Online => Ok(()),
            state => panic!(
                "processor {} left Arriving for {state:?}, which no bring-up publishes",
                processor.id()
            ),
        }
    }
}

/// How many processors are online, or zero before the first processor
/// has built its executor — when none is.
pub fn online_processor_count() -> usize {
    machine_executor_group().map_or(0, |group| group.online().count())
}

/// One per-processor owner's half of the lifecycle.
///
/// Both halves run on the processor whose state they bring up or tear
/// down. `offline` either refuses with nothing changed or completes.
pub(crate) trait ProcessorStep {
    fn online<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
    ) where
        CpuImpl: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot;

    fn offline<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
        successor: ProcessorId,
    ) -> Result<(), OfflineRefusal>
    where
        CpuImpl: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot;
}

/// `First` comes up before `Then` and goes down after it; a refusal by
/// `First` brings `Then` back up.
impl<First: ProcessorStep, Then: ProcessorStep> ProcessorStep for (First, Then) {
    fn online<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
    ) where
        CpuImpl: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        self.0.online(kernel);
        self.1.online(kernel);
    }

    fn offline<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
        successor: ProcessorId,
    ) -> Result<(), OfflineRefusal>
    where
        CpuImpl: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        self.1.offline(kernel, successor)?;
        self.0.offline(kernel, successor).inspect_err(|_| {
            self.1.online(kernel);
        })
    }
}

/// The frames a processor holds for itself: its page-fault reserve and
/// its slab shard. Coming up needs nothing — the run loop refills the
/// reserve and frees refill the shard — and going down returns both to
/// the pool, since a stopped processor allocates nothing.
struct MemoryStep;

/// The processor's timer base: armed on the way up, its pending sleeps
/// handed to the successor on the way down.
struct TimerStep;

/// The processor's executor: joins the online set on the way up; on the
/// way down runs what is queued locally, refuses while local tasks are
/// alive, leaves the online set and hands queued global work over.
struct ExecutorStep;

/// The watchdog pet the processor runs as a local task: spawned on the
/// way up, cancelled on the way down so it is not pinned work.
struct WatchdogStep;

/// Every per-processor owner, in bring-up order.
const STEPS: (MemoryStep, (TimerStep, (ExecutorStep, WatchdogStep))) =
    (MemoryStep, (TimerStep, (ExecutorStep, WatchdogStep)));

impl ProcessorStep for MemoryStep {
    fn online<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
        &self,
        _kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
    ) where
        CpuImpl: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
    }

    fn offline<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
        _successor: ProcessorId,
    ) -> Result<(), OfflineRefusal>
    where
        CpuImpl: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        crate::memory::release_processor_frames(kernel.executor.owner_processor());
        Ok(())
    }
}

impl ProcessorStep for TimerStep {
    fn online<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
    ) where
        CpuImpl: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        kernel.timer.bring_base_online();
    }

    fn offline<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
        successor: ProcessorId,
    ) -> Result<(), OfflineRefusal>
    where
        CpuImpl: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        kernel.timer.migrate_base(successor);
        Ok(())
    }
}

impl ProcessorStep for ExecutorStep {
    fn online<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
    ) where
        CpuImpl: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        kernel
            .executor
            .group()
            .bring_processor_online(kernel.executor.owner_processor());
    }

    fn offline<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
        successor: ProcessorId,
    ) -> Result<(), OfflineRefusal>
    where
        CpuImpl: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        kernel.timer.arm_scheduler_tick();
        kernel.executor.take_offline(&kernel.cpu, successor)?;
        Ok(())
    }
}

impl ProcessorStep for WatchdogStep {
    fn online<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
    ) where
        CpuImpl: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        let pet = kernel.spawn_watchdog_pet();
        let previous = kernel.watchdog_pet.replace(pet);
        assert!(
            previous.is_none(),
            "processor {} brought its watchdog pet up twice",
            kernel.executor.owner_processor().id()
        );
    }

    fn offline<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
        &self,
        kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
        _successor: ProcessorId,
    ) -> Result<(), OfflineRefusal>
    where
        CpuImpl: Cpu + Clone,
        WatchdogImpl: Watchdog + Clone,
        MeasuredBootImpl: MeasuredBoot,
    {
        // Dropping the handle cancels the task; its owner's executor
        // drops the future when the executor step runs what is queued.
        drop(kernel.watchdog_pet.take());
        Ok(())
    }
}

impl<CpuImpl: Cpu + Clone, WatchdogImpl: Watchdog + Clone, MeasuredBootImpl: MeasuredBoot>
    Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>
{
    /// The initiator's handle on the processor lifecycle.
    pub fn processor_control(&self) -> ProcessorControl<CpuImpl> {
        ProcessorControl::new(self.cpu.clone(), self.executor.group())
    }

    /// This processor's first bring-up, from `init`: every step, in
    /// order, then the processor is online.
    pub(crate) fn bring_up(&self) {
        let cell = self
            .executor
            .group()
            .lifecycle()
            .cell(self.executor.owner_processor());
        let state = cell.state();
        assert!(
            matches!(state, LifecycleState::Absent | LifecycleState::Arriving),
            "processor {} ran its first bring-up from {state:?}",
            self.executor.owner_processor().id()
        );
        STEPS.online(self);
        cell.publish(LifecycleState::Online);
        // This first bring-up is the one join no transition lock covers:
        // a freeze that began while it ran either counts this processor
        // or holds it here, before it fires a timer or polls a task.
        self.executor.freeze_join_checkpoint(&self.cpu);
    }

    /// Runs a pending offline request for this processor: the teardown,
    /// the stop, and — once another processor restarts it — the
    /// bring-up. Called at the top of every run-loop pass, where the
    /// processor holds nothing.
    pub(crate) fn service_lifecycle(&self) {
        let processor = self.executor.owner_processor();
        let lifecycles = self.executor.group().lifecycle();
        if !lifecycles.departure_requested(processor) {
            return;
        }
        let cell = lifecycles.cell(processor);
        let successor = ProcessorId::new(cell.successor.load(Ordering::Relaxed));
        if let Err(refusal) = STEPS.offline(self, successor) {
            let OfflineRefusal::PinnedTasks(pinned) = refusal;
            cell.refused_live.store(pinned.live, Ordering::Relaxed);
            tracing::info!(
                processor = processor.id(),
                %refusal,
                "Processor refused to go offline"
            );
            cell.publish(LifecycleState::Online);
            return;
        }
        tracing::info!(
            processor = processor.id(),
            successor = successor.id(),
            "Processor offline"
        );
        cell.publish(LifecycleState::Offline);
        self.cpu.stop_current();
        let state = cell.state();
        assert_eq!(
            state,
            LifecycleState::Arriving,
            "processor {} returned from stop_current without a restart",
            processor.id()
        );
        STEPS.online(self);
        // No join checkpoint: the initiator holds the transition lock
        // until this publish, so no freeze has set `hold`, and one that
        // starts after it counts this processor, already online.
        cell.publish(LifecycleState::Online);
        tracing::info!(processor = processor.id(), "Processor back online");
    }
}

#[cfg(test)]
mod tests {
    use core::future::{Future as _, pending};
    use core::pin::Pin;
    use core::task::{Context, Poll};

    use futures::task::noop_waker_ref;
    use helios_hal::cpu::{Instant, ProcessorId};
    use helios_hal::watchdog::ProgressCounter;

    use super::OnlineSet;
    use crate::exec::Executor;
    use crate::exec::executor::PinnedTasks;
    use crate::test_processor_identity;
    use crate::test_support::{ManualClockCpu, RecordingSmpCpu};

    const P0: ProcessorId = ProcessorId::new(0);
    const P1: ProcessorId = ProcessorId::new(1);
    const P2: ProcessorId = ProcessorId::new(2);

    #[test]
    fn a_global_wake_picks_only_online_processors_other_than_the_waker() {
        let online = OnlineSet::new(70);
        for processor in [P0, P2, ProcessorId::new(65)] {
            online.insert(processor);
        }
        assert_eq!(online.count(), 3);

        let picks: alloc::vec::Vec<_> = (0..4).map(|start| online.nth_after(start, P0)).collect();
        assert_eq!(
            picks,
            [
                Some(P2),
                Some(ProcessorId::new(65)),
                Some(P2),
                Some(ProcessorId::new(65))
            ]
        );

        online.remove(P2);
        online.remove(ProcessorId::new(65));
        assert_eq!(online.nth_after(0, P0), None);
        assert!(!online.contains(P2));
        assert_eq!(online.count(), 1);
    }

    #[test]
    #[should_panic(expected = "processor 1 joined the online set while already in it")]
    fn joining_the_online_set_twice_is_a_lifecycle_bug() {
        let online = OnlineSet::new(2);
        online.insert(P1);
        online.insert(P1);
    }

    /// The executor's step refuses while a local task is alive on the
    /// departing processor, changes nothing when it does, and once the
    /// task is gone takes the processor out of the online set and hands
    /// queued global work to the successor by waking it.
    #[test]
    fn a_pinned_local_task_refuses_the_departure_until_it_is_gone() {
        let cpu = RecordingSmpCpu::new(0, 2);
        let executor0 = Executor::isolated(ProgressCounter::new(), 2, P0);
        let group = executor0.group();
        group.bring_processor_online(P0);

        test_processor_identity::set(P1);
        let executor1 = executor0.sibling(ProgressCounter::new(), P1);
        group.bring_processor_online(P1);
        let spawner1 = executor1.spawner(cpu.clone());
        let pinned = spawner1.spawn_local(pending::<()>());

        assert_eq!(
            executor1.take_offline(&cpu, P0),
            Err(PinnedTasks {
                processor: P1,
                live: 1
            })
        );
        assert!(group.online().contains(P1), "a refusal changes nothing");

        // Global work is queued while the successor is parked.
        spawner1.spawn_detached(async {});
        group.publish_parked(P0);
        let woken_before = cpu.woken().len();

        // Cancelling queues the task to be dropped on its owner, which
        // the step does before it counts.
        drop(pinned);
        assert_eq!(executor1.take_offline(&cpu, P0), Ok(()));
        assert!(!group.online().contains(P1));
        assert_eq!(group.online().count(), 1);
        assert_eq!(cpu.woken()[woken_before..], [P0]);
        test_processor_identity::set(P0);
    }

    /// A sleep registered on a processor's timer base moves with the
    /// base's other entries when the processor goes offline, and fires
    /// from the successor's base, whose owner arms it.
    #[test]
    fn a_departing_processors_pending_sleep_fires_on_its_successor() {
        let cpu = ManualClockCpu::new();
        test_processor_identity::set(P0);
        let executor0 = Executor::isolated(ProgressCounter::new(), 2, P0);
        let timer0 = executor0.timer(cpu.clone());

        test_processor_identity::set(P1);
        let executor1 = executor0.sibling(ProgressCounter::new(), P1);
        let timer1 = executor1.timer(cpu.clone());
        // Whole multiples of the wheel's 50 µs quantum on this
        // one-tick-per-nanosecond clock, so each is due exactly at its
        // deadline.
        const DUE: u64 = 200_000;
        const CANCELLED_DUE: u64 = 100_000;
        let mut context = Context::from_waker(noop_waker_ref());
        let mut sleep = timer1.sleep_until(Instant::new(DUE));
        assert!(Pin::new(&mut sleep).poll(&mut context).is_pending());
        let mut cancelled = timer1.sleep_until(Instant::new(CANCELLED_DUE));
        assert!(Pin::new(&mut cancelled).poll(&mut context).is_pending());
        drop(cancelled);

        timer1.migrate_base(P0);
        assert_eq!(
            timer1.fire_expired(),
            0,
            "the departed base holds nothing to fire"
        );

        test_processor_identity::set(P0);
        // The cancelled sleep did not travel: the earliest deadline the
        // successor's base publishes is the live one.
        assert!(!timer0.is_due(Instant::new(DUE - 1)));
        assert!(timer0.is_due(Instant::new(DUE)));
        cpu.advance(DUE);
        assert_eq!(timer0.fire_expired(), 1);
        assert_eq!(Pin::new(&mut sleep).poll(&mut context), Poll::Ready(()));
    }
}
