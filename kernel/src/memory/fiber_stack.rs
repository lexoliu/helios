//! Lazily committed fiber stacks.
//!
//! Every store the component host runs owns one async fiber stack, and
//! the stack is sized for the deepest thing that runs on it — CPython's
//! class construction — rather than for what an average program touches.
//! Committing that span up front is what made a live instance cost
//! megabytes of user memory it never read.
//!
//! So the kernel reserves one *arena* of user address space at engine
//! construction, `slots × (guard + stack)`, and hands the runtime stacks
//! out of it. A slot is `[guard][stack]` with the guard at the bottom,
//! the direction a stack grows. Only the top window of a fresh stack,
//! [`FIBER_STACK_ENTRY_RESERVE`] bytes, is committed at claim, through the
//! user pool's blocking fallible path: it is what host code runs on from
//! the fiber's start to the first guest stack-limit check. Every page below
//! it is committed ahead of the guest, by the runtime's stack-reserve
//! callback ([`commit_fiber_stack_reserve`]), and when the stack is dropped
//! the slot keeps those pages for the next stack while a machine-wide
//! budget has room for them, or gives them back when it does not.
//!
//! # Retained slots
//!
//! Giving a slot's pages back is the expensive half of a stack's life:
//! every page unmapped is a TLB shootdown to every other processor, and
//! under a hypervisor an interrupt to an idle processor is a wake-up
//! measured in hundreds of microseconds. A store is torn down on the
//! spawn path, so that cost lands on every process start. A released
//! slot therefore stays *warm* — its committed pages kept, its region
//! still recorded — and the next stack claims a warm slot before a cold
//! one: no page-table work, no fault for the pages the last stack
//! touched, and no shootdown when it goes. The pages a warm slot holds
//! count against `retain_budget`; a release that would exceed it gives
//! its pages back instead. A warm slot carries what its last stack
//! wrote, which is the runtime's own contract for a reused stack
//! (`async_stack_zeroing(false)`).
//!
//! # Why the arena exists at all
//!
//! The reserve callback runs ahead of the guest on the ordinary path and
//! may wait for the pool. A fault-time commit runs on the faulting stack's own processor,
//! inside whatever the interrupted code was doing, and the arena's shape
//! is what lets it be short:
//!
//! - Classifying an address is arithmetic. Subtract the base, divide by
//!   the slot stride, compare against the guard length, read one atomic
//!   flag. No lookup, no tracker, no list.
//! - The page tables above the leaf already exist, because
//!   [`hal::vmm::AddressSpace::prepare_demand_commit`](helios_hal::vmm::AddressSpace::prepare_demand_commit)
//!   built them on the ordinary locked path when the stack was created.
//!   The fault writes leaf entries into tables nothing is editing.
//! - The frames come from one of two places, chosen by what the fault
//!   interrupted. Every lock on the user pool's path is an
//!   [`IrqSafeMutex`](super::irq_safe), which masks the holder's
//!   interrupts while it is held, so a context that was interrupted with
//!   interrupts enabled holds none of them, and the fault takes its
//!   frames through the pool's ordinary blocking path: it only ever waits
//!   for another processor's critical section, which waits for nothing.
//!   A context interrupted with interrupts masked may hold one, so the
//!   fault cannot wait: it takes its frames from the per-processor
//!   [`super::frame_reserve`], which no lock guards.
//!
//! # Concurrency contract
//!
//! The arena is machine-wide and every one of its operations may arrive
//! from any processor:
//!
//! - `claim` and the release in [`FiberStack`]'s `Drop` run on the
//!   ordinary path with no fault outstanding. They may wait on the pool's
//!   locks and take the address space's own lock through the hooks.
//!   Slot state moves by compare-and-swap. A cold claim takes the frames
//!   of the entry window through the pool's blocking path before it
//!   touches the slot, and writes the watermark before the fiber exists.
//! - [`commit_fiber_stack_reserve`] runs in ordinary context (interrupts
//!   enabled, no kernel lock held) on the processor running the fiber that
//!   owns the slot its stack pointer lies in: Wasmtime calls it from a
//!   guest stack-limit check or at a guest entry. It may wait on the user
//!   pool's `IrqSafeMutex` locks; the wait is bounded by another
//!   processor's critical section, which waits for nothing. It takes no
//!   address-space lock: the leaf entries go into tables
//!   `prepare_demand_commit` built at claim, and a not-present page
//!   becoming present needs no shootdown. It writes the slot's
//!   watermark, which has one writer at a time, and the per-processor
//!   counters on the executing processor's own lines. Nothing fans out to
//!   another processor. Whether interrupts are enabled is not asserted:
//!   `hal` has no query for the local interrupt mask short of masking
//!   it.
//! - [`resolve_stack_fault`] runs in fault context on the processor that
//!   faulted. Classification is lock-free. Frames are taken through the
//!   pool's blocking path, which takes only `IrqSafeMutex` locks, when the
//!   interrupted context says it was running with interrupts enabled
//!   ([`InterruptedContext`]), and from the processor's own lock-free
//!   reserve when it says they were masked; the first form waits on
//!   another processor's pool critical section if it meets one, the
//!   second never waits. The leaf writes that map the frames take no lock
//!   and need no shootdown: a not-present page becoming present is one no
//!   processor caches. Nothing fans out to another processor.
//!
//! One fiber runs on one processor at a time, so the pages of a given
//! slot are only ever committed (by the reserve callback, or faulted in)
//! by one processor at a time and its watermark has one writer at a time
//! once the stack exists; the atomics below are what keep
//! that true against a fiber that migrates between polls, and what keep
//! a claim on one processor from racing a release on another.
//! Per-processor counters sit on their own cache lines.
//!
//! A slot's guard page is never mapped. Guest code that overflows its
//! stack hits it and the runtime turns the fault into a wasm trap;
//! kernel code that overflows one hits the same page with nothing to
//! unwind, and [`StackFault`] is what names the slot in the fatal
//! report.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::{ProcessorId, current_processor};
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpaceError, PageFlags, VirtAddr, VirtRange};
use spin::Once;
use thiserror::Error;

use super::{allocate_user_frame_zeroed_on, deallocate_user_frame_on, frame_reserve};
use crate::ProgramOutOfMemory;
use crate::component::{
    FIBER_STACK_COMMIT_STEP, FIBER_STACK_ENTRY_RESERVE, FIBER_STACK_HOST_RESERVE,
};
#[cfg(all(target_os = "none", feature = "wasmtime-bare-metal"))]
use crate::wasmtime_adapter::parking::ParkedFibers;

/// Bytes of unmapped address space below every fiber stack.
///
/// One page would catch a walk off the bottom of the stack, and nothing
/// else. A frame larger than the guard could step over it and land in
/// the slot below, where the fault would look like an ordinary demand
/// commit and the overflow would go unnoticed; 64 KiB is larger than any
/// frame either Cranelift or the kernel's own Rust code builds, so an
/// overflow always lands inside it.
pub const FIBER_STACK_GUARD_BYTES: usize = 64 * 1024;

/// Pages a cold claim commits: the entry window.
const ENTRY_WINDOW_PAGES: usize = FIBER_STACK_ENTRY_RESERVE / PhysFrame::SIZE;

const _: () = assert!(
    FIBER_STACK_ENTRY_RESERVE.is_multiple_of(PhysFrame::SIZE)
        && FIBER_STACK_ENTRY_RESERVE != 0
        && FIBER_STACK_HOST_RESERVE.is_multiple_of(PhysFrame::SIZE)
        && FIBER_STACK_COMMIT_STEP.is_multiple_of(PhysFrame::SIZE),
    "the entry window is at least a page, and it, the host reserve and the commit step are whole pages"
);

/// The address-space operations the arena needs, as plain function
/// pointers.
///
/// Same shape and reason as [`super::SwapVmHooks`]: the kernel is not
/// generic over the address space — there is exactly one per machine,
/// chosen at link time — and a `dyn AddressSpace` would put a vtable on
/// the page-fault path. The backend that owns the address space builds
/// one `&'static` table and installs it at boot.
pub struct FiberStackVmHooks {
    /// Carve the arena's virtual range. Called once.
    pub reserve: fn(usize) -> Result<VirtRange, AddressSpaceError>,
    /// Record one slot's body as a demand-commit region and build every
    /// page-table level above its leaves.
    pub prepare_demand_commit: fn(VirtRange, PageFlags) -> Result<(), AddressSpaceError>,
    /// Map one page of a prepared region from fault context.
    pub commit_demand_page: fn(VirtAddr, NonNull<u8>, PageFlags) -> Result<(), AddressSpaceError>,
    /// Give one slot's body back, frames and record together. The
    /// second range is the part of the body a commit may have mapped:
    /// from the slot's watermark to its top.
    pub end_demand_commit: fn(VirtRange, VirtRange) -> Result<(), AddressSpaceError>,
}

/// What a fault entry knows about the context it interrupted.
///
/// The fault may wait for the user pool's locks only when the interrupted
/// context holds none of them. Every one of those locks masks its
/// holder's interrupts, so the interrupted context's own interrupt-enable
/// bit, as the exception saved it, is the whole answer: enabled, the
/// fault takes its frames through the pool; masked, it takes them from
/// the per-processor reserve. The backend reads the bit from the frame it
/// saved: `RFLAGS.IF` on x86, `SPSR_EL1.I` (inverted) on aarch64,
/// `sstatus.SPIE` on riscv.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptedContext {
    /// Whether the context the fault interrupted was running with
    /// interrupts enabled.
    pub interrupts_enabled: bool,
}

/// What a backend's fault entry does with a faulting address.
///
/// The three fault entries all read the same way: ask, return when the
/// answer is [`StackFault::Committed`], and carry the value into the
/// fatal report otherwise. [`StackFault`]'s `Display` is written to be
/// appended to that report — it says nothing at all for an address the
/// arena does not own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackFault {
    /// The arena does not own this address; the fault belongs to
    /// whatever the entry would have done with it anyway.
    Elsewhere,
    /// A reserved page inside a live stack, committed in place together
    /// with any gap between it and the committed range. The faulting
    /// instruction runs again.
    Committed,
    /// The user pool could not back the faulting page and the gap above
    /// it. Nothing was committed: the stack is as the fault found it.
    Starved {
        slot: usize,
        addr: usize,
        shortfall: ProgramOutOfMemory,
    },
    /// The guard page below a live fiber stack: a stack overflow.
    ///
    /// Guest code overflowing its stack is the runtime's to trap, so the
    /// entry hands the fault on as it always did. Kernel code that
    /// overflows a fiber stack has nothing to unwind, so the runtime
    /// declines it and the entry's fatal path reports this.
    Guard { slot: usize, addr: usize },
}

impl fmt::Display for StackFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Elsewhere | Self::Committed => Ok(()),
            Self::Starved {
                slot,
                addr,
                shortfall,
            } => write!(
                f,
                "; {addr:#x} in fiber stack slot {slot} needed a page and the user pool is \
                 exhausted: {shortfall}"
            ),
            Self::Guard { slot, addr } => write!(
                f,
                "; {addr:#x} is the guard page below fiber stack slot {slot}, so a kernel path \
                 overflowed a fiber stack"
            ),
        }
    }
}

/// Why a stack could not be handed out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum FiberStackError {
    #[error("the fiber stack arena was not built before a stack was asked for")]
    NoArena,
    #[error(
        "the runtime asked for a {requested}-byte fiber stack and the arena's slots are \
         {slot} bytes"
    )]
    WrongSize { requested: usize, slot: usize },
    #[error("every one of the arena's {slots} fiber stack slots is live")]
    Exhausted { slots: usize },
    #[error("the address space refused to prepare a fiber stack slot: {0}")]
    Prepare(#[source] AddressSpaceError),
    /// The user pool could not back the stack's entry window. The runtime
    /// wraps this in its own error, so the failed instantiation or call
    /// is classified as running out of memory like any refused grow.
    #[error("the user pool could not back a fiber stack's entry window: {0}")]
    OutOfMemory(#[source] ProgramOutOfMemory),
}

/// Why the stack below a guest stack pointer could not be committed.
///
/// The runtime ends the guest call with whatever the adapter makes of
/// this: [`Self::Overflow`] is a stack overflow, [`Self::OutOfMemory`] is
/// the same out-of-memory a refused grow is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum StackReserveError {
    /// The host reserve below the stack pointer does not fit above the
    /// slot's guard: the stack has no room left for the guest and the host
    /// code under it.
    #[error(
        "fiber stack slot {slot} cannot hold the {reserve}-byte host reserve below the stack \
         pointer {sp:#x}: the stack's lowest usable address is {lowest:#x}"
    )]
    Overflow {
        slot: usize,
        sp: usize,
        /// The bytes the commit would have kept below `sp`: the host
        /// reserve and the step beyond it.
        reserve: usize,
        lowest: usize,
    },
    /// The user pool could not back the pages the reserve needed. Nothing
    /// was committed: the stack is as the call found it.
    #[error("the user pool could not back the host reserve below a fiber stack: {0}")]
    OutOfMemory(#[source] ProgramOutOfMemory),
}

/// One slot's mutable state.
///
/// `live` is written by the claim that takes the slot and by the release
/// that gives it back, from any processor. `watermark` is written by the
/// claim that creates the stack, and from then on by the reserve commit and
/// the fault path on whichever processor is running the fiber, and read by
/// the release; it is the lowest address of the slot's committed run, which
/// is one run from there to `body_top`.
/// A slot with nothing mapped and no region recorded.
const SLOT_COLD: u8 = 0;
/// A released slot whose pages are still committed, counted against the
/// arena's retain budget, waiting for the next claim.
const SLOT_WARM: u8 = 1;
/// A slot a running stack owns.
const SLOT_LIVE: u8 = 2;

/// Every move between the three states is one compare-and-swap on
/// `state`, which is what lets a claim on one processor race a release
/// on another without a lock.
struct FiberStackSlot {
    state: AtomicU8,
    watermark: AtomicUsize,
    /// The Wasmtime unpark generation the fiber on this stack last
    /// returned from a parking wait at; see
    /// `wasmtime_adapter::parking`. A value left by an earlier fiber is
    /// one the generation has already reached, so it can only make the
    /// next fiber's first wait return early, never miss an unpark.
    #[cfg(all(target_os = "none", feature = "wasmtime-bare-metal"))]
    parked_generation: AtomicU64,
}

/// The machine's fiber stacks.
pub struct FiberStackArena {
    hooks: &'static FiberStackVmHooks,
    base: usize,
    slot_bytes: usize,
    stack_bytes: usize,
    slots: Box<[CachePadded<FiberStackSlot>]>,
    /// The most bytes warm slots may hold between them.
    retain_budget: usize,
    /// Bytes warm slots hold now. Written on every claim and release from
    /// whichever processor runs them, so it sits on its own line.
    retained_bytes: CachePadded<AtomicUsize>,
    /// Demand commits (faults) this processor has resolved. One counter
    /// per processor, each on its own line, because every one of them is
    /// written from fault context on its own processor.
    demand_commits: Box<[CachePadded<AtomicU64>]>,
    /// Pages those faults mapped, the faulting page and any gap above it
    /// together. Written like `demand_commits`, by the owning processor
    /// alone.
    committed_pages: Box<[CachePadded<AtomicU64>]>,
    /// Reserve commits that mapped pages, per processor, each on its own
    /// line. Written by [`Self::commit_reserve`] on the executing
    /// processor alone; a commit that found the stack already covered maps
    /// nothing and counts nothing.
    reserve_commits: Box<[CachePadded<AtomicU64>]>,
    /// Pages those reserve commits mapped. Written like `reserve_commits`.
    reserve_committed_pages: Box<[CachePadded<AtomicU64>]>,
    /// Whether the console has already been told this processor is
    /// resolving commits. Written from the release path, which is a
    /// place a `tracing` call is allowed; see [`Self::announce_processors`].
    announced: Box<[CachePadded<AtomicBool>]>,
    /// What every released stack cost, summed. Written on the release
    /// path from whichever processor tears the store down, which is why
    /// the pair sits on its own line rather than beside a per-processor
    /// counter.
    released: CachePadded<ReleasedTotals>,
    /// The Wasmtime parking registry of the fibers these stacks run.
    /// Written by every unpark, from whichever processor runs the
    /// notifying wasm thread, so it sits on its own line.
    #[cfg(all(target_os = "none", feature = "wasmtime-bare-metal"))]
    parked: CachePadded<ParkedFibers>,
}

struct ReleasedTotals {
    stacks: AtomicU64,
    committed_bytes: AtomicU64,
}

/// What the arena is holding, for the boot log and the stats panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FiberStackArenaStats {
    /// Slots the arena has in total.
    pub slots: usize,
    /// Slots a live stack is using right now.
    pub live_slots: usize,
    /// Released slots still holding their pages for the next stack.
    pub warm_slots: usize,
    /// Bytes those warm slots hold, against the retain budget.
    pub retained_bytes: usize,
    /// Bytes of user memory the live stacks have actually faulted in.
    pub committed_bytes: usize,
    /// Bytes of user memory the same stacks would have cost committed
    /// up front.
    pub eager_bytes: usize,
    /// Demand commits resolved since boot, across every processor.
    pub demand_commits: u64,
    /// Pages those demand commits mapped, skipped gaps included.
    pub demand_committed_pages: u64,
    /// Reserve commits that mapped pages since boot, across every
    /// processor.
    pub reserve_commits: u64,
    /// Pages those reserve commits mapped.
    pub reserve_committed_pages: u64,
    /// Stacks given back over the life of the arena.
    pub released_stacks: u64,
    /// Bytes those stacks had committed when they were given back.
    pub released_committed_bytes: u64,
    /// What the same stacks would have cost committed up front.
    pub released_eager_bytes: u64,
}

static HOOKS: Once<&'static FiberStackVmHooks> = Once::new();
static ARENA: Once<FiberStackArena> = Once::new();

/// Publishes the address space's demand-commit surface. Called once by
/// the backend that owns the address space, before any engine is built.
pub fn install_fiber_stack_hooks(hooks: &'static FiberStackVmHooks) {
    let mut installed = false;
    HOOKS.call_once(|| {
        installed = true;
        hooks
    });
    assert!(installed, "fiber-stack address-space hooks installed twice");
}

/// Builds the arena, once, for `slots` stacks of `stack_bytes` each.
///
/// Called at engine construction. The kernel builds more than one engine
/// and they share this arena: the number that bounds live stacks is the
/// kernel's instance budget, which is machine-wide, and each engine's
/// own stack pool carries the same number.
///
/// # Panics
///
/// When the backend installed no hooks, or when the address space
/// refuses the reservation. Both are boot-time configuration failures on
/// a target that has no second way to place a fiber stack, so they are
/// reported here rather than answered with a smaller stack or an eagerly
/// committed one.
pub fn install_fiber_stack_arena(
    slots: usize,
    stack_bytes: usize,
    processor_count: usize,
    retain_bytes: usize,
) {
    ARENA.call_once(|| {
        let hooks = *HOOKS.get().unwrap_or_else(|| {
            panic!(
                "this backend installed no fiber-stack address-space hooks, so it cannot host \
                 the lazily committed fiber stacks the component engine needs"
            )
        });
        assert!(slots != 0, "the fiber stack arena needs at least one slot");
        assert!(
            stack_bytes != 0 && stack_bytes.is_multiple_of(PhysFrame::SIZE),
            "a fiber stack of {stack_bytes} bytes is not a whole number of pages"
        );
        assert!(
            processor_count != 0,
            "the fiber stack arena needs at least one processor to count commits for"
        );
        assert!(
            stack_bytes >= FIBER_STACK_ENTRY_RESERVE,
            "a fiber stack of {stack_bytes} bytes cannot hold its {FIBER_STACK_ENTRY_RESERVE}-byte \
             entry window"
        );
        let slot_bytes = FIBER_STACK_GUARD_BYTES
            .checked_add(stack_bytes)
            .unwrap_or_else(|| panic!("fiber stack slot size overflows"));
        let bytes = slot_bytes
            .checked_mul(slots)
            .unwrap_or_else(|| panic!("fiber stack arena size overflows"));
        let range = (hooks.reserve)(bytes).unwrap_or_else(|error| {
            panic!("the address space refused the {bytes}-byte fiber stack arena: {error}")
        });
        let mut slot_states = Vec::with_capacity(slots);
        slot_states.resize_with(slots, || {
            CachePadded::new(FiberStackSlot {
                state: AtomicU8::new(SLOT_COLD),
                watermark: AtomicUsize::new(0),
                #[cfg(all(target_os = "none", feature = "wasmtime-bare-metal"))]
                parked_generation: AtomicU64::new(0),
            })
        });
        let mut counters = Vec::with_capacity(processor_count);
        counters.resize_with(processor_count, || CachePadded::new(AtomicU64::new(0)));
        let mut page_counters = Vec::with_capacity(processor_count);
        page_counters.resize_with(processor_count, || CachePadded::new(AtomicU64::new(0)));
        let mut reserve_counters = Vec::with_capacity(processor_count);
        reserve_counters.resize_with(processor_count, || CachePadded::new(AtomicU64::new(0)));
        let mut reserve_page_counters = Vec::with_capacity(processor_count);
        reserve_page_counters.resize_with(processor_count, || CachePadded::new(AtomicU64::new(0)));
        let mut announced = Vec::with_capacity(processor_count);
        announced.resize_with(processor_count, || CachePadded::new(AtomicBool::new(false)));
        tracing::info!(
            target: "helios_kernel::fiber_stack",
            base = range.start.raw(),
            slots,
            stack_bytes,
            guard_bytes = FIBER_STACK_GUARD_BYTES,
            entry_window_bytes = FIBER_STACK_ENTRY_RESERVE,
            host_reserve_bytes = FIBER_STACK_HOST_RESERVE,
            commit_step_bytes = FIBER_STACK_COMMIT_STEP,
            reserved_bytes = bytes,
            retain_bytes,
            "fiber stack arena reserved; stacks commit ahead of the guest"
        );
        FiberStackArena {
            hooks,
            base: range.start.raw(),
            slot_bytes,
            stack_bytes,
            slots: slot_states.into_boxed_slice(),
            retain_budget: retain_bytes,
            retained_bytes: CachePadded::new(AtomicUsize::new(0)),
            demand_commits: counters.into_boxed_slice(),
            committed_pages: page_counters.into_boxed_slice(),
            reserve_commits: reserve_counters.into_boxed_slice(),
            reserve_committed_pages: reserve_page_counters.into_boxed_slice(),
            announced: announced.into_boxed_slice(),
            released: CachePadded::new(ReleasedTotals {
                stacks: AtomicU64::new(0),
                committed_bytes: AtomicU64::new(0),
            }),
            #[cfg(all(target_os = "none", feature = "wasmtime-bare-metal"))]
            parked: CachePadded::new(ParkedFibers::new()),
        }
    });
}

/// Whether this machine has an arena, and so anything that draws on the
/// page-fault frame reserve and every store that asks the kernel to commit
/// its fiber's stack ahead of the guest.
pub(crate) fn fiber_stack_arena_installed() -> bool {
    ARENA.get().is_some()
}

/// What the arena is holding right now, or nothing when this machine
/// has none.
pub fn fiber_stack_arena_stats() -> Option<FiberStackArenaStats> {
    ARENA.get().map(FiberStackArena::stats)
}

/// Demand commits processor `processor` has resolved since boot.
pub fn fiber_stack_demand_commits_on(processor: ProcessorId) -> u64 {
    ARENA
        .get()
        .and_then(|arena| arena.demand_commits.get(usize::from(processor.id())))
        .map_or(0, |counter| counter.load(Ordering::Relaxed))
}

/// Classifies `addr` and, when it is a demand-commit page, resolves it.
///
/// This is what a backend's fault entry calls, with what it knows of the
/// context the fault interrupted. Classification takes no lock and walks
/// no list. Resolving takes the frames through the user pool's blocking
/// path when `interrupted` says interrupts were enabled, which the module
/// docs show is safe, and from the processor's reserve when it says they
/// were masked.
///
/// # Panics
///
/// When `interrupted` says interrupts were masked and the processor's
/// reserve cannot supply the frames the fault needs: a path holding an
/// interrupt-masking lock grew its stack further than the reserve covers
/// between two refills, or the pool is exhausted.
pub fn resolve_stack_fault(addr: VirtAddr, interrupted: InterruptedContext) -> StackFault {
    let Some(arena) = ARENA.get() else {
        return StackFault::Elsewhere;
    };
    arena.resolve(addr, interrupted)
}

/// Commits the stack below `sp` so host code running under it has
/// [`FIBER_STACK_HOST_RESERVE`] bytes, and answers the new reserve limit.
///
/// This is what the runtime's stack-reserve callback calls, from a guest
/// stack-limit check that fell below the reserve limit of the stack it
/// runs on, or at a guest entry. The answer is the lowest stack pointer at
/// which the stack is committed for that many bytes below it. It is at most
/// `sp`, and never below the stack's watermark plus the reserve.
///
/// The frames of the commit are all taken from the user pool's blocking
/// path before any is mapped, so a pool that cannot back them leaves the
/// stack as it was and answers [`StackReserveError::OutOfMemory`].
///
/// Concurrency: ordinary context on the processor running the fiber that
/// owns the slot `sp` lies in; see the module docs.
///
/// # Panics
///
/// When `sp` is not inside the body of a live slot: the runtime called it
/// off a fiber stack, or on one it gave back.
pub fn commit_fiber_stack_reserve(sp: usize) -> Result<usize, StackReserveError> {
    ARENA
        .get()
        .unwrap_or_else(|| {
            panic!(
                "the runtime asked for a host reserve below {sp:#x} before the fiber stack arena \
                 was built"
            )
        })
        .commit_reserve(sp)
}

impl FiberStackArena {
    fn reserved_bytes(&self) -> usize {
        self.slot_bytes * self.slots.len()
    }

    /// The body of slot `index`: the stack itself, above its guard.
    fn body(&self, index: usize) -> VirtRange {
        let start = self.base + index * self.slot_bytes + FIBER_STACK_GUARD_BYTES;
        VirtRange::new(VirtAddr::new(start), self.stack_bytes)
    }

    fn guard(&self, index: usize) -> Range<*mut u8> {
        let start = self.base + index * self.slot_bytes;
        (start as *mut u8)..((start + FIBER_STACK_GUARD_BYTES) as *mut u8)
    }

    #[cfg(all(target_os = "none", feature = "wasmtime-bare-metal"))]
    fn parked_generation(&self, addr: usize) -> &AtomicU64 {
        let index = addr
            .checked_sub(self.base)
            .filter(|offset| *offset < self.reserved_bytes())
            .map(|offset| offset / self.slot_bytes)
            .unwrap_or_else(|| {
                panic!(
                    "Wasmtime parking wait at stack address {addr:#x}, outside the fiber-stack \
                     arena: only a fiber can give its processor back while it waits"
                )
            });
        let slot = &self.slots[index];
        assert_eq!(
            slot.state.load(Ordering::Acquire),
            SLOT_LIVE,
            "Wasmtime parking wait on fiber stack slot {index}, which no live stack owns"
        );
        &slot.parked_generation
    }

    /// Resolves a fault at `addr`, or classifies it.
    ///
    /// A fault on a page of a live stack's body maps `[page, watermark)`:
    /// the faulting page and any gap a large frame skipped above it, so
    /// the committed range stays one run `[watermark, top)`, every page
    /// charged to the pool as user memory. The frames are all taken before
    /// any is mapped. A fault that interrupted a context with interrupts
    /// enabled takes them through the pool and answers
    /// [`StackFault::Starved`] with nothing mapped when the pool cannot
    /// back them all; one that interrupted a masked context takes them
    /// from the processor's reserve without touching the pool.
    ///
    /// Concurrency: fault context on the faulting processor, see the
    /// module docs. The slot's watermark has one writer, the processor
    /// running its fiber.
    ///
    /// # Panics
    ///
    /// - The slot is not live: the runtime is running on a stack it gave
    ///   back.
    /// - The page is already inside the committed range: the stack's
    ///   bookkeeping and its page tables disagree.
    /// - The faulting context had interrupts masked and the processor's
    ///   reserve cannot supply the frames.
    fn resolve(&self, addr: VirtAddr, interrupted: InterruptedContext) -> StackFault {
        let raw = addr.raw();
        let Some(offset) = raw.checked_sub(self.base) else {
            return StackFault::Elsewhere;
        };
        if offset >= self.reserved_bytes() {
            return StackFault::Elsewhere;
        }
        let index = offset / self.slot_bytes;
        let within = offset - index * self.slot_bytes;
        let slot = &self.slots[index];
        if slot.state.load(Ordering::Acquire) != SLOT_LIVE {
            panic!(
                "page fault at {raw:#x} inside fiber stack slot {index}, which no live stack \
                 owns: the runtime is running on a stack it gave back"
            );
        }
        if within < FIBER_STACK_GUARD_BYTES {
            return StackFault::Guard {
                slot: index,
                addr: raw,
            };
        }
        let page = raw & !(PhysFrame::SIZE - 1);
        let committed_from = slot.watermark.load(Ordering::Acquire);
        if page >= committed_from {
            panic!(
                "page fault at {raw:#x} in fiber stack slot {index}, on a page its committed range \
                 from {committed_from:#x} already maps: the stack's bookkeeping and its page tables \
                 disagree"
            );
        }
        let pages = (committed_from - page) / PhysFrame::SIZE;
        let processor = current_processor();
        let mut frames = if interrupted.interrupts_enabled {
            match FrameChain::take(processor, pages) {
                Ok(frames) => frames,
                Err(shortfall) => {
                    return StackFault::Starved {
                        slot: index,
                        addr: raw,
                        shortfall,
                    };
                }
            }
        } else {
            FrameChain::take_from_reserve(processor, pages).unwrap_or_else(|held| {
                panic!(
                    "a fault at {raw:#x} in fiber stack slot {index}, from a context running with \
                     interrupts masked, needed {pages} frames and processor {} reserved {held} of \
                     its {}: a path holding an interrupt-masking lock grew its stack further than \
                     the reserve covers between two refills, or the user pool is exhausted",
                    processor.id(),
                    frame_reserve::RESERVE_CAPACITY,
                )
            })
        };
        if let Err(error) = self.map_frames(index, &mut frames) {
            panic!(
                "page fault at {raw:#x} in fiber stack slot {index} could not be committed in \
                 place: {error}"
            );
        }
        self.demand_commits[usize::from(processor.id())].fetch_add(1, Ordering::Relaxed);
        self.committed_pages[usize::from(processor.id())]
            .fetch_add(pages as u64, Ordering::Relaxed);
        if interrupted.interrupts_enabled {
            // The interrupted context held no pool lock, so the refill
            // may wait on the pool like the fault just did. It is one
            // atomic load when the reserve is full.
            frame_reserve::top_up(processor);
        }
        // Nothing is logged from here. `tracing` reaches the debug
        // console, which is a lock the interrupted code may be holding
        // on this very processor; the counters above are what the
        // release path reports instead.
        StackFault::Committed
    }

    /// Maps `frames`, one page each, immediately below slot `index`'s
    /// committed range, from the page next to it downwards.
    ///
    /// The watermark follows every page, so the committed range is one
    /// run at each step and the release walk stays exact however far a
    /// refused page got. The refused page's frame goes back on the chain:
    /// a fault that interrupted a masked context may not hand it to the
    /// pool, so what becomes of the chain is the caller's to decide.
    fn map_frames(&self, index: usize, frames: &mut FrameChain) -> Result<(), AddressSpaceError> {
        let slot = &self.slots[index];
        let flags = PageFlags::READ | PageFlags::WRITE;
        while let Some(frame) = frames.pop() {
            let page = VirtAddr::new(slot.watermark.load(Ordering::Acquire) - PhysFrame::SIZE);
            if let Err(error) = (self.hooks.commit_demand_page)(page, frame, flags) {
                frames.push(frame);
                return Err(error);
            }
            slot.watermark.fetch_min(page.raw(), Ordering::AcqRel);
        }
        Ok(())
    }

    /// Commits the stack below `sp` for the host reserve and answers the
    /// new reserve limit.
    ///
    /// 1. The slot is found by the arithmetic [`Self::resolve`] uses.
    /// 2. The commit reaches down to `need = page_floor(sp - R - G)`: the
    ///    reserve `R` below `sp`, and the step `G` beyond it so the next
    ///    check is a step away.
    /// 3. A `need` below the slot's body leaves no room for the reserve above
    ///    the guard: [`StackReserveError::Overflow`]. The runtime's own limit
    ///    (`max_wasm_stack` below the entry) is the stack less the same
    ///    `R + G`, measured from the entry rather than from the top, so it
    ///    lies a little lower and this is the limit a deep guest meets.
    /// 4. A `need` at or above the watermark is already covered: nothing is
    ///    taken, and the answer is `watermark + R`.
    /// 5. Otherwise `(watermark - need) / page` frames are taken, all before
    ///    any is mapped, and mapped from the watermark downwards. The answer
    ///    is `need + R`.
    /// 6. A pool that cannot supply them leaves nothing mapped:
    ///    [`StackReserveError::OutOfMemory`].
    ///
    /// Concurrency: ordinary context on the processor running the slot's
    /// fiber, see the module docs. The slot's watermark has one writer, that
    /// fiber.
    ///
    /// # Panics
    ///
    /// - `sp` is not inside the body of a slot: outside the arena, in a
    ///   guard, or at the body's top, where no frame has been pushed.
    /// - The slot is not live.
    /// - The address space refuses a page of a prepared slot's body.
    fn commit_reserve(&self, sp: usize) -> Result<usize, StackReserveError> {
        let index = sp
            .checked_sub(self.base)
            .filter(|offset| *offset < self.reserved_bytes())
            .map(|offset| offset / self.slot_bytes)
            .filter(|index| {
                let body = self.body(*index);
                body.start.raw() <= sp && sp < body.end().raw()
            })
            .unwrap_or_else(|| {
                panic!(
                    "the runtime asked for a host reserve below {sp:#x}, which is not inside the \
                     body of a fiber stack slot: it called the reserve commit off a fiber stack"
                )
            });
        let slot = &self.slots[index];
        assert_eq!(
            slot.state.load(Ordering::Acquire),
            SLOT_LIVE,
            "the runtime asked for a host reserve below {sp:#x} in fiber stack slot {index}, which \
             no live stack owns"
        );
        let lowest = self.body(index).start.raw();
        let reserve = FIBER_STACK_HOST_RESERVE + FIBER_STACK_COMMIT_STEP;
        let Some(need) = sp
            .checked_sub(reserve)
            .map(|address| address & !(PhysFrame::SIZE - 1))
            .filter(|need| *need >= lowest)
        else {
            return Err(StackReserveError::Overflow {
                slot: index,
                sp,
                reserve,
                lowest,
            });
        };
        let committed_from = slot.watermark.load(Ordering::Acquire);
        if committed_from <= need {
            return Ok(committed_from + FIBER_STACK_HOST_RESERVE);
        }
        let pages = (committed_from - need) / PhysFrame::SIZE;
        let processor = current_processor();
        let mut frames =
            FrameChain::take(processor, pages).map_err(StackReserveError::OutOfMemory)?;
        if let Err(error) = self.map_frames(index, &mut frames) {
            panic!(
                "the host reserve below {sp:#x} in fiber stack slot {index} could not be \
                 committed in place: {error}"
            );
        }
        self.reserve_commits[usize::from(processor.id())].fetch_add(1, Ordering::Relaxed);
        self.reserve_committed_pages[usize::from(processor.id())]
            .fetch_add(pages as u64, Ordering::Relaxed);
        Ok(need + FIBER_STACK_HOST_RESERVE)
    }

    /// Takes a slot: a warm one as it stands, or a cold one prepared
    /// with its top window of [`FIBER_STACK_ENTRY_RESERVE`] bytes
    /// committed.
    ///
    /// The window is what the runtime writes the fiber's initial frame to
    /// and what host code runs on, from the fiber's start to the first
    /// guest check that commits the reserve (instantiation and the first
    /// entry run before any such check). A warm slot already holds at
    /// least the window, because the watermark only ever moves down while a
    /// slot is warm or live.
    ///
    /// The window's frames come from the pool's blocking fallible path, all
    /// of them before anything of the slot is touched, so a pool that cannot
    /// back them answers [`FiberStackError::OutOfMemory`] with the slot as
    /// it was found. A claim never draws on the page-fault reserve.
    ///
    /// Concurrency: ordinary context on any processor. It waits on the
    /// pool's locks, so no pool lock may be held by the caller.
    fn claim(&'static self, stack_bytes: usize) -> Result<FiberStack, FiberStackError> {
        if stack_bytes != self.stack_bytes {
            return Err(FiberStackError::WrongSize {
                requested: stack_bytes,
                slot: self.stack_bytes,
            });
        }
        if let Some(index) = self.take_slot(SLOT_WARM) {
            // The pages its last stack touched are still mapped, so the
            // runtime's first frame lands on a committed page and nothing
            // here touches the address space.
            self.retained_bytes
                .fetch_sub(self.committed_bytes(index), Ordering::AcqRel);
            return Ok(FiberStack { arena: self, index });
        }
        let index = self
            .take_slot(SLOT_COLD)
            .ok_or(FiberStackError::Exhausted {
                slots: self.slots.len(),
            })?;
        let body = self.body(index);
        let body_top = body.end().raw();
        // The runtime writes the fiber's initial frame at the top of the
        // stack and host code runs below it before any guest check, so the
        // window is committed here rather than left to faults. Its frames
        // are all taken before anything of the slot is touched, so a pool
        // that cannot back them leaves the slot exactly as it was found.
        let processor = current_processor();
        let mut frames = match FrameChain::take(processor, ENTRY_WINDOW_PAGES) {
            Ok(frames) => frames,
            Err(shortfall) => {
                self.slots[index].state.store(SLOT_COLD, Ordering::Release);
                return Err(FiberStackError::OutOfMemory(shortfall));
            }
        };
        if let Err(error) =
            (self.hooks.prepare_demand_commit)(body, PageFlags::READ | PageFlags::WRITE)
        {
            frames.give_back(processor);
            self.slots[index].state.store(SLOT_COLD, Ordering::Release);
            return Err(FiberStackError::Prepare(error));
        }
        self.slots[index]
            .watermark
            .store(body_top, Ordering::Release);
        if let Err(error) = self.map_frames(index, &mut frames) {
            frames.give_back(processor);
            (self.hooks.end_demand_commit)(body, body).unwrap_or_else(|cleanup| {
                panic!(
                    "fiber stack slot {index} could not be given back after its entry window was \
                     refused ({error}): {cleanup}"
                )
            });
            self.slots[index]
                .watermark
                .store(body_top, Ordering::Release);
            self.slots[index].state.store(SLOT_COLD, Ordering::Release);
            return Err(FiberStackError::Prepare(error));
        }
        Ok(FiberStack { arena: self, index })
    }

    /// Moves the lowest slot in state `from` to live and returns it.
    ///
    /// Lowest first, on purpose: a cold claim then lands on a slot whose
    /// leaf tables an earlier claim already built, and the warm slots
    /// stay packed at the bottom of the arena. The load before the
    /// exchange keeps a scan over hundreds of live slots to plain reads.
    fn take_slot(&self, from: u8) -> Option<usize> {
        self.slots.iter().position(|slot| {
            slot.state.load(Ordering::Acquire) == from
                && slot
                    .state
                    .compare_exchange(from, SLOT_LIVE, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
        })
    }

    /// Bytes slot `index` has committed: from its watermark to its top.
    fn committed_bytes(&self, index: usize) -> usize {
        self.body(index)
            .end()
            .raw()
            .saturating_sub(self.slots[index].watermark.load(Ordering::Acquire))
    }

    /// Gives slot `index` back: warm, keeping its pages for the next
    /// stack, while the retain budget has room for them; otherwise cold,
    /// with every page a fault ever mapped returned to the user pool on
    /// the ordinary locked path with the shootdown that path already
    /// does.
    fn release(&self, index: usize) {
        let committed_bytes = self.committed_bytes(index);
        // What this stack actually cost, against what it would have cost
        // committed up front, summed for the boot-end report. Nothing is
        // logged per release: a store teardown is on the spawn path, and
        // a console line there is a serial write per instance.
        self.released.stacks.fetch_add(1, Ordering::Relaxed);
        self.released
            .committed_bytes
            .fetch_add(committed_bytes as u64, Ordering::Relaxed);
        let retained = self
            .retained_bytes
            .fetch_add(committed_bytes, Ordering::AcqRel);
        if retained + committed_bytes <= self.retain_budget {
            self.slots[index].state.store(SLOT_WARM, Ordering::Release);
            self.announce_processors();
            return;
        }
        self.retained_bytes
            .fetch_sub(committed_bytes, Ordering::AcqRel);
        let body = self.body(index);
        let watermark = self.slots[index].watermark.load(Ordering::Acquire);
        // Only the pages between the watermark and the top can be
        // mapped, so that is all the address space walks.
        let mapped = VirtRange::new(VirtAddr::new(watermark), committed_bytes);
        (self.hooks.end_demand_commit)(body, mapped).unwrap_or_else(|error| {
            panic!(
                "fiber stack slot {index} at {:#x} could not be given back: {error}",
                body.start.raw()
            )
        });
        self.slots[index]
            .watermark
            .store(body.end().raw(), Ordering::Release);
        self.slots[index].state.store(SLOT_COLD, Ordering::Release);
        self.announce_processors();
    }

    /// Names each processor that has resolved a demand commit, once.
    ///
    /// The fault path may not log, so this is where the per-processor
    /// counters reach the console: on the release path, at most once per
    /// processor for the whole boot. Whether the fault path works on a
    /// processor is not something to infer from a stack that did not
    /// crash, and a machine where only the bootstrap processor ever
    /// resolved one is a machine where the others were never asked.
    fn announce_processors(&self) {
        for (processor, counter) in self.demand_commits.iter().enumerate() {
            let commits = counter.load(Ordering::Relaxed);
            if commits == 0 || self.announced[processor].swap(true, Ordering::AcqRel) {
                continue;
            }
            tracing::info!(
                target: "helios_kernel::fiber_stack",
                processor,
                demand_commits = commits,
                "processor is resolving fiber stack demand commits"
            );
        }
    }

    fn stats(&self) -> FiberStackArenaStats {
        let mut live_slots = 0;
        let mut warm_slots = 0;
        let mut committed_bytes = 0;
        for (index, slot) in self.slots.iter().enumerate() {
            match slot.state.load(Ordering::Acquire) {
                SLOT_LIVE => {
                    live_slots += 1;
                    committed_bytes += self.committed_bytes(index);
                }
                SLOT_WARM => warm_slots += 1,
                _ => {}
            }
        }
        let released_stacks = self.released.stacks.load(Ordering::Relaxed);
        FiberStackArenaStats {
            slots: self.slots.len(),
            live_slots,
            warm_slots,
            retained_bytes: self.retained_bytes.load(Ordering::Acquire),
            committed_bytes,
            eager_bytes: live_slots * self.stack_bytes,
            demand_commits: self
                .demand_commits
                .iter()
                .map(|counter| counter.load(Ordering::Relaxed))
                .sum(),
            demand_committed_pages: self
                .committed_pages
                .iter()
                .map(|counter| counter.load(Ordering::Relaxed))
                .sum(),
            reserve_commits: self
                .reserve_commits
                .iter()
                .map(|counter| counter.load(Ordering::Relaxed))
                .sum(),
            reserve_committed_pages: self
                .reserve_committed_pages
                .iter()
                .map(|counter| counter.load(Ordering::Relaxed))
                .sum(),
            released_stacks,
            released_committed_bytes: self.released.committed_bytes.load(Ordering::Relaxed),
            released_eager_bytes: released_stacks * self.stack_bytes as u64,
        }
    }
}

/// The frames one commit needs, taken from the user pool (or, for a
/// masked fault, the processor's reserve) before any of them is mapped.
///
/// Mapping is not undone from fault context (taking a translation away
/// needs a shootdown, and frees nothing until every processor has
/// acknowledged it), so a commit that could run out of frames halfway
/// must not have mapped any. Taking all of them first makes a refusal
/// cost nothing: the chain gives its frames back to the pool and the
/// stack is as it was.
///
/// The list is threaded through the frames' own first word, as
/// [`super::frame_slab`] does, so it allocates nothing. Every frame is
/// zeroed by the pool and the link is cleared on the way out, so a frame
/// leaves the chain exactly as the pool handed it over.
///
/// Concurrency: a local value of the one context that built it.
struct FrameChain {
    head: Option<NonNull<u8>>,
}

impl FrameChain {
    /// `count` zeroed frames from the user pool, through its blocking
    /// path, or the refusal that stopped it, with what was taken given
    /// back.
    fn take(processor: ProcessorId, count: usize) -> Result<Self, ProgramOutOfMemory> {
        let mut chain = Self { head: None };
        for _ in 0..count {
            match allocate_user_frame_zeroed_on(processor) {
                Ok(frame) => chain.push(frame),
                Err(shortfall) => {
                    chain.give_back(processor);
                    return Err(shortfall);
                }
            }
        }
        Ok(chain)
    }

    /// `count` zeroed frames from `processor`'s reserve, without touching
    /// the pool, or how many the reserve held when it ran dry. The frames
    /// taken before it ran dry are not returned: the caller panics.
    fn take_from_reserve(processor: ProcessorId, count: usize) -> Result<Self, usize> {
        let mut chain = Self { head: None };
        for held in 0..count {
            match frame_reserve::take_frame(processor) {
                Some(frame) => chain.push(frame),
                None => return Err(held),
            }
        }
        Ok(chain)
    }

    fn push(&mut self, frame: NonNull<u8>) {
        // SAFETY: the pool handed this page over whole and nothing else
        // holds it, and a page has room for a pointer-sized link at its
        // start.
        unsafe { frame.cast::<Option<NonNull<u8>>>().write(self.head) };
        self.head = Some(frame);
    }

    /// The most recently taken frame, zeroed again.
    fn pop(&mut self) -> Option<NonNull<u8>> {
        let frame = self.head?;
        // SAFETY: `push` wrote the link of every frame on the chain, and
        // this one is leaving it, so nothing reads the link again.
        unsafe {
            let link = frame.cast::<Option<NonNull<u8>>>();
            self.head = link.read();
            link.write(None);
        }
        Some(frame)
    }

    fn give_back(&mut self, processor: ProcessorId) {
        while let Some(frame) = self.pop() {
            deallocate_user_frame_on(processor, frame);
        }
    }
}

/// One fiber stack, owned by the runtime for as long as its store lives.
///
/// Dropping it gives the slot back. The runtime's stack pool drops the
/// stack when the store that ran on it is torn down, and the generic
/// pool keeps nothing warm, so this is the whole lifecycle.
pub struct FiberStack {
    arena: &'static FiberStackArena,
    index: usize,
}

impl FiberStack {
    /// The stack's usable range, guard excluded.
    pub fn range(&self) -> Range<usize> {
        let body = self.arena.body(self.index);
        body.start.raw()..body.end().raw()
    }

    /// The highest address of the stack; the fiber's initial stack
    /// pointer.
    pub fn top(&self) -> *mut u8 {
        self.arena.body(self.index).end().raw() as *mut u8
    }

    /// The unmapped range below the stack.
    pub fn guard_range(&self) -> Range<*mut u8> {
        self.arena.guard(self.index)
    }
}

impl Drop for FiberStack {
    fn drop(&mut self) {
        self.arena.release(self.index);
    }
}

/// The arena's Wasmtime parking registry, and the unpark generation the
/// fiber running on the stack that holds `addr` last returned from a
/// parking wait at. Panics when `addr` is not inside a live fiber stack.
#[cfg(all(target_os = "none", feature = "wasmtime-bare-metal"))]
pub(crate) fn fiber_parking(addr: usize) -> (&'static ParkedFibers, &'static AtomicU64) {
    let arena = installed_arena();
    (&arena.parked, arena.parked_generation(addr))
}

/// The arena's Wasmtime parking registry.
#[cfg(all(target_os = "none", feature = "wasmtime-bare-metal"))]
pub(crate) fn parked_fibers() -> &'static ParkedFibers {
    &installed_arena().parked
}

#[cfg(all(target_os = "none", feature = "wasmtime-bare-metal"))]
fn installed_arena() -> &'static FiberStackArena {
    ARENA
        .get()
        .expect("Wasmtime parking hook before the fiber-stack arena was installed")
}

/// Takes one stack out of the arena.
pub fn claim_fiber_stack(stack_bytes: usize) -> Result<FiberStack, FiberStackError> {
    ARENA
        .get()
        .ok_or(FiberStackError::NoArena)?
        .claim(stack_bytes)
}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;
    use core::sync::atomic::AtomicUsize;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use helios_hal::cpu::ProcessorId;

    use super::*;
    use crate::test_processor_identity;

    /// Bytes of pretend user memory the pool is built over. It has to
    /// cover the reserve and every page the tests commit.
    const POOL_BYTES: usize = 4 * 1024 * 1024;
    /// Deep enough for a fault dozens of pages below the top.
    const TEST_STACK_BYTES: usize = 96 * PhysFrame::SIZE;
    const TEST_SLOTS: usize = 3;
    /// Two, so that a commit resolved on the test's processor can be
    /// shown to land on that processor's counter and no other.
    const TEST_PROCESSORS: usize = 2;
    /// Two entry windows of warm slots: enough to keep two untouched
    /// stacks and too little for a third, or for one that grew.
    const TEST_RETAIN_BYTES: usize = 2 * FIBER_STACK_ENTRY_RESERVE;
    /// What a fresh stack has committed.
    const ENTRY_BYTES: usize = FIBER_STACK_ENTRY_RESERVE;
    /// Pages a fault thirty pages below the top has to map: the thirty,
    /// less the entry window a claim already committed.
    const THIRTY_PAGE_FAULT_RUN: usize = 30 - ENTRY_WINDOW_PAGES;
    /// Where the fake address space puts the arena. Any page-aligned
    /// value works; nothing dereferences it.
    const TEST_ARENA_BASE: usize = 0x0000_4000_0000_0000;
    const TEST_SLOT_BYTES: usize = FIBER_STACK_GUARD_BYTES + TEST_STACK_BYTES;

    static PREPARED: AtomicUsize = AtomicUsize::new(0);
    static COMMITTED: AtomicUsize = AtomicUsize::new(0);
    static ENDED: AtomicUsize = AtomicUsize::new(0);
    /// The `mapped` range of the most recent `end_demand_commit`, as
    /// `(start, byte_len)`.
    static LAST_MAPPED: (AtomicUsize, AtomicUsize) = (AtomicUsize::new(0), AtomicUsize::new(0));
    /// The fake page table: every mapped page and the frame behind it.
    static MAPPED: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

    /// An address space that records what the arena asked of it.
    ///
    /// The arena's own logic is the geometry, the slot bookkeeping and
    /// the fault classification; what the page tables do with the
    /// result is each backend's, and is covered by booting them. The
    /// fake does keep what a real one owes the pool: a page mapped twice
    /// is refused, a page in a guard is refused, a frame handed to it is
    /// a whole zeroed page, and ending a commit gives its frames back.
    static TEST_HOOKS: FiberStackVmHooks = FiberStackVmHooks {
        reserve: |bytes| {
            assert_eq!(bytes, TEST_SLOTS * TEST_SLOT_BYTES);
            Ok(VirtRange::new(VirtAddr::new(TEST_ARENA_BASE), bytes))
        },
        prepare_demand_commit: |virt, flags| {
            assert_eq!(virt.byte_len, TEST_STACK_BYTES);
            assert_eq!(flags, PageFlags::READ | PageFlags::WRITE);
            PREPARED.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
        commit_demand_page: |addr, frame, flags| {
            assert!(addr.is_page_aligned());
            assert_eq!(flags, PageFlags::READ | PageFlags::WRITE);
            assert!(
                (addr.raw() - TEST_ARENA_BASE) % TEST_SLOT_BYTES >= FIBER_STACK_GUARD_BYTES,
                "{addr:?} is in a guard, which is never mapped"
            );
            // SAFETY: the frame is one whole page the pool handed over.
            let page = unsafe { core::slice::from_raw_parts(frame.as_ptr(), PhysFrame::SIZE) };
            assert!(
                page.iter().all(|byte| *byte == 0),
                "a frame arrives zeroed, its chain link included"
            );
            let mut mapped = MAPPED.lock().expect("fake page table");
            if mapped.insert(addr.raw(), frame.as_ptr() as usize).is_some() {
                return Err(AddressSpaceError::Overlap);
            }
            COMMITTED.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
        end_demand_commit: |virt, mapped| {
            assert_eq!(virt.byte_len, TEST_STACK_BYTES);
            assert!(
                virt.encloses(mapped),
                "the mapped range {mapped:?} lies inside the body {virt:?}"
            );
            LAST_MAPPED.0.store(mapped.start.raw(), Ordering::Relaxed);
            LAST_MAPPED.1.store(mapped.byte_len, Ordering::Relaxed);
            ENDED.fetch_add(1, Ordering::Relaxed);
            let mut table = MAPPED.lock().expect("fake page table");
            let pages: alloc::vec::Vec<usize> = table
                .range(virt.start.raw()..virt.end().raw())
                .map(|(page, _)| *page)
                .collect();
            for page in pages {
                let frame = table.remove(&page).expect("a page just listed");
                assert!(
                    mapped.start.raw() <= page && page < mapped.end().raw(),
                    "page {page:#x} is mapped outside the range the arena named"
                );
                super::super::deallocate_user_frame(
                    NonNull::new(frame as *mut u8).expect("a mapped frame"),
                );
            }
            Ok(())
        },
    };

    fn enabled() -> InterruptedContext {
        InterruptedContext {
            interrupts_enabled: true,
        }
    }

    fn masked() -> InterruptedContext {
        InterruptedContext {
            interrupts_enabled: false,
        }
    }

    /// The fake page table's mapped pages, lowest first.
    fn mapped_pages() -> alloc::vec::Vec<usize> {
        MAPPED
            .lock()
            .expect("fake page table")
            .keys()
            .copied()
            .collect()
    }

    /// The pages of `[low, high)`.
    fn pages_between(low: usize, high: usize) -> alloc::vec::Vec<usize> {
        (low..high).step_by(PhysFrame::SIZE).collect()
    }

    /// Frames taken out of the pool, so a test can give some back.
    struct Hoard(alloc::vec::Vec<NonNull<u8>>);

    impl Hoard {
        /// Takes every frame the pool has, then gives `left` of them
        /// back.
        fn exhaust_pool_leaving(left: usize) -> Self {
            let mut hoard = Self(alloc::vec::Vec::new());
            while let Ok(frame) = super::super::allocate_user_frame_zeroed() {
                hoard.0.push(frame);
            }
            hoard.give_back(left);
            hoard
        }

        fn give_back(&mut self, count: usize) {
            for _ in 0..count {
                super::super::deallocate_user_frame(self.0.pop().expect("a frame to give back"));
            }
        }
    }

    /// How many frames the pool can hand out right now; it takes them
    /// all and gives them back.
    fn frames_in_pool() -> usize {
        let mut held = alloc::vec::Vec::new();
        while let Ok(frame) = super::super::allocate_user_frame_zeroed() {
            held.push(frame);
        }
        let count = held.len();
        for frame in held {
            super::super::deallocate_user_frame(frame);
        }
        count
    }

    /// Brings up the user pool the arena draws on and the arena on top
    /// of it. Each test runs in its own process, so the install-once
    /// statics are fresh every time.
    fn arena_retaining(retain_bytes: usize) {
        let layout = Layout::from_size_align(POOL_BYTES, PhysFrame::SIZE).expect("pool layout");
        // Leaked on purpose: the pool is a `&'static` for the life of
        // the machine, and a test process is that life.
        // SAFETY: the layout has a non-zero size.
        let base = unsafe { alloc::alloc::alloc_zeroed(layout) } as usize;
        assert!(base != 0, "pool backing");
        let pool =
            super::super::install_user_memory_pool(super::super::allocate_user_memory_pool());
        pool.initialize(&[(base, base + POOL_BYTES)]);
        pool.configure_processors(TEST_PROCESSORS);
        frame_reserve::configure_processors(TEST_PROCESSORS);
        install_fiber_stack_hooks(&TEST_HOOKS);
        install_fiber_stack_arena(TEST_SLOTS, TEST_STACK_BYTES, TEST_PROCESSORS, retain_bytes);
    }

    fn arena() {
        arena_retaining(TEST_RETAIN_BYTES);
    }

    #[test]
    fn a_fresh_stack_commits_only_its_entry_window_and_reports_its_guard() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");

        assert_eq!(PREPARED.load(Ordering::Relaxed), 1);
        assert_eq!(
            COMMITTED.load(Ordering::Relaxed),
            ENTRY_WINDOW_PAGES,
            "a fresh stack pays for its entry window, not for the span it may grow into"
        );
        let top = stack.range().end;
        assert_eq!(mapped_pages(), pages_between(top - ENTRY_BYTES, top));
        assert_eq!(
            fiber_stack_arena_stats().expect("stats").committed_bytes,
            ENTRY_BYTES
        );
        assert_eq!(
            reserved_bytes(),
            0,
            "a claim draws on the page-fault reserve never and fills it never"
        );
        let range = stack.range();
        assert_eq!(range.end - range.start, TEST_STACK_BYTES);
        assert_eq!(stack.top() as usize, range.end);
        let guard = stack.guard_range();
        assert_eq!(guard.end as usize, range.start);
        assert_eq!(
            guard.end as usize - guard.start as usize,
            FIBER_STACK_GUARD_BYTES
        );
    }

    #[test]
    fn an_enabled_fault_commits_exactly_the_page_and_the_gap_above_it_as_one_run() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let range = stack.range();
        let committed_before = COMMITTED.load(Ordering::Relaxed);

        // Thirty pages below the top: with a gap between the committed
        // entry window and the faulting page that a large frame skipped.
        let page = range.end - 30 * PhysFrame::SIZE;
        assert_eq!(
            resolve_stack_fault(VirtAddr::new(page + 8), enabled()),
            StackFault::Committed
        );

        assert_eq!(
            mapped_pages(),
            pages_between(page, range.end),
            "the committed range is one run from the faulting page to the top, the skipped gap \
             included, and nothing below the faulting page"
        );
        let faulted_pages = THIRTY_PAGE_FAULT_RUN;
        assert_eq!(
            COMMITTED.load(Ordering::Relaxed),
            committed_before + faulted_pages
        );
        let stats = fiber_stack_arena_stats().expect("arena stats");
        assert_eq!(stats.live_slots, 1);
        assert_eq!(stats.committed_bytes, range.end - page);
        assert_eq!(stats.eager_bytes, TEST_STACK_BYTES);
        assert_eq!(stats.demand_commits, 1, "one fault is one demand commit");
        assert_eq!(stats.demand_committed_pages, faulted_pages as u64);
    }

    #[test]
    fn a_served_enabled_fault_tops_the_reserve_up_through_the_pool() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        assert_eq!(reserved_bytes(), 0);

        assert_eq!(
            resolve_stack_fault(thirty_page_fault(&stack), enabled()),
            StackFault::Committed
        );
        assert_eq!(
            reserved_bytes(),
            frame_reserve::RESERVE_CAPACITY * PhysFrame::SIZE,
            "the fault's frames came from the pool and the reserve was refilled behind it"
        );
    }

    #[test]
    fn a_guard_hit_is_an_overflow_for_any_interrupted_context() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let range = stack.range();
        let mapped_before = mapped_pages();

        for interrupted in [enabled(), masked()] {
            let guard = VirtAddr::new(range.start - PhysFrame::SIZE);
            let outcome = resolve_stack_fault(guard, interrupted);
            assert!(
                matches!(outcome, StackFault::Guard { .. }),
                "a guard hit is a stack overflow, never a commit and never the masked-fault \
                 panic: {outcome:?}"
            );
            assert!(
                alloc::format!("{outcome}").contains("guard page"),
                "the fatal report has to name it"
            );
        }
        assert_eq!(mapped_pages(), mapped_before);
    }

    #[test]
    fn a_fault_on_the_lowest_page_of_the_body_commits_the_whole_body_and_never_the_guard() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let range = stack.range();

        assert_eq!(
            resolve_stack_fault(VirtAddr::new(range.start), enabled()),
            StackFault::Committed
        );
        assert_eq!(mapped_pages(), pages_between(range.start, range.end));
        assert_eq!(
            fiber_stack_arena_stats().expect("stats").committed_bytes,
            TEST_STACK_BYTES
        );
    }

    #[test]
    fn an_address_outside_the_arena_is_nobody_else_business() {
        arena();
        assert_eq!(
            resolve_stack_fault(VirtAddr::new(TEST_ARENA_BASE - PhysFrame::SIZE), enabled()),
            StackFault::Elsewhere
        );
        let past = TEST_ARENA_BASE + TEST_SLOTS * TEST_SLOT_BYTES;
        assert_eq!(
            resolve_stack_fault(VirtAddr::new(past), enabled()),
            StackFault::Elsewhere
        );
    }

    /// A processor's reserve, filled the way the executor loop fills it,
    /// with the calling thread as processor 0.
    fn full_reserve() {
        frame_reserve::top_up(current_processor());
        assert_eq!(
            reserved_bytes(),
            frame_reserve::RESERVE_CAPACITY * PhysFrame::SIZE
        );
    }

    fn reserved_bytes() -> usize {
        frame_reserve::reserved_bytes()
    }

    #[test]
    #[should_panic(expected = "needed 26 frames and processor 0 reserved 0 of its 64")]
    fn a_masked_fault_with_the_reserve_empty_is_the_named_panic() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        assert_eq!(reserved_bytes(), 0);

        let _ = resolve_stack_fault(thirty_page_fault(&stack), masked());
    }

    #[test]
    fn a_masked_fault_is_served_from_the_reserve_while_another_processor_holds_the_pool_lock() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let fault = thirty_page_fault(&stack);
        full_reserve();
        let served = core::sync::atomic::AtomicBool::new(false);

        std::thread::scope(|scope| {
            test_processor_identity::set(ProcessorId::new(1));
            let pool = super::super::user::installed_user_memory_pool().expect("pool");
            pool.hold_buddy_lock_while(|| {
                scope.spawn(|| {
                    test_processor_identity::set(ProcessorId::new(0));
                    assert_eq!(resolve_stack_fault(fault, masked()), StackFault::Committed);
                    served.store(true, Ordering::Release);
                });
                // The fault has no pool lock to wait for; give it ample
                // time to finish while this thread still holds the lock.
                for _ in 0..200 {
                    if served.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(core::time::Duration::from_millis(10));
                }
                assert!(
                    served.load(Ordering::Acquire),
                    "the masked fault was not served while the pool lock was held: it touched \
                     the pool"
                );
            });
        });
        assert_eq!(
            reserved_bytes(),
            (frame_reserve::RESERVE_CAPACITY - THIRTY_PAGE_FAULT_RUN) * PhysFrame::SIZE,
            "the fault's frames came out of the reserve, and the reserve is not refilled from \
             fault context"
        );
        assert_eq!(
            mapped_pages(),
            pages_between(stack.range().end - 30 * PhysFrame::SIZE, stack.range().end)
        );
    }

    #[test]
    #[should_panic(expected = "already maps")]
    fn a_fault_on_a_page_the_stack_already_maps_is_a_bookkeeping_defect() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let range = stack.range();

        let _ = resolve_stack_fault(VirtAddr::new(range.end - PhysFrame::SIZE), enabled());
    }

    #[test]
    fn a_dropped_stack_gives_its_slot_back_and_the_arena_refuses_more_than_it_has() {
        arena();
        let held: alloc::vec::Vec<FiberStack> = (0..TEST_SLOTS)
            .map(|_| claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot"))
            .collect();
        assert_eq!(
            claim_fiber_stack(TEST_STACK_BYTES).err(),
            Some(FiberStackError::Exhausted { slots: TEST_SLOTS })
        );

        drop(held);
        // Two untouched stacks fit the retain budget and stay warm; the
        // third does not and is given back.
        assert_eq!(ENDED.load(Ordering::Relaxed), 1);
        let stats = fiber_stack_arena_stats().expect("stats");
        assert_eq!(stats.live_slots, 0);
        assert_eq!(stats.warm_slots, 2);
        assert_eq!(stats.retained_bytes, 2 * ENTRY_BYTES);
        assert_eq!(stats.released_stacks, TEST_SLOTS as u64);
        assert_eq!(
            stats.released_committed_bytes,
            (TEST_SLOTS * ENTRY_BYTES) as u64,
            "an untouched stack cost its entry window and nothing more"
        );
        // And the slots are usable again: a warm one first, as it stands.
        let prepared = PREPARED.load(Ordering::Relaxed);
        let committed = COMMITTED.load(Ordering::Relaxed);
        let reused = claim_fiber_stack(TEST_STACK_BYTES).expect("a slot came back");
        let stats = fiber_stack_arena_stats().expect("stats");
        assert_eq!(stats.live_slots, 1);
        assert_eq!(stats.warm_slots, 1);
        assert_eq!(stats.retained_bytes, ENTRY_BYTES);
        assert_eq!(PREPARED.load(Ordering::Relaxed), prepared);
        assert_eq!(COMMITTED.load(Ordering::Relaxed), committed);
        assert_eq!(reused.range().end - reused.range().start, TEST_STACK_BYTES);
    }

    #[test]
    fn a_warm_slot_serves_the_next_stack_without_touching_the_address_space_or_the_pool() {
        // Room for a stack that grew, so it is kept rather than given back.
        arena_retaining(TEST_STACK_BYTES);
        let first = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let range = first.range();
        assert_eq!(
            resolve_stack_fault(VirtAddr::new(range.end - 20 * PhysFrame::SIZE), enabled()),
            StackFault::Committed
        );
        // The entry window, the skipped gap and the faulting page.
        let held = 20 * PhysFrame::SIZE;
        drop(first);
        let stats = fiber_stack_arena_stats().expect("stats");
        assert_eq!(stats.warm_slots, 1);
        assert_eq!(stats.retained_bytes, held);
        assert_eq!(ENDED.load(Ordering::Relaxed), 0);

        // Nothing is left in the pool: a warm claim has to need none.
        let _hoard = Hoard::exhaust_pool_leaving(0);
        let prepared = PREPARED.load(Ordering::Relaxed);
        let committed = COMMITTED.load(Ordering::Relaxed);
        let second = claim_fiber_stack(TEST_STACK_BYTES).expect("the warm slot");
        assert_eq!(second.range(), range, "the warm slot is the one handed out");
        assert_eq!(PREPARED.load(Ordering::Relaxed), prepared);
        assert_eq!(COMMITTED.load(Ordering::Relaxed), committed);
        let stats = fiber_stack_arena_stats().expect("stats");
        assert_eq!(stats.warm_slots, 0);
        assert_eq!(stats.retained_bytes, 0);
        assert_eq!(
            stats.committed_bytes, held,
            "the pages the first stack touched are still the second's"
        );
    }

    #[test]
    fn a_released_stack_hands_back_only_the_span_it_touched() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let range = stack.range();
        let page = range.end - 30 * PhysFrame::SIZE;
        assert_eq!(
            resolve_stack_fault(VirtAddr::new(page + 8), enabled()),
            StackFault::Committed
        );

        drop(stack);
        let lowest = page;
        assert_eq!(LAST_MAPPED.0.load(Ordering::Relaxed), lowest);
        assert_eq!(
            LAST_MAPPED.1.load(Ordering::Relaxed),
            range.end - lowest,
            "the address space walks from the watermark to the top and nothing below"
        );
        assert!(
            mapped_pages().is_empty(),
            "every frame the stack held went back to the address space's owner"
        );
        let stats = fiber_stack_arena_stats().expect("stats");
        assert_eq!(stats.released_stacks, 1);
        assert_eq!(stats.released_committed_bytes, (range.end - lowest) as u64);
    }

    #[test]
    fn a_stack_of_the_wrong_size_is_refused_rather_than_trimmed() {
        arena();
        assert_eq!(
            claim_fiber_stack(TEST_STACK_BYTES * 2).err(),
            Some(FiberStackError::WrongSize {
                requested: TEST_STACK_BYTES * 2,
                slot: TEST_STACK_BYTES,
            })
        );
    }

    #[test]
    fn every_processor_counts_its_own_demand_commits() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let range = stack.range();
        assert_eq!(
            resolve_stack_fault(VirtAddr::new(range.end - 20 * PhysFrame::SIZE), enabled()),
            StackFault::Committed
        );

        // A test runs on one processor; the other's counter has to stay
        // where it was, or the counters are not per processor.
        let here = current_processor();
        let other = ProcessorId::new(u16::from(here.id() == 0));
        assert_eq!(fiber_stack_demand_commits_on(here), 1);
        assert_eq!(fiber_stack_demand_commits_on(other), 0);
        assert_eq!(fiber_stack_arena_stats().expect("stats").demand_commits, 1);
    }

    /// The fault that needs 30 pages, and the page it faults on.
    fn thirty_page_fault(stack: &FiberStack) -> VirtAddr {
        VirtAddr::new(stack.range().end - 30 * PhysFrame::SIZE + 8)
    }

    #[test]
    fn an_enabled_fault_with_the_pool_exhausted_is_starved_and_changes_nothing() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let fault = thirty_page_fault(&stack);
        let mapped_before = mapped_pages();
        let stats_before = fiber_stack_arena_stats().expect("stats");

        // One frame short of what the fault needs.
        let mut hoard = Hoard::exhaust_pool_leaving(THIRTY_PAGE_FAULT_RUN - 1);
        let outcome = resolve_stack_fault(fault, enabled());
        let StackFault::Starved {
            slot,
            addr,
            shortfall,
        } = outcome
        else {
            panic!("a pool one frame short must starve the fault: {outcome:?}");
        };
        assert_eq!(slot, 0);
        assert_eq!(addr, fault.raw());
        assert_eq!(shortfall.requested_bytes, PhysFrame::SIZE);
        assert!(
            alloc::format!("{outcome}").contains("user pool is exhausted"),
            "the fatal report has to name it"
        );

        assert_eq!(mapped_pages(), mapped_before, "nothing was mapped");
        let stats = fiber_stack_arena_stats().expect("stats");
        assert_eq!(stats.committed_bytes, stats_before.committed_bytes);
        assert_eq!(stats.demand_commits, stats_before.demand_commits);
        assert_eq!(stats.demand_committed_pages, 0);
        assert_eq!(
            frames_in_pool(),
            THIRTY_PAGE_FAULT_RUN - 1,
            "the frames the fault had taken went back to the pool"
        );

        // And it is recoverable: with the last frame back the same fault
        // is served.
        hoard.give_back(1);
        assert_eq!(resolve_stack_fault(fault, enabled()), StackFault::Committed);
        assert_eq!(
            mapped_pages().len(),
            mapped_before.len() + THIRTY_PAGE_FAULT_RUN,
            "the whole run arrives together"
        );
    }

    #[test]
    fn a_cold_claim_with_the_pool_exhausted_is_a_typed_error_and_leaves_the_reserve_alone() {
        arena();
        full_reserve();
        // One frame short of the entry window.
        let mut hoard = Hoard::exhaust_pool_leaving(ENTRY_WINDOW_PAGES - 1);

        let refusal = claim_fiber_stack(TEST_STACK_BYTES).err();
        let Some(FiberStackError::OutOfMemory(shortfall)) = refusal else {
            panic!("a pool one frame short of the window must refuse the claim: {refusal:?}");
        };
        assert_eq!(shortfall.requested_bytes, PhysFrame::SIZE);
        assert_eq!(
            PREPARED.load(Ordering::Relaxed),
            0,
            "nothing of the slot is touched before its entry window is in hand"
        );
        assert!(mapped_pages().is_empty());
        assert_eq!(
            frames_in_pool(),
            ENTRY_WINDOW_PAGES - 1,
            "the frames the claim had taken went back to the pool"
        );
        let stats = fiber_stack_arena_stats().expect("stats");
        assert_eq!((stats.live_slots, stats.warm_slots), (0, 0));
        assert_eq!(
            reserved_bytes(),
            frame_reserve::RESERVE_CAPACITY * PhysFrame::SIZE,
            "a claim does not draw on the page-fault reserve"
        );

        // The slot is as found: with the last frame back the same claim
        // takes it, and commits the whole window.
        hoard.give_back(1);
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("the slot is still cold");
        assert_eq!(
            stack.range().start,
            TEST_ARENA_BASE + FIBER_STACK_GUARD_BYTES,
            "the refused claim left the lowest slot free"
        );
        assert_eq!(mapped_pages().len(), ENTRY_WINDOW_PAGES);
    }

    #[test]
    fn an_enabled_fault_waits_for_a_pool_lock_another_processor_holds_and_is_served_with_the_reserve_empty()
     {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let fault = thirty_page_fault(&stack);
        assert_eq!(reserved_bytes(), 0, "this processor's reserve is empty");
        // Frames the pool holds in its fallback shard, so the faulting
        // processor's own shard is empty and the allocation has to take
        // the buddy lock.
        let mut hoard = Hoard::exhaust_pool_leaving(0);
        hoard.give_back(64);
        let served = core::sync::atomic::AtomicBool::new(false);

        std::thread::scope(|scope| {
            test_processor_identity::set(ProcessorId::new(1));
            let pool = super::super::user::installed_user_memory_pool().expect("pool");
            pool.hold_buddy_lock_while(|| {
                scope.spawn(|| {
                    // Processor 0 faults while processor 1 is inside the
                    // pool's critical section.
                    test_processor_identity::set(ProcessorId::new(0));
                    assert_eq!(resolve_stack_fault(fault, enabled()), StackFault::Committed);
                    served.store(true, Ordering::Release);
                });
                std::thread::sleep(core::time::Duration::from_millis(200));
                assert!(
                    !served.load(Ordering::Acquire),
                    "the fault was served, or refused, while the pool lock was held: it did not \
                     wait for it"
                );
            });
        });
        assert!(served.load(Ordering::Acquire));
        assert_eq!(
            mapped_pages().len(),
            ENTRY_WINDOW_PAGES + THIRTY_PAGE_FAULT_RUN
        );
    }

    #[test]
    fn the_refill_waits_out_a_pool_lock_another_processor_holds_and_fills_the_reserve() {
        arena();
        let processor = ProcessorId::new(0);
        assert_eq!(reserved_bytes(), 0, "the reserve starts empty");
        let filled = core::sync::atomic::AtomicBool::new(false);

        std::thread::scope(|scope| {
            test_processor_identity::set(ProcessorId::new(1));
            let pool = super::super::user::installed_user_memory_pool().expect("pool");
            pool.hold_buddy_lock_while(|| {
                scope.spawn(|| {
                    test_processor_identity::set(processor);
                    frame_reserve::top_up(processor);
                    filled.store(true, Ordering::Release);
                });
                std::thread::sleep(core::time::Duration::from_millis(200));
                assert!(
                    !filled.load(Ordering::Acquire),
                    "the refill returned while the pool's lock was held: it gave up instead of \
                     waiting"
                );
            });
        });
        assert_eq!(
            reserved_bytes(),
            frame_reserve::RESERVE_CAPACITY * PhysFrame::SIZE,
            "the refill filled the reserve once the lock came free"
        );
    }

    /// A stack pointer two pages and a little below the top of `stack`,
    /// where host code with the entry window under it has run.
    fn entry_sp(stack: &FiberStack) -> usize {
        stack.range().end - 2 * PhysFrame::SIZE - 100
    }

    /// `R + G` in pages: what a reserve commit keeps below the stack
    /// pointer, rounded as the commit rounds it.
    const RESERVE_AND_STEP_PAGES: usize =
        (FIBER_STACK_HOST_RESERVE + FIBER_STACK_COMMIT_STEP) / PhysFrame::SIZE;

    #[test]
    fn a_reserve_commit_maps_exactly_the_run_below_the_reserve_and_answers_the_limit() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let top = stack.range().end;
        let sp = entry_sp(&stack);
        let committed_before = COMMITTED.load(Ordering::Relaxed);

        let limit = commit_fiber_stack_reserve(sp).expect("the pool has room");

        // `sp - R - G` is two pages and 100 bytes plus R + G below the
        // top, which the commit rounds down to the page.
        let need = top - (2 + RESERVE_AND_STEP_PAGES + 1) * PhysFrame::SIZE;
        assert_eq!(
            mapped_pages(),
            pages_between(need, top),
            "the committed range is one run from the page below `sp - R - G` to the top"
        );
        assert_eq!(
            COMMITTED.load(Ordering::Relaxed) - committed_before,
            2 + RESERVE_AND_STEP_PAGES + 1 - ENTRY_WINDOW_PAGES
        );
        assert_eq!(
            limit,
            need + FIBER_STACK_HOST_RESERVE,
            "the answer is the new floor plus the reserve"
        );
        assert!(
            limit <= sp,
            "the reserve limit never lies above the stack pointer"
        );
        let stats = fiber_stack_arena_stats().expect("stats");
        assert_eq!(stats.committed_bytes, top - need);
        assert_eq!(stats.reserve_commits, 1);
        assert_eq!(
            stats.reserve_committed_pages,
            (2 + RESERVE_AND_STEP_PAGES + 1 - ENTRY_WINDOW_PAGES) as u64
        );
        assert_eq!(
            (stats.demand_commits, stats.demand_committed_pages),
            (0, 0),
            "a reserve commit is not a fault"
        );
        assert_eq!(
            reserved_bytes(),
            0,
            "a reserve commit draws on the page-fault reserve never and fills it never"
        );
    }

    #[test]
    fn a_covered_reserve_commit_takes_no_frame_and_answers_the_floor_it_found() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let first = commit_fiber_stack_reserve(entry_sp(&stack)).expect("the pool has room");
        let mapped = mapped_pages();
        let committed = COMMITTED.load(Ordering::Relaxed);
        let in_pool = frames_in_pool();
        let stats = fiber_stack_arena_stats().expect("stats");

        // A stack pointer above the one already served needs nothing new.
        let again = commit_fiber_stack_reserve(entry_sp(&stack) + PhysFrame::SIZE)
            .expect("covered by what is committed");

        assert_eq!(again, first, "the answer is the watermark plus the reserve");
        assert_eq!(mapped_pages(), mapped);
        assert_eq!(COMMITTED.load(Ordering::Relaxed), committed);
        assert_eq!(frames_in_pool(), in_pool, "no frame left the pool");
        let after = fiber_stack_arena_stats().expect("stats");
        assert_eq!(after.reserve_commits, stats.reserve_commits);
        assert_eq!(after.reserve_committed_pages, stats.reserve_committed_pages);
    }

    #[test]
    fn a_reserve_commit_with_the_pool_exhausted_maps_nothing_and_leaves_the_watermark() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let sp = entry_sp(&stack);
        let mapped_before = mapped_pages();
        let stats_before = fiber_stack_arena_stats().expect("stats");
        let needed = 2 + RESERVE_AND_STEP_PAGES + 1 - ENTRY_WINDOW_PAGES;

        // One frame short of the run.
        let mut hoard = Hoard::exhaust_pool_leaving(needed - 1);
        let refusal = commit_fiber_stack_reserve(sp);
        let Err(StackReserveError::OutOfMemory(shortfall)) = refusal else {
            panic!("a pool one frame short must refuse the commit: {refusal:?}");
        };
        assert_eq!(shortfall.requested_bytes, PhysFrame::SIZE);

        assert_eq!(mapped_pages(), mapped_before, "nothing was mapped");
        let stats = fiber_stack_arena_stats().expect("stats");
        assert_eq!(
            stats.committed_bytes, stats_before.committed_bytes,
            "the watermark did not move"
        );
        assert_eq!(stats.reserve_commits, 0);
        assert_eq!(stats.reserve_committed_pages, 0);
        assert_eq!(
            frames_in_pool(),
            needed - 1,
            "the frames the commit had taken went back to the pool"
        );

        // And it is recoverable: with the last frame back the same commit
        // is served, the whole run arriving together.
        hoard.give_back(1);
        commit_fiber_stack_reserve(sp).expect("the pool has the run again");
        assert_eq!(mapped_pages().len(), mapped_before.len() + needed);
    }

    #[test]
    fn a_reserve_that_does_not_fit_above_the_guard_is_a_stack_overflow() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let lowest = stack.range().start;
        let mapped_before = mapped_pages();
        let reserve = FIBER_STACK_HOST_RESERVE + FIBER_STACK_COMMIT_STEP;

        // `sp - R` still lies in the body, `sp - R - G` does not.
        let sp = lowest + FIBER_STACK_HOST_RESERVE + 100;
        assert_eq!(
            commit_fiber_stack_reserve(sp),
            Err(StackReserveError::Overflow {
                slot: 0,
                sp,
                reserve,
                lowest,
            })
        );
        // And with `sp - R` below it too.
        let sp = lowest + 100;
        assert!(matches!(
            commit_fiber_stack_reserve(sp),
            Err(StackReserveError::Overflow { .. })
        ));
        assert_eq!(mapped_pages(), mapped_before, "nothing was mapped");
        assert_eq!(
            fiber_stack_arena_stats().expect("stats").committed_bytes,
            ENTRY_BYTES
        );

        // The last stack pointer that fits: the reserve and the step end
        // exactly at the body's bottom, and the commit reaches it.
        let limit = commit_fiber_stack_reserve(lowest + reserve).expect("exactly fits");
        assert_eq!(limit, lowest + FIBER_STACK_HOST_RESERVE);
        assert_eq!(
            mapped_pages(),
            pages_between(lowest, stack.range().end),
            "the whole body is committed and the guard never is"
        );
    }

    #[test]
    #[should_panic(expected = "not inside the body of a fiber stack slot")]
    fn a_reserve_commit_outside_the_arena_is_the_runtime_calling_off_a_fiber_stack() {
        arena();
        let _stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");

        let _ = commit_fiber_stack_reserve(TEST_ARENA_BASE - PhysFrame::SIZE);
    }

    #[test]
    #[should_panic(expected = "not inside the body of a fiber stack slot")]
    fn a_reserve_commit_in_a_guard_is_the_runtime_calling_off_a_fiber_stack() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");

        let _ = commit_fiber_stack_reserve(stack.range().start - PhysFrame::SIZE);
    }

    #[test]
    #[should_panic(expected = "which no live stack owns")]
    fn a_reserve_commit_in_a_slot_nobody_owns_is_the_runtime_running_on_a_stack_it_gave_back() {
        arena();
        let _stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");

        // The second slot's body: nothing claimed it.
        let _ = commit_fiber_stack_reserve(TEST_ARENA_BASE + 2 * TEST_SLOT_BYTES - PhysFrame::SIZE);
    }

    #[test]
    fn a_reserve_commit_waits_for_a_pool_lock_another_processor_holds() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let sp = entry_sp(&stack);
        // Frames the pool holds in its fallback shard, so the committing
        // processor's own shard is empty and the allocation has to take
        // the buddy lock.
        let mut hoard = Hoard::exhaust_pool_leaving(0);
        hoard.give_back(64);
        let served = core::sync::atomic::AtomicBool::new(false);

        std::thread::scope(|scope| {
            test_processor_identity::set(ProcessorId::new(1));
            let pool = super::super::user::installed_user_memory_pool().expect("pool");
            pool.hold_buddy_lock_while(|| {
                scope.spawn(|| {
                    // Processor 0 commits while processor 1 is inside the
                    // pool's critical section.
                    test_processor_identity::set(ProcessorId::new(0));
                    commit_fiber_stack_reserve(sp).expect("served once the lock is free");
                    served.store(true, Ordering::Release);
                });
                std::thread::sleep(core::time::Duration::from_millis(200));
                assert!(
                    !served.load(Ordering::Acquire),
                    "the commit was served, or refused, while the pool lock was held: it did not \
                     wait for it"
                );
            });
        });
        assert!(served.load(Ordering::Acquire));
        assert_eq!(
            mapped_pages().len(),
            2 + RESERVE_AND_STEP_PAGES + 1,
            "the whole run arrived"
        );
    }

    /// The failed instantiation or call has to classify as out of memory:
    /// the runtime wraps the creator's error in its own, and the
    /// classifier walks the chain for the pool's refusal.
    #[cfg(feature = "wasmtime-runtime")]
    #[test]
    fn the_runtimes_error_chain_reaches_the_pools_refusal() {
        let refusal = ProgramOutOfMemory {
            requested_bytes: PhysFrame::SIZE,
            available_bytes: 0,
            pool_bytes: POOL_BYTES,
            reserved_bytes: 0,
        };
        let error = wasmtime::Error::new(FiberStackError::OutOfMemory(refusal));
        assert!(
            error.chain().any(|cause| cause.is::<ProgramOutOfMemory>()),
            "the creator's error reaches the runtime with the refusal in its chain"
        );
    }
}
