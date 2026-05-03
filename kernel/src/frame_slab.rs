//! Fixed-size physical-frame slab cache.
//!
//! This is the kernel-side fast path for one-frame allocations. The buddy
//! allocator remains the source of contiguous ranges; this cache keeps recently
//! freed single frames in a compact slab-style freelist and drains them back to
//! buddy storage when a larger allocation needs contiguity.

extern crate alloc;

use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use helios_hal::pmm::PhysFrame;
use spin::Mutex;

const SLAB_RETAIN_DIVISOR: usize = 64;

struct FreeFrame {
    next: Option<NonNull<FreeFrame>>,
}

pub(crate) struct FrameSlabCache {
    head: Mutex<Option<NonNull<FreeFrame>>>,
    cached_frames: AtomicUsize,
}

unsafe impl Send for FrameSlabCache {}
unsafe impl Sync for FrameSlabCache {}

impl FrameSlabCache {
    pub(crate) const fn new() -> Self {
        Self {
            head: Mutex::new(None),
            cached_frames: AtomicUsize::new(0),
        }
    }

    pub(crate) fn allocate(&self) -> Option<NonNull<u8>> {
        let mut head = self.head.lock();
        let frame = head.take()?;
        let next = unsafe { frame.as_ref().next };
        *head = next;
        self.cached_frames.fetch_sub(1, Ordering::AcqRel);
        Some(frame.cast())
    }

    pub(crate) fn deallocate(&self, frame: NonNull<u8>, total_frames: usize) -> bool {
        let capacity = slab_capacity_frames(total_frames);
        let mut head = self.head.lock();
        if self.cached_frames.load(Ordering::Acquire) >= capacity {
            return false;
        }
        let mut frame = frame.cast::<FreeFrame>();
        unsafe {
            frame.as_mut().next = *head;
        }
        *head = Some(frame);
        self.cached_frames.fetch_add(1, Ordering::AcqRel);
        true
    }

    pub(crate) fn drain(&self, mut release: impl FnMut(NonNull<u8>)) {
        let mut head = self.head.lock();
        while let Some(frame) = head.take() {
            let next = unsafe { frame.as_ref().next };
            *head = next;
            self.cached_frames.fetch_sub(1, Ordering::AcqRel);
            release(frame.cast());
        }
    }

    pub(crate) fn cached_bytes(&self) -> usize {
        self.cached_frames.load(Ordering::Acquire) * PhysFrame::SIZE
    }
}

fn slab_capacity_frames(total_frames: usize) -> usize {
    total_frames.div_ceil(SLAB_RETAIN_DIVISOR)
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use core::ptr::NonNull;

    use super::*;

    #[repr(align(4096))]
    struct AlignedFrames([u8; PhysFrame::SIZE * 2]);

    #[test]
    fn slab_reuses_single_frame() {
        let cache = FrameSlabCache::new();
        let backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let ptr = NonNull::new(backing.0.as_ptr() as *mut u8).expect("aligned frame pointer");

        assert!(cache.deallocate(ptr, 64));
        assert_eq!(cache.cached_bytes(), PhysFrame::SIZE);
        assert_eq!(cache.allocate(), Some(ptr));
        assert_eq!(cache.cached_bytes(), 0);
    }

    #[test]
    fn slab_drains_cached_frames() {
        let cache = FrameSlabCache::new();
        let backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let first = NonNull::new(backing.0.as_ptr() as *mut u8).expect("first frame pointer");
        let second = NonNull::new(unsafe { backing.0.as_ptr().add(PhysFrame::SIZE) } as *mut u8)
            .expect("second frame pointer");

        assert!(cache.deallocate(first, 128));
        assert!(cache.deallocate(second, 128));

        let mut drained = 0;
        cache.drain(|_| drained += 1);
        assert_eq!(drained, 2);
        assert_eq!(cache.cached_bytes(), 0);
        assert_eq!(cache.allocate(), None);
    }
}
