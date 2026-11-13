//! The fiber-stack arena under real threads standing in for processors.
//!
//! A fiber-stack fault is served from the user pool, which another
//! processor may be inside of, and a cold claim draws on the same pool
//! while it may be empty. The arena's own bookkeeping is covered by the
//! kernel's unit tests against a fake address space; what only a hosted
//! machine adds is more than one thread to take the pool's locks, so
//! these tests are about what happens when the pool is contended or
//! empty: the fault waits and is served, the claim answers with a typed
//! error, and nothing panics.

#![cfg(test)]

use std::alloc::{Layout, alloc_zeroed};
use std::collections::BTreeMap;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use helios_hal::cpu::ProcessorId;
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpaceError, VirtAddr, VirtRange};
use helios_kernel::{
    FiberStackError, FiberStackVmHooks, InterruptedContext, StackFault, allocate_user_frame_zeroed,
    allocate_user_memory_pool, allocate_user_run_zeroed_on, claim_fiber_stack,
    deallocate_user_frame, deallocate_user_run_on, install_fiber_stack_arena,
    install_fiber_stack_hooks, install_user_memory_pool, resolve_stack_fault,
};

use crate::cpu::set_current_processor;

/// Ninety-six pages deep: a fault at the bottom has ninety-five fresh
/// pages to commit.
const STACK_BYTES: usize = 96 * PhysFrame::SIZE;
const SLOTS: usize = 2;
const PROCESSORS: usize = 2;
const POOL_BYTES: usize = 16 * 1024 * 1024;
const ARENA_BASE: usize = 0x0000_4000_0000_0000;

/// The fake page table: every mapped page and the frame behind it. A real
/// address space owns the frames it maps and gives them back with the
/// range that held them, and so does this.
static MAPPED: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());
static PREPARED: AtomicUsize = AtomicUsize::new(0);

static HOOKS: FiberStackVmHooks = FiberStackVmHooks {
    reserve: |bytes| Ok(VirtRange::new(VirtAddr::new(ARENA_BASE), bytes)),
    prepare_demand_commit: |_, _| {
        PREPARED.fetch_add(1, Ordering::Relaxed);
        Ok(())
    },
    commit_demand_page: |addr, frame, _| {
        let mut mapped = MAPPED.lock().expect("fake page table");
        if mapped.insert(addr.raw(), frame.as_ptr() as usize).is_some() {
            return Err(AddressSpaceError::Overlap);
        }
        Ok(())
    },
    end_demand_commit: |virt, _| {
        let mut mapped = MAPPED.lock().expect("fake page table");
        let pages: Vec<usize> = mapped
            .range(virt.start.raw()..virt.end().raw())
            .map(|(page, _)| *page)
            .collect();
        for page in pages {
            let frame = mapped.remove(&page).expect("a page just listed");
            deallocate_user_frame(NonNull::new(frame as *mut u8).expect("a mapped frame"));
        }
        Ok(())
    },
};

/// Builds the user pool and the arena on top of it, with the calling
/// thread as processor 0. Each test runs in its own process, so the
/// install-once statics are fresh.
fn arena() {
    set_current_processor(ProcessorId::new(0));
    let layout = Layout::from_size_align(POOL_BYTES, PhysFrame::SIZE).expect("pool layout");
    // SAFETY: the layout has a non-zero size.
    let base = unsafe { alloc_zeroed(layout) } as usize;
    assert!(base != 0, "pool backing");
    let pool = install_user_memory_pool(allocate_user_memory_pool());
    pool.initialize(&[(base, base + POOL_BYTES)]);
    pool.configure_processors(PROCESSORS);
    install_fiber_stack_hooks(&HOOKS);
    install_fiber_stack_arena(SLOTS, STACK_BYTES, PROCESSORS, 0);
}

fn enabled() -> InterruptedContext {
    InterruptedContext {
        interrupts_enabled: true,
    }
}

/// The processor-1 side of a contended pool: every pass takes and gives
/// back a run of two frames, which the pool serves from its buddy heap
/// under the buddy lock and never from a frame cache.
fn hammer_the_buddy_lock(stop: &AtomicBool) -> usize {
    set_current_processor(ProcessorId::new(1));
    let layout = Layout::from_size_align(2 * PhysFrame::SIZE, PhysFrame::SIZE).expect("run layout");
    let mut passes = 0;
    while !stop.load(Ordering::Acquire) {
        let run = allocate_user_run_zeroed_on(ProcessorId::new(1), layout)
            .expect("the pool has room for a two-frame run");
        deallocate_user_run_on(ProcessorId::new(1), run, layout);
        passes += 1;
    }
    passes
}

#[test]
fn a_fiber_stack_fault_under_a_contended_pool_is_served() {
    arena();
    let stop = AtomicBool::new(false);

    std::thread::scope(|scope| {
        let hammer = scope.spawn(|| hammer_the_buddy_lock(&stop));
        for _ in 0..200 {
            // Claim, fault at the bottom of the body (ninety-five fresh
            // pages), and give the stack back, all against the other
            // processor's use of the pool. The page-fault reserve and
            // this processor's frame-slab shard are empty throughout: a
            // hosted machine configures no reserve, and the pool's
            // frames are all in the buddy heap.
            let stack = claim_fiber_stack(STACK_BYTES).expect("a free slot");
            let range = stack.range();
            assert_eq!(
                resolve_stack_fault(VirtAddr::new(range.start), enabled()),
                StackFault::Committed,
                "a fault that finds the pool's lock taken waits for it"
            );
            drop(stack);
        }
        stop.store(true, Ordering::Release);
        let passes = hammer.join().expect("the contending processor");
        assert!(passes > 0, "the other processor used the pool meanwhile");
    });
    assert!(
        MAPPED.lock().expect("fake page table").is_empty(),
        "every stack gave its frames back"
    );
}

#[test]
fn a_cold_claim_and_a_fault_with_the_pool_exhausted_answer_without_panicking() {
    arena();
    // Everything the pool has.
    let mut held = Vec::new();
    while let Ok(frame) = allocate_user_frame_zeroed() {
        held.push(frame);
    }

    let refusal = claim_fiber_stack(STACK_BYTES).err();
    assert!(
        matches!(refusal, Some(FiberStackError::OutOfMemory(_))),
        "a cold claim with the pool exhausted is a typed out-of-memory error: {refusal:?}"
    );
    assert_eq!(PREPARED.load(Ordering::Relaxed), 0);

    // With a frame back the claim succeeds; then the pool runs dry
    // again and a fault needing more than is left is starved, not fatal.
    deallocate_user_frame(held.pop().expect("a frame to give back"));
    let stack = claim_fiber_stack(STACK_BYTES).expect("the top page's frame is back");
    let outcome = resolve_stack_fault(VirtAddr::new(stack.range().start), enabled());
    assert!(
        matches!(outcome, StackFault::Starved { .. }),
        "a fault the pool cannot back is starved: {outcome:?}"
    );
    assert_eq!(
        MAPPED.lock().expect("fake page table").len(),
        1,
        "the starved fault mapped nothing beyond the top page"
    );
}
