//! The kernel's block cache: the blocks of every mount-source disk that
//! a filesystem has read or written, held in kernel memory.
//!
//! # Concurrency contract
//!
//! - **A hit takes no lock.** A lookup first reads the executing
//!   processor's lookaside — a small direct-mapped table of the blocks
//!   this processor touched last, written only by tasks running on that
//!   processor — and then the shared index, whose readers never lock.
//!   Either way the block is taken with one compare-and-swap on the
//!   entry's state word, which pins the entry and grants a shared read
//!   in the same step. Releasing a read guard is one more.
//! - **A miss takes one bucket-stripe spin lock**, held only while the
//!   miss inserts its entry into the chain and never across an
//!   `.await`. The insertion re-checks the chain under that lock, so
//!   two processors missing on the same block meet at one entry: the
//!   first inserts it locked and asks the device for the block, the
//!   second finds the locked entry, pins it and parks until the read
//!   completes. The device is asked once.
//! - **Device requests fan out to the disk's I/O tasks.** The cache does
//!   not know the device's type; it hands requests to a fixed-capacity
//!   ring that the disk's worker tasks — monomorphized over the device
//!   the backend brought up — drain on the processor the device's
//!   completions are routed to (see `worker.rs`). A request names an
//!   entry, so submitting one allocates nothing.
//! - **Waiting parks on a hashed wait table.** A task waiting on an
//!   entry sets the entry's `WAITERS` bit in the same word the state
//!   lives in and parks on one of [`WAIT_TABLE_SIZE`] broadcast signals;
//!   the task that changes the state clears the bit and raises the
//!   signal in the same update. A state change nobody waits on raises
//!   nothing.
//! - **Moving a block between tiers owns the entry like a read does.**
//!   Compressing a block or decompressing it takes the entry's `LOCKED`
//!   bit with one compare-and-swap, so the entry stays in the index and
//!   a task that wants the block meanwhile parks on it exactly as on a
//!   read in flight. The work runs on the processor that took the bit,
//!   against the pool's per-processor decoder, with no lock another
//!   processor takes.
//! - **Every entry word is written by any processor**, so each entry
//!   lives on its own cache line. Bucket heads are written only by a
//!   miss or an eviction under the stripe lock that owns their whole
//!   cache line, so two processors never write one line at once.
//!
//! # Prior art
//!
//! The shape follows Linux's buffer and page caches, with the pieces
//! that let a reader avoid RCU taken from the socket hash tables:
//!
//! - The per-processor lookaside is `bh_lru` (`fs/buffer.c`): sixteen
//!   recently used buffers per CPU, checked before the shared lookup.
//! - The index is a hash table whose chains end in a *nulls* marker
//!   naming their bucket (`include/linux/list_nulls.h`, documented in
//!   `Documentation/RCU/rculist_nulls.rst`). Entries are type-stable —
//!   never freed while the cache lives, only reused — so a reader that
//!   follows a reused entry into another chain lands on the wrong
//!   bucket's marker and restarts, instead of needing RCU to keep the
//!   entry where it was.
//! - A lookup pins speculatively and re-checks the key afterwards, as
//!   `find_get_page` does with `page_cache_get_speculative`.
//! - Two concurrent misses share one read because the entry is inserted
//!   locked before the read is issued, as a buffer head is locked
//!   (`BH_Lock`) around its read and later arrivals `wait_on_buffer`.
//! - The `WAITERS` bit and the hashed wait table are `PG_waiters` and
//!   the page wait table (`mm/filemap.c`).
//! - Write errors are reported to the next [`CachedDisk::sync`] from a
//!   per-disk error sequence, as `errseq_t` reports writeback errors.
//! - Concurrent flushes coalesce: one device flush answers every flush
//!   requested before it was issued, as the block layer's flush state
//!   machine merges pending flushes (`block/blk-flush.c`).
//! - Eviction is a CLOCK sweep with a second chance for referenced
//!   blocks, the approximation of LRU the page cache's
//!   active/inactive lists refine.
//!
//! # Write ordering
//!
//! A block written through [`CachedDisk::write`] or
//! [`CachedDisk::overwrite`] is *dirty*: the cache may write it back at
//! any time, under memory pressure or to make room, and guarantees only
//! that it is on stable storage when the next [`CachedDisk::sync`]
//! returns. That is the contract a copy-on-write filesystem's tree
//! blocks need, because nothing refers to them until the commit block
//! does. The commit block itself never becomes dirty:
//! [`CachedDisk::commit`] writes every block dirtied before it, waits for
//! them, flushes the device's write cache, writes the commit block and
//! flushes again — the order a btrfs superblock or a jbd2 commit record
//! is written in. A filesystem therefore writes its commit block only
//! through [`CachedDisk::commit`].
//!
//! # Memory
//!
//! Blocks live in page-sized buffers taken from the kernel heap: the
//! cache is kernel memory (AGENTS §3.1), never charged to an instance.
//! It grows while the machine's memory pressure is green and recycles
//! its coldest clean block once it is not. The entry table's bound is
//! half the machine — the cache cannot grow past the point where
//! pressure stops being green — and only the entries the cache actually
//! grows into are allocated.
//!
//! Under pressure, [`BlockCache::shrink`] moves blocks down two tiers:
//!
//! - **Yellow compresses.** The coldest blocks nobody holds, clean and
//!   dirty, are compressed into the kernel's [`CompressedPool`] and their
//!   page buffers go back to the heap. The entry stays in the index,
//!   marked `COMPRESSED`, so a later hit decompresses it into a fresh
//!   buffer without asking the device. A dirty block stays dirty while it
//!   is compressed: it reaches the disk uncompressed, through the same
//!   write-back and [`CachedDisk::sync`] as any other dirty block, and
//!   `sync` decompresses one it covers before writing it. A block that
//!   does not compress below [`INCOMPRESSIBLE_BYTES`], or that the pool
//!   has no room for, skips the tier: a clean one is dropped and a dirty
//!   one is written back, as zswap and zram send such a page straight to
//!   the backing device.
//! - **Red empties the compressed tier.** A clean compressed block is
//!   dropped; a dirty one is decompressed and written back, then dropped
//!   by a later pass once it is clean. Clean blocks in buffers are
//!   dropped and dirty ones written back, as before.
//!
//! The miss path's clock may also recycle a clean compressed entry, as
//! it recycles any clean one; it does not compress the block it
//! recycles, because a miss under pressure needs the entry, and the
//! reclaim pass is what moves blocks into the pool.

extern crate alloc;

use alloc::alloc::{alloc, dealloc, handle_alloc_error};
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};

use arrayvec::ArrayVec;
use crossbeam_utils::CachePadded;
use helios_hal::cpu::current_processor;
use helios_hal::fs::{BlockGeometry, BlockSerial};
use helios_hal::io::IoError;
use helios_hal::pmm::PhysFrame;
use spin::{Mutex, Once};

use super::worker::{DiskQueue, Request};
use crate::memory::{CompressedPage, CompressedPool, INCOMPRESSIBLE_BYTES};
use crate::{MAX_BLOCK_DEVICES, PressureLevel, ProgressSignal};

/// Bytes in one cache block.
///
/// A page: the unit the kernel's allocators hand out, and the block size
/// both filesystems the cache serves are built around (EROFS's 4 KiB
/// blocks, btrfs's 4 KiB sectors and its nodes of whole sectors).
pub const CACHE_BLOCK_BYTES: usize = PhysFrame::SIZE;

/// Broadcast signals tasks waiting on an entry park on, hashed by entry.
///
/// Linux's page wait table has the same size (`PAGE_WAIT_TABLE_BITS`).
/// A collision only wakes a task that re-checks its entry and parks
/// again; it never loses a wake.
const WAIT_TABLE_SIZE: usize = 256;

/// Recently used blocks one processor remembers, as `bh_lru` does.
const LOOKASIDE_WAYS: usize = 16;

/// Entries allocated together when the cache grows.
const SEGMENT_ENTRIES: usize = 512;

/// Low bits of a key that name the disk.
const DISK_BITS: u32 = MAX_BLOCK_DEVICES.next_power_of_two().trailing_zeros();
const DISK_MASK: u64 = (1 << DISK_BITS) - 1;

/// The key of an entry that holds no block.
const NO_KEY: u64 = u64::MAX;

/// Marks a chain link as the end of a chain rather than an entry.
const NULLS: u32 = 1 << 31;
/// Ends the free list. Its bucket index exceeds every real one, so a
/// reader that follows a recycled entry onto the free list restarts.
const FREE_END: u32 = u32::MAX;
/// Most buckets the index may have, so `NULLS | bucket` never reaches
/// [`FREE_END`].
const MAX_BUCKETS: usize = 1 << 30;

const FIBONACCI: u64 = 0x9e37_79b9_7f4a_7c15;

// The entry state word.
//
// Pins keep an entry's identity: a pinned entry is never recycled.
// Shares are the pins that may also read the buffer; a writer needs
// none outstanding. The flags above them describe the buffer.
const PIN: u64 = 1;
const COUNT_BITS: u32 = 20;
const PIN_MASK: u64 = (1 << COUNT_BITS) - 1;
const SHARE: u64 = 1 << COUNT_BITS;
const SHARE_MASK: u64 = PIN_MASK << COUNT_BITS;
/// The buffer holds the block's bytes.
const VALID: u64 = 1 << 40;
/// One task owns the buffer: the read filling it, or a writer.
const LOCKED: u64 = 1 << 41;
/// The buffer differs from the disk.
const DIRTY: u64 = 1 << 42;
/// The last read of the block failed; cleared by the next attempt.
const FAILED: u64 = 1 << 43;
/// Touched since the clock hand last passed.
const REFERENCED: u64 = 1 << 44;
/// Owned by whoever is recycling it, or on the free list; no pin may
/// be taken.
const RECLAIM: u64 = 1 << 45;
/// A write of the buffer to the disk is in flight.
const WRITEBACK: u64 = 1 << 46;
/// A task is parked on this entry's wait signal.
const WAITERS: u64 = 1 << 47;
/// The block's bytes are held in the entry's compressed page and the
/// entry has no buffer. Exclusive of [`VALID`].
const COMPRESSED: u64 = 1 << 48;

const fn pins(state: u64) -> u64 {
    state & PIN_MASK
}

const fn shares(state: u64) -> u64 {
    (state & SHARE_MASK) >> COUNT_BITS
}

/// Whether the clock may take this entry: nobody holds it and its
/// buffer agrees with the disk.
const fn evictable(state: u64) -> bool {
    pins(state) == 0 && state & (LOCKED | DIRTY | WRITEBACK | RECLAIM) == 0
}

/// Whether a reclaim pass may move this entry: nobody holds it and no
/// read, writer or write-back owns its buffer. Dirty entries qualify.
const fn idle(state: u64) -> bool {
    pins(state) == 0 && state & (LOCKED | WRITEBACK | RECLAIM) == 0
}

const fn buffer_layout() -> Layout {
    match Layout::from_size_align(CACHE_BLOCK_BYTES, CACHE_BLOCK_BYTES) {
        Ok(layout) => layout,
        Err(_) => panic!("cache block layout is invalid"),
    }
}

fn allocate_buffer() -> *mut u8 {
    let layout = buffer_layout();
    // SAFETY: the layout has a non-zero size.
    let memory = unsafe { alloc(layout) };
    if memory.is_null() {
        handle_alloc_error(layout);
    }
    memory
}

/// What the cache could not do with a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BlockCacheError {
    #[error("block {block} is past the end of a disk of {capacity_blocks} cache blocks")]
    OutOfRange { block: u64, capacity_blocks: u64 },
    #[error("byte range {offset}+{len} is past the end of a disk of {capacity_bytes} bytes")]
    ByteRangeOutOfRange {
        offset: u64,
        len: usize,
        capacity_bytes: u64,
    },
    #[error("the disk refuses writes")]
    ReadOnly,
    #[error("the device failed: {0}")]
    Device(IoError),
}

/// Why a disk cannot be put behind the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CacheLayoutError {
    #[error(
        "logical blocks of {logical_block_bytes} bytes do not divide the {CACHE_BLOCK_BYTES}-byte cache block"
    )]
    UnsupportedBlockSize { logical_block_bytes: usize },
    #[error("the disk holds less than one {CACHE_BLOCK_BYTES}-byte cache block")]
    TooSmall,
}

/// How one disk's logical blocks map onto cache blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DiskLayout {
    /// Logical blocks in one cache block.
    pub(super) device_blocks_per_block: usize,
    /// Whole cache blocks the disk holds. A tail shorter than a cache
    /// block is not addressable through the cache.
    pub(super) capacity_blocks: u64,
    /// Cache blocks one miss reads, aligned to their own size: the
    /// device's optimal transfer, or the missing block alone when the
    /// device states none.
    pub(super) readahead_blocks: u64,
}

impl DiskLayout {
    pub(super) fn new(geometry: BlockGeometry) -> Result<Self, CacheLayoutError> {
        let logical_block_bytes = geometry.logical_block_bytes;
        if logical_block_bytes == 0
            || logical_block_bytes > CACHE_BLOCK_BYTES
            || CACHE_BLOCK_BYTES % logical_block_bytes != 0
        {
            return Err(CacheLayoutError::UnsupportedBlockSize {
                logical_block_bytes,
            });
        }
        let device_blocks_per_block = CACHE_BLOCK_BYTES / logical_block_bytes;
        let capacity_blocks = (geometry.capacity_blocks / device_blocks_per_block) as u64;
        if capacity_blocks == 0 {
            return Err(CacheLayoutError::TooSmall);
        }
        let optimal_bytes = (geometry.opt_io_blocks as usize).saturating_mul(logical_block_bytes);
        let readahead_blocks =
            ((optimal_bytes / CACHE_BLOCK_BYTES) as u64).clamp(1, capacity_blocks);
        Ok(Self {
            device_blocks_per_block,
            capacity_blocks,
            readahead_blocks,
        })
    }
}

/// How large the cache may grow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockCacheLimits {
    /// Most blocks the cache may hold at once.
    pub max_blocks: usize,
}

impl BlockCacheLimits {
    /// The bound a machine of `usable_bytes` implies: the cache grows
    /// only while at least half the machine is free, so it cannot come
    /// to hold more than the other half.
    pub fn for_machine(usable_bytes: usize) -> Self {
        Self {
            max_blocks: (usable_bytes / 2 / CACHE_BLOCK_BYTES).max(1),
        }
    }
}

/// A wait for a condition other tasks publish, whose publishers pay for
/// a wake only while someone waits.
///
/// The waiter registers, fences, then tests; the publisher makes its
/// change with a sequentially consistent operation (or fences after
/// it) and then reads the registration count. Whichever comes second
/// in the single order of those operations sees the other, so a change
/// is never missed and a change nobody waits for costs one load.
pub(super) struct Gate {
    signal: ProgressSignal,
    waiters: AtomicUsize,
}

struct Registered<'a>(&'a AtomicUsize);

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Gate {
    pub(super) const fn new() -> Self {
        Self {
            signal: ProgressSignal::new(),
            waiters: AtomicUsize::new(0),
        }
    }

    /// Parks until `ready` answers true. `ready` must read what it tests
    /// with sequentially consistent loads or operations.
    pub(super) async fn wait_until(&self, mut ready: impl FnMut() -> bool) {
        self.waiters.fetch_add(1, Ordering::SeqCst);
        let _registered = Registered(&self.waiters);
        fence(Ordering::SeqCst);
        loop {
            let mark = self.signal.mark();
            if ready() {
                return;
            }
            self.signal.changed(mark).await;
        }
    }

    /// Wakes the waiters, if there are any. The caller's change must be
    /// sequentially consistent or followed by a `SeqCst` fence.
    pub(super) fn open(&self) {
        if self.waiters.load(Ordering::SeqCst) != 0 {
            self.signal.signal();
        }
    }
}

/// One cached block's metadata. Type-stable: an entry is reused for
/// other blocks but never freed while the cache lives.
struct Entry {
    /// `block << DISK_BITS | disk`, or [`NO_KEY`].
    key: AtomicU64,
    state: AtomicU64,
    /// The next entry in this entry's chain or on the free list, or a
    /// nulls marker.
    next: AtomicU32,
    /// A [`CACHE_BLOCK_BYTES`] kernel-heap buffer, or null. Written only
    /// by the task that owns the entry under [`RECLAIM`] or [`LOCKED`].
    buffer: AtomicPtr<u8>,
    /// The block compressed, while the state says [`COMPRESSED`]. Read
    /// and written only by the task that owns the entry under
    /// [`RECLAIM`] or [`LOCKED`].
    compressed: UnsafeCell<Option<CompressedPage>>,
}

// SAFETY: every field but `compressed` is atomic, and `compressed` is
// touched only by the one task that owns the entry through its state
// word, whose acquiring and releasing operations order the accesses.
unsafe impl Sync for Entry {}

impl Entry {
    const fn free() -> Self {
        Self {
            key: AtomicU64::new(NO_KEY),
            state: AtomicU64::new(RECLAIM),
            next: AtomicU32::new(FREE_END),
            buffer: AtomicPtr::new(core::ptr::null_mut()),
            compressed: UnsafeCell::new(None),
        }
    }

    /// Takes the compressed page out of an entry the caller owns.
    ///
    /// # Safety
    ///
    /// The caller owns the entry under [`RECLAIM`] or [`LOCKED`].
    unsafe fn take_compressed(&self) -> Option<CompressedPage> {
        // SAFETY: the caller's ownership excludes every other access.
        unsafe { (*self.compressed.get()).take() }
    }

    /// Gives an entry the caller owns a compressed page.
    ///
    /// # Safety
    ///
    /// The caller owns the entry under [`LOCKED`].
    unsafe fn store_compressed(&self, page: CompressedPage) {
        // SAFETY: the caller's ownership excludes every other access.
        unsafe { *self.compressed.get() = Some(page) };
    }

    /// Frees the buffer of an entry the caller owns.
    ///
    /// # Safety
    ///
    /// The caller owns the entry under [`RECLAIM`] or [`LOCKED`], and no
    /// reference into the buffer outlives the call.
    unsafe fn free_buffer(&self) {
        let buffer = self.buffer.swap(core::ptr::null_mut(), Ordering::Relaxed);
        if !buffer.is_null() {
            // SAFETY: the buffer came from `allocate_buffer` and the
            // entry no longer names it.
            unsafe { dealloc(buffer, buffer_layout()) };
        }
    }
}

/// How a lookup holds the entry it finds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Take {
    /// A pin alone: identity, not contents.
    Pin,
    /// A pin and a shared read, granted only on a valid, unlocked entry.
    Share,
}

enum Insert {
    Inserted,
    Existing(u32),
}

/// What [`CacheShared::write_back_entry`] did.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteBack {
    Started,
    /// The entry was not dirty in a buffer, or someone held it.
    Skipped,
    /// The disk's request ring had no room.
    RingFull,
}

pub(super) struct CacheShared {
    segments: Box<[Once<Box<[CachePadded<Entry>]>>]>,
    max_entries: u32,
    /// Entries ever created; ids below it name a live segment.
    allocated: CachePadded<AtomicU32>,
    /// Treiber stack of free entries: `tag << 32 | id`.
    free: CachePadded<AtomicU64>,
    buckets: Box<[AtomicU32]>,
    bucket_shift: u32,
    stripes: Box<[CachePadded<Mutex<()>>]>,
    /// Indexed by processor; row `p` is written only by tasks running on
    /// processor `p`.
    lookaside: Box<[CachePadded<[AtomicU32; LOOKASIDE_WAYS]>]>,
    waits: Box<[CachePadded<ProgressSignal>]>,
    /// The clock hand, advanced by every sweep on any processor.
    hand: CachePadded<AtomicU32>,
    /// Raised when an entry becomes evictable, for a miss that found the
    /// cache full of pinned, dirty or in-flight blocks.
    reclaimable: Gate,
    /// The last [`PressureLevel`] the reclaim task measured.
    pressure: AtomicU8,
    /// The tier cold blocks are compressed into under pressure.
    pool: CompressedPool,
    pub(super) disks: ArrayVec<DiskQueue, MAX_BLOCK_DEVICES>,
}

impl CacheShared {
    fn new(
        limits: BlockCacheLimits,
        processors: usize,
        pool: CompressedPool,
        disks: ArrayVec<DiskQueue, MAX_BLOCK_DEVICES>,
    ) -> Self {
        assert!(
            processors != 0,
            "the block cache needs at least one processor"
        );
        let max_entries = u32::try_from(limits.max_blocks.max(1))
            .unwrap_or(u32::MAX)
            .min(NULLS - 1);
        let segment_count = (max_entries as usize).div_ceil(SEGMENT_ENTRIES);
        // Writers to one cache line of bucket heads all hold the same
        // stripe lock, so the line count per stripe is what matters.
        let buckets_per_line =
            (core::mem::size_of::<CachePadded<u8>>() / core::mem::size_of::<AtomicU32>()).max(1);
        let bucket_count = (max_entries as usize / 2)
            .next_power_of_two()
            .clamp(buckets_per_line, MAX_BUCKETS);
        let stripe_count = processors.saturating_mul(4).next_power_of_two();
        Self {
            segments: (0..segment_count).map(|_| Once::new()).collect(),
            max_entries,
            allocated: CachePadded::new(AtomicU32::new(0)),
            free: CachePadded::new(AtomicU64::new(u64::from(FREE_END))),
            buckets: (0..bucket_count)
                .map(|bucket| AtomicU32::new(NULLS | bucket as u32))
                .collect(),
            bucket_shift: u64::BITS - bucket_count.trailing_zeros(),
            stripes: (0..stripe_count)
                .map(|_| CachePadded::new(Mutex::new(())))
                .collect(),
            lookaside: (0..processors)
                .map(|_| CachePadded::new([const { AtomicU32::new(0) }; LOOKASIDE_WAYS]))
                .collect(),
            waits: (0..WAIT_TABLE_SIZE)
                .map(|_| CachePadded::new(ProgressSignal::new()))
                .collect(),
            hand: CachePadded::new(AtomicU32::new(0)),
            reclaimable: Gate::new(),
            pressure: AtomicU8::new(PressureLevel::Green as u8),
            pool,
            disks,
        }
    }

    pub(super) fn entry_key(&self, id: u32) -> u64 {
        self.entry(id).key.load(Ordering::Acquire)
    }

    fn entry(&self, id: u32) -> &Entry {
        let segment = self.segments[id as usize / SEGMENT_ENTRIES]
            .get()
            .unwrap_or_else(|| {
                panic!("block cache entry {id} was named before its segment exists")
            });
        &segment[id as usize % SEGMENT_ENTRIES]
    }

    fn hash(key: u64) -> u64 {
        key.wrapping_mul(FIBONACCI)
    }

    fn bucket_of(&self, key: u64) -> usize {
        (Self::hash(key) >> self.bucket_shift) as usize
    }

    fn stripe(&self, bucket: usize) -> &Mutex<()> {
        let buckets_per_line =
            (core::mem::size_of::<CachePadded<u8>>() / core::mem::size_of::<AtomicU32>()).max(1);
        &self.stripes[(bucket / buckets_per_line) & (self.stripes.len() - 1)]
    }

    fn wait_signal(&self, id: u32) -> &ProgressSignal {
        &self.waits
            [(Self::hash(u64::from(id)) >> (u64::BITS - WAIT_TABLE_SIZE.trailing_zeros())) as usize]
    }

    fn lookaside_slot(&self, key: u64) -> &AtomicU32 {
        let processor = current_processor().id() as usize;
        let row = self.lookaside.get(processor).unwrap_or_else(|| {
            panic!(
                "processor {processor} has no block cache lookaside among the {} configured",
                self.lookaside.len()
            )
        });
        &row[(Self::hash(key) >> (u64::BITS - LOOKASIDE_WAYS.trailing_zeros())) as usize]
    }

    fn pressure(&self) -> PressureLevel {
        match self.pressure.load(Ordering::Relaxed) {
            level if level == PressureLevel::Green as u8 => PressureLevel::Green,
            level if level == PressureLevel::Yellow as u8 => PressureLevel::Yellow,
            _ => PressureLevel::Red,
        }
    }

    /// Takes `entry` as `take` asks, unless it is being recycled or,
    /// for a share, its contents are not readable yet.
    fn try_take(&self, id: u32, take: Take) -> bool {
        let state = &self.entry(id).state;
        let mut current = state.load(Ordering::Relaxed);
        loop {
            if current & RECLAIM != 0 {
                return false;
            }
            let next = match take {
                Take::Pin => current + PIN,
                Take::Share => {
                    if current & VALID == 0 || current & LOCKED != 0 {
                        return false;
                    }
                    (current + PIN + SHARE) | REFERENCED
                }
            };
            match state.compare_exchange_weak(current, next, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn release(&self, id: u32, take: Take) {
        match take {
            Take::Pin => self.unpin(id),
            Take::Share => {
                self.transition(id, |state| state - SHARE - PIN);
            }
        }
    }

    /// Drops a pin. Nobody waits on a pin count, so only the clock hears
    /// about it.
    pub(super) fn unpin(&self, id: u32) {
        let previous = self.entry(id).state.fetch_sub(PIN, Ordering::SeqCst);
        if evictable(previous - PIN) {
            self.reclaimable.open();
        }
    }

    /// Applies `change` to the state word and wakes whoever waits on the
    /// entry. Every release goes through here; an acquisition never
    /// does, because it changes nothing a waiter waits for.
    pub(super) fn transition(&self, id: u32, change: impl Fn(u64) -> u64) -> u64 {
        let state = &self.entry(id).state;
        let mut current = state.load(Ordering::Relaxed);
        loop {
            let next = change(current) & !WAITERS;
            match state.compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::Relaxed) {
                Ok(previous) => {
                    if previous & WAITERS != 0 {
                        self.wait_signal(id).signal();
                    }
                    if evictable(next) {
                        self.reclaimable.open();
                    }
                    return next;
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Parks until the state satisfies `ready`, and returns it. The
    /// caller holds a pin.
    async fn wait_state(&self, id: u32, ready: impl Fn(u64) -> bool) -> u64 {
        let state = &self.entry(id).state;
        let signal = self.wait_signal(id);
        loop {
            let mark = signal.mark();
            let current = state.load(Ordering::SeqCst);
            if ready(current) {
                return current;
            }
            let current = state.fetch_or(WAITERS, Ordering::SeqCst);
            if ready(current) {
                return current;
            }
            signal.changed(mark).await;
        }
    }

    fn lookaside_find(&self, key: u64, take: Take) -> Option<u32> {
        let id = self
            .lookaside_slot(key)
            .load(Ordering::Relaxed)
            .checked_sub(1)?;
        let entry = self.entry(id);
        if entry.key.load(Ordering::Relaxed) != key || !self.try_take(id, take) {
            return None;
        }
        if entry.key.load(Ordering::Acquire) != key {
            self.release(id, take);
            return None;
        }
        Some(id)
    }

    fn remember(&self, key: u64, id: u32) {
        self.lookaside_slot(key).store(id + 1, Ordering::Relaxed);
    }

    /// The lock-free lookup: walks the key's chain and takes the first
    /// entry that still carries the key once taken.
    fn find(&self, key: u64, take: Take) -> Option<u32> {
        let bucket = self.bucket_of(key);
        'restart: loop {
            let mut cursor = self.buckets[bucket].load(Ordering::Acquire);
            while cursor & NULLS == 0 {
                let entry = self.entry(cursor);
                if entry.key.load(Ordering::Acquire) == key && self.try_take(cursor, take) {
                    if entry.key.load(Ordering::Acquire) == key {
                        return Some(cursor);
                    }
                    self.release(cursor, take);
                    continue 'restart;
                }
                cursor = entry.next.load(Ordering::Acquire);
            }
            if cursor != (NULLS | bucket as u32) {
                // The walk followed a recycled entry into another chain
                // or onto the free list.
                continue 'restart;
            }
            return None;
        }
    }

    /// Links the claimed entry `id` under `key`, unless another miss got
    /// there first, in which case its entry comes back pinned.
    fn insert(&self, key: u64, id: u32) -> Insert {
        let bucket = self.bucket_of(key);
        let _stripe = self.stripe(bucket).lock();
        let head = self.buckets[bucket].load(Ordering::Relaxed);
        let mut cursor = head;
        while cursor & NULLS == 0 {
            let entry = self.entry(cursor);
            // A matching entry under recycle is about to leave the
            // chain; it is not the block any more.
            if entry.key.load(Ordering::Acquire) == key && self.try_take(cursor, Take::Pin) {
                return Insert::Existing(cursor);
            }
            cursor = entry.next.load(Ordering::Relaxed);
        }
        let entry = self.entry(id);
        entry.key.store(key, Ordering::Relaxed);
        entry.next.store(head, Ordering::Relaxed);
        // The owner of a `RECLAIM` entry is the only writer of its state,
        // so this store loses no other processor's update.
        entry.state.store(PIN | LOCKED, Ordering::Release);
        self.buckets[bucket].store(id, Ordering::Release);
        Insert::Inserted
    }

    /// Takes the recycled entry `id` out of its chain.
    fn unlink(&self, id: u32) {
        let entry = self.entry(id);
        let key = entry.key.load(Ordering::Acquire);
        let bucket = self.bucket_of(key);
        let _stripe = self.stripe(bucket).lock();
        let successor = entry.next.load(Ordering::Relaxed);
        let mut link = &self.buckets[bucket];
        loop {
            let cursor = link.load(Ordering::Relaxed);
            if cursor == id {
                link.store(successor, Ordering::Release);
                break;
            }
            assert!(
                cursor & NULLS == 0,
                "block cache entry {id} with key {key:#x} is missing from bucket {bucket}"
            );
            link = &self.entry(cursor).next;
        }
        entry.key.store(NO_KEY, Ordering::Release);
    }

    fn push_free(&self, id: u32) {
        let entry = self.entry(id);
        let mut head = self.free.load(Ordering::Acquire);
        loop {
            entry.next.store(head as u32, Ordering::Relaxed);
            let next = ((head >> 32).wrapping_add(1) << 32) | u64::from(id);
            match self
                .free
                .compare_exchange_weak(head, next, Ordering::Release, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(observed) => head = observed,
            }
        }
    }

    fn pop_free(&self) -> Option<u32> {
        let mut head = self.free.load(Ordering::Acquire);
        loop {
            let id = head as u32;
            if id == FREE_END {
                return None;
            }
            let successor = self.entry(id).next.load(Ordering::Relaxed);
            let next = ((head >> 32).wrapping_add(1) << 32) | u64::from(successor);
            match self
                .free
                .compare_exchange_weak(head, next, Ordering::Acquire, Ordering::Acquire)
            {
                Ok(_) => return Some(id),
                Err(observed) => head = observed,
            }
        }
    }

    /// Creates one more entry, up to the bound.
    fn grow(&self) -> Option<u32> {
        let mut allocated = self.allocated.load(Ordering::Relaxed);
        loop {
            if allocated >= self.max_entries {
                return None;
            }
            // The segment exists before any id in it is counted, so a
            // sweep that reads the count never names a missing segment.
            self.segments[allocated as usize / SEGMENT_ENTRIES].call_once(|| {
                (0..SEGMENT_ENTRIES)
                    .map(|_| CachePadded::new(Entry::free()))
                    .collect()
            });
            match self.allocated.compare_exchange_weak(
                allocated,
                allocated + 1,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(allocated),
                Err(observed) => allocated = observed,
            }
        }
    }

    /// Takes the coldest evictable entry out of the index, giving
    /// referenced entries a second chance.
    fn evict_one(&self) -> Option<u32> {
        let allocated = self.allocated.load(Ordering::Acquire);
        if allocated == 0 {
            return None;
        }
        for _ in 0..u64::from(allocated) * 2 {
            let id = self.hand.fetch_add(1, Ordering::Relaxed) % allocated;
            let state = &self.entry(id).state;
            let current = state.load(Ordering::SeqCst);
            if !evictable(current) {
                continue;
            }
            if current & REFERENCED != 0 {
                let _ = state.compare_exchange(
                    current,
                    current & !REFERENCED,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                continue;
            }
            if state
                .compare_exchange(current, RECLAIM, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                self.unlink(id);
                // A clean compressed block leaves the pool with its entry.
                // SAFETY: this task owns the entry under `RECLAIM`.
                drop(unsafe { self.entry(id).take_compressed() });
                return Some(id);
            }
        }
        None
    }

    /// An entry this caller owns under [`RECLAIM`], with a buffer.
    ///
    /// Green pressure reuses a free entry or grows; otherwise the clock
    /// recycles a clean block first. An `urgent` claim — a reader that
    /// cannot proceed without one — grows under pressure when nothing
    /// is evictable; a speculative one does not.
    fn try_claim(&self, urgent: bool) -> Option<u32> {
        let id = if self.pressure() == PressureLevel::Green {
            self.pop_free()
                .or_else(|| self.grow())
                .or_else(|| self.evict_one())
        } else {
            self.evict_one().or_else(|| {
                if urgent {
                    self.pop_free().or_else(|| self.grow())
                } else {
                    None
                }
            })
        }?;
        let buffer = &self.entry(id).buffer;
        if buffer.load(Ordering::Relaxed).is_null() {
            buffer.store(allocate_buffer(), Ordering::Relaxed);
        }
        Some(id)
    }

    async fn claim(&self) -> u32 {
        if let Some(id) = self.try_claim(true) {
            return id;
        }
        // Every entry is pinned, dirty or in flight and the cache is at
        // its bound: write back what nobody holds and wait for the first
        // entry to come free.
        self.write_back_idle();
        let mut claimed = None;
        self.reclaimable
            .wait_until(|| {
                claimed = self.try_claim(true);
                claimed.is_some()
            })
            .await;
        claimed.unwrap_or_else(|| panic!("block cache claim returned without an entry"))
    }

    /// Moves the pinned, dirty entry `id` into write-back. The caller
    /// submits the request the transition stands for.
    fn start_writeback(&self, id: u32) -> bool {
        let state = &self.entry(id).state;
        let mut current = state.load(Ordering::Acquire);
        loop {
            // A compressed block is decompressed before it is written.
            if current & DIRTY == 0 || current & (LOCKED | WRITEBACK | RECLAIM | COMPRESSED) != 0 {
                return false;
            }
            let next = ((current & !DIRTY) | WRITEBACK) + SHARE + PIN;
            match state.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    /// Puts an entry whose write-back could not be submitted back into
    /// the dirty set.
    fn cancel_writeback(&self, id: u32) {
        self.transition(id, |state| ((state & !WRITEBACK) | DIRTY) - SHARE - PIN);
    }

    /// Starts the write-back of every dirty block nobody holds, as far
    /// as the disks' request rings have room, decompressing the ones
    /// held compressed. Never waits.
    fn write_back_idle(&self) -> usize {
        let allocated = self.allocated.load(Ordering::Acquire);
        let mut started = 0;
        for id in 0..allocated {
            match self.write_back_dirty(id) {
                WriteBack::Started => started += 1,
                WriteBack::Skipped => {}
                WriteBack::RingFull => break,
            }
        }
        started
    }

    /// Starts the write-back of the entry `id` if it is dirty and nobody
    /// holds it, decompressing it first when it is held compressed.
    fn write_back_dirty(&self, id: u32) -> WriteBack {
        let state = self.entry(id).state.load(Ordering::Relaxed);
        if state & (COMPRESSED | DIRTY) == COMPRESSED | DIRTY && idle(state) {
            if !self.try_take(id, Take::Pin) {
                return WriteBack::Skipped;
            }
            let inflated = self.try_inflate(id);
            self.unpin(id);
            if !inflated {
                return WriteBack::Skipped;
            }
        }
        self.write_back_entry(id)
    }

    /// Starts the write-back of the entry `id` if it is dirty, in a
    /// buffer, and nobody holds it. Never waits.
    fn write_back_entry(&self, id: u32) -> WriteBack {
        let entry = self.entry(id);
        let state = entry.state.load(Ordering::Relaxed);
        if state & DIRTY == 0
            || state & (LOCKED | WRITEBACK | RECLAIM | COMPRESSED) != 0
            || pins(state) != 0
            || !self.try_take(id, Take::Pin)
        {
            return WriteBack::Skipped;
        }
        let key = entry.key.load(Ordering::Acquire);
        let mut outcome = WriteBack::Skipped;
        if key != NO_KEY && self.start_writeback(id) {
            if self.disks[key_disk(key)].try_submit(Request::Write(id)) {
                outcome = WriteBack::Started;
            } else {
                self.cancel_writeback(id);
                outcome = WriteBack::RingFull;
            }
        }
        self.unpin(id);
        outcome
    }

    /// Decompresses the compressed entry `id` into a fresh buffer, so it
    /// is valid again with its dirtiness unchanged. Answers false, doing
    /// nothing, when the entry is not compressed or another task owns
    /// it; that task's release wakes anyone waiting on the entry.
    ///
    /// The caller holds a pin.
    fn try_inflate(&self, id: u32) -> bool {
        let entry = self.entry(id);
        let mut current = entry.state.load(Ordering::Acquire);
        loop {
            if current & COMPRESSED == 0 || current & (LOCKED | RECLAIM) != 0 {
                return false;
            }
            match entry.state.compare_exchange_weak(
                current,
                current | LOCKED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        // SAFETY: this task owns the entry under `LOCKED`.
        let page = unsafe { entry.take_compressed() }
            .unwrap_or_else(|| panic!("block cache entry {id} is compressed without a page"));
        let buffer = allocate_buffer();
        // SAFETY: the buffer is fresh, page-sized and aligned, and this
        // task is its only user.
        page.decompress_into(unsafe {
            &mut *buffer.cast::<[MaybeUninit<u8>; CACHE_BLOCK_BYTES]>()
        });
        drop(page);
        entry.buffer.store(buffer, Ordering::Relaxed);
        self.transition(id, |state| (state & !(LOCKED | COMPRESSED)) | VALID);
        true
    }

    /// Compresses the idle, valid entry `id`, last seen in state
    /// `current`, into the pool and frees its buffer. A block the pool
    /// refuses skips the tier: a clean one is dropped, a dirty one
    /// written back. Answers whether the entry's buffer was freed.
    fn demote(&self, id: u32, current: u64) -> bool {
        let entry = self.entry(id);
        if entry
            .state
            .compare_exchange(
                current,
                current | LOCKED,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return false;
        }
        // SAFETY: this task owns the entry under `LOCKED`, so nobody
        // writes the buffer while it is compressed.
        let bytes = unsafe { &*self.buffer(id).cast::<[u8; CACHE_BLOCK_BYTES]>() };
        match self.pool.compress(bytes) {
            Ok(page) => {
                // SAFETY: this task owns the entry under `LOCKED`, and
                // `bytes` is not used past this point.
                unsafe {
                    entry.store_compressed(page);
                    entry.free_buffer();
                }
                self.transition(id, |state| {
                    (state & !(LOCKED | VALID | REFERENCED)) | COMPRESSED
                });
                true
            }
            Err(_) => {
                let state = self.transition(id, |state| state & !LOCKED);
                if state & DIRTY != 0 {
                    self.write_back_entry(id);
                    false
                } else {
                    self.drop_entry(id, state)
                }
            }
        }
    }

    /// Drops the idle, clean entry `id`, last seen in state `current`,
    /// from the cache, freeing its buffer or its compressed page.
    /// Answers whether it did.
    fn drop_entry(&self, id: u32, current: u64) -> bool {
        if !evictable(current)
            || self
                .entry(id)
                .state
                .compare_exchange(current, RECLAIM, Ordering::SeqCst, Ordering::Relaxed)
                .is_err()
        {
            return false;
        }
        self.unlink(id);
        let entry = self.entry(id);
        // SAFETY: this task owns the entry under `RECLAIM`, and nothing
        // that reaches the entry any more can name its buffer.
        unsafe {
            drop(entry.take_compressed());
            entry.free_buffer();
        }
        self.push_free(id);
        true
    }

    /// Takes the compressed entry `id`, last seen in state `current`,
    /// out of the pool: dropped when it is clean, decompressed and
    /// written back when it is dirty, for a later pass to drop. Answers
    /// whether the entry left the cache.
    fn evict_compressed(&self, id: u32, current: u64) -> bool {
        if current & DIRTY == 0 {
            return self.drop_entry(id, current);
        }
        self.write_back_dirty(id);
        false
    }

    /// A finished read: the buffer is valid or the read failed.
    pub(super) fn complete_read(&self, id: u32, succeeded: bool) {
        self.transition(id, |state| {
            let state = (state & !LOCKED) - PIN;
            if succeeded {
                (state | VALID) & !FAILED
            } else {
                (state | FAILED) & !VALID
            }
        });
    }

    /// A finished write-back. A failed one leaves the block dirty, so
    /// the next sync tries again and reports the failure.
    pub(super) fn complete_write(&self, id: u32, succeeded: bool) {
        self.transition(id, |state| {
            let state = (state & !WRITEBACK) - SHARE - PIN;
            if succeeded { state } else { state | DIRTY }
        });
    }

    /// The buffer of an entry its caller holds.
    pub(super) fn buffer(&self, id: u32) -> *mut u8 {
        self.entry(id).buffer.load(Ordering::Acquire)
    }

    /// One reclaim pass; see the module's *Memory* section. Answers the
    /// page buffers it gave back to the heap.
    fn shrink(&self, level: PressureLevel) -> usize {
        let allocated = self.allocated.load(Ordering::Acquire);
        let budget = match level {
            PressureLevel::Green => return 0,
            PressureLevel::Yellow => (allocated as usize / 8).max(1),
            PressureLevel::Red => allocated as usize,
        };
        let mut released = 0;
        for _ in 0..allocated {
            if released == budget {
                break;
            }
            let id = self.hand.fetch_add(1, Ordering::Relaxed) % allocated;
            let state = &self.entry(id).state;
            let current = state.load(Ordering::SeqCst);
            if !idle(current) {
                continue;
            }
            if current & COMPRESSED != 0 {
                // Compressed blocks hold no buffer; red empties the tier.
                if level == PressureLevel::Red {
                    self.evict_compressed(id, current);
                }
                continue;
            }
            let freed = if current & VALID == 0 {
                // A failed read: nothing worth keeping.
                self.drop_entry(id, current)
            } else if level == PressureLevel::Red {
                // Dirty blocks are written back below, for a later pass.
                current & DIRTY == 0 && self.drop_entry(id, current)
            } else if current & REFERENCED != 0 {
                let _ = state.compare_exchange(
                    current,
                    current & !REFERENCED,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                false
            } else {
                self.demote(id, current)
            };
            if freed {
                released += 1;
            }
        }
        if level == PressureLevel::Red {
            // Dirty blocks are not reclaimable until they are written;
            // start that now so the next pass can take them.
            self.write_back_idle();
        }
        released
    }

    fn resident_blocks(&self) -> usize {
        let allocated = self.allocated.load(Ordering::Acquire);
        (0..allocated)
            .filter(|&id| self.entry(id).state.load(Ordering::Relaxed) & RECLAIM == 0)
            .count()
    }

    fn compressed_blocks(&self) -> usize {
        let allocated = self.allocated.load(Ordering::Acquire);
        (0..allocated)
            .filter(|&id| {
                self.entry(id).state.load(Ordering::Relaxed) & (COMPRESSED | RECLAIM) == COMPRESSED
            })
            .count()
    }
}

impl Drop for CacheShared {
    fn drop(&mut self) {
        let allocated = *self.allocated.get_mut();
        for id in 0..allocated {
            let buffer = self.entry(id).buffer.load(Ordering::Relaxed);
            if !buffer.is_null() {
                // SAFETY: every buffer came from `alloc` with this layout,
                // and nothing can reach the cache any more.
                unsafe { dealloc(buffer, buffer_layout()) };
            }
        }
    }
}

/// What the cache knows about one disk it serves.
pub(super) struct CacheDisk {
    pub(super) serial: BlockSerial,
    pub(super) layout: DiskLayout,
    pub(super) read_only: bool,
    /// Requests the disk's ring holds before a submitter waits.
    pub(super) queue_capacity: usize,
}

/// The kernel's block cache, shared by every mount-source disk.
#[derive(Clone)]
pub struct BlockCache {
    shared: Arc<CacheShared>,
}

impl BlockCache {
    /// A cache over `disks`, in the order their worker tasks name them.
    pub(super) fn new(
        limits: BlockCacheLimits,
        processors: usize,
        pool: CompressedPool,
        disks: impl IntoIterator<Item = CacheDisk>,
    ) -> Self {
        let queues = disks.into_iter().map(DiskQueue::new).collect();
        Self {
            shared: Arc::new(CacheShared::new(limits, processors, pool, queues)),
        }
    }

    /// The pool cold blocks are compressed into, whose limit and level
    /// are operator settings.
    pub fn compressed_pool(&self) -> &CompressedPool {
        &self.shared.pool
    }

    pub(super) fn shared(&self) -> &CacheShared {
        &self.shared
    }

    /// The view of one disk; `index` is its position in construction.
    pub(super) fn disk(&self, index: usize) -> CachedDisk {
        assert!(
            index < self.shared.disks.len(),
            "block cache has no disk {index}"
        );
        CachedDisk {
            cache: self.clone(),
            disk: index as u8,
        }
    }

    /// Every disk the cache serves.
    pub fn disks(&self) -> impl Iterator<Item = CachedDisk> + '_ {
        (0..self.shared.disks.len()).map(|index| self.disk(index))
    }

    /// Records the pressure the machine is under. Growth stops at
    /// anything above green.
    pub fn set_pressure(&self, level: PressureLevel) {
        self.shared.pressure.store(level as u8, Ordering::Relaxed);
    }

    /// Gives page buffers nobody holds back to the kernel heap. Under
    /// yellow pressure the coldest eighth of the cache is compressed into
    /// the [`CompressedPool`], or dropped or written back when the pool
    /// refuses it; under red every clean block is dropped, the
    /// compressed tier with it, and idle dirty blocks are sent to the
    /// disk so the next pass can take them. Returns the buffers freed.
    pub fn shrink(&self, level: PressureLevel) -> usize {
        self.shared.shrink(level)
    }

    /// Blocks the cache holds right now, in buffers or compressed.
    pub fn resident_blocks(&self) -> usize {
        self.shared.resident_blocks()
    }

    /// Blocks the cache holds compressed right now.
    pub fn compressed_blocks(&self) -> usize {
        self.shared.compressed_blocks()
    }
}

/// Request and failure counts for one cached disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CachedDiskStats {
    pub misses: u64,
    pub device_reads: u64,
    pub readahead: u64,
    pub device_writes: u64,
    pub flushes: u64,
    pub read_errors: u64,
    pub write_errors: u64,
}

/// One mount-source disk, seen through the block cache.
///
/// Blocks are [`CACHE_BLOCK_BYTES`] long and numbered from the start of
/// the disk.
#[derive(Clone)]
pub struct CachedDisk {
    cache: BlockCache,
    disk: u8,
}

/// A block a [`CachedDisk::read`] holds; the bytes cannot change while
/// it lives.
pub struct BlockRef<'a> {
    disk: &'a CachedDisk,
    id: u32,
    bytes: &'a [u8; CACHE_BLOCK_BYTES],
}

/// A block a [`CachedDisk::write`] or [`CachedDisk::overwrite`] holds
/// exclusively. Dropping it leaves the block dirty.
pub struct BlockMut<'a> {
    disk: &'a CachedDisk,
    id: u32,
    bytes: *mut [u8; CACHE_BLOCK_BYTES],
}

// SAFETY: a `BlockMut` is a unique borrow of its buffer, like `&mut`.
unsafe impl Send for BlockMut<'_> {}
// SAFETY: shared access to a `BlockMut` only reads the buffer.
unsafe impl Sync for BlockMut<'_> {}

/// A pin this task holds, given back when it is dropped.
struct Held<'a> {
    cache: &'a CacheShared,
    id: u32,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.cache.unpin(self.id);
    }
}

/// A read this task locked an entry for, until the read is queued. If
/// the task is dropped first, the lock is given back, so waiters find
/// an entry they can load themselves instead of one locked forever.
struct PendingRead<'a> {
    cache: &'a CacheShared,
    id: u32,
}

impl Drop for PendingRead<'_> {
    fn drop(&mut self) {
        self.cache
            .transition(self.id, |state| (state & !LOCKED) - PIN);
    }
}

/// A write-back this task started, until it is queued.
struct PendingWrite<'a> {
    cache: &'a CacheShared,
    id: u32,
}

impl Drop for PendingWrite<'_> {
    fn drop(&mut self) {
        self.cache.cancel_writeback(self.id);
    }
}

impl CachedDisk {
    fn shared(&self) -> &CacheShared {
        &self.cache.shared
    }

    fn queue(&self) -> &DiskQueue {
        &self.shared().disks[self.disk as usize]
    }

    /// The serial the disk reported.
    pub fn serial(&self) -> BlockSerial {
        self.queue().serial
    }

    /// Cache blocks the disk holds.
    pub fn capacity_blocks(&self) -> u64 {
        self.queue().layout.capacity_blocks
    }

    /// Whether the disk refuses writes.
    pub fn is_read_only(&self) -> bool {
        self.queue().read_only
    }

    pub fn stats(&self) -> CachedDiskStats {
        self.queue().stats()
    }

    fn key(&self, block: u64) -> Result<u64, BlockCacheError> {
        let capacity_blocks = self.capacity_blocks();
        if block >= capacity_blocks {
            return Err(BlockCacheError::OutOfRange {
                block,
                capacity_blocks,
            });
        }
        Ok((block << DISK_BITS) | u64::from(self.disk))
    }

    fn writable(&self) -> Result<(), BlockCacheError> {
        if self.is_read_only() {
            return Err(BlockCacheError::ReadOnly);
        }
        Ok(())
    }

    fn device_error(&self) -> BlockCacheError {
        BlockCacheError::Device(self.queue().last_error())
    }

    /// The block, read from the disk if the cache does not hold it.
    pub async fn read(&self, block: u64) -> Result<BlockRef<'_>, BlockCacheError> {
        let key = self.key(block)?;
        let cache = self.shared();
        if let Some(id) = cache.lookaside_find(key, Take::Share) {
            return Ok(self.block_ref(id));
        }
        if let Some(id) = cache.find(key, Take::Share) {
            cache.remember(key, id);
            return Ok(self.block_ref(id));
        }
        let (held, inserted) = self.hold(key).await;
        if inserted {
            self.submit_read(held.id).await;
            self.read_ahead(block);
        }
        cache.remember(key, held.id);
        loop {
            let state = cache.entry(held.id).state.load(Ordering::Acquire);
            if state & LOCKED != 0 {
                let state = cache.wait_state(held.id, |state| state & LOCKED == 0).await;
                if state & VALID == 0 && state & FAILED != 0 {
                    return Err(self.device_error());
                }
                continue;
            }
            if state & COMPRESSED != 0 {
                // A hit in the compressed tier: no device read.
                cache.try_inflate(held.id);
                continue;
            }
            if state & VALID == 0 {
                if cache
                    .entry(held.id)
                    .state
                    .compare_exchange(
                        state,
                        (state | LOCKED) & !FAILED,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    self.submit_read(held.id).await;
                }
                continue;
            }
            if cache
                .entry(held.id)
                .state
                .compare_exchange(
                    state,
                    (state + SHARE) | REFERENCED,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                // The guard takes over the pin.
                let id = held.id;
                core::mem::forget(held);
                return Ok(self.block_ref(id));
            }
        }
    }

    /// Reads `buf.len()` bytes starting at byte `offset` of the disk.
    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), BlockCacheError> {
        let capacity_bytes = self.capacity_blocks() * CACHE_BLOCK_BYTES as u64;
        let end = offset.checked_add(buf.len() as u64);
        if end.is_none_or(|end| end > capacity_bytes) {
            return Err(BlockCacheError::ByteRangeOutOfRange {
                offset,
                len: buf.len(),
                capacity_bytes,
            });
        }
        let mut copied = 0;
        while copied < buf.len() {
            let position = offset + copied as u64;
            let block = position / CACHE_BLOCK_BYTES as u64;
            let start = (position % CACHE_BLOCK_BYTES as u64) as usize;
            let len = (CACHE_BLOCK_BYTES - start).min(buf.len() - copied);
            let cached = self.read(block).await?;
            buf[copied..copied + len].copy_from_slice(&cached[start..start + len]);
            copied += len;
        }
        Ok(())
    }

    /// The block for modification, read from the disk first if the
    /// cache does not hold it.
    pub async fn write(&self, block: u64) -> Result<BlockMut<'_>, BlockCacheError> {
        self.writable()?;
        let key = self.key(block)?;
        let (held, inserted) = self.hold(key).await;
        if inserted {
            self.submit_read(held.id).await;
            let state = self
                .shared()
                .wait_state(held.id, |state| state & LOCKED == 0)
                .await;
            if state & VALID == 0 {
                return Err(self.device_error());
            }
        }
        self.exclusive(held, true).await
    }

    /// The block for modification without reading it: the caller is
    /// about to write every byte. It starts zeroed.
    pub async fn overwrite(&self, block: u64) -> Result<BlockMut<'_>, BlockCacheError> {
        self.writable()?;
        let key = self.key(block)?;
        let (held, inserted) = self.hold(key).await;
        if inserted {
            // Inserted locked: this task already owns the buffer.
            let id = held.id;
            core::mem::forget(held);
            return Ok(self.block_mut(id, true));
        }
        self.exclusive(held, false).await
    }

    /// Writes every block dirtied before the call and commits the
    /// device's write cache.
    pub async fn sync(&self) -> Result<(), BlockCacheError> {
        if self.is_read_only() {
            return Ok(());
        }
        let queue = self.queue();
        let errors_before = queue.write_errors();
        let cache = self.shared();
        let disk = usize::from(self.disk);
        let allocated = cache.allocated.load(Ordering::Acquire);
        // Every block dirty when the scan reaches it is queued. One under
        // a writer is waited for and queued afterwards, because its dirt
        // may predate the call.
        for id in 0..allocated {
            let entry = cache.entry(id);
            loop {
                if entry.state.load(Ordering::Acquire) & DIRTY == 0
                    || key_disk(entry.key.load(Ordering::Relaxed)) != disk
                    || !cache.try_take(id, Take::Pin)
                {
                    break;
                }
                let held = Held { cache, id };
                if key_disk(entry.key.load(Ordering::Acquire)) != disk {
                    break;
                }
                if cache.start_writeback(id) {
                    let pending = PendingWrite { cache, id };
                    queue.submit(Request::Write(id)).await;
                    core::mem::forget(pending);
                    break;
                }
                // A dirty block held compressed reaches the disk as its
                // uncompressed bytes.
                if cache.try_inflate(id) {
                    continue;
                }
                let state = cache.wait_state(id, |state| state & LOCKED == 0).await;
                drop(held);
                if state & DIRTY == 0 {
                    break;
                }
            }
        }
        // Wait for every write of this disk in flight, ours or not.
        for id in 0..allocated {
            let entry = cache.entry(id);
            if entry.state.load(Ordering::Acquire) & WRITEBACK == 0
                || key_disk(entry.key.load(Ordering::Relaxed)) != disk
                || !cache.try_take(id, Take::Pin)
            {
                continue;
            }
            let _held = Held { cache, id };
            if key_disk(entry.key.load(Ordering::Acquire)) == disk {
                cache.wait_state(id, |state| state & WRITEBACK == 0).await;
            }
        }
        self.flush().await?;
        if queue.write_errors() != errors_before {
            return Err(self.device_error());
        }
        Ok(())
    }

    /// The commit point of a filesystem transaction: every block dirtied
    /// before the call reaches stable storage, then `block`, filled by
    /// `fill` from its current contents, is written and committed.
    ///
    /// The device sees the dirty blocks, a flush, the commit block and a
    /// second flush, in that order, so a crash leaves either the old
    /// commit block or the new one with everything it refers to.
    pub async fn commit(
        &self,
        block: u64,
        fill: impl FnOnce(&mut [u8; CACHE_BLOCK_BYTES]),
    ) -> Result<(), BlockCacheError> {
        self.sync().await?;
        let queue = self.queue();
        let errors_before = queue.write_errors();
        let mut guard = self.write(block).await?;
        fill(&mut *guard);
        let id = guard.id;
        core::mem::forget(guard);
        let cache = self.shared();
        // Hand the buffer from the writer to the write: readable, not
        // dirty, with a share and a pin of the request's own. This
        // task's pin stays to wait on the write.
        cache.transition(id, |state| {
            ((state & !(LOCKED | FAILED | DIRTY)) | VALID | REFERENCED | WRITEBACK) + SHARE + PIN
        });
        let held = Held { cache, id };
        let pending = PendingWrite { cache, id };
        queue.submit(Request::Write(id)).await;
        core::mem::forget(pending);
        cache.wait_state(id, |state| state & WRITEBACK == 0).await;
        drop(held);
        if queue.write_errors() != errors_before {
            return Err(self.device_error());
        }
        self.flush().await
    }

    /// Commits the device's volatile write cache.
    async fn flush(&self) -> Result<(), BlockCacheError> {
        let queue = self.queue();
        let ticket = queue.request_flush();
        queue.submit(Request::Flush).await;
        if queue.wait_flush(ticket).await {
            Ok(())
        } else {
            Err(self.device_error())
        }
    }

    fn block_ref(&self, id: u32) -> BlockRef<'_> {
        // SAFETY: the caller holds a share, so no writer exists and the
        // buffer stays this block's until the guard drops.
        let bytes = unsafe { &*self.shared().buffer(id).cast::<[u8; CACHE_BLOCK_BYTES]>() };
        BlockRef {
            disk: self,
            id,
            bytes,
        }
    }

    fn block_mut(&self, id: u32, zero: bool) -> BlockMut<'_> {
        let bytes = self.shared().buffer(id).cast::<[u8; CACHE_BLOCK_BYTES]>();
        if zero {
            // SAFETY: the caller holds the entry locked, so this task is
            // the buffer's only user.
            unsafe { bytes.write_bytes(0, 1) };
        }
        BlockMut {
            disk: self,
            id,
            bytes,
        }
    }

    /// The entry for `key`, pinned, inserting it if the cache has none.
    /// An inserted entry comes back locked by this task and not valid.
    async fn hold(&self, key: u64) -> (Held<'_>, bool) {
        let cache = self.shared();
        if let Some(id) = cache.find(key, Take::Pin) {
            return (Held { cache, id }, false);
        }
        self.queue().count_miss();
        let id = cache.claim().await;
        match cache.insert(key, id) {
            Insert::Inserted => (Held { cache, id }, true),
            Insert::Existing(existing) => {
                // Another miss inserted the block while this one was
                // claiming an entry; the claimed entry goes back.
                cache.push_free(id);
                (
                    Held {
                        cache,
                        id: existing,
                    },
                    false,
                )
            }
        }
    }

    /// Queues the read of an entry this task locked. The request takes
    /// a pin of its own.
    async fn submit_read(&self, id: u32) {
        let cache = self.shared();
        cache.entry(id).state.fetch_add(PIN, Ordering::AcqRel);
        let pending = PendingRead { cache, id };
        self.queue().submit(Request::Read(id)).await;
        core::mem::forget(pending);
    }

    /// Starts reading the rest of `block`'s readahead window into
    /// entries nobody asked for yet. Best effort: it stops at the first
    /// block it cannot place without waiting.
    fn read_ahead(&self, block: u64) {
        let layout = self.queue().layout;
        if layout.readahead_blocks <= 1 {
            return;
        }
        let cache = self.shared();
        let start = block - block % layout.readahead_blocks;
        let end = (start + layout.readahead_blocks).min(layout.capacity_blocks);
        for neighbour in (start..end).filter(|&neighbour| neighbour != block) {
            let key = (neighbour << DISK_BITS) | u64::from(self.disk);
            if let Some(id) = cache.find(key, Take::Pin) {
                cache.unpin(id);
                continue;
            }
            let Some(id) = cache.try_claim(false) else {
                return;
            };
            match cache.insert(key, id) {
                Insert::Existing(existing) => {
                    cache.push_free(id);
                    cache.unpin(existing);
                }
                Insert::Inserted => {
                    // The insertion pin is the request's.
                    if !self.queue().try_submit(Request::Read(id)) {
                        cache.transition(id, |state| (state & !LOCKED) - PIN);
                        return;
                    }
                    self.queue().count_readahead();
                }
            }
        }
    }

    /// Takes the pinned entry exclusively, reading it first when
    /// `contents` asks for the block's bytes.
    async fn exclusive(
        &self,
        held: Held<'_>,
        contents: bool,
    ) -> Result<BlockMut<'_>, BlockCacheError> {
        let cache = self.shared();
        let id = held.id;
        let state_word = &cache.entry(id).state;
        loop {
            let state = state_word.load(Ordering::Acquire);
            if state & (LOCKED | WRITEBACK) != 0 || shares(state) != 0 {
                // A read, a writer, a write-back or readers hold the
                // buffer. A read that fails here is retried below by
                // this task, whose own failure is the one reported.
                cache
                    .wait_state(id, |state| {
                        state & (LOCKED | WRITEBACK) == 0 && shares(state) == 0
                    })
                    .await;
                continue;
            }
            if state & COMPRESSED != 0 {
                // Decompressed even for an overwrite: until the writer
                // holds the buffer, readers must keep seeing the block's
                // latest bytes, which only the compressed page has.
                cache.try_inflate(id);
                continue;
            }
            if contents && state & VALID == 0 {
                if state_word
                    .compare_exchange(
                        state,
                        (state | LOCKED) & !FAILED,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    self.submit_read(id).await;
                    let state = cache.wait_state(id, |state| state & LOCKED == 0).await;
                    if state & VALID == 0 {
                        return Err(self.device_error());
                    }
                }
                continue;
            }
            if state_word
                .compare_exchange(
                    state,
                    state | LOCKED | REFERENCED,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                core::mem::forget(held);
                return Ok(self.block_mut(id, state & VALID == 0));
            }
        }
    }
}

impl Deref for BlockRef<'_> {
    type Target = [u8; CACHE_BLOCK_BYTES];

    fn deref(&self) -> &Self::Target {
        self.bytes
    }
}

impl Drop for BlockRef<'_> {
    fn drop(&mut self) {
        self.disk.shared().release(self.id, Take::Share);
    }
}

impl Deref for BlockMut<'_> {
    type Target = [u8; CACHE_BLOCK_BYTES];

    fn deref(&self) -> &Self::Target {
        // SAFETY: this guard holds the entry locked.
        unsafe { &*self.bytes }
    }
}

impl DerefMut for BlockMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: this guard holds the entry locked.
        unsafe { &mut *self.bytes }
    }
}

impl Drop for BlockMut<'_> {
    fn drop(&mut self) {
        self.disk.shared().transition(self.id, |state| {
            ((state & !(LOCKED | FAILED)) | VALID | DIRTY | REFERENCED) - PIN
        });
    }
}

/// The blocks the cache holds dirty, for tests that need to see its
/// state.
#[cfg(test)]
pub(super) fn dirty_blocks(cache: &BlockCache) -> alloc::vec::Vec<u64> {
    let shared = cache.shared();
    let allocated = shared.allocated.load(Ordering::Acquire);
    (0..allocated)
        .filter(|&id| shared.entry(id).state.load(Ordering::Acquire) & DIRTY != 0)
        .map(|id| shared.entry(id).key.load(Ordering::Acquire) >> DISK_BITS)
        .collect()
}

/// The block a key names.
pub(super) const fn key_block(key: u64) -> u64 {
    key >> DISK_BITS
}

/// The disk a key names.
pub(super) const fn key_disk(key: u64) -> usize {
    (key & DISK_MASK) as usize
}
