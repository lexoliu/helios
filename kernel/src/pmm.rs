//! Kernel-side physical-frame allocator.
//!
//! A slab-fronted adapter over `buddy_system_allocator::LockedHeap<32>`
//! that exposes the [`hal::pmm::PhysFrameAllocator`] trait. Single-frame
//! allocations use a fixed-size frame slab before falling back to the
//! buddy heap; contiguous ranges still come from the buddy allocator.
//!
//! # Concurrency
//!
//! Single-frame reuse is serialized by the slab lock, while contiguous
//! range allocation is serialized by the buddy heap. The slab is drained
//! before retrying a failed contiguous allocation so cached frames do not
//! harm large-range availability.

extern crate alloc;

use core::alloc::Layout;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use buddy_system_allocator::LockedHeap;
use helios_hal::pmm::{
    FrameAllocError, FrameAllocStats, PhysFrame, PhysFrameAllocator, PhysFrameRange,
};

use crate::frame_slab::FrameSlabCache;

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
}

impl KernelPhysFrameAllocator {
    pub const fn new() -> Self {
        Self {
            heap: LockedHeap::empty(),
            slab: FrameSlabCache::new(),
            added_bytes: AtomicUsize::new(0),
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
        self.added_bytes.fetch_add(end - start, Ordering::Release);
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
        if count == 0 {
            return Err(FrameAllocError::OutOfFrames {
                requested: 0,
                available: 0,
            });
        }
        if count == 1 {
            if let Some(ptr) = self.slab.allocate() {
                if zero_first_use {
                    unsafe {
                        core::ptr::write_bytes(ptr.as_ptr(), 0, PhysFrame::SIZE);
                    }
                }
                return Ok(PhysFrameRange::from_phys_addr(
                    ptr.as_ptr() as usize,
                    PhysFrame::SIZE,
                ));
            }
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
        if zero_first_use {
            unsafe {
                core::ptr::write_bytes(ptr.as_ptr(), 0, bytes);
            }
        }
        Ok(PhysFrameRange::from_phys_addr(ptr.as_ptr() as usize, bytes))
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
        }
    }
}

fn single_frame_layout() -> Layout {
    Layout::from_size_align(PhysFrame::SIZE, PhysFrame::SIZE)
        .unwrap_or_else(|_| panic!("invalid single-frame layout"))
}
