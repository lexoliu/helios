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
//!   first inserts it locked and reads the block, the second finds the
//!   locked entry, pins it and parks until the read completes. The
//!   device is asked once.
//! - **The task that owns an entry issues its request; the answer
//!   completes it.** The miss that inserted a block submits its read,
//!   with its readahead neighbours, as one scattered request; whoever
//!   starts a block's write-back — a sync, a miss that needs an entry,
//!   the reclaim task — submits it, with its dirty neighbours, as
//!   another. The request carries a [`BlockPages`] that owns the run: the
//!   entries' pins, locks and write-back bits, and any page staged for
//!   the write. The device's completion path calls the run's completion,
//!   which makes the entries valid, clean, dirty again or stale, and
//!   records a failure for the next sync — whether or not the task that
//!   submitted it still waits. The completion is lock-free apart from the
//!   one interrupt-safe lock that keeps the disk's last error, and the
//!   request's own pages are freed through the disk's memory, whose free
//!   path an interrupt handler may take ([`BlockDevice::Memory`]), so it
//!   may run in the device's interrupt handler. The device is the
//!   backend's type, monomorphized in, and a multi-queue device gives
//!   each processor its own queue, so requests from different processors
//!   share nothing on their way down. Nothing is handed to another task
//!   to issue.
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
//!   read in flight. The work runs on the processor that took the bit.
//! - **Syncs of one disk take turns.** One sync at a time leads: it
//!   drains the disk's dirty set, writes what it found, and flushes; a
//!   sync that arrives meanwhile waits for the leader, and is answered by
//!   it when the leader started after the sync's own call. The turn is a
//!   flag and a wait, never a lock held across `.await`.
//! - **Cache lines.** Every entry word is written by any processor, so
//!   each entry lives on its own cache line. Bucket heads are grouped a
//!   cache line at a time and written only by a miss or an eviction under
//!   the stripe lock that owns their whole line, so two processors never
//!   write one line at once. Event counters are per processor, each row
//!   on its own line, written only by tasks on that processor. The words
//!   every processor writes — the free list, the clock hand, the entry
//!   count, the reclaim gate, each disk's sync turn and error sequence —
//!   sit on lines of their own. A disk's dirty set is a bitmap whose
//!   words any processor sets on a block's clean-to-dirty transition,
//!   when it is already writing that block's entry line.
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
//! - The dirty set is the page cache's dirty tag (`PAGECACHE_TAG_DIRTY`
//!   in `mm/page-writeback.c`): a writeback walks the tagged entries
//!   rather than the whole cache.
//! - Write errors are reported to the next [`CachedDisk::sync`] from a
//!   per-disk error sequence, as `errseq_t` reports writeback errors.
//! - A request owns its pages until the device answers, and its
//!   completion — not the task that submitted it — finishes the blocks
//!   it names: a bio holds its pages and `bi_end_io` (`end_buffer_async_read`,
//!   `end_buffer_async_write` in `fs/buffer.c`) unlocks them, marks them
//!   up to date or records the error from the completion path. A
//!   submitter that goes away only stops waiting, as an io_uring operation
//!   whose future tokio-uring or monoio drops keeps its buffer with the
//!   kernel's completion.
//! - A failed or abandoned commit aborts the disk, as jbd2 aborts its
//!   journal (`jbd2_journal_abort`): every later write fails rather than
//!   build on a commit whose outcome on the media is unknown.
//! - Readahead is bounded by Linux's default window, `VM_READAHEAD_PAGES`
//!   (128 KiB, `include/linux/mm.h`), whatever optimal transfer the device
//!   claims.
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
//! A commit that fails, or whose task is dropped before it finishes,
//! aborts the disk, and every later write, sync and commit answers
//! [`BlockCacheError::Aborted`]. Dirty blocks of an aborted disk are
//! discarded rather than written. A commit block whose write failed is
//! never written again and is left *stale*: the readers that hold it
//! keep their bytes, no new reader is given them, and the next one reads
//! the block from the disk once nobody holds it.
//!
//! # Requests a dropped task leaves behind
//!
//! Dropping a task never cancels a request the device has: the request
//! owns its run, and its completion finishes the blocks whenever the
//! device answers. A read dropped in flight still leaves its blocks
//! valid, so a later reader is served without a second device read; a
//! write-back dropped in flight still leaves its blocks clean, or dirty
//! again with the failure recorded for the next sync. A sync dropped
//! part-way puts back into the dirty set every block it took out and did
//! not start, so the next sync still covers them. Only a commit's task
//! decides about the disk: a commit dropped before it finished aborts
//! it. A request dropped before the device took it never reached the
//! device, and gives its blocks back as they were: unread, or dirty.
//!
//! # Memory
//!
//! Blocks live in page-sized, page-aligned buffers of their disk's own
//! device memory ([`BlockDevice::memory`]), zeroed when they are
//! allocated, so a request carries an entry's page to the device in place
//! and a device is never handed a page it cannot reach. A page belongs to
//! the disk whose block it holds: an entry recycled for another disk's
//! block gives its page back and takes one of the new disk's. The cache
//! is kernel memory (AGENTS §3.1), never charged to an instance. It grows
//! while the machine's memory pressure is green and recycles its coldest
//! clean block once it is not. The entry table's bound is half the
//! machine — the cache cannot grow past the point where pressure stops
//! being green — and only the entries the cache actually grows into are
//! allocated. A free entry holds no buffer.
//!
//! A page the disk's memory refuses is a typed failure of the operation
//! that needed it ([`BlockCacheError::Memory`]): the miss that needed an
//! entry, the hit that decompresses a block, the overwrite of a
//! compressed block, the sync that stages a compressed dirty block for
//! its write. The cache neither recycles harder nor tries again because
//! of it.
//!
//! Under pressure, [`BlockCache::shrink`] moves blocks down two tiers:
//!
//! - **Yellow compresses.** The coldest blocks nobody holds, clean and
//!   dirty, are compressed into the kernel's [`CompressedPool`], which
//!   lives on the kernel heap, and their pages go back to the memory of
//!   the disk that allocated them, freed through its
//!   [`BlockDevice::Memory`]. The entry stays in the index, marked
//!   `COMPRESSED`, so a later hit decompresses it into a fresh page of
//!   the disk's memory without asking the device. A dirty block stays
//!   dirty while it is compressed: it reaches the disk uncompressed,
//!   through the same write-back and [`CachedDisk::sync`] as any other
//!   dirty block, from a page of the disk's memory decompressed for the
//!   write alone, which the request owns and frees when it is done. A
//!   block that does not compress below
//!   [`crate::memory::INCOMPRESSIBLE_BYTES`] skips the tier: a clean one
//!   is dropped and a dirty one is written back and dropped, as zswap
//!   and zram send such a page straight to the backing device. When the pool is full it evicts its own coldest pages first
//!   — a clean one is dropped, a dirty one written back uncompressed —
//!   and the block is offered once more.
//! - **Red empties the cache of everything nobody holds.** Clean blocks
//!   are dropped, compressed or not; dirty ones are written back and then
//!   dropped, and every page they held goes back to its disk's memory. A
//!   compressed block is written from a page decompressed for that write,
//!   so emptying the tier never holds more page buffers than one request
//!   carries. A staging page the disk's memory refuses leaves its block
//!   dirty in the cache, for the next sync to write or report.
//!
//! The miss path's clock may also recycle a clean compressed entry, as
//! it recycles any clean one; it does not compress the block it
//! recycles, because a miss under pressure needs the entry, and the
//! reclaim pass is what moves blocks into the pool.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::future::poll_fn;
use core::ops::{Deref, DerefMut};
use core::pin::pin;
use core::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};

use arrayvec::ArrayVec;
use crossbeam_utils::CachePadded;
use helios_hal::cpu::current_processor;
use helios_hal::dma::DmaBuffer;
use helios_hal::dma::DmaError;
use helios_hal::fs::{BlockDevice, BlockGeometry, BlockSerial};
use helios_hal::io::{IoError, IoResult};
use helios_hal::pmm::PhysFrame;
use spin::{Mutex, Once};

use super::pages::{BlockPages, DevicePages};
use crate::exec::InFlight;
use crate::memory::{CompressRefusal, CompressedPage, CompressedPool, IrqSafeMutex};
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

/// Most cache blocks one device request carries, and the bound on the
/// readahead window: Linux's default readahead, `VM_READAHEAD_PAGES`
/// (128 KiB, `include/linux/mm.h`). A device's own optimal transfer is
/// honoured below it and never above, so a device that claims an
/// enormous one cannot make a miss queue more than this or keep a demand
/// read waiting behind more than one request.
pub(super) const MAX_RUN_BLOCKS: usize = 128 * 1024 / CACHE_BLOCK_BYTES;

/// Write-back requests one sync keeps in flight at once: eight runs of
/// at most [`MAX_RUN_BLOCKS`] blocks, 1 MiB. The device's queue depth
/// bounds it further.
const SYNC_RUNS_IN_FLIGHT: usize = 8;

/// Bucket heads on one cache line.
const BUCKETS_PER_LINE: usize =
    core::mem::size_of::<CachePadded<u8>>() / core::mem::size_of::<AtomicU32>();

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
// none outstanding. The flags above them describe the entry's slot.
const PIN: u64 = 1;
const COUNT_BITS: u32 = 20;
const PIN_MASK: u64 = (1 << COUNT_BITS) - 1;
const SHARE: u64 = 1 << COUNT_BITS;
const SHARE_MASK: u64 = PIN_MASK << COUNT_BITS;
/// The slot holds a buffer with the block's bytes. Exclusive of
/// [`COMPRESSED`], as the slot's variants are.
const VALID: u64 = 1 << 40;
/// One task owns the slot: the read filling it, a writer, or a move
/// between tiers.
const LOCKED: u64 = 1 << 41;
/// The block differs from the disk.
const DIRTY: u64 = 1 << 42;
/// The last read of the block failed; cleared by the next attempt.
const FAILED: u64 = 1 << 43;
/// Touched since the clock hand last passed.
const REFERENCED: u64 = 1 << 44;
/// Owned by whoever is recycling it, or on the free list; no pin may
/// be taken.
const RECLAIM: u64 = 1 << 45;
/// A write of the block to the disk is in flight.
const WRITEBACK: u64 = 1 << 46;
/// A task is parked on this entry's wait signal.
const WAITERS: u64 = 1 << 47;
/// The slot holds the block compressed, and no buffer.
const COMPRESSED: u64 = 1 << 48;
/// The buffer holds bytes the disk may not: a commit block whose write
/// failed. Readers that hold a share keep reading them; nobody is given
/// a new share, and the block is read afresh once no share is left.
/// Always set together with [`VALID`], so the shares it outlives stay
/// legal.
const STALE: u64 = 1 << 49;

const fn pins(state: u64) -> u64 {
    state & PIN_MASK
}

const fn shares(state: u64) -> u64 {
    (state & SHARE_MASK) >> COUNT_BITS
}

/// Whether the clock may take this entry: nobody holds it and its
/// contents agree with the disk.
const fn evictable(state: u64) -> bool {
    pins(state) == 0 && state & (LOCKED | DIRTY | WRITEBACK | RECLAIM) == 0
}

/// Whether a reclaim pass may move this entry: nobody holds it and no
/// read, writer or write-back owns it. Dirty entries qualify.
const fn idle(state: u64) -> bool {
    pins(state) == 0 && state & (LOCKED | WRITEBACK | RECLAIM) == 0
}

/// Whether a reader may share the buffer: it holds the block, nobody owns
/// it, and its bytes are the disk's or newer.
const fn readable(state: u64) -> bool {
    state & (VALID | LOCKED | STALE) == VALID
}

/// What the cache needs of a disk's memory: the bounds
/// [`BlockDevice::Memory`] carries, named once.
pub(super) trait PageMemory: DmaBuffer + AsRef<[u8]> + AsMut<[u8]> {}

impl<Memory: DmaBuffer + AsRef<[u8]> + AsMut<[u8]>> PageMemory for Memory {}

/// The block a cache page holds.
fn block_of<Memory: PageMemory>(page: &Memory) -> &[u8; CACHE_BLOCK_BYTES] {
    page.as_ref()
        .try_into()
        .expect("a block cache page is one cache block long")
}

/// The block a cache page holds, to write.
fn block_of_mut<Memory: PageMemory>(page: &mut Memory) -> &mut [u8; CACHE_BLOCK_BYTES] {
    page.as_mut()
        .try_into()
        .expect("a block cache page is one cache block long")
}

/// Where an entry keeps its block.
enum Slot<Memory> {
    /// A free entry, or one whose block left the cache.
    Empty,
    /// The block in a page of the memory of the disk the entry's key
    /// names; its bytes are the block's once the state says [`VALID`].
    Buffer(Memory),
    /// The block compressed into the pool.
    Compressed(CompressedPage),
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
    #[error("the disk was aborted after a commit failed or was abandoned; it takes no more writes")]
    Aborted,
    #[error("the device failed: {0}")]
    Device(IoError),
    #[error("the disk's memory refused a page: {0}")]
    Memory(DmaError),
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
    /// Cache blocks one device request carries: as many as the device's
    /// segments hold, up to [`MAX_RUN_BLOCKS`].
    pub(super) run_blocks: usize,
    /// Cache blocks one miss reads, aligned to their own size: the
    /// device's optimal transfer, or the missing block alone when the
    /// device states none, and never more than one request carries.
    pub(super) readahead_blocks: u64,
}

impl DiskLayout {
    pub(super) fn new(geometry: BlockGeometry) -> Result<Self, CacheLayoutError> {
        let logical_block_bytes = geometry.logical_block_bytes;
        if logical_block_bytes == 0
            || logical_block_bytes > CACHE_BLOCK_BYTES
            || !CACHE_BLOCK_BYTES.is_multiple_of(logical_block_bytes)
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
        // A segment carries whole logical blocks, so a cache block takes
        // as many segments as it takes pieces of that size.
        let segment_bytes = (geometry.max_segment_bytes as usize / logical_block_bytes).max(1)
            * logical_block_bytes;
        let segments_per_block = CACHE_BLOCK_BYTES.div_ceil(segment_bytes);
        let run_blocks =
            (geometry.max_segments as usize / segments_per_block).clamp(1, MAX_RUN_BLOCKS);
        let optimal_bytes = (geometry.opt_io_blocks as usize).saturating_mul(logical_block_bytes);
        let readahead_blocks = ((optimal_bytes / CACHE_BLOCK_BYTES) as u64)
            .clamp(1, run_blocks as u64)
            .min(capacity_blocks);
        Ok(Self {
            device_blocks_per_block,
            capacity_blocks,
            run_blocks,
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
struct Gate {
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
    const fn new() -> Self {
        Self {
            signal: ProgressSignal::new(),
            waiters: AtomicUsize::new(0),
        }
    }

    /// Parks until `ready` answers true. `ready` must read what it tests
    /// with sequentially consistent loads or operations.
    async fn wait_until(&self, mut ready: impl FnMut() -> bool) {
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
    fn open(&self) {
        if self.waiters.load(Ordering::SeqCst) != 0 {
            self.signal.signal();
        }
    }
}

/// One cached block's metadata. Type-stable: an entry is reused for
/// other blocks but never freed while the cache lives.
struct Entry<Memory> {
    /// `block << DISK_BITS | disk`, or [`NO_KEY`].
    key: AtomicU64,
    state: AtomicU64,
    /// The next entry in this entry's chain or on the free list, or a
    /// nulls marker.
    next: AtomicU32,
    /// Read and replaced only as the state word allows; see
    /// [`Entry::slot`] and [`Entry::slot_mut`].
    slot: UnsafeCell<Slot<Memory>>,
}

// SAFETY: every field but `slot` is atomic. `slot` is replaced only by
// the one task that owns the entry exclusively through its state word
// (under `LOCKED` with no share outstanding, or under `RECLAIM`), and
// read only by tasks whose share, lock or reclaim excludes that owner;
// the acquiring and releasing operations on the state word order those
// accesses.
unsafe impl<Memory: Send + Sync> Sync for Entry<Memory> {}

impl<Memory> Entry<Memory> {
    const fn free() -> Self {
        Self {
            key: AtomicU64::new(NO_KEY),
            state: AtomicU64::new(RECLAIM),
            next: AtomicU32::new(FREE_END),
            slot: UnsafeCell::new(Slot::Empty),
        }
    }

    /// The entry's slot, to read.
    ///
    /// # Safety
    ///
    /// Nobody may replace the slot while the reference lives: the caller
    /// holds the entry `LOCKED`, under `RECLAIM`, or holds a share of
    /// it.
    unsafe fn slot(&self) -> &Slot<Memory> {
        // SAFETY: the caller's hold excludes the only writers.
        unsafe { &*self.slot.get() }
    }

    /// The entry's slot, to replace or write through.
    ///
    /// # Safety
    ///
    /// The caller owns the entry exclusively: it holds it `LOCKED` with
    /// no share outstanding, or under `RECLAIM`, and no other reference
    /// into the slot lives.
    #[expect(
        clippy::mut_from_ref,
        reason = "the entry's state word, not the borrow, grants exclusive access to the slot"
    )]
    unsafe fn slot_mut(&self) -> &mut Slot<Memory> {
        // SAFETY: the caller's exclusive ownership excludes every other
        // access.
        unsafe { &mut *self.slot.get() }
    }
}

impl<Memory: PageMemory> Entry<Memory> {
    /// The bytes of a buffer the caller may read.
    ///
    /// # Safety
    ///
    /// As [`Entry::slot`], and the slot holds a buffer: the caller holds
    /// a share, or owns a buffered entry.
    unsafe fn bytes(&self) -> &[u8; CACHE_BLOCK_BYTES] {
        // SAFETY: forwarded from the caller.
        match unsafe { self.slot() } {
            Slot::Buffer(page) => block_of(page),
            Slot::Empty | Slot::Compressed(_) => {
                panic!("a block cache entry read as a buffer holds none")
            }
        }
    }

    /// The bytes of a buffer the caller owns.
    ///
    /// # Safety
    ///
    /// As [`Entry::slot_mut`], and the slot holds a buffer.
    #[expect(
        clippy::mut_from_ref,
        reason = "the entry's state word, not the borrow, grants exclusive access to the slot"
    )]
    unsafe fn bytes_mut(&self) -> &mut [u8; CACHE_BLOCK_BYTES] {
        // SAFETY: forwarded from the caller.
        match unsafe { self.slot_mut() } {
            Slot::Buffer(page) => block_of_mut(page),
            Slot::Empty | Slot::Compressed(_) => {
                panic!("a block cache entry written as a buffer holds none")
            }
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

/// What a cache request does to the entries it names, which decides how
/// its answer finishes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunKind {
    /// Fills entries locked for the read: valid on success, failed on
    /// an error, unread again when the device never saw the request.
    Read,
    /// Writes dirty entries back: clean on success, dirty again when the
    /// write failed or never reached the device.
    WriteBack,
    /// Writes a commit block: clean on success, stale when the write
    /// failed or never reached the device, because a commit block that
    /// did not reach the disk must neither be written later nor read as
    /// if it had.
    Commit,
}

/// The cache blocks one device request names, and what its answer does
/// to them: the cache's half of a [`BlockPages`].
///
/// It owns what the request holds of each entry — the read's lock and
/// pin, or the write-back's bit, share and pin — until the answer, or
/// the request's end without one, gives them back. It may be completed
/// in the device's interrupt handler, so nothing here blocks, logs or
/// takes a lock an interrupt handler cannot take.
pub(super) struct CacheRun<Memory> {
    cache: Arc<CacheCore<Memory>>,
    disk: usize,
    first_block: u64,
    ids: ArrayVec<u32, MAX_RUN_BLOCKS>,
    kind: RunKind,
    /// Page buffers staged for this write, which the request owns and
    /// the cache counts.
    staged: usize,
    answered: bool,
}

impl<Memory> CacheRun<Memory> {
    /// The device answered the request, or refused it.
    pub(super) fn complete(&mut self, result: IoResult<()>) {
        assert!(!self.answered, "a block cache request was answered twice");
        self.answered = true;
        let cache = &*self.cache;
        let disk = &cache.disks[self.disk];
        let succeeded = result.is_ok();
        match self.kind {
            RunKind::Read => {
                disk.count(|counters| &counters.device_reads);
                if let Err(error) = result {
                    disk.count(|counters| &counters.read_errors);
                    disk.record_error(error);
                }
                for &id in &self.ids {
                    cache.complete_read(id, succeeded);
                }
            }
            RunKind::WriteBack | RunKind::Commit => {
                disk.count(|counters| &counters.device_writes);
                if let Err(error) = result {
                    disk.record_error(error);
                    disk.errors.writes.fetch_add(1, Ordering::AcqRel);
                }
                for &id in &self.ids {
                    if self.kind == RunKind::Commit && !succeeded {
                        cache.leave_stale(id);
                    } else {
                        cache.complete_write(self.disk, id, succeeded);
                    }
                }
            }
        }
    }

    pub(super) fn disk(&self) -> usize {
        self.disk
    }

    pub(super) fn first_block(&self) -> u64 {
        self.first_block
    }

    pub(super) fn blocks(&self) -> usize {
        self.ids.len()
    }

    /// The entry holding the run's `index`th block.
    pub(super) fn id(&self, index: usize) -> u32 {
        self.ids[index]
    }
}

impl<Memory> Drop for CacheRun<Memory> {
    fn drop(&mut self) {
        let cache = &*self.cache;
        cache.pages.fetch_sub(self.staged, Ordering::Relaxed);
        if self.answered {
            return;
        }
        // The device never saw the request: the blocks go back as they
        // were before it was built.
        for &id in &self.ids {
            match self.kind {
                RunKind::Read => cache.abandon_read(id),
                RunKind::WriteBack => cache.complete_write(self.disk, id, false),
                RunKind::Commit => cache.leave_stale(id),
            }
        }
    }
}

/// What [`CacheShared::compress_entry`] did.
enum Demotion {
    Compressed,
    /// Another task took the entry first.
    Busy,
    /// The pool refused the block; the entry is back in the state given.
    Refused(CompressRefusal, u64),
}

/// Why a sync's first pass starts no run for a block it took out of the
/// dirty set.
enum Skip {
    /// Nothing of this disk to write.
    Clean,
    /// Dirty or in flight, held by someone else: settled once the first
    /// pass's writes are done.
    Busy,
    /// Compressed and dirty, and the disk's memory refused the page its
    /// write is staged in: settled by the second pass like a held block,
    /// where a refusal fails the sync.
    Refused,
}

/// The blocks one read request fills, in block order. Every entry is
/// locked for the read and carries a pin the read gives back.
struct ReadRun {
    disk: usize,
    first_block: u64,
    ids: ArrayVec<u32, MAX_RUN_BLOCKS>,
}

/// The blocks one write request carries, in block order. Every entry is
/// in write-back with a share and a pin the write gives back.
struct WriteRun<Memory> {
    disk: usize,
    first_block: u64,
    ids: ArrayVec<u32, MAX_RUN_BLOCKS>,
    /// For a block held compressed, its bytes decompressed for this
    /// write alone into a page of the disk's memory; the entry keeps its
    /// compressed page.
    staged: ArrayVec<Option<Memory>, MAX_RUN_BLOCKS>,
}

impl<Memory> WriteRun<Memory> {
    fn new(disk: usize, block: u64, id: u32, staged: Option<Memory>) -> Self {
        let mut run = Self {
            disk,
            first_block: block,
            ids: ArrayVec::new(),
            staged: ArrayVec::new(),
        };
        run.push_back(id, staged);
        run
    }

    fn push_back(&mut self, id: u32, staged: Option<Memory>) {
        self.ids.push(id);
        self.staged.push(staged);
    }

    fn push_front(&mut self, id: u32, staged: Option<Memory>) {
        self.ids.insert(0, id);
        self.staged.insert(0, staged);
        self.first_block -= 1;
    }
}

/// A set of entry ids, one bit each, that any processor adds to and one
/// task at a time drains: a disk's dirty blocks.
///
/// A summary bit per word says which words may be non-zero, so a drain
/// reads the words that hold something rather than all of them. Adding
/// sets the word bit before the summary bit; draining clears the summary
/// bit before the word, so an id added during a drain is either taken
/// by it or left set for the next.
struct DirtySet {
    words: Box<[AtomicU64]>,
    summary: Box<[AtomicU64]>,
}

impl DirtySet {
    fn new(entries: usize) -> Self {
        let words = entries.div_ceil(64);
        Self {
            words: (0..words).map(|_| AtomicU64::new(0)).collect(),
            summary: (0..words.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn insert(&self, id: u32) {
        let word = id as usize / 64;
        self.words[word].fetch_or(1 << (id % 64), Ordering::SeqCst);
        self.summary[word / 64].fetch_or(1 << (word % 64), Ordering::SeqCst);
    }
}

/// A walk that takes every id out of a [`DirtySet`] as it reaches it.
///
/// It takes a word of ids, and a summary word of words, at a time, so
/// until it hands them out it holds ids the set no longer shows. Dropped
/// part-way — its sync dropped at an `.await` — it puts back every id it
/// took and has not handed out, and the id it handed out last, whose
/// caller may not have finished with it: the set always covers every
/// dirty entry, and an id put back for a block already clean is skipped
/// by the next walk.
struct Drain<'a> {
    set: &'a DirtySet,
    next_summary: usize,
    words: u64,
    summary: usize,
    word: usize,
    bits: u64,
    /// The id handed out last, until the next one is asked for.
    current: Option<u32>,
}

impl<'a> Drain<'a> {
    fn new(set: &'a DirtySet) -> Self {
        Self {
            set,
            next_summary: 0,
            words: 0,
            summary: 0,
            word: 0,
            bits: 0,
            current: None,
        }
    }

    fn next(&mut self) -> Option<u32> {
        self.current = None;
        loop {
            if self.bits != 0 {
                let bit = self.bits.trailing_zeros() as usize;
                self.bits &= self.bits - 1;
                let id = (self.word * 64 + bit) as u32;
                self.current = Some(id);
                return Some(id);
            }
            if self.words != 0 {
                let word = self.words.trailing_zeros() as usize;
                self.words &= self.words - 1;
                self.word = self.summary * 64 + word;
                self.bits = self.set.words[self.word].swap(0, Ordering::SeqCst);
                continue;
            }
            self.summary = self.next_summary;
            let summary = self.set.summary.get(self.summary)?;
            self.next_summary += 1;
            self.words = summary.swap(0, Ordering::SeqCst);
        }
    }
}

impl Drop for Drain<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.current {
            self.set.insert(id);
        }
        // Word bits before summary bits, as `DirtySet::insert` orders
        // them, so a concurrent walk that sees the summary sees the ids.
        if self.bits != 0 {
            self.set.words[self.word].fetch_or(self.bits, Ordering::SeqCst);
            self.set.summary[self.word / 64].fetch_or(1 << (self.word % 64), Ordering::SeqCst);
        }
        // Words whose summary bit this walk cleared but which it has not
        // taken still hold their ids.
        if self.words != 0 {
            self.set.summary[self.summary].fetch_or(self.words, Ordering::SeqCst);
        }
    }
}

/// What one processor's tasks asked of one disk.
#[derive(Default)]
struct DiskCounters {
    misses: AtomicU64,
    device_reads: AtomicU64,
    readahead: AtomicU64,
    device_writes: AtomicU64,
    flushes: AtomicU64,
    read_errors: AtomicU64,
}

/// One disk's turn-taking for syncs: tickets in, coverage out.
struct SyncTurn {
    /// Syncs asked for, numbered from one.
    requested: AtomicU64,
    /// The highest ticket a successful leader covered.
    completed: AtomicU64,
    /// The highest ticket a leader whose flush failed covered.
    failed: AtomicU64,
    /// Whether a sync is leading.
    leading: AtomicBool,
    /// Opened when a leader finishes.
    finished: Gate,
}

/// The device's failures on one disk.
struct DiskErrors {
    /// Bumped by every failed write; a sync compares it across itself.
    writes: AtomicU64,
    /// How the disk last failed: what the device said when it last
    /// failed a request, written by a request's completion, which may run
    /// in an interrupt handler, or the disk's memory refusing the page a
    /// sync needed to write a block.
    last: IrqSafeMutex<Option<BlockCacheError>>,
}

/// What the cache knows about one disk it serves, as the caller hands
/// it over.
pub(super) struct CacheDisk<Device> {
    pub(super) serial: BlockSerial,
    pub(super) layout: DiskLayout,
    pub(super) read_only: bool,
    pub(super) device: Device,
}

/// The cache's state for one disk; the device itself is the cache's
/// handle on it, in [`CacheShared`].
struct DiskState {
    serial: BlockSerial,
    layout: DiskLayout,
    read_only: bool,
    aborted: AtomicBool,
    dirty: DirtySet,
    turn: CachePadded<SyncTurn>,
    errors: CachePadded<DiskErrors>,
    /// Indexed by processor; row `p` is written only by tasks running on
    /// processor `p`.
    counters: Box<[CachePadded<DiskCounters>]>,
}

impl DiskState {
    fn new<Device>(disk: &CacheDisk<Device>, processors: usize, entries: usize) -> Self {
        Self {
            serial: disk.serial,
            layout: disk.layout,
            read_only: disk.read_only,
            aborted: AtomicBool::new(false),
            dirty: DirtySet::new(entries),
            turn: CachePadded::new(SyncTurn {
                requested: AtomicU64::new(0),
                completed: AtomicU64::new(0),
                failed: AtomicU64::new(0),
                leading: AtomicBool::new(false),
                finished: Gate::new(),
            }),
            errors: CachePadded::new(DiskErrors {
                writes: AtomicU64::new(0),
                last: IrqSafeMutex::new(None),
            }),
            counters: (0..processors)
                .map(|_| CachePadded::new(DiskCounters::default()))
                .collect(),
        }
    }

    /// Adds one to the executing processor's `counter`.
    fn count(&self, counter: impl FnOnce(&DiskCounters) -> &AtomicU64) {
        self.add(counter, 1);
    }

    /// Adds `amount` to the executing processor's `counter`.
    fn add(&self, counter: impl FnOnce(&DiskCounters) -> &AtomicU64, amount: u64) {
        let processor = current_processor().id() as usize;
        let row = self.counters.get(processor).unwrap_or_else(|| {
            panic!(
                "processor {processor} has no block cache counters among the {} configured",
                self.counters.len()
            )
        });
        counter(row).fetch_add(amount, Ordering::Relaxed);
    }

    /// Records a request the device failed.
    fn record_error(&self, error: IoError) {
        self.record_failure(BlockCacheError::Device(error));
    }

    fn record_failure(&self, failure: BlockCacheError) {
        self.errors.last.with(|last| *last = Some(failure));
    }

    fn last_failure(&self) -> BlockCacheError {
        self.errors
            .last
            .with(|last| last.unwrap_or(BlockCacheError::Device(IoError::DeviceFault)))
    }

    fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    fn stats(&self) -> CachedDiskStats {
        let mut stats = CachedDiskStats {
            write_errors: self.errors.writes.load(Ordering::Relaxed),
            ..CachedDiskStats::default()
        };
        for row in self.counters.iter() {
            stats.misses += row.misses.load(Ordering::Relaxed);
            stats.device_reads += row.device_reads.load(Ordering::Relaxed);
            stats.readahead += row.readahead.load(Ordering::Relaxed);
            stats.device_writes += row.device_writes.load(Ordering::Relaxed);
            stats.flushes += row.flushes.load(Ordering::Relaxed);
            stats.read_errors += row.read_errors.load(Ordering::Relaxed);
        }
        stats
    }
}

/// A sync that is leading its disk, until it is dropped.
struct Leading<'a>(&'a SyncTurn);

impl Drop for Leading<'_> {
    fn drop(&mut self) {
        self.0.leading.store(false, Ordering::SeqCst);
        self.0.finished.open();
    }
}

/// A commit in progress. Dropped before it finished, it aborts the
/// disk: the commit block may or may not be on the media.
struct Committing<'a, Memory> {
    cache: &'a CacheCore<Memory>,
    disk: usize,
    finished: bool,
}

impl<Memory> Drop for Committing<'_, Memory> {
    fn drop(&mut self) {
        if !self.finished {
            self.cache.abort(self.disk);
        }
    }
}

/// A run of entries allocated together the first time an id in it is named.
type Segment<Memory> = Once<Box<[CachePadded<Entry<Memory>>]>>;

/// The cache and the devices it reaches: the core every request's
/// completion holds, and the disks' devices, which only the tasks that
/// issue requests need, and which allocate every page the cache holds.
pub(super) struct CacheShared<Device: BlockDevice> {
    core: Arc<CacheCore<Device::Memory>>,
    /// In the order of [`CacheCore::disks`].
    devices: ArrayVec<Device, MAX_BLOCK_DEVICES>,
}

/// Everything the cache keeps but the devices. A request's completion
/// holds it, so it names no device type, only the type of the memory the
/// devices hand out.
pub(super) struct CacheCore<Memory> {
    segments: Box<[Segment<Memory>]>,
    max_entries: u32,
    /// Entries ever created; ids below it name a live segment.
    allocated: CachePadded<AtomicU32>,
    /// Treiber stack of free entries: `tag << 32 | id`.
    free: CachePadded<AtomicU64>,
    /// Bucket heads, one cache line of them per element.
    buckets: Box<[CachePadded<[AtomicU32; BUCKETS_PER_LINE]>]>,
    bucket_shift: u32,
    stripes: Box<[CachePadded<Mutex<()>>]>,
    /// Indexed by processor; row `p` is written only by tasks running on
    /// processor `p`.
    lookaside: Box<[CachePadded<[AtomicU32; LOOKASIDE_WAYS]>]>,
    waits: Box<[CachePadded<ProgressSignal>]>,
    /// The clock hand, advanced by every sweep on any processor.
    hand: CachePadded<AtomicU32>,
    /// Raised when an entry becomes idle, for a miss that found the
    /// cache full of pinned, dirty or in-flight blocks.
    reclaimable: CachePadded<Gate>,
    /// The last [`PressureLevel`] the reclaim task measured.
    pressure: CachePadded<AtomicU8>,
    /// Page buffers the cache holds, staged write copies included.
    pages: CachePadded<AtomicUsize>,
    /// The tier cold blocks are compressed into under pressure.
    pool: CompressedPool,
    disks: ArrayVec<DiskState, MAX_BLOCK_DEVICES>,
}

impl<Device: BlockDevice> Deref for CacheShared<Device> {
    type Target = CacheCore<Device::Memory>;

    fn deref(&self) -> &CacheCore<Device::Memory> {
        &self.core
    }
}

impl<Device: BlockDevice> CacheShared<Device> {
    fn new(
        limits: BlockCacheLimits,
        processors: usize,
        pool: CompressedPool,
        disks: impl IntoIterator<Item = CacheDisk<Device>>,
    ) -> Self {
        let mut states = ArrayVec::new();
        let mut devices = ArrayVec::new();
        let core = CacheCore::new(limits, processors, pool);
        for disk in disks {
            states.push(DiskState::new(&disk, processors, core.max_entries as usize));
            devices.push(disk.device);
        }
        Self {
            core: Arc::new(CacheCore {
                disks: states,
                ..core
            }),
            devices,
        }
    }
}

/// The parts of the cache that never reach a device.
impl<Memory> CacheCore<Memory> {
    /// A cache with no disk yet; [`CacheShared::new`] adds them.
    fn new(limits: BlockCacheLimits, processors: usize, pool: CompressedPool) -> Self {
        assert!(
            processors != 0,
            "the block cache needs at least one processor"
        );
        let max_entries = u32::try_from(limits.max_blocks.max(1))
            .unwrap_or(u32::MAX)
            .min(NULLS - 1);
        let segment_count = (max_entries as usize).div_ceil(SEGMENT_ENTRIES);
        let bucket_count = (max_entries as usize / 2)
            .next_power_of_two()
            .clamp(BUCKETS_PER_LINE, MAX_BUCKETS);
        let stripe_count = processors.saturating_mul(4).next_power_of_two();
        Self {
            segments: (0..segment_count).map(|_| Once::new()).collect(),
            max_entries,
            allocated: CachePadded::new(AtomicU32::new(0)),
            free: CachePadded::new(AtomicU64::new(u64::from(FREE_END))),
            buckets: (0..bucket_count / BUCKETS_PER_LINE)
                .map(|line| {
                    CachePadded::new(core::array::from_fn(|slot| {
                        AtomicU32::new(NULLS | (line * BUCKETS_PER_LINE + slot) as u32)
                    }))
                })
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
            reclaimable: CachePadded::new(Gate::new()),
            pressure: CachePadded::new(AtomicU8::new(PressureLevel::Green as u8)),
            pages: CachePadded::new(AtomicUsize::new(0)),
            pool,
            disks: ArrayVec::new(),
        }
    }

    fn entry(&self, id: u32) -> &Entry<Memory> {
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

    fn bucket(&self, bucket: usize) -> &AtomicU32 {
        &self.buckets[bucket / BUCKETS_PER_LINE][bucket % BUCKETS_PER_LINE]
    }

    /// The lock every writer of `bucket`'s cache line takes.
    fn stripe(&self, bucket: usize) -> &Mutex<()> {
        &self.stripes[(bucket / BUCKETS_PER_LINE) & (self.stripes.len() - 1)]
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

    /// Gives a page back to the memory of the disk that allocated it.
    fn release_page(&self, page: Memory) {
        drop(page);
        self.pages.fetch_sub(1, Ordering::Relaxed);
    }

    /// Frees what a slot the caller took out of an entry held.
    fn release_slot(&self, slot: Slot<Memory>) {
        match slot {
            Slot::Empty => {}
            Slot::Buffer(page) => self.release_page(page),
            Slot::Compressed(page) => drop(page),
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
                    if !readable(current) {
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
    fn unpin(&self, id: u32) {
        let previous = self.entry(id).state.fetch_sub(PIN, Ordering::SeqCst);
        if idle(previous - PIN) {
            self.reclaimable.open();
        }
    }

    /// Applies `change` to the state word and wakes whoever waits on the
    /// entry, answering the state before and after. Every release goes
    /// through here; an acquisition never does, because it changes
    /// nothing a waiter waits for.
    fn update(&self, id: u32, change: impl Fn(u64) -> u64) -> (u64, u64) {
        let state = &self.entry(id).state;
        let mut current = state.load(Ordering::Relaxed);
        loop {
            let next = change(current) & !WAITERS;
            match state.compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::Relaxed) {
                Ok(previous) => {
                    if previous & WAITERS != 0 {
                        self.wait_signal(id).signal();
                    }
                    if idle(next) {
                        self.reclaimable.open();
                    }
                    return (previous, next);
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// [`CacheShared::update`], answering the new state.
    fn transition(&self, id: u32, change: impl Fn(u64) -> u64) -> u64 {
        self.update(id, change).1
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
            let mut cursor = self.bucket(bucket).load(Ordering::Acquire);
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
        let head = self.bucket(bucket).load(Ordering::Relaxed);
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
        self.bucket(bucket).store(id, Ordering::Release);
        Insert::Inserted
    }

    /// Takes the entry `id`, which its caller owns, out of its chain.
    fn unlink(&self, id: u32) {
        let entry = self.entry(id);
        let key = entry.key.load(Ordering::Acquire);
        let bucket = self.bucket_of(key);
        let _stripe = self.stripe(bucket).lock();
        let successor = entry.next.load(Ordering::Relaxed);
        let mut link = self.bucket(bucket);
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

    /// Gives back the entry `id`, owned under `RECLAIM`, with its slot
    /// emptied: a free entry holds no memory.
    fn push_free(&self, id: u32) {
        let entry = self.entry(id);
        // SAFETY: this task owns the entry under `RECLAIM`, and no
        // reference into its slot lives.
        let slot = unsafe { entry.slot_mut() };
        self.release_slot(core::mem::replace(slot, Slot::Empty));
        let mut head = self.free.load(Ordering::Acquire);
        loop {
            entry.next.store(head as u32, Ordering::Relaxed);
            let next = ((head >> 32).wrapping_add(1) << 32) | u64::from(id);
            match self
                .free
                .compare_exchange_weak(head, next, Ordering::SeqCst, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(observed) => head = observed,
            }
        }
        // A claimer parked on a full cache can take it now.
        self.reclaimable.open();
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
    /// referenced entries a second chance. Answers it with the disk whose
    /// block it held, whose memory any page it holds is.
    fn evict_one(&self) -> Option<(u32, usize)> {
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
                // The owner of a `RECLAIM` entry is the only writer of
                // its key, so the key read here is the one unlinked.
                let disk = key_disk(self.entry(id).key.load(Ordering::Acquire));
                self.unlink(id);
                return Some((id, disk));
            }
        }
        None
    }

    /// An entry this caller owns under [`RECLAIM`], with the disk whose
    /// page it may still hold: `None` for a free or new entry, which
    /// holds nothing.
    ///
    /// Green pressure reuses a free entry or grows; otherwise the clock
    /// recycles a clean block first. An `urgent` claim — a reader that
    /// cannot proceed without one — grows under pressure when nothing
    /// is evictable; a speculative one does not.
    fn take_entry(&self, urgent: bool) -> Option<(u32, Option<usize>)> {
        let fresh = || self.pop_free().or_else(|| self.grow()).map(|id| (id, None));
        let recycled = || self.evict_one().map(|(id, disk)| (id, Some(disk)));
        if self.pressure() == PressureLevel::Green {
            fresh().or_else(recycled)
        } else {
            recycled().or_else(|| if urgent { fresh() } else { None })
        }
    }

    /// An idle dirty entry, found by the clock. A dirty entry of an
    /// aborted disk is discarded on the way rather than answered.
    fn find_idle_dirty(&self) -> Option<u32> {
        let allocated = self.allocated.load(Ordering::Acquire);
        for _ in 0..allocated {
            let id = self.hand.fetch_add(1, Ordering::Relaxed) % allocated;
            let current = self.entry(id).state.load(Ordering::SeqCst);
            if current & DIRTY == 0 || !idle(current) {
                continue;
            }
            if self.disk_aborted(id) {
                self.discard(id);
                continue;
            }
            return Some(id);
        }
        None
    }

    /// Whether the disk the entry `id` belongs to was aborted.
    fn disk_aborted(&self, id: u32) -> bool {
        let key = self.entry(id).key.load(Ordering::Acquire);
        key != NO_KEY && self.disks[key_disk(key)].is_aborted()
    }

    /// Forgets that the entry differs from its disk: its disk was
    /// aborted, and nothing will be written to it again.
    fn discard(&self, id: u32) {
        self.transition(id, |state| state & !DIRTY);
    }

    /// Adds the entry `id`, which just became dirty, to its disk's dirty
    /// set.
    fn mark_dirty(&self, disk: usize, id: u32) {
        self.disks[disk].dirty.insert(id);
    }

    fn abort(&self, disk: usize) {
        let state = &self.disks[disk];
        if !state.aborted.swap(true, Ordering::SeqCst) {
            tracing::error!(
                serial = ?state.serial,
                "block cache aborted a disk: a commit failed or was abandoned, and the disk takes no more writes"
            );
        }
    }

    /// Finishes the write of the commit block `id` that did not reach
    /// the disk: not dirty, so nothing writes it again, and stale, so no
    /// new reader is given its bytes. The write's share and pin go back.
    fn leave_stale(&self, id: u32) {
        self.transition(id, |state| ((state & !WRITEBACK) | STALE) - SHARE - PIN);
    }

    /// Gives back the entry `id`, locked for a read the device never saw:
    /// unread and unlocked, so the next reader issues the read itself.
    /// The read's pin goes back.
    fn abandon_read(&self, id: u32) {
        self.transition(id, |state| (state & !LOCKED) - PIN);
    }
}

/// The parts of the cache that take pages from a disk's memory.
impl<Device: BlockDevice> CacheShared<Device> {
    /// A zeroed page of `disk`'s memory, for one cache block.
    fn new_page(&self, disk: usize) -> Result<Device::Memory, DmaError> {
        let page = self.devices[disk].memory(CACHE_BLOCK_BYTES)?;
        self.pages.fetch_add(1, Ordering::Relaxed);
        Ok(page)
    }

    /// An entry this caller owns under [`RECLAIM`], holding a page of
    /// `disk`'s memory, or `None` when [`CacheCore::take_entry`] finds
    /// none.
    ///
    /// Fails when the disk's memory refuses the page the entry needs; the
    /// entry goes back to the free list.
    fn try_claim(&self, disk: usize, urgent: bool) -> Result<Option<u32>, DmaError> {
        let Some((id, holder)) = self.take_entry(urgent) else {
            return Ok(None);
        };
        // A recycled page of the same disk is reused as it is: a read
        // fills it whole and an overwrite zeroes it before anyone sees its
        // bytes. A page of another disk is that disk's memory, so it goes
        // back and the entry takes one of this disk's.
        //
        // SAFETY: this task owns the entry under `RECLAIM`, and no
        // reference into its slot lives.
        let reusable =
            holder == Some(disk) && matches!(unsafe { self.entry(id).slot() }, Slot::Buffer(_));
        if !reusable {
            let page = match self.new_page(disk) {
                Ok(page) => page,
                Err(error) => {
                    self.push_free(id);
                    return Err(error);
                }
            };
            // SAFETY: as above.
            let slot = unsafe { self.entry(id).slot_mut() };
            let previous = core::mem::replace(slot, Slot::Buffer(page));
            self.release_slot(previous);
        }
        Ok(Some(id))
    }

    /// Moves the pinned, dirty entry `id` of `disk` into write-back, with
    /// a share and a pin of the write's own. A block held compressed is
    /// decompressed into a page of the disk's memory staged for the
    /// write, and keeps its compressed page.
    ///
    /// Answers `None`, doing nothing, when the entry is clean, held, or
    /// already in flight. Fails when the disk's memory refuses the
    /// staging page; the entry is left as it was, dirty and compressed.
    fn start_writeback(
        &self,
        disk: usize,
        id: u32,
    ) -> Result<Option<Option<Device::Memory>>, DmaError> {
        let entry = self.entry(id);
        let mut current = entry.state.load(Ordering::Acquire);
        loop {
            if current & DIRTY == 0 || current & (LOCKED | WRITEBACK | RECLAIM) != 0 {
                return Ok(None);
            }
            let next = if current & COMPRESSED == 0 {
                ((current & !DIRTY) | WRITEBACK) + SHARE + PIN
            } else {
                current | LOCKED
            };
            match entry.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) if current & COMPRESSED == 0 => return Ok(Some(None)),
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        let mut staged = match self.new_page(disk) {
            Ok(page) => page,
            Err(error) => {
                self.transition(id, |state| state & !LOCKED);
                return Err(error);
            }
        };
        // SAFETY: this task holds the entry `LOCKED`, so nobody replaces
        // its slot while the compressed page is read.
        match unsafe { entry.slot() } {
            Slot::Compressed(page) => page.decompress_into(block_of_mut(&mut staged)),
            Slot::Empty | Slot::Buffer(_) => {
                panic!("block cache entry {id} is marked compressed without a compressed page")
            }
        }
        self.transition(id, |state| {
            ((state & !(LOCKED | DIRTY)) | WRITEBACK) + SHARE + PIN
        });
        Ok(Some(Some(staged)))
    }

    /// Starts the write-back of the next block of `disk` after or
    /// before a run, if it is cached and dirty.
    ///
    /// A neighbour whose staging page the disk's memory refuses ends the
    /// run there, as a held one does: it stays dirty and in its disk's
    /// dirty set, and its own write-back as a seed is where the refusal
    /// is answered.
    fn start_neighbour(&self, disk: usize, block: u64) -> Option<(u32, Option<Device::Memory>)> {
        let id = self.find(key_of(disk, block), Take::Pin)?;
        let started = self.start_writeback(disk, id);
        self.unpin(id);
        started.ok().flatten().map(|staged| (id, staged))
    }

    /// Starts the write-back of the entry `seed` and of the dirty blocks
    /// next to it on its disk, as far as one request carries. Answers
    /// `None` when `seed` cannot be written now, and fails when the
    /// disk's memory refuses the page `seed`'s write is staged in.
    fn start_write_run(&self, seed: u32) -> Result<Option<WriteRun<Device::Memory>>, DmaError> {
        if !self.try_take(seed, Take::Pin) {
            return Ok(None);
        }
        let _held = Held {
            cache: self,
            id: seed,
        };
        let key = self.entry(seed).key.load(Ordering::Acquire);
        if key == NO_KEY {
            return Ok(None);
        }
        let disk = key_disk(key);
        let state = &self.disks[disk];
        if state.is_aborted() {
            self.discard(seed);
            return Ok(None);
        }
        let block = key_block(key);
        let Some(staged) = self.start_writeback(disk, seed)? else {
            return Ok(None);
        };
        let mut run = WriteRun::new(disk, block, seed, staged);
        let limit = state.layout.run_blocks;
        let mut after = block + 1;
        while run.ids.len() < limit && after < state.layout.capacity_blocks {
            let Some((id, staged)) = self.start_neighbour(disk, after) else {
                break;
            };
            run.push_back(id, staged);
            after += 1;
        }
        while run.ids.len() < limit && run.first_block > 0 {
            let Some((id, staged)) = self.start_neighbour(disk, run.first_block - 1) else {
                break;
            };
            run.push_front(id, staged);
        }
        Ok(Some(run))
    }

    /// Decompresses the compressed entry `id` of `disk` into a fresh page
    /// of the disk's memory, so it is valid again with its dirtiness
    /// unchanged. Answers false, doing nothing, when the entry is not
    /// compressed or another task owns it; that task's release wakes
    /// anyone waiting on the entry. Fails when the disk's memory refuses
    /// the page; the entry is left compressed.
    ///
    /// The caller holds a pin.
    fn try_inflate(&self, disk: usize, id: u32) -> Result<bool, DmaError> {
        let entry = self.entry(id);
        let mut current = entry.state.load(Ordering::Acquire);
        loop {
            if current & COMPRESSED == 0 || current & (LOCKED | RECLAIM) != 0 {
                return Ok(false);
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
        let mut page = match self.new_page(disk) {
            Ok(page) => page,
            Err(error) => {
                self.transition(id, |state| state & !LOCKED);
                return Err(error);
            }
        };
        // SAFETY: this task holds the entry `LOCKED`. Shares are granted
        // only on a valid buffer, and a write-back in flight reads the
        // copy it staged, so no reference into the slot lives.
        let slot = unsafe { entry.slot_mut() };
        match &*slot {
            Slot::Compressed(compressed) => compressed.decompress_into(block_of_mut(&mut page)),
            Slot::Empty | Slot::Buffer(_) => {
                panic!("block cache entry {id} is marked compressed without a compressed page")
            }
        }
        let previous = core::mem::replace(slot, Slot::Buffer(page));
        self.release_slot(previous);
        self.transition(id, |state| (state & !(LOCKED | COMPRESSED)) | VALID);
        Ok(true)
    }
}

impl<Memory: PageMemory> CacheCore<Memory> {
    /// Compresses the idle, valid entry `id`, last seen in state
    /// `current`, into the pool and frees its buffer.
    fn compress_entry(&self, id: u32, current: u64) -> Demotion {
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
            return Demotion::Busy;
        }
        // SAFETY: this task holds the entry `LOCKED`, and it was idle, so
        // no share is outstanding.
        let slot = unsafe { entry.slot_mut() };
        let compressed = match &*slot {
            Slot::Buffer(page) => self.pool.compress(block_of(page)),
            Slot::Empty | Slot::Compressed(_) => {
                panic!("block cache entry {id} is valid without a buffer")
            }
        };
        match compressed {
            Ok(page) => {
                let previous = core::mem::replace(slot, Slot::Compressed(page));
                self.release_slot(previous);
                self.transition(id, |state| {
                    (state & !(LOCKED | VALID | REFERENCED)) | COMPRESSED
                });
                Demotion::Compressed
            }
            Err(refusal) => {
                let state = self.transition(id, |state| state & !LOCKED);
                Demotion::Refused(refusal, state)
            }
        }
    }
}

impl<Memory> CacheCore<Memory> {
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
        self.push_free(id);
        true
    }

    /// A finished read: the entry is valid, or the read failed. The
    /// read's pin goes back.
    fn complete_read(&self, id: u32, succeeded: bool) {
        self.transition(id, |state| {
            let state = (state & !LOCKED) - PIN;
            if succeeded {
                (state | VALID) & !FAILED
            } else {
                (state | FAILED) & !VALID
            }
        });
    }

    /// A finished write-back. A failed one leaves the block dirty, so the
    /// next sync tries again and reports the failure, unless the disk was
    /// aborted, in which case nothing will try again.
    fn complete_write(&self, disk: usize, id: u32, succeeded: bool) {
        let redirty = !succeeded && !self.disks[disk].is_aborted();
        self.transition(id, |state| {
            let state = (state & !WRITEBACK) - SHARE - PIN;
            if redirty { state | DIRTY } else { state }
        });
        if redirty {
            self.mark_dirty(disk, id);
        }
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

    /// Entries nobody holds and no request or move owns, of those the
    /// cache has created; with no task running, every resident entry.
    #[cfg(test)]
    fn idle_blocks(&self) -> usize {
        let allocated = self.allocated.load(Ordering::Acquire);
        (0..allocated)
            .filter(|&id| {
                let state = self.entry(id).state.load(Ordering::Acquire);
                state & RECLAIM == 0 && idle(state) && shares(state) == 0
            })
            .count()
    }
}

/// Building the requests the cache submits. Every one is built without
/// an `.await` between taking its entries and handing them to its
/// [`BlockPages`], so a task dropped at any `.await` leaves no entry
/// held by a request that does not exist.
impl<Device: BlockDevice> CacheShared<Device> {
    /// The request for the read run `run`, whose entries this task locked.
    fn read_pages(&self, run: ReadRun) -> DevicePages<Device> {
        let mut pages = BlockPages::for_run(
            CacheRun {
                cache: Arc::clone(&self.core),
                disk: run.disk,
                first_block: run.first_block,
                ids: run.ids,
                kind: RunKind::Read,
                staged: 0,
                answered: false,
            },
            0,
        );
        for index in 0..pages.run_blocks() {
            let id = pages.run_id(index);
            // SAFETY: the run holds the entry `LOCKED` with no share
            // outstanding, and a locked entry claimed for a read holds a
            // buffer, which nobody replaces until the read completes.
            match unsafe { self.entry(id).slot_mut() } {
                Slot::Buffer(page) => pages.push_borrowed(page),
                Slot::Empty | Slot::Compressed(_) => {
                    panic!("block cache entry {id} was locked for a read without a buffer")
                }
            }
        }
        pages
    }

    /// The request for the write run `run`, whose entries are in
    /// write-back with a share of the run's own.
    fn write_pages(&self, run: WriteRun<Device::Memory>, kind: RunKind) -> DevicePages<Device> {
        let WriteRun {
            disk,
            first_block,
            ids,
            staged,
        } = run;
        let staged_pages = staged.iter().flatten().count();
        let mut pages = BlockPages::for_run(
            CacheRun {
                cache: Arc::clone(&self.core),
                disk,
                first_block,
                ids: ids.clone(),
                kind,
                staged: staged_pages,
                answered: false,
            },
            staged_pages,
        );
        for (id, staged) in ids.into_iter().zip(staged) {
            match staged {
                Some(page) => pages.push_staged(page),
                None => {
                    // SAFETY: the run holds a share of the entry, so it
                    // holds a valid buffer that nobody writes or replaces
                    // until the write completes; the device only reads it.
                    match unsafe { self.entry(id).slot() } {
                        Slot::Buffer(page) => pages.push_borrowed(page),
                        Slot::Empty | Slot::Compressed(_) => {
                            panic!("block cache entry {id} is in write-back without a buffer")
                        }
                    }
                }
            }
        }
        pages
    }

    /// Starts the write-back of `seed` with its dirty neighbours, as one
    /// request, if it can be written now. Fails when the disk's memory
    /// refuses the page `seed`'s write is staged in.
    fn start_write_back(&self, seed: u32) -> Result<Option<DevicePages<Device>>, DmaError> {
        Ok(self
            .start_write_run(seed)?
            .map(|run| self.write_pages(run, RunKind::WriteBack)))
    }
}

/// The parts of the cache that talk to the device.
impl<Device> CacheShared<Device>
where
    Device: BlockDevice<Buffer = DevicePages<Device>>,
{
    /// Reads the run `pages` carries into its entries. Its completion
    /// makes them valid or failed, whether or not this task is still
    /// there to see it. Answers whether the device delivered the blocks.
    async fn read_run(&self, pages: DevicePages<Device>) -> bool {
        let (disk, first_block, blocks) = pages.run_target();
        let state = &self.disks[disk];
        let first = first_block as usize * state.layout.device_blocks_per_block;
        // The buffer comes back either way and goes here: its run was
        // completed by the answer, so dropping it only gives back what
        // the request held.
        match self.devices[disk].read(first, pages).await {
            Ok(((), _pages)) => true,
            Err(failure) => {
                let error = failure.error;
                tracing::warn!(?error, serial = ?state.serial, block = first_block, blocks, "block cache read failed");
                false
            }
        }
    }

    /// Writes the run `pages` carries. Its completion makes the blocks
    /// clean, dirty again or stale, and records a failure for the next
    /// sync, whether or not this task is still there to see it.
    async fn write_run(&self, pages: DevicePages<Device>) -> Result<(), IoError> {
        let (disk, first_block, blocks) = pages.run_target();
        let state = &self.disks[disk];
        let first = first_block as usize * state.layout.device_blocks_per_block;
        match self.devices[disk].write(first, pages).await {
            Ok(((), _pages)) => Ok(()),
            Err(failure) => {
                let error = failure.error;
                tracing::warn!(?error, serial = ?state.serial, block = first_block, blocks, "block cache write-back failed");
                Err(error)
            }
        }
    }

    /// Writes the entry `seed` back with its dirty neighbours, if it can
    /// be written now.
    ///
    /// Fails only when the disk's memory refuses the page `seed`'s write
    /// is staged in, which leaves it dirty; a write the device fails is
    /// counted in the disk's error sequence, which the next sync reports.
    async fn write_back(&self, seed: u32) -> Result<(), DmaError> {
        if let Some(pages) = self.start_write_back(seed)? {
            let _ = self.write_run(pages).await;
        }
        Ok(())
    }

    /// Commits the device's write cache.
    async fn flush_device(&self, disk: usize) -> Result<(), IoError> {
        let state = &self.disks[disk];
        let result = self.devices[disk].flush().await;
        state.count(|counters| &counters.flushes);
        if let Err(error) = result {
            state.record_error(error);
            tracing::warn!(?error, serial = ?state.serial, "block cache flush failed");
        }
        result
    }

    /// An entry for a miss on `disk`, holding a page of its memory,
    /// writing idle dirty blocks back when the cache holds nothing it can
    /// recycle.
    ///
    /// Fails when a disk's memory refuses a page the claim needs: the
    /// entry's own, or the one an idle dirty compressed block's write-back
    /// is staged in.
    async fn claim(&self, disk: usize) -> Result<u32, DmaError> {
        loop {
            if let Some(id) = self.try_claim(disk, true)? {
                return Ok(id);
            }
            // Every entry is pinned, dirty or in flight and the cache is
            // at its bound. Wait until an entry comes free or one goes
            // idle dirty, which this task then writes back itself.
            let mut claimed = Ok(None);
            let mut dirty = None;
            self.reclaimable
                .wait_until(|| {
                    claimed = self.try_claim(disk, true);
                    if matches!(claimed, Ok(None)) {
                        dirty = self.find_idle_dirty();
                    }
                    !matches!(claimed, Ok(None)) || dirty.is_some()
                })
                .await;
            if let Some(id) = claimed? {
                return Ok(id);
            }
            if let Some(id) = dirty {
                self.write_back(id).await?;
            }
        }
    }

    /// Takes the idle entry `id`, last seen in state `current`, out of
    /// the cache: dropped when it is clean, written back first when it
    /// is dirty. Answers whether it left.
    async fn evict(&self, id: u32, current: u64) -> bool {
        if current & DIRTY == 0 {
            return self.drop_entry(id, current);
        }
        if let Err(error) = self.write_back(id).await {
            // A reclaim pass answers what it gave back and has nobody to
            // fail to: the block stays dirty and cached, and the next sync
            // of its disk writes it or answers the refusal.
            tracing::warn!(
                ?error,
                "block cache could not stage a compressed dirty block for its write-back; it stays cached"
            );
            return false;
        }
        let current = self.entry(id).state.load(Ordering::SeqCst);
        self.drop_entry(id, current)
    }

    /// Evicts the pool's coldest pages until `needed` more compressed
    /// bytes fit, or every entry has been looked at once.
    async fn make_pool_room(&self, needed: usize) {
        let allocated = self.allocated.load(Ordering::Acquire);
        for _ in 0..allocated {
            if self.pool.has_room_for(needed) {
                return;
            }
            let id = self.hand.fetch_add(1, Ordering::Relaxed) % allocated;
            let current = self.entry(id).state.load(Ordering::SeqCst);
            if current & COMPRESSED == 0 || !idle(current) {
                continue;
            }
            self.evict(id, current).await;
        }
    }

    /// Moves the idle, valid entry `id`, last seen in state `current`,
    /// into the compressed tier. A pool at its limit evicts its own
    /// coldest pages and is offered the block once more; a block the
    /// pool still refuses skips the tier and leaves the cache. Answers
    /// whether the entry's buffer was freed.
    async fn demote(&self, id: u32, current: u64) -> bool {
        let state = match self.compress_entry(id, current) {
            Demotion::Compressed => return true,
            Demotion::Busy => return false,
            Demotion::Refused(CompressRefusal::Incompressible, state) => state,
            Demotion::Refused(CompressRefusal::Full { needed, .. }, _) => {
                self.make_pool_room(needed).await;
                let current = self.entry(id).state.load(Ordering::SeqCst);
                if !idle(current) || current & VALID == 0 {
                    return false;
                }
                match self.compress_entry(id, current) {
                    Demotion::Compressed => return true,
                    Demotion::Busy => return false,
                    Demotion::Refused(_, state) => state,
                }
            }
        };
        idle(state) && self.evict(id, state).await
    }

    /// One reclaim pass; see the module's *Memory* section. Answers the
    /// entries whose memory it gave back.
    async fn shrink(&self, level: PressureLevel) -> usize {
        let allocated = self.allocated.load(Ordering::Acquire);
        let budget = match level {
            PressureLevel::Green => return 0,
            PressureLevel::Yellow => (allocated as usize / 8).max(1),
            PressureLevel::Red => allocated as usize,
        };
        let mut released = 0;
        for _ in 0..allocated {
            if released >= budget {
                break;
            }
            let id = self.hand.fetch_add(1, Ordering::Relaxed) % allocated;
            let state = &self.entry(id).state;
            let current = state.load(Ordering::SeqCst);
            if !idle(current) {
                continue;
            }
            if current & DIRTY != 0 && self.disk_aborted(id) {
                self.discard(id);
                continue;
            }
            let freed = if current & COMPRESSED != 0 {
                // Compressed blocks hold no buffer; red empties the tier.
                level == PressureLevel::Red && self.evict(id, current).await
            } else if !readable(current) {
                // A failed read or a stale commit block: nothing worth
                // keeping.
                self.drop_entry(id, current)
            } else if level == PressureLevel::Red {
                self.evict(id, current).await
            } else if current & REFERENCED != 0 {
                let _ = state.compare_exchange(
                    current,
                    current & !REFERENCED,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                false
            } else {
                self.demote(id, current).await
            };
            if freed {
                released += 1;
            }
        }
        released
    }

    /// The run a sync's first pass starts for the entry `id` it took out
    /// of `disk`'s dirty set, or why it starts none.
    fn sync_candidate(&self, disk: usize, id: u32) -> Result<DevicePages<Device>, Skip> {
        let pending = |state: u64| state & (DIRTY | WRITEBACK) != 0;
        if !pending(self.entry(id).state.load(Ordering::SeqCst)) {
            return Err(Skip::Clean);
        }
        let key = self.entry(id).key.load(Ordering::Acquire);
        if key == NO_KEY || key_disk(key) != disk {
            return Err(Skip::Clean);
        }
        match self.start_write_back(id) {
            Ok(Some(pages)) => return Ok(pages),
            Ok(None) => {}
            Err(_) => return Err(Skip::Refused),
        }
        if pending(self.entry(id).state.load(Ordering::SeqCst)) {
            Err(Skip::Busy)
        } else {
            Err(Skip::Clean)
        }
    }

    /// Makes the entry `id`, taken out of `disk`'s dirty set, durable as
    /// far as a write does: waits out its writer or its write in flight,
    /// and writes it once if it is still dirty. Dirt that comes back after
    /// that write is left in the set for the next sync.
    ///
    /// Fails when the disk's memory refuses the page the block's write is
    /// staged in; the block stays dirty and goes back in the set.
    async fn settle(&self, disk: usize, id: u32) -> Result<(), DmaError> {
        if !self.try_take(id, Take::Pin) {
            // Being recycled: it was clean.
            return Ok(());
        }
        let _held = Held { cache: self, id };
        let key = self.entry(id).key.load(Ordering::Acquire);
        if key == NO_KEY || key_disk(key) != disk {
            return Ok(());
        }
        let mut written = false;
        loop {
            let state = self
                .wait_state(id, |state| state & (LOCKED | WRITEBACK) == 0)
                .await;
            if state & DIRTY == 0 {
                return Ok(());
            }
            if self.disks[disk].is_aborted() {
                self.discard(id);
                return Ok(());
            }
            if written {
                self.mark_dirty(disk, id);
                return Ok(());
            }
            // A run that cannot start now is retried once the task that
            // took the entry meanwhile lets it go.
            match self.start_write_back(id) {
                Ok(Some(pages)) => {
                    written = true;
                    // A failure is counted in the disk's error sequence.
                    let _ = self.write_run(pages).await;
                }
                Ok(None) => {}
                Err(error) => {
                    self.mark_dirty(disk, id);
                    return Err(error);
                }
            }
        }
    }

    /// The work of the sync leading `disk`: every block in the dirty set
    /// when it starts reaches the disk, then the device's write cache is
    /// committed.
    ///
    /// A block whose staging page the disk's memory refuses is left dirty
    /// for the next sync, the refusal is recorded as the disk's last
    /// failure, and the rest of the sync still runs; the sync then fails.
    async fn lead_sync(&self, disk: usize) -> Result<(), BlockCacheError> {
        let state = &self.disks[disk];
        let set = &state.dirty;
        // First pass: start a run for every dirty block nobody holds,
        // keeping a bounded number of requests in flight.
        {
            let mut runs = pin!(InFlight::<_, SYNC_RUNS_IN_FLIGHT>::new());
            let mut drain = Drain::new(set);
            while let Some(id) = drain.next() {
                match self.sync_candidate(disk, id) {
                    Err(Skip::Clean) => {}
                    Err(Skip::Busy | Skip::Refused) => set.insert(id),
                    Ok(pages) => {
                        // The request owns its blocks from here: a sync
                        // dropped with it still queued puts them back in
                        // the dirty set, and one dropped with it in flight
                        // leaves its completion to finish them.
                        runs.as_mut().push(self.write_run(pages));
                        if runs.len() == SYNC_RUNS_IN_FLIGHT {
                            // Full: the next run waits for one to finish.
                            poll_fn(|context| runs.as_mut().poll_next(context)).await;
                        }
                    }
                }
            }
            while poll_fn(|context| runs.as_mut().poll_next(context))
                .await
                .is_some()
            {}
        }
        // Second pass: whatever was held, in flight elsewhere, or failed
        // is waited for and written.
        let mut refused = None;
        let mut drain = Drain::new(set);
        while let Some(id) = drain.next() {
            if let Err(error) = self.settle(disk, id).await {
                tracing::warn!(?error, serial = ?state.serial, "block cache sync could not stage a compressed dirty block for its write");
                let failure = BlockCacheError::Memory(error);
                state.record_failure(failure);
                refused = Some(failure);
            }
        }
        let flushed = self.flush_device(disk).await;
        match refused {
            Some(failure) => Err(failure),
            None => flushed.map_err(BlockCacheError::Device),
        }
    }
}

/// The kernel's block cache, shared by every mount-source disk, which
/// reaches them through the backend's device type.
pub struct BlockCache<Device: BlockDevice> {
    shared: Arc<CacheShared<Device>>,
}

impl<Device: BlockDevice> Clone for BlockCache<Device> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<Device> BlockCache<Device>
where
    Device: BlockDevice<Buffer = DevicePages<Device>>,
{
    /// A cache over `disks`, which keep their order.
    pub(super) fn new(
        limits: BlockCacheLimits,
        processors: usize,
        pool: CompressedPool,
        disks: impl IntoIterator<Item = CacheDisk<Device>>,
    ) -> Self {
        Self {
            shared: Arc::new(CacheShared::new(limits, processors, pool, disks)),
        }
    }

    /// The pool cold blocks are compressed into, whose limit and level
    /// are operator settings.
    pub fn compressed_pool(&self) -> &CompressedPool {
        &self.shared.pool
    }

    /// The view of one disk; `index` is its position in construction.
    pub fn disk(&self, index: usize) -> CachedDisk<Device> {
        assert!(
            index < self.shared.disks.len(),
            "block cache has no disk {index}"
        );
        CachedDisk {
            cache: self.clone(),
            disk: index,
        }
    }

    /// Every disk the cache serves.
    pub fn disks(&self) -> impl Iterator<Item = CachedDisk<Device>> + '_ {
        (0..self.shared.disks.len()).map(|index| self.disk(index))
    }

    /// The mount-source disk that reported `serial`.
    pub fn disk_by_serial(&self, serial: &[u8]) -> Option<CachedDisk<Device>> {
        self.disks().find(|disk| disk.serial().as_bytes() == serial)
    }

    /// Records the pressure the machine is under. Growth stops at
    /// anything above green.
    pub fn set_pressure(&self, level: PressureLevel) {
        self.shared.pressure.store(level as u8, Ordering::Relaxed);
    }

    /// Gives memory nobody holds back to the disks it came from. Under yellow
    /// pressure the coldest eighth of the cache is compressed into the
    /// [`CompressedPool`], which evicts its own coldest pages when it is
    /// full, and a block it still refuses leaves the cache; under red
    /// every block nobody holds leaves, dirty ones once written. Answers
    /// the entries that gave their memory back.
    pub async fn shrink(&self, level: PressureLevel) -> usize {
        self.shared.shrink(level).await
    }

    /// Blocks the cache holds right now, in buffers or compressed.
    pub fn resident_blocks(&self) -> usize {
        self.shared.resident_blocks()
    }

    /// Blocks the cache holds compressed right now.
    pub fn compressed_blocks(&self) -> usize {
        self.shared.compressed_blocks()
    }

    /// Page buffers the cache holds right now, pages staged for a write
    /// included.
    pub fn page_buffers(&self) -> usize {
        self.shared.pages.load(Ordering::Relaxed)
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
pub struct CachedDisk<Device: BlockDevice> {
    cache: BlockCache<Device>,
    disk: usize,
}

impl<Device: BlockDevice> Clone for CachedDisk<Device> {
    fn clone(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            disk: self.disk,
        }
    }
}

/// A block a [`CachedDisk::read`] holds; the bytes cannot change while
/// it lives.
///
/// `Memory` is the disk's [`BlockDevice::Memory`], which holds the bytes.
pub struct BlockRef<'a, Memory> {
    cache: &'a CacheCore<Memory>,
    id: u32,
    bytes: &'a [u8; CACHE_BLOCK_BYTES],
}

/// A block a [`CachedDisk::write`] or [`CachedDisk::overwrite`] holds
/// exclusively. Dropping it leaves the block dirty.
///
/// `Memory` is the disk's [`BlockDevice::Memory`], which holds the bytes.
pub struct BlockMut<'a, Memory> {
    cache: &'a CacheCore<Memory>,
    disk: usize,
    id: u32,
    bytes: &'a mut [u8; CACHE_BLOCK_BYTES],
}

/// A pin this task holds, given back when it is dropped.
struct Held<'a, Memory> {
    cache: &'a CacheCore<Memory>,
    id: u32,
}

impl<Memory> Drop for Held<'_, Memory> {
    fn drop(&mut self) {
        self.cache.unpin(self.id);
    }
}

impl<Device> CachedDisk<Device>
where
    Device: BlockDevice<Buffer = DevicePages<Device>>,
{
    fn shared(&self) -> &CacheShared<Device> {
        &self.cache.shared
    }

    fn state(&self) -> &DiskState {
        &self.shared().disks[self.disk]
    }

    /// The serial the disk reported.
    pub fn serial(&self) -> BlockSerial {
        self.state().serial
    }

    /// Cache blocks the disk holds.
    pub fn capacity_blocks(&self) -> u64 {
        self.state().layout.capacity_blocks
    }

    /// Whether the disk refuses writes.
    pub fn is_read_only(&self) -> bool {
        self.state().read_only
    }

    /// Whether a failed or abandoned commit aborted the disk.
    pub fn is_aborted(&self) -> bool {
        self.state().is_aborted()
    }

    pub fn stats(&self) -> CachedDiskStats {
        self.state().stats()
    }

    fn key(&self, block: u64) -> Result<u64, BlockCacheError> {
        let capacity_blocks = self.capacity_blocks();
        if block >= capacity_blocks {
            return Err(BlockCacheError::OutOfRange {
                block,
                capacity_blocks,
            });
        }
        Ok(key_of(self.disk, block))
    }

    fn writable(&self) -> Result<(), BlockCacheError> {
        if self.is_read_only() {
            return Err(BlockCacheError::ReadOnly);
        }
        if self.is_aborted() {
            return Err(BlockCacheError::Aborted);
        }
        Ok(())
    }

    /// How the disk last failed: a device error, or a page its memory
    /// refused a sync.
    fn device_error(&self) -> BlockCacheError {
        self.state().last_failure()
    }

    /// The block, read from the disk if the cache does not hold it.
    pub async fn read(&self, block: u64) -> Result<BlockRef<'_, Device::Memory>, BlockCacheError> {
        let key = self.key(block)?;
        let cache = self.shared();
        if let Some(id) = cache.lookaside_find(key, Take::Share) {
            return Ok(self.block_ref(id));
        }
        if let Some(id) = cache.find(key, Take::Share) {
            cache.remember(key, id);
            return Ok(self.block_ref(id));
        }
        let (held, inserted) = self.hold(key).await?;
        if inserted && !self.read_miss(held.id, block).await {
            return Err(self.device_error());
        }
        cache.remember(key, held.id);
        loop {
            let state = cache.entry(held.id).state.load(Ordering::Acquire);
            if state & LOCKED != 0 {
                let state = cache.wait_state(held.id, |state| state & LOCKED == 0).await;
                if state & (VALID | FAILED) == FAILED {
                    return Err(self.device_error());
                }
                continue;
            }
            if state & COMPRESSED != 0 {
                // A hit in the compressed tier: no device read.
                cache
                    .try_inflate(self.disk, held.id)
                    .map_err(BlockCacheError::Memory)?;
                continue;
            }
            if state & STALE != 0 && (shares(state) != 0 || state & WRITEBACK != 0) {
                // A commit block that did not reach the disk, still in the
                // hands of readers who had it before: read afresh once
                // they let it go.
                cache
                    .wait_state(held.id, |state| {
                        state & STALE == 0
                            || (shares(state) == 0 && state & (LOCKED | WRITEBACK) == 0)
                    })
                    .await;
                continue;
            }
            if !readable(state) {
                if self.try_lock_for_read(held.id, state) && !self.read_one(held.id, block).await {
                    return Err(self.device_error());
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
    pub async fn write(&self, block: u64) -> Result<BlockMut<'_, Device::Memory>, BlockCacheError> {
        self.writable()?;
        let key = self.key(block)?;
        let (held, inserted) = self.hold(key).await?;
        if inserted && !self.read_one(held.id, block).await {
            return Err(self.device_error());
        }
        self.exclusive(held, block, true).await
    }

    /// The block for modification without reading it: the caller is
    /// about to write every byte. It starts zeroed, whatever the cache
    /// held for it.
    pub async fn overwrite(
        &self,
        block: u64,
    ) -> Result<BlockMut<'_, Device::Memory>, BlockCacheError> {
        self.writable()?;
        let key = self.key(block)?;
        let (held, inserted) = self.hold(key).await?;
        if inserted {
            // Inserted locked: this task already owns the buffer.
            let id = held.id;
            core::mem::forget(held);
            return Ok(self.block_mut(id, true));
        }
        self.exclusive(held, block, false).await
    }

    /// Writes every block dirtied before the call and commits the
    /// device's write cache.
    pub async fn sync(&self) -> Result<(), BlockCacheError> {
        if self.is_read_only() {
            return Ok(());
        }
        if self.is_aborted() {
            return Err(BlockCacheError::Aborted);
        }
        let state = self.state();
        let turn = &*state.turn;
        let errors_before = state.errors.writes.load(Ordering::Acquire);
        let ticket = turn.requested.fetch_add(1, Ordering::SeqCst) + 1;
        loop {
            if turn.completed.load(Ordering::SeqCst) >= ticket {
                break;
            }
            if turn.failed.load(Ordering::SeqCst) >= ticket {
                return Err(self.device_error());
            }
            if turn
                .leading
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let leading = Leading(turn);
                // Every ticket numbered up to here was asked for after
                // the dirt it covers, and the dirty set is drained after
                // this read.
                let covered = turn.requested.load(Ordering::SeqCst);
                match self.shared().lead_sync(self.disk).await {
                    Ok(()) => turn.completed.fetch_max(covered, Ordering::SeqCst),
                    Err(_) => turn.failed.fetch_max(covered, Ordering::SeqCst),
                };
                drop(leading);
                continue;
            }
            turn.finished
                .wait_until(|| {
                    turn.completed.load(Ordering::SeqCst) >= ticket
                        || turn.failed.load(Ordering::SeqCst) >= ticket
                        || !turn.leading.load(Ordering::SeqCst)
                })
                .await;
        }
        if state.errors.writes.load(Ordering::Acquire) != errors_before {
            return Err(self.device_error());
        }
        if self.is_aborted() {
            return Err(BlockCacheError::Aborted);
        }
        Ok(())
    }

    /// The commit point of a filesystem transaction: every block dirtied
    /// before the call reaches stable storage, then `block`, filled by
    /// `fill` from its current contents, is written and committed.
    ///
    /// The device sees the dirty blocks, a flush, the commit block and a
    /// second flush, in that order, so a crash leaves either the old
    /// commit block or the new one with everything it refers to. A
    /// commit that fails or is dropped before it finished aborts the
    /// disk.
    pub async fn commit(
        &self,
        block: u64,
        fill: impl FnOnce(&mut [u8; CACHE_BLOCK_BYTES]),
    ) -> Result<(), BlockCacheError> {
        self.writable()?;
        let mut committing = Committing {
            cache: self.shared(),
            disk: self.disk,
            finished: false,
        };
        let result = self.commit_in_order(block, fill).await;
        committing.finished = result.is_ok();
        result
    }

    async fn commit_in_order(
        &self,
        block: u64,
        fill: impl FnOnce(&mut [u8; CACHE_BLOCK_BYTES]),
    ) -> Result<(), BlockCacheError> {
        self.sync().await?;
        let mut guard = self.write(block).await?;
        fill(&mut guard);
        let id = guard.id;
        core::mem::forget(guard);
        let cache = self.shared();
        // Hand the buffer from the writer to the write: readable, not
        // dirty, with a share and a pin of the write's own. The writer's
        // pin is this task's until the write completes.
        cache.transition(id, |state| {
            ((state & !(LOCKED | FAILED | DIRTY | STALE)) | VALID | REFERENCED | WRITEBACK)
                + SHARE
                + PIN
        });
        let held = Held { cache, id };
        let pages = cache.write_pages(WriteRun::new(self.disk, block, id, None), RunKind::Commit);
        let written = cache.write_run(pages).await;
        drop(held);
        written.map_err(BlockCacheError::Device)?;
        cache
            .flush_device(self.disk)
            .await
            .map_err(BlockCacheError::Device)
    }

    fn block_ref(&self, id: u32) -> BlockRef<'_, Device::Memory> {
        let cache = self.shared();
        BlockRef {
            cache,
            id,
            // SAFETY: the caller holds a share, so the entry holds a valid
            // buffer that no writer changes and nobody replaces until the
            // guard gives the share back.
            bytes: unsafe { cache.entry(id).bytes() },
        }
    }

    fn block_mut(&self, id: u32, zero: bool) -> BlockMut<'_, Device::Memory> {
        let cache = self.shared();
        // SAFETY: the caller holds the entry `LOCKED` with no share
        // outstanding and its slot holds a buffer, so this guard is the
        // buffer's only user until it releases the lock.
        let bytes = unsafe { cache.entry(id).bytes_mut() };
        if zero {
            bytes.fill(0);
        }
        BlockMut {
            cache,
            disk: self.disk,
            id,
            bytes,
        }
    }

    /// The entry for `key`, pinned, inserting it if the cache has none.
    /// An inserted entry comes back locked by this task and not valid.
    /// Fails when a disk's memory refuses a page the miss needs.
    async fn hold(&self, key: u64) -> Result<(Held<'_, Device::Memory>, bool), BlockCacheError> {
        let cache = self.shared();
        if let Some(id) = cache.find(key, Take::Pin) {
            return Ok((Held { cache, id }, false));
        }
        self.state().count(|counters| &counters.misses);
        let id = cache
            .claim(self.disk)
            .await
            .map_err(BlockCacheError::Memory)?;
        Ok(match cache.insert(key, id) {
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
        })
    }

    /// Locks the pinned entry, last seen unlocked in `state` with no
    /// share outstanding and either unread or stale, for a read this task
    /// issues.
    fn try_lock_for_read(&self, id: u32, state: u64) -> bool {
        self.shared()
            .entry(id)
            .state
            .compare_exchange(
                state,
                (state | LOCKED) & !(FAILED | VALID | STALE),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Reads the entry this task locked, alone. Answers whether the
    /// device delivered it.
    async fn read_one(&self, id: u32, block: u64) -> bool {
        let cache = self.shared();
        // The read gives back a pin of its own; this task's stays.
        cache.entry(id).state.fetch_add(PIN, Ordering::AcqRel);
        let mut ids = ArrayVec::new();
        ids.push(id);
        let pages = cache.read_pages(ReadRun {
            disk: self.disk,
            first_block: block,
            ids,
        });
        cache.read_run(pages).await
    }

    /// Reads the block this task inserted for a miss, together with the
    /// rest of its readahead window that the cache does not hold, in one
    /// request. Answers whether the device delivered them.
    async fn read_miss(&self, id: u32, block: u64) -> bool {
        let cache = self.shared();
        let state = self.state();
        let layout = state.layout;
        cache.entry(id).state.fetch_add(PIN, Ordering::AcqRel);
        let mut run = ReadRun {
            disk: self.disk,
            first_block: block,
            ids: ArrayVec::new(),
        };
        run.ids.push(id);
        let window = layout.readahead_blocks;
        let start = block - block % window;
        let end = (start + window).min(layout.capacity_blocks);
        // The run is contiguous: it grows from the missing block until it
        // meets a block the cache holds or cannot place without waiting.
        for neighbour in block + 1..end {
            let Some(neighbour_id) = self.claim_absent(neighbour) else {
                break;
            };
            run.ids.push(neighbour_id);
        }
        for neighbour in (start..block).rev() {
            let Some(neighbour_id) = self.claim_absent(neighbour) else {
                break;
            };
            run.ids.insert(0, neighbour_id);
            run.first_block = neighbour;
        }
        state.add(|counters| &counters.readahead, run.ids.len() as u64 - 1);
        let pages = cache.read_pages(run);
        cache.read_run(pages).await
    }

    /// An entry inserted locked for `block`, which nobody asked for yet,
    /// for readahead. `None` when the cache holds the block already or
    /// cannot place it without waiting.
    ///
    /// A page the disk's memory refuses places nothing either, and ends
    /// the readahead window there: the demand read the window rides with
    /// does not need the page, and failing it for one would fail a read
    /// the device can serve.
    fn claim_absent(&self, block: u64) -> Option<u32> {
        let cache = self.shared();
        let key = key_of(self.disk, block);
        if let Some(id) = cache.find(key, Take::Pin) {
            cache.unpin(id);
            return None;
        }
        let id = cache.try_claim(self.disk, false).ok().flatten()?;
        match cache.insert(key, id) {
            // The insertion pin is the read's.
            Insert::Inserted => Some(id),
            Insert::Existing(existing) => {
                cache.push_free(id);
                cache.unpin(existing);
                None
            }
        }
    }

    /// Takes the pinned entry exclusively, reading it first when
    /// `contents` asks for the block's bytes and starting it zeroed when
    /// it does not.
    async fn exclusive(
        &self,
        held: Held<'_, Device::Memory>,
        block: u64,
        contents: bool,
    ) -> Result<BlockMut<'_, Device::Memory>, BlockCacheError> {
        let cache = self.shared();
        let id = held.id;
        let entry = cache.entry(id);
        loop {
            let state = entry.state.load(Ordering::Acquire);
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
                if contents {
                    cache
                        .try_inflate(self.disk, id)
                        .map_err(BlockCacheError::Memory)?;
                    continue;
                }
                // The whole block is about to be replaced, so its
                // compressed bytes are dropped rather than decompressed.
                // Readers keep parking on the lock until the writer is
                // done. The page comes first, so a refusal leaves the
                // entry as it was; it starts zeroed, as the disk's memory
                // hands it out.
                let page = cache.new_page(self.disk).map_err(BlockCacheError::Memory)?;
                if entry
                    .state
                    .compare_exchange(
                        state,
                        (state & !COMPRESSED) | LOCKED | REFERENCED,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_err()
                {
                    cache.release_page(page);
                    continue;
                }
                // SAFETY: this task holds the entry `LOCKED`; a
                // compressed entry with no write-back has no share.
                let slot = unsafe { entry.slot_mut() };
                let previous = core::mem::replace(slot, Slot::Buffer(page));
                cache.release_slot(previous);
                core::mem::forget(held);
                return Ok(self.block_mut(id, false));
            }
            if contents && !readable(state) {
                // Unread, failed, or a stale commit block: the bytes come
                // from the disk first.
                if self.try_lock_for_read(id, state) && !self.read_one(id, block).await {
                    return Err(self.device_error());
                }
                continue;
            }
            if entry
                .state
                .compare_exchange(
                    state,
                    state | LOCKED | REFERENCED,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                core::mem::forget(held);
                return Ok(self.block_mut(id, !contents));
            }
        }
    }
}

impl<Memory> Deref for BlockRef<'_, Memory> {
    type Target = [u8; CACHE_BLOCK_BYTES];

    fn deref(&self) -> &Self::Target {
        self.bytes
    }
}

impl<Memory> Drop for BlockRef<'_, Memory> {
    fn drop(&mut self) {
        self.cache.release(self.id, Take::Share);
    }
}

impl<Memory> Deref for BlockMut<'_, Memory> {
    type Target = [u8; CACHE_BLOCK_BYTES];

    fn deref(&self) -> &Self::Target {
        self.bytes
    }
}

impl<Memory> DerefMut for BlockMut<'_, Memory> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.bytes
    }
}

impl<Memory> Drop for BlockMut<'_, Memory> {
    fn drop(&mut self) {
        let (previous, _) = self.cache.update(self.id, |state| {
            ((state & !(LOCKED | FAILED | STALE)) | VALID | DIRTY | REFERENCED) - PIN
        });
        if previous & DIRTY == 0 {
            self.cache.mark_dirty(self.disk, self.id);
        }
    }
}

/// The blocks the cache holds dirty, for tests that need to see its
/// state.
#[cfg(test)]
pub(super) fn dirty_blocks<Device: BlockDevice>(
    cache: &BlockCache<Device>,
) -> alloc::vec::Vec<u64> {
    let shared = &cache.shared;
    let allocated = shared.allocated.load(Ordering::Acquire);
    (0..allocated)
        .filter(|&id| shared.entry(id).state.load(Ordering::Acquire) & DIRTY != 0)
        .map(|id| key_block(shared.entry(id).key.load(Ordering::Acquire)))
        .collect()
}

/// Resident entries nobody holds, for tests that check nothing is left
/// held once every task and request is done.
#[cfg(test)]
pub(super) fn idle_blocks<Device: BlockDevice>(cache: &BlockCache<Device>) -> usize {
    cache.shared.idle_blocks()
}

/// The key naming `block` of disk `disk`.
const fn key_of(disk: usize, block: u64) -> u64 {
    (block << DISK_BITS) | disk as u64
}

/// The block a key names.
const fn key_block(key: u64) -> u64 {
    key >> DISK_BITS
}

/// The disk a key names.
const fn key_disk(key: u64) -> usize {
    (key & DISK_MASK) as usize
}
