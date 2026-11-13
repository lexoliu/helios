//! One structure per processor, each backed by memory of the node its
//! processor is on.
//!
//! [`PerProcessor<T>`] is Linux's per-cpu first chunk grouped by node
//! (`setup_per_cpu_areas` allocating each group's units from that node's
//! memory): the table of blocks is a small pointer array on the kernel
//! heap, and each block is carved from the [`NodeLocalArena`] of the
//! node its processor sits on, so the words a processor writes on every
//! pass live in its own node's memory and on a cache line of their own.
//!
//! # Where the bytes come from
//!
//! A [`NodeLocalArena`] grows by lending: it takes one buddy block from
//! its node's memory order (the same one-way transfer
//! [`UserMemoryPool::lend_to_kernel_heap`](super::UserMemoryPool) makes,
//! so the lent bytes stop counting towards the node's pool) and bump
//! carves it. Nothing carved is ever returned: the structures built here
//! are boot-time and live as long as the machine, exactly as the
//! `Once`-held statics they replace did. What is returned is a block's
//! unused tail, so a task arena of 24 MiB costs 24 MiB and not the 32 MiB
//! buddy block it was lent out of.
//!
//! The structures' own later allocations (a socket inside a network
//! shard, a queue's buffer) still come from the machine-wide kernel heap.
//!
//! # Concurrency contract
//!
//! - [`NodeLocalArena`]: carving takes the arena's [`IrqSafeMutex`] from
//!   any processor and holds it across the lend, which takes the lent
//!   node's buddy lock: arena, then buddy, and nothing takes them the
//!   other way round. It is boot-time work, never reached from an
//!   interrupt handler. Each arena sits on its own cache line.
//! - [`PerProcessor`]: the table of blocks is immutable once built, so
//!   [`PerProcessor::get`] and [`PerProcessor::iter`] are lock-free from
//!   any processor. Construction may run on any processor and takes the
//!   arena locks. What each `T` says about which processors write it is
//!   `T`'s own contract; the table only guarantees that no two
//!   processors' blocks share a cache line.
//! - [`LocalSlice`]: owned by the structure that carved it, with the
//!   contract of its element type.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::ProcessorId;
use helios_hal::numa::NodeId;
use helios_hal::pmm::PhysFrame;

use super::irq_safe::IrqSafeMutex;
use super::locality::Locality;
use super::user::UserMemoryPool;

/// The block an arena lends when a carve does not fit what it holds.
///
/// Small structures (an owner cell, a reserve shard) are packed into one
/// chunk rather than each taking a buddy block of their own.
const NODE_ARENA_CHUNK_BYTES: usize = 16 * PhysFrame::SIZE;

/// Where a [`NodeLocalArena`] gets its memory and where it returns an
/// unused tail.
///
/// Implemented by the user pool, and by the node buddies it is built
/// from while it does not yet exist.
pub trait NodeMemory {
    /// One block of `bytes` — a power of two, at least a frame — from
    /// the nearest node with memory to `node`, taken for good. Answers
    /// its address, aligned to its own size within its node's buddy.
    fn lend(&self, node: NodeId, bytes: usize) -> Option<usize>;

    /// Returns `bytes` at `start` to the node it lies on.
    ///
    /// # Safety
    ///
    /// The range is part of a block [`Self::lend`] answered, aligned to
    /// its own size relative to that block, not returned before, and
    /// nothing uses it any more: the node hands it out again.
    unsafe fn give_back(&self, start: usize, bytes: usize);
}

/// A node's bump region of lent memory.
///
/// See the module docs for the concurrency contract.
pub struct NodeLocalArena {
    bump: IrqSafeMutex<Bump>,
    /// Bytes this arena took from the pool and kept: lent less returned.
    /// Written under the lock, read from anywhere.
    lent_bytes: AtomicUsize,
}

/// The block currently being carved.
struct Bump {
    /// Where the lent block starts: the origin its tail's alignment is
    /// measured from.
    block: usize,
    cursor: usize,
    end: usize,
}

impl Bump {
    fn take(&mut self, layout: Layout) -> Option<usize> {
        let start = self.cursor.next_multiple_of(layout.align());
        let end = start.checked_add(layout.size())?;
        (end <= self.end).then(|| {
            self.cursor = end;
            start
        })
    }
}

impl NodeLocalArena {
    pub(crate) const fn new() -> Self {
        Self {
            bump: IrqSafeMutex::new(Bump {
                block: 0,
                cursor: 0,
                end: 0,
            }),
            lent_bytes: AtomicUsize::new(0),
        }
    }

    /// Bytes this arena has taken out of the pool for good.
    pub(crate) fn lent_bytes(&self) -> usize {
        self.lent_bytes.load(Ordering::Relaxed)
    }

    /// `layout` bytes of `node`'s memory, uninitialised and never freed.
    ///
    /// Panics when the pool has nothing to lend: this is boot-time
    /// kernel memory, and kernel out-of-memory is fatal.
    fn carve<Source: NodeMemory>(
        &self,
        node: NodeId,
        source: &Source,
        layout: Layout,
    ) -> NonNull<u8> {
        assert!(
            layout.align().is_power_of_two() && layout.align() <= NODE_ARENA_CHUNK_BYTES,
            "a per-processor allocation aligned to {} bytes exceeds the arena chunk",
            layout.align()
        );
        if layout.size() == 0 {
            // Nothing to back: any address aligned for the type serves.
            return NonNull::new(core::ptr::without_provenance_mut(layout.align()))
                .unwrap_or_else(|| panic!("a layout's alignment is never zero"));
        }
        let address = self.bump.with(|bump| {
            if let Some(address) = bump.take(layout) {
                return address;
            }
            if layout.size() >= NODE_ARENA_CHUNK_BYTES {
                return self.carve_large(node, source, layout);
            }
            // The chunk is spent: hand what is left of it back and
            // start the next one.
            self.release_tail(source, bump.block, bump.cursor, bump.end);
            let block = self.lend(node, source, NODE_ARENA_CHUNK_BYTES);
            *bump = Bump {
                block,
                cursor: block,
                end: block + NODE_ARENA_CHUNK_BYTES,
            };
            bump.take(layout)
                .unwrap_or_else(|| panic!("a fresh arena chunk cannot hold {layout:?}"))
        });
        NonNull::new(core::ptr::with_exposed_provenance_mut(address))
            .unwrap_or_else(|| panic!("the node arena handed out a null address"))
    }

    /// A carve too big to share a chunk: its own block, with the part
    /// of the block it does not use going straight back.
    fn carve_large<Source: NodeMemory>(
        &self,
        node: NodeId,
        source: &Source,
        layout: Layout,
    ) -> usize {
        let used = layout.size().next_multiple_of(PhysFrame::SIZE);
        let block_bytes = used.next_power_of_two().max(layout.align());
        let block = self.lend(node, source, block_bytes);
        assert!(
            block.is_multiple_of(layout.align()),
            "a lent block at {block:#x} is not aligned to {} bytes",
            layout.align()
        );
        self.release_tail(source, block, block + used, block + block_bytes);
        block
    }

    fn lend<Source: NodeMemory>(&self, node: NodeId, source: &Source, bytes: usize) -> usize {
        let block = source.lend(node, bytes).unwrap_or_else(|| {
            panic!(
                "no node has {bytes} bytes to lend to node {}'s per-processor arena: kernel \
                 memory is exhausted",
                node.index()
            )
        });
        self.lent_bytes.fetch_add(bytes, Ordering::Relaxed);
        block
    }

    /// Returns the whole frames of `from..to`, a stretch of the block
    /// that starts at `block`, to where they came from.
    fn release_tail<Source: NodeMemory>(
        &self,
        source: &Source,
        block: usize,
        from: usize,
        to: usize,
    ) {
        if block == 0 {
            return;
        }
        let from = from.next_multiple_of(PhysFrame::SIZE);
        for (start, bytes) in aligned_pieces(block, from, to) {
            // SAFETY: `from..to` is the uncarved tail of a block this
            // arena was lent and holds under its lock, each piece is
            // aligned to its own size relative to the block, and the
            // caller moves the bump past the tail before unlocking.
            unsafe { source.give_back(start, bytes) };
            self.lent_bytes.fetch_sub(bytes, Ordering::Relaxed);
        }
    }
}

/// `from..to` as the fewest power-of-two pieces each aligned to its own
/// size relative to `block`, which is the alignment a buddy allocator
/// takes a free from.
fn aligned_pieces(block: usize, from: usize, to: usize) -> impl Iterator<Item = (usize, usize)> {
    let mut at = from;
    core::iter::from_fn(move || {
        let left = to.checked_sub(at).filter(|left| *left >= PhysFrame::SIZE)?;
        let offset = at - block;
        let by_alignment = if offset == 0 {
            usize::MAX
        } else {
            1 << offset.trailing_zeros()
        };
        let by_length = 1 << left.ilog2();
        let bytes = by_alignment.min(by_length);
        let start = at;
        at += bytes;
        Some((start, bytes))
    })
}

/// The arenas of every node, index: [`NodeId`].
pub(crate) struct NodeArenas {
    arenas: Box<[CachePadded<NodeLocalArena>]>,
}

impl NodeArenas {
    pub(crate) fn new(node_count: usize) -> Self {
        Self {
            arenas: (0..node_count)
                .map(|_| CachePadded::new(NodeLocalArena::new()))
                .collect(),
        }
    }

    fn arena(&self, node: NodeId) -> &NodeLocalArena {
        &self.arenas[node.index()]
    }

    /// Bytes the arenas have taken out of the pool, summed.
    pub(crate) fn lent_bytes(&self) -> usize {
        self.arenas.iter().map(|arena| arena.lent_bytes()).sum()
    }
}

/// The memory a processor's block can still be given, in the node its
/// processor is on.
///
/// Handed to the closure [`PerProcessor::new`] builds each block with, so
/// a block that owns more than its own bytes (a stack, a table sized by
/// the node count) takes them from the same node.
pub struct LocalMemory<'a, Source: NodeMemory> {
    node: NodeId,
    arena: &'a NodeLocalArena,
    source: &'a Source,
}

impl<Source: NodeMemory> LocalMemory<'_, Source> {
    /// The node the memory is on.
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// `layout` bytes, uninitialised, never freed.
    pub fn carve(&self, layout: Layout) -> NonNull<u8> {
        self.arena.carve(self.node, self.source, layout)
    }

    /// `layout` bytes, zeroed, never freed.
    pub fn carve_zeroed(&self, layout: Layout) -> NonNull<u8> {
        let ptr = self.carve(layout);
        // SAFETY: the carve is `layout.size()` bytes nobody else holds.
        unsafe { core::ptr::write_bytes(ptr.as_ptr(), 0, layout.size()) };
        ptr
    }

    /// A slice of `len` elements built by `init`, never freed.
    pub fn carve_slice<T>(&self, len: usize, mut init: impl FnMut(usize) -> T) -> LocalSlice<T> {
        let layout = Layout::array::<T>(len)
            .unwrap_or_else(|_| panic!("a per-processor slice of {len} elements overflows"));
        let ptr = if layout.size() == 0 {
            NonNull::dangling()
        } else {
            self.carve(layout).cast::<T>()
        };
        for index in 0..len {
            // SAFETY: `index < len` elements fit in the carve.
            unsafe { ptr.add(index).write(init(index)) };
        }
        LocalSlice { ptr, len }
    }
}

/// A slice carved from a node's arena. Its memory is never returned; its
/// elements are dropped with it.
pub struct LocalSlice<T> {
    ptr: NonNull<T>,
    len: usize,
}

// SAFETY: a `LocalSlice` owns its elements exclusively, like a `Box<[T]>`.
unsafe impl<T: Send> Send for LocalSlice<T> {}
unsafe impl<T: Sync> Sync for LocalSlice<T> {}

impl<T> Deref for LocalSlice<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: `len` initialised elements at `ptr`.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl<T> Drop for LocalSlice<T> {
    fn drop(&mut self) {
        // SAFETY: the elements are initialised and dropped once; the
        // memory itself is the arena's and stays.
        unsafe {
            core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(
                self.ptr.as_ptr(),
                self.len,
            ));
        }
    }
}

/// One `T` per processor, each on its own cache line and in memory of
/// the node its processor is on.
///
/// Index: [`ProcessorId`]. See the module docs for the concurrency
/// contract.
pub struct PerProcessor<T> {
    blocks: Box<[NonNull<CachePadded<T>>]>,
}

// SAFETY: the table is immutable and each block is a `T` shared by
// reference, so the table is as thread-safe as `T`.
unsafe impl<T: Send> Send for PerProcessor<T> {}
unsafe impl<T: Send + Sync> Sync for PerProcessor<T> {}

impl<T> PerProcessor<T> {
    /// One block per processor of the machine `pool` describes, each
    /// built by `init` on the calling processor and carved from the
    /// memory of its processor's node.
    pub fn new(
        pool: &UserMemoryPool,
        init: impl FnMut(ProcessorId, &LocalMemory<'_, UserMemoryPool>) -> T,
    ) -> Self {
        Self::build(pool.locality(), pool.arenas(), pool, init)
    }

    /// One block per processor of `locality`, each built by `init` on
    /// the calling processor and placed in its processor's node's
    /// memory.
    pub(crate) fn build<Source: NodeMemory>(
        locality: &Locality,
        arenas: &NodeArenas,
        source: &Source,
        mut init: impl FnMut(ProcessorId, &LocalMemory<'_, Source>) -> T,
    ) -> Self {
        let mut blocks = Vec::with_capacity(locality.processor_count());
        for index in 0..locality.processor_count() {
            let processor = ProcessorId::new(
                u16::try_from(index)
                    .unwrap_or_else(|_| panic!("processor index {index} exceeds u16")),
            );
            let node = locality.node_of(processor);
            let local = LocalMemory {
                node,
                arena: arenas.arena(node),
                source,
            };
            let value = init(processor, &local);
            let block = local.carve(Layout::new::<CachePadded<T>>()).cast();
            // SAFETY: a fresh carve of exactly one `CachePadded<T>`.
            unsafe { block.write(CachePadded::new(value)) };
            blocks.push(block);
        }
        Self {
            blocks: blocks.into_boxed_slice(),
        }
    }

    /// How many processors there are blocks for.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether there are no blocks, which a machine never has.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// `processor`'s block. Panics on a processor the machine does not
    /// have.
    pub fn get(&self, processor: ProcessorId) -> &T {
        self.try_get(processor).unwrap_or_else(|| {
            panic!(
                "processor {} is outside the {} the per-processor table was built for",
                processor.id(),
                self.blocks.len()
            )
        })
    }

    /// `processor`'s block, or `None` for a processor the machine does
    /// not have.
    pub fn try_get(&self, processor: ProcessorId) -> Option<&T> {
        let block = self.blocks.get(usize::from(processor.id()))?;
        // SAFETY: a block is initialised before the table exists and is
        // dropped only with it.
        Some(unsafe { block.as_ref() })
    }

    /// Every processor's block, in processor order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (ProcessorId, &T)> + Clone {
        self.blocks.iter().enumerate().map(|(index, block)| {
            // SAFETY: as in `try_get`.
            let value: &T = unsafe { block.as_ref() };
            (ProcessorId::new(index as u16), value)
        })
    }
}

impl<T> Drop for PerProcessor<T> {
    fn drop(&mut self) {
        for block in &mut self.blocks {
            // SAFETY: each block is initialised and dropped once; its
            // memory belongs to the arena and is not returned.
            unsafe { core::ptr::drop_in_place(block.as_ptr()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::test_support::{HostNodeMemory as HostMemory, TestMachine};

    fn arenas_for(machine: &TestMachine) -> (&'static Locality, NodeArenas) {
        let locality = machine.locality();
        (locality, NodeArenas::new(locality.node_count()))
    }

    #[test]
    fn pieces_cover_a_tail_with_aligned_power_of_two_blocks() {
        let block = 0x40_0000;
        let pieces: Vec<_> =
            aligned_pieces(block, block + 0x3000, block + 0x10000).collect::<Vec<_>>();
        assert_eq!(
            pieces,
            [
                (block + 0x3000, 0x1000),
                (block + 0x4000, 0x4000),
                (block + 0x8000, 0x8000),
            ]
        );
        for (start, bytes) in pieces {
            assert!((start - block).is_multiple_of(bytes));
        }
    }

    #[test]
    fn an_empty_or_sub_frame_tail_has_no_pieces() {
        assert_eq!(aligned_pieces(0x1000, 0x3000, 0x3000).count(), 0);
        assert_eq!(aligned_pieces(0x1000, 0x3000, 0x3800).count(), 0);
    }

    #[test]
    fn a_block_is_in_the_memory_of_its_processors_node() {
        let machine = TestMachine {
            processor_nodes: alloc::vec![0, 0, 1, 1],
            distances: alloc::vec![10, 20, 20, 10],
        };
        let (locality, arenas) = arenas_for(&machine);
        let memory = HostMemory::new(2, 1 << 20);
        let regions: Vec<_> = memory.regions().to_vec();
        let table = PerProcessor::build(locality, &arenas, &memory, |processor, local| {
            (processor.id(), local.node().index())
        });
        assert_eq!(table.len(), 4);
        for (processor, value) in table.iter() {
            let node = usize::from(processor.id() / 2);
            assert_eq!(*value, (processor.id(), node));
            let address = value as *const _ as usize;
            let (start, end) = regions[node];
            assert!(
                (start..end).contains(&address),
                "processor {} block at {address:#x} is outside node {node}'s memory",
                processor.id()
            );
        }
    }

    #[test]
    fn blocks_do_not_share_a_cache_line() {
        let machine = TestMachine {
            processor_nodes: alloc::vec![0, 0, 0],
            distances: alloc::vec![10],
        };
        let (locality, arenas) = arenas_for(&machine);
        let memory = HostMemory::new(1, 1 << 20);
        let table = PerProcessor::build(locality, &arenas, &memory, |_, _| 0_u8);
        let lines: Vec<_> = table
            .iter()
            .map(|(_, value)| value as *const u8 as usize / align_of::<CachePadded<u8>>())
            .collect();
        assert_eq!(lines.len(), 3);
        assert!(lines.windows(2).all(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn a_spent_chunk_returns_its_tail_and_the_next_carve_lends_again() {
        let machine = TestMachine {
            processor_nodes: alloc::vec![0],
            distances: alloc::vec![10],
        };
        let (_, arenas) = arenas_for(&machine);
        let memory = HostMemory::new(1, 1 << 20);
        let node = NodeId::FIRST;
        let layout = Layout::from_size_align(PhysFrame::SIZE, 64).unwrap();
        for _ in 0..NODE_ARENA_CHUNK_BYTES / PhysFrame::SIZE {
            arenas.arena(node).carve(node, &memory, layout);
        }
        assert_eq!(arenas.lent_bytes(), NODE_ARENA_CHUNK_BYTES);
        assert!(memory.returned().is_empty());
        arenas.arena(node).carve(node, &memory, layout);
        assert_eq!(
            arenas.lent_bytes(),
            2 * NODE_ARENA_CHUNK_BYTES,
            "the spent chunk had no whole frame left to return"
        );
    }

    #[test]
    fn a_large_carve_costs_what_it_uses_and_returns_the_rest_of_its_block() {
        let machine = TestMachine {
            processor_nodes: alloc::vec![0],
            distances: alloc::vec![10],
        };
        let (_, arenas) = arenas_for(&machine);
        let memory = HostMemory::new(1, 1 << 24);
        let node = NodeId::FIRST;
        let bytes = 3 << 20;
        let layout = Layout::from_size_align(bytes, 64).unwrap();
        let ptr = arenas.arena(node).carve(node, &memory, layout);
        assert_eq!(arenas.lent_bytes(), bytes);
        let returned: usize = memory.returned().iter().map(|piece| piece.1).sum();
        assert_eq!(returned, (4 << 20) - bytes);
        let start = ptr.as_ptr() as usize;
        for (piece, size) in memory.returned() {
            assert!(piece >= start + bytes);
            assert!((piece - start).is_multiple_of(size));
        }
    }

    #[test]
    fn a_node_without_memory_of_its_own_borrows_from_the_lender() {
        // The lender decides which node serves; the arena only asks in
        // the processor's node's name.
        let machine = TestMachine {
            processor_nodes: alloc::vec![0, 1],
            distances: alloc::vec![10, 20, 20, 10],
        };
        let (locality, arenas) = arenas_for(&machine);
        let memory = HostMemory::new(2, 1 << 20);
        let table = PerProcessor::build(locality, &arenas, &memory, |_, local| local.node());
        assert_eq!(table.get(ProcessorId::new(1)).index(), 1);
        assert!(arenas.lent_bytes() >= 2 * NODE_ARENA_CHUNK_BYTES);
    }

    #[test]
    fn a_slice_is_built_in_place_and_dropped_with_its_owner() {
        use alloc::rc::Rc;
        let machine = TestMachine {
            processor_nodes: alloc::vec![0],
            distances: alloc::vec![10],
        };
        let (locality, arenas) = arenas_for(&machine);
        let memory = HostMemory::new(1, 1 << 20);
        let witness = Rc::new(());
        let table = PerProcessor::build(locality, &arenas, &memory, |_, local| {
            local.carve_slice(3, |_| witness.clone())
        });
        assert_eq!(Rc::strong_count(&witness), 4);
        assert_eq!(table.get(ProcessorId::new(0)).len(), 3);
        drop(table);
        assert_eq!(Rc::strong_count(&witness), 1);
    }

    #[test]
    fn zeroed_carves_are_zero() {
        let machine = TestMachine {
            processor_nodes: alloc::vec![0],
            distances: alloc::vec![10],
        };
        let (locality, arenas) = arenas_for(&machine);
        let memory = HostMemory::new(1, 1 << 20);
        let table = PerProcessor::build(locality, &arenas, &memory, |_, local| {
            let bytes = local.carve_zeroed(Layout::from_size_align(256, 16).unwrap());
            unsafe { core::slice::from_raw_parts(bytes.as_ptr(), 256) }
                .iter()
                .all(|byte| *byte == 0)
        });
        assert!(*table.get(ProcessorId::new(0)));
    }
}
