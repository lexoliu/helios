//! Kernel-side physical-frame allocator.
//!
//! A slab-fronted adapter over `buddy_system_allocator::LockedHeap<32>`
//! that exposes the [`hal::pmm::PhysFrameAllocator`] trait. Single-frame
//! allocations use a fixed-size frame slab before falling back to the
//! buddy heap; contiguous ranges still come from the buddy allocator.
//!
//! # Concurrency
//!
//! Single-frame reuse uses lock-free per-processor slab shards, while
//! contiguous range allocation is serialized by the buddy heap. The slab is
//! drained before retrying a failed contiguous allocation so cached frames do
//! not harm large-range availability.

extern crate alloc;

use core::alloc::Layout;
use core::future::Future;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use buddy_system_allocator::LockedHeap;
use helios_hal::pmm::{
    FrameAllocError, FrameAllocStats, PhysFrame, PhysFrameAllocator, PhysFrameRange,
};

use crate::memory::frame_slab::FrameSlabCache;
use crate::memory::reported::{ReportedFrames, visit_free_runs};

const HEAP_ORDER: usize = 32;

/// Kernel physical-frame allocator. Constructed empty and grown by
/// [`KernelPhysFrameAllocator::add_region`] during boot.
pub struct KernelPhysFrameAllocator {
    heap: LockedHeap<HEAP_ORDER>,
    slab: FrameSlabCache,
    /// Total bytes published into the buddy heap via `add_region`.
    /// `LockedHeap` does not expose a `largest_free_run` helper, so
    /// `total_added_bytes - currently_allocated` is the closest cheap
    /// approximation we can give the OOM policy.
    added_bytes: AtomicUsize,
    /// Frames shown to a free-page consumer, whose contents the
    /// allocator can no longer vouch for.
    reported: ReportedFrames,
}

impl KernelPhysFrameAllocator {
    pub const fn new() -> Self {
        Self {
            heap: LockedHeap::empty(),
            slab: FrameSlabCache::new(),
            added_bytes: AtomicUsize::new(0),
            reported: ReportedFrames::new(),
        }
    }

    /// Publish a `[start, end)` byte range into the buddy heap. Boot
    /// code calls this once per `MemoryRegion` carved off for user
    /// memory. The range must be page-aligned at both ends.
    pub fn add_region(&self, start: usize, end: usize) {
        if end <= start {
            return;
        }
        assert!(
            start.is_multiple_of(PhysFrame::SIZE),
            "frame-allocator region start {start:#x} is not page-aligned"
        );
        assert!(
            end.is_multiple_of(PhysFrame::SIZE),
            "frame-allocator region end {end:#x} is not page-aligned"
        );
        unsafe {
            self.heap.lock().add_to_heap(start, end);
        }
        self.reported.cover(start, end);
        self.added_bytes.fetch_add(end - start, Ordering::Release);
    }

    /// Takes `count` contiguous frames without consulting the reported
    /// bitmap.
    ///
    /// This is the raw pool path both the public allocation — which adds
    /// the reported-frame zeroing on top — and free-page reporting —
    /// which must not pay for that zeroing on memory it is about to hand
    /// straight back — are built from.
    fn allocate_frames(&self, count: usize) -> Result<PhysFrameRange, FrameAllocError> {
        if count == 0 {
            return Err(FrameAllocError::OutOfFrames {
                requested: 0,
                available: 0,
            });
        }
        if count == 1
            && let Some(ptr) = self.slab.allocate()
        {
            return Ok(PhysFrameRange::from_phys_addr(
                ptr.as_ptr() as usize,
                PhysFrame::SIZE,
            ));
        }

        let bytes = count * PhysFrame::SIZE;
        let layout = Layout::from_size_align(bytes, PhysFrame::SIZE)
            .unwrap_or_else(|_| panic!("frame-allocator layout overflow for {bytes} bytes"));
        let mut allocator = self.heap.lock();
        let ptr = allocator
            .alloc(layout)
            .or_else(|_| {
                self.slab.drain(|ptr| unsafe {
                    allocator.dealloc(ptr, single_frame_layout());
                });
                allocator.alloc(layout)
            })
            .map_err(|_| {
                let total = allocator.stats_total_bytes();
                let used = allocator.stats_alloc_actual();
                let cached = self.slab.cached_bytes();
                FrameAllocError::OutOfFrames {
                    requested: count,
                    available: total.saturating_sub(used).saturating_add(cached) / PhysFrame::SIZE,
                }
            })?;
        Ok(PhysFrameRange::from_phys_addr(ptr.as_ptr() as usize, bytes))
    }
}

impl Default for KernelPhysFrameAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl PhysFrameAllocator for KernelPhysFrameAllocator {
    fn allocate(
        &self,
        count: usize,
        zero_first_use: bool,
    ) -> Result<PhysFrameRange, FrameAllocError> {
        let range = self.allocate_frames(count)?;
        // A run that was shown to a free-page consumer may have been
        // discarded by it, so the caller's opt-out does not apply to it.
        let clobbered = self.reported.take(range);
        if zero_first_use || clobbered {
            unsafe {
                core::ptr::write_bytes(range.start.phys_addr() as *mut u8, 0, range.byte_size());
            }
        }
        Ok(range)
    }

    fn free_runs<Visit, Visited>(
        &self,
        min_frames: usize,
        max_runs: usize,
        visit: Visit,
    ) -> impl Future<Output = usize> + Send
    where
        Visit: FnMut(PhysFrameRange) -> Visited + Send,
        Visited: Future<Output = ()> + Send,
    {
        visit_free_runs(
            &self.reported,
            min_frames,
            max_runs,
            |frames| self.allocate_frames(frames).ok(),
            |range| self.deallocate(range),
            visit,
        )
    }

    fn deallocate(&self, range: PhysFrameRange) {
        if range.is_empty() {
            return;
        }
        let bytes = range.byte_size();
        let layout = Layout::from_size_align(bytes, PhysFrame::SIZE)
            .unwrap_or_else(|_| panic!("frame-allocator layout overflow for {bytes} bytes"));
        let ptr = NonNull::new(range.start.phys_addr() as *mut u8)
            .unwrap_or_else(|| panic!("frame deallocator received null pointer"));
        if range.frame_count == 1 {
            let total_frames = self.added_bytes.load(Ordering::Acquire) / PhysFrame::SIZE;
            if self.slab.deallocate(ptr, total_frames) {
                return;
            }
        }
        unsafe {
            self.heap.lock().dealloc(ptr, layout);
        }
    }

    fn stats(&self) -> FrameAllocStats {
        let allocator = self.heap.lock();
        let total = allocator.stats_total_bytes();
        let cached = self.slab.cached_bytes();
        let used = allocator.stats_alloc_actual().saturating_sub(cached);
        let free_bytes = total.saturating_sub(used);
        FrameAllocStats {
            total_frames: total / PhysFrame::SIZE,
            allocated_frames: used / PhysFrame::SIZE,
            // `LockedHeap` does not surface a true largest-free-run; we
            // report total free as the upper bound. Pressure-monitor
            // policy uses this as a coarse signal — a real per-order
            // walk can replace this once a hot consumer exists.
            largest_free_run: free_bytes / PhysFrame::SIZE,
            reported_frames: self.reported.count(),
        }
    }
}

fn single_frame_layout() -> Layout {
    Layout::from_size_align(PhysFrame::SIZE, PhysFrame::SIZE)
        .unwrap_or_else(|_| panic!("invalid single-frame layout"))
}

#[cfg(test)]
mod tests {
    use super::{KernelPhysFrameAllocator, Layout};
    use futures_lite::future::block_on;
    use helios_hal::pmm::{PhysFrame, PhysFrameAllocator, PhysFrameRange};

    /// A page-aligned pool of host memory, leaked for the lifetime of
    /// the test process so the allocator may hand pieces of it out.
    fn pool(bytes: usize) -> KernelPhysFrameAllocator {
        let layout = Layout::from_size_align(bytes, PhysFrame::SIZE).expect("pool layout");
        let start = unsafe { alloc::alloc::alloc(layout) } as usize;
        assert!(start != 0, "host allocation for the frame pool failed");
        let allocator = KernelPhysFrameAllocator::new();
        allocator.add_region(start, start + bytes);
        allocator
    }

    /// Reads the last byte of `range`.
    ///
    /// The buddy allocator threads its free list through the head of
    /// every free block, so the first bytes of a released run belong to
    /// the allocator rather than to whoever wrote them last; the tail
    /// is what a caller's fill is still visible in.
    fn tail_byte(range: PhysFrameRange) -> u8 {
        let tail = range.start.phys_addr() + range.byte_size() - 1;
        unsafe { core::ptr::read_volatile(tail as *const u8) }
    }

    fn fill(range: PhysFrameRange, value: u8) {
        unsafe {
            core::ptr::write_bytes(range.start.phys_addr() as *mut u8, value, range.byte_size());
        }
    }

    /// Reporting hands runs out and takes them straight back: a pass
    /// that left them allocated would shrink the pool every time it ran.
    #[test]
    fn a_reporting_pass_returns_every_run_it_visited() {
        let allocator = pool(16 * 1024 * 1024);
        let free_before = allocator.stats().free_frames();

        let mut visited = alloc::vec::Vec::new();
        let runs = block_on(allocator.free_runs(512, 4, |range| {
            visited.push(range);
            core::future::ready(())
        }));

        assert_eq!(runs, 4);
        assert_eq!(visited.len(), 4);
        assert!(
            visited.iter().all(|range| range.frame_count >= 512),
            "a run shorter than the caller asked for is not reportable"
        );
        assert_eq!(
            allocator.stats().free_frames(),
            free_before,
            "the pool is whole again once the pass ends"
        );
        assert_eq!(allocator.stats().reported_frames, 4 * 512);
    }

    /// The host is entitled to throw a reported run away, so the next
    /// caller gets zeroes whether or not it asked for them.
    #[test]
    fn a_reported_run_is_zeroed_even_when_the_caller_opted_out() {
        let allocator = pool(4 * 1024 * 1024);

        // Writing through the visit is what a report cannot prevent: the
        // memory is still the guest's while the consumer looks at it.
        let mut reported = None;
        assert_eq!(
            block_on(allocator.free_runs(8, 1, |run| {
                fill(run, 0xa5);
                reported = Some(run);
                core::future::ready(())
            })),
            1
        );
        let reported = reported.expect("one run was reported");
        assert_eq!(allocator.stats().reported_frames, reported.frame_count);

        let reused = allocator.allocate(8, false).expect("8 frames");
        assert_eq!(reused, reported, "the pool hands the same run back");
        assert_eq!(
            tail_byte(reused),
            0,
            "memory shown to a free-page consumer cannot be handed back unzeroed"
        );
        assert_eq!(
            allocator.stats().reported_frames,
            0,
            "handing a reported run out clears its mark"
        );
    }

    /// Reporting must not pay for zeroing the memory it is about to hand
    /// straight back, so a second pass over the same pool leaves the
    /// contents alone.
    #[test]
    fn a_reporting_pass_does_not_zero_the_memory_it_shows() {
        let allocator = pool(4 * 1024 * 1024);
        let mut seen = alloc::vec::Vec::new();

        block_on(allocator.free_runs(8, 1, |run| {
            fill(run, 0x5a);
            seen.push(run);
            core::future::ready(())
        }));
        block_on(allocator.free_runs(8, 1, |run| {
            seen.push(run);
            core::future::ready(())
        }));

        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0], seen[1],
            "the same free run is reportable until something allocates it"
        );
        assert_eq!(tail_byte(seen[1]), 0x5a);
    }

    /// A pool with nothing large enough left reports nothing rather than
    /// handing back a short run the consumer cannot use.
    #[test]
    fn a_pool_without_a_long_enough_run_reports_nothing() {
        let allocator = pool(1024 * 1024);
        assert_eq!(
            block_on(allocator.free_runs(4096, 4, |_| core::future::ready(()))),
            0
        );
        assert_eq!(allocator.stats().reported_frames, 0);
    }
}
