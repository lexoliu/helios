//! The kernel heap's per-processor front: block magazines and the
//! allocation counters behind them.
//!
//! # Why the heap needs a front
//!
//! Every kernel allocation ends in one `IrqSafeMutex<Heap<ORDER>>`
//! (`crate::KernelAllocator`), and the buddy heap behind that lock is
//! not cheap even uncontended: a free walks its class's free list
//! looking for the block's buddy, and walks again at every class it
//! merges up through. On a machine whose executor, network service and
//! component host all allocate on every processor at once, that one
//! word is the machine's hottest.
//!
//! A magazine is the standard answer: each processor keeps a small
//! stack of recently freed blocks per size class and serves its own
//! allocations from it, so the shared heap sees one batch instead of
//! sixteen allocations. Blocks are fungible — a class-`k` block is
//! `1 << k` bytes at `1 << k` alignment, which is exactly what the
//! buddy heap hands out for that class — so a block one processor
//! allocated is a block any processor may later serve out of its own
//! magazine, and nothing has to be traced back to where it came from.
//!
//! # Concurrency contract
//!
//! A [`ProcessorFront`] belongs to one processor, named by the slot
//! [`helios_hal::cpu::current_processor_slot`] answers with. Its two
//! halves are not reached the same way:
//!
//! - **Owner only.** [`ProcessorFront::heads`] — the list heads, and
//!   the links threaded through the cached blocks themselves — is read
//!   and written by the owning processor and by nothing else. There is
//!   no lock on it and no atomic in it. What keeps the owner's own
//!   interrupt handler from re-entering it is the local mask
//!   `IrqSafeMutex` already takes for the heap: every method here that
//!   touches a head holds
//!   [`helios_hal::critical_section::with_local_interrupts_masked`] for
//!   the whole of it. With interrupts masked the owner cannot be
//!   preempted, so it cannot migrate mid-operation and no second
//!   processor can arrive.
//! - **Owner writes, anyone reads.** [`ProcessorFront::depths`] and
//!   [`ProcessorFront::counters`] are written by the owner and read by
//!   whichever processor answers `heap_stats()`. They are atomics for
//!   that reason alone: the owner steps them with a relaxed load, an
//!   add and a relaxed store — a plain read-modify-write on every
//!   target here and never a locked one, because a single writer needs
//!   no atomicity against itself. [`CounterStep`] names that, and names
//!   the locked step the one shared counter block takes instead.
//!
//! There is no cross-processor free path and no return queue, because
//! there is nothing for one to carry: a free is served by the magazine
//! of the processor the free runs on, whichever processor allocated the
//! block. A cached block has no owner to return to.
//!
//! Different processors write different fronts, so the array holds each
//! behind a [`CachePadded`] and one processor's allocation never
//! invalidates another's line.
//!
//! # Bring-up
//!
//! The array is sized once, by the processor count the backend hands
//! `helios_kernel::prime_bootstrap_allocator`, out of the kernel heap
//! that same call has just filled. Before that the heap serves every
//! allocation directly; afterwards a processor naming a slot the array
//! does not hold is a bring-up ordering bug and panics saying so. A
//! processor still carrying a bootstrapping identity names no slot at
//! all (see [`helios_hal::cpu::current_processor_slot`]) and takes the
//! same direct path, which is why nothing here has to guess one.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use arrayvec::ArrayVec;
use crossbeam_utils::CachePadded;
use helios_hal::cpu::ProcessorId;
use helios_hal::critical_section::with_local_interrupts_masked;
use spin::Once;

use crate::{HEAP_SIZE_CLASS_COUNT, HeapStats, heap_size_class, usize_to_u64};

/// The smallest block `buddy_system_allocator::Heap` hands out: its
/// free lists are threaded through the blocks themselves, so a block is
/// never narrower than the pointer that links it.
const MIN_BLOCK_BYTES: usize = size_of::<usize>();

/// Buddy order of [`MIN_BLOCK_BYTES`], and so the order of class zero.
const MIN_CACHED_ORDER: u32 = MIN_BLOCK_BYTES.trailing_zeros();

/// The largest buddy order a magazine caches.
///
/// Chosen from the kernel-heap size-class distribution of the workloads
/// this front exists for, captured over `hostcall-loop`, `sched-tasks`,
/// `spawn-wait`, `instance-startup-100` and `pipe-pingpong` with
/// `helios-inspector vm workload-bench --perf-metrics-output`. Of the
/// 7,591,655 kernel allocations that run served:
///
/// | request | allocations | share |
/// | --- | --- | --- |
/// | up to 8 B | 38,695 | 0.51% |
/// | up to 16 B | 75,227 | 0.99% |
/// | up to 32 B | 388,087 | 5.11% |
/// | up to 64 B | 972,387 | 12.81% |
/// | up to 128 B | 5,475,036 | 72.12% |
/// | up to 256 B | 446,852 | 5.89% |
/// | up to 512 B | 183,979 | 2.42% |
/// | up to 1 KiB | 5,569 | 0.07% |
/// | above 1 KiB | 5,823 | 0.08% |
///
/// The 512-byte class is where that curve ends: everything at or below
/// it is 99.85% of the allocations, and the classes above it are rare
/// enough that a cached block would sit idle holding memory out of the
/// heap for the life of the kernel. They still pay the buddy walk, but
/// they do not repeat.
const MAX_CACHED_ORDER: u32 = 9;

/// How many size classes one processor keeps a magazine for.
const MAGAZINE_CLASS_COUNT: usize = (MAX_CACHED_ORDER - MIN_CACHED_ORDER + 1) as usize;

/// The largest allocation a magazine can serve.
const MAX_CACHED_BYTES: usize = 1 << MAX_CACHED_ORDER;

/// How many blocks a refill takes from the shared heap, and how many a
/// full magazine gives back.
///
/// This is the factor the heap lock is amortised by, and the bound on
/// how long one masked region runs: a drain unlinks sixteen blocks and
/// the caller returns them under one acquisition.
pub(crate) const MAGAZINE_BATCH: usize = 16;

/// How many blocks one class may hold on one processor.
///
/// Two batches, the same for every class: enough that a drain leaves a
/// batch behind to serve from, and no more, because what a parked
/// block costs is not its bytes.
///
/// `buddy_system_allocator::Heap::dealloc` finds a block's buddy by
/// walking that class's free list from the head, so a block a magazine
/// holds is a block whose buddy arrives at the heap, finds nothing to
/// merge with, and stays on the list — and every later free of the
/// class walks past it. A magazine's depth is therefore the length of
/// a scan the shared heap pays on every free of that class, and it is
/// what the earlier byte budget failed to bound: it allowed 128 blocks
/// of the hot classes on each processor, and the mass free that ends
/// `instance-startup-100` — a hundred instances alive at once, then
/// all destroyed — walked hundreds of unmergeable blocks per free
/// (#169). The whole regression was there: the spawn half of that
/// workload, which the magazines serve out of cache, never moved.
const CLASS_CAPACITY: usize = 2 * MAGAZINE_BATCH;

/// The link a cached block carries while it sits on a magazine, written
/// into the block's own bytes.
#[repr(C)]
struct FreeBlock {
    next: *mut FreeBlock,
}

const _: () = assert!(
    size_of::<FreeBlock>() <= MIN_BLOCK_BYTES,
    "a cached block must be wide enough to hold the link that threads it"
);

/// One size class of the kernel heap, as the buddy allocator sees it.
///
/// A class is a buddy order: its blocks are `1 << order` bytes at
/// `1 << order` alignment, which is the block
/// `buddy_system_allocator::Heap` serves any layout that rounds to that
/// order. Talking to the heap in [`MagazineClass::layout`] rather than
/// in the caller's own layout is what makes a cached block fungible,
/// and it costs nothing: the heap rounds the caller's layout to the
/// same order anyway, so it is the same block, and the heap's own byte
/// accounting stays symmetric between the allocation and the free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MagazineClass(u32);

impl MagazineClass {
    /// The class serving `layout`, or `None` when the block it needs is
    /// larger than the largest class cached.
    #[inline]
    pub(crate) fn of(layout: Layout) -> Option<Self> {
        if layout.size() > MAX_CACHED_BYTES || layout.align() > MAX_CACHED_BYTES {
            return None;
        }
        // Mirrors `Heap::alloc`: the served block is the next power of
        // two at or above the request, never below the alignment and
        // never below one link.
        let bytes = layout
            .size()
            .next_power_of_two()
            .max(layout.align())
            .max(MIN_BLOCK_BYTES);
        Some(Self(bytes.trailing_zeros()))
    }

    /// The layout every block of this class is allocated and freed
    /// under.
    #[inline]
    pub(crate) fn layout(self) -> Layout {
        let bytes = self.block_bytes();
        // SAFETY: `bytes` is a power of two between `MIN_BLOCK_BYTES`
        // and `MAX_CACHED_BYTES`, so it is a valid alignment and
        // rounding the size up to it cannot overflow.
        unsafe { Layout::from_size_align_unchecked(bytes, bytes) }
    }

    #[inline]
    const fn block_bytes(self) -> usize {
        1 << self.0
    }

    #[inline]
    const fn index(self) -> usize {
        (self.0 - MIN_CACHED_ORDER) as usize
    }

    const fn from_index(index: usize) -> Self {
        Self(MIN_CACHED_ORDER + index as u32)
    }
}

/// How a counter block is stepped.
///
/// A per-processor block has one writer, which needs no atomicity
/// against itself and takes the local interrupt mask against its own
/// interrupt handler; it steps with a relaxed load, an add and a
/// relaxed store, which no target lowers to a locked instruction. The
/// one block no processor owns — the counters an allocation made before
/// its processor could name a slot lands on — may have several writers
/// at once and steps with a real atomic add. The two are the same
/// counters and the same arithmetic, so the difference is a type rather
/// than a second copy of [`HeapCounters`].
pub(crate) trait CounterStep {
    fn add_u64(cell: &AtomicU64, by: u64);
    fn add_usize_signed(cell: &AtomicUsize, by: isize);
}

/// The single-writer step; see [`CounterStep`].
pub(crate) struct OwnedStep;

impl CounterStep for OwnedStep {
    #[inline]
    fn add_u64(cell: &AtomicU64, by: u64) {
        cell.store(
            cell.load(Ordering::Relaxed).wrapping_add(by),
            Ordering::Relaxed,
        );
    }

    #[inline]
    fn add_usize_signed(cell: &AtomicUsize, by: isize) {
        cell.store(
            cell.load(Ordering::Relaxed).wrapping_add_signed(by),
            Ordering::Relaxed,
        );
    }
}

/// The many-writer step; see [`CounterStep`].
pub(crate) struct SharedStep;

impl CounterStep for SharedStep {
    #[inline]
    fn add_u64(cell: &AtomicU64, by: u64) {
        cell.fetch_add(by, Ordering::Relaxed);
    }

    #[inline]
    fn add_usize_signed(cell: &AtomicUsize, by: isize) {
        cell.fetch_add(by as usize, Ordering::Relaxed);
    }
}

/// One block of kernel-heap allocation counters.
///
/// `requested_live_bytes` is the one field whose per-processor value is
/// not a total: an allocation served on one processor may be freed on
/// another, so a processor's word is the difference between what it
/// allocated and what it freed and may wrap below zero. It is added up
/// with wrapping arithmetic, and the sum over every block is the live
/// total.
pub(crate) struct HeapCounters {
    requested_live_bytes: AtomicUsize,
    allocation_count: AtomicU64,
    deallocation_count: AtomicU64,
    reallocation_count: AtomicU64,
    total_allocation_bytes: AtomicU64,
    total_deallocation_bytes: AtomicU64,
    total_reallocation_bytes: AtomicU64,
    size_class_allocation_count: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_deallocation_count: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_reallocation_count: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_allocation_bytes: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_deallocation_bytes: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_reallocation_bytes: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
}

impl HeapCounters {
    pub(crate) const fn new() -> Self {
        Self {
            requested_live_bytes: AtomicUsize::new(0),
            allocation_count: AtomicU64::new(0),
            deallocation_count: AtomicU64::new(0),
            reallocation_count: AtomicU64::new(0),
            total_allocation_bytes: AtomicU64::new(0),
            total_deallocation_bytes: AtomicU64::new(0),
            total_reallocation_bytes: AtomicU64::new(0),
            size_class_allocation_count: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_deallocation_count: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_reallocation_count: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_allocation_bytes: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_deallocation_bytes: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_reallocation_bytes: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
        }
    }

    /// Records one allocation of `size` bytes.
    pub(crate) fn record_alloc<Step: CounterStep>(&self, size: usize, size_class_metrics: bool) {
        let size_u64 = usize_to_u64(size, "kernel allocation size");
        Step::add_u64(&self.allocation_count, 1);
        Step::add_usize_signed(&self.requested_live_bytes, size_to_step(size));
        Step::add_u64(&self.total_allocation_bytes, size_u64);
        if size_class_metrics {
            let class = heap_size_class(size);
            Step::add_u64(&self.size_class_allocation_count[class], 1);
            Step::add_u64(&self.size_class_allocation_bytes[class], size_u64);
        }
    }

    /// Records one deallocation of `size` bytes.
    pub(crate) fn record_dealloc<Step: CounterStep>(&self, size: usize, size_class_metrics: bool) {
        let size_u64 = usize_to_u64(size, "kernel deallocation size");
        Step::add_u64(&self.deallocation_count, 1);
        Step::add_usize_signed(&self.requested_live_bytes, -size_to_step(size));
        Step::add_u64(&self.total_deallocation_bytes, size_u64);
        if size_class_metrics {
            let class = heap_size_class(size);
            Step::add_u64(&self.size_class_deallocation_count[class], 1);
            Step::add_u64(&self.size_class_deallocation_bytes[class], size_u64);
        }
    }

    /// Records one reallocation from `old_size` to `new_size`.
    pub(crate) fn record_realloc<Step: CounterStep>(
        &self,
        old_size: usize,
        new_size: usize,
        size_class_metrics: bool,
    ) {
        let new_size_u64 = usize_to_u64(new_size, "kernel reallocation size");
        Step::add_u64(&self.reallocation_count, 1);
        Step::add_usize_signed(
            &self.requested_live_bytes,
            size_to_step(new_size) - size_to_step(old_size),
        );
        Step::add_u64(&self.total_reallocation_bytes, new_size_u64);
        if size_class_metrics {
            let class = heap_size_class(new_size);
            Step::add_u64(&self.size_class_reallocation_count[class], 1);
            Step::add_u64(&self.size_class_reallocation_bytes[class], new_size_u64);
        }
    }

    /// Adds this block's counters into `total`.
    pub(crate) fn accumulate_into(&self, total: &mut HeapStats) {
        total.requested_live_bytes = total
            .requested_live_bytes
            .wrapping_add(self.requested_live_bytes.load(Ordering::Relaxed));
        total.allocation_count += self.allocation_count.load(Ordering::Relaxed);
        total.deallocation_count += self.deallocation_count.load(Ordering::Relaxed);
        total.reallocation_count += self.reallocation_count.load(Ordering::Relaxed);
        total.total_allocation_bytes += self.total_allocation_bytes.load(Ordering::Relaxed);
        total.total_deallocation_bytes += self.total_deallocation_bytes.load(Ordering::Relaxed);
        total.total_reallocation_bytes += self.total_reallocation_bytes.load(Ordering::Relaxed);
        accumulate_class_counts(
            &mut total.size_class_allocation_count,
            &self.size_class_allocation_count,
        );
        accumulate_class_counts(
            &mut total.size_class_deallocation_count,
            &self.size_class_deallocation_count,
        );
        accumulate_class_counts(
            &mut total.size_class_reallocation_count,
            &self.size_class_reallocation_count,
        );
        accumulate_class_counts(
            &mut total.size_class_allocation_bytes,
            &self.size_class_allocation_bytes,
        );
        accumulate_class_counts(
            &mut total.size_class_deallocation_bytes,
            &self.size_class_deallocation_bytes,
        );
        accumulate_class_counts(
            &mut total.size_class_reallocation_bytes,
            &self.size_class_reallocation_bytes,
        );
    }
}

fn accumulate_class_counts(
    total: &mut [u64; HEAP_SIZE_CLASS_COUNT],
    values: &[AtomicU64; HEAP_SIZE_CLASS_COUNT],
) {
    for (index, slot) in total.iter_mut().enumerate() {
        *slot += values[index].load(Ordering::Relaxed);
    }
}

/// A request size as a step on the live-bytes counter.
///
/// A kernel allocation past `isize::MAX` is not a thing this machine
/// can serve, and `Layout` refuses to describe one, so a size that does
/// not fit is a corrupted layout rather than a large request.
fn size_to_step(size: usize) -> isize {
    isize::try_from(size)
        .unwrap_or_else(|_| panic!("kernel allocation size {size} does not fit an isize"))
}

/// What one processor's magazines did.
///
/// Owner-stepped, and stepped only from inside a masked region that is
/// already there for the magazine operation being counted, so no
/// allocation pays a mask or an atomic for these. They are what says
/// whether the front is serving a workload or fighting it: a hit rate,
/// how much of a batch a refill actually came back with, and whether
/// drains track refills.
struct MagazineCounters {
    hit_count: AtomicU64,
    miss_count: AtomicU64,
    refill_count: AtomicU64,
    refill_block_count: AtomicU64,
    drain_count: AtomicU64,
    drain_block_count: AtomicU64,
}

impl MagazineCounters {
    const fn new() -> Self {
        Self {
            hit_count: AtomicU64::new(0),
            miss_count: AtomicU64::new(0),
            refill_count: AtomicU64::new(0),
            refill_block_count: AtomicU64::new(0),
            drain_count: AtomicU64::new(0),
            drain_block_count: AtomicU64::new(0),
        }
    }

    fn accumulate_into(&self, total: &mut HeapStats) {
        total.magazine_hit_count += self.hit_count.load(Ordering::Relaxed);
        total.magazine_miss_count += self.miss_count.load(Ordering::Relaxed);
        total.magazine_refill_count += self.refill_count.load(Ordering::Relaxed);
        total.magazine_refill_block_count += self.refill_block_count.load(Ordering::Relaxed);
        total.magazine_drain_count += self.drain_count.load(Ordering::Relaxed);
        total.magazine_drain_block_count += self.drain_block_count.load(Ordering::Relaxed);
    }
}

/// One processor's magazines and counters.
///
/// See the module documentation for which half is reached how. Every
/// method that touches the owner-only half masks local interrupts for
/// the whole of it, so a caller needs nothing but to be the owner.
pub(crate) struct ProcessorFront {
    /// Owner only, under the local interrupt mask: the head of each
    /// class's block list, or null.
    heads: UnsafeCell<[*mut FreeBlock; MAGAZINE_CLASS_COUNT]>,
    /// Owner writes, any processor reads: how many blocks each head
    /// holds. Kept beside the heads rather than derived from them
    /// because walking a list to answer `heap_stats()` would touch
    /// every cached block on every processor.
    depths: [AtomicUsize; MAGAZINE_CLASS_COUNT],
    /// Owner writes, any processor reads.
    counters: HeapCounters,
    /// Owner writes, any processor reads: what this processor's
    /// magazines did.
    magazine_counters: MagazineCounters,
}

// SAFETY: `heads` is only ever reached from the owning processor with
// local interrupts masked — every method that touches it takes the mask
// itself — and everything another processor reads is atomic.
unsafe impl Sync for ProcessorFront {}
unsafe impl Send for ProcessorFront {}

impl ProcessorFront {
    fn new() -> Self {
        Self {
            heads: UnsafeCell::new([core::ptr::null_mut(); MAGAZINE_CLASS_COUNT]),
            depths: [const { AtomicUsize::new(0) }; MAGAZINE_CLASS_COUNT],
            counters: HeapCounters::new(),
            magazine_counters: MagazineCounters::new(),
        }
    }

    /// This processor's counters, for the paths the shared heap served
    /// and that count outside a magazine operation.
    #[inline]
    pub(crate) fn counters(&self) -> &HeapCounters {
        &self.counters
    }

    /// Takes one block of `class` out of this processor's magazine.
    ///
    /// The caller runs on this processor; the mask is taken here.
    #[inline]
    pub(crate) fn take(&self, class: MagazineClass) -> Option<NonNull<u8>> {
        with_local_interrupts_masked(|| {
            // SAFETY: this is the owning processor and interrupts are
            // masked for the whole of the access, so no other reference
            // into the cell exists.
            let heads = unsafe { &mut *self.heads.get() };
            let counters = &self.magazine_counters;
            let Some(head) = NonNull::new(heads[class.index()]) else {
                OwnedStep::add_u64(&counters.miss_count, 1);
                return None;
            };
            // SAFETY: every block on this list was given its link by
            // `give` or `stock`, and nothing has touched it since: the
            // list is owner-only.
            heads[class.index()] = unsafe { head.as_ref().next };
            self.set_depth(class, self.depth(class) - 1);
            OwnedStep::add_u64(&counters.hit_count, 1);
            Some(head.cast())
        })
    }

    /// Returns one block of `class` to this processor's magazine, and
    /// hands back a batch for the shared heap when the magazine is
    /// already at capacity.
    ///
    /// The batch travels out of the masked region on purpose: giving it
    /// back takes the heap lock, and that belongs outside the window
    /// this processor holds its interrupts masked for.
    ///
    /// # Safety
    ///
    /// `block` is a block of `class` that the buddy heap served under
    /// [`MagazineClass::layout`] and that nothing else references.
    #[inline]
    pub(crate) unsafe fn give(
        &self,
        class: MagazineClass,
        block: NonNull<u8>,
    ) -> Option<ArrayVec<NonNull<u8>, MAGAZINE_BATCH>> {
        with_local_interrupts_masked(|| {
            // SAFETY: owning processor, interrupts masked.
            let heads = unsafe { &mut *self.heads.get() };
            let mut depth = self.depth(class);
            let mut overflow = None;
            if depth >= CLASS_CAPACITY {
                let mut batch = ArrayVec::new();
                while !batch.is_full()
                    && let Some(head) = NonNull::new(heads[class.index()])
                {
                    // SAFETY: the block is on this owner-only list, so
                    // its link is the one this method wrote.
                    heads[class.index()] = unsafe { head.as_ref().next };
                    depth -= 1;
                    batch.push(head.cast());
                }
                let counters = &self.magazine_counters;
                OwnedStep::add_u64(&counters.drain_count, 1);
                OwnedStep::add_u64(
                    &counters.drain_block_count,
                    usize_to_u64(batch.len(), "magazine drain block count"),
                );
                overflow = Some(batch);
            }
            // SAFETY: the caller promises the block is unaliased and at
            // least `MIN_BLOCK_BYTES` wide, so the link goes into the
            // block's own freed bytes.
            unsafe { push_block(heads, class, block) };
            self.set_depth(class, depth + 1);
            overflow
        })
    }

    /// Adds the blocks a refill took to this processor's magazine, and
    /// leaves the caller whatever did not fit.
    ///
    /// # Safety
    ///
    /// As [`ProcessorFront::give`], for every block in `blocks`.
    pub(crate) unsafe fn stock(
        &self,
        class: MagazineClass,
        blocks: &mut ArrayVec<NonNull<u8>, MAGAZINE_BATCH>,
    ) {
        with_local_interrupts_masked(|| {
            // SAFETY: owning processor, interrupts masked.
            let heads = unsafe { &mut *self.heads.get() };
            let counters = &self.magazine_counters;
            OwnedStep::add_u64(&counters.refill_count, 1);
            OwnedStep::add_u64(
                &counters.refill_block_count,
                usize_to_u64(blocks.len(), "magazine refill block count"),
            );
            let mut depth = self.depth(class);
            while depth < CLASS_CAPACITY
                && let Some(block) = blocks.pop()
            {
                // SAFETY: as `give`, for every block the caller handed
                // over.
                unsafe { push_block(heads, class, block) };
                depth += 1;
            }
            self.set_depth(class, depth);
        });
    }

    #[inline]
    fn depth(&self, class: MagazineClass) -> usize {
        self.depths[class.index()].load(Ordering::Relaxed)
    }

    #[inline]
    fn set_depth(&self, class: MagazineClass, depth: usize) {
        self.depths[class.index()].store(depth, Ordering::Relaxed);
    }

    fn cached_bytes(&self) -> usize {
        self.depths
            .iter()
            .enumerate()
            .map(|(index, depth)| {
                depth.load(Ordering::Relaxed) * MagazineClass::from_index(index).block_bytes()
            })
            .sum()
    }
}

/// Links `block` onto `class`'s list.
///
/// # Safety
///
/// The caller is the owning processor with interrupts masked, `heads`
/// is its own head array, and `block` is an unaliased block of `class`.
#[inline]
unsafe fn push_block(
    heads: &mut [*mut FreeBlock; MAGAZINE_CLASS_COUNT],
    class: MagazineClass,
    block: NonNull<u8>,
) {
    let mut block = block.cast::<FreeBlock>();
    // SAFETY: the caller promises the block is unaliased and at least
    // `MIN_BLOCK_BYTES` wide.
    unsafe {
        block.as_mut().next = heads[class.index()];
    }
    heads[class.index()] = block.as_ptr();
}

/// The kernel heap's per-processor front.
///
/// One [`ProcessorFront`] per configured processor, sized at bring-up.
/// See the module documentation for the contract.
pub(crate) struct HeapMagazines {
    processors: Once<Box<[CachePadded<ProcessorFront>]>>,
}

impl HeapMagazines {
    pub(crate) const fn new() -> Self {
        Self {
            processors: Once::new(),
        }
    }

    /// Builds one front per processor, out of the heap the caller has
    /// just filled.
    ///
    /// Called once, from the bring-up path that already knows how many
    /// processors the machine has. A second call is a bring-up bug and
    /// panics rather than silently keeping the first sizing.
    pub(crate) fn configure_processors(&self, processor_count: usize) {
        assert!(
            processor_count != 0,
            "the kernel heap front requires at least one processor"
        );
        let mut built = false;
        self.processors.call_once(|| {
            built = true;
            let mut fronts = Vec::with_capacity(processor_count);
            fronts.resize_with(processor_count, || CachePadded::new(ProcessorFront::new()));
            fronts.into_boxed_slice()
        });
        assert!(
            built,
            "the kernel heap front was configured more than once; a second sizing of \
             {processor_count} processors arrived after the array already existed"
        );
    }

    /// The front `slot` owns, or `None` while the array does not exist
    /// yet.
    ///
    /// # Panics
    ///
    /// Panics when the array exists and does not hold `slot`: a
    /// processor running with a slot the bring-up path never sized for
    /// is an ordering bug, and serving it out of some other processor's
    /// magazine would be a data race on that processor's owner-only
    /// metadata.
    #[inline]
    pub(crate) fn front(&self, slot: ProcessorId) -> Option<&ProcessorFront> {
        let fronts = self.processors.get()?;
        let front = fronts.get(usize::from(slot.id())).unwrap_or_else(|| {
            panic!(
                "processor {} has no kernel heap front; the front was sized for {} processors \
                 before this processor came online",
                slot.id(),
                fronts.len()
            )
        });
        Some(front)
    }

    /// Bytes every processor is holding out of the shared heap.
    pub(crate) fn cached_bytes(&self) -> usize {
        self.processors.get().map_or(0, |fronts| {
            fronts.iter().map(|front| front.cached_bytes()).sum()
        })
    }

    /// Adds every processor's counters into `total`.
    pub(crate) fn accumulate_counters(&self, total: &mut HeapStats) {
        if let Some(fronts) = self.processors.get() {
            for front in fronts.iter() {
                front.counters.accumulate_into(total);
                front.magazine_counters.accumulate_into(total);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::alloc::{alloc, dealloc};
    use alloc::vec::Vec;
    use core::alloc::Layout;
    use core::ptr::NonNull;

    use arrayvec::ArrayVec;
    use helios_hal::cpu::ProcessorId;

    use super::{
        CLASS_CAPACITY, HeapMagazines, MAGAZINE_BATCH, MAGAZINE_CLASS_COUNT, MAX_CACHED_BYTES,
        MIN_BLOCK_BYTES, MagazineClass, OwnedStep, ProcessorFront, SharedStep,
    };
    use crate::HeapStats;

    const OWNER: ProcessorId = ProcessorId::new(0);

    /// Allocates one block of `class` the way the allocator does, so
    /// the test can hand it back to the host allocator afterwards.
    fn block(class: MagazineClass) -> NonNull<u8> {
        // SAFETY: a class layout is a power of two of non-zero size.
        let ptr = unsafe { alloc(class.layout()) };
        NonNull::new(ptr).expect("the test allocator ran out of memory")
    }

    fn release(class: MagazineClass, ptr: NonNull<u8>) {
        // SAFETY: `ptr` came from `block` under the same layout.
        unsafe { dealloc(ptr.as_ptr(), class.layout()) };
    }

    /// A layout is served by the class the buddy heap would round it
    /// to, and by no smaller one.
    #[test]
    fn a_class_covers_the_layout_it_serves() {
        for (size, align, bytes) in [
            (1usize, 1usize, 8usize),
            (8, 8, 8),
            (9, 1, 16),
            (48, 8, 64),
            (64, 64, 64),
            (65, 8, 128),
            (512, 8, 512),
        ] {
            let layout = Layout::from_size_align(size, align).expect("a valid layout");
            let class = MagazineClass::of(layout).expect("a cached class");
            assert_eq!(class.layout().size(), bytes, "size {size} align {align}");
            assert!(class.layout().size() >= size);
            assert!(class.layout().align() >= align);
        }
    }

    /// Anything past the largest cached class goes straight to the
    /// heap.
    #[test]
    fn a_large_layout_has_no_class() {
        let layout = Layout::from_size_align(MAX_CACHED_BYTES + 1, 8).expect("a valid layout");
        assert_eq!(MagazineClass::of(layout), None);
        let aligned = Layout::from_size_align(8, MAX_CACHED_BYTES * 2).expect("a valid layout");
        assert_eq!(MagazineClass::of(aligned), None);
    }

    /// A class's own layout maps back to the same class, which is what
    /// keeps the heap's byte accounting symmetric across a magazine.
    #[test]
    fn a_class_layout_round_trips() {
        for index in 0..MAGAZINE_CLASS_COUNT {
            let class = MagazineClass::from_index(index);
            assert_eq!(MagazineClass::of(class.layout()), Some(class));
            assert!(class.layout().size() >= MIN_BLOCK_BYTES);
        }
    }

    /// A block given to a magazine comes back out, and the depth
    /// follows it.
    #[test]
    fn a_cached_block_comes_back() {
        let magazines = HeapMagazines::new();
        magazines.configure_processors(1);
        let front = magazines.front(OWNER).expect("a configured front");
        let class = MagazineClass::of(Layout::from_size_align(64, 8).expect("a valid layout"))
            .expect("a class");
        let ptr = block(class);

        // SAFETY: the block was allocated under the class layout and
        // nothing else holds it.
        let overflow = unsafe { front.give(class, ptr) };
        assert!(overflow.is_none());
        assert_eq!(magazines.cached_bytes(), class.layout().size());

        let taken = front.take(class).expect("the magazine held a block");
        assert_eq!(taken, ptr);
        assert_eq!(magazines.cached_bytes(), 0);
        release(class, ptr);
    }

    /// An empty magazine answers `None` rather than inventing a block.
    #[test]
    fn an_empty_magazine_has_nothing_to_give() {
        let magazines = HeapMagazines::new();
        magazines.configure_processors(1);
        let front = magazines.front(OWNER).expect("a configured front");
        assert_eq!(front.take(MagazineClass::from_index(0)), None);
    }

    /// Nothing is cached before bring-up sizes the array, and the
    /// allocator sees that as "no front" rather than as slot zero.
    #[test]
    fn an_unconfigured_front_does_not_exist() {
        let magazines = HeapMagazines::new();
        assert!(magazines.front(OWNER).is_none());
        assert_eq!(magazines.cached_bytes(), 0);
    }

    /// Filling a magazine past its capacity hands a batch back for the
    /// shared heap, and the magazine keeps the rest.
    #[test]
    fn a_full_magazine_gives_a_batch_back() {
        let magazines = HeapMagazines::new();
        magazines.configure_processors(1);
        let front = magazines.front(OWNER).expect("a configured front");
        let class = MagazineClass::from_index(MAGAZINE_CLASS_COUNT - 1);
        let capacity = CLASS_CAPACITY;
        let mut blocks = Vec::new();
        let mut returned = Vec::new();

        for _ in 0..=capacity {
            let ptr = block(class);
            blocks.push(ptr);
            // SAFETY: the block was allocated under the class layout.
            if let Some(batch) = unsafe { front.give(class, ptr) } {
                assert_eq!(batch.len(), MAGAZINE_BATCH);
                returned.extend(batch);
            }
        }

        assert_eq!(returned.len(), MAGAZINE_BATCH);
        assert_eq!(
            magazines.cached_bytes(),
            (capacity + 1 - MAGAZINE_BATCH) * class.layout().size()
        );

        while front.take(class).is_some() {}
        for ptr in blocks {
            release(class, ptr);
        }
    }

    /// A refill stops at the magazine's capacity and leaves the rest
    /// for the caller to give back.
    #[test]
    fn stocking_stops_at_capacity() {
        let magazines = HeapMagazines::new();
        magazines.configure_processors(1);
        let front = magazines.front(OWNER).expect("a configured front");
        let class = MagazineClass::from_index(MAGAZINE_CLASS_COUNT - 1);
        let mut blocks: ArrayVec<NonNull<u8>, MAGAZINE_BATCH> = ArrayVec::new();
        let mut all = Vec::new();
        for _ in 0..MAGAZINE_BATCH {
            let ptr = block(class);
            all.push(ptr);
            blocks.push(ptr);
        }

        // SAFETY: every block was allocated under the class layout.
        unsafe { front.stock(class, &mut blocks) };
        assert!(blocks.is_empty(), "capacity is at least one batch");
        assert_eq!(
            magazines.cached_bytes(),
            MAGAZINE_BATCH * class.layout().size()
        );

        while front.take(class).is_some() {}
        for ptr in all {
            release(class, ptr);
        }
    }

    /// A processor the array was never sized for is a bring-up ordering
    /// bug, not a slot to invent.
    #[test]
    #[should_panic(expected = "has no kernel heap front")]
    fn an_unsized_processor_panics() {
        let magazines = HeapMagazines::new();
        magazines.configure_processors(1);
        let _ = magazines.front(ProcessorId::new(3));
    }

    /// Counters are per processor and the stats read sums them.
    #[test]
    fn counters_sum_across_processors() {
        let first = ProcessorFront::new();
        let second = ProcessorFront::new();
        first.counters().record_alloc::<OwnedStep>(64, true);
        second.counters().record_alloc::<OwnedStep>(64, true);
        second.counters().record_dealloc::<OwnedStep>(64, true);

        let mut total = HeapStats::zeroed();
        first.counters().accumulate_into(&mut total);
        second.counters().accumulate_into(&mut total);

        assert_eq!(total.allocation_count, 2);
        assert_eq!(total.deallocation_count, 1);
        assert_eq!(total.requested_live_bytes, 64);
        assert_eq!(total.size_class_allocation_count[3], 2);
    }

    /// A processor that frees more than it allocated wraps below zero,
    /// and the sum with the processor that allocated is still the live
    /// total.
    #[test]
    fn a_free_on_another_processor_still_totals() {
        let allocator = ProcessorFront::new();
        let freer = ProcessorFront::new();
        allocator.counters().record_alloc::<OwnedStep>(128, false);
        freer.counters().record_dealloc::<OwnedStep>(128, false);

        let mut total = HeapStats::zeroed();
        allocator.counters().accumulate_into(&mut total);
        freer.counters().accumulate_into(&mut total);
        assert_eq!(total.requested_live_bytes, 0);
    }

    /// The two counter steps are the same arithmetic; only their
    /// atomicity differs.
    #[test]
    fn both_counter_steps_agree() {
        let owned = ProcessorFront::new();
        let shared = ProcessorFront::new();
        owned.counters().record_alloc::<OwnedStep>(200, true);
        owned.counters().record_realloc::<OwnedStep>(200, 300, true);
        shared.counters().record_alloc::<SharedStep>(200, true);
        shared
            .counters()
            .record_realloc::<SharedStep>(200, 300, true);

        let mut owned_total = HeapStats::zeroed();
        owned.counters().accumulate_into(&mut owned_total);
        let mut shared_total = HeapStats::zeroed();
        shared.counters().accumulate_into(&mut shared_total);
        assert_eq!(owned_total, shared_total);
        assert_eq!(owned_total.requested_live_bytes, 300);
    }
}
