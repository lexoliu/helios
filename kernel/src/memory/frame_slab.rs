//! Fixed-size physical-frame slab cache.
//!
//! This is the kernel-side fast path for one-frame allocations. The buddy
//! allocator remains the source of contiguous ranges; this cache keeps recently
//! freed single frames in a compact slab-style freelist and drains them back to
//! buddy storage when a larger allocation needs contiguity.
//!
//! Each processor shard has an IRQ-safe freelist and cached-frame counter,
//! on separate cache lines from other shards. Pop, push, and snapshot detachment
//! transfer ownership under the shard lock; drain callbacks run after unlocking.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::ProcessorId;
use helios_hal::pmm::PhysFrame;
use spin::Once;

use super::IrqSafeMutex;

const SLAB_RETAIN_DIVISOR: usize = 64;

struct FreeFrame {
    next: *mut FreeFrame,
}

struct FrameSlabShard {
    head: IrqSafeMutex<*mut FreeFrame>,
    cached_frames: AtomicUsize,
}

pub(crate) struct FrameSlabCache {
    fallback: CachePadded<FrameSlabShard>,
    shards: Once<Box<[CachePadded<FrameSlabShard>]>>,
}

unsafe impl Send for FrameSlabShard {}
unsafe impl Sync for FrameSlabShard {}

impl FrameSlabShard {
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
    pub(crate) const fn new() -> Self {
        Self {
            fallback: CachePadded::new(FrameSlabShard::new()),
            shards: Once::new(),
        }
    }

    pub(crate) fn configure_processors(&self, processor_count: usize) {
        assert!(
            processor_count != 0,
            "frame slab requires at least one processor"
        );
        self.shards.call_once(|| {
            let mut shards = Vec::with_capacity(processor_count);
            shards.resize_with(processor_count, || CachePadded::new(FrameSlabShard::new()));
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
        self.fallback.drain(&mut release);
        if let Some(shards) = self.shards.get() {
            for shard in shards.iter() {
                shard.drain(&mut release);
            }
        }
    }

    pub(crate) fn cached_bytes(&self) -> usize {
        let processor_cached = self
            .shards
            .get()
            .map(|shards| {
                shards
                    .iter()
                    .map(|shard| shard.cached_frames())
                    .sum::<usize>()
            })
            .unwrap_or(0);
        (self.fallback.cached_frames() + processor_cached) * PhysFrame::SIZE
    }

    fn allocate_from(&self, shard: &FrameSlabShard) -> Option<NonNull<u8>> {
        shard.allocate()
    }

    fn deallocate_to(
        &self,
        shard: &FrameSlabShard,
        frame: NonNull<u8>,
        total_frames: usize,
    ) -> bool {
        shard.deallocate(frame, self.shard_capacity_frames(total_frames))
    }

    fn shard_capacity_frames(&self, total_frames: usize) -> usize {
        let shard_count = self.shards.get().map_or(1, |shards| shards.len() + 1);
        total_frames
            .div_ceil(SLAB_RETAIN_DIVISOR)
            .div_ceil(shard_count)
            .max(1)
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

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use core::ptr::NonNull;

    use super::*;

    #[repr(align(4096))]
    struct AlignedFrames([u8; PhysFrame::SIZE * 2]);

    #[test]
    fn a_delayed_pop_cannot_publish_a_frame_owned_by_another_caller() {
        let mut backing = Box::new([
            AlignedFrames([0; PhysFrame::SIZE * 2]),
            AlignedFrames([0; PhysFrame::SIZE * 2]),
        ]);
        let cache = FrameSlabCache::new();
        let base = backing.as_mut_ptr().cast::<u8>();
        for index in 0..3 {
            let frame =
                NonNull::new(unsafe { base.add(index * PhysFrame::SIZE) }).expect("backing frame");
            assert!(cache.deallocate(frame, 3 * SLAB_RETAIN_DIVISOR));
        }
        std::thread::scope(|scope| {
            let (start_tx, start_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let cache_ref = &cache;
            let other = scope.spawn(move || {
                start_rx.recv().expect("start competing pops");
                let recycled = cache_ref.allocate().expect("first competing frame");
                let held = cache_ref.allocate().expect("second competing frame");
                assert!(cache_ref.deallocate(recycled, 3 * SLAB_RETAIN_DIVISOR));
                done_tx.send(()).expect("competing pops finished");
                held.as_ptr() as usize
            });
            let claimed = cache
                .fallback
                .allocate_observed(|| {
                    start_tx.send(()).expect("start competing pops");
                    match done_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(error) => panic!("competing pops disconnected: {error}"),
                    }
                })
                .expect("delayed claim");
            let held = other.join().expect("competing thread");
            let next = cache.allocate().expect("remaining frame");
            assert_ne!(claimed.as_ptr() as usize, held);
            assert_ne!(
                next.as_ptr() as usize,
                held,
                "a live frame reappeared in the cache"
            );
            assert_ne!(claimed, next);
            assert_eq!(cache.cached_bytes(), 0);
        });
    }

    #[test]
    fn concurrent_pops_and_drains_preserve_exclusive_frame_ownership() {
        let mut backing = Box::new(std::array::from_fn::<_, 4, _>(|_| {
            AlignedFrames([0; PhysFrame::SIZE * 2])
        }));
        let cache = FrameSlabCache::new();
        cache.configure_processors(2);
        let base = backing.as_mut_ptr().cast::<u8>();
        let base_address = base as usize;
        let owners = std::array::from_fn::<_, 8, _>(|_| core::sync::atomic::AtomicBool::new(false));
        let total_frames = owners.len() * SLAB_RETAIN_DIVISOR * 3;
        let return_frame = |frame: NonNull<u8>| {
            let index = (frame.as_ptr() as usize - base_address) / PhysFrame::SIZE;
            let accepted = match index % 3 {
                0 => cache.deallocate(frame, total_frames),
                shard => {
                    cache.deallocate_on(ProcessorId::new((shard - 1) as u16), frame, total_frames)
                }
            };
            assert!(accepted, "every test frame fits within its shard quota");
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
            for shard in 0..3 {
                let cache = &cache;
                let use_frame = &use_frame;
                scope.spawn(move || {
                    for _ in 0..250 {
                        let frame = match shard {
                            0 => cache.allocate(),
                            shard => cache.allocate_on(ProcessorId::new(shard - 1)),
                        };
                        if let Some(frame) = frame {
                            use_frame(frame);
                        }
                    }
                });
            }
            scope.spawn(|| {
                for _ in 0..250 {
                    cache.drain(use_frame);
                }
            });
        });
        let mut returned = Vec::new();
        cache.drain(|frame| returned.push(frame.as_ptr() as usize));
        assert_eq!(returned.len(), owners.len());
        returned.sort_unstable();
        returned.dedup();
        assert_eq!(returned.len(), owners.len());
        assert_eq!(cache.cached_bytes(), 0);
        assert!(owners.iter().all(|owner| !owner.load(Ordering::Acquire)));
    }

    #[test]
    fn slab_reuses_single_frame() {
        let cache = FrameSlabCache::new();
        cache.configure_processors(2);
        let mut backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let ptr = NonNull::new(backing.0.as_mut_ptr()).expect("aligned frame pointer");

        assert!(cache.deallocate_on(ProcessorId::new(1), ptr, 64));
        assert_eq!(cache.cached_bytes(), PhysFrame::SIZE);
        assert_eq!(cache.allocate_on(ProcessorId::new(1)), Some(ptr));
        assert_eq!(cache.cached_bytes(), 0);
    }

    #[test]
    fn slab_drains_cached_frames() {
        let cache = FrameSlabCache::new();
        cache.configure_processors(2);
        let mut backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let first = NonNull::new(backing.0.as_mut_ptr()).expect("first frame pointer");
        let second = NonNull::new(unsafe { backing.0.as_mut_ptr().add(PhysFrame::SIZE) })
            .expect("second frame pointer");

        assert!(cache.deallocate_on(ProcessorId::new(0), first, 128));
        assert!(cache.deallocate_on(ProcessorId::new(1), second, 128));

        let mut drained = 0;
        cache.drain(|_| drained += 1);
        assert_eq!(drained, 2);
        assert_eq!(cache.cached_bytes(), 0);
        assert_eq!(cache.allocate(), None);
    }

    #[test]
    fn slab_drain_releases_after_unlocking_shard() {
        let cache = FrameSlabCache::new();
        cache.configure_processors(1);
        let mut backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let first = NonNull::new(backing.0.as_mut_ptr()).expect("first frame pointer");
        let second = NonNull::new(unsafe { backing.0.as_mut_ptr().add(PhysFrame::SIZE) })
            .expect("second frame pointer");

        assert!(cache.deallocate_on(ProcessorId::new(0), first, 128));

        let mut drained = 0;
        cache.drain(|_| {
            drained += 1;
            assert!(cache.deallocate_on(ProcessorId::new(0), second, 128));
        });

        assert_eq!(drained, 1);
        assert_eq!(cache.cached_bytes(), PhysFrame::SIZE);
        assert_eq!(cache.allocate_on(ProcessorId::new(0)), Some(second));
    }

    #[test]
    fn slab_capacity_is_shard_local() {
        let cache = FrameSlabCache::new();
        cache.configure_processors(2);
        let mut backing = Box::new(AlignedFrames([0; PhysFrame::SIZE * 2]));
        let first = NonNull::new(backing.0.as_mut_ptr()).expect("first frame pointer");
        let second = NonNull::new(unsafe { backing.0.as_mut_ptr().add(PhysFrame::SIZE) })
            .expect("second frame pointer");

        assert!(cache.deallocate_on(ProcessorId::new(0), first, 64));
        assert!(cache.deallocate_on(ProcessorId::new(1), second, 64));
        assert_eq!(cache.cached_bytes(), PhysFrame::SIZE * 2);
        assert_eq!(cache.allocate_on(ProcessorId::new(0)), Some(first));
        assert_eq!(cache.allocate_on(ProcessorId::new(1)), Some(second));
    }
}
