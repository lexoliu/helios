//! The user pool the swap tests that take frames from it share.
//!
//! The pool is process-global and its accounting is what several tests
//! read, so a test holds the turn from [`serialise`] for its whole body.

use alloc::alloc::{Layout, alloc_zeroed};
use alloc::vec;
use alloc::vec::Vec;
use std::sync::{Mutex, MutexGuard, Once};

use helios_hal::pmm::PhysFrame;

use super::slot::StoredObject;
use super::store::{Store, StoreFrames};

use crate::memory::{allocate_user_memory_pool, install_user_memory_pool, user_heap_stats};

/// Large enough for a balloon test to find free runs of 512 frames around
/// the store's frames.
const POOL_BYTES: usize = 16 * 1024 * 1024;

/// Frames the pool holds.
pub(super) const POOL_FRAMES: usize = POOL_BYTES / PhysFrame::SIZE;

/// The tests share one process-global pool when run as threads, and read
/// its accounting, so they take turns.
pub(crate) fn serialise() -> MutexGuard<'static, ()> {
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

/// Compressed-store frames held from the shared pool, for a test outside
/// the swap module that must show other users of the pool never touch them.
pub(crate) struct HeldStoreFrames {
    frames: StoreFrames,
    store: Store,
    /// Where each object frame, and the spare, begins.
    addresses: Vec<usize>,
    objects: Vec<StoredObject>,
}

impl HeldStoreFrames {
    /// Holds `objects` full-frame objects and the spare frame.
    pub(crate) fn new(objects: usize) -> Self {
        use super::slot::{CompressedLen, Gate, SlotAllocator, SlotTable};
        use super::store::Limits;

        let frames = StoreFrames::new(POOL_FRAMES);
        let mut store = Store::new();
        store.ensure_spare(&frames).expect("the pool has a frame");
        let slots = SlotTable::new();
        let mut allocator = SlotAllocator::new();
        // 63 granules: the largest object the gate admits.
        let len: CompressedLen = Gate::new(63)
            .expect("a gate")
            .accept(63 * 64 - 4)
            .expect("an object of 63 granules");
        let block = vec![0xA5_u8; len.get()];
        let limits = Limits {
            pool_frames: POOL_FRAMES,
            limit_percent: 100,
            accept_percent: 100,
        };
        let mut addresses = Vec::new();
        let mut held = Vec::new();
        for _ in 0..objects {
            store.ensure_spare(&frames).expect("the pool has a frame");
            let claim = allocator.reserve(&slots).expect("a slot");
            let object = store
                .allocate(&frames, claim.index(), &block, len, limits)
                .expect("an unlimited store takes the object");
            addresses.push(frames.frame_bytes(object.frame()) as usize);
            held.push(object);
        }
        store.ensure_spare(&frames).expect("the pool has a frame");
        let spare = store.spare().expect("the spare is held");
        addresses.push(frames.frame_bytes(spare) as usize);
        Self {
            frames,
            store,
            addresses,
            objects: held,
        }
    }

    /// The first byte of every frame the store holds.
    pub(crate) fn addresses(&self) -> &[usize] {
        &self.addresses
    }
}

impl Drop for HeldStoreFrames {
    fn drop(&mut self) {
        for object in self.objects.drain(..) {
            self.frames.free_object(object);
        }
        self.store.drain(&self.frames);
        self.store.release_spare(&self.frames);
    }
}
