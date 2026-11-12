//! The user pool the swap tests that take frames from it share.
//!
//! The pool is process-global and its accounting is what several tests
//! read, so a test holds the turn from [`serialise`] for its whole body.

use alloc::alloc::{Layout, alloc_zeroed};
use std::sync::{Mutex, MutexGuard, Once};

use helios_hal::pmm::PhysFrame;

use crate::memory::{allocate_user_memory_pool, install_user_memory_pool, user_heap_stats};

const POOL_BYTES: usize = 4 * 1024 * 1024;

/// Frames the pool holds.
pub(super) const POOL_FRAMES: usize = POOL_BYTES / PhysFrame::SIZE;

/// The tests share one process-global pool when run as threads, and read
/// its accounting, so they take turns.
pub(super) fn serialise() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    static POOL: Once = Once::new();
    let guard = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    POOL.call_once(|| {
        let layout = Layout::from_size_align(POOL_BYTES, PhysFrame::SIZE).expect("pool layout");
        // Leaked on purpose: the pool is a `&'static` for the life of the
        // test process.
        // SAFETY: the layout has a non-zero size.
        let base = unsafe { alloc_zeroed(layout) } as usize;
        assert!(base != 0, "pool backing");
        let pool = install_user_memory_pool(allocate_user_memory_pool());
        pool.initialize(&[(base, base + POOL_BYTES)]);
        pool.configure_processors(1);
    });
    guard
}

/// Bytes the pool has handed out right now.
pub(super) fn allocated_bytes() -> usize {
    user_heap_stats().allocated_bytes
}
