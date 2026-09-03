//! Tests for `helios_kernel::KernelPhysFrameAllocator`.
//!
//! Hosted-side because the kernel crate's lib-test compilation has
//! pre-existing breakage in unrelated WASI binding code. The frame
//! allocator only depends on `buddy_system_allocator::LockedHeap`,
//! which works the same way in any environment.

#![cfg(test)]

use helios_hal::pmm::{FrameAllocError, PhysFrame, PhysFrameAllocator};
use helios_kernel::KernelPhysFrameAllocator;

const PAGE: usize = PhysFrame::SIZE;

/// Allocate a 16 MiB block of host memory aligned to a page, leaked
/// for the duration of the test process. Tests use this as the
/// physical pool. We do not free it; the OS reclaims at process exit.
fn leak_aligned_pool(size: usize) -> (usize, usize) {
    use std::alloc::{Layout, alloc};
    let layout = Layout::from_size_align(size, PAGE).expect("layout");
    let ptr = unsafe { alloc(layout) };
    assert!(!ptr.is_null(), "host alloc failed");
    let start = ptr as usize;
    (start, start + size)
}

#[test]
fn allocate_zero_count_errors() {
    let allocator = KernelPhysFrameAllocator::new();
    let (start, end) = leak_aligned_pool(16 * 1024 * 1024);
    allocator.add_region(start, end);
    assert!(matches!(
        allocator.allocate(0, false),
        Err(FrameAllocError::OutOfFrames { .. })
    ));
}

#[test]
fn allocate_and_deallocate_round_trip() {
    let allocator = KernelPhysFrameAllocator::new();
    let (start, end) = leak_aligned_pool(16 * 1024 * 1024);
    allocator.add_region(start, end);

    let stats_before = allocator.stats();
    let range = allocator
        .allocate(4, false)
        .expect("4-frame allocation must succeed in a 16 MiB pool");
    assert_eq!(range.frame_count, 4);
    assert_eq!(range.byte_size(), 4 * PAGE);
    assert!(allocator.stats().allocated_frames >= 4);

    allocator.deallocate(range);
    let stats_after = allocator.stats();
    assert_eq!(
        stats_after.allocated_frames, stats_before.allocated_frames,
        "deallocate must restore the allocated-frame count"
    );
}

#[test]
fn zero_first_use_writes_zeros() {
    let allocator = KernelPhysFrameAllocator::new();
    let (start, end) = leak_aligned_pool(4 * 1024 * 1024);
    allocator.add_region(start, end);

    // Allocate without zeroing, write a non-zero pattern, free.
    let dirty = allocator.allocate(2, false).expect("dirty alloc");
    let dirty_ptr = dirty.start.phys_addr() as *mut u8;
    unsafe {
        core::ptr::write_bytes(dirty_ptr, 0xAA, dirty.byte_size());
    }
    allocator.deallocate(dirty);

    // Re-allocate the same size with zero_first_use=true. The buddy
    // is likely to hand us back the same region, and the bytes must
    // be zero.
    let clean = allocator.allocate(2, true).expect("clean alloc");
    let clean_slice = unsafe {
        core::slice::from_raw_parts(clean.start.phys_addr() as *const u8, clean.byte_size())
    };
    assert!(clean_slice.iter().all(|byte| *byte == 0));
    allocator.deallocate(clean);
}

#[test]
fn out_of_frames_reports_available() {
    let allocator = KernelPhysFrameAllocator::new();
    let pool_bytes = 1024 * 1024; // 1 MiB
    let (start, end) = leak_aligned_pool(pool_bytes);
    allocator.add_region(start, end);

    // Request 1024 frames = 4 MiB, way more than the 256-frame pool.
    let result = allocator.allocate(1024, false);
    match result {
        Err(FrameAllocError::OutOfFrames {
            requested,
            available,
        }) => {
            assert_eq!(requested, 1024);
            assert!(available <= pool_bytes / PAGE);
        }
        other => panic!("expected OutOfFrames, got {other:?}"),
    }
}

#[test]
fn stats_reflect_added_regions() {
    let allocator = KernelPhysFrameAllocator::new();
    let (start, end) = leak_aligned_pool(8 * 1024 * 1024);
    allocator.add_region(start, end);

    let stats = allocator.stats();
    // Buddy heap reserves some metadata so total_frames is at most
    // the added region size in frames; never zero.
    assert!(stats.total_frames > 0);
    assert!(stats.total_frames <= 8 * 1024 * 1024 / PAGE);
    assert_eq!(stats.allocated_frames, 0);
    // largest_free_run is approximated from total - allocated.
    assert_eq!(stats.largest_free_run, stats.total_frames);
}

#[test]
fn multiple_regions_aggregate() {
    let allocator = KernelPhysFrameAllocator::new();
    let (start_a, end_a) = leak_aligned_pool(2 * 1024 * 1024);
    let (start_b, end_b) = leak_aligned_pool(2 * 1024 * 1024);
    allocator.add_region(start_a, end_a);
    allocator.add_region(start_b, end_b);

    let stats = allocator.stats();
    assert!(stats.total_frames >= (4 * 1024 * 1024 / PAGE) - 4);
}
