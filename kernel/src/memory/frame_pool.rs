use core::num::NonZeroUsize;
use core::ptr::{NonNull, with_exposed_provenance_mut};

use arrayvec::ArrayVec;
use frame_alloc::{
    AllocError, PageSize, PhysRange, PhysicalAllocator, Provenance, RegionInit,
    SummaryBuddyAllocator,
};
use helios_hal::pmm::PhysFrame;

use crate::MAX_BOOT_MEMORY_REGIONS;

const FRAME_ORDERS: usize = 32 - PhysFrame::SIZE.ilog2() as usize;
const BASE_FRAME: PageSize = PageSize::from_log2(PhysFrame::SIZE.ilog2() as u8);

struct PoolProvenance;

unsafe impl Provenance for PoolProvenance {
    unsafe fn create(address: usize) -> NonNull<u8> {
        NonNull::new(with_exposed_provenance_mut(address))
            .expect("user frame pool metadata address is null")
    }

    unsafe fn destroy<T>(ptr: NonNull<T>) -> usize {
        ptr.as_ptr().expose_provenance()
    }
}

pub(super) struct FramePool {
    allocator: SummaryBuddyAllocator<FRAME_ORDERS, PoolProvenance>,
    pub(super) total_bytes: usize,
    pub(super) allocated_bytes: usize,
}

impl FramePool {
    pub(super) const fn new() -> Self {
        Self {
            allocator: SummaryBuddyAllocator::new(BASE_FRAME),
            total_bytes: 0,
            allocated_bytes: 0,
        }
    }

    pub(super) fn initialize(&mut self, regions: &[(usize, usize)]) {
        assert_eq!(
            self.total_bytes, 0,
            "user frame pool initialized more than once"
        );
        let mut usable = ArrayVec::<PhysRange, MAX_BOOT_MEMORY_REGIONS>::new();
        let mut total_bytes = 0_usize;
        for &(start, end) in regions {
            let len = end
                .checked_sub(start)
                .expect("user frame region ends before its start");
            total_bytes = total_bytes
                .checked_add(len)
                .expect("user frame capacity overflows");
            usable
                .try_push(PhysRange { base: start, len })
                .unwrap_or_else(|_| {
                    panic!("user frame map exceeds {MAX_BOOT_MEMORY_REGIONS} regions")
                });
        }
        assert!(
            total_bytes >= PhysFrame::SIZE,
            "user frame pool has no usable frames"
        );
        usable.sort_unstable_by_key(|region| region.base);
        let first = usable.first().expect("user frame map is empty").base;
        let last = usable.last().expect("user frame map is empty");
        let span = last
            .base
            .checked_add(last.len)
            .and_then(|end| end.checked_sub(first))
            .expect("user frame span overflows");
        let max_shift = total_bytes
            .ilog2()
            .min(BASE_FRAME.log2() as u32 + FRAME_ORDERS as u32 - 1);
        self.allocator =
            SummaryBuddyAllocator::with_max_page(BASE_FRAME, PageSize::from_log2(max_shift as u8));
        unsafe { self.allocator.try_init(first, span, &usable) }
            .unwrap_or_else(|error| panic!("invalid user frame map: {error:?}"));
        self.total_bytes = total_bytes;
        self.allocated_bytes = self.allocator.reserved_frames() * PhysFrame::SIZE;
    }

    pub(super) fn allocate(&mut self, bytes: usize) -> Result<NonNull<u8>, AllocError> {
        assert!(
            bytes.is_power_of_two() && bytes >= PhysFrame::SIZE,
            "invalid user frame block size {bytes}"
        );
        let page = PageSize::from_log2(bytes.ilog2() as u8);
        let address = self.allocator.allocate_physical(page, NonZeroUsize::MIN)?;
        self.allocated_bytes += bytes;
        Ok(NonNull::new(with_exposed_provenance_mut(address))
            .expect("user frame allocation is null"))
    }

    pub(super) unsafe fn deallocate(&mut self, ptr: NonNull<u8>, bytes: usize) {
        assert!(
            bytes.is_power_of_two() && bytes >= PhysFrame::SIZE,
            "invalid user frame block size {bytes}"
        );
        let page = PageSize::from_log2(bytes.ilog2() as u8);
        unsafe {
            self.allocator.deallocate_physical(
                page,
                NonZeroUsize::MIN,
                ptr.as_ptr().expose_provenance(),
            )
        };
        self.allocated_bytes = self
            .allocated_bytes
            .checked_sub(bytes)
            .expect("user frame accounting underflow");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::alloc::{alloc_zeroed, dealloc};
    use alloc::collections::BTreeSet;
    use alloc::vec::Vec;
    use core::alloc::Layout;
    use frame_alloc::AllocatorStats;

    struct Backing {
        ptr: NonNull<u8>,
        layout: Layout,
    }

    impl Backing {
        fn new(bytes: usize) -> Self {
            let layout = Layout::from_size_align(bytes, bytes).expect("test pool layout");
            let ptr = NonNull::new(unsafe { alloc_zeroed(layout) }).expect("test pool backing");
            Self { ptr, layout }
        }

        fn start(&self) -> usize {
            self.ptr.as_ptr().expose_provenance()
        }

        fn range(&self) -> (usize, usize) {
            (self.start(), self.start() + self.layout.size())
        }
    }

    impl Drop for Backing {
        fn drop(&mut self) {
            unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
        }
    }

    #[test]
    fn metadata_and_holes_never_become_payload_frames() {
        let backing = Backing::new(2 * 1024 * 1024);
        let start = backing.start();
        let hole_start = start + 200 * PhysFrame::SIZE;
        let hole_end = hole_start + 4 * PhysFrame::SIZE;
        let mut pool = FramePool::new();
        pool.initialize(&[(hole_end, backing.range().1), (start, hole_start)]);
        let metadata = pool.allocated_bytes;
        assert!(metadata > 0);
        assert_eq!(
            pool.total_bytes,
            backing.layout.size() - 4 * PhysFrame::SIZE
        );
        let available = pool.total_bytes - metadata;
        let mut frames = Vec::new();
        let mut addresses = BTreeSet::new();
        while let Ok(ptr) = pool.allocate(PhysFrame::SIZE) {
            let address = ptr.as_ptr().expose_provenance();
            assert!(address >= start + metadata && address < backing.range().1);
            assert!(address < hole_start || address >= hole_end);
            assert!(addresses.insert(address), "live frame allocated twice");
            unsafe { ptr.as_ptr().write_bytes(0xa5, PhysFrame::SIZE) };
            frames.push(ptr);
        }
        assert_eq!(frames.len() * PhysFrame::SIZE, available);
        assert_eq!(pool.allocated_bytes, pool.total_bytes);
        for ptr in &frames {
            let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), PhysFrame::SIZE) };
            assert!(bytes.iter().all(|byte| *byte == 0xa5));
        }
        for parity in [0, 1] {
            for (index, ptr) in frames.iter().copied().enumerate() {
                if index % 2 == parity {
                    unsafe { pool.deallocate(ptr, PhysFrame::SIZE) };
                }
            }
        }
        assert_eq!(pool.allocated_bytes, metadata);
        assert_eq!(pool.allocator.free_bytes(), available);
    }

    #[test]
    fn counters_match_bitmap_occupancy_for_aligned_runs() {
        let backing = Backing::new(4 * 1024 * 1024);
        let mut pool = FramePool::new();
        pool.initialize(&[backing.range()]);
        let metadata = pool.allocated_bytes;
        let mut held = Vec::new();
        for shift in 12..=20 {
            let bytes = 1 << shift;
            let ptr = pool.allocate(bytes).expect("aligned run");
            assert_eq!(ptr.as_ptr().addr() % bytes, 0);
            assert_eq!(
                pool.total_bytes - pool.allocated_bytes,
                pool.allocator.free_bytes()
            );
            held.push((ptr, bytes));
        }
        for (ptr, bytes) in held.into_iter().rev() {
            unsafe { pool.deallocate(ptr, bytes) };
            assert_eq!(
                pool.total_bytes - pool.allocated_bytes,
                pool.allocator.free_bytes()
            );
        }
        assert_eq!(pool.allocated_bytes, metadata);
        assert!(pool.allocate(backing.layout.size()).is_err());
        assert_eq!(pool.allocated_bytes, metadata);
    }

    #[test]
    fn small_misaligned_maps_reserve_only_their_bitmap() {
        let backing = Backing::new(256 * 1024);
        let start = backing.start() + PhysFrame::SIZE;
        let mut pool = FramePool::new();
        pool.initialize(&[(start, start + 16 * PhysFrame::SIZE)]);
        assert_eq!(pool.total_bytes, 16 * PhysFrame::SIZE);
        assert_eq!(pool.allocated_bytes, PhysFrame::SIZE);
    }

    #[test]
    #[should_panic(expected = "InvalidUsable")]
    fn overlapping_regions_are_rejected() {
        let backing = Backing::new(256 * 1024);
        let mut pool = FramePool::new();
        pool.initialize(&[backing.range(), backing.range()]);
    }

    #[test]
    #[should_panic(expected = "initialized more than once")]
    fn initialization_cannot_replace_live_ownership() {
        let backing = Backing::new(256 * 1024);
        let mut pool = FramePool::new();
        pool.initialize(&[backing.range()]);
        let _held = pool.allocate(PhysFrame::SIZE).expect("frame");
        pool.initialize(&[backing.range()]);
    }
}
