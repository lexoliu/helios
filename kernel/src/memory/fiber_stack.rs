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
//! and the whole slot is given back when the stack is dropped.
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
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

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
    /// Give one slot's body back, frames and record together.
    pub end_demand_commit: fn(VirtRange) -> Result<(), AddressSpaceError>,
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
struct FiberStackSlot {
    live: AtomicBool,
    watermark: AtomicUsize,
}

/// The machine's fiber stacks.
pub struct FiberStackArena {
    hooks: &'static FiberStackVmHooks,
    base: usize,
    slot_bytes: usize,
    stack_bytes: usize,
    slots: Box<[CachePadded<FiberStackSlot>]>,
    /// Where the next claim starts looking, so a machine with hundreds
    /// of live stacks does not rescan the live prefix every time.
    claim_cursor: AtomicUsize,
    /// Demand commits this processor has resolved. One counter per
    /// processor, each on its own line, because every one of them is
    /// written from fault context on its own processor.
    demand_commits: Box<[CachePadded<AtomicU64>]>,
    /// Whether the console has already been told this processor is
    /// resolving commits. Written from the release path, which is a
    /// place a `tracing` call is allowed; see [`Self::announce_processors`].
    announced: Box<[CachePadded<AtomicBool>]>,
}

/// What the arena is holding, for the boot log and the stats panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FiberStackArenaStats {
    /// Slots the arena has in total.
    pub slots: usize,
    /// Slots a live stack is using right now.
    pub live_slots: usize,
    /// Bytes of user memory the live stacks have actually faulted in.
    pub committed_bytes: usize,
    /// Bytes of user memory the same stacks would have cost committed
    /// up front.
    pub eager_bytes: usize,
    /// Demand commits resolved since boot, across every processor.
    pub demand_commits: u64,
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
pub fn install_fiber_stack_arena(slots: usize, stack_bytes: usize, processor_count: usize) {
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
                live: AtomicBool::new(false),
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
            "fiber stack arena reserved; stacks commit on demand"
        );
        FiberStackArena {
            hooks,
            base: range.start.raw(),
            slot_bytes,
            stack_bytes,
            slots: slot_states.into_boxed_slice(),
            claim_cursor: AtomicUsize::new(0),
            demand_commits: counters.into_boxed_slice(),
            announced: announced.into_boxed_slice(),
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
        if !slot.live.load(Ordering::Acquire) {
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

    /// Takes a free slot, prepares it, and commits the one page the
    /// runtime writes first.
    fn claim(&'static self, stack_bytes: usize) -> Result<FiberStack, FiberStackError> {
        if stack_bytes != self.stack_bytes {
            return Err(FiberStackError::WrongSize {
                requested: stack_bytes,
                slot: self.stack_bytes,
            });
        }
        let index = self.claim_slot()?;
        let body = self.body(index);
        let flags = PageFlags::READ | PageFlags::WRITE;
        if let Err(error) = (self.hooks.prepare_demand_commit)(body, flags) {
            self.slots[index].live.store(false, Ordering::Release);
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
        let processor = current_processor();
        let top_page = VirtAddr::new(body_top - PhysFrame::SIZE);
        let frame = frame_reserve::take_frame(processor);
        if let Err(error) = (self.hooks.commit_demand_page)(top_page, frame, flags) {
            (self.hooks.end_demand_commit)(body).unwrap_or_else(|cleanup| {
                panic!(
                    "fiber stack slot {index} could not be given back after its top page was \
                     refused ({error}): {cleanup}"
                )
            });
            self.slots[index].live.store(false, Ordering::Release);
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

    fn claim_slot(&self) -> Result<usize, FiberStackError> {
        let count = self.slots.len();
        let start = self.claim_cursor.load(Ordering::Relaxed) % count;
        for step in 0..count {
            let index = (start + step) % count;
            if self.slots[index]
                .live
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.claim_cursor.store(index + 1, Ordering::Relaxed);
                return Ok(index);
            }
        }
        Err(FiberStackError::Exhausted { slots: count })
    }

    /// Gives slot `index` back: every page a fault ever mapped goes back
    /// to the user pool, on the ordinary locked path with the shootdown
    /// that path already does.
    fn release(&self, index: usize) {
        let body = self.body(index);
        let committed_bytes = body
            .end()
            .raw()
            .saturating_sub(self.slots[index].watermark.load(Ordering::Acquire));
        (self.hooks.end_demand_commit)(body).unwrap_or_else(|error| {
            panic!(
                "fiber stack slot {index} at {:#x} could not be given back: {error}",
                body.start.raw()
            )
        });
        self.slots[index]
            .watermark
            .store(body.end().raw(), Ordering::Release);
        self.slots[index].live.store(false, Ordering::Release);
        // What this stack actually cost, against what it would have cost
        // committed up front. One line per store teardown, on the
        // ordinary locked path — the fault path itself logs nothing,
        // because `tracing` reaches the debug console's lock.
        tracing::info!(
            target: "helios_kernel::fiber_stack",
            slot = index,
            committed_bytes,
            stack_bytes = self.stack_bytes,
            "fiber stack released; it committed only the pages it touched"
        );
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
        let mut committed_bytes = 0;
        for (index, slot) in self.slots.iter().enumerate() {
            if !slot.live.load(Ordering::Acquire) {
                continue;
            }
            live_slots += 1;
            let top = self.body(index).end().raw();
            committed_bytes += top.saturating_sub(slot.watermark.load(Ordering::Acquire));
        }
        FiberStackArenaStats {
            slots: self.slots.len(),
            live_slots,
            committed_bytes,
            eager_bytes: live_slots * self.stack_bytes,
            demand_commits: self
                .demand_commits
                .iter()
                .map(|counter| counter.load(Ordering::Relaxed))
                .sum(),
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
    /// Where the fake address space puts the arena. Any page-aligned
    /// value works; nothing dereferences it.
    const TEST_ARENA_BASE: usize = 0x0000_4000_0000_0000;

    static PREPARED: AtomicUsize = AtomicUsize::new(0);
    static COMMITTED: AtomicUsize = AtomicUsize::new(0);
    static ENDED: AtomicUsize = AtomicUsize::new(0);

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
        end_demand_commit: |virt| {
            assert_eq!(virt.byte_len, TEST_STACK_BYTES);
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
        let base = unsafe { alloc::alloc::alloc_zeroed(layout) } as usize;
        assert!(base != 0, "pool backing");
        let pool =
            super::super::install_user_memory_pool(super::super::allocate_user_memory_pool());
        pool.initialize(&[(base, base + POOL_BYTES)]);
        pool.configure_processors(1);
        frame_reserve::configure_processors(1);
        install_fiber_stack_hooks(&TEST_HOOKS);
        install_fiber_stack_arena(TEST_SLOTS, TEST_STACK_BYTES, 1);
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
        assert_eq!(ENDED.load(Ordering::Relaxed), TEST_SLOTS);
        assert_eq!(fiber_stack_arena_stats().expect("stats").live_slots, 0);
        // And the slots are usable again.
        let _reused = claim_fiber_stack(TEST_STACK_BYTES).expect("a slot came back");
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
        resolve_stack_fault(VirtAddr::new(range.end - 2 * PhysFrame::SIZE));

        assert!(fiber_stack_demand_commits_on(ProcessorId::new(0)) >= 1);
    }
}
