//! Fixed-size physical-frame slab cache.
//!
//! This is the kernel-side fast path for one-frame allocations. The buddy
//! allocator remains the source of contiguous ranges; this cache keeps recently
//! freed single frames in a compact slab-style freelist and drains them back to
//! buddy storage when a larger allocation needs contiguity.
//!
//! The cache keeps one list per (processor, node): Linux's per-zone
//! `per_cpu_pages`. A processor's lists hold the frames it cached for
//! every node, and a frame always sits on the list of the node its memory
//! is on. A free of a node-1 frame on a node-0 processor pushes that
//! processor's node-1 list, so a foreign free never takes the remote
//! node's buddy lock and never re-labels the frame.
//!
//! # Concurrency contract
//!
//! - **On the owning processor:** pop and push ([`FrameSlabCache::allocate_on`],
//!   [`FrameSlabCache::deallocate_on`]) and the page-fault path's try-pop
//!   ([`FrameSlabCache::try_allocate_on`]). Callers name the executing
//!   processor; every list operation takes the list's own lock, so naming
//!   another processor is a placement mistake, never a race.
//! - **From any processor:** [`FrameSlabCache::drain`] takes each
//!   processor's list for one node briefly under its lock, and
//!   [`FrameSlabCache::cached_bytes`] reads the lists' counters without it.
//! - Each list's lock is an [`IrqSafeMutex`], which masks the local
//!   processor's interrupts while held, because interrupt handlers
//!   allocate. Drain callbacks run after the list is unlocked.
//! - Each list is [`CachePadded`]. Its lock and counter are written by its
//!   owning processor, and by a drainer under the lock.

extern crate alloc;

use alloc::boxed::Box;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::ProcessorId;
use helios_hal::numa::NodeId;
use helios_hal::pmm::PhysFrame;

use super::IrqSafeMutex;

/// A list retains at most this fraction of its node's frames, shared out
/// between the processors.
const SLAB_RETAIN_DIVISOR: usize = 64;

struct FreeFrame {
    next: *mut FreeFrame,
}

/// One processor's cached frames of one node.
struct FrameList {
    head: IrqSafeMutex<*mut FreeFrame>,
    cached_frames: AtomicUsize,
}

pub(crate) struct FrameSlabCache {
    processor_count: usize,
    node_count: usize,
    /// Index: `processor * node_count + node`.
    lists: Box<[CachePadded<FrameList>]>,
}

unsafe impl Send for FrameList {}
unsafe impl Sync for FrameList {}

impl FrameList {
    const fn new() -> Self {
        Self {
            head: IrqSafeMutex::new(core::ptr::null_mut()),
            cached_frames: AtomicUsize::new(0),
        }
    }

    fn allocate(&self) -> Option<NonNull<u8>> {
        self.allocate_observed(|| {})
    }

    fn allocate_observed(&self, observe: impl FnOnce()) -> Option<NonNull<u8>> {
        self.head.with(|head| {
            let frame = NonNull::new(*head)?;
            let next = unsafe { frame.as_ref().next };
            observe();
            *head = next;
            let cached = self.cached_frames.load(Ordering::Relaxed);
            self.cached_frames.store(cached - 1, Ordering::Release);
            Some(frame.cast())
        })
    }

    /// Pops a cached frame when the list's lock is free, and answers
    /// `None` when it is not.
    ///
    /// A list's lock is taken by its own processor on the ordinary path
    /// and by any processor during [`FrameSlabCache::drain`], so a
    /// page-fault handler cannot afford to wait on it.
    fn try_allocate(&self) -> Option<NonNull<u8>> {
        self.head.try_with(|head| {
            let frame = NonNull::new(*head)?;
            // SAFETY: every frame on this list was pushed by
            // `deallocate`, which wrote its link word.
            let next = unsafe { frame.as_ref().next };
            *head = next;
            let cached = self.cached_frames.load(Ordering::Relaxed);
            self.cached_frames.store(cached - 1, Ordering::Release);
            Some(frame.cast())
        })?
    }

    fn deallocate(&self, frame: NonNull<u8>, capacity: usize) -> bool {
        self.head.with(|head| {
            let cached = self.cached_frames.load(Ordering::Relaxed);
            if cached >= capacity {
                return false;
            }
            let mut frame = frame.cast::<FreeFrame>();
            unsafe { frame.as_mut().next = *head };
            *head = frame.as_ptr();
            self.cached_frames.store(cached + 1, Ordering::Release);
            true
        })
    }

    fn drain(&self, mut release: impl FnMut(NonNull<u8>)) {
        let mut head = self.head.with(|head| {
            self.cached_frames.store(0, Ordering::Release);
            core::mem::replace(head, core::ptr::null_mut())
        });
        while let Some(frame) = NonNull::new(head) {
            head = unsafe { frame.as_ref().next };
            release(frame.cast());
        }
    }

    fn cached_frames(&self) -> usize {
        self.cached_frames.load(Ordering::Acquire)
    }
}

impl FrameSlabCache {
    pub(crate) fn new(processor_count: usize, node_count: usize) -> Self {
        assert!(
            processor_count != 0,
            "frame slab requires at least one processor"
        );
        assert!(node_count != 0, "frame slab requires at least one node");
        let lists = (0..processor_count * node_count)
            .map(|_| CachePadded::new(FrameList::new()))
            .collect();
        Self {
            processor_count,
            node_count,
            lists,
        }
    }

    /// Pops `processor`'s cached frame of `node`.
    pub(crate) fn allocate_on(&self, processor: ProcessorId, node: NodeId) -> Option<NonNull<u8>> {
        self.list(processor, node).allocate()
    }

    /// See [`FrameList::try_allocate`].
    pub(crate) fn try_allocate_on(
        &self,
        processor: ProcessorId,
        node: NodeId,
    ) -> Option<NonNull<u8>> {
        self.list(processor, node).try_allocate()
    }

    /// Caches `frame`, which lies on `node`, on `processor`'s list for
    /// that node, unless the list already holds its share of the node's
    /// `node_total_frames`. Answers whether the frame was taken.
    pub(crate) fn deallocate_on(
        &self,
        processor: ProcessorId,
        node: NodeId,
        frame: NonNull<u8>,
        node_total_frames: usize,
    ) -> bool {
        self.list(processor, node)
            .deallocate(frame, self.list_capacity_frames(node_total_frames))
    }

    /// Empties every processor's list for `node` into `release`.
    pub(crate) fn drain(&self, node: NodeId, mut release: impl FnMut(NonNull<u8>)) {
        for processor in 0..self.processor_count {
            self.lists[self.index(processor, node)].drain(&mut release);
        }
    }

    /// Bytes of `node` cached across every processor.
    pub(crate) fn cached_bytes(&self, node: NodeId) -> usize {
        (0..self.processor_count)
            .map(|processor| self.lists[self.index(processor, node)].cached_frames())
            .sum::<usize>()
            * PhysFrame::SIZE
    }

    /// The node's retain share, divided between the processors that may
    /// each cache its frames.
    fn list_capacity_frames(&self, node_total_frames: usize) -> usize {
        node_total_frames
            .div_ceil(SLAB_RETAIN_DIVISOR)
            .div_ceil(self.processor_count)
            .max(1)
    }

    fn list(&self, processor: ProcessorId, node: NodeId) -> &FrameList {
        let processor = usize::from(processor.id());
        assert!(
            processor < self.processor_count,
            "processor {processor} is outside the frame slab's {} processors",
            self.processor_count
        );
        &self.lists[self.index(processor, node)]
    }

    fn index(&self, processor: usize, node: NodeId) -> usize {
        assert!(
            node.index() < self.node_count,
            "node {} is outside the frame slab's {} nodes",
            node.index(),
            self.node_count
        );
        processor * self.node_count + node.index()
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use core::ptr::NonNull;

    use super::*;

    #[repr(align(4096))]
    struct AlignedFrames([u8; PhysFrame::SIZE * 2]);

    const NODE_0: NodeId = NodeId::FIRST;

    fn p(index: u16) -> ProcessorId {
        ProcessorId::new(index)
    }

    #[test]
    fn a_delayed_pop_cannot_publish_a_frame_owned_by_another_caller() {
        let mut backing = Box::new([
            AlignedFrames([0; PhysFrame::SIZE * 2]),
            AlignedFrames([0; PhysFrame::SIZE * 2]),
        ]);
        let cache = FrameSlabCache::new(1, 1);
        let total_frames = 3 * SLAB_RETAIN_DIVISOR;
        let base = backing.as_mut_ptr().cast::<u8>();
        for index in 0..3 {
            let frame =
                NonNull::new(unsafe { base.add(index * PhysFrame::SIZE) }).expect("backing frame");
            assert!(cache.deallocate_on(p(0), NODE_0, frame, total_frames));
        }
        std::thread::scope(|scope| {
            let (start_tx, start_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let cache_ref = &cache;
            let other = scope.spawn(move || {
                start_rx.recv().expect("start competing pops");
                let recycled = cache_ref
                    .allocate_on(p(0), NODE_0)
                    .expect("first competing frame");
                let held = cache_ref
                    .allocate_on(p(0), NODE_0)
                    .expect("second competing frame");
                assert!(cache_ref.deallocate_on(p(0), NODE_0, recycled, total_frames));
                done_tx.send(()).expect("competing pops finished");
                held.as_ptr() as usize
            });
            let claimed = cache
                .list(p(0), NODE_0)
                .allocate_observed(|| {
                    start_tx.send(()).expect("start competing pops");
                    match done_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(error) => panic!("competing pops disconnected: {error}"),
                    }
                })
                .expect("delayed claim");
            let held = other.join().expect("competing thread");
            let next = cache.allocate_on(p(0), NODE_0).expect("remaining frame");
            assert_ne!(claimed.as_ptr() as usize, held);
            assert_ne!(
                next.as_ptr() as usize,
                held,
                "a live frame reappeared in the cache"
            );
            assert_ne!(claimed, next);
            assert_eq!(cache.cached_bytes(NODE_0), 0);
        });
    }

    #[test]
    fn concurrent_pops_and_drains_preserve_exclusive_frame_ownership() {
        let mut backing = Box::new(std::array::from_fn::<_, 4, _>(|_| {
            AlignedFrames([0; PhysFrame::SIZE * 2])
        }));
        let cache = FrameSlabCache::new(3, 1);
        let base = backing.as_mut_ptr().cast::<u8>();
        let base_address = base as usize;
        let owners = std::array::from_fn::<_, 8, _>(|_| core::sync::atomic::AtomicBool::new(false));
        let total_frames = owners.len() * SLAB_RETAIN_DIVISOR * 3;
        let return_frame = |frame: NonNull<u8>| {
            let index = (frame.as_ptr() as usize - base_address) / PhysFrame::SIZE;
            let accepted = cache.deallocate_on(p((index % 3) as u16), NODE_0, frame, total_frames);
            assert!(accepted, "every test frame fits within its list quota");
        };
        let use_frame = |frame: NonNull<u8>| {
            let index = (frame.as_ptr() as usize - base_address) / PhysFrame::SIZE;
            assert!(
                !owners[index].swap(true, Ordering::AcqRel),
                "frame has two owners"
            );
            unsafe { frame.as_ptr().write_bytes(index as u8, PhysFrame::SIZE) };
            std::thread::yield_now();
            let bytes = unsafe { core::slice::from_raw_parts(frame.as_ptr(), PhysFrame::SIZE) };
            assert!(bytes.iter().all(|&byte| byte == index as u8));
            owners[index].store(false, Ordering::Release);
            return_frame(frame);
        };
        for index in 0..owners.len() {
            return_frame(
                NonNull::new(unsafe { base.add(index * PhysFrame::SIZE) }).expect("frame"),
            );
        }
        std::thread::scope(|scope| {
            for processor in 0..3 {
                let cache = &cache;
                let use_frame = &use_frame;
                scope.spawn(move || {
                    for _ in 0..250 {
                        if let Some(frame) = cache.allocate_on(p(processor), NODE_0) {
                            use_frame(frame);
                        }
                    }
                });
            }
            scope.spawn(|| {
                for _ in 0..250 {
                    cache.drain(NODE_0, use_frame);
                }
            });
        });
        let mut returned = Vec::new();
        cache.drain(NODE_0, |frame| returned.push(frame.as_ptr() as usize));
        assert_eq!(returned.len(), owners.len());
        returned.sort_unstable();
        returned.dedup();
        assert_eq!(returned.len(), owners.len());
        assert_eq!(cache.cached_bytes(NODE_0), 0);
        assert!(owners.iter().all(|owner| !owner.load(Ordering::Acquire)));
    }

    #[test]
    fn slab_reuses_single_frame() {
        let cache = FrameSlabCache::new(2, 1);
        let mut backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let ptr = NonNull::new(backing.0.as_mut_ptr()).expect("aligned frame pointer");

        assert!(cache.deallocate_on(p(1), NODE_0, ptr, 64));
        assert_eq!(cache.cached_bytes(NODE_0), PhysFrame::SIZE);
        assert_eq!(cache.allocate_on(p(1), NODE_0), Some(ptr));
        assert_eq!(cache.cached_bytes(NODE_0), 0);
    }

    #[test]
    fn slab_drains_cached_frames() {
        let cache = FrameSlabCache::new(2, 1);
        let mut backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let first = NonNull::new(backing.0.as_mut_ptr()).expect("first frame pointer");
        let second = NonNull::new(unsafe { backing.0.as_mut_ptr().add(PhysFrame::SIZE) })
            .expect("second frame pointer");

        assert!(cache.deallocate_on(p(0), NODE_0, first, 128));
        assert!(cache.deallocate_on(p(1), NODE_0, second, 128));

        let mut drained = 0;
        cache.drain(NODE_0, |_| drained += 1);
        assert_eq!(drained, 2);
        assert_eq!(cache.cached_bytes(NODE_0), 0);
        assert_eq!(cache.allocate_on(p(0), NODE_0), None);
        assert_eq!(cache.allocate_on(p(1), NODE_0), None);
    }

    #[test]
    fn slab_drain_releases_after_unlocking_list() {
        let cache = FrameSlabCache::new(1, 1);
        let mut backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let first = NonNull::new(backing.0.as_mut_ptr()).expect("first frame pointer");
        let second = NonNull::new(unsafe { backing.0.as_mut_ptr().add(PhysFrame::SIZE) })
            .expect("second frame pointer");

        assert!(cache.deallocate_on(p(0), NODE_0, first, 128));

        let mut drained = 0;
        cache.drain(NODE_0, |_| {
            drained += 1;
            assert!(cache.deallocate_on(p(0), NODE_0, second, 128));
        });

        assert_eq!(drained, 1);
        assert_eq!(cache.cached_bytes(NODE_0), PhysFrame::SIZE);
        assert_eq!(cache.allocate_on(p(0), NODE_0), Some(second));
    }

    #[test]
    fn slab_capacity_is_list_local() {
        let cache = FrameSlabCache::new(2, 1);
        let mut backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let first = NonNull::new(backing.0.as_mut_ptr()).expect("first frame pointer");
        let second = NonNull::new(unsafe { backing.0.as_mut_ptr().add(PhysFrame::SIZE) })
            .expect("second frame pointer");

        // 128 frames give each of two processors one frame of room.
        assert!(cache.deallocate_on(p(0), NODE_0, first, 128));
        assert!(cache.deallocate_on(p(1), NODE_0, second, 128));
        assert_eq!(cache.cached_bytes(NODE_0), PhysFrame::SIZE * 2);
        assert_eq!(cache.allocate_on(p(0), NODE_0), Some(first));
        assert_eq!(cache.allocate_on(p(1), NODE_0), Some(second));
    }

    /// A frame of node 1 freed by a processor is cached on that
    /// processor's node-1 list: draining node 0 leaves it, popping node 0
    /// never hands it out, and draining node 1 returns it.
    #[test]
    fn a_frame_stays_on_the_list_of_its_own_node() {
        let node_1 = NodeId::new(1);
        let cache = FrameSlabCache::new(2, 2);
        let mut backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let frame = NonNull::new(backing.0.as_mut_ptr()).expect("aligned frame pointer");

        assert!(cache.deallocate_on(p(0), node_1, frame, 128));
        assert_eq!(cache.cached_bytes(NODE_0), 0);
        assert_eq!(cache.cached_bytes(node_1), PhysFrame::SIZE);
        assert_eq!(cache.allocate_on(p(0), NODE_0), None);

        cache.drain(NODE_0, |_| panic!("node 0's lists hold nothing"));
        let mut drained = Vec::new();
        cache.drain(node_1, |frame| drained.push(frame));
        assert_eq!(drained, [frame]);
        assert_eq!(cache.cached_bytes(node_1), 0);
    }

    /// Each list's share is its own node's frames spread over every
    /// processor; a node with few frames still lets each list hold one.
    #[test]
    fn a_list_holds_its_share_of_its_own_node() {
        let cache = FrameSlabCache::new(4, 2);
        assert_eq!(cache.list_capacity_frames(64 * 1024), 256);
        assert_eq!(cache.list_capacity_frames(64), 1);
    }
}
