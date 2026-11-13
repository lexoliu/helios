//! The user-memory pool: the guest's bulk memory, one buddy pool per NUMA
//! node.
//!
//! An allocation on a processor of node N walks N's memory order — N
//! first when it has memory, then the other nodes with memory nearest
//! first ([`Locality::memory_order`]) — and is served by the first node
//! that can. For each node it tries the processor's cached frame of that
//! node, then the node's buddy, then drains every processor's cached
//! frames of that node back into the buddy and retries, before moving on.
//! This is Linux's first touch over its zonelists: local first, and
//! machine-wide exhaustion is the only out-of-memory condition.
//!
//! A frame belongs to the node its memory is on for life. A free looks the
//! node up from the address, a single frame goes onto the freeing
//! processor's list for that node, and anything larger goes back to that
//! node's buddy.
//!
//! # Concurrency contract
//!
//! - **Node pools** ([`NodePool`]): each node's buddy sits behind an
//!   [`IrqSafeMutex`], taken from any processor that allocates from or
//!   frees to that node, a drain, a lend to the kernel heap, the balloon
//!   and free-page reporting included. The lock masks the local
//!   processor's interrupts while held: the kernel's global allocator
//!   lends itself memory out of this pool from wherever it was called, an
//!   interrupt handler included (#206). A node's reported-frame bitmap has
//!   the same kind of lock, its `total_bytes` is an atomic a lend lowers.
//!   Each node pool sits on its own cache line. No path holds two nodes'
//!   buddy locks at once, and the only nesting is a node's buddy lock
//!   around a drain of that node's frame-slab lists.
//! - **Frame slab** ([`FrameSlabCache`]): one list per (processor, node),
//!   popped and pushed by the processor it belongs to and drained from any
//!   processor; see its module.
//! - **Off-node counters**: one per processor, each on its own cache line,
//!   incremented (Relaxed) by allocations made in that processor's name —
//!   callers name the executing processor — and summed by node from any
//!   processor.
//! - **Locality and the bootstrap processor**: set on the bootstrap
//!   processor before the pool is installed and immutable after; read
//!   without synchronization from any processor and from fault and
//!   interrupt context.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::future::Future;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use arrayvec::ArrayVec;
use crossbeam_utils::CachePadded;
use helios_hal::cpu::{ProcessorId, current_processor};
use helios_hal::numa::{NodeId, NumaTopology};
use helios_hal::pmm::{
    FrameAllocError, FrameAllocStats, PhysFrame, PhysFrameAllocator, PhysFrameRange,
};

use crate::memory::frame_pool::FramePool;
use crate::memory::frame_slab::FrameSlabCache;
use crate::memory::irq_safe::IrqSafeMutex;
use crate::memory::locality::{Locality, NodeRun};
use crate::memory::per_processor::{NodeArenas, NodeMemory, PerProcessor};
use crate::memory::reported::{ReportedFrames, visit_free_runs};
use crate::{MAX_BOOT_MEMORY_REGIONS, ProgramOutOfMemory};

pub struct UserMemoryPool {
    /// Index: [`NodeId`]. `None` for a node the platform gave no user
    /// memory; no memory order names it.
    nodes: Box<[CachePadded<Option<NodePool>>]>,
    frame_slab: FrameSlabCache,
    /// Allocations each processor had served by a node other than its
    /// own.
    off_node: PerProcessor<AtomicU64>,
    /// Where per-processor structures are carved from, one arena per
    /// node.
    arenas: NodeArenas,
    /// Where each processor and each byte of the pool sits.
    locality: Locality,
    /// The processor that primed the pool. The kernel heap's boot share
    /// sits on its node, and the heap grows from that node's memory
    /// order.
    bootstrap: ProcessorId,
}

/// One node's share of the pool.
struct NodePool {
    /// The node's buddy storage, over this node's regions only, with its
    /// metadata inside them.
    heap: IrqSafeMutex<FramePool>,
    /// Frames of this node shown to a free-page consumer. The memory is
    /// still the pool's to hand out; its contents are not.
    reported: ReportedFrames,
    /// What the node still owns: everything it was seeded with, less what
    /// it lent the kernel heap for good. Sizes the node's frame-slab
    /// lists.
    total_bytes: AtomicUsize,
}

impl NodePool {
    fn new(regions: &[(usize, usize)]) -> Self {
        let mut heap = FramePool::new();
        heap.initialize(regions);
        let reported = ReportedFrames::new();
        for &(start, end) in regions {
            reported.cover(start, end);
        }
        Self {
            total_bytes: AtomicUsize::new(heap.total_bytes),
            heap: IrqSafeMutex::new(heap),
            reported,
        }
    }

    fn stats(&self, cached_bytes: usize) -> UserHeapStats {
        self.heap.with(|heap| UserHeapStats {
            total_bytes: heap.total_bytes,
            allocated_bytes: heap.allocated_bytes.saturating_sub(cached_bytes),
        })
    }
}

/// The nodes' buddies before the pool around them exists: the memory
/// source the pool's own per-processor tables are carved with.
struct NodeBuddies<'a> {
    nodes: &'a [CachePadded<Option<NodePool>>],
    locality: &'a Locality,
}

impl NodeMemory for NodeBuddies<'_> {
    fn lend(&self, node: NodeId, bytes: usize) -> Option<usize> {
        lend_from_nodes(self.nodes, self.locality, node, bytes, |_, pool| {
            pool.heap.with(|heap| heap.allocate(bytes).ok())
        })
    }

    unsafe fn give_back(&self, start: usize, bytes: usize) {
        // SAFETY: the caller's contract is `return_to_nodes`'s.
        unsafe { return_to_nodes(self.nodes, self.locality, start, bytes) };
    }
}

/// Takes `bytes` for good from the first node of `node`'s memory order
/// that `allocate` can serve, and stops counting them towards that
/// node's pool, as a lend to the kernel heap does.
fn lend_from_nodes(
    nodes: &[CachePadded<Option<NodePool>>],
    locality: &Locality,
    node: NodeId,
    bytes: usize,
    mut allocate: impl FnMut(NodeId, &NodePool) -> Option<NonNull<u8>>,
) -> Option<usize> {
    debug_assert!(
        bytes.is_power_of_two() && bytes >= PhysFrame::SIZE,
        "a lend is one buddy block"
    );
    locality.memory_order(node).iter().find_map(|&candidate| {
        let pool = nodes[candidate.index()].as_ref()?;
        let ptr = allocate(candidate, pool)?;
        pool.total_bytes.fetch_sub(bytes, Ordering::Release);
        Some(ptr.as_ptr() as usize)
    })
}

/// Puts `bytes` at `start`, part of a block [`lend_from_nodes`] answered,
/// back in the node it lies on and counts them towards its pool again.
///
/// # Safety
///
/// The range is part of a block `lend_from_nodes` answered, aligned to
/// its own size relative to that block, not returned before and no
/// longer used by anything.
unsafe fn return_to_nodes(
    nodes: &[CachePadded<Option<NodePool>>],
    locality: &Locality,
    start: usize,
    bytes: usize,
) {
    let node = locality.node_of_address(start);
    let pool = nodes[node.index()]
        .as_ref()
        .unwrap_or_else(|| panic!("node {} owns user memory but has no pool", node.index()));
    let ptr = NonNull::new(core::ptr::with_exposed_provenance_mut(start))
        .unwrap_or_else(|| panic!("a returned lend is at address zero"));
    // SAFETY: by this function's contract the range is part of a block
    // this pool lent out and the lender no longer uses.
    pool.heap
        .with(|heap| unsafe { heap.deallocate(ptr, bytes) });
    pool.total_bytes.fetch_add(bytes, Ordering::Release);
}

/// Per-processor tables come out of the pool's own nodes, lending from
/// the nearest node with memory and draining cached frames first.
impl NodeMemory for UserMemoryPool {
    fn lend(&self, node: NodeId, bytes: usize) -> Option<usize> {
        lend_from_nodes(&self.nodes, &self.locality, node, bytes, |node, pool| {
            self.allocate_block(node, pool, bytes)
        })
    }

    unsafe fn give_back(&self, start: usize, bytes: usize) {
        // SAFETY: the caller's contract is `return_to_nodes`'s.
        unsafe { return_to_nodes(&self.nodes, &self.locality, start, bytes) };
    }
}

impl UserMemoryPool {
    /// Bytes the per-processor arenas have taken out of the pool for
    /// good: what [`PerProcessor`] tables and everything carved beside
    /// them occupy.
    pub fn per_processor_bytes(&self) -> usize {
        self.arenas.lent_bytes()
    }

    pub(crate) fn arenas(&self) -> &NodeArenas {
        &self.arenas
    }

    /// A pool over `runs`, each on its node, for the machine `numa`
    /// describes, primed by `bootstrap`.
    ///
    /// Each node's buddy is built over that node's runs alone, so its
    /// metadata lands in the node's own memory and no buddy spans another
    /// node's. Panics, naming what it checked, on a description the
    /// kernel refuses (see [`Locality`]) and on a bootstrap processor the
    /// description does not have.
    pub fn new<Numa: NumaTopology>(numa: &Numa, bootstrap: ProcessorId, runs: &[NodeRun]) -> Self {
        let locality = Locality::new(numa, runs);
        let processor_count = locality.processor_count();
        assert!(
            usize::from(bootstrap.id()) < processor_count,
            "bootstrap processor {} is outside the NUMA description's {processor_count} processors",
            bootstrap.id()
        );
        let nodes: Box<[CachePadded<Option<NodePool>>]> = (0..locality.node_count())
            .map(|index| {
                let node = NodeId::new(index);
                let regions: ArrayVec<(usize, usize), MAX_BOOT_MEMORY_REGIONS> = runs
                    .iter()
                    .filter(|run| run.node == node)
                    .map(|run| (run.start, run.end))
                    .collect();
                CachePadded::new((!regions.is_empty()).then(|| NodePool::new(&regions)))
            })
            .collect();
        // The pool's own per-processor tables are carved from the nodes'
        // buddies directly: the pool they will live in does not exist
        // yet, and there is nothing in any slab list to drain.
        let arenas = NodeArenas::new(locality.node_count());
        let buddies = NodeBuddies {
            nodes: &nodes,
            locality: &locality,
        };
        let frame_slab = FrameSlabCache::new(&locality, &arenas, &buddies);
        let off_node = PerProcessor::build(&locality, &arenas, &buddies, |_, _| AtomicU64::new(0));
        Self {
            nodes,
            frame_slab,
            off_node,
            arenas,
            locality,
            bootstrap,
        }
    }

    /// The machine's memory locality.
    pub fn locality(&self) -> &Locality {
        &self.locality
    }

    /// The processor that primed the pool.
    pub fn bootstrap_processor(&self) -> ProcessorId {
        self.bootstrap
    }

    fn node_pool(&self, node: NodeId) -> Option<&NodePool> {
        self.nodes[node.index()].as_ref()
    }

    /// The pool of `node`, which the caller knows has memory because a
    /// pool address resolved to it.
    fn owning_pool(&self, node: NodeId) -> &NodePool {
        self.node_pool(node)
            .unwrap_or_else(|| panic!("node {} owns user memory but has no pool", node.index()))
    }

    /// Hands `bytes` of the pool to the kernel heap, for good.
    ///
    /// This is the run-time half of the boot memory policy: the pool is
    /// seeded with everything the machine has and the kernel heap takes
    /// what it turns out to need out of it, rather than both guessing
    /// their share at boot. See [`super::policy`] for why the transfer
    /// is one-way.
    ///
    /// The kernel heap is one heap for the whole machine, so the block
    /// comes from the bootstrap processor's memory order — where the
    /// heap's boot share already sits — rather than from whichever
    /// processor's allocation happened to need it. That also keeps this
    /// path free of the executing processor's identity, which a backend
    /// publishes only once its processor runtime exists, after the heap
    /// is already serving allocations.
    ///
    /// `bytes` must be a power of two, and the block comes back aligned
    /// to its own size, so the pool loses one buddy rather than a
    /// straddling pair.
    ///
    /// Called from inside the global allocator, so it must not itself
    /// allocate on the kernel heap — which is why it goes straight to a
    /// node's buddy, skipping both the free-page reporting check and the
    /// frame slab.
    pub(crate) fn lend_to_kernel_heap(&self, bytes: usize) -> Option<(usize, usize)> {
        debug_assert!(bytes.is_power_of_two(), "a lend is one buddy block");
        let size = buddy_allocation_size(Layout::from_size_align(bytes, bytes).ok()?);
        let home = self.locality.node_of(self.bootstrap);
        self.locality.memory_order(home).iter().find_map(|&node| {
            let pool = self.node_pool(node)?;
            let ptr = self.allocate_block(node, pool, size)?;
            // The node never sees this memory again, so it must stop
            // counting towards the frame-slab capacity the node sizes
            // from its own total.
            pool.total_bytes.fetch_sub(size, Ordering::Release);
            let start = ptr.as_ptr() as usize;
            Some((start, start + size))
        })
    }

    /// The whole machine's user memory: every node's added up.
    pub fn stats(&self) -> UserHeapStats {
        (0..self.locality.node_count())
            .map(|node| self.stats_on(NodeId::new(node)))
            .fold(
                UserHeapStats {
                    total_bytes: 0,
                    allocated_bytes: 0,
                },
                |sum, node| UserHeapStats {
                    total_bytes: sum.total_bytes + node.total_bytes,
                    allocated_bytes: sum.allocated_bytes + node.allocated_bytes,
                },
            )
    }

    /// `node`'s user memory. A node without memory has none of either.
    pub fn stats_on(&self, node: NodeId) -> UserHeapStats {
        self.node_pool(node).map_or(
            UserHeapStats {
                total_bytes: 0,
                allocated_bytes: 0,
            },
            |pool| pool.stats(self.frame_slab.cached_bytes(node)),
        )
    }

    /// Allocations the processors of `node` had served by another node.
    pub fn off_node_allocations(&self, node: NodeId) -> u64 {
        self.locality
            .processors_of(node)
            .iter()
            .map(|&processor| self.off_node.get(processor).load(Ordering::Relaxed))
            .sum()
    }

    /// One reading per node, in node order, for the statistics sample.
    ///
    /// Every field is a read under the lock [`Self::stats_on`] already
    /// takes, or a relaxed counter load.
    pub fn node_stats(&self) -> Vec<MemoryNodeStats> {
        (0..self.locality.node_count())
            .map(|index| {
                let node = NodeId::new(index);
                let heap = self.stats_on(node);
                MemoryNodeStats {
                    node,
                    total_bytes: heap.total_bytes,
                    available_bytes: heap.available_bytes(),
                    off_node_allocations: self.off_node_allocations(node),
                }
            })
            .collect()
    }

    pub fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<u8>, ProgramOutOfMemory> {
        self.allocate_zeroed_on(current_processor(), layout)
    }

    pub fn allocate_zeroed_on(
        &self,
        processor: ProcessorId,
        layout: Layout,
    ) -> Result<NonNull<u8>, ProgramOutOfMemory> {
        let (ptr, size) = self.allocate_uninit(processor, layout)?;
        unsafe {
            core::ptr::write_bytes(ptr.as_ptr(), 0, size);
        }
        Ok(ptr)
    }

    /// Allocates `layout` bytes of user memory without zero-filling.
    ///
    /// Returns the allocation pointer and the actual byte count
    /// produced by the buddy allocator (rounded up to its block size).
    /// The caller must zero or fully initialise the returned region
    /// before exposing it to user code; the architecture-specific zero
    /// path (for example AArch64 `dc zva` via `Cpu::zero_memory`)
    /// applies here.
    pub fn allocate_uninit_on(
        &self,
        processor: ProcessorId,
        layout: Layout,
    ) -> Result<(NonNull<u8>, usize), ProgramOutOfMemory> {
        self.allocate_uninit(processor, layout)
    }

    /// One frame from `node` and from no other, without zero-filling,
    /// taken in `processor`'s name.
    ///
    /// This is a relocation's replacement frame: moving a page to another
    /// node would undo where it was placed, so a node with no free frame
    /// refuses rather than migrating the page.
    pub fn allocate_frame_uninit_on_node(
        &self,
        processor: ProcessorId,
        node: NodeId,
    ) -> Result<NonNull<u8>, ProgramOutOfMemory> {
        let layout = frame_layout(1);
        let ptr = self
            .allocate_on_node(processor, node, layout)
            .ok_or_else(|| self.node_out_of_memory(node, PhysFrame::SIZE))?;
        self.zero_if_reported(node, ptr, PhysFrame::SIZE);
        Ok(ptr)
    }

    fn allocate_uninit(
        &self,
        processor: ProcessorId,
        layout: Layout,
    ) -> Result<(NonNull<u8>, usize), ProgramOutOfMemory> {
        let (ptr, size, node) = self.allocate_raw(layout, processor)?;
        self.zero_if_reported(node, ptr, size);
        Ok((ptr, size))
    }

    /// Zeroes `ptr..ptr + size` of `node` if any of it was shown to a
    /// free-page consumer.
    ///
    /// Memory the pool showed to a free-page consumer may have been
    /// discarded by it. The uninit contract lets a caller skip zeroing
    /// what it is about to overwrite; it does not let the pool hand back
    /// a hole.
    fn zero_if_reported(&self, node: NodeId, ptr: NonNull<u8>, size: usize) -> bool {
        let reported = self
            .owning_pool(node)
            .reported
            .take_bytes(ptr.as_ptr() as usize, size);
        if reported {
            unsafe {
                core::ptr::write_bytes(ptr.as_ptr(), 0, size);
            }
        }
        reported
    }

    /// The largest block the pool will actually hand out right now, at most
    /// `limit` bytes.
    ///
    /// [`UserHeapStats::largest_allocatable_bytes`] is a granularity bound
    /// derived from the free byte count alone: it answers what the allocator
    /// could serve if its free space were one intact buddy. It usually is not.
    /// A pool with 1000 MiB free reports 512 MiB and then refuses the 512 MiB
    /// request, because one earlier allocation landing inside that buddy split
    /// it for good — issue #62, where a spawning program sized its shared
    /// memory from the bound and died on the allocation that followed.
    ///
    /// The buddy heap does not publish its free lists, so the only honest
    /// answer is the one it gives when asked. This takes a candidate block and
    /// releases it immediately, halving until one is served. No block spans
    /// two nodes, so the answer is the largest any one node serves, and the
    /// nodes are probed in the executing processor's memory order. What it
    /// returns is a size, not a reservation: the caller still races every
    /// other processor for it, so callers keep handling
    /// [`ProgramOutOfMemory`].
    pub fn largest_servable_bytes(&self, limit: usize) -> usize {
        let home = self.locality.node_of(current_processor());
        let mut best = 0;
        for &node in self.locality.memory_order(home) {
            let pool = self.owning_pool(node);
            // The buddy heap rounds every request up to a power of two, so
            // only powers of two are worth probing.
            let mut candidate =
                previous_power_of_two(self.stats_on(node).largest_allocatable_bytes().min(limit));
            while candidate >= PhysFrame::SIZE && candidate > best {
                // The probe deliberately skips the reported-frame check and
                // the zeroing it can trigger: nothing reads this block, and
                // paying to zero half a gigabyte to answer a question would
                // be absurd.
                if let Some(ptr) = self.allocate_block(node, pool, candidate) {
                    pool.heap
                        .with(|heap| unsafe { heap.deallocate(ptr, candidate) });
                    best = candidate;
                    break;
                }
                candidate /= 2;
            }
        }
        best
    }

    /// The pool's allocation path without the reported-frame check:
    /// `processor`'s memory order, each node tried whole before the next.
    /// Answers the block, its size and the node that served it.
    fn allocate_raw(
        &self,
        layout: Layout,
        processor: ProcessorId,
    ) -> Result<(NonNull<u8>, usize, NodeId), ProgramOutOfMemory> {
        let size = buddy_allocation_size(layout);
        let home = self.locality.node_of(processor);
        for &node in self.locality.memory_order(home) {
            if let Some(ptr) = self.allocate_on_node(processor, node, layout) {
                if node != home {
                    self.count_off_node(processor);
                }
                return Ok((ptr, size, node));
            }
        }
        Err(self.machine_out_of_memory(size))
    }

    /// `layout` from `node` alone: `processor`'s cached frame of `node`
    /// for a single frame, then `node`'s buddy.
    fn allocate_on_node(
        &self,
        processor: ProcessorId,
        node: NodeId,
        layout: Layout,
    ) -> Option<NonNull<u8>> {
        let pool = self.node_pool(node)?;
        if is_single_frame_layout(layout)
            && let Some(ptr) = self.frame_slab.allocate_on(processor, node)
        {
            return Some(ptr);
        }
        self.allocate_block(node, pool, buddy_allocation_size(layout))
    }

    /// A `size`-byte block of `node`'s buddy, draining every processor's
    /// cached frames of `node` back into it before giving up.
    fn allocate_block(&self, node: NodeId, pool: &NodePool, size: usize) -> Option<NonNull<u8>> {
        pool.heap.with(|heap| {
            heap.allocate(size)
                .or_else(|_| {
                    self.frame_slab.drain(node, |ptr| unsafe {
                        heap.deallocate(ptr, PhysFrame::SIZE);
                    });
                    heap.allocate(size)
                })
                .ok()
        })
    }

    fn count_off_node(&self, processor: ProcessorId) {
        self.off_node.get(processor).fetch_add(1, Ordering::Relaxed);
    }

    fn machine_out_of_memory(&self, requested_bytes: usize) -> ProgramOutOfMemory {
        let stats = self.stats();
        ProgramOutOfMemory {
            requested_bytes,
            available_bytes: stats.available_bytes(),
            pool_bytes: stats.total_bytes,
            reserved_bytes: 0,
        }
    }

    fn node_out_of_memory(&self, node: NodeId, requested_bytes: usize) -> ProgramOutOfMemory {
        let stats = self.stats_on(node);
        ProgramOutOfMemory {
            requested_bytes,
            available_bytes: stats.available_bytes(),
            pool_bytes: stats.total_bytes,
            reserved_bytes: 0,
        }
    }

    /// One zeroed user frame, or `None` when taking it would have
    /// meant waiting on a lock.
    ///
    /// This is the page-fault path's allocation. A fault arrives inside
    /// whatever the interrupted code was doing, and that code may be
    /// holding a node's lock on this processor — the kernel heap grows
    /// out of this pool from inside the global allocator — so a blocking
    /// acquire here would spin on a word only the interrupted context can
    /// clear. The caller keeps a per-processor reserve for exactly the
    /// times this answers `None`; see [`super::frame_reserve`].
    ///
    /// It walks the same memory order as every other allocation. For each
    /// node the frame-slab list is tried first because it is the cheaper
    /// lock and the one most likely to be free; the node's buddy is tried
    /// only through [`IrqSafeMutex::try_with`], and no drain is attempted.
    /// Zeroing happens outside every lock.
    pub(crate) fn try_allocate_frame_on(&self, processor: ProcessorId) -> Option<NonNull<u8>> {
        let home = self.locality.node_of(processor);
        let (ptr, pool, node) = self.locality.memory_order(home).iter().find_map(|&node| {
            let pool = self.node_pool(node)?;
            self.frame_slab
                .try_allocate_on(processor, node)
                .or_else(|| {
                    pool.heap
                        .try_with(|heap| heap.allocate(PhysFrame::SIZE).ok())
                        .flatten()
                })
                .map(|ptr| (ptr, pool, node))
        })?;
        if node != home {
            self.count_off_node(processor);
        }
        // A frame the pool showed to a free-page consumer may have been
        // discarded by it, so the zeroing below is not an optimisation
        // that can be skipped for a frame the caller will overwrite: it
        // is what makes the page readable at all. Because it happens
        // either way, the mark is cleared only if the bitmap is free —
        // the blocking form would be waiting on whatever this fault
        // interrupted.
        let _ = pool
            .reported
            .try_clear_bytes(ptr.as_ptr() as usize, PhysFrame::SIZE);
        // SAFETY: `ptr` is one exclusively owned page from this pool.
        unsafe {
            core::ptr::write_bytes(ptr.as_ptr(), 0, PhysFrame::SIZE);
        }
        Some(ptr)
    }

    /// Returns a byte allocation to the pool, in the executing
    /// processor's name.
    ///
    /// Named apart from the frame-allocator contract's `deallocate`,
    /// which takes a frame run: one pool answers both, and a reader
    /// should not have to work out which is which from the argument
    /// list.
    pub fn deallocate_bytes(&self, ptr: NonNull<u8>, layout: Layout) {
        self.deallocate_bytes_on(current_processor(), ptr, layout);
    }

    /// Returns every frame `processor`'s lists cache to the buddy of the
    /// node it lies on, for a processor going offline: nothing pops those
    /// lists while the processor is stopped.
    ///
    /// The lists are kept per node, so each node's list drains into that
    /// node's buddy under that node's lock alone — the nesting the module
    /// contract allows — and no frame changes node. A node with no pool
    /// was never given a frame to cache, so its list is empty.
    pub(crate) fn release_processor_frames(&self, processor: ProcessorId) {
        for (index, pool) in self.nodes.iter().enumerate() {
            let Some(pool) = pool.as_ref() else {
                continue;
            };
            let node = NodeId::new(index);
            pool.heap.with(|heap| {
                self.frame_slab.drain_processor(processor, node, |ptr| {
                    // SAFETY: a frame on `node`'s list is a single frame
                    // of `node`'s buddy that no one holds.
                    unsafe { heap.deallocate(ptr, PhysFrame::SIZE) };
                });
            });
        }
    }

    /// Returns a byte allocation to the node it lies on. A single frame
    /// goes onto `processor`'s list for that node; anything larger goes
    /// back to the node's buddy.
    pub fn deallocate_bytes_on(&self, processor: ProcessorId, ptr: NonNull<u8>, layout: Layout) {
        let node = self.locality.node_of_address(ptr.as_ptr() as usize);
        let pool = self.owning_pool(node);
        if is_single_frame_layout(layout) {
            let total_frames = pool.total_bytes.load(Ordering::Acquire) / PhysFrame::SIZE;
            if self
                .frame_slab
                .deallocate_on(processor, node, ptr, total_frames)
            {
                return;
            }
        }
        pool.heap.with(|heap| unsafe {
            heap.deallocate(ptr, buddy_allocation_size(layout));
        });
    }
}

/// The pool as a frame allocator.
///
/// User memory is the guest's bulk memory: it is where wasm linear
/// memories live, it is page-granular, and running it dry costs an
/// instance rather than the kernel. That makes it the pool a memory
/// balloon draws from — the kernel heap's exhaustion is fatal, so it is
/// not something a host may ask the guest to give away.
///
/// Every operation runs in the executing processor's name, which picks
/// the frame-slab list a single frame comes from or goes to.
impl PhysFrameAllocator for UserMemoryPool {
    fn node_count(&self) -> usize {
        self.locality.node_count()
    }

    fn allocate(
        &self,
        node: NodeId,
        count: usize,
        zero_first_use: bool,
    ) -> Result<PhysFrameRange, FrameAllocError> {
        if count == 0 {
            return Err(FrameAllocError::OutOfFrames {
                requested: 0,
                available: 0,
            });
        }
        let layout = frame_layout(count);
        let ptr = self
            .allocate_on_node(current_processor(), node, layout)
            .ok_or_else(|| FrameAllocError::OutOfFrames {
                requested: count,
                available: self.stats_on(node).available_bytes() / PhysFrame::SIZE,
            })?;
        let size = buddy_allocation_size(layout);
        if !self.zero_if_reported(node, ptr, size) && zero_first_use {
            unsafe {
                core::ptr::write_bytes(ptr.as_ptr(), 0, size);
            }
        }
        Ok(PhysFrameRange::from_phys_addr(
            ptr.as_ptr() as usize,
            count * PhysFrame::SIZE,
        ))
    }

    async fn free_runs<Visit, Visited>(
        &self,
        node: NodeId,
        min_frames: usize,
        max_runs: usize,
        visit: Visit,
    ) -> usize
    where
        Visit: FnMut(PhysFrameRange) -> Visited + Send,
        Visited: Future<Output = ()> + Send,
    {
        let Some(pool) = self.node_pool(node) else {
            return 0;
        };
        visit_free_runs(
            &pool.reported,
            min_frames,
            max_runs,
            |frames| {
                self.allocate_on_node(current_processor(), node, frame_layout(frames))
                    .map(|ptr| {
                        PhysFrameRange::from_phys_addr(
                            ptr.as_ptr() as usize,
                            frames * PhysFrame::SIZE,
                        )
                    })
            },
            |range| PhysFrameAllocator::deallocate(self, range),
            visit,
        )
        .await
    }

    fn deallocate(&self, range: PhysFrameRange) {
        if range.is_empty() {
            return;
        }
        let ptr = NonNull::new(range.start.phys_addr() as *mut u8)
            .unwrap_or_else(|| panic!("user frame deallocator received a null range"));
        self.deallocate_bytes(ptr, frame_layout(range.frame_count));
    }

    fn deallocate_reported(&self, range: PhysFrameRange) {
        if range.is_empty() {
            return;
        }
        // Marked while the run is still the caller's: once it is on a
        // free list another processor may allocate it, and that
        // allocation has to find the mark. The mark lives on the node
        // the run lies on, the one `deallocate` returns it to.
        let node = self.locality.node_of_address(range.start.phys_addr());
        self.owning_pool(node).reported.mark(range);
        PhysFrameAllocator::deallocate(self, range);
    }

    fn stats(&self, node: NodeId) -> FrameAllocStats {
        let heap = self.stats_on(node);
        FrameAllocStats {
            total_frames: heap.total_bytes / PhysFrame::SIZE,
            allocated_frames: heap.allocated_bytes / PhysFrame::SIZE,
            // The buddy heap surfaces no true largest-free-run; total
            // free is the upper bound, which is what the pressure policy
            // treats it as.
            largest_free_run: heap.available_bytes() / PhysFrame::SIZE,
            reported_frames: self.node_pool(node).map_or(0, |pool| pool.reported.count()),
        }
    }
}

fn frame_layout(frames: usize) -> Layout {
    let bytes = frames * PhysFrame::SIZE;
    Layout::from_size_align(bytes, PhysFrame::SIZE)
        .unwrap_or_else(|_| panic!("user frame layout overflow for {bytes} bytes"))
}

static USER_MEMORY_POOL: AtomicPtr<UserMemoryPool> = AtomicPtr::new(core::ptr::null_mut());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserHeapStats {
    pub total_bytes: usize,
    pub allocated_bytes: usize,
}

/// One node's share of the user pool, as the statistics sample reports
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryNodeStats {
    pub node: NodeId,
    /// Usable bytes the node's pool was seeded with.
    pub total_bytes: usize,
    /// Bytes the node's pool can still hand out, cached frames included.
    pub available_bytes: usize,
    /// Allocations the node's processors had served by another node.
    pub off_node_allocations: u64,
}

/// The largest power of two not greater than `value`, or `0` for `0`.
fn previous_power_of_two(value: usize) -> usize {
    if value == 0 {
        return 0;
    }

    1_usize << (usize::BITS - 1 - value.leading_zeros())
}

impl UserHeapStats {
    pub fn available_bytes(self) -> usize {
        self.total_bytes.saturating_sub(self.allocated_bytes)
    }

    /// The largest single allocation the pool can still be asked for.
    ///
    /// [`buddy_allocation_size`] rounds every request up to a power of
    /// two, so a request for exactly [`Self::available_bytes`] is refused
    /// whenever the free byte count is not itself a power of two: the
    /// allocator asks its buddy heap for the next power of two above it,
    /// which by definition exceeds what is free. Callers that size an
    /// allocation from whatever is left of the pool must clamp to this
    /// value; clamping to `available_bytes` computes a budget the
    /// allocator can never hand out.
    ///
    /// This is the allocator's granularity bound, not a promise: a buddy
    /// heap can still refuse a block this large when its free space is
    /// fragmented across smaller buddies, so callers keep handling
    /// [`ProgramOutOfMemory`].
    pub fn largest_allocatable_bytes(self) -> usize {
        previous_power_of_two(self.available_bytes())
    }
}

/// Leaks `pool` and publishes it as the machine's user pool.
pub fn install_user_memory_pool(pool: UserMemoryPool) -> &'static UserMemoryPool {
    let pool: &'static UserMemoryPool = Box::leak(Box::new(pool));
    USER_MEMORY_POOL
        .compare_exchange(
            core::ptr::null_mut(),
            pool as *const _ as *mut _,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .unwrap_or_else(|_| panic!("user memory pool was installed more than once"));
    pool
}

pub(crate) fn installed_user_memory_pool() -> Option<&'static UserMemoryPool> {
    let ptr = USER_MEMORY_POOL.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &*ptr })
    }
}

/// The memory step of a processor going offline: every frame it holds
/// for itself — its page-fault reserve and its frame-slab lists — goes
/// back to the buddy of the node the frame lies on. Run on the departing
/// processor.
///
/// The reserve is released first and in this processor's name, so its
/// frames land on this processor's lists for their nodes, which the
/// drain after it then empties.
pub(crate) fn release_processor_frames(processor: ProcessorId) {
    super::frame_reserve::release(processor);
    if let Some(pool) = installed_user_memory_pool() {
        pool.release_processor_frames(processor);
    }
}

pub(crate) fn user_memory_pool() -> &'static UserMemoryPool {
    installed_user_memory_pool()
        .unwrap_or_else(|| panic!("user memory pool accessed before bootstrap installation"))
}

/// The machine's memory locality, installed with the user pool.
pub fn locality() -> &'static Locality {
    user_memory_pool().locality()
}

pub fn user_heap_stats() -> UserHeapStats {
    user_memory_pool().stats()
}

/// Every node's share of the user pool; see
/// [`UserMemoryPool::node_stats`].
pub fn user_node_stats() -> Vec<MemoryNodeStats> {
    user_memory_pool().node_stats()
}

/// User memory the pool can still hand out, or nothing at all before
/// the pool exists.
///
/// Unlike [`user_heap_stats`] this does not panic on a pool that has
/// not been installed: both callers run on the kernel heap's growth
/// path, which is reached during boot before the pool is there.
pub(crate) fn user_pool_available_bytes() -> usize {
    installed_user_memory_pool().map_or(0, |pool| pool.stats().available_bytes())
}

/// See [`UserMemoryPool::lend_to_kernel_heap`].
pub(crate) fn lend_user_memory_to_kernel_heap(bytes: usize) -> Option<(usize, usize)> {
    installed_user_memory_pool()?.lend_to_kernel_heap(bytes)
}

/// See [`UserMemoryPool::largest_servable_bytes`].
pub fn largest_servable_user_bytes(limit: usize) -> usize {
    user_memory_pool().largest_servable_bytes(limit)
}

pub fn allocate_user_frame_zeroed() -> Result<NonNull<u8>, ProgramOutOfMemory> {
    user_memory_pool().allocate_zeroed(frame_layout(1))
}

pub fn allocate_user_frame_zeroed_on(
    processor: ProcessorId,
) -> Result<NonNull<u8>, ProgramOutOfMemory> {
    user_memory_pool().allocate_zeroed_on(processor, frame_layout(1))
}

/// Allocates a single user-mode physical frame without zero-filling.
///
/// The returned pointer is suitable for the caller's own zero path —
/// in particular AArch64 backends use `dc zva` via
/// [`helios_hal::cpu::Cpu::zero_memory`] which is several times faster
/// than the generic memset that [`allocate_user_frame_zeroed_on`]
/// performs internally. Callers must zero or otherwise initialise the
/// frame before exposing it to user code.
pub fn allocate_user_frame_uninit_on(
    processor: ProcessorId,
) -> Result<NonNull<u8>, ProgramOutOfMemory> {
    user_memory_pool()
        .allocate_uninit_on(processor, frame_layout(1))
        .map(|(ptr, _)| ptr)
}

/// The node a user frame's memory is on. `frame` is the frame's address
/// in the kernel's direct map; an address outside the pool panics.
pub fn user_frame_node(frame: NonNull<u8>) -> NodeId {
    locality().node_of_address(frame.as_ptr() as usize)
}

/// One user frame from `node` and from no other, without zero-filling,
/// in the executing processor's name. See
/// [`UserMemoryPool::allocate_frame_uninit_on_node`].
pub fn allocate_user_frame_uninit_on_node(node: NodeId) -> Result<NonNull<u8>, ProgramOutOfMemory> {
    user_memory_pool().allocate_frame_uninit_on_node(current_processor(), node)
}

/// Allocates one physically contiguous, zeroed run of user memory.
///
/// A device that reads or writes memory by itself sees physical
/// addresses and no page table, so a DMA buffer has to be one run of
/// physical memory rather than a list of frames. The buddy allocator
/// already answers in contiguous blocks, so the run is exactly what a
/// single allocation of `layout` returns; splitting it back into
/// frames is what would be wrong here, and is why the matching release
/// takes the same layout.
pub fn allocate_user_run_zeroed_on(
    processor: ProcessorId,
    layout: Layout,
) -> Result<NonNull<u8>, ProgramOutOfMemory> {
    user_memory_pool().allocate_zeroed_on(processor, layout)
}

/// Gives back a run from [`allocate_user_run_zeroed_on`].
///
/// `layout` is the one the run was allocated with. The buddy allocator
/// derives the block order from it, so a different layout returns the
/// memory to the wrong free list.
pub fn deallocate_user_run_on(processor: ProcessorId, ptr: NonNull<u8>, layout: Layout) {
    user_memory_pool().deallocate_bytes_on(processor, ptr, layout);
}

/// See [`UserMemoryPool::try_allocate_frame_on`].
pub(crate) fn try_allocate_user_frame_zeroed_on(processor: ProcessorId) -> Option<NonNull<u8>> {
    installed_user_memory_pool()?.try_allocate_frame_on(processor)
}

pub fn deallocate_user_frame(ptr: NonNull<u8>) {
    user_memory_pool().deallocate_bytes(ptr, frame_layout(1));
}

pub fn deallocate_user_frame_on(processor: ProcessorId, ptr: NonNull<u8>) {
    user_memory_pool().deallocate_bytes_on(processor, ptr, frame_layout(1));
}

fn buddy_allocation_size(layout: Layout) -> usize {
    layout
        .size()
        .next_power_of_two()
        .max(layout.align())
        .max(PhysFrame::SIZE)
}

fn is_single_frame_layout(layout: Layout) -> bool {
    layout.size() == PhysFrame::SIZE && layout.align() == PhysFrame::SIZE
}

#[cfg(test)]
impl UserMemoryPool {
    /// A one-node pool over `regions` for `processors` processors, the
    /// machine every backend primes today.
    pub(crate) fn single_node(processors: usize, regions: &[(usize, usize)]) -> Self {
        let runs: alloc::vec::Vec<NodeRun> = regions
            .iter()
            .map(|&(start, end)| NodeRun {
                start,
                end,
                node: NodeId::FIRST,
            })
            .collect();
        Self::new(
            &helios_hal::numa::SingleNode::new(processors),
            ProcessorId::new(0),
            &runs,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use futures_lite::future::block_on;
    use helios_hal::numa::{MemoryAffinity, NodeDistance};

    fn stats(total: usize, allocated: usize) -> UserHeapStats {
        UserHeapStats {
            total_bytes: total,
            allocated_bytes: allocated,
        }
    }

    const P0: ProcessorId = ProcessorId::new(0);
    const P1: ProcessorId = ProcessorId::new(1);

    #[test]
    fn largest_allocatable_rounds_free_space_down_to_a_buddy_block() {
        // 488 MiB free: the buddy allocator would round a request for all
        // of it up to 512 MiB and refuse, so the usable request is 256 MiB.
        let free = 511_705_088;
        let stats = stats(1 << 30, (1 << 30) - free);
        assert_eq!(stats.available_bytes(), free);
        assert_eq!(stats.largest_allocatable_bytes(), 256 * 1024 * 1024);
        assert_eq!(
            buddy_allocation_size(
                Layout::from_size_align(stats.largest_allocatable_bytes(), 64 * 1024)
                    .expect("power-of-two layout is valid")
            ),
            stats.largest_allocatable_bytes(),
        );
    }

    #[test]
    fn largest_allocatable_keeps_an_exact_power_of_two() {
        let stats = stats(1 << 30, 1 << 29);
        assert_eq!(stats.largest_allocatable_bytes(), 1 << 29);
    }

    #[test]
    fn largest_allocatable_is_zero_for_an_exhausted_pool() {
        let stats = stats(1 << 30, 1 << 30);
        assert_eq!(stats.largest_allocatable_bytes(), 0);
    }

    /// A leaked block of host memory placed exactly where the caller asks:
    /// the first address at or above the allocation that is a multiple of
    /// `alignment`, plus `offset`.
    ///
    /// A buddy heap's largest block is decided by where its region starts.
    /// `add_to_heap` walks the region and takes, at each step, the largest
    /// power-of-two block that both fits in what is left and is aligned to
    /// the running start address, so a region starting one frame past its
    /// natural alignment can never be one intact buddy. Backing a pool with
    /// a plain host allocation therefore makes the test assert where the
    /// host allocator happened to place it -- an 8 MiB request comes back
    /// naturally 8 MiB-aligned on macOS arm64 and does not on the Linux CI
    /// runner. Over-allocating and carving the region out at a chosen
    /// alignment and offset makes the pool the subject instead.
    fn region_at(bytes: usize, alignment: usize, offset: usize) -> (usize, usize) {
        assert!(
            alignment.is_power_of_two(),
            "a buddy region aligns to a power of two, not {alignment}"
        );
        let layout = Layout::from_size_align(bytes + alignment + offset, PhysFrame::SIZE)
            .expect("pool layout");
        let base = unsafe { alloc::alloc::alloc(layout) } as usize;
        assert!(base != 0, "host allocation for the user pool failed");
        let start = base.next_multiple_of(alignment) + offset;
        (start, start + bytes)
    }

    fn pool_at(bytes: usize, alignment: usize, offset: usize) -> UserMemoryPool {
        UserMemoryPool::single_node(1, &[region_at(bytes, alignment, offset)])
    }

    /// A pool whose region is naturally aligned; its metadata occupies the
    /// first frames and the largest remaining buddy meets its free-byte bound.
    fn pool(bytes: usize) -> UserMemoryPool {
        assert!(
            bytes.is_power_of_two(),
            "a naturally aligned region is a power of two, not {bytes}"
        );
        pool_at(bytes, bytes, 0)
    }

    /// A pool whose region starts one frame past its natural alignment. The
    /// heap carves it into ascending blocks -- one frame, two, four, up to
    /// half the region, then a trailing frame -- so the largest block it can
    /// ever serve is half of what the pool holds.
    fn misaligned_pool(bytes: usize) -> UserMemoryPool {
        pool_at(bytes, bytes, PhysFrame::SIZE)
    }

    /// Asserts the pool hands out exactly what it just advertised.
    fn assert_advertises_what_it_serves(pool: &UserMemoryPool) -> usize {
        let advertised = pool.largest_servable_bytes(usize::MAX);
        assert!(advertised > 0, "a pool with free space advertises a block");
        let layout =
            Layout::from_size_align(advertised, PhysFrame::SIZE).expect("advertised layout");
        let (served, _, _) = pool
            .allocate_raw(layout, P0)
            .expect("the pool serves the block it advertised");
        pool.deallocate_bytes_on(P0, served, layout);
        advertised
    }

    /// Regression for #62.
    ///
    /// A spawning program sized its shared memory from
    /// `largest_allocatable_bytes`, which reads the free byte count and
    /// answers with the power of two below it. A buddy heap serves that only
    /// while the matching buddy is intact -- which it is not over a region
    /// that is not naturally aligned, and stops being the moment anything is
    /// allocated inside it. The pool then refuses a request it had just
    /// advertised, which is what the guest reported verbatim: "program memory
    /// request of 536870912 bytes exceeds its memory budget:
    /// available=1048576000 reserved=0".
    ///
    /// The misalignment is constructed rather than hoped for: a region one
    /// frame past its natural alignment holds every byte the bound counts and
    /// yet cannot hand out more than half of them at once.
    #[test]
    fn the_granularity_bound_overstates_what_the_pool_serves() {
        let bytes = 8 * 1024 * 1024;
        let layout = Layout::from_size_align(bytes * 2, bytes).expect("fragmented pool layout");
        let start = unsafe { alloc::alloc::alloc(layout) }.expose_provenance();
        assert_ne!(start, 0, "host allocation for fragmented pool failed");
        let pool = UserMemoryPool::single_node(
            1,
            &[
                (start + PhysFrame::SIZE, start + bytes / 2),
                (start + bytes + PhysFrame::SIZE, start + bytes + bytes / 2),
            ],
        );

        let bound = pool.stats().largest_allocatable_bytes();
        assert_eq!(bound, bytes / 2, "the bound excludes metadata and holes");
        let servable = assert_advertises_what_it_serves(&pool);
        assert_eq!(
            servable,
            bytes / 4,
            "neither misaligned range holds a buddy as large as the combined bound"
        );
        assert!(
            servable < bound,
            "the bound claims {bound} bytes but the pool serves {servable}"
        );

        // Fragmenting it further must not break the promise either.
        let frame = frame_layout(1);
        let (held, _, _) = pool
            .allocate_raw(frame, P0)
            .expect("a fresh pool has a frame");
        let fragmented = assert_advertises_what_it_serves(&pool);
        assert!(fragmented <= servable);

        pool.deallocate_bytes_on(P0, held, frame);
    }

    /// The other half of #62: the bound never *understates* what the pool
    /// serves. Over a naturally aligned region the two meet exactly, and
    /// `largest_servable_bytes` must not answer with a smaller block than
    /// the pool would hand out -- a caller that sized a spawn from it would
    /// leave the difference unusable.
    #[test]
    fn an_aligned_pool_serves_exactly_its_bound() {
        let bytes = 8 * 1024 * 1024;
        let pool = pool(bytes);

        let bound = pool.stats().largest_allocatable_bytes();
        assert_eq!(bound, bytes / 2);
        assert!(pool.stats().allocated_bytes > 0);
        assert_eq!(
            assert_advertises_what_it_serves(&pool),
            bound,
            "an intact buddy serves the whole bound"
        );
    }

    #[test]
    fn an_exhausted_pool_advertises_nothing() {
        // A region the heap holds as one block would drain in a single
        // request and prove nothing; the misaligned one makes the loop walk
        // down every buddy the carve produced.
        let pool = misaligned_pool(8 * 1024 * 1024);
        let mut held = Vec::new();

        // Taking the largest servable block repeatedly is the only way to
        // actually drain a buddy heap: asking for `available_bytes` in one go
        // is refused even on an untouched pool, which is the bug this whole
        // family of tests is about.
        loop {
            let servable = pool.largest_servable_bytes(usize::MAX);
            if servable == 0 {
                break;
            }
            let layout =
                Layout::from_size_align(servable, PhysFrame::SIZE).expect("servable layout");
            let (ptr, _, _) = pool
                .allocate_raw(layout, P0)
                .expect("the pool serves the block it advertised");
            held.push((ptr, layout));
        }

        assert_eq!(pool.largest_servable_bytes(usize::MAX), 0);
        assert!(
            pool.stats().available_bytes() < PhysFrame::SIZE,
            "draining by servable blocks leaves less than a frame"
        );

        for (ptr, layout) in held {
            pool.deallocate_bytes_on(P0, ptr, layout);
        }
    }

    #[test]
    fn a_limit_caps_what_the_pool_advertises() {
        let pool = pool(8 * 1024 * 1024);

        assert_eq!(
            pool.largest_servable_bytes(1024 * 1024),
            1024 * 1024,
            "a caller's cap is applied before the pool is asked"
        );
        assert_eq!(
            pool.largest_servable_bytes(3 * 1024 * 1024),
            2 * 1024 * 1024,
            "a cap that is not a power of two rounds down to a buddy block"
        );
    }

    fn tail_byte(address: usize, bytes: usize) -> u8 {
        unsafe { core::ptr::read_volatile((address + bytes - 1) as *const u8) }
    }

    /// The balloon draws from user memory, so the pool has to answer the
    /// frame-allocator contract as well as the wasm allocation paths.
    #[test]
    fn the_pool_hands_out_and_takes_back_contiguous_frames() {
        let pool = pool(8 * 1024 * 1024);
        let free_before = PhysFrameAllocator::stats(&pool, NodeId::FIRST).free_frames();

        let range = pool.allocate(NodeId::FIRST, 64, false).expect("64 frames");
        assert_eq!(range.frame_count, 64);
        assert!(PhysFrameAllocator::stats(&pool, NodeId::FIRST).free_frames() < free_before);

        PhysFrameAllocator::deallocate(&pool, range);
        assert_eq!(
            PhysFrameAllocator::stats(&pool, NodeId::FIRST).free_frames(),
            free_before,
            "a returned run is free again"
        );
    }

    /// A reporting pass names as much of the pool as the caller lets it,
    /// in runs of the size the caller asked for. A pass that could only
    /// ever name one run would leave a mostly idle guest holding memory
    /// its host could have back.
    #[test]
    fn a_pass_names_as_many_runs_as_the_pool_can_spare() {
        let pool = pool(64 * 1024 * 1024);
        let free_before = PhysFrameAllocator::stats(&pool, NodeId::FIRST).free_frames();

        let mut runs = 0usize;
        let named = block_on(pool.free_runs(NodeId::FIRST, 512, 256, |run| {
            assert_eq!(run.frame_count, 512);
            runs += 1;
            core::future::ready(())
        }));

        assert_eq!(named, runs);
        assert!(
            runs >= 24,
            "a 64 MiB pool has more than {runs} spare 2 MiB runs"
        );
        assert_eq!(
            PhysFrameAllocator::stats(&pool, NodeId::FIRST).free_frames(),
            free_before,
            "a pass leaves the pool as it found it"
        );
        assert_eq!(
            PhysFrameAllocator::stats(&pool, NodeId::FIRST).reported_frames,
            runs * 512
        );
    }

    /// Reporting user memory to a host that may discard it means the
    /// next wasm instance to be handed that memory must not see a hole,
    /// even on the uninit path that normally skips zeroing.
    #[test]
    fn reported_user_memory_is_zeroed_before_it_is_handed_out_again() {
        let pool = pool(4 * 1024 * 1024);

        let mut reported = None;
        assert_eq!(
            block_on(pool.free_runs(NodeId::FIRST, 16, 1, |run| {
                unsafe {
                    core::ptr::write_bytes(run.start.phys_addr() as *mut u8, 0xc3, run.byte_size());
                }
                reported = Some(run);
                core::future::ready(())
            })),
            1
        );
        let reported = reported.expect("one run was reported");
        assert_eq!(
            PhysFrameAllocator::stats(&pool, NodeId::FIRST).reported_frames,
            reported.frame_count
        );

        let layout =
            Layout::from_size_align(reported.byte_size(), PhysFrame::SIZE).expect("frame layout");
        let (ptr, size) = pool
            .allocate_uninit(P0, layout)
            .expect("the reported run is free again");
        assert_eq!(ptr.as_ptr() as usize, reported.start.phys_addr());
        assert_eq!(
            tail_byte(ptr.as_ptr() as usize, size),
            0,
            "user memory the host may have discarded is zeroed on reuse"
        );
        assert_eq!(
            PhysFrameAllocator::stats(&pool, NodeId::FIRST).reported_frames,
            0
        );
    }

    /// A machine a test spells out: which node each processor is on and
    /// the distance matrix. The pool never reads the affinity ranges; the
    /// runs it is handed carry their nodes.
    struct Machine {
        processor_nodes: Vec<usize>,
        /// Row-major, `node_count²`.
        distances: Vec<u8>,
    }

    impl NumaTopology for Machine {
        type MemoryAffinities<'a> = core::iter::Empty<MemoryAffinity>;

        fn node_count(&self) -> usize {
            self.distances.len().isqrt()
        }

        fn processor_count(&self) -> usize {
            self.processor_nodes.len()
        }

        fn processor_node(&self, processor: ProcessorId) -> NodeId {
            NodeId::new(self.processor_nodes[usize::from(processor.id())])
        }

        fn distance(&self, from: NodeId, to: NodeId) -> NodeDistance {
            NodeDistance::new(self.distances[from.index() * self.node_count() + to.index()])
        }

        fn memory_affinity(&self) -> Self::MemoryAffinities<'_> {
            core::iter::empty()
        }
    }

    const NODE_BYTES: usize = 4 * 1024 * 1024;

    /// A pool with one naturally aligned `NODE_BYTES` region per node of
    /// `machine`, answering the pool and each node's range.
    fn machine_pool(machine: &Machine) -> (UserMemoryPool, Vec<(usize, usize)>) {
        let ranges: Vec<_> = (0..machine.node_count())
            .map(|_| region_at(NODE_BYTES, NODE_BYTES, 0))
            .collect();
        let runs: Vec<_> = ranges
            .iter()
            .enumerate()
            .map(|(node, &(start, end))| NodeRun {
                start,
                end,
                node: NodeId::new(node),
            })
            .collect();
        (UserMemoryPool::new(machine, P0, &runs), ranges)
    }

    /// P0 on node 0, P1 on node 1, remote at Linux's default distance.
    fn two_nodes() -> (UserMemoryPool, Vec<(usize, usize)>) {
        machine_pool(&Machine {
            processor_nodes: alloc::vec![0, 1],
            #[rustfmt::skip]
            distances: alloc::vec![
                10, 20,
                20, 10,
            ],
        })
    }

    fn node_of(ranges: &[(usize, usize)], ptr: NonNull<u8>) -> usize {
        let address = ptr.as_ptr() as usize;
        ranges
            .iter()
            .position(|&(start, end)| (start..end).contains(&address))
            .unwrap_or_else(|| panic!("{address:#x} lies in no node's range"))
    }

    /// Takes every frame `node` has, frame by frame, and from no other
    /// node.
    fn exhaust(pool: &UserMemoryPool, node: NodeId) -> Vec<PhysFrameRange> {
        let mut held = Vec::new();
        while let Ok(range) = pool.allocate(node, 1, false) {
            held.push(range);
        }
        assert_eq!(pool.stats_on(node).available_bytes(), 0);
        held
    }

    fn frame_ptr(range: PhysFrameRange) -> NonNull<u8> {
        NonNull::new(range.start.phys_addr() as *mut u8).expect("a pool frame is not null")
    }

    #[test]
    fn each_processor_is_served_by_its_own_node() {
        let (pool, ranges) = two_nodes();
        for _ in 0..8 {
            let near = pool.allocate_zeroed_on(P0, frame_layout(1)).expect("frame");
            assert_eq!(node_of(&ranges, near), 0);
            let far = pool.allocate_zeroed_on(P1, frame_layout(1)).expect("frame");
            assert_eq!(node_of(&ranges, far), 1);
        }
        let run = Layout::from_size_align(64 * 1024, PhysFrame::SIZE).expect("run");
        assert_eq!(
            node_of(&ranges, pool.allocate_zeroed_on(P1, run).expect("run")),
            1
        );
        assert_eq!(pool.off_node_allocations(NodeId::new(0)), 0);
        assert_eq!(pool.off_node_allocations(NodeId::new(1)), 0);
    }

    #[test]
    fn node_stats_report_each_node_in_order() {
        let (pool, _ranges) = two_nodes();
        let before = pool.node_stats();
        assert_eq!(before.len(), 2);
        assert_eq!(
            before
                .iter()
                .map(|node| node.node.index())
                .collect::<Vec<_>>(),
            [0, 1]
        );
        for (index, node) in before.iter().enumerate() {
            let heap = pool.stats_on(NodeId::new(index));
            assert_eq!(node.total_bytes, heap.total_bytes);
            assert_eq!(node.available_bytes, heap.available_bytes());
            assert_eq!(node.off_node_allocations, 0);
        }

        let _held = exhaust(&pool, NodeId::new(0));
        let _spilled = pool.allocate_zeroed_on(P0, frame_layout(1)).expect("frame");
        let after = pool.node_stats();
        assert_eq!(after[0].available_bytes, 0);
        assert_eq!(after[0].off_node_allocations, 1);
        assert_eq!(after[1].off_node_allocations, 0);
        assert!(after[1].available_bytes < before[1].available_bytes);
    }

    #[test]
    fn an_exhausted_node_falls_back_to_the_next_and_counts_it() {
        let (pool, ranges) = two_nodes();
        let _held = exhaust(&pool, NodeId::new(0));

        let frame = pool.allocate_zeroed_on(P0, frame_layout(1)).expect("frame");
        assert_eq!(node_of(&ranges, frame), 1, "node 0 has nothing left");
        assert_eq!(pool.off_node_allocations(NodeId::new(0)), 1);
        assert_eq!(pool.off_node_allocations(NodeId::new(1)), 0);

        let faulted = pool.try_allocate_frame_on(P0).expect("fault frame");
        assert_eq!(
            node_of(&ranges, faulted),
            1,
            "the fault path walks the same order"
        );
        assert_eq!(pool.off_node_allocations(NodeId::new(0)), 2);
    }

    #[test]
    fn a_node_is_served_from_itself_or_refused() {
        let (pool, ranges) = two_nodes();
        let held = exhaust(&pool, NodeId::new(0));
        assert!(
            held.iter()
                .all(|range| node_of(&ranges, frame_ptr(*range)) == 0),
            "every frame named node 0 came from node 0"
        );
        assert!(matches!(
            pool.allocate(NodeId::new(0), 1, false),
            Err(FrameAllocError::OutOfFrames { available: 0, .. })
        ));
        assert!(
            pool.allocate_frame_uninit_on_node(P1, NodeId::new(0))
                .is_err(),
            "a relocation stays on its node or does not happen"
        );
        let other = pool
            .allocate(NodeId::new(1), 1, false)
            .expect("node 1 frame");
        assert_eq!(node_of(&ranges, frame_ptr(other)), 1);
        assert_eq!(
            pool.off_node_allocations(NodeId::new(0)),
            0,
            "an allocation that names its node is never off-node"
        );
    }

    #[test]
    fn a_cross_processor_free_returns_the_frame_to_its_own_node() {
        let (pool, ranges) = two_nodes();
        let node_0 = NodeId::new(0);
        let node_1 = NodeId::new(1);
        let node_0_before = pool.stats_on(node_0).available_bytes();
        let node_1_before = pool.stats_on(node_1).available_bytes();

        let frame = pool.allocate_zeroed_on(P0, frame_layout(1)).expect("frame");
        assert_eq!(node_of(&ranges, frame), 0);
        assert_eq!(
            pool.stats_on(node_0).available_bytes(),
            node_0_before - PhysFrame::SIZE
        );

        pool.deallocate_bytes_on(P1, frame, frame_layout(1));
        assert_eq!(pool.stats_on(node_0).available_bytes(), node_0_before);
        assert_eq!(pool.stats_on(node_1).available_bytes(), node_1_before);
        assert_eq!(pool.frame_slab.cached_bytes(node_0), PhysFrame::SIZE);
        assert_eq!(
            pool.frame_slab.allocate_on(P1, node_0),
            Some(frame),
            "the frame sits on P1's node-0 list"
        );
        pool.deallocate_bytes_on(P1, frame, frame_layout(1));
    }

    /// A processor going offline hands every frame its lists cache back
    /// to the buddy of the node the frame lies on, and leaves the other
    /// processors' lists alone.
    #[test]
    fn an_offline_processors_cached_frames_return_to_their_own_nodes() {
        let (pool, ranges) = two_nodes();
        let node_0 = NodeId::new(0);
        let node_1 = NodeId::new(1);
        let buddy_allocated = |node: NodeId| {
            pool.owning_pool(node)
                .heap
                .with(|heap| heap.allocated_bytes)
        };
        let before = [buddy_allocated(node_0), buddy_allocated(node_1)];

        let local = pool
            .allocate_zeroed_on(P0, frame_layout(1))
            .expect("node-0 frame");
        let remote = pool
            .allocate_zeroed_on(P1, frame_layout(1))
            .expect("node-1 frame");
        let kept = pool
            .allocate_zeroed_on(P1, frame_layout(1))
            .expect("second node-1 frame");
        assert_eq!((node_of(&ranges, local), node_of(&ranges, remote)), (0, 1));
        pool.deallocate_bytes_on(P0, local, frame_layout(1));
        pool.deallocate_bytes_on(P0, remote, frame_layout(1));
        pool.deallocate_bytes_on(P1, kept, frame_layout(1));
        assert_eq!(pool.frame_slab.cached_bytes(node_0), PhysFrame::SIZE);
        assert_eq!(pool.frame_slab.cached_bytes(node_1), 2 * PhysFrame::SIZE);

        pool.release_processor_frames(P0);

        assert_eq!(pool.frame_slab.cached_bytes(node_0), 0);
        assert_eq!(
            pool.frame_slab.cached_bytes(node_1),
            PhysFrame::SIZE,
            "P1's node-1 list is untouched"
        );
        assert_eq!(buddy_allocated(node_0), before[0]);
        assert_eq!(
            buddy_allocated(node_1),
            before[1] + PhysFrame::SIZE,
            "only the frame P1 still caches is out of node 1's buddy"
        );
        assert_eq!(pool.frame_slab.allocate_on(P1, node_1), Some(kept));
    }

    #[test]
    fn a_node_drains_its_cached_frames_before_falling_back() {
        let (pool, ranges) = two_nodes();
        let held = exhaust(&pool, NodeId::new(0));
        let cached = frame_ptr(held[0]);
        pool.deallocate_bytes_on(P1, cached, frame_layout(1));

        let frame = pool.allocate_zeroed_on(P0, frame_layout(1)).expect("frame");
        assert_eq!(frame, cached, "P1's node-0 frame is drained and reused");
        assert_eq!(node_of(&ranges, frame), 0);
        assert_eq!(pool.off_node_allocations(NodeId::new(0)), 0);
    }

    #[test]
    fn the_nearest_node_serves_first_when_distances_are_asymmetric() {
        let (pool, ranges) = machine_pool(&Machine {
            processor_nodes: alloc::vec![0, 1, 2],
            #[rustfmt::skip]
            distances: alloc::vec![
                10, 30, 20,
                30, 10, 20,
                20, 20, 10,
            ],
        });
        let _held = exhaust(&pool, NodeId::new(0));

        let frame = pool.allocate_zeroed_on(P0, frame_layout(1)).expect("frame");
        assert_eq!(node_of(&ranges, frame), 2, "node 2 is nearer to node 0");

        let _rest = exhaust(&pool, NodeId::new(2));
        let frame = pool.allocate_zeroed_on(P0, frame_layout(1)).expect("frame");
        assert_eq!(node_of(&ranges, frame), 1);
        assert_eq!(pool.off_node_allocations(NodeId::new(0)), 2);
    }

    #[test]
    fn a_node_without_memory_is_served_by_its_nearest_and_reports_nothing() {
        let ranges = [region_at(NODE_BYTES, NODE_BYTES, 0)];
        let pool = UserMemoryPool::new(
            &Machine {
                processor_nodes: alloc::vec![0, 1],
                #[rustfmt::skip]
                distances: alloc::vec![
                    10, 20,
                    20, 10,
                ],
            },
            P0,
            &[NodeRun {
                start: ranges[0].0,
                end: ranges[0].1,
                node: NodeId::new(0),
            }],
        );
        let frame = pool.allocate_zeroed_on(P1, frame_layout(1)).expect("frame");
        assert_eq!(node_of(&ranges, frame), 0);
        assert_eq!(pool.off_node_allocations(NodeId::new(1)), 1);

        let empty = PhysFrameAllocator::stats(&pool, NodeId::new(1));
        assert_eq!(empty.total_frames, 0);
        assert_eq!(empty.free_frames(), 0);
        assert!(pool.allocate(NodeId::new(1), 1, false).is_err());
        assert_eq!(
            block_on(pool.free_runs(NodeId::new(1), 1, 1, |_| core::future::ready(()))),
            0
        );
    }

    /// The machine-wide figures are every node's added up, and a frame run
    /// reported on one node is counted on that node alone.
    #[test]
    fn statistics_and_reporting_are_kept_per_node() {
        let (pool, _) = two_nodes();
        let node_0 = pool.stats_on(NodeId::new(0));
        let node_1 = pool.stats_on(NodeId::new(1));
        assert_eq!(
            pool.stats().total_bytes,
            node_0.total_bytes + node_1.total_bytes
        );
        assert_eq!(
            pool.stats().available_bytes(),
            node_0.available_bytes() + node_1.available_bytes()
        );
        assert_eq!(PhysFrameAllocator::node_count(&pool), 2);

        let named = block_on(pool.free_runs(NodeId::new(1), 16, 2, |_| core::future::ready(())));
        assert_eq!(named, 2);
        assert_eq!(
            PhysFrameAllocator::stats(&pool, NodeId::new(1)).reported_frames,
            32
        );
        assert_eq!(
            PhysFrameAllocator::stats(&pool, NodeId::new(0)).reported_frames,
            0
        );
    }

    /// The kernel heap grows out of the bootstrap processor's node, and the
    /// memory it takes stops counting towards that node's frame cache.
    #[test]
    fn a_lend_comes_from_the_bootstrap_node() {
        let (pool, ranges) = machine_pool(&Machine {
            processor_nodes: alloc::vec![1, 0],
            #[rustfmt::skip]
            distances: alloc::vec![
                10, 20,
                20, 10,
            ],
        });
        let node_1 = pool.owning_pool(NodeId::new(1));
        let before = node_1.total_bytes.load(Ordering::Acquire);

        let (start, end) = pool.lend_to_kernel_heap(64 * 1024).expect("a lend");
        let lent = NonNull::new(start as *mut u8).expect("lend start");
        assert_eq!(node_of(&ranges, lent), 1, "P0, the bootstrap, is on node 1");
        assert_eq!(end - start, 64 * 1024);
        assert_eq!(
            node_1.total_bytes.load(Ordering::Acquire),
            before - 64 * 1024
        );
    }

    /// A run handed back through `deallocate_reported` is one whose
    /// contents something outside the guest may have dropped, so it
    /// carries the reported mark from the moment it is free: it counts
    /// as reported on the node it lies on and on no other, and the next
    /// caller to be handed it, on the uninit path included, gets zeroes.
    #[test]
    fn a_run_returned_reported_is_zeroed_when_it_is_handed_out_again() {
        let (pool, ranges) = two_nodes();
        let node = NodeId::new(1);
        let free_before = PhysFrameAllocator::stats(&pool, node).free_frames();

        let run = PhysFrameAllocator::allocate(&pool, node, 16, false).expect("a 16-frame run");
        assert_eq!(node_of(&ranges, frame_ptr(run)), 1);
        unsafe {
            core::ptr::write_bytes(run.start.phys_addr() as *mut u8, 0xc3, run.byte_size());
        }
        PhysFrameAllocator::deallocate_reported(&pool, run);
        let stats = PhysFrameAllocator::stats(&pool, node);
        assert_eq!(stats.free_frames(), free_before, "the run is free again");
        assert_eq!(stats.reported_frames, run.frame_count);
        assert_eq!(
            PhysFrameAllocator::stats(&pool, NodeId::new(0)).reported_frames,
            0,
            "the mark is kept by the node the run lies on"
        );

        let layout =
            Layout::from_size_align(run.byte_size(), PhysFrame::SIZE).expect("frame layout");
        let (ptr, size) = pool
            .allocate_uninit(P1, layout)
            .expect("the returned run is free");
        assert_eq!(ptr.as_ptr() as usize, run.start.phys_addr());
        assert_eq!(
            tail_byte(ptr.as_ptr() as usize, size),
            0,
            "the run is zeroed on reuse"
        );
        assert_eq!(PhysFrameAllocator::stats(&pool, node).reported_frames, 0);
    }
}
