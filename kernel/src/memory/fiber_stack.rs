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
//! the direction a stack grows. Only the top page of a fresh stack is
//! committed; every page below it arrives when something faults on it,
//! and when the stack is dropped the slot keeps those pages for the next
//! stack while a machine-wide budget has room for them, or gives them
//! back when it does not.
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
//! A fault-time commit runs on the faulting stack's own processor,
//! inside whatever the interrupted code was doing. Kernel host calls run
//! on fiber stacks, so the interrupted code may hold the address-space
//! tracker's lock, the frame pool's, or the kernel heap's — and an
//! exception is not an interrupt, so masking buys nothing. The handler
//! therefore has to resolve the fault without taking a lock and without
//! allocating, which is what the arena's shape is for:
//!
//! - Classifying an address is arithmetic. Subtract the base, divide by
//!   the slot stride, compare against the guard length, read one atomic
//!   flag. No lookup, no tracker, no list.
//! - The page tables above the leaf already exist, because
//!   [`hal::vmm::AddressSpace::prepare_demand_commit`](helios_hal::vmm::AddressSpace::prepare_demand_commit)
//!   built them on the ordinary locked path when the stack was created.
//!   The fault writes one leaf entry into tables nothing is editing.
//! - The frame comes from [`super::frame_reserve`], which the executor
//!   loop keeps full outside fault context.
//!
//! # Concurrency contract
//!
//! The arena is machine-wide and every one of its operations may arrive
//! from any processor:
//!
//! - `claim` and the release in [`FiberStack`]'s `Drop` run on the
//!   ordinary path with no fault outstanding. They take the address
//!   space's own lock through the hooks, which is what makes them the
//!   *locked* half.
//! - [`resolve_stack_fault`] runs in fault context on the processor that
//!   faulted, and takes no lock at all.
//!
//! One fiber runs on one processor at a time, so the pages of a given
//! slot are only ever faulted in by one processor at a time; the atomics
//! below are what keep that true against a fiber that migrates between
//! polls, and what keep a claim on one processor from racing a release
//! on another. Per-processor counters sit on their own cache lines.
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

use super::frame_reserve;

/// Bytes of unmapped address space below every fiber stack.
///
/// One page would catch a walk off the bottom of the stack, and nothing
/// else. A frame larger than the guard could step over it and land in
/// the slot below, where the fault would look like an ordinary demand
/// commit and the overflow would go unnoticed; 64 KiB is larger than any
/// frame either Cranelift or the kernel's own Rust code builds, so an
/// overflow always lands inside it.
pub const FIBER_STACK_GUARD_BYTES: usize = 64 * 1024;

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
    /// A reserved page inside a live stack, committed in place. The
    /// faulting instruction runs again.
    Committed,
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
}

/// One slot's mutable state.
///
/// `live` is written by the claim that takes the slot and by the release
/// that gives it back, from any processor. `watermark` is written by the
/// fault path on whichever processor is running the fiber, and read by
/// the release; it is the lowest address in this slot anything has ever
/// faulted on, and `body_top` when nothing has.
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
    /// Demand commits this processor has resolved. One counter per
    /// processor, each on its own line, because every one of them is
    /// written from fault context on its own processor.
    demand_commits: Box<[CachePadded<AtomicU64>]>,
    /// Whether the console has already been told this processor is
    /// resolving commits. Written from the release path, which is a
    /// place a `tracing` call is allowed; see [`Self::announce_processors`].
    announced: Box<[CachePadded<AtomicBool>]>,
    /// What every released stack cost, summed. Written on the release
    /// path from whichever processor tears the store down, which is why
    /// the pair sits on its own line rather than beside a per-processor
    /// counter.
    released: CachePadded<ReleasedTotals>,
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
            })
        });
        let mut counters = Vec::with_capacity(processor_count);
        counters.resize_with(processor_count, || CachePadded::new(AtomicU64::new(0)));
        let mut announced = Vec::with_capacity(processor_count);
        announced.resize_with(processor_count, || CachePadded::new(AtomicBool::new(false)));
        tracing::info!(
            target: "helios_kernel::fiber_stack",
            base = range.start.raw(),
            slots,
            stack_bytes,
            guard_bytes = FIBER_STACK_GUARD_BYTES,
            reserved_bytes = bytes,
            retain_bytes,
            "fiber stack arena reserved; stacks commit on demand"
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
            announced: announced.into_boxed_slice(),
            released: CachePadded::new(ReleasedTotals {
                stacks: AtomicU64::new(0),
                committed_bytes: AtomicU64::new(0),
            }),
        }
    });
}

/// Whether this machine has an arena, and so anything that draws on the
/// page-fault frame reserve.
pub(super) fn arena_installed() -> bool {
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
/// This is what a backend's fault entry calls. It takes no lock, walks
/// no list and allocates nothing; see the module docs for why it may
/// not.
pub fn resolve_stack_fault(addr: VirtAddr) -> StackFault {
    let Some(arena) = ARENA.get() else {
        return StackFault::Elsewhere;
    };
    arena.resolve(addr)
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

    fn resolve(&self, addr: VirtAddr) -> StackFault {
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
        let page = VirtAddr::new(raw & !(PhysFrame::SIZE - 1));
        let processor = current_processor();
        let frame = frame_reserve::take_frame(processor);
        match (self.hooks.commit_demand_page)(page, frame, PageFlags::READ | PageFlags::WRITE) {
            Ok(()) => {}
            Err(error) => {
                panic!(
                    "page fault at {raw:#x} in fiber stack slot {index} could not be committed \
                     in place: {error}"
                );
            }
        }
        slot.watermark.fetch_min(page.raw(), Ordering::AcqRel);
        self.demand_commits[usize::from(processor.id())].fetch_add(1, Ordering::Relaxed);
        // Nothing is logged from here. `tracing` reaches the debug
        // console, which is a lock the interrupted code may be holding
        // on this very processor; the counter above is what the release
        // path reports instead.
        StackFault::Committed
    }

    /// Takes a slot: a warm one as it stands, or a cold one prepared
    /// with the one page the runtime writes first.
    fn claim(&'static self, stack_bytes: usize) -> Result<FiberStack, FiberStackError> {
        if stack_bytes != self.stack_bytes {
            return Err(FiberStackError::WrongSize {
                requested: stack_bytes,
                slot: self.stack_bytes,
            });
        }
        let processor = current_processor();
        if let Some(index) = self.take_slot(SLOT_WARM) {
            // The pages its last stack touched are still mapped, so the
            // runtime's first frame lands on a committed page and nothing
            // here touches the address space.
            self.retained_bytes
                .fetch_sub(self.committed_bytes(index), Ordering::AcqRel);
            frame_reserve::top_up(processor);
            return Ok(FiberStack { arena: self, index });
        }
        let index = self
            .take_slot(SLOT_COLD)
            .ok_or(FiberStackError::Exhausted {
                slots: self.slots.len(),
            })?;
        let body = self.body(index);
        let flags = PageFlags::READ | PageFlags::WRITE;
        if let Err(error) = (self.hooks.prepare_demand_commit)(body, flags) {
            self.slots[index].state.store(SLOT_COLD, Ordering::Release);
            return Err(FiberStackError::Prepare(error));
        }
        let body_top = body.end().raw();
        self.slots[index]
            .watermark
            .store(body_top, Ordering::Release);
        // The runtime writes the fiber's initial frame at the top of the
        // stack, so that page is committed here rather than left to a
        // fault: the write happens before anything has run on the stack,
        // and paying for it on the locked path keeps the very first
        // fault a stack the runtime is already using.
        let top_page = VirtAddr::new(body_top - PhysFrame::SIZE);
        let frame = frame_reserve::take_frame(processor);
        if let Err(error) = (self.hooks.commit_demand_page)(top_page, frame, flags) {
            (self.hooks.end_demand_commit)(body, body).unwrap_or_else(|cleanup| {
                panic!(
                    "fiber stack slot {index} could not be given back after its top page was \
                     refused ({error}): {cleanup}"
                )
            });
            self.slots[index].state.store(SLOT_COLD, Ordering::Release);
            return Err(FiberStackError::Prepare(error));
        }
        self.slots[index]
            .watermark
            .store(top_page.raw(), Ordering::Release);
        // Creating a stack is outside fault context, so it is one of the
        // two places that may refill the reserve the ordinary way.
        frame_reserve::top_up(processor);
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
            released_stacks,
            released_committed_bytes: self.released.committed_bytes.load(Ordering::Relaxed),
            released_eager_bytes: released_stacks * self.stack_bytes as u64,
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

    use helios_hal::cpu::ProcessorId;

    use super::*;

    /// Bytes of pretend user memory the pool is built over. It has to
    /// cover the reserve's own fill as well as the pages the test
    /// faults in.
    const POOL_BYTES: usize = 4 * 1024 * 1024;
    /// A stack small enough to walk in a test and still several pages
    /// deep, so a fault below the top page is a real demand commit.
    const TEST_STACK_BYTES: usize = 16 * PhysFrame::SIZE;
    const TEST_SLOTS: usize = 3;
    /// Two, so that a commit resolved on the test's processor can be
    /// shown to land on that processor's counter and no other.
    const TEST_PROCESSORS: usize = 2;
    /// Two pages of warm slots: enough to keep two untouched stacks and
    /// too little for one that grew.
    const TEST_RETAIN_BYTES: usize = 2 * PhysFrame::SIZE;
    /// Where the fake address space puts the arena. Any page-aligned
    /// value works; nothing dereferences it.
    const TEST_ARENA_BASE: usize = 0x0000_4000_0000_0000;

    static PREPARED: AtomicUsize = AtomicUsize::new(0);
    static COMMITTED: AtomicUsize = AtomicUsize::new(0);
    static ENDED: AtomicUsize = AtomicUsize::new(0);
    /// The `mapped` range of the most recent `end_demand_commit`, as
    /// `(start, byte_len)`.
    static LAST_MAPPED: (AtomicUsize, AtomicUsize) = (AtomicUsize::new(0), AtomicUsize::new(0));

    /// An address space that records what the arena asked of it.
    ///
    /// The arena's own logic is the geometry, the slot bookkeeping and
    /// the fault classification; what the page tables do with the
    /// result is each backend's, and is covered by booting them.
    static TEST_HOOKS: FiberStackVmHooks = FiberStackVmHooks {
        reserve: |bytes| {
            assert_eq!(
                bytes,
                TEST_SLOTS * (FIBER_STACK_GUARD_BYTES + TEST_STACK_BYTES)
            );
            Ok(VirtRange::new(VirtAddr::new(TEST_ARENA_BASE), bytes))
        },
        prepare_demand_commit: |virt, flags| {
            assert_eq!(virt.byte_len, TEST_STACK_BYTES);
            assert_eq!(flags, PageFlags::READ | PageFlags::WRITE);
            PREPARED.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
        commit_demand_page: |addr, _frame, flags| {
            assert!(addr.is_page_aligned());
            assert_eq!(flags, PageFlags::READ | PageFlags::WRITE);
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
            Ok(())
        },
    };

    /// Brings up the one-processor user pool the frame reserve draws on
    /// and the arena on top of it. Each test runs in its own process,
    /// so the install-once statics are fresh every time.
    fn arena() {
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
        install_fiber_stack_arena(
            TEST_SLOTS,
            TEST_STACK_BYTES,
            TEST_PROCESSORS,
            TEST_RETAIN_BYTES,
        );
    }

    #[test]
    fn a_fresh_stack_commits_only_its_top_page_and_reports_its_guard() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");

        assert_eq!(PREPARED.load(Ordering::Relaxed), 1);
        assert_eq!(
            COMMITTED.load(Ordering::Relaxed),
            1,
            "a fresh stack pays for one page, not for the span it may grow into"
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
    fn a_fault_below_the_top_commits_that_page_and_a_guard_hit_does_not() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let range = stack.range();
        let committed_before = COMMITTED.load(Ordering::Relaxed);

        // Four pages below the top: the stack grew past its first page.
        let deep = VirtAddr::new(range.end - 4 * PhysFrame::SIZE + 8);
        assert_eq!(resolve_stack_fault(deep), StackFault::Committed);
        assert_eq!(COMMITTED.load(Ordering::Relaxed), committed_before + 1);

        // And a second fault on the same page is a second commit only
        // because nothing here maps anything; what matters is that the
        // watermark followed the deepest address.
        let stats = fiber_stack_arena_stats().expect("arena stats");
        assert_eq!(stats.live_slots, 1);
        assert_eq!(stats.committed_bytes, 4 * PhysFrame::SIZE);
        assert_eq!(stats.eager_bytes, TEST_STACK_BYTES);
        assert!(stats.demand_commits >= 1);

        let guard = VirtAddr::new(range.start - PhysFrame::SIZE);
        let outcome = resolve_stack_fault(guard);
        assert!(
            matches!(outcome, StackFault::Guard { .. }),
            "a guard hit is a stack overflow, never a commit: {outcome:?}"
        );
        assert!(
            alloc::format!("{outcome}").contains("guard page"),
            "the fatal report has to name it"
        );
    }

    #[test]
    fn an_address_outside_the_arena_is_nobody_else_business() {
        arena();
        assert_eq!(
            resolve_stack_fault(VirtAddr::new(TEST_ARENA_BASE - PhysFrame::SIZE)),
            StackFault::Elsewhere
        );
        let past = TEST_ARENA_BASE + TEST_SLOTS * (FIBER_STACK_GUARD_BYTES + TEST_STACK_BYTES);
        assert_eq!(
            resolve_stack_fault(VirtAddr::new(past)),
            StackFault::Elsewhere
        );
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
        assert_eq!(stats.retained_bytes, 2 * PhysFrame::SIZE);
        assert_eq!(stats.released_stacks, TEST_SLOTS as u64);
        assert_eq!(
            stats.released_committed_bytes,
            (TEST_SLOTS * PhysFrame::SIZE) as u64,
            "an untouched stack cost its top page and nothing more"
        );
        // And the slots are usable again: a warm one first, as it stands.
        let prepared = PREPARED.load(Ordering::Relaxed);
        let committed = COMMITTED.load(Ordering::Relaxed);
        let reused = claim_fiber_stack(TEST_STACK_BYTES).expect("a slot came back");
        let stats = fiber_stack_arena_stats().expect("stats");
        assert_eq!(stats.live_slots, 1);
        assert_eq!(stats.warm_slots, 1);
        assert_eq!(stats.retained_bytes, PhysFrame::SIZE);
        assert_eq!(PREPARED.load(Ordering::Relaxed), prepared);
        assert_eq!(COMMITTED.load(Ordering::Relaxed), committed);
        assert_eq!(reused.range().end - reused.range().start, TEST_STACK_BYTES);
    }

    #[test]
    fn a_warm_slot_serves_the_next_stack_without_touching_the_address_space() {
        arena();
        let first = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let range = first.range();
        assert_eq!(
            resolve_stack_fault(VirtAddr::new(range.end - PhysFrame::SIZE - 8)),
            StackFault::Committed
        );
        drop(first);
        let stats = fiber_stack_arena_stats().expect("stats");
        assert_eq!(stats.warm_slots, 1);
        assert_eq!(stats.retained_bytes, 2 * PhysFrame::SIZE);
        assert_eq!(ENDED.load(Ordering::Relaxed), 0);

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
            stats.committed_bytes,
            2 * PhysFrame::SIZE,
            "the pages the first stack touched are still the second's"
        );
    }

    #[test]
    fn a_released_stack_hands_back_only_the_span_it_touched() {
        arena();
        let stack = claim_fiber_stack(TEST_STACK_BYTES).expect("a free slot");
        let range = stack.range();
        let deep = range.end - 4 * PhysFrame::SIZE + 8;
        assert_eq!(
            resolve_stack_fault(VirtAddr::new(deep)),
            StackFault::Committed
        );

        drop(stack);
        let deepest_page = deep & !(PhysFrame::SIZE - 1);
        assert_eq!(LAST_MAPPED.0.load(Ordering::Relaxed), deepest_page);
        assert_eq!(
            LAST_MAPPED.1.load(Ordering::Relaxed),
            range.end - deepest_page,
            "the address space walks from the watermark to the top and nothing below"
        );
        let stats = fiber_stack_arena_stats().expect("stats");
        assert_eq!(stats.released_stacks, 1);
        assert_eq!(
            stats.released_committed_bytes,
            (range.end - deepest_page) as u64
        );
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
            resolve_stack_fault(VirtAddr::new(range.end - 2 * PhysFrame::SIZE)),
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
}
