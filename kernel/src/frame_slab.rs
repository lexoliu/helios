//! Fixed-size physical-frame slab cache.
//!
//! This is the kernel-side fast path for one-frame allocations. The buddy
//! allocator remains the source of contiguous ranges; this cache keeps recently
//! freed single frames in a compact slab-style freelist and drains them back to
//! buddy storage when a larger allocation needs contiguity.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use helios_hal::cpu::ProcessorId;
use helios_hal::pmm::PhysFrame;
use spin::{Mutex, Once};

const SLAB_RETAIN_DIVISOR: usize = 64;

struct FreeFrame {
    next: Option<NonNull<FreeFrame>>,
}

struct FrameSlabShard {
    head: Mutex<Option<NonNull<FreeFrame>>>,
}

pub(crate) struct FrameSlabCache {
    fallback: FrameSlabShard,
    shards: Once<Box<[FrameSlabShard]>>,
    cached_frames: AtomicUsize,
}

unsafe impl Send for FrameSlabShard {}
unsafe impl Sync for FrameSlabShard {}
unsafe impl Send for FrameSlabCache {}
unsafe impl Sync for FrameSlabCache {}

impl FrameSlabShard {
    const fn new() -> Self {
        Self {
            head: Mutex::new(None),
        }
    }

    fn allocate(&self) -> Option<NonNull<u8>> {
        let mut head = self.head.lock();
        let frame = head.take()?;
        let next = unsafe { frame.as_ref().next };
        *head = next;
        Some(frame.cast())
    }

    fn deallocate(&self, frame: NonNull<u8>) {
        let mut head = self.head.lock();
        let mut frame = frame.cast::<FreeFrame>();
        unsafe {
            frame.as_mut().next = *head;
        }
        *head = Some(frame);
    }

    fn drain(&self, cached_frames: &AtomicUsize, mut release: impl FnMut(NonNull<u8>)) {
        let mut head = self.head.lock();
        while let Some(frame) = head.take() {
            let next = unsafe { frame.as_ref().next };
            *head = next;
            cached_frames.fetch_sub(1, Ordering::AcqRel);
            release(frame.cast());
        }
    }
}

impl FrameSlabCache {
    pub(crate) const fn new() -> Self {
        Self {
            fallback: FrameSlabShard::new(),
            shards: Once::new(),
            cached_frames: AtomicUsize::new(0),
        }
    }

    pub(crate) fn configure_processors(&self, processor_count: usize) {
        assert!(
            processor_count != 0,
            "frame slab requires at least one processor"
        );
        self.shards.call_once(|| {
            let mut shards = Vec::with_capacity(processor_count);
            shards.resize_with(processor_count, FrameSlabShard::new);
            shards.into_boxed_slice()
        });
    }

    pub(crate) fn allocate(&self) -> Option<NonNull<u8>> {
        self.allocate_from(&self.fallback)
    }

    pub(crate) fn allocate_on(&self, processor: ProcessorId) -> Option<NonNull<u8>> {
        self.allocate_from(self.processor_shard(processor))
    }

    pub(crate) fn deallocate(&self, frame: NonNull<u8>, total_frames: usize) -> bool {
        self.deallocate_to(&self.fallback, frame, total_frames)
    }

    pub(crate) fn deallocate_on(
        &self,
        processor: ProcessorId,
        frame: NonNull<u8>,
        total_frames: usize,
    ) -> bool {
        self.deallocate_to(self.processor_shard(processor), frame, total_frames)
    }

    pub(crate) fn drain(&self, mut release: impl FnMut(NonNull<u8>)) {
        self.fallback.drain(&self.cached_frames, &mut release);
        if let Some(shards) = self.shards.get() {
            for shard in shards.iter() {
                shard.drain(&self.cached_frames, &mut release);
            }
        }
    }

    pub(crate) fn cached_bytes(&self) -> usize {
        self.cached_frames.load(Ordering::Acquire) * PhysFrame::SIZE
    }

    fn allocate_from(&self, shard: &FrameSlabShard) -> Option<NonNull<u8>> {
        let frame = shard.allocate()?;
        self.cached_frames.fetch_sub(1, Ordering::AcqRel);
        Some(frame)
    }

    fn deallocate_to(
        &self,
        shard: &FrameSlabShard,
        frame: NonNull<u8>,
        total_frames: usize,
    ) -> bool {
        let capacity = slab_capacity_frames(total_frames);
        if !self.try_reserve_cached_frame(capacity) {
            return false;
        }
        shard.deallocate(frame);
        true
    }

    fn try_reserve_cached_frame(&self, capacity: usize) -> bool {
        let mut cached = self.cached_frames.load(Ordering::Acquire);
        loop {
            if cached >= capacity {
                return false;
            }
            match self.cached_frames.compare_exchange_weak(
                cached,
                cached + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => cached = observed,
            }
        }
    }

    fn processor_shard(&self, processor: ProcessorId) -> &FrameSlabShard {
        let shards = self
            .shards
            .get()
            .unwrap_or_else(|| panic!("per-processor frame slab used before configuration"));
        shards.get(processor.id() as usize).unwrap_or_else(|| {
            panic!(
                "processor {} is outside configured frame slab shard count {}",
                processor.id(),
                shards.len()
            )
        })
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
        cache.configure_processors(2);
        let backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let ptr = NonNull::new(backing.0.as_ptr() as *mut u8).expect("aligned frame pointer");

        assert!(cache.deallocate_on(ProcessorId::new(1), ptr, 64));
        assert_eq!(cache.cached_bytes(), PhysFrame::SIZE);
        assert_eq!(cache.allocate_on(ProcessorId::new(1)), Some(ptr));
        assert_eq!(cache.cached_bytes(), 0);
    }

    #[test]
    fn slab_drains_cached_frames() {
        let cache = FrameSlabCache::new();
        cache.configure_processors(2);
        let backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let first = NonNull::new(backing.0.as_ptr() as *mut u8).expect("first frame pointer");
        let second = NonNull::new(unsafe { backing.0.as_ptr().add(PhysFrame::SIZE) } as *mut u8)
            .expect("second frame pointer");

        assert!(cache.deallocate_on(ProcessorId::new(0), first, 128));
        assert!(cache.deallocate_on(ProcessorId::new(1), second, 128));

        let mut drained = 0;
        cache.drain(|_| drained += 1);
        assert_eq!(drained, 2);
        assert_eq!(cache.cached_bytes(), 0);
        assert_eq!(cache.allocate(), None);
    }
}
