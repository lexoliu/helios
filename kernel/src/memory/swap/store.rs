//! The compressed store: LZ4 objects packed into single user-pool frames.
//!
//! # Layout
//!
//! Every storage frame is one 4 KiB frame of the user pool, cut into
//! [`GRANULES_PER_FRAME`] granules of [`GRANULE_BYTES`] bytes and tracked by
//! a 64-bit occupancy bitmap. An object takes consecutive granules of one
//! frame, never spanning two, so decompression reads one contiguous slice of
//! the direct map. It is a 4-byte header, the slot index its page
//! belongs to, then the LZ4 block. The header is the reverse map that
//! writeback and compaction need: from an object, its slot.
//!
//! A second bitmap, `starts`, marks the first granule of each object, so the
//! extents are recoverable from the two bitmaps alone. A raw page parked
//! after a failed placement owns a whole frame (`occupied` all ones, `starts`
//! bit 0) and records its slot.
//!
//! Frames come from the user pool, not the kernel heap: the balloon and
//! free-page reporting can only reach frames the pool has free, so a frame
//! the store holds is neither inflated nor reported, and the pool's pressure
//! accounting sees compression as net free frames.
//!
//! # Concurrency contract
//!
//! The store has two halves.
//!
//! [`StoreFrames`] is shared: the frame table and a dirty stack.
//!
//! - **Freeing** ([`StoreFrames::free_object`], [`StoreFrames::free_parked`])
//!   runs on any processor, by whoever holds the claim on the object's
//!   slot. It clears the object's bits with two `fetch_and`s (`starts`, then
//!   `occupied`) and queues the frame on the dirty stack once
//!   (`dirty_queued`). It is lock-free and allocates nothing.
//! - **Reading** an object ([`StoreFrames::payload`],
//!   [`StoreFrames::parked_page`]) runs on any processor, by whoever holds the
//!   claim. The bytes cannot move or be freed while it does: only the
//!   claimant frees them, and the owner moves an object only under its own
//!   claim.
//! - Frames are created and released by the owner only. A frame's table
//!   entry is never freed, and its index is reused.
//!
//! [`Store`] is the owner task's: the allocator and everything that changes
//! which frames exist. It is single-threaded by ownership.
//!
//! - **Allocation, parking, spare replacement, draining and release** run on
//!   the owner task only. They never block and never await.
//! - **Bits are set only by the owner and cleared by anyone** (`fetch_or`
//!   against `fetch_and`), so the owner's view of a frame's free space can
//!   only underestimate it. That is why a concurrent free needs no lock: a run
//!   the owner sees free is free, and a run it thinks occupied may already
//!   have been freed. The owner rebuckets a frame when it drains the dirty
//!   stack, at the top of its loop, and reads `occupied` after clearing the
//!   frame's `dirty_queued`, so a free that lands after the read queues the
//!   frame again.
//! - A frame is released to the pool only when its `occupied` is zero and it
//!   is not the spare. A dirty entry that names a frame already released
//!   (its pusher cleared the last bits and was slow to queue) finds the index
//!   dead, or live again as another frame, and is harmless in both cases.
//!
//! - **Walking a frame** ([`Store::claim_next`]) and **compaction**
//!   ([`Store::compact`]) run on the owner task only, and take the claim on
//!   each object's slot before touching it. The object's header names its
//!   slot, and the slot word names the object; an object is acted on only
//!   when the two agree and the claim's compare-and-swap succeeds, so an
//!   object a fault holds, or has just freed, is skipped. The owner is the
//!   only writer of object bytes, so nothing reads bytes while they change.
//!   Compaction is synchronous with no await; writeback holds each claim
//!   across the device write, which is what keeps a fault on that page
//!   waiting on the slot rather than racing the move.
//!
//! The store holds no lock and does not wait. Nothing in it is reachable from
//! an interrupt handler or from trap context.
//!
//! # Age
//!
//! Objects are, by construction, untouched since they were stored, because
//! any access faults them out, so the order frames were first filled in is
//! recency order. The owner keeps the frames that hold objects or a parked
//! page in a list in that order (`oldest` to `newest`), which writeback works
//! from the front. A frame takes its place when it is first filled, and
//! leaves when it is released.
//!
//! The frame table's entries are not cache-line padded, for the reason the
//! slot words are not: no processor owns one, and padding would cost
//! the memory the store exists to save.
//!
//! # The spare frame
//!
//! The store holds one empty, allocated spare frame. Taking it is how the
//! store grows, and parking a page after a failed placement uses it too, so
//! once the spare is held every detached page has a placement that needs no
//! allocation and no device. The caller replaces it
//! ([`Store::ensure_spare`]) before the next detach, and stops the pass if the
//! pool has no frame. The spare counts in [`Store::frames`] and is exempt
//! from the limit: the limit decides whether the store may take it for
//! objects, not whether it may exist.
//!
//! # Limits
//!
//! The store may grow to `pool_frames * limit_percent / 100` frames of
//! objects, where the pool's total shrinks when the kernel heap borrows, so
//! the caller supplies it with each call ([`Limits`]). Once an allocation is
//! refused for the limit the store refuses every allocation until it is down
//! to `limit * accept_percent / 100` frames of objects: the hysteresis keeps
//! it from flapping at the boundary.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::num::NonZeroU32;
use core::ptr::{self, NonNull};
use core::slice;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use helios_hal::cpu::current_processor;
use helios_hal::pmm::PhysFrame;
use spin::Once;
use thiserror::Error;

use super::slot::{
    Claim, CompressedLen, GRANULE_BYTES, GRANULES_PER_FRAME, Granule, OBJECT_HEADER_BYTES,
    SlotIndex, SlotState, SlotTable, StoreFrameIndex, StoredObject, object_granules,
};
use super::stack::{Chain, Links, ReturnStack};
use crate::memory::user::{allocate_user_frame_uninit_on, deallocate_user_frame_on};

/// Frames one chunk of the frame table holds.
const CHUNK_FRAMES: usize = 128;

/// The bitmap of a whole frame.
const FULL: u64 = u64::MAX;

/// A run of `granules` granules starting at `first`, as a bitmap.
const fn run_mask(first: usize, granules: usize) -> u64 {
    debug_assert!(granules >= 1 && first + granules <= GRANULES_PER_FRAME);
    if granules == GRANULES_PER_FRAME {
        FULL
    } else {
        ((1 << granules) - 1) << first
    }
}

/// The length of the longest run of free granules in `occupied`.
pub(super) const fn largest_free_run(occupied: u64) -> usize {
    let mut free = !occupied;
    let mut run = 0;
    while free != 0 {
        free &= free << 1;
        run += 1;
    }
    run
}

/// The first granule of the lowest run of `granules` free granules in
/// `occupied`.
pub(super) const fn first_free_run(occupied: u64, granules: usize) -> Option<usize> {
    debug_assert!(granules >= 1 && granules <= GRANULES_PER_FRAME);
    let free = !occupied;
    // Bit `s` survives when granules `s..s + granules` are all free.
    let mut starts = free;
    let mut step = 1;
    while step < granules {
        starts &= free >> step;
        step += 1;
    }
    if starts == 0 {
        None
    } else {
        Some(starts.trailing_zeros() as usize)
    }
}

/// One storage frame's shared state.
struct StoreFrame {
    /// The frame's direct-map address, null while its index is unused.
    /// Written by the owner when it creates or releases the frame.
    base: AtomicPtr<u8>,
    /// Granules in use: the owner sets bits, any processor clears them.
    occupied: AtomicU64,
    /// The first granule of each object, or bit 0 for a parked page.
    starts: AtomicU64,
    /// The link of the dirty stack.
    dirty_next: AtomicU32,
    /// Whether the frame is on the dirty stack or about to be.
    dirty_queued: AtomicBool,
    /// The slot of the page parked in this frame. Written by the owner.
    parked_slot: AtomicU32,
}

/// The waste at which compaction starts, as a percentage of the granules the
/// store's frames hold. An engineering constant, not an operator setting: it
/// trades the copy work of a pass against the frames it frees.
pub(super) const COMPACT_WASTE_PERCENT: usize = 25;

impl StoreFrame {
    const fn new() -> Self {
        Self {
            base: AtomicPtr::new(ptr::null_mut()),
            occupied: AtomicU64::new(0),
            starts: AtomicU64::new(0),
            dirty_next: AtomicU32::new(0),
            dirty_queued: AtomicBool::new(false),
            parked_slot: AtomicU32::new(0),
        }
    }

    /// The frame's bytes as a pointer. Only a live frame has any.
    fn bytes(&self) -> *mut u8 {
        let base = self.base.load(Ordering::Acquire);
        debug_assert!(
            !base.is_null(),
            "a store frame that is not live has no bytes"
        );
        base
    }
}

/// The shared half of the store: the frame table and the dirty stack.
pub(super) struct StoreFrames {
    top: Box<[Once<Box<[StoreFrame; CHUNK_FRAMES]>>]>,
    /// The most frames the table holds, which is the user pool's.
    capacity: u32,
    dirty: ReturnStack,
}

impl StoreFrames {
    /// A table for up to `max_frames` frames: the number the user pool
    /// holds, since the store can never exceed it.
    pub(super) fn new(max_frames: usize) -> Self {
        let capacity = u32::try_from(max_frames)
            .ok()
            .filter(|&frames| StoreFrameIndex::new(frames.max(1)).is_some())
            .expect("the store's frame table is sized by the user pool, which has fewer frames than a slot word can name");
        Self {
            top: (0..=max_frames / CHUNK_FRAMES)
                .map(|_| Once::new())
                .collect(),
            capacity,
            dirty: ReturnStack::new(),
        }
    }

    fn frame(&self, index: StoreFrameIndex) -> &StoreFrame {
        let raw = index.raw() as usize;
        let chunk = self.top[raw / CHUNK_FRAMES]
            .get()
            .unwrap_or_else(|| panic!("store frame {raw} has no table chunk: never created"));
        &chunk[raw % CHUNK_FRAMES]
    }

    /// The entry for `raw`, creating its chunk. The owner only.
    fn frame_or_create(&self, raw: u32) -> &StoreFrame {
        assert!(
            raw <= self.capacity,
            "store frame {raw} is past the table's {} frames: the store exceeded the user pool",
            self.capacity
        );
        let raw = raw as usize;
        let chunk = self.top[raw / CHUNK_FRAMES]
            .call_once(|| Box::new(core::array::from_fn(|_| StoreFrame::new())));
        &chunk[raw % CHUNK_FRAMES]
    }

    /// Frees the granules of an object whose slot the caller holds the claim
    /// on, and queues its frame for the owner. Any processor.
    ///
    /// Returns whether this free queued the frame, which is when the owner
    /// has a drain to do.
    pub(super) fn free_object(&self, object: StoredObject) -> bool {
        let frame = self.frame(object.frame());
        let first = object.first().raw();
        let granules = object_granules(object.len().get());
        frame.starts.fetch_and(!(1 << first), Ordering::AcqRel);
        frame
            .occupied
            .fetch_and(!run_mask(first, granules), Ordering::AcqRel);
        self.mark_dirty(object.frame())
    }

    /// Frees a parked page's frame and queues it for the owner. Any
    /// processor. Returns whether this free queued the frame.
    pub(super) fn free_parked(&self, index: StoreFrameIndex) -> bool {
        let frame = self.frame(index);
        frame.starts.fetch_and(!1, Ordering::AcqRel);
        frame.occupied.fetch_and(!FULL, Ordering::AcqRel);
        self.mark_dirty(index)
    }

    /// Queues the frame for the owner's next drain, once however many
    /// frees land before it. Returns whether this call queued it.
    pub(super) fn mark_dirty(&self, index: StoreFrameIndex) -> bool {
        let frame = self.frame(index);
        let queued = !frame.dirty_queued.swap(true, Ordering::AcqRel);
        if queued {
            self.dirty.push(self, nonzero(index));
        }
        queued
    }

    /// The LZ4 block of an object, for the holder of the claim on `slot`.
    ///
    /// # Panics
    ///
    /// If the object's header names another slot than the claim's: the
    /// store's bookkeeping or its memory is corrupt.
    pub(super) fn payload(&self, object: StoredObject, claim: &Claim) -> &[u8] {
        let base = self.frame(object.frame()).bytes();
        let start = object.first().raw() * GRANULE_BYTES;
        // SAFETY: the object's granules lie inside its frame (a `Granule`
        // and a gate-accepted length cannot describe more), the frame is
        // live while its object is, and the caller's claim keeps the owner
        // from moving the object and everyone else from freeing it, so the
        // bytes are stable and initialised: they were written before the
        // slot word that publishes them.
        let bytes = unsafe {
            slice::from_raw_parts(base.add(start), OBJECT_HEADER_BYTES + object.len().get())
        };
        let (header, payload) = bytes.split_at(OBJECT_HEADER_BYTES);
        let named = u32::from_le_bytes(header.try_into().expect("a four-byte header"));
        assert_eq!(
            named,
            claim.index().raw(),
            "store frame {} granule {} belongs to slot {named}, but slot {} claimed it",
            object.frame().raw(),
            object.first().raw(),
            claim.index().raw(),
        );
        payload
    }

    /// The page parked in a frame, for the holder of the claim on its slot.
    ///
    /// # Panics
    ///
    /// If the frame records another slot than the claim's.
    pub(super) fn parked_page(&self, index: StoreFrameIndex, claim: &Claim) -> &[u8] {
        let frame = self.frame(index);
        let named = frame.parked_slot.load(Ordering::Acquire);
        assert_eq!(
            named,
            claim.index().raw(),
            "store frame {} parks slot {named}'s page, but slot {} claimed it",
            index.raw(),
            claim.index().raw(),
        );
        // SAFETY: a parked frame is one whole live frame, its bytes were
        // written before the slot word that publishes them, and the claim
        // keeps it from being freed or rewritten.
        unsafe { slice::from_raw_parts(frame.bytes(), PhysFrame::SIZE) }
    }

    /// A frame's bytes, for a test that corrupts a stored object.
    #[cfg(test)]
    pub(super) fn frame_bytes(&self, index: StoreFrameIndex) -> *mut u8 {
        self.frame(index).bytes()
    }

    /// The slot a frame's parked page belongs to, as its raw index.
    pub(super) fn parked_slot(&self, index: StoreFrameIndex) -> u32 {
        self.frame(index).parked_slot.load(Ordering::Acquire)
    }

    /// The slot an object's header names: the reverse map from an object to
    /// the page it holds.
    ///
    /// The caller is the owner and `first` is a granule the frame's `starts`
    /// marked, so the bytes are an object's, or a freed object's, which stay
    /// until the owner reuses them.
    fn header_slot(&self, index: StoreFrameIndex, first: usize) -> SlotIndex {
        // SAFETY: `first` is below `GRANULES_PER_FRAME`, so a header lies
        // inside the live frame, and only the owner writes object bytes.
        let header = unsafe {
            slice::from_raw_parts(
                self.frame(index).bytes().add(first * GRANULE_BYTES),
                OBJECT_HEADER_BYTES,
            )
        };
        let raw = u32::from_le_bytes(header.try_into().expect("a four-byte header"));
        SlotIndex::new(raw).unwrap_or_else(|| {
            panic!(
                "store frame {} granule {first} names slot {raw}, past the slot table",
                index.raw()
            )
        })
    }

    /// The granules in use in a frame, as a bitmap.
    pub(super) fn occupied(&self, index: StoreFrameIndex) -> u64 {
        self.frame(index).occupied.load(Ordering::Acquire)
    }

    /// The first granule of each object in a frame, as a bitmap.
    pub(super) fn starts(&self, index: StoreFrameIndex) -> u64 {
        self.frame(index).starts.load(Ordering::Acquire)
    }
}

fn nonzero(index: StoreFrameIndex) -> NonZeroU32 {
    NonZeroU32::new(index.raw()).expect("a store frame index is never zero")
}

impl Links for StoreFrames {
    fn next_of(&self, handle: NonZeroU32) -> Option<NonZeroU32> {
        let index = StoreFrameIndex::new(handle.get()).expect("a queued frame index is valid");
        NonZeroU32::new(self.frame(index).dirty_next.load(Ordering::Acquire))
    }

    fn set_next(&self, handle: NonZeroU32, next: Option<NonZeroU32>) {
        let index = StoreFrameIndex::new(handle.get()).expect("a queued frame index is valid");
        self.frame(index)
            .dirty_next
            .store(next.map_or(0, NonZeroU32::get), Ordering::Release);
    }
}

/// The settings that bound the store, as the caller reads them for one
/// decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Limits {
    /// Frames the user pool holds right now.
    pub(super) pool_frames: usize,
    /// The share of the pool the store's objects may take, in percent.
    pub(super) limit_percent: u8,
    /// Once refused, the share of the limit the store must be back down to
    /// before it takes objects again, in percent.
    pub(super) accept_percent: u8,
}

impl Limits {
    pub(super) fn limit_frames(&self) -> usize {
        self.pool_frames * usize::from(self.limit_percent) / 100
    }

    pub(super) fn accept_frames(&self) -> usize {
        self.limit_frames() * usize::from(self.accept_percent) / 100
    }
}

/// The store refused an object: it is at its limit, or refusing until it is
/// back under the accept level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("the compressed store is at its limit")]
pub(super) struct StoreFull;

/// The user pool had no frame for the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("the user pool has no free frame for the compressed store")]
pub(super) struct PoolExhausted;

/// The owner's bookkeeping for one frame index.
#[derive(Clone, Copy, Default)]
struct OwnerFrame {
    /// Whether the index names a frame held from the pool.
    live: bool,
    /// The bucket the frame is in, by its largest free run, or `None` for
    /// the spare and for an index with no frame.
    bucket: Option<u8>,
    /// The frame's position in its bucket.
    position: u32,
    /// The neighbours in the age list, as raw frame indices, `0` for none.
    older: u32,
    newer: u32,
    /// Whether the frame is in the age list.
    aged: bool,
}

/// The owner's half of the store: where every frame is, by how much room it
/// has, and the one spare.
pub(super) struct Store {
    /// By frame index; entry 0 is unused.
    frames: Vec<OwnerFrame>,
    /// Frames by their largest free run, `0..=63`: a frame with no free
    /// granule is in bucket 0, and a frame with all 64 free is released
    /// rather than bucketed.
    buckets: Vec<Vec<u32>>,
    /// Bit `b` is set when bucket `b` has a frame.
    non_empty: u64,
    /// Indices of released frames, to reuse before the table grows.
    free_indices: Vec<u32>,
    next_index: u32,
    spare: Option<StoreFrameIndex>,
    /// Frames held from the pool, the spare included.
    held: usize,
    /// Whether the store is refusing until it is back under the accept
    /// level.
    refusing: bool,
    /// The ends of the age list, as raw frame indices, `0` when it is empty.
    oldest: u32,
    newest: u32,
    /// Frames in the age list.
    aged: usize,
}

/// What a pass of [`Store::compact`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Compacted {
    /// Objects moved.
    pub(super) objects: usize,
    /// Bytes of those objects, in whole granules.
    pub(super) bytes: usize,
    /// Frames the moves emptied and returned to the pool.
    pub(super) frames: usize,
}

impl Store {
    pub(super) fn new() -> Self {
        Self {
            oldest: 0,
            newest: 0,
            aged: 0,
            frames: Vec::from([OwnerFrame::default()]),
            buckets: (0..GRANULES_PER_FRAME).map(|_| Vec::new()).collect(),
            non_empty: 0,
            free_indices: Vec::new(),
            next_index: 1,
            spare: None,
            held: 0,
            refusing: false,
        }
    }

    /// Frames held from the pool, the spare included.
    pub(super) fn frames(&self) -> usize {
        self.held
    }

    /// Frames holding objects or parked pages.
    pub(super) fn object_frames(&self) -> usize {
        self.held - usize::from(self.spare.is_some())
    }

    #[cfg(test)]
    pub(super) fn has_spare(&self) -> bool {
        self.spare.is_some()
    }

    /// The spare frame, if one is held.
    #[cfg(test)]
    pub(super) fn spare(&self) -> Option<StoreFrameIndex> {
        self.spare
    }

    /// Holds a spare frame, allocating one from the user pool if there is
    /// none.
    pub(super) fn ensure_spare(&mut self, shared: &StoreFrames) -> Result<(), PoolExhausted> {
        if self.spare.is_some() {
            return Ok(());
        }
        // Not zeroed: nothing reads a frame's bytes before an object or a
        // page is written into it.
        let bytes =
            allocate_user_frame_uninit_on(current_processor()).map_err(|_| PoolExhausted)?;
        let raw = self.free_indices.pop().unwrap_or_else(|| {
            let raw = self.next_index;
            self.next_index += 1;
            self.frames.push(OwnerFrame::default());
            raw
        });
        let index = StoreFrameIndex::new(raw).expect("the table's capacity is a valid index");
        let frame = shared.frame_or_create(raw);
        // The index may be a reused one whose last frame was freed by
        // another processor a moment ago: `dirty_queued` is left alone, for a
        // stale queue entry still owns it.
        frame.occupied.store(0, Ordering::Release);
        frame.starts.store(0, Ordering::Release);
        frame.base.store(bytes.as_ptr(), Ordering::Release);
        self.frames[raw as usize] = OwnerFrame {
            live: true,
            ..OwnerFrame::default()
        };
        self.spare = Some(index);
        self.held += 1;
        Ok(())
    }

    /// Stores an LZ4 block of `len` bytes, which belongs to `slot`, in the
    /// best-fitting free run of any frame, or in a new frame when none fits.
    ///
    /// # Errors
    ///
    /// [`StoreFull`] when a new frame is needed and the store is at its
    /// limit, or while it is refusing until it is back under the accept level.
    ///
    /// # Panics
    ///
    /// If a new frame is needed and no spare is held: the caller holds one
    /// before every detach.
    pub(super) fn allocate(
        &mut self,
        shared: &StoreFrames,
        slot: SlotIndex,
        block: &[u8],
        len: CompressedLen,
        limits: Limits,
    ) -> Result<StoredObject, StoreFull> {
        assert_eq!(
            block.len(),
            len.get(),
            "the block is the length the gate took"
        );
        let granules = object_granules(len.get());
        assert!(
            granules < GRANULES_PER_FRAME,
            "an object of {granules} granules saves nothing and cannot have passed the gate"
        );
        if self.refusing {
            if self.object_frames() > limits.accept_frames() {
                return Err(StoreFull);
            }
            self.refusing = false;
        }

        let (index, first, occupied) = match self.best_fit(shared, granules) {
            Some(found) => found,
            None => {
                if self.object_frames() >= limits.limit_frames() {
                    self.refusing = true;
                    return Err(StoreFull);
                }
                let spare = self
                    .spare
                    .take()
                    .expect("a spare frame is held before every detach");
                self.age_push(spare);
                (spare, 0, 0)
            }
        };

        let frame = shared.frame(index);
        let start = first * GRANULE_BYTES;
        // SAFETY: the run `first..first + granules` is free in the owner's
        // view, which can only underestimate free space, and only the owner
        // sets bits, so nothing else writes or reads it. It lies inside the
        // frame (`first_free_run` found it in 64 granules), and
        // `OBJECT_HEADER_BYTES + len` bytes fit the `granules` granules.
        unsafe {
            let dst = frame.bytes().add(start);
            ptr::copy_nonoverlapping(slot.raw().to_le_bytes().as_ptr(), dst, OBJECT_HEADER_BYTES);
            ptr::copy_nonoverlapping(block.as_ptr(), dst.add(OBJECT_HEADER_BYTES), block.len());
        }
        let mask = run_mask(first, granules);
        // The bytes are written; the bits that say so follow. The object is
        // reachable only through the slot word the caller publishes next.
        frame.starts.fetch_or(1 << first, Ordering::AcqRel);
        frame.occupied.fetch_or(mask, Ordering::AcqRel);
        self.rebucket(index, largest_free_run(occupied | mask));

        Ok(StoredObject::new(
            index,
            Granule::new(first).expect("a run found inside the frame"),
            len,
        ))
    }

    /// Parks a raw page, which belongs to `claim`'s slot, in the spare frame.
    ///
    /// This is the placement of last resort: it needs no allocation and no
    /// device, so a page detached after the spare was ensured always has
    /// somewhere to go. It is exempt from the limit.
    ///
    /// # Panics
    ///
    /// If no spare is held, or `page` is not one frame.
    pub(super) fn park(
        &mut self,
        shared: &StoreFrames,
        claim: &Claim,
        page: &[u8],
    ) -> StoreFrameIndex {
        assert_eq!(page.len(), PhysFrame::SIZE, "a parked page is one frame");
        let index = self
            .spare
            .take()
            .expect("a spare frame is held before every detach");
        let frame = shared.frame(index);
        // SAFETY: the spare is one whole live frame nothing else reads or
        // writes, and `page` is one page that cannot overlap a pool frame.
        unsafe {
            ptr::copy_nonoverlapping(page.as_ptr(), frame.bytes(), PhysFrame::SIZE);
        }
        frame
            .parked_slot
            .store(claim.index().raw(), Ordering::Release);
        frame.starts.fetch_or(1, Ordering::AcqRel);
        frame.occupied.fetch_or(FULL, Ordering::AcqRel);
        self.rebucket(index, 0);
        self.age_push(index);
        index
    }

    /// Takes the frames other processors freed things in off the dirty stack:
    /// rebuckets those that still hold something, and gives those that hold
    /// nothing back to the pool, except the spare. Returns how many frames
    /// went back.
    pub(super) fn drain(&mut self, shared: &StoreFrames) -> usize {
        let mut released = 0;
        let mut chain: Chain = shared.dirty.take_all();
        while let Some(handle) = chain.pop(shared) {
            let index = StoreFrameIndex::new(handle.get()).expect("a queued frame index is valid");
            let frame = shared.frame(index);
            // Cleared before `occupied` is read: a free that lands after the
            // read finds the flag clear and queues the frame again. An
            // acquire on the swap pairs with the release of a free that
            // found the flag set and so queued nothing.
            frame.dirty_queued.swap(false, Ordering::AcqRel);
            if !self.frames[index.raw() as usize].live || self.spare == Some(index) {
                // A stale entry for a frame already released, or the spare,
                // which holds nothing by definition and is not the store's to
                // give back.
                continue;
            }
            let occupied = frame.occupied.load(Ordering::Acquire);
            if occupied == 0 {
                self.release(shared, index);
                released += 1;
            } else {
                self.rebucket(index, largest_free_run(occupied));
            }
        }
        released
    }

    /// The frame with the smallest largest free run that still fits
    /// `granules`, the lowest run of that size in it, and the frame's
    /// occupancy as read.
    fn best_fit(
        &self,
        shared: &StoreFrames,
        granules: usize,
    ) -> Option<(StoreFrameIndex, usize, u64)> {
        let fitting = self.non_empty & (FULL << granules);
        if fitting == 0 {
            return None;
        }
        let bucket = fitting.trailing_zeros() as usize;
        let raw = *self.buckets[bucket]
            .last()
            .expect("a bucket the mask marks is not empty");
        let index = StoreFrameIndex::new(raw).expect("a bucketed index is valid");
        let occupied = shared.occupied(index);
        let first = first_free_run(occupied, granules).unwrap_or_else(|| {
            panic!(
                "store frame {raw} is in bucket {bucket}, which fits {granules} granules, but its \
                 occupancy {occupied:#018x} has no such run"
            )
        });
        Some((index, first, occupied))
    }

    /// Moves a frame to the bucket for its largest free run.
    fn rebucket(&mut self, index: StoreFrameIndex, run: usize) {
        self.unbucket(index);
        let raw = index.raw();
        let bucket = &mut self.buckets[run];
        self.frames[raw as usize].bucket = Some(run as u8);
        self.frames[raw as usize].position = bucket.len() as u32;
        bucket.push(raw);
        self.non_empty |= 1 << run;
    }

    fn unbucket(&mut self, index: StoreFrameIndex) {
        let raw = index.raw() as usize;
        let Some(run) = self.frames[raw].bucket.take() else {
            return;
        };
        let position = self.frames[raw].position as usize;
        let bucket = &mut self.buckets[usize::from(run)];
        bucket.swap_remove(position);
        if let Some(&moved) = bucket.get(position) {
            self.frames[moved as usize].position = position as u32;
        }
        if bucket.is_empty() {
            self.non_empty &= !(1 << run);
        }
    }

    /// Appends a frame that has just taken its first object, or its parked
    /// page, to the age list.
    fn age_push(&mut self, index: StoreFrameIndex) {
        let raw = index.raw();
        let newest = self.newest;
        let entry = &mut self.frames[raw as usize];
        debug_assert!(!entry.aged, "store frame {raw} is already in the age list");
        *entry = OwnerFrame {
            aged: true,
            older: newest,
            newer: 0,
            ..*entry
        };
        if newest == 0 {
            self.oldest = raw;
        } else {
            self.frames[newest as usize].newer = raw;
        }
        self.newest = raw;
        self.aged += 1;
    }

    /// Takes a frame out of the age list, wherever it is. A frame that is
    /// not in it is left alone.
    fn age_unlink(&mut self, index: StoreFrameIndex) {
        let raw = index.raw() as usize;
        let OwnerFrame {
            aged, older, newer, ..
        } = self.frames[raw];
        if !aged {
            return;
        }
        if older == 0 {
            self.oldest = newer;
        } else {
            self.frames[older as usize].newer = newer;
        }
        if newer == 0 {
            self.newest = older;
        } else {
            self.frames[newer as usize].older = older;
        }
        let entry = &mut self.frames[raw];
        entry.aged = false;
        entry.older = 0;
        entry.newer = 0;
        self.aged -= 1;
    }

    /// Frames in the age list: those holding objects or a parked page.
    pub(super) fn aged_frames(&self) -> usize {
        self.aged
    }

    /// The oldest frame in the age list, moved to the newest end so that the
    /// next call names the frame after it.
    ///
    /// Writeback works frames from the front. A frame it could not empty (an
    /// object a fault held, a device that refused) goes to the back rather
    /// than staying where the next pass would meet it first.
    pub(super) fn rotate_oldest(&mut self) -> Option<StoreFrameIndex> {
        let index = StoreFrameIndex::new(self.oldest)?;
        self.age_unlink(index);
        self.age_push(index);
        Some(index)
    }

    /// How many frames the store would have to free to be at or under
    /// `limits.accept_frames()`, when it is over its limit or refusing, which
    /// is when writeback runs whatever the pressure.
    pub(super) fn over_limit_by(&self, limits: Limits) -> Option<usize> {
        let frames = self.object_frames();
        if !self.refusing && frames <= limits.limit_frames() {
            return None;
        }
        Some(frames.saturating_sub(limits.accept_frames())).filter(|&over| over > 0)
    }

    /// Takes the claim on the next object, or the parked page, of `frame` at
    /// or after granule `*cursor` that no one else holds, and moves the
    /// cursor past it.
    ///
    /// Returns the claim and the state it was taken from: `Stored` for an
    /// object, `Parked` for a page. An object whose slot is held (a fault is
    /// reading it) or has just been freed is passed over.
    pub(super) fn claim_next(
        &self,
        shared: &StoreFrames,
        slots: &SlotTable,
        frame: StoreFrameIndex,
        cursor: &mut usize,
    ) -> Option<(Claim, SlotState)> {
        let starts = shared.starts(frame);
        if starts == 1 && shared.occupied(frame) == FULL {
            // A parked page is a whole frame: one object at granule 0.
            if *cursor != 0 {
                return None;
            }
            *cursor = GRANULES_PER_FRAME;
            let slot = SlotIndex::new(shared.parked_slot(frame))
                .expect("a parked frame records the slot of its page");
            let state = SlotState::Parked(frame);
            return slots.claim(slot, state).ok().map(|claim| (claim, state));
        }
        while *cursor < GRANULES_PER_FRAME {
            let remaining = starts >> *cursor;
            if remaining == 0 {
                break;
            }
            let first = *cursor + remaining.trailing_zeros() as usize;
            *cursor = first + 1;
            let slot = shared.header_slot(frame, first);
            let state = slots.state(slot);
            let SlotState::Stored(object) = state else {
                continue;
            };
            if object.frame() != frame || object.first().raw() != first {
                continue;
            }
            if let Ok(claim) = slots.claim(slot, state) {
                return Some((claim, state));
            }
        }
        None
    }

    /// Granules in use across the store's frames, the parked pages' included.
    fn live_granules(&self, shared: &StoreFrames) -> usize {
        self.frames
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, frame)| frame.live)
            .filter_map(|(raw, _)| StoreFrameIndex::new(raw as u32))
            .filter(|&index| self.spare != Some(index))
            .map(|index| shared.occupied(index).count_ones() as usize)
            .sum()
    }

    /// The share of the granules the store's frames hold that no object
    /// uses, as a percentage, given the granules in use.
    fn waste_percent(&self, live: usize) -> usize {
        let held = self.object_frames() * GRANULES_PER_FRAME;
        if held == 0 {
            return 0;
        }
        held.saturating_sub(live) * 100 / held
    }

    /// The store's waste as it stands, for a test to compare with the
    /// threshold.
    #[cfg(test)]
    pub(super) fn waste_percent_now(&self, shared: &StoreFrames) -> usize {
        self.waste_percent(self.live_granules(shared))
    }

    /// Moves objects out of the sparsest frames into the holes of others,
    /// returning the frames that empties to the pool, while the store wastes
    /// at least [`COMPACT_WASTE_PERCENT`] of its granules and `budget` bytes
    /// of objects remain to move.
    ///
    /// `published` runs after each object's slot is published at its new
    /// place, which is when a fault that found the slot held can go on.
    ///
    /// Synchronous: the owner task has no await in the pass. A frame that
    /// cannot be emptied (an object a fault holds, no hole that fits, the
    /// budget spent) ends the pass.
    pub(super) fn compact(
        &mut self,
        shared: &StoreFrames,
        slots: &SlotTable,
        budget: usize,
        mut published: impl FnMut(),
    ) -> Compacted {
        let mut done = Compacted::default();
        let live = self.live_granules(shared);
        while done.bytes + GRANULE_BYTES <= budget
            && self.waste_percent(live) >= COMPACT_WASTE_PERCENT
        {
            let Some(source) = self.sparsest() else {
                break;
            };
            if !self.empty_frame(
                shared,
                slots,
                source,
                budget - done.bytes,
                &mut published,
                &mut done,
            ) {
                break;
            }
        }
        done
    }

    /// The frame to empty next: the one in the highest non-empty bucket, which
    /// has the most contiguous room and so at most `64 - run` granules of
    /// objects. A full frame (bucket 0) has nothing to gain.
    fn sparsest(&self) -> Option<StoreFrameIndex> {
        let top = self.non_empty.checked_ilog2()? as usize;
        if top == 0 {
            return None;
        }
        StoreFrameIndex::new(*self.buckets[top].last()?)
    }

    /// Moves what it can of `source`'s objects, up to `budget` bytes, into
    /// the best-fitting holes of other frames, and releases the frame if that
    /// emptied it. Returns whether it did.
    fn empty_frame(
        &mut self,
        shared: &StoreFrames,
        slots: &SlotTable,
        source: StoreFrameIndex,
        budget: usize,
        published: &mut impl FnMut(),
        done: &mut Compacted,
    ) -> bool {
        // Out of the buckets while it is emptied, so it is never its own
        // destination.
        self.unbucket(source);
        let mut spent = 0;
        let mut starts = shared.starts(source);
        while starts != 0 {
            let first = starts.trailing_zeros() as usize;
            starts &= starts - 1;
            let slot = shared.header_slot(source, first);
            let state = slots.state(slot);
            let SlotState::Stored(object) = state else {
                continue;
            };
            if object.frame() != source || object.first().raw() != first {
                continue;
            }
            let granules = object_granules(object.len().get());
            if spent + granules * GRANULE_BYTES > budget {
                break;
            }
            let Some((destination, at, occupied)) = self.best_fit(shared, granules) else {
                break;
            };
            let Ok(claim) = slots.claim(slot, state) else {
                continue;
            };
            let len = OBJECT_HEADER_BYTES + object.len().get();
            // SAFETY: both runs lie inside their frames, which are live and
            // distinct (the source is out of the buckets, so `best_fit` cannot
            // name it); the destination run is free in the owner's view and
            // only the owner sets bits; the source object is held by the
            // claim, so no one frees or reads it as it is copied.
            unsafe {
                ptr::copy_nonoverlapping(
                    shared
                        .frame(source)
                        .bytes()
                        .add(first * GRANULE_BYTES)
                        .cast_const(),
                    shared.frame(destination).bytes().add(at * GRANULE_BYTES),
                    len,
                );
            }
            let target = shared.frame(destination);
            let mask = run_mask(at, granules);
            target.starts.fetch_or(1 << at, Ordering::AcqRel);
            target.occupied.fetch_or(mask, Ordering::AcqRel);
            self.rebucket(destination, largest_free_run(occupied | mask));
            let moved = StoredObject::new(
                destination,
                Granule::new(at).expect("a run found inside the frame"),
                object.len(),
            );
            slots.publish(claim, SlotState::Stored(moved));
            published();
            shared.free_object(object);
            spent += granules * GRANULE_BYTES;
            done.objects += 1;
            done.bytes += granules * GRANULE_BYTES;
        }
        let occupied = shared.occupied(source);
        if occupied == 0 {
            self.release(shared, source);
            done.frames += 1;
            true
        } else {
            self.rebucket(source, largest_free_run(occupied));
            false
        }
    }

    /// Gives the spare back to the pool, so a test can check the pool is as
    /// it was found.
    #[cfg(test)]
    pub(super) fn release_spare(&mut self, shared: &StoreFrames) {
        if let Some(index) = self.spare.take() {
            self.release(shared, index);
        }
    }

    /// Gives an empty frame back to the pool.
    fn release(&mut self, shared: &StoreFrames, index: StoreFrameIndex) {
        self.unbucket(index);
        self.age_unlink(index);
        let raw = index.raw();
        let base = shared
            .frame(index)
            .base
            .swap(ptr::null_mut(), Ordering::AcqRel);
        let base = NonNull::new(base).expect("a live store frame has a base");
        deallocate_user_frame_on(current_processor(), base);
        self.frames[raw as usize] = OwnerFrame::default();
        self.free_indices.push(raw);
        self.held -= 1;
    }
}
