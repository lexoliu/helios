//! A page allocator over the memory one device reaches.
//!
//! A [`DmaArena`] is kernel memory made of **chunks**: naturally aligned
//! blocks lent by a [`ChunkSource`], each already reachable by the device
//! at a known address. Pages are handed out from an atomic bitmap that
//! lives beside the chunk, on the kernel heap, and never inside it.
//!
//! # Why the metadata is out of band
//!
//! A device can write any page its domain maps, at any time, freed pages
//! included. A free list threaded through freed pages would let a faulty
//! device model redirect the allocator. Everything the allocator reads to
//! decide anything (the bitmaps, the state words, the chunk map) is on
//! the kernel heap, so a device that scribbles on its own free pages
//! corrupts bytes the next [`DmaArena::allocate`] zeroes anyway.
//!
//! # Concurrency contract
//!
//! | Operation | Runs | Synchronisation |
//! |---|---|---|
//! | allocate, one bitmap word or fewer (up to 64 pages) | task context, any processor | Lock-free: a CAS on the chunk's state word to enter, then a CAS on one bitmap word. [`DmaArena::allocate_overwritten`] is the same without the zeroing pass over the run. |
//! | allocate, a run spanning bitmap words | task context, any processor | The arena's `runs` spin lock, which only these allocations take, so two of them cannot livelock. The bits are still claimed by CAS, so a concurrent single-word claim or a free composes without it. No interrupt handler reaches the lock. |
//! | free (the drop of an [`ArenaBuffer`]) | any processor, task context or interrupt handler | No lock. Atomics of the buffer's own chunk, then a `SeqCst` fence and a relaxed load of the arena's `starved` flag. Only while an allocator is starved, one `swap` of that flag, and its winner raises the space signal: one IRQ-safe broadcast per starvation episode. |
//! | [`DmaArena::want_space`] | task context, any processor | Wait-free: samples the space signal, stores `starved`, `SeqCst` fence. |
//! | [`DmaArena::space_changed`], [`DmaArena::space_mark`], [`DmaArena::short_of_space`] | task context, any processor | Lock-free: an acquire load, and for `space_changed` a park on the space signal. |
//! | [`DmaArena::device_range`] | task context, any processor | Lock-free: two Acquire loads, the chunk map's root and one leaf entry. |
//! | growth inside an allocation | the allocating task, any processor | `grow.try_lock()`: when another task holds it the allocation answers [`DmaError::Replenishing`]. Under the guard: the source's lend (the pool's IRQ-safe lock), [`ChunkDomain::map`] (no arena lock), and publication by Release stores. No await is held. |
//! | [`DmaArena::reserve`] | task context | The arena's `grow` async mutex, which is the only writer of the chunk map and of the chunk headers. |
//! | [`DmaArena::retire_empty`] | the maintenance task | `grow.lock().await`; a `swap` of each empty-set word (AcqRel); a CAS of the chunk's state word (Acquire); the chunk map store (Release); [`ChunkDomain::unmap`]; the source's take-back (the pool's IRQ-safe lock). No await inside the guard. |
//! | release of `grow` | whoever held it | The gated wake: the lock is released, then a `SeqCst` fence, a relaxed load of `starved`, and the `swap` winner raises the space signal. An allocation whose own lend the source refused releases without it: the wake would only reach that allocator, whose own `want_space` set the flag, and consume the flag it is about to park on. |
//! | fragment allocation (an allocation of a request class under one page) | task context, the executing processor's row | One CAS on that processor's row (`frags`), which only that processor's tasks and the detach write, so it is uncontended except against a detach. A new run is claimed like any run of pages, then installed over the row by a Release CAS. |
//! | fragment free (the drop of an [`ArenaBuffer`] that is a fragment) | any processor, task context or interrupt handler | No lock. One `fetch_sub(1, AcqRel)` of its run's count in the chunk header; the subtraction that reaches zero frees the run's pages as a page free does (bitmap, state word, the gated wake). Wait-free. |
//! | detach of an idle run (first step of [`DmaArena::retire_empty`]) | the maintenance task, under `grow` | A load of the row (Acquire), a load of the run's count (Acquire), a CAS of the row to empty (Acquire), then `fetch_sub(unspent, AcqRel)` of the count. |
//! | a refused lend inside an allocation | the allocating task | A Release `fetch_or` of the class's bit in `refused`, then the growth policy's kick ([`ChunkGrowth::refused`], one interrupt-safe signal for [`Elastic`]). |
//! | [`DmaArenas::grow_refused`] | the maintenance task | [`ChunkSource::want_chunk`] first (arm), then for each arena that has a bit set: `grow.lock().await`, an AcqRel `swap` of `refused`, one growth per set class. The pass samples the demand signal ([`DmaArenas::demand_mark`]) before it reads the bits. |
//!
//! No interrupt handler allocates: every handler that touches DMA memory
//! only frees it. A buffer holds a pointer to its chunk header and nothing
//! else of the arena, so its drop cannot reach a lock. The header holds a
//! pointer to the arena's [`SpaceWaiters`], which is all the drop reaches
//! beyond its own chunk.
//!
//! # Growth and retirement
//!
//! A [`ChunkGrowth`] policy says which classes an allocation that finds no
//! room grows by lending a chunk ([`Elastic`] grows all of them, [`Fixed`]
//! only [`ChunkClass::Resident`]). A chunk is published in this order: the
//! domain maps it, its header is written, its chunk map entry is stored
//! (Release), and its state word is stored (Release) with the class and a
//! count of zero, which is what lets the first allocator enter it. A chunk
//! with no page in use is noted in the empty set by the free that emptied
//! it; [`DmaArena::retire_empty`], which only an [`Elastic`] arena has,
//! turns a noted chunk into a retired one by a CAS of its state word from
//! "this class, no pages" to `RETIRING`, then removes its map entry, unmaps
//! it and gives it back to the source. The CAS and [`Chunk::enter`] are on
//! the same word, so exactly one of an allocator and a retirement wins a
//! chunk. The header of a retired chunk is reused by a later publication
//! and its class is part of the state word, so an allocator that read the
//! chunk before it was reused for another class fails its CAS.
//!
//! # Fragments
//!
//! An allocation of a request class ([`ChunkClass::Read`] or
//! [`ChunkClass::ReadWrite`]) under one page, aligned to at most a page,
//! is a **fragment**: it is carved from a **run** of [`FRAGMENT_RUN_PAGES`]
//! pages that the executing processor caches in its row of `frags`, one row
//! per class. The row is one word packing the run (chunk slot and run
//! index), the byte offset the next fragment starts after, and the bias the
//! row still holds (see [`RunRow`]); a carve is one CAS of that word. A run
//! is claimed from the chunks like a run of pages, so it is a run of pages
//! for the bitmap, the state word and the retirement CAS, and a chunk that
//! caches a run is in use. A page or more takes the page path and never
//! touches a row. So does an allocation under a page when its row needs a
//! new run and no chunk has one free whole: it takes a free page, and the
//! arena grows for a run only when no page is free either. A fragment is
//! zeroed by [`DmaArena::allocate`] for its length only.
//!
//! Each run has a count in its chunk header, out of band like the bitmap
//! (see [`Chunk`]'s `fragment_refs`). Whoever drops the count to zero frees
//! the run's pages. [`DmaArena::retire_empty`] first detaches the rows
//! whose run has no outstanding fragment, because a cached run would
//! otherwise pin its chunk for as long as its processor stays idle.
//!
//! # Waiting for space
//!
//! A caller refused with [`DmaError::Exhausted`] calls
//! [`DmaArena::want_space`], allocates once more, and only if that is
//! refused too parks on [`DmaArena::space_changed`] with the mark
//! `want_space` returned. The protocol is Linux's `SOCK_NOSPACE` one: the
//! waiter samples the signal, stores `starved`, fences, then tests; a free
//! updates the bitmap, fences, then loads `starved`. Either the free sees
//! `starved` and signals past the waiter's mark, or the waiter's fence
//! comes after the free's bitmap update and its second allocation finds the
//! page. A free with nobody starved raises nothing.
//!
//! # Cache lines
//!
//! The [`SpaceWaiters`] line is written by a refused allocator (`starved`,
//! through `want_space`) and by whichever freeing processor wins the
//! `swap` that clears it (`starved` and the signal's generation). A free
//! with nobody starved only reads it.
//!
//! Each chunk's state word sits on its own line. It is written by the
//! processors that allocate from the chunk (usually one, through its hint)
//! and by whichever processor frees into it. Each processor's allocation
//! hints sit on a line of their own and are written only by tasks running
//! on that processor. So are its fragment rows, one padded block for both
//! classes: a row is written by the tasks running on its processor (a
//! carve, a new run), by the maintenance task's detach, and by the arena's
//! drop. A run's count is written by the processor that starts the run, by
//! whichever processor drops one of its fragments, and by the detach; the
//! counts of one chunk share lines, because they are out of band and
//! neighbouring runs are rarely dropped at the same time. The published
//! chunk count and the growth counters
//! share one line and are written only under `grow`, by whichever task
//! holds it. A chunk map leaf is written only under `grow` too, and read
//! by every processor that resolves an address.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::fmt;
use core::future::Future;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut, Range};
use core::ptr::{self, NonNull};
use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::current_processor;
use helios_hal::dma::{DeviceAddress, DmaBuffer, DmaError, DmaSpaceMark};
use helios_hal::iommu::DmaAccess;
use helios_hal::pmm::PhysFrame;

use crate::exec::{Mutex, MutexGuard, ProgressMark, ProgressSignal};

/// Bytes in one page of an arena.
const PAGE_BYTES: usize = PhysFrame::SIZE;

/// Pages one bitmap word describes.
const WORD_PAGES: usize = u64::BITS as usize;

/// Chunk headers in the first segment of the chunk table. Each later
/// segment holds twice as many as the one before.
const FIRST_SEGMENT_CHUNKS: usize = 8;

/// Segments in the chunk table, so an arena holds at most
/// `FIRST_SEGMENT_CHUNKS * (2^SEGMENT_COUNT - 1)` chunks.
const SEGMENT_COUNT: usize = 16;

const MAX_CHUNKS: usize = FIRST_SEGMENT_CHUNKS * ((1 << SEGMENT_COUNT) - 1);

/// Chunks one leaf of the chunk map describes.
const LEAF_CHUNKS: usize = 512;

/// The pages in use in a chunk's state word.
const COUNT_MASK: u64 = u32::MAX as u64;

/// The chunk is being retired: it is empty and nothing may enter it.
const RETIRING: u64 = 1 << 33;

/// Where the chunk's [`ChunkClass`] index sits in its state word.
const CLASS_SHIFT: u32 = 34;

/// The class bits of a chunk's state word.
const CLASS_MASK: u64 = 0b11 << CLASS_SHIFT;

/// Pages in one fragment run: 32 KiB, Linux's `PAGE_FRAG_CACHE_MAX_SIZE`.
/// A chunk smaller than this has one run of the whole chunk.
const FRAGMENT_RUN_PAGES: usize = 8;

/// Bytes in a run of [`FRAGMENT_RUN_PAGES`] pages, the longest a run is.
const FRAGMENT_RUN_BYTES: usize = FRAGMENT_RUN_PAGES * PAGE_BYTES;

/// The count a run starts with, and so the bias its row holds: more than
/// the most fragments a run can carve. Each fragment spends one unit; the
/// units still held are the row's, and are given up when the row lets go
/// of the run.
const BIAS: u32 = 1 << 16;

// A fragment is at least a byte, so a run carves at most
// `FRAGMENT_RUN_BYTES` of them, and a row never spends its last unit: it
// always holds at least one, and a live row is never the empty word.
const _: () = assert!((FRAGMENT_RUN_BYTES as u64) < BIAS as u64);

/// Bits of a row for the bias still held: `BIAS` itself must be
/// representable.
const HELD_BITS: u32 = BIAS.ilog2() + 1;

/// Bits of a row for the byte offset inside the run: the offset after the
/// last carve is `FRAGMENT_RUN_BYTES`, which must be representable.
const OFFSET_BITS: u32 = FRAGMENT_RUN_BYTES.ilog2() + 1;

/// Bits of a row for the chunk slot: slots are below [`MAX_CHUNKS`].
const SLOT_BITS: u32 = (MAX_CHUNKS - 1).ilog2() + 1;

/// Bits of a row for the run index: what is left of the word. It bounds a
/// chunk to [`MAX_RUNS_PER_CHUNK`] runs, 128 MiB of 32 KiB runs.
const RUN_BITS: u32 = u64::BITS - HELD_BITS - OFFSET_BITS - SLOT_BITS;

/// Runs a chunk may have, which a row's run index can name.
const MAX_RUNS_PER_CHUNK: usize = 1 << RUN_BITS;

const OFFSET_SHIFT: u32 = HELD_BITS;
const RUN_SHIFT: u32 = OFFSET_SHIFT + OFFSET_BITS;
const SLOT_SHIFT: u32 = RUN_SHIFT + RUN_BITS;

/// Request classes that cache fragments: [`ChunkClass::Read`] and
/// [`ChunkClass::ReadWrite`].
const FRAGMENT_CLASSES: usize = 2;

/// A block of memory lent to an arena.
///
/// `start` is the kernel address of the first byte: the key the arena's
/// chunk map is indexed by. It is not necessarily the block's physical
/// address, because a backend may reach RAM through an offset map; the
/// [`ChunkDomain`] turns it into whatever address its device needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LentChunk {
    start: usize,
    bytes: usize,
}

impl LentChunk {
    pub const fn new(start: usize, bytes: usize) -> Self {
        Self { start, bytes }
    }

    pub const fn start(&self) -> usize {
        self.start
    }

    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Where an arena's chunks come from and go back to.
pub trait ChunkSource: Send + Sync + 'static {
    /// One block of `bytes`, aligned to `bytes` (a power of two).
    fn lend(&self, bytes: usize) -> Option<LentChunk>;

    /// Gives a chunk back. Its pages are free and no domain maps it.
    fn take_back(&self, chunk: LentChunk);

    /// The kernel addresses every chunk this source lends lies in.
    ///
    /// Fixed for the life of the source: an arena sizes its chunk map from
    /// it.
    fn span(&self) -> Range<usize>;

    /// After a refused lend: marks the source wanted, and samples its
    /// return signal.
    ///
    /// The caller lends once more before it parks on
    /// [`ChunkSource::chunk_returned`] with the mark (AGENTS.md §4: arm,
    /// then test, then park). The source raises the signal for the next
    /// chunk to come back after the mark, whichever of its borrowers gave
    /// it back.
    fn want_chunk(&self) -> ProgressMark;

    /// Resolves once memory came back to the source after `mark`.
    ///
    /// Resolving says a lend is worth asking for again, not that it will
    /// succeed.
    fn chunk_returned(&self, mark: ProgressMark) -> impl Future<Output = ()> + Send + '_;
}

/// How one device reaches a chunk.
///
/// Called only by the holder of an arena's `grow` guard, so a domain's
/// calls for one arena never overlap.
pub trait ChunkDomain: Send + Sync + 'static {
    /// The device address of `chunk`'s first byte, which is page aligned,
    /// reachable with `access` rights.
    ///
    /// Called before the chunk is published; a refusal gives the chunk
    /// back to its source.
    fn map(&self, chunk: LentChunk, access: DmaAccess) -> Result<DeviceAddress, DmaError>;

    /// The device stops reaching `chunk`, which `device` addressed.
    ///
    /// Called after the chunk's map entry left the chunk map and before
    /// the chunk goes back to the source. A refusal leaves the chunk with
    /// the arena for good, because the device may still reach it.
    fn unmap(&self, chunk: LentChunk, device: DeviceAddress) -> Result<(), DmaError>;
}

/// Which allocations an arena answers by lending a chunk.
pub trait ChunkGrowth: Send + Sync + 'static {
    /// Whether an allocation of `class` that finds no room lends a chunk.
    fn grows(&self, class: ChunkClass) -> bool;

    /// The source refused a lend that an allocation asked for: wake
    /// whoever retries growth.
    ///
    /// Called by the allocating task after it set the arena's `refused`
    /// bit. Wait-free and interrupt-safe.
    fn refused(&self);
}

/// What wakes the maintenance pass when an allocation was refused a
/// chunk. One per [`DmaArenas`], shared by its arenas.
pub struct GrowthDemand {
    /// Raised by an allocator after it set its arena's `refused` bit.
    kick: ProgressSignal,
}

impl GrowthDemand {
    pub const fn new() -> Self {
        Self {
            kick: ProgressSignal::new(),
        }
    }
}

impl Default for GrowthDemand {
    fn default() -> Self {
        Self::new()
    }
}

/// Grows every class. The maintenance pass retires its empty chunks and
/// retries the growth a refusal left undone.
///
/// For the network, host-share and control devices.
#[derive(Clone)]
pub struct Elastic(Arc<GrowthDemand>);

impl Elastic {
    pub fn new(demand: Arc<GrowthDemand>) -> Self {
        Self(demand)
    }
}

impl ChunkGrowth for Elastic {
    fn grows(&self, _class: ChunkClass) -> bool {
        true
    }

    fn refused(&self) {
        self.0.kick.signal();
    }
}

/// Grows [`ChunkClass::Resident`] only. A request class is filled by
/// [`DmaArena::reserve`] alone and its chunks never retire.
///
/// For disks, whose request memory is derived from the device. A refused
/// resident growth happens at bring-up and is fatal to the driver, and
/// nobody retries a fixed arena's growth, so there is nothing to wake.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fixed;

impl ChunkGrowth for Fixed {
    fn grows(&self, class: ChunkClass) -> bool {
        class == ChunkClass::Resident
    }

    fn refused(&self) {}
}

/// What a chunk's pages may be used for, and so which allocations are
/// served from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkClass {
    /// Memory for the life of the device.
    Resident,
    /// Memory the device only reads.
    Read,
    /// Memory the device reads and writes.
    ReadWrite,
}

impl ChunkClass {
    const COUNT: usize = 3;

    const ALL: [Self; Self::COUNT] = [Self::Resident, Self::Read, Self::ReadWrite];

    const fn index(self) -> usize {
        match self {
            Self::Resident => 0,
            Self::Read => 1,
            Self::ReadWrite => 2,
        }
    }

    /// The class's place in a processor's fragment rows, or `None` for the
    /// class that serves whole pages only.
    const fn fragment_index(self) -> Option<usize> {
        match self {
            Self::Resident => None,
            Self::Read => Some(0),
            Self::ReadWrite => Some(1),
        }
    }

    /// The class in its place in a chunk's state word.
    const fn state_bits(self) -> u64 {
        (self.index() as u64) << CLASS_SHIFT
    }

    /// The class a state word holds.
    const fn of_state(state: u64) -> Self {
        match (state & CLASS_MASK) >> CLASS_SHIFT {
            0 => Self::Resident,
            1 => Self::Read,
            _ => Self::ReadWrite,
        }
    }

    /// What the device may do with a chunk of this class. A read-only
    /// resident class does not exist yet.
    const fn access(self) -> DmaAccess {
        match self {
            Self::Read => DmaAccess::READ,
            Self::Resident | Self::ReadWrite => DmaAccess::READ.union(DmaAccess::WRITE),
        }
    }
}

/// A snapshot of an arena's occupancy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArenaStats {
    /// Live chunks the arena holds: published and not retired.
    pub chunks: usize,
    /// The live chunks of each class, indexed Resident, Read, ReadWrite.
    pub chunks_by_class: [usize; ChunkClass::COUNT],
    /// Pages in all of them.
    pub pages: usize,
    /// Pages handed out and not yet freed. A run a processor caches for
    /// fragments counts whole until its row lets go of it.
    pub pages_in_use: usize,
    /// Chunks with no page in use.
    pub empty_chunks: usize,
    /// Chunks recorded in the empty set. Every chunk with no page in use
    /// is in it; one that has been re-entered since may be too.
    pub noted_empty_chunks: usize,
    /// Chunks published since the arena was built.
    pub grown: u64,
    /// Chunks given back to the source since the arena was built.
    pub returned: u64,
}

/// What an allocator refused for space waits on, and what a free checks.
///
/// Lives on a cache line of its own, in a box the arena owns. A refused
/// allocator writes `starved` (through [`DmaArena::want_space`]); a free
/// reads it, and the one free that wins the `swap` that clears it writes
/// `starved` and the signal's generation. The arena that owns it outlives
/// every buffer, so a chunk header points at it for good.
struct SpaceWaiters {
    /// An allocator was refused and nothing has come back since.
    starved: AtomicBool,
    /// Raised past a waiter's mark by the free that clears `starved`.
    space: ProgressSignal,
}

impl SpaceWaiters {
    /// Raises the space signal when an allocator is starved.
    ///
    /// Called after the caller's update that an allocator may be waiting
    /// for: a free's bitmap and count, or the release of `grow`. The fence
    /// orders that update before the load of `starved`, and pairs with the
    /// fence in [`DmaArena::want_space`]: either this load sees `starved`,
    /// or the waiter's second attempt sees the update. The `swap` makes
    /// one caller per starvation episode the signaller, and happens before
    /// the generation moves, so a waiter that still sees the stale flag
    /// parks on a mark the increment then passes.
    ///
    /// Wait-free without a starved allocator, and otherwise one
    /// interrupt-safe broadcast; safe in an interrupt handler on any
    /// processor.
    fn wake_if_starved(&self) {
        fence(Ordering::SeqCst);
        if self.starved.load(Ordering::Relaxed) && self.starved.swap(false, Ordering::AcqRel) {
            self.space.signal();
        }
    }
}

/// One chunk's out-of-band record, type-stable for the life of the arena.
///
/// The state word holds the pages in use in its low 32 bits, the
/// `RETIRING` flag above them, and the chunk's [`ChunkClass`] in bits 34
/// and 35. A header is reused by a later chunk, so a reader that expects a
/// class compares it in the same CAS that checks the flag and the count.
struct Chunk {
    state: CachePadded<AtomicU64>,
    /// One bit per page. The bits past the chunk's last page are set for
    /// good, so a scan never hands them out.
    bitmap: Box<[AtomicU64]>,
    pages: u32,
    /// Kernel address of the first byte. Written under `grow` before the
    /// chunk is published.
    base: AtomicUsize,
    /// The device's address of the first byte. Written like `base`.
    device: AtomicU64,
    /// The word of the arena's empty set holding this chunk's bit.
    empty_word: NonNull<AtomicU64>,
    empty_bit: u64,
    /// The arena's space waiters, which a free checks after it gives its
    /// pages back.
    waiters: NonNull<SpaceWaiters>,
    /// Pages in one fragment run of this chunk: [`FRAGMENT_RUN_PAGES`], or
    /// the whole chunk when it is smaller.
    run_pages: u32,
    /// One count for each place a fragment run can start, indexed by the
    /// run's first page divided by `run_pages`. Out of band, like the
    /// bitmap. A count is meaningful while its run is claimed in the
    /// bitmap and is zero otherwise.
    ///
    /// The count of a run is the units its row still holds plus the
    /// fragments not yet dropped. Starting the run stores [`BIAS`], all of it
    /// the row's. A carve moves one unit from the row to the fragment and
    /// does not touch the count. A fragment's drop subtracts 1
    /// (`fetch_sub`, AcqRel). When the row lets go of the run, which it
    /// does by one CAS of the row, whoever won that CAS subtracts the units
    /// it saw the row still hold (`fetch_sub`, AcqRel): the detach, or the
    /// owner when it replaces a run that cannot take the next fragment.
    /// Whichever subtraction finds the count equal to what it subtracts is
    /// the one that reached zero, and frees the run's pages as a page free
    /// does.
    ///
    /// Why exactly one subtraction frees the run, and none too early. Let
    /// `H` be the units the row holds (zero once it let go) and `F` the
    /// fragments outstanding. The count is `H + F` at every instant except
    /// between a row's CAS and its subtraction, when it is larger by the
    /// units about to go, never smaller: the CAS comes first, so no instant
    /// undercounts. A carve needs the row to hold at least two units and
    /// leaves one, so while the row holds the run `H >= 1` and the count is
    /// at least 1; a fragment's drop therefore cannot reach zero while the
    /// row holds the run, and nobody frees it early. The row lets go once
    /// for each time the run was installed in it, because the CAS that
    /// replaces its word is the only way a word leaves and the word names
    /// exactly the units that installation holds (a later installation of
    /// the same run that happens to make an equal word holds units of its
    /// own, which the equal word names as well), so the units are
    /// subtracted once. After that `H` is zero and the count is `F`; no
    /// fragment is carved from a run the row no longer holds, so only
    /// subtractions of 1 lower it, each atomic, and it passes through zero
    /// once. The single subtraction that finds it so (the previous value
    /// equals what it subtracts) is the freer: the last fragment's drop
    /// when the row let go first, the row's subtraction when every
    /// fragment was dropped first.
    ///
    /// Orderings: the subtractions are `AcqRel`, so the one that reaches
    /// zero sees every fragment's last use of its bytes before the pages
    /// go back through the bitmap's Release. The starting store is
    /// `Relaxed`: the owner's Release CAS that installs the run in its row
    /// publishes it to the detach's Acquire load, and the bitmap's Acquire
    /// claim orders it after the previous use's last subtraction.
    fragment_refs: Box<[AtomicU32]>,
}

impl Chunk {
    fn new(
        pages: usize,
        empty_word: NonNull<AtomicU64>,
        empty_bit: u64,
        waiters: NonNull<SpaceWaiters>,
    ) -> Self {
        let run_pages = FRAGMENT_RUN_PAGES.min(pages);
        let words = pages.div_ceil(WORD_PAGES);
        let padding = if pages.is_multiple_of(WORD_PAGES) {
            0
        } else {
            u64::MAX << (pages % WORD_PAGES)
        };
        let bitmap = (0..words)
            .map(|word| AtomicU64::new(if word + 1 == words { padding } else { 0 }))
            .collect();
        Self {
            state: CachePadded::new(AtomicU64::new(0)),
            bitmap,
            pages: pages as u32,
            base: AtomicUsize::new(0),
            device: AtomicU64::new(0),
            empty_word,
            empty_bit,
            waiters,
            run_pages: run_pages as u32,
            fragment_refs: (0..pages / run_pages).map(|_| AtomicU32::new(0)).collect(),
        }
    }

    /// Gives the pages of a run back: clears the bits, leaves the state
    /// word, and wakes a starved allocator. What the drop of a run of pages
    /// does, and what the subtraction that frees a fragment run does.
    fn free_pages(&self, first_page: usize, pages: usize) {
        self.release(first_page, pages);
        self.leave(pages);
        self.wake_starved();
    }

    /// Subtracts `units` from the count of the run that starts at
    /// `first_page`, and frees the run when that took the count to zero.
    /// See `fragment_refs` for why exactly one subtraction does.
    fn put_run_refs(&self, first_page: usize, units: u32) {
        let count = &self.fragment_refs[first_page / self.run_pages as usize];
        let before = count.fetch_sub(units, Ordering::AcqRel);
        debug_assert!(
            before >= units,
            "a fragment run's count went below zero: {before} less {units}"
        );
        if before == units {
            self.free_pages(first_page, self.run_pages as usize);
        }
    }

    /// Reserves `pages` of the chunk's capacity, or refuses when the chunk
    /// is retiring, is not of the class `class_bits` holds, or cannot hold
    /// them.
    fn enter(&self, pages: usize, class_bits: u64) -> bool {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            if state & (RETIRING | CLASS_MASK) != class_bits
                || (state & COUNT_MASK) as usize + pages > self.pages as usize
            {
                return false;
            }
            match self.state.compare_exchange_weak(
                state,
                state + pages as u64,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => state = now,
            }
        }
    }

    /// Gives back `pages` of capacity [`Chunk::enter`] reserved, and notes
    /// the chunk in the empty set when it was the last of them.
    ///
    /// Wait-free and touches only this chunk's atomics and its word of the
    /// empty set, so it is safe in an interrupt handler on any processor.
    fn leave(&self, pages: usize) {
        let before = self.state.fetch_sub(pages as u64, Ordering::Release);
        debug_assert!(
            (before & COUNT_MASK) as usize >= pages,
            "a chunk's page count went below zero"
        );
        if (before & COUNT_MASK) as usize == pages {
            self.note_empty();
        }
    }

    /// Raises the arena's space signal when an allocator is starved.
    ///
    /// Called after the free's bitmap and count updates; see
    /// [`SpaceWaiters::wake_if_starved`] for the protocol.
    fn wake_starved(&self) {
        // SAFETY: the arena that owns the waiters outlives every live
        // page: it panics, leaking them, otherwise.
        unsafe { self.waiters.as_ref() }.wake_if_starved();
    }

    fn note_empty(&self) {
        // SAFETY: the word is in the empty set of the segment holding this
        // header, which is freed only when the arena drops without a live
        // page, and leaked when it drops with one.
        let word = unsafe { self.empty_word.as_ref() };
        word.fetch_or(self.empty_bit, Ordering::Release);
    }

    fn pages_in_use(&self) -> usize {
        (self.state.load(Ordering::Acquire) & COUNT_MASK) as usize
    }

    /// Claims a run of `pages` (at most one word's worth) that stays inside
    /// one bitmap word and starts at a multiple of `align_pages`, and
    /// returns its first page.
    fn claim_within_word(&self, pages: usize, align_pages: usize) -> Option<usize> {
        let starts = start_mask(align_pages);
        let word_step = (align_pages / WORD_PAGES).max(1);
        for (index, word) in self.bitmap.iter().enumerate().step_by(word_step) {
            let mut current = word.load(Ordering::Relaxed);
            loop {
                let fits = fit_mask(!current, pages) & starts;
                if fits == 0 {
                    break;
                }
                let at = fits.trailing_zeros() as usize;
                match word.compare_exchange_weak(
                    current,
                    current | (run_mask(pages) << at),
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return Some(index * WORD_PAGES + at),
                    Err(now) => current = now,
                }
            }
        }
        None
    }

    /// Claims a run of more than one word's worth of pages. It starts at a
    /// word boundary that is a multiple of `align_pages`, so every word but
    /// the last is wholly the run's.
    fn claim_across_words(&self, pages: usize, align_pages: usize) -> Option<usize> {
        let words = pages.div_ceil(WORD_PAGES);
        let tail = pages - (words - 1) * WORD_PAGES;
        let word_step = (align_pages / WORD_PAGES).max(1);
        (0..=self.bitmap.len().checked_sub(words)?)
            .step_by(word_step)
            .find(|&start| self.claim_words(start, words, tail))
            .map(|start| start * WORD_PAGES)
    }

    /// Claims `words` words from `start`: whole ones, then the low `tail`
    /// bits of the last. On a bit already taken it gives back what it
    /// claimed and reports failure.
    fn claim_words(&self, start: usize, words: usize, tail: usize) -> bool {
        for claimed in 0..words {
            let mask = if claimed + 1 == words {
                run_mask(tail)
            } else {
                u64::MAX
            };
            let word = &self.bitmap[start + claimed];
            let mut current = word.load(Ordering::Relaxed);
            loop {
                if current & mask != 0 {
                    // The words claimed so far were wholly free and are
                    // wholly ours, so giving them back clears them.
                    for undone in &self.bitmap[start..start + claimed] {
                        undone.store(0, Ordering::Release);
                    }
                    return false;
                }
                match word.compare_exchange_weak(
                    current,
                    current | mask,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(now) => current = now,
                }
            }
        }
        true
    }

    /// Clears the bits of a run.
    fn release(&self, first_page: usize, pages: usize) {
        let mut page = first_page;
        let mut left = pages;
        while left > 0 {
            let bit = page % WORD_PAGES;
            let take = left.min(WORD_PAGES - bit);
            self.bitmap[page / WORD_PAGES].fetch_and(!(run_mask(take) << bit), Ordering::Release);
            page += take;
            left -= take;
        }
    }
}

/// The low `pages` bits set. `pages` is at most one word.
const fn run_mask(pages: usize) -> u64 {
    if pages == WORD_PAGES {
        u64::MAX
    } else {
        (1 << pages) - 1
    }
}

/// The bits of `free` at which `pages` consecutive set bits begin.
///
/// Each round doubles the span a set bit vouches for: after covering
/// `covered` pages, bit `p` is set when `p..p + covered` are all free.
fn fit_mask(free: u64, pages: usize) -> u64 {
    let mut fits = free;
    let mut covered = 1;
    while covered < pages {
        let step = covered.min(pages - covered);
        fits &= fits >> step;
        covered += step;
    }
    fits
}

/// The bit positions a run aligned to `align_pages` may start at inside
/// one word.
const fn start_mask(align_pages: usize) -> u64 {
    if align_pages >= WORD_PAGES {
        return 1;
    }
    let mut mask = 0;
    let mut bit = 0;
    while bit < WORD_PAGES {
        mask |= 1 << bit;
        bit += align_pages;
    }
    mask
}

/// What a fragment row holds while it caches a run, decoded.
///
/// The row is one `AtomicU64`, from the low bits up: `held` in
/// [`HELD_BITS`], `offset` in [`OFFSET_BITS`], `run` in [`RUN_BITS`], and
/// `slot` in the rest ([`SLOT_BITS`]). Each width is the smallest that names
/// every value its field takes: [`BIAS`], the run's byte length
/// ([`FRAGMENT_RUN_BYTES`]), the runs of a chunk ([`MAX_RUNS_PER_CHUNK`],
/// what is left of the word), and the chunk slots ([`MAX_CHUNKS`]). A row
/// that caches a run always holds a unit, so a live row is never the empty
/// word, which means no run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RunRow {
    /// The slot of the run's chunk.
    slot: usize,
    /// The run's index in its chunk: its first page over the run's pages.
    run: usize,
    /// Bytes of the run the carves so far span, alignment padding
    /// included: where the next fragment may start.
    offset: usize,
    /// Units of the run's count the row still holds.
    held: u32,
}

impl RunRow {
    /// The word of a row that caches no run.
    const EMPTY: u64 = 0;

    const fn mask(bits: u32) -> u64 {
        (1 << bits) - 1
    }

    fn pack(self) -> u64 {
        debug_assert!(self.slot < MAX_CHUNKS && self.run < MAX_RUNS_PER_CHUNK);
        debug_assert!(self.offset <= FRAGMENT_RUN_BYTES && (1..=BIAS).contains(&self.held));
        (self.slot as u64) << SLOT_SHIFT
            | (self.run as u64) << RUN_SHIFT
            | (self.offset as u64) << OFFSET_SHIFT
            | u64::from(self.held)
    }

    /// The run a word caches, or `None` for the empty word.
    fn unpack(word: u64) -> Option<Self> {
        (word != Self::EMPTY).then(|| Self {
            slot: (word >> SLOT_SHIFT) as usize,
            run: ((word >> RUN_SHIFT) & Self::mask(RUN_BITS)) as usize,
            offset: ((word >> OFFSET_SHIFT) & Self::mask(OFFSET_BITS)) as usize,
            held: (word & Self::mask(HELD_BITS)) as u32,
        })
    }
}

/// One processor's cache of the run it carves fragments from, for one
/// request class: a [`RunRow`] word, empty when it caches nothing.
///
/// Written by CAS only: by the tasks running on the processor, and by the
/// maintenance task's detach.
struct FragmentCache {
    row: AtomicU64,
}

impl FragmentCache {
    const fn new() -> Self {
        Self {
            row: AtomicU64::new(RunRow::EMPTY),
        }
    }
}

/// A segment of the chunk table: headers, and the bits that note which of
/// them have been empty.
struct Segment {
    chunks: Box<[Chunk]>,
    empty: Box<[AtomicU64]>,
}

impl Segment {
    fn new(headers: usize, pages_per_chunk: usize, waiters: NonNull<SpaceWaiters>) -> Self {
        let empty: Box<[AtomicU64]> = (0..headers.div_ceil(WORD_PAGES))
            .map(|_| AtomicU64::new(0))
            .collect();
        let chunks = (0..headers)
            .map(|header| {
                Chunk::new(
                    pages_per_chunk,
                    NonNull::from(&empty[header / WORD_PAGES]),
                    1 << (header % WORD_PAGES),
                    waiters,
                )
            })
            .collect();
        Self { chunks, empty }
    }
}

/// The segment and the position in it of chunk slot `slot`.
fn locate(slot: usize) -> (usize, usize) {
    let segment = (slot / FIRST_SEGMENT_CHUNKS + 1).ilog2() as usize;
    (segment, slot - FIRST_SEGMENT_CHUNKS * ((1 << segment) - 1))
}

/// What a task holds while it is the arena's only writer of the chunk map,
/// the chunk headers and the retired-slot list. Dropping it releases the
/// lock and then raises the space signal when an allocator is starved:
/// whoever was refused with [`DmaError::Replenishing`] while it was held
/// asks again, whatever the holder did.
struct GrowGuard<'a> {
    state: ManuallyDrop<MutexGuard<'a, GrowState>>,
    waiters: &'a SpaceWaiters,
}

/// What `grow` guards besides the right to write.
struct GrowState {
    /// Slots of retired chunks, whose headers the next publication reuses.
    retired_slots: Vec<u32>,
}

impl Deref for GrowGuard<'_> {
    type Target = GrowState;

    fn deref(&self) -> &GrowState {
        &self.state
    }
}

impl DerefMut for GrowGuard<'_> {
    fn deref_mut(&mut self) -> &mut GrowState {
        &mut self.state
    }
}

impl GrowGuard<'_> {
    /// Releases the lock without the wake, for an allocation whose own
    /// growth the source refused.
    ///
    /// The wake would only reach the allocator itself: it set `starved`
    /// (through [`DmaArena::want_space`]) before this attempt, and its
    /// release would consume the flag and raise the signal past the mark
    /// the allocator is about to park on, so it would never park. Whoever
    /// was refused with [`DmaError::Replenishing`] meanwhile is woken by
    /// the release that follows the retry the `refused` bit and the kick
    /// start, or by a free.
    fn release_silently(self) {
        let mut guard = ManuallyDrop::new(self);
        // SAFETY: the guard is never used or dropped again, so the lock
        // guard is dropped exactly once, here.
        unsafe { ManuallyDrop::drop(&mut guard.state) };
    }
}

impl Drop for GrowGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: the lock guard is dropped exactly once, here, and not
        // used again. It goes first: a waiter this wake releases asks for
        // the lock at once, and would find it held and park again.
        unsafe { ManuallyDrop::drop(&mut self.state) };
        self.waiters.wake_if_starved();
    }
}

/// One leaf of the chunk map: the entries of [`LEAF_CHUNKS`] consecutive
/// chunk numbers.
struct Leaf {
    /// The chunk's device address with bit 0 set, or `0` for no chunk.
    /// A chunk's device address is page aligned, so bit 0 is free.
    entries: [AtomicU64; LEAF_CHUNKS],
}

/// The kernel address of a chunk to the device's address of it, with
/// lock-free readers and a single writer.
///
/// A two-level direct map over the chunk number relative to the source's
/// span. The root is a fixed array sized from the span at construction. A
/// leaf is allocated under `grow` and published (Release) into the root,
/// and is freed only when the arena drops, so a reader never meets a freed
/// leaf. Removing a chunk is one store, and the memory is bounded by the
/// span, not by how often chunks come and go.
struct ChunkMap {
    chunk_bytes: usize,
    /// The chunk number of the span's first byte.
    first: usize,
    root: Box<[AtomicPtr<Leaf>]>,
}

impl ChunkMap {
    fn new(span: Range<usize>, chunk_bytes: usize) -> Self {
        assert!(
            span.start < span.end,
            "a chunk source's span {span:x?} is empty"
        );
        let first = span.start / chunk_bytes;
        let last = (span.end - 1) / chunk_bytes;
        Self {
            chunk_bytes,
            first,
            root: (0..(last - first) / LEAF_CHUNKS + 1)
                .map(|_| AtomicPtr::new(ptr::null_mut()))
                .collect(),
        }
    }

    /// The leaf and the entry of the chunk `address` lies in, or `None`
    /// for an address outside the span.
    fn position(&self, address: usize) -> Option<(usize, usize)> {
        let number = (address / self.chunk_bytes).checked_sub(self.first)?;
        let leaf = number / LEAF_CHUNKS;
        (leaf < self.root.len()).then_some((leaf, number % LEAF_CHUNKS))
    }

    /// The device's address of the first byte of the chunk `address` lies
    /// in.
    fn lookup(&self, address: usize) -> Option<u64> {
        let (leaf, entry) = self.position(address)?;
        let leaf = self.root[leaf].load(Ordering::Acquire);
        if leaf.is_null() {
            return None;
        }
        // SAFETY: a published leaf is live until the arena drops.
        let entry = unsafe { &*leaf }.entries[entry].load(Ordering::Acquire);
        (entry != 0).then_some(entry & !1)
    }

    /// Records the chunk at `address`, allocating its leaf first when it
    /// is the first chunk of its leaf.
    fn insert(&self, _grow: &GrowGuard<'_>, address: usize, device: u64) {
        let (leaf, entry) = self
            .position(address)
            .unwrap_or_else(|| panic!("a chunk at {address:#x} lies outside its source's span"));
        debug_assert!(
            device & 1 == 0,
            "a chunk's device address {device:#x} is not page aligned"
        );
        let mut at = self.root[leaf].load(Ordering::Relaxed);
        if at.is_null() {
            at = Box::into_raw(Box::new(Leaf {
                entries: core::array::from_fn(|_| AtomicU64::new(0)),
            }));
            self.root[leaf].store(at, Ordering::Release);
        }
        // SAFETY: the leaf is live until the arena drops.
        let slot = &unsafe { &*at }.entries[entry];
        debug_assert!(
            slot.load(Ordering::Relaxed) == 0,
            "the chunk at {address:#x} is already in the chunk map"
        );
        slot.store(device | 1, Ordering::Release);
    }

    /// Forgets the chunk at `address`.
    fn remove(&self, _grow: &GrowGuard<'_>, address: usize) {
        let (leaf, entry) = self
            .position(address)
            .unwrap_or_else(|| panic!("a chunk at {address:#x} lies outside its source's span"));
        let leaf = self.root[leaf].load(Ordering::Relaxed);
        assert!(
            !leaf.is_null(),
            "the chunk at {address:#x} was never in the chunk map"
        );
        // SAFETY: the leaf is live until the arena drops.
        unsafe { &*leaf }.entries[entry].store(0, Ordering::Release);
    }
}

impl Drop for ChunkMap {
    fn drop(&mut self) {
        for leaf in &mut self.root {
            let leaf = *leaf.get_mut();
            if !leaf.is_null() {
                // SAFETY: the pointer came from `Box::into_raw` in
                // `insert`, and `&mut self` excludes every reader.
                drop(unsafe { Box::from_raw(leaf) });
            }
        }
    }
}

/// What the pages of a run hold when an allocation returns it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fill {
    /// Zeroes.
    Zeroed,
    /// Whatever the pages held: the caller writes every byte first.
    Unspecified,
}

/// The pages a buffer owns, or shares as a fragment.
#[derive(Clone, Copy)]
struct Run {
    first_page: usize,
    pages: usize,
    /// Whether the buffer is one fragment of the run, which other
    /// fragments share, and not the run itself.
    fragment: bool,
}

/// What `grow` writes besides the chunk map and the headers: the chunk
/// slots published and the counters. One line, written only under `grow`.
struct GrowCounters {
    /// Chunk slots published: the headers a scan walks. A retired chunk's
    /// slot stays counted until a later publication reuses it.
    slots: AtomicUsize,
    /// Chunks published, reused slots included.
    grown: AtomicU64,
    /// Chunks given back to the source.
    returned: AtomicU64,
}

/// Why one growth did not publish a chunk.
enum GrowError {
    /// The source refused the lend.
    Refused,
    /// The domain refused the map; the chunk went back to the source.
    Domain(DmaError),
}

/// The memory one device reaches.
///
/// `D` maps and unmaps its chunks, and `G` says which allocations lend a
/// chunk for it.
pub struct DmaArena<S: ChunkSource, D: ChunkDomain, G: ChunkGrowth> {
    chunk_bytes: usize,
    pages_per_chunk: usize,
    /// The chunk table. A segment is allocated and published (Release)
    /// under `grow` and freed only when the arena drops.
    segments: [AtomicPtr<Segment>; SEGMENT_COUNT],
    counters: CachePadded<GrowCounters>,
    /// One bit per class whose lend the source refused to an allocation
    /// and that nobody has retried since. Set by an allocator (Release),
    /// swapped to zero by [`DmaArenas::grow_refused`]. On a line of its
    /// own, because allocators on every processor write it while the
    /// source is dry.
    refused: CachePadded<AtomicU8>,
    map: ChunkMap,
    /// Per processor and class, the slot of the chunk the last allocation
    /// there came from.
    hints: Box<[CachePadded<[AtomicU32; ChunkClass::COUNT]>]>,
    /// Per processor and request class, the run its fragments are carved
    /// from, indexed by [`ChunkClass::fragment_index`].
    frags: Box<[CachePadded<[FragmentCache; FRAGMENT_CLASSES]>]>,
    /// Pages in a fragment run: [`FRAGMENT_RUN_PAGES`], or the whole chunk
    /// when it is smaller.
    run_pages: usize,
    /// Serialises the allocations that span bitmap words.
    runs: spin::Mutex<()>,
    /// Serialises the writers of the chunk table and the chunk map.
    grow: Mutex<GrowState>,
    /// Boxed so its address is stable for the chunk headers that point at
    /// it. Not dropped when the arena drops with a live page, so the
    /// buffers that outlive the panic still reach it.
    waiters: ManuallyDrop<Box<CachePadded<SpaceWaiters>>>,
    source: S,
    domain: D,
    growth: G,
}

impl<S: ChunkSource, D: ChunkDomain, G: ChunkGrowth> DmaArena<S, D, G> {
    /// An arena with no chunks, for `processors` processors, whose chunks
    /// are `chunk_bytes` each. [`DmaArena::reserve`] fills it, and
    /// `growth` decides which allocations fill it further.
    ///
    /// `chunk_bytes` is a power of two and at least a page, and at least
    /// as large as the largest run the arena will be asked for.
    pub fn new(source: S, domain: D, growth: G, chunk_bytes: usize, processors: usize) -> Self {
        assert!(
            chunk_bytes.is_power_of_two() && chunk_bytes >= PAGE_BYTES,
            "a DMA chunk is a power of two of at least a page, not {chunk_bytes} bytes"
        );
        assert!(
            chunk_bytes / PAGE_BYTES <= u32::MAX as usize,
            "a DMA chunk of {chunk_bytes} bytes has more pages than its state word counts"
        );
        let run_pages = FRAGMENT_RUN_PAGES.min(chunk_bytes / PAGE_BYTES);
        assert!(
            chunk_bytes / PAGE_BYTES / run_pages <= MAX_RUNS_PER_CHUNK,
            "a DMA chunk of {chunk_bytes} bytes has more fragment runs than a row names"
        );
        Self {
            chunk_bytes,
            pages_per_chunk: chunk_bytes / PAGE_BYTES,
            segments: core::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            counters: CachePadded::new(GrowCounters {
                slots: AtomicUsize::new(0),
                grown: AtomicU64::new(0),
                returned: AtomicU64::new(0),
            }),
            refused: CachePadded::new(AtomicU8::new(0)),
            map: ChunkMap::new(source.span(), chunk_bytes),
            hints: (0..processors)
                .map(|_| CachePadded::new(core::array::from_fn(|_| AtomicU32::new(0))))
                .collect(),
            frags: (0..processors)
                .map(|_| CachePadded::new(core::array::from_fn(|_| FragmentCache::new())))
                .collect(),
            run_pages,
            runs: spin::Mutex::new(()),
            grow: Mutex::new(GrowState {
                retired_slots: Vec::new(),
            }),
            waiters: ManuallyDrop::new(Box::new(CachePadded::new(SpaceWaiters {
                starved: AtomicBool::new(false),
                space: ProgressSignal::new(),
            }))),
            source,
            domain,
            growth,
        }
    }

    /// The `grow` guard, when nobody holds it.
    fn try_grow(&self) -> Option<GrowGuard<'_>> {
        self.grow.try_lock().map(|state| GrowGuard {
            state: ManuallyDrop::new(state),
            waiters: &self.waiters,
        })
    }

    /// The `grow` guard, waiting for its holder.
    async fn lock_grow(&self) -> GrowGuard<'_> {
        GrowGuard {
            state: ManuallyDrop::new(self.grow.lock().await),
            waiters: &self.waiters,
        }
    }

    /// Declares that the caller was refused and will wait for space, and
    /// returns the space signal's generation as of that declaration.
    ///
    /// Marks the arena starved, so the next free raises the signal, and
    /// fences. The caller then allocates once more, and only if that is
    /// refused too parks on [`DmaArena::space_changed`] with the mark.
    ///
    /// Wait-free, task context, any processor.
    pub fn want_space(&self) -> DmaSpaceMark {
        let mark = self.space_mark();
        self.waiters.starved.store(true, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        mark
    }

    /// Resolves once a page was freed after `mark`, or a signal was
    /// otherwise raised after it.
    ///
    /// Resolving says an allocation is worth asking for again, not that it
    /// will succeed.
    pub fn space_changed(&self, mark: DmaSpaceMark) -> impl Future<Output = ()> + Send + '_ {
        self.waiters
            .space
            .changed(ProgressMark::from_generation(mark.get()))
    }

    /// Whether an allocation was refused and no page has come back since.
    ///
    /// Only a hint: the next allocation is the authority.
    pub fn short_of_space(&self) -> bool {
        self.waiters.starved.load(Ordering::Acquire)
    }

    /// Samples the space signal's generation without marking the arena
    /// starved, for a waiter that asks for no memory.
    pub fn space_mark(&self) -> DmaSpaceMark {
        DmaSpaceMark::new(self.waiters.space.mark().generation())
    }

    /// Bytes in each chunk.
    pub fn chunk_bytes(&self) -> usize {
        self.chunk_bytes
    }

    /// Lends, maps and publishes `chunks` chunks of `class`, so a
    /// request class has the memory its device's requests need before the
    /// first one is made.
    ///
    /// A refusal leaves the chunks already published in place. Task
    /// context; waits for the `grow` lock.
    pub async fn reserve(&self, class: ChunkClass, chunks: usize) -> Result<(), DmaError> {
        let mut grow = self.lock_grow().await;
        for _ in 0..chunks {
            self.grow_one(&mut grow, class)
                .map_err(|error| match error {
                    GrowError::Refused => DmaError::Exhausted {
                        bytes: self.chunk_bytes,
                    },
                    GrowError::Domain(error) => error,
                })?;
        }
        Ok(())
    }

    /// Lends one chunk, maps it for `class`, and publishes it. Returns the
    /// slot it was published in.
    ///
    /// The guard makes the caller the only writer. The chunk is allocatable
    /// only once published, so a buffer from it always resolves in
    /// [`DmaArena::device_range`].
    fn grow_one(&self, grow: &mut GrowGuard<'_>, class: ChunkClass) -> Result<usize, GrowError> {
        let chunk = self
            .source
            .lend(self.chunk_bytes)
            .ok_or(GrowError::Refused)?;
        assert_eq!(
            chunk.bytes, self.chunk_bytes,
            "a source lent {} bytes to an arena of {}-byte chunks",
            chunk.bytes, self.chunk_bytes
        );
        assert!(
            chunk.start != 0 && chunk.start.is_multiple_of(self.chunk_bytes),
            "a chunk at {:#x} is not aligned to its {} bytes",
            chunk.start,
            self.chunk_bytes
        );
        let device = match self.domain.map(chunk, class.access()) {
            Ok(device) => device,
            Err(error) => {
                self.source.take_back(chunk);
                return Err(GrowError::Domain(error));
            }
        };
        Ok(self.publish(grow, class, chunk, device))
    }

    /// Makes a mapped chunk allocatable, in this order: the header's
    /// address fields, the chunk map entry, the state word with the class
    /// and a count of zero, the empty-set bit, and for a new slot the
    /// slot count. Each of the Release stores is what the reader that
    /// follows it Acquires, so whoever finds the chunk finds it whole.
    fn publish(
        &self,
        grow: &mut GrowGuard<'_>,
        class: ChunkClass,
        chunk: LentChunk,
        device: DeviceAddress,
    ) -> usize {
        let (slot, new_slot) = match grow.retired_slots.pop() {
            Some(slot) => (slot as usize, false),
            None => (self.counters.slots.load(Ordering::Relaxed), true),
        };
        assert!(
            slot < MAX_CHUNKS,
            "a DMA arena holds at most {MAX_CHUNKS} chunks"
        );
        let (index, position) = locate(slot);
        let mut segment = self.segments[index].load(Ordering::Relaxed);
        if segment.is_null() {
            segment = Box::into_raw(Box::new(Segment::new(
                FIRST_SEGMENT_CHUNKS << index,
                self.pages_per_chunk,
                NonNull::from(&***self.waiters),
            )));
            self.segments[index].store(segment, Ordering::Release);
        }
        // SAFETY: the segment is live until the arena drops.
        let header = &unsafe { &*segment }.chunks[position];
        header.base.store(chunk.start, Ordering::Relaxed);
        header.device.store(device.get(), Ordering::Relaxed);
        self.map.insert(grow, chunk.start, device.get());
        header.state.store(class.state_bits(), Ordering::Release);
        header.note_empty();
        if new_slot {
            self.counters.slots.store(slot + 1, Ordering::Release);
        }
        self.counters.grown.fetch_add(1, Ordering::Relaxed);
        slot
    }

    /// Zeroed pages for `layout`, from a chunk of `class`.
    ///
    /// The run is contiguous in the device's address space, inside one
    /// chunk, rounded up to whole pages, and aligned as `layout` asks.
    ///
    /// Task context only.
    pub fn allocate(&self, class: ChunkClass, layout: Layout) -> Result<ArenaBuffer, DmaError> {
        self.take(class, layout, Fill::Zeroed)
    }

    /// [`DmaArena::allocate`] without the pass that zeroes the pages: the
    /// run holds whatever its pages held, which may be bytes a device
    /// wrote after the run before went back.
    ///
    /// The caller writes every byte of the run before anything reads it:
    /// a transfer run is filled whole before a device reads it, or is
    /// accepted only after a device wrote it whole. The out-of-band
    /// metadata never lives in the pages, so stale bytes in them reach
    /// nothing the allocator reads.
    ///
    /// Concurrency contract as [`DmaArena::allocate`]: task context, any
    /// processor, lock-free within one bitmap word.
    pub fn allocate_overwritten(
        &self,
        class: ChunkClass,
        layout: Layout,
    ) -> Result<ArenaBuffer, DmaError> {
        self.take(class, layout, Fill::Unspecified)
    }

    /// The longest run an allocation can be: one chunk, which a run never
    /// spans.
    pub fn max_run_bytes(&self) -> usize {
        self.chunk_bytes
    }

    fn take(&self, class: ChunkClass, layout: Layout, fill: Fill) -> Result<ArenaBuffer, DmaError> {
        let bytes = layout.size();
        if bytes == 0 {
            return Err(DmaError::InvalidLength {
                bytes,
                required_multiple: 1,
            });
        }
        let pages = bytes.div_ceil(PAGE_BYTES);
        if pages > self.pages_per_chunk || layout.align() > self.chunk_bytes {
            return Err(DmaError::TooLarge {
                bytes,
                limit: self.chunk_bytes,
            });
        }
        if bytes < PAGE_BYTES
            && layout.align() <= PAGE_BYTES
            && let Some(cache) = self.fragment_cache(class)
        {
            return self.take_fragment(cache, class, layout, fill);
        }
        let align_pages = layout.align().div_ceil(PAGE_BYTES);
        let (slot, first_page) = self.locate(class, pages, align_pages, bytes)?;
        let run = Run {
            first_page,
            pages,
            fragment: false,
        };
        Ok(self.buffer(self.chunk(slot), run, first_page * PAGE_BYTES, bytes, fill))
    }

    /// Claims a run of `pages` pages aligned to `align_pages` from a chunk
    /// of `class`, growing the arena when none has room and its policy
    /// grows the class. Returns the chunk's slot and the run's first page.
    /// `bytes` is what the caller asked for, which a refusal names.
    fn locate(
        &self,
        class: ChunkClass,
        pages: usize,
        align_pages: usize,
        bytes: usize,
    ) -> Result<(usize, usize), DmaError> {
        if let Some(found) = self.find(class, pages, align_pages) {
            return Ok(found);
        }
        if !self.growth.grows(class) {
            return Err(DmaError::Exhausted { bytes });
        }
        // One growth at a time. The holder wakes whoever was refused here
        // when it lets go, unless its own lend was refused: that release
        // is silent (`GrowGuard::release_silently`).
        let Some(mut grow) = self.try_grow() else {
            return Err(DmaError::Replenishing { bytes });
        };
        loop {
            // A growth or a free may have landed while the lock was out of
            // reach.
            if let Some(found) = self.find(class, pages, align_pages) {
                return Ok(found);
            }
            match self.grow_one(&mut grow, class) {
                Ok(slot) => {
                    let hint = self.hint(class);
                    hint.store(slot as u32, Ordering::Relaxed);
                }
                Err(GrowError::Refused) => {
                    // The bit first, then the kick: the maintenance pass
                    // samples the kick before it reads the bits.
                    self.refused.fetch_or(1 << class.index(), Ordering::Release);
                    self.growth.refused();
                    grow.release_silently();
                    return Err(DmaError::Exhausted { bytes });
                }
                Err(GrowError::Domain(error)) => return Err(error),
            }
        }
    }

    /// Claims a run of `pages` pages aligned to `align_pages` from a
    /// published chunk of `class`, starting at the executing processor's
    /// hint. Returns the chunk's slot and the run's first page.
    fn find(&self, class: ChunkClass, pages: usize, align_pages: usize) -> Option<(usize, usize)> {
        let hint = self.hint(class);
        let first = hint.load(Ordering::Relaxed) as usize;
        let slots = self.counters.slots.load(Ordering::Acquire);
        for step in 0..slots {
            let slot = (first + step) % slots;
            let Some(first_page) = self.claim(self.chunk(slot), class, pages, align_pages) else {
                continue;
            };
            if slot != first {
                hint.store(slot as u32, Ordering::Relaxed);
            }
            return Some((slot, first_page));
        }
        None
    }

    /// The executing processor's fragment row for `class`, or `None` for
    /// the class that serves whole pages only.
    fn fragment_cache(&self, class: ChunkClass) -> Option<&FragmentCache> {
        let index = class.fragment_index()?;
        let processor = current_processor().id() as usize;
        let row = self.frags.get(processor).unwrap_or_else(|| {
            panic!(
                "processor {processor} has no fragment row among the DMA arena's {}",
                self.frags.len()
            )
        });
        Some(&row[index])
    }

    /// A fragment of `layout` (under a page, aligned to at most one) from
    /// the run `cache` holds, or from a new run when it holds none or the
    /// fragment does not fit what is left of it.
    ///
    /// Task context, on the processor that owns `cache`: only its tasks
    /// install a run in it, so the one CAS it races is a detach's, which
    /// can only empty the row.
    fn take_fragment(
        &self,
        cache: &FragmentCache,
        class: ChunkClass,
        layout: Layout,
        fill: Fill,
    ) -> Result<ArenaBuffer, DmaError> {
        let bytes = layout.size();
        let mut row = cache.row.load(Ordering::Acquire);
        while let Some(held) = RunRow::unpack(row) {
            // The run starts on a page, which `layout` is aligned within.
            let at = held.offset.next_multiple_of(layout.align());
            if at + bytes > self.run_pages * PAGE_BYTES {
                break;
            }
            debug_assert!(held.held >= 2, "a row spent the last unit of its run");
            let carved = RunRow {
                offset: at + bytes,
                held: held.held - 1,
                ..held
            };
            // Spends a unit and moves the offset. A success hands the
            // fragment one unit of a count the row held a unit of, which
            // keeps the run alive. Relaxed: nothing is published, and the
            // word's release sequence is the installing CAS's.
            match cache.row.compare_exchange_weak(
                row,
                carved.pack(),
                Ordering::Relaxed,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    let first_page = held.run * self.run_pages;
                    return Ok(self.fragment(held.slot, first_page, at, bytes, fill));
                }
                Err(now) => row = now,
            }
        }

        // No run, or the run cannot take this fragment: start a new one.
        // `row` is what the install below expects to replace.
        let (slot, first_page) = match self.find(class, self.run_pages, self.run_pages) {
            Some(run) => run,
            None => {
                // No chunk has a whole run free. A free page is all this
                // allocation needs, so it is served as a page and the row
                // keeps what it has: a fragment never fails for want of a
                // run while a page is free. Only when no page is free
                // either is the arena asked to grow, and then for a run.
                if let Some((slot, first_page)) = self.find(class, 1, 1) {
                    let page = Run {
                        first_page,
                        pages: 1,
                        fragment: false,
                    };
                    let offset = first_page * PAGE_BYTES;
                    return Ok(self.buffer(self.chunk(slot), page, offset, bytes, fill));
                }
                self.locate(class, self.run_pages, self.run_pages, bytes)?
            }
        };
        let run = first_page / self.run_pages;
        self.chunk(slot).fragment_refs[run].store(BIAS, Ordering::Relaxed);
        let first = RunRow {
            slot,
            run,
            offset: bytes,
            held: BIAS - 1,
        };
        let mut expected = row;
        let replaced = loop {
            // Release: publishes the starting count to the detach.
            match cache.row.compare_exchange(
                expected,
                first.pack(),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break expected,
                // A detach emptied the row after `row` was read, and it
                // took the units of the old run with it.
                Err(now) => expected = now,
            }
        };
        // The old run's unspent units are given up now that the row has let
        // go of it. A detach that won the race already did, and the word it
        // left is the empty one.
        if let Some(old) = RunRow::unpack(replaced) {
            self.chunk(old.slot)
                .put_run_refs(old.run * self.run_pages, old.held);
        }
        Ok(self.fragment(slot, first_page, 0, bytes, fill))
    }

    /// The address a device has to issue to reach `bytes`.
    ///
    /// The whole slice has to lie inside one chunk of this arena, so a
    /// slice of the heap, of another arena's chunk, or one that runs past
    /// the end of a chunk is refused.
    pub fn device_range(&self, bytes: &[u8]) -> Result<DeviceAddress, DmaError> {
        let address = bytes.as_ptr() as usize;
        let unreachable = DmaError::Unreachable {
            address,
            bytes: bytes.len(),
        };
        let Some(device) = self.map.lookup(address) else {
            return Err(unreachable);
        };
        let offset = address % self.chunk_bytes;
        if offset + bytes.len() > self.chunk_bytes {
            return Err(unreachable);
        }
        Ok(DeviceAddress::new(device + offset as u64))
    }

    /// The arena's occupancy now.
    pub fn stats(&self) -> ArenaStats {
        let slots = self.counters.slots.load(Ordering::Acquire);
        let mut stats = ArenaStats {
            chunks: 0,
            chunks_by_class: [0; ChunkClass::COUNT],
            pages: 0,
            pages_in_use: 0,
            empty_chunks: 0,
            noted_empty_chunks: 0,
            grown: self.counters.grown.load(Ordering::Relaxed),
            returned: self.counters.returned.load(Ordering::Relaxed),
        };
        for slot in 0..slots {
            let state = self.chunk(slot).state.load(Ordering::Acquire);
            if state & RETIRING != 0 {
                continue;
            }
            let in_use = (state & COUNT_MASK) as usize;
            stats.chunks += 1;
            stats.chunks_by_class[ChunkClass::of_state(state).index()] += 1;
            stats.pages += self.pages_per_chunk;
            stats.pages_in_use += in_use;
            stats.empty_chunks += usize::from(in_use == 0);
        }
        for segment in &self.segments {
            let segment = segment.load(Ordering::Acquire);
            if segment.is_null() {
                continue;
            }
            // SAFETY: the segment is live until the arena drops.
            for word in unsafe { &*segment }.empty.iter() {
                stats.noted_empty_chunks += word.load(Ordering::Acquire).count_ones() as usize;
            }
        }
        stats
    }

    /// The executing processor's hint row.
    fn hint(&self, class: ChunkClass) -> &AtomicU32 {
        let processor = current_processor().id() as usize;
        let row = self.hints.get(processor).unwrap_or_else(|| {
            panic!(
                "processor {processor} has no hint row among the DMA arena's {}",
                self.hints.len()
            )
        });
        &row[class.index()]
    }

    fn chunk(&self, slot: usize) -> &Chunk {
        let (index, position) = locate(slot);
        // SAFETY: `slot` is below the published chunk count, whose Release
        // store follows the segment's, and the segment is live until the
        // arena drops.
        &unsafe { &*self.segments[index].load(Ordering::Acquire) }.chunks[position]
    }

    /// Empties every fragment row whose run has no outstanding fragment,
    /// so an idle processor's cached run does not pin its chunk.
    ///
    /// A run is idle when its count equals the units the row holds. The
    /// count cannot fall below them, and a carve changes the row, so the
    /// CAS that empties it fails if a fragment was carved since the load,
    /// and succeeds only on the word the check was made against; either
    /// way the subtraction that follows is correct for the word it won (see
    /// `Chunk::fragment_refs`). Idle runs only, so a busy processor keeps
    /// its run.
    ///
    /// Maintenance task context: `grow` is held, so the chunks of a row's
    /// run are not retired or reused while it looks at them. They cannot
    /// be anyway, a run in a row being in use.
    fn detach_idle_runs(&self, _grow: &GrowGuard<'_>) {
        for caches in self.frags.iter() {
            for cache in caches.iter() {
                let word = cache.row.load(Ordering::Acquire);
                let Some(held) = RunRow::unpack(word) else {
                    continue;
                };
                let chunk = self.chunk(held.slot);
                if chunk.fragment_refs[held.run].load(Ordering::Acquire) != held.held {
                    continue;
                }
                if cache
                    .row
                    .compare_exchange(word, RunRow::EMPTY, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    chunk.put_run_refs(held.run * self.run_pages, held.held);
                }
            }
        }
    }

    /// Claims `pages` in `chunk`: enters it, then takes the bits. A
    /// refusal leaves the chunk as it was.
    fn claim(
        &self,
        chunk: &Chunk,
        class: ChunkClass,
        pages: usize,
        align_pages: usize,
    ) -> Option<usize> {
        if !chunk.enter(pages, class.state_bits()) {
            return None;
        }
        let first_page = if pages <= WORD_PAGES {
            chunk.claim_within_word(pages, align_pages)
        } else {
            let _runs = self.runs.lock();
            chunk.claim_across_words(pages, align_pages)
        };
        if first_page.is_none() {
            chunk.leave(pages);
        }
        first_page
    }

    /// A fragment of `bytes` at byte `at` of the run that starts at
    /// `first_page` of the chunk in `slot`. Zeroes the `bytes` and no more.
    fn fragment(
        &self,
        slot: usize,
        first_page: usize,
        at: usize,
        bytes: usize,
        fill: Fill,
    ) -> ArenaBuffer {
        let run = Run {
            first_page,
            pages: self.run_pages,
            fragment: true,
        };
        self.buffer(
            self.chunk(slot),
            run,
            first_page * PAGE_BYTES + at,
            bytes,
            fill,
        )
    }

    /// The buffer for `bytes` at byte `offset` of `chunk`, in `run`. A run
    /// of pages is zeroed whole; a fragment, which shares its run, is
    /// zeroed for its `bytes`.
    fn buffer(
        &self,
        chunk: &Chunk,
        run: Run,
        offset: usize,
        bytes: usize,
        fill: Fill,
    ) -> ArenaBuffer {
        let Run {
            first_page,
            pages,
            fragment,
        } = run;
        let base = chunk.base.load(Ordering::Relaxed);
        let ptr = ptr::with_exposed_provenance_mut::<u8>(base + offset);
        if fill == Fill::Zeroed {
            let zeroed = if fragment { bytes } else { pages * PAGE_BYTES };
            // SAFETY: the bitmap gave this task the run, which lies inside
            // the chunk the source lent, and a fragment's bytes are its
            // own inside it, so nothing else holds a reference to them.
            // The device may write them, which a byte write tolerates.
            unsafe { ptr.write_bytes(0, zeroed) };
        }
        ArenaBuffer {
            ptr: NonNull::new(ptr).expect("a chunk's pages are never at address zero"),
            len: bytes,
            device: DeviceAddress::new(chunk.device.load(Ordering::Relaxed) + offset as u64),
            chunk: NonNull::from(chunk),
            first_page: first_page as u32,
            pages: pages as u32,
            fragment,
        }
    }
}

impl<S: ChunkSource, D: ChunkDomain> DmaArena<S, D, Elastic> {
    /// Gives back the chunks with no page in use, keeping `retained` empty
    /// chunks of each request class, and returns how many it gave back.
    ///
    /// Walks the empty set, which every free that emptied a chunk noted
    /// in. A chunk an allocation entered since is skipped: its next free
    /// notes it again. A retained chunk is noted again for the next pass.
    /// [`ChunkClass::Resident`] chunks never retire. Only an [`Elastic`]
    /// arena has this, so a disk arena cannot be shrunk.
    ///
    /// A retired chunk leaves in this order: its chunk map entry, so no new
    /// buffer resolves in it; the domain's unmap; the source's take-back.
    /// A domain that refuses the unmap keeps the chunk, which the device
    /// may still reach, and the arena never gives it back.
    ///
    /// First it detaches the fragment rows whose run has no outstanding
    /// fragment, which frees those runs and notes their chunks.
    ///
    /// Maintenance task context; waits for the `grow` lock.
    pub async fn retire_empty(&self, retained: usize) -> usize {
        let mut grow = self.lock_grow().await;
        // Before the walk, so a run this frees notes its chunk in time.
        self.detach_idle_runs(&grow);
        let mut kept = [0_usize; ChunkClass::COUNT];
        let mut retired = 0;
        for (index, segment) in self.segments.iter().enumerate() {
            let segment = segment.load(Ordering::Acquire);
            if segment.is_null() {
                continue;
            }
            // SAFETY: the segment is live until the arena drops.
            let segment = unsafe { &*segment };
            for (word_index, word) in segment.empty.iter().enumerate() {
                let mut noted = word.swap(0, Ordering::AcqRel);
                while noted != 0 {
                    let position = word_index * WORD_PAGES + noted.trailing_zeros() as usize;
                    noted &= noted - 1;
                    let chunk = &segment.chunks[position];
                    let slot = FIRST_SEGMENT_CHUNKS * ((1 << index) - 1) + position;
                    if self.retire(&mut grow, chunk, slot, retained, &mut kept) {
                        retired += 1;
                    }
                }
            }
        }
        retired
    }

    /// Retries the growth an allocation was refused, one chunk for each
    /// class whose `refused` bit is set, and says whether a lend stayed
    /// refused.
    ///
    /// A bit is cleared before its retry, so a refusal that lands during
    /// the retry sets it again; a retry the source refuses sets it again
    /// too, without waking anyone, because the caller learns it from the
    /// answer and parks on the source. A domain that refuses the map is
    /// logged and its bit stays clear: no retry changes that answer.
    ///
    /// Maintenance task context; waits for the `grow` lock.
    async fn grow_refused(&self) -> bool {
        if self.refused.load(Ordering::Acquire) == 0 {
            return false;
        }
        let mut grow = self.lock_grow().await;
        let bits = self.refused.swap(0, Ordering::AcqRel);
        let mut stayed = false;
        for class in ChunkClass::ALL {
            let bit = 1 << class.index();
            if bits & bit == 0 {
                continue;
            }
            match self.grow_one(&mut grow, class) {
                Ok(_) => {}
                Err(GrowError::Refused) => {
                    self.refused.fetch_or(bit, Ordering::Release);
                    stayed = true;
                }
                Err(GrowError::Domain(error)) => tracing::error!(
                    ?class,
                    %error,
                    "a DMA domain refused to map a chunk the arena retried for"
                ),
            }
        }
        stayed
    }

    /// Retires the chunk `chunk` of slot `slot` when it is empty and past
    /// the retained count of its class. Whether it did.
    fn retire(
        &self,
        grow: &mut GrowGuard<'_>,
        chunk: &Chunk,
        slot: usize,
        retained: usize,
        kept: &mut [usize; ChunkClass::COUNT],
    ) -> bool {
        let state = chunk.state.load(Ordering::Acquire);
        let class = ChunkClass::of_state(state);
        if state & RETIRING != 0 || class == ChunkClass::Resident || state & COUNT_MASK != 0 {
            return false;
        }
        if kept[class.index()] < retained {
            kept[class.index()] += 1;
            chunk.note_empty();
            return false;
        }
        // An allocation that entered since the load makes this fail, and
        // the free that empties the chunk again notes it again.
        if chunk
            .state
            .compare_exchange(
                state,
                state | RETIRING,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return false;
        }
        let base = chunk.base.load(Ordering::Relaxed);
        let device = DeviceAddress::new(chunk.device.load(Ordering::Relaxed));
        self.map.remove(grow, base);
        let lent = LentChunk::new(base, self.chunk_bytes);
        if let Err(error) = self.domain.unmap(lent, device) {
            tracing::error!(
                chunk = format_args!("{base:#x}"),
                %error,
                "a DMA domain refused to unmap a retired chunk; the arena keeps it"
            );
            return false;
        }
        self.source.take_back(lent);
        grow.retired_slots.push(slot as u32);
        self.counters.returned.fetch_add(1, Ordering::Relaxed);
        true
    }
}

/// The elastic arenas that share one chunk source, and what the
/// maintenance pass does for them.
///
/// Built mutably on the bootstrap processor, one arena per device, and then
/// moved into the maintenance task: one owner, no lock. An arena is shared
/// with its driver through the `Arc` it returns.
pub struct DmaArenas<S: ChunkSource + Clone, D: ChunkDomain> {
    source: S,
    demand: Arc<GrowthDemand>,
    arenas: Vec<Arc<DmaArena<S, D, Elastic>>>,
}

impl<S: ChunkSource + Clone, D: ChunkDomain> DmaArenas<S, D> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            demand: Arc::new(GrowthDemand::new()),
            arenas: Vec::new(),
        }
    }

    /// Builds and registers an elastic arena of `chunk_bytes` chunks for
    /// `processors` processors. Bring-up only.
    pub fn arena(
        &mut self,
        domain: D,
        chunk_bytes: usize,
        processors: usize,
    ) -> Arc<DmaArena<S, D, Elastic>> {
        let arena = Arc::new(DmaArena::new(
            self.source.clone(),
            domain,
            Elastic::new(self.demand.clone()),
            chunk_bytes,
            processors,
        ));
        self.arenas.push(arena.clone());
        arena
    }

    /// Samples the demand signal, which an allocation refused a chunk
    /// raises after it set its arena's `refused` bit.
    ///
    /// The maintenance pass takes it before [`DmaArenas::grow_refused`]
    /// reads the bits, so a refusal that lands after the read moves the
    /// signal past the mark.
    pub fn demand_mark(&self) -> ProgressMark {
        self.demand.kick.mark()
    }

    /// Resolves once an allocation was refused a chunk after `mark`.
    pub fn demand_changed(&self, mark: ProgressMark) -> impl Future<Output = ()> + Send + '_ {
        self.demand.kick.changed(mark)
    }

    /// Every arena's [`DmaArena::retire_empty`], one after another. Returns
    /// the chunks given back.
    pub async fn retire_empty(&self, retained: usize) -> usize {
        let mut retired = 0;
        for arena in &self.arenas {
            retired += arena.retire_empty(retained).await;
        }
        retired
    }

    /// Retries the growth refused allocations left undone.
    ///
    /// Marks the source wanted first, so a chunk that comes back after this
    /// call moves the source's return signal past the mark the answer
    /// carries; then, for each arena, one growth of each class whose
    /// `refused` bit is set, under that arena's `grow`. `Some(mark)` when a
    /// lend stayed refused: the caller parks on
    /// [`ChunkSource::chunk_returned`] with it. Each retry is the test the
    /// mark was armed before.
    ///
    /// Maintenance task context.
    pub async fn grow_refused(&self) -> Option<ProgressMark> {
        let mark = self.source.want_chunk();
        let mut stayed = false;
        for arena in &self.arenas {
            stayed |= arena.grow_refused().await;
        }
        stayed.then_some(mark)
    }
}

impl<S: ChunkSource, D: ChunkDomain, G: ChunkGrowth> Drop for DmaArena<S, D, G> {
    /// Unmaps every live chunk and gives it back to the source.
    ///
    /// A page still live means a driver outlived its device's memory, and
    /// the device may be writing it: the arena panics. It leaks its chunk
    /// table first, so the buffers that outlive the panic still free into
    /// valid headers, and it gives no chunk back. A retired chunk is
    /// already back and is skipped.
    fn drop(&mut self) {
        let slots = *self.counters.slots.get_mut();
        // A cached run is not a live page: the rows let go of theirs, which
        // frees the runs that have no outstanding fragment.
        for cache in self.frags.iter().flat_map(|caches| caches.iter()) {
            let word = cache.row.swap(RunRow::EMPTY, Ordering::Acquire);
            if let Some(held) = RunRow::unpack(word) {
                self.chunk(held.slot)
                    .put_run_refs(held.run * self.run_pages, held.held);
            }
        }
        let mut live_pages = 0;
        let mut live_chunks = 0;
        for slot in 0..slots {
            let in_use = self.chunk(slot).pages_in_use();
            live_pages += in_use;
            live_chunks += usize::from(in_use != 0);
        }
        assert!(
            live_pages == 0,
            "a DMA arena was dropped with {live_pages} live pages in {live_chunks} of its \
             {slots} chunks"
        );
        for slot in 0..slots {
            let chunk = self.chunk(slot);
            if chunk.state.load(Ordering::Relaxed) & RETIRING != 0 {
                continue;
            }
            let lent = LentChunk::new(chunk.base.load(Ordering::Relaxed), self.chunk_bytes);
            let device = DeviceAddress::new(chunk.device.load(Ordering::Relaxed));
            match self.domain.unmap(lent, device) {
                Ok(()) => self.source.take_back(lent),
                Err(error) => tracing::error!(
                    chunk = format_args!("{:#x}", lent.start()),
                    %error,
                    "a DMA domain refused to unmap a chunk of a dropped arena; it is not given back"
                ),
            }
        }
        for segment in &mut self.segments {
            let segment = *segment.get_mut();
            if !segment.is_null() {
                // SAFETY: the pointer came from `Box::into_raw` in
                // `publish`, and no page is live, so no header is read
                // again.
                drop(unsafe { Box::from_raw(segment) });
            }
        }
        // SAFETY: no page is live, so no header that points at the waiters
        // is read again, and nothing drops them twice.
        unsafe { ManuallyDrop::drop(&mut self.waiters) };
    }
}

/// Pages a driver holds, or a fragment of a run of them. Dropping it frees
/// them, or gives its share of the run back.
///
/// The drop reaches only the atomics of the buffer's chunk and the arena's
/// space waiters, so it takes no lock and is safe from an interrupt handler
/// on any processor. It raises the space signal only while an allocator is
/// starved.
pub struct ArenaBuffer {
    ptr: NonNull<u8>,
    len: usize,
    device: DeviceAddress,
    chunk: NonNull<Chunk>,
    /// The first page of the buffer's run, which a fragment shares.
    first_page: u32,
    pages: u32,
    /// The buffer is one fragment of its run, not the run: its drop
    /// subtracts from the run's count instead of freeing the pages.
    fragment: bool,
}

// SAFETY: the buffer owns its pages exclusively, and its chunk header is
// reached only through atomics.
unsafe impl Send for ArenaBuffer {}
// SAFETY: a shared reference exposes the bytes as `&[u8]` only.
unsafe impl Sync for ArenaBuffer {}

impl DmaBuffer for ArenaBuffer {
    fn device_address(&self) -> DeviceAddress {
        self.device
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl AsRef<[u8]> for ArenaBuffer {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: the buffer owns `len` bytes of its run.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl AsMut<[u8]> for ArenaBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` is the only access.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl fmt::Debug for ArenaBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArenaBuffer")
            .field("len", &self.len)
            .field("device", &self.device)
            .field("first_page", &self.first_page)
            .field("pages", &self.pages)
            .finish()
    }
}

impl Drop for ArenaBuffer {
    fn drop(&mut self) {
        // SAFETY: the arena keeps its chunk headers alive for as long as a
        // page is live: it panics, leaking them, otherwise.
        let chunk = unsafe { self.chunk.as_ref() };
        if self.fragment {
            chunk.put_run_refs(self.first_page as usize, 1);
        } else {
            chunk.free_pages(self.first_page as usize, self.pages as usize);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::alloc::{alloc_zeroed, dealloc};
    use alloc::sync::{Arc, Weak};
    use alloc::vec;
    use alloc::vec::Vec;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Mutex as StdMutex;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use futures_lite::future::block_on;
    use helios_hal::cpu::ProcessorId;

    use super::*;
    use crate::test_processor_identity;

    /// `2^64` divided by the golden ratio, as a source of test noise.
    const NOISE_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;

    /// What a source or a domain did to a chunk, by the chunk's address.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Event {
        Lend(usize),
        Map(usize),
        Unmap(usize),
        TakeBack(usize),
    }

    /// One aligned host block of `capacity` chunks, lent out one chunk at
    /// a time, followed by a guard chunk the arena never owns, so a test
    /// can build a slice that runs past the last chunk's end without
    /// leaving allocated memory. A chunk returned is not freed to the
    /// host: the block lives as long as the source does.
    struct Region {
        layout: Layout,
        base: usize,
        chunk_bytes: usize,
        lent: Vec<AtomicBool>,
        mapped: Vec<AtomicBool>,
        lends: AtomicUsize,
        returns: AtomicUsize,
        log: StdMutex<Vec<Event>>,
        /// Raised for every chunk that comes back.
        returned: ProgressSignal,
    }

    impl Region {
        fn slot(&self, start: usize) -> usize {
            assert!(
                start >= self.base && (start - self.base).is_multiple_of(self.chunk_bytes),
                "{start:#x} is not a chunk of this region"
            );
            (start - self.base) / self.chunk_bytes
        }
    }

    impl Drop for Region {
        fn drop(&mut self) {
            // SAFETY: `new` allocated the block with this layout.
            unsafe { dealloc(ptr::with_exposed_provenance_mut(self.base), self.layout) };
        }
    }

    #[derive(Clone)]
    struct TestSource(Arc<Region>);

    impl TestSource {
        fn new(chunk_bytes: usize, capacity: usize) -> Self {
            let layout = Layout::from_size_align(chunk_bytes * (capacity + 1), chunk_bytes)
                .expect("region layout");
            // SAFETY: the layout is not zero-sized.
            let block = unsafe { alloc_zeroed(layout) };
            assert!(!block.is_null(), "host memory");
            Self(Arc::new(Region {
                layout,
                base: block.expose_provenance(),
                chunk_bytes,
                lent: (0..capacity).map(|_| AtomicBool::new(false)).collect(),
                mapped: (0..capacity).map(|_| AtomicBool::new(false)).collect(),
                lends: AtomicUsize::new(0),
                returns: AtomicUsize::new(0),
                log: StdMutex::new(Vec::new()),
                returned: ProgressSignal::new(),
            }))
        }

        fn lends(&self) -> usize {
            self.0.lends.load(Ordering::SeqCst)
        }

        fn returns(&self) -> usize {
            self.0.returns.load(Ordering::SeqCst)
        }

        fn log(&self) -> Vec<Event> {
            self.0.log.lock().expect("the log").clone()
        }

        fn record(&self, event: Event) {
            self.0.log.lock().expect("the log").push(event);
        }

        /// Whether the chunk holding `address` is lent right now.
        fn is_lent(&self, address: usize) -> bool {
            let slot = (address - self.0.base) / self.0.chunk_bytes;
            self.0.lent[slot].load(Ordering::SeqCst)
        }

        /// The kernel address of the first byte of chunk `slot`.
        fn chunk_start(&self, slot: usize) -> usize {
            self.0.base + slot * self.0.chunk_bytes
        }
    }

    impl ChunkSource for TestSource {
        fn lend(&self, bytes: usize) -> Option<LentChunk> {
            assert_eq!(bytes, self.0.chunk_bytes, "a lend of the wrong size");
            let slot = (0..self.0.lent.len()).find(|&slot| {
                self.0.lent[slot]
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            })?;
            self.0.lends.fetch_add(1, Ordering::SeqCst);
            let start = self.chunk_start(slot);
            self.record(Event::Lend(start));
            Some(LentChunk::new(start, bytes))
        }

        fn take_back(&self, chunk: LentChunk) {
            let slot = self.0.slot(chunk.start());
            assert!(
                !self.0.mapped[slot].load(Ordering::SeqCst),
                "the chunk at {:#x} went back while a domain still mapped it",
                chunk.start()
            );
            assert!(
                self.0.lent[slot].swap(false, Ordering::SeqCst),
                "the chunk at {:#x} went back without being lent",
                chunk.start()
            );
            self.0.returns.fetch_add(1, Ordering::SeqCst);
            self.record(Event::TakeBack(chunk.start()));
            self.0.returned.signal();
        }

        fn span(&self) -> Range<usize> {
            self.0.base..self.0.base + self.0.lent.len() * self.0.chunk_bytes
        }

        fn want_chunk(&self) -> ProgressMark {
            self.0.returned.mark()
        }

        fn chunk_returned(&self, mark: ProgressMark) -> impl Future<Output = ()> + Send + '_ {
            self.0.returned.changed(mark)
        }
    }

    /// The device's address of the chunk at `start`: not the host address.
    fn device_of(start: usize) -> u64 {
        0x8000_0000_0000 + start as u64
    }

    /// Maps each chunk at `device_of` its address, and checks the arena's
    /// state at every call: a chunk is not in the chunk map while the
    /// domain maps or unmaps it.
    struct TestDomain<G: ChunkGrowth> {
        source: TestSource,
        arena: Weak<TestArena<G>>,
        /// Class of the last map, per request: what the arena asked for.
        accesses: StdMutex<Vec<DmaAccess>>,
    }

    type TestArena<G> = DmaArena<TestSource, TestDomain<G>, G>;

    impl<G: ChunkGrowth> TestDomain<G> {
        /// The arena, unless it is being dropped.
        fn arena(&self) -> Option<Arc<TestArena<G>>> {
            self.arena.upgrade()
        }
    }

    impl<G: ChunkGrowth> ChunkDomain for TestDomain<G> {
        fn map(&self, chunk: LentChunk, access: DmaAccess) -> Result<DeviceAddress, DmaError> {
            if let Some(arena) = self.arena() {
                assert_eq!(
                    arena.map.lookup(chunk.start()),
                    None,
                    "the chunk at {:#x} resolved before the domain mapped it",
                    chunk.start()
                );
            }
            self.accesses.lock().expect("accesses").push(access);
            self.source.0.mapped[self.source.0.slot(chunk.start())].store(true, Ordering::SeqCst);
            self.source.record(Event::Map(chunk.start()));
            Ok(DeviceAddress::new(device_of(chunk.start())))
        }

        fn unmap(&self, chunk: LentChunk, device: DeviceAddress) -> Result<(), DmaError> {
            assert_eq!(device.get(), device_of(chunk.start()));
            if let Some(arena) = self.arena() {
                assert_eq!(
                    arena.map.lookup(chunk.start()),
                    None,
                    "the chunk at {:#x} still resolved when the domain unmapped it",
                    chunk.start()
                );
            }
            self.source.0.mapped[self.source.0.slot(chunk.start())].store(false, Ordering::SeqCst);
            self.source.record(Event::Unmap(chunk.start()));
            Ok(())
        }
    }

    /// An arena with no chunk, over a source of `capacity` chunks.
    fn empty_arena<G: ChunkGrowth>(
        chunk_bytes: usize,
        capacity: usize,
        growth: G,
        processors: usize,
    ) -> (Arc<TestArena<G>>, TestSource) {
        let source = TestSource::new(chunk_bytes, capacity);
        let arena = Arc::new_cyclic(|weak| {
            DmaArena::new(
                source.clone(),
                TestDomain {
                    source: source.clone(),
                    arena: weak.clone(),
                    accesses: StdMutex::new(Vec::new()),
                },
                growth,
                chunk_bytes,
                processors,
            )
        });
        (arena, source)
    }

    /// A [`Fixed`] arena of one chunk per entry of `classes`, over a source
    /// that has no other.
    fn arena(
        chunk_bytes: usize,
        classes: &[ChunkClass],
        processors: usize,
    ) -> (Arc<TestArena<Fixed>>, TestSource) {
        let (arena, source) = empty_arena(chunk_bytes, classes.len(), Fixed, processors);
        for &class in classes {
            block_on(arena.reserve(class, 1)).expect("host memory");
        }
        (arena, source)
    }

    fn pages(count: usize) -> Layout {
        Layout::from_size_align(count * PAGE_BYTES, 1).expect("layout")
    }

    /// The bits set in every bitmap, less the padding past each chunk's
    /// last page.
    fn bitmap_pages_in_use<S: ChunkSource, D: ChunkDomain, G: ChunkGrowth>(
        arena: &DmaArena<S, D, G>,
    ) -> usize {
        (0..arena.counters.slots.load(Ordering::Acquire))
            .map(|slot| {
                let chunk = arena.chunk(slot);
                let set: usize = chunk
                    .bitmap
                    .iter()
                    .map(|word| word.load(Ordering::Acquire).count_ones() as usize)
                    .sum();
                set - (chunk.bitmap.len() * WORD_PAGES - chunk.pages as usize)
            })
            .sum()
    }

    fn chunk_range<S: ChunkSource, D: ChunkDomain, G: ChunkGrowth>(
        arena: &DmaArena<S, D, G>,
        slot: usize,
    ) -> (usize, usize) {
        let base = arena.chunk(slot).base.load(Ordering::Relaxed);
        (base, base + arena.chunk_bytes())
    }

    #[test]
    fn allocations_stay_inside_a_chunk_and_no_run_crosses_one() {
        let chunk_bytes = 16 * PAGE_BYTES;
        let (arena, _) = arena(chunk_bytes, &[ChunkClass::ReadWrite; 3], 1);

        let mut held = Vec::new();
        let mut sizes = [1, 3, 5, 2, 7, 4, 6].into_iter().cycle();
        loop {
            let want = sizes.next().expect("cycle");
            match arena.allocate(ChunkClass::ReadWrite, pages(want)) {
                Ok(buffer) => held.push((buffer, want)),
                Err(DmaError::Exhausted { .. }) => break,
                Err(other) => panic!("{other}"),
            }
        }
        assert!(held.len() > 6, "three chunks hold more than one run each");

        let mut runs = Vec::new();
        for (buffer, want) in &held {
            let start = buffer.as_ptr() as usize;
            let end = start + want * PAGE_BYTES;
            let inside = (0..3).any(|slot| {
                let (base, limit) = chunk_range(&arena, slot);
                base <= start && end <= limit
            });
            assert!(inside, "the run {start:#x}..{end:#x} crosses a chunk");
            assert_eq!(start % PAGE_BYTES, 0);
            runs.push((start, end));
        }
        runs.sort_unstable();
        for pair in runs.windows(2) {
            assert!(pair[0].1 <= pair[1].0, "runs overlap: {pair:x?}");
        }
        let held_pages: usize = held.iter().map(|(_, want)| want).sum();
        assert_eq!(arena.stats().pages_in_use, held_pages);
        assert_eq!(bitmap_pages_in_use(&arena), held_pages);

        // A run larger than a chunk is refused outright, not split.
        assert_eq!(
            arena
                .allocate(ChunkClass::ReadWrite, pages(17))
                .unwrap_err(),
            DmaError::TooLarge {
                bytes: 17 * PAGE_BYTES,
                limit: chunk_bytes
            }
        );
        assert_eq!(
            arena
                .allocate(
                    ChunkClass::ReadWrite,
                    Layout::from_size_align(0, 1).unwrap()
                )
                .unwrap_err(),
            DmaError::InvalidLength {
                bytes: 0,
                required_multiple: 1
            }
        );
    }

    #[test]
    fn a_run_spanning_bitmap_words_starts_on_a_word_and_stays_in_its_chunk() {
        // A chunk is four bitmap words (256 pages); a 100-page run takes two.
        let chunk_bytes = 256 * PAGE_BYTES;
        let (arena, _) = arena(chunk_bytes, &[ChunkClass::ReadWrite; 2], 1);

        let one = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("one page");
        let first = arena
            .allocate(ChunkClass::ReadWrite, pages(100))
            .expect("one run");
        let (base, _) = chunk_range(&arena, 0);
        assert_eq!(
            first.as_ptr() as usize - base,
            WORD_PAGES * PAGE_BYTES,
            "the first word is partly used, so the run starts at the second"
        );
        let mut runs = vec![first];
        while let Ok(run) = arena.allocate(ChunkClass::ReadWrite, pages(100)) {
            runs.push(run);
        }
        // Chunk 0 has words 3 and 4 left after the first run: one more run
        // fits there only if it starts at a word boundary it still has
        // whole, which is word 3 alone. So it does not fit; chunk 1 takes
        // two.
        assert_eq!(runs.len(), 3, "one run in chunk 0 and two in chunk 1");
        for run in &runs {
            let start = run.as_ptr() as usize;
            let end = start + 100 * PAGE_BYTES;
            assert!((0..2).any(|slot| {
                let (base, limit) = chunk_range(&arena, slot);
                base <= start && end <= limit
            }));
        }
        assert_eq!(arena.stats().pages_in_use, 301);
        assert_eq!(bitmap_pages_in_use(&arena), 301);
        drop(one);
    }

    #[test]
    fn an_aligned_run_starts_on_its_alignment() {
        let (arena, _) = arena(64 * PAGE_BYTES, &[ChunkClass::ReadWrite], 1);
        let _spacer = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("spacer");
        let layout = Layout::from_size_align(PAGE_BYTES, 4 * PAGE_BYTES).expect("layout");
        let run = arena
            .allocate(ChunkClass::ReadWrite, layout)
            .expect("aligned page");
        assert_eq!(run.as_ptr() as usize % (4 * PAGE_BYTES), 0);
    }

    #[test]
    fn a_page_comes_back_zeroed_after_the_device_wrote_it() {
        let (arena, _) = arena(4 * PAGE_BYTES, &[ChunkClass::ReadWrite], 1);
        let mut buffer = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("a page");
        buffer.as_mut().fill(0xaa);
        let address = buffer.as_ptr();
        drop(buffer);
        let again = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("the page again");
        assert_eq!(again.as_ptr(), address, "the freed page is the first fit");
        assert!(again.as_ref().iter().all(|&byte| byte == 0));
    }

    #[test]
    fn an_overwritten_allocation_leaves_the_pages_as_the_device_left_them() {
        let (arena, _) = arena(4 * PAGE_BYTES, &[ChunkClass::ReadWrite], 1);
        let mut buffer = arena
            .allocate(ChunkClass::ReadWrite, pages(2))
            .expect("two pages");
        buffer.as_mut().fill(0xaa);
        let address = buffer.as_ptr();
        drop(buffer);

        let again = arena
            .allocate_overwritten(ChunkClass::ReadWrite, pages(2))
            .expect("the pages again");
        assert_eq!(again.as_ptr(), address, "the freed pages are the first fit");
        assert_eq!(again.len(), 2 * PAGE_BYTES);
        assert!(
            again.as_ref().iter().all(|&byte| byte == 0xaa),
            "an overwritten allocation does not pay for a zeroing pass"
        );
        drop(again);

        // The zeroing allocation still zeroes what the other left.
        let zeroed = arena
            .allocate(ChunkClass::ReadWrite, pages(2))
            .expect("the pages once more");
        assert_eq!(zeroed.as_ptr(), address);
        assert!(zeroed.as_ref().iter().all(|&byte| byte == 0));
        assert_eq!(arena.max_run_bytes(), 4 * PAGE_BYTES);
    }

    #[test]
    fn a_chunk_serves_only_its_class() {
        let (arena, _) = arena(
            4 * PAGE_BYTES,
            &[ChunkClass::Read, ChunkClass::ReadWrite],
            1,
        );
        let (read_base, read_end) = chunk_range(&arena, 0);
        let buffer = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("read-write page");
        let address = buffer.as_ptr() as usize;
        assert!(
            !(read_base..read_end).contains(&address),
            "a read-write page came out of a read chunk"
        );
        assert_eq!(
            arena.allocate(ChunkClass::Resident, pages(1)).unwrap_err(),
            DmaError::Exhausted { bytes: PAGE_BYTES }
        );
    }

    #[test]
    fn device_range_refuses_what_is_not_inside_one_chunk_of_this_arena() {
        let chunk_bytes = 4 * PAGE_BYTES;
        let (arena, _) = arena(chunk_bytes, &[ChunkClass::ReadWrite; 2], 1);
        let (other, _) = self::arena(chunk_bytes, &[ChunkClass::ReadWrite], 1);

        let buffer = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("a page");
        assert_eq!(
            arena.device_range(buffer.as_ref()),
            Ok(buffer.device_address()),
            "a buffer's own bytes resolve to its device address"
        );
        let (base, limit) = chunk_range(&arena, 0);
        let offset = buffer.as_ptr() as usize - base;
        assert_eq!(
            buffer.device_address().get(),
            device_of(base) + offset as u64
        );

        // The heap.
        let heap = vec![0_u8; PAGE_BYTES];
        assert_eq!(
            arena.device_range(&heap),
            Err(DmaError::Unreachable {
                address: heap.as_ptr() as usize,
                bytes: PAGE_BYTES
            })
        );

        // Another arena's chunk.
        let foreign = other
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("a page");
        assert!(matches!(
            arena.device_range(foreign.as_ref()),
            Err(DmaError::Unreachable { .. })
        ));
        assert!(other.device_range(foreign.as_ref()).is_ok());

        // A slice that ends exactly at the chunk's end is whole; one byte
        // further, into the guard region the source left, is refused.
        // SAFETY: the chunk's block is followed by a guard of the same
        // size, so both slices are inside live host memory.
        let (whole, past) = unsafe {
            (
                core::slice::from_raw_parts((limit - PAGE_BYTES) as *const u8, PAGE_BYTES),
                core::slice::from_raw_parts((limit - PAGE_BYTES) as *const u8, PAGE_BYTES + 1),
            )
        };
        assert_eq!(
            arena.device_range(whole),
            Ok(DeviceAddress::new(
                device_of(base) + (chunk_bytes - PAGE_BYTES) as u64
            ))
        );
        assert_eq!(
            arena.device_range(past),
            Err(DmaError::Unreachable {
                address: limit - PAGE_BYTES,
                bytes: PAGE_BYTES + 1
            })
        );
    }

    #[test]
    fn pages_allocated_on_one_thread_and_freed_on_another_leave_every_count_exact() {
        const THREADS: usize = 8;
        const ROUNDS: usize = 3000;
        const CHUNKS: usize = 8;
        let chunk_bytes = 256 * PAGE_BYTES;
        let (arena, source) = arena(chunk_bytes, &[ChunkClass::ReadWrite; CHUNKS], THREADS);

        let (senders, receivers): (Vec<_>, Vec<_>) = (0..THREADS)
            .map(|_| mpsc::channel::<(ArenaBuffer, u8)>())
            .unzip();
        let all_sent = AtomicUsize::new(0);
        thread::scope(|scope| {
            for (me, receiver) in receivers.into_iter().enumerate() {
                let arena = &arena;
                let all_sent = &all_sent;
                let next = senders[(me + 1) % THREADS].clone();
                scope.spawn(move || {
                    test_processor_identity::set(ProcessorId::new(me as u16));
                    let check = |(buffer, tag): (ArenaBuffer, u8)| {
                        assert!(
                            buffer.as_ref().iter().all(|&byte| byte == tag),
                            "a buffer's bytes were written by someone else"
                        );
                    };
                    let mut random = (me as u64 + 1).wrapping_mul(NOISE_MULTIPLIER);
                    let mut sent = 0;
                    while sent < ROUNDS {
                        for received in receiver.try_iter() {
                            check(received);
                        }
                        random ^= random << 13;
                        random ^= random >> 7;
                        random ^= random << 17;
                        // At least two pages, less the noise, so every
                        // buffer is a run of pages: a buffer under a page is
                        // a fragment, and the fragment tests cover those.
                        let run = if random.is_multiple_of(16) {
                            70
                        } else {
                            2 + (random >> 8) as usize % 8
                        };
                        let bytes = run * PAGE_BYTES - (random >> 20) as usize % 100;
                        let layout = Layout::from_size_align(bytes, 1).expect("layout");
                        match arena.allocate(ChunkClass::ReadWrite, layout) {
                            Ok(mut buffer) => {
                                let tag = 1 + (sent % 250) as u8;
                                buffer.as_mut().fill(tag);
                                next.send((buffer, tag)).expect("the receiver is alive");
                                sent += 1;
                            }
                            Err(DmaError::Exhausted { .. }) => thread::yield_now(),
                            Err(other) => panic!("{other}"),
                        }
                    }
                    // Keep freeing what the others send until all are done:
                    // a thread parked here would starve their allocations.
                    all_sent.fetch_add(1, Ordering::AcqRel);
                    while all_sent.load(Ordering::Acquire) < THREADS {
                        for received in receiver.try_iter() {
                            check(received);
                        }
                        thread::yield_now();
                    }
                    for received in receiver.try_iter() {
                        check(received);
                    }
                });
            }
        });
        drop(senders);

        let stats = arena.stats();
        assert_eq!(stats.pages_in_use, 0);
        assert_eq!(bitmap_pages_in_use(&arena), 0);
        assert_eq!(stats.empty_chunks, CHUNKS);
        assert_eq!(
            stats.noted_empty_chunks, CHUNKS,
            "every chunk that went empty is in the empty set"
        );
        for slot in 0..CHUNKS {
            assert_eq!(
                arena.chunk(slot).state.load(Ordering::Acquire),
                ChunkClass::ReadWrite.state_bits(),
                "a chunk's state is its class and a count of zero"
            );
        }
        drop(arena);
        assert_eq!(source.returns(), CHUNKS);
    }

    #[test]
    fn chunk_map_readers_racing_leaf_publication_never_miss_a_live_chunk() {
        const CHUNK: usize = PAGE_BYTES;
        // Two keys to a leaf: the second insert into a leaf must find the
        // leaf the first one published.
        const STEP: usize = LEAF_CHUNKS / 2;
        const KEYS: usize = 128;
        let address = |key: usize| (1 + key * STEP) * CHUNK;
        let device = |key: usize| 0x1000 * (key as u64 + 1);
        let map = ChunkMap::new(CHUNK..address(KEYS), CHUNK);
        assert_eq!(map.root.len(), KEYS / 2, "a root sized from the span");

        let published = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let passes = AtomicUsize::new(0);
        with_grow(|grow| {
            thread::scope(|scope| {
                for _ in 0..4 {
                    scope.spawn(|| {
                        loop {
                            let live = published.load(Ordering::Acquire);
                            for key in 0..live {
                                assert_eq!(
                                    map.lookup(address(key)),
                                    Some(device(key)),
                                    "a live chunk went missing while a leaf was published"
                                );
                            }
                            passes.fetch_add(1, Ordering::Relaxed);
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                    });
                }
                // The writer waits for a reader's pass after each insert: a
                // scheduler that starts the readers late would otherwise
                // finish every publication before the first lookup.
                for key in 0..KEYS - 1 {
                    let seen = passes.load(Ordering::Relaxed);
                    map.insert(grow, address(key), device(key));
                    published.store(key + 1, Ordering::Release);
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    while passes.load(Ordering::Relaxed) == seen {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "no reader finished a pass: they stopped on a failed lookup"
                        );
                        thread::yield_now();
                    }
                }
                stop.store(true, Ordering::Relaxed);
            });
            for key in 0..KEYS - 1 {
                assert_eq!(map.lookup(address(key)), Some(device(key)));
                assert_eq!(
                    map.lookup(address(key) + CHUNK - 1),
                    Some(device(key)),
                    "an address anywhere in the chunk resolves to it"
                );
            }
            assert_eq!(map.lookup(address(KEYS - 1)), None);
        });
    }

    #[test]
    fn a_returned_chunk_no_longer_resolves() {
        const CHUNK: usize = 4 * PAGE_BYTES;
        let map = ChunkMap::new(CHUNK..CHUNK * 8, CHUNK);
        with_grow(|grow| {
            map.insert(grow, 2 * CHUNK, 0x7000);
            map.insert(grow, 3 * CHUNK, 0x8000);
            assert_eq!(map.lookup(2 * CHUNK + 5), Some(0x7000));
            map.remove(grow, 2 * CHUNK);
            assert_eq!(map.lookup(2 * CHUNK + 5), None, "the removed chunk");
            assert_eq!(map.lookup(3 * CHUNK), Some(0x8000), "its neighbour stays");
            // A chunk goes in again at another address after a removal.
            map.insert(grow, 2 * CHUNK, 0x9000);
            assert_eq!(map.lookup(2 * CHUNK), Some(0x9000));
            // Outside the span nothing resolves.
            assert_eq!(map.lookup(CHUNK - 1), None);
            assert_eq!(map.lookup(CHUNK * 8), None);
            assert_eq!(map.lookup(0), None);
        });
    }

    /// Runs `body` holding a `grow` guard of a lock of its own, for the
    /// tests of the chunk map alone.
    fn with_grow<R>(body: impl FnOnce(&GrowGuard<'_>) -> R) -> R {
        let lock = Mutex::new(GrowState {
            retired_slots: Vec::new(),
        });
        let waiters = SpaceWaiters {
            starved: AtomicBool::new(false),
            space: ProgressSignal::new(),
        };
        let guard = GrowGuard {
            state: ManuallyDrop::new(block_on(lock.lock())),
            waiters: &waiters,
        };
        body(&guard)
    }

    #[test]
    #[should_panic(expected = "dropped with 1 live pages in 1 of its 1 chunks")]
    fn dropping_an_arena_with_a_live_page_panics() {
        let (arena, _) = arena(4 * PAGE_BYTES, &[ChunkClass::ReadWrite], 1);
        let _page = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("a page");
        drop(arena);
    }

    #[test]
    fn a_page_that_outlives_its_arena_frees_into_a_chunk_the_panic_left_alone() {
        let (arena, source) = arena(4 * PAGE_BYTES, &[ChunkClass::ReadWrite], 1);
        let mut page = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("a page");
        assert!(catch_unwind(AssertUnwindSafe(move || drop(arena))).is_err());
        assert_eq!(
            source.returns(),
            0,
            "a chunk with a live page is not given back"
        );
        page.as_mut().fill(1);
        drop(page);
    }

    /// How long a test waits for a parked thread before it calls the wake
    /// lost. A wake that works arrives in microseconds.
    const LOST_WAKE_TIMEOUT: Duration = Duration::from_secs(30);

    /// Allocates one page by the space protocol: on a refusal, arm, ask
    /// again, and park only if refused again. `parked` counts the threads
    /// between the second refusal and their wake.
    fn allocate_waiting<S: ChunkSource, D: ChunkDomain, G: ChunkGrowth>(
        arena: &DmaArena<S, D, G>,
        parked: &AtomicUsize,
        about_to_park: impl Fn(),
    ) -> ArenaBuffer {
        loop {
            match arena.allocate(ChunkClass::ReadWrite, pages(1)) {
                Ok(buffer) => return buffer,
                Err(DmaError::Exhausted { .. } | DmaError::Replenishing { .. }) => {}
                Err(other) => panic!("{other}"),
            }
            let mark = arena.want_space();
            match arena.allocate(ChunkClass::ReadWrite, pages(1)) {
                Ok(buffer) => return buffer,
                Err(DmaError::Exhausted { .. } | DmaError::Replenishing { .. }) => {}
                Err(other) => panic!("{other}"),
            }
            parked.fetch_add(1, Ordering::SeqCst);
            about_to_park();
            block_on(arena.space_changed(mark));
            parked.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_refused_allocator_parks_and_a_free_on_another_thread_releases_it() {
        let (arena, _) = arena(4 * PAGE_BYTES, &[ChunkClass::ReadWrite], 1);
        let held: Vec<_> = (0..4)
            .map(|_| {
                arena
                    .allocate(ChunkClass::ReadWrite, pages(1))
                    .expect("a page")
            })
            .collect();

        let (parking, parked_signal) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let waiter = {
            let arena = arena.clone();
            thread::spawn(move || {
                test_processor_identity::set(ProcessorId::new(0));
                let parked = AtomicUsize::new(0);
                let page = allocate_waiting(&arena, &parked, || {
                    parking.send(()).expect("the test is listening");
                });
                done.send(()).expect("the test is listening");
                drop(page);
            })
        };
        parked_signal
            .recv_timeout(LOST_WAKE_TIMEOUT)
            .expect("the allocator was refused at a full arena");
        assert!(arena.short_of_space(), "the refusal armed the arena");
        assert!(
            finished.recv_timeout(Duration::from_millis(50)).is_err(),
            "the allocator found a page in a full arena"
        );

        // The free is on this thread, which never allocated.
        let mut held = held.into_iter();
        drop(held.next());
        finished
            .recv_timeout(LOST_WAKE_TIMEOUT)
            .expect("a free on another thread did not release the parked allocator");
        waiter.join().expect("the waiter finished");
        assert!(!arena.short_of_space(), "the free cleared the flag");
        drop(held);
    }

    #[test]
    fn frees_with_nobody_starved_leave_the_space_signal_alone() {
        let (arena, _) = arena(4 * PAGE_BYTES, &[ChunkClass::ReadWrite], 1);
        let start = arena.space_mark();
        for _ in 0..3 {
            let page = arena
                .allocate(ChunkClass::ReadWrite, pages(2))
                .expect("pages");
            drop(page);
        }
        assert_eq!(
            arena.space_mark(),
            start,
            "a free raised an unwanted signal"
        );
        assert!(!arena.short_of_space());

        // A refusal arms the arena; the first free after it signals once,
        // and a second free in the same episode does not.
        let held: Vec<_> = (0..2)
            .map(|_| {
                arena
                    .allocate(ChunkClass::ReadWrite, pages(2))
                    .expect("pages")
            })
            .collect();
        assert!(arena.allocate(ChunkClass::ReadWrite, pages(1)).is_err());
        let mark = arena.want_space();
        assert!(arena.short_of_space());
        let mut held = held.into_iter();
        drop(held.next());
        let after_first = arena.space_mark();
        assert_eq!(
            after_first.get(),
            mark.get() + 1,
            "the first free of an episode signals exactly once"
        );
        assert!(!arena.short_of_space());
        drop(held.next());
        assert_eq!(arena.space_mark(), after_first, "the episode was over");
    }

    #[test]
    fn no_allocator_stays_parked_while_the_arena_has_a_free_page() {
        const WAITERS: usize = 6;
        const FREERS: usize = 2;
        const ROUNDS: usize = 3000;
        let (arena, _) = arena(4 * PAGE_BYTES, &[ChunkClass::ReadWrite], WAITERS);
        let parked = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));

        // Each waiter hands what it allocated to a freeing thread, so every
        // free is on a thread other than the one that waits for it.
        let (senders, receivers): (Vec<_>, Vec<_>) =
            (0..FREERS).map(|_| mpsc::channel::<ArenaBuffer>()).unzip();
        let freers: Vec<_> = receivers
            .into_iter()
            .map(|receiver| thread::spawn(move || receiver.into_iter().for_each(drop)))
            .collect();
        let waiters: Vec<_> = (0..WAITERS)
            .map(|me| {
                let (arena, parked, finished) = (arena.clone(), parked.clone(), finished.clone());
                let to_free = senders[me % FREERS].clone();
                thread::spawn(move || {
                    test_processor_identity::set(ProcessorId::new(me as u16));
                    for _ in 0..ROUNDS {
                        let page = allocate_waiting(&arena, &parked, || {});
                        to_free.send(page).expect("the freer is alive");
                    }
                    finished.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();
        drop(senders);

        let deadline = std::time::Instant::now() + LOST_WAKE_TIMEOUT;
        while finished.load(Ordering::SeqCst) < WAITERS && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let stats = arena.stats();
        assert_eq!(
            finished.load(Ordering::SeqCst),
            WAITERS,
            "{} allocators are parked while {} of {} pages are free: a wake was lost",
            parked.load(Ordering::SeqCst),
            stats.pages - stats.pages_in_use,
            stats.pages
        );
        for waiter in waiters {
            waiter.join().expect("a waiter finished");
        }
        for freer in freers {
            freer.join().expect("a freer finished");
        }
        assert_eq!(arena.stats().pages_in_use, 0);
    }

    #[test]
    fn dropping_an_idle_arena_gives_every_chunk_back() {
        let (arena, source) = arena(4 * PAGE_BYTES, &[ChunkClass::Read; 20], 1);
        assert_eq!(
            arena.stats().chunks,
            20,
            "the chunk table grew past a segment"
        );
        let page = arena.allocate(ChunkClass::Read, pages(2)).expect("a page");
        drop(page);
        drop(arena);
        assert_eq!(source.lends(), 20);
        assert_eq!(source.returns(), 20);
    }

    fn elastic(chunk_bytes: usize, capacity: usize, processors: usize) -> ElasticArena {
        empty_arena(
            chunk_bytes,
            capacity,
            Elastic::new(Arc::new(GrowthDemand::new())),
            processors,
        )
    }

    type ElasticArena = (Arc<TestArena<Elastic>>, TestSource);

    #[test]
    fn growth_maps_a_chunk_before_it_can_be_allocated() {
        let (arena, source) = elastic(4 * PAGE_BYTES, 4, 1);
        assert_eq!(arena.stats().chunks, 0, "an elastic arena starts empty");
        let buffer = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("the allocation grows the arena");
        let start = source.chunk_start(0);
        // The domain hooks assert that the chunk did not resolve at `Map`.
        assert_eq!(source.log(), [Event::Lend(start), Event::Map(start)]);
        assert_eq!(buffer.as_ptr() as usize, start);
        assert_eq!(buffer.device_address().get(), device_of(start));
        assert_eq!(
            arena.device_range(buffer.as_ref()),
            Ok(buffer.device_address())
        );
        assert_eq!(
            arena.stats().chunks_by_class,
            [0, 0, 1],
            "a read-write allocation grows a read-write chunk"
        );
        assert_eq!(arena.stats().grown, 1);
        let accesses = arena.domain.accesses.lock().expect("accesses").clone();
        assert_eq!(accesses, [DmaAccess::READ | DmaAccess::WRITE]);
    }

    #[test]
    fn retirement_unmaps_after_the_map_entry_goes_and_before_the_chunk_goes_back() {
        let (arena, source) = elastic(4 * PAGE_BYTES, 4, 1);
        let buffer = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("the allocation grows the arena");
        let start = source.chunk_start(0);
        // SAFETY: the source's block outlives the arena, so the chunk's
        // first byte is live host memory whether or not it is lent.
        let first_byte = unsafe { core::slice::from_raw_parts(start as *const u8, 1) };
        assert!(arena.device_range(first_byte).is_ok());
        assert_eq!(
            block_on(arena.retire_empty(0)),
            0,
            "a chunk with a page in use stays"
        );
        drop(buffer);

        assert_eq!(block_on(arena.retire_empty(0)), 1);
        // The domain hook asserts that the chunk was refused by the map at
        // `Unmap`, and the source that it was unmapped at `TakeBack`.
        assert_eq!(
            source.log(),
            [
                Event::Lend(start),
                Event::Map(start),
                Event::Unmap(start),
                Event::TakeBack(start)
            ]
        );
        assert!(matches!(
            arena.device_range(first_byte),
            Err(DmaError::Unreachable { .. })
        ));
        let stats = arena.stats();
        assert_eq!((stats.chunks, stats.grown, stats.returned), (0, 1, 1));
        assert!(!source.is_lent(start));
    }

    #[test]
    fn an_allocation_racing_a_retirement_never_gets_a_page_of_a_returned_chunk() {
        const ALLOCATORS: usize = 4;
        const RETIREMENTS: usize = 5000;
        let (arena, source) = elastic(16 * PAGE_BYTES, 4, ALLOCATORS + 1);
        let stop = AtomicBool::new(false);
        let retirements = AtomicUsize::new(0);
        thread::scope(|scope| {
            for me in 0..ALLOCATORS {
                let (arena, source, stop) = (&arena, &source, &stop);
                scope.spawn(move || {
                    test_processor_identity::set(ProcessorId::new(me as u16));
                    let mut done = 0_usize;
                    while !stop.load(Ordering::SeqCst) {
                        let buffer = match arena.allocate(ChunkClass::ReadWrite, pages(1)) {
                            Ok(buffer) => buffer,
                            Err(DmaError::Replenishing { .. } | DmaError::Exhausted { .. }) => {
                                thread::yield_now();
                                continue;
                            }
                            Err(other) => panic!("{other}"),
                        };
                        let address = buffer.as_ptr() as usize;
                        assert!(
                            source.is_lent(address),
                            "an allocation was served from a chunk already returned"
                        );
                        assert_eq!(
                            arena.device_range(buffer.as_ref()),
                            Ok(buffer.device_address()),
                            "a held page stopped resolving"
                        );
                        thread::yield_now();
                        assert!(
                            source.is_lent(address),
                            "a held page's chunk was returned under it"
                        );
                        drop(buffer);
                        done += 1;
                        // Leave the chunk empty long enough for the
                        // retiring thread to find it so.
                        thread::sleep(Duration::from_micros(50));
                    }
                    assert!(done > 0, "an allocator never got a page");
                });
            }
            scope.spawn(|| {
                test_processor_identity::set(ProcessorId::new(ALLOCATORS as u16));
                let deadline = std::time::Instant::now() + LOST_WAKE_TIMEOUT;
                while retirements.load(Ordering::Relaxed) < RETIREMENTS
                    && std::time::Instant::now() < deadline
                {
                    retirements.fetch_add(block_on(arena.retire_empty(0)), Ordering::Relaxed);
                    thread::yield_now();
                }
                stop.store(true, Ordering::SeqCst);
            });
        });
        assert!(
            retirements.load(Ordering::Relaxed) >= RETIREMENTS,
            "the retiring thread raced too few allocations to mean anything"
        );
        assert_eq!(arena.stats().pages_in_use, 0);
        block_on(arena.retire_empty(0));
        assert_eq!(arena.stats().chunks, 0);
        assert_eq!(source.lends(), source.returns());
    }

    #[test]
    fn retained_empty_chunks_stay() {
        let (arena, source) = elastic(4 * PAGE_BYTES, 4, 1);
        block_on(arena.reserve(ChunkClass::ReadWrite, 3)).expect("host memory");
        assert_eq!(block_on(arena.retire_empty(1)), 2);
        assert_eq!(arena.stats().chunks, 1);
        assert_eq!(block_on(arena.retire_empty(0)), 1);
        assert_eq!(arena.stats().chunks, 0);
        assert_eq!(source.returns(), 3);
    }

    #[test]
    fn retained_empty_chunks_are_counted_per_class() {
        let (arena, _) = elastic(4 * PAGE_BYTES, 6, 1);
        block_on(arena.reserve(ChunkClass::ReadWrite, 3)).expect("host memory");
        block_on(arena.reserve(ChunkClass::Read, 2)).expect("host memory");
        block_on(arena.reserve(ChunkClass::Resident, 1)).expect("host memory");
        assert_eq!(
            block_on(arena.retire_empty(1)),
            3,
            "two read-write and one read chunk go, one of each stays"
        );
        assert_eq!(arena.stats().chunks_by_class, [1, 1, 1]);
        assert_eq!(
            block_on(arena.retire_empty(0)),
            2,
            "a resident chunk never retires"
        );
        assert_eq!(arena.stats().chunks_by_class, [1, 0, 0]);
    }

    #[test]
    fn a_free_on_another_thread_empties_a_chunk_and_the_next_pass_returns_it() {
        let (arena, source) = elastic(4 * PAGE_BYTES, 4, 1);
        let buffer = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("the allocation grows the arena");
        assert_eq!(block_on(arena.retire_empty(0)), 0);
        // The pass consumed the chunk's empty-set bit while it was in use.
        assert_eq!(arena.stats().noted_empty_chunks, 0);
        thread::spawn(move || drop(buffer))
            .join()
            .expect("the freeing thread finished");
        assert_eq!(
            arena.stats().noted_empty_chunks,
            1,
            "the free that emptied the chunk noted it"
        );
        assert_eq!(block_on(arena.retire_empty(0)), 1);
        assert_eq!(source.returns(), 1);
    }

    #[test]
    fn a_contended_growth_answers_replenishing_and_the_release_wakes_the_waiter() {
        let (arena, _) = elastic(4 * PAGE_BYTES, 2, 1);
        let mut grow = arena.try_grow().expect("nobody holds the lock");
        assert_eq!(
            arena.allocate(ChunkClass::ReadWrite, pages(1)).unwrap_err(),
            DmaError::Replenishing { bytes: PAGE_BYTES },
            "a dry class whose growth is held answers Replenishing"
        );

        let (parking, parked_signal) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let waiter = {
            let arena = arena.clone();
            thread::spawn(move || {
                test_processor_identity::set(ProcessorId::new(0));
                let parked = AtomicUsize::new(0);
                let page = allocate_waiting(&arena, &parked, || {
                    parking.send(()).expect("the test is listening");
                });
                done.send(()).expect("the test is listening");
                drop(page);
            })
        };
        parked_signal
            .recv_timeout(LOST_WAKE_TIMEOUT)
            .expect("the allocator was told to wait for the growth");
        assert!(
            finished.recv_timeout(Duration::from_millis(50)).is_err(),
            "the allocator found a page before the growth published one"
        );

        arena
            .grow_one(&mut grow, ChunkClass::ReadWrite)
            .unwrap_or_else(|_| panic!("the source has a chunk"));
        drop(grow);
        finished
            .recv_timeout(LOST_WAKE_TIMEOUT)
            .expect("the release of `grow` did not wake the parked allocator");
        waiter.join().expect("the waiter finished");
    }

    #[test]
    fn a_refused_growth_wakes_replenishing_waiters_who_then_learn_exhausted() {
        let (arena, source) = elastic(4 * PAGE_BYTES, 1, 1);
        // The source has nothing left to lend.
        let taken = source.lend(4 * PAGE_BYTES).expect("the one chunk");
        let mut grow = arena.try_grow().expect("nobody holds the lock");

        let (parking, parked_signal) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let waiter = {
            let arena = arena.clone();
            thread::spawn(move || {
                test_processor_identity::set(ProcessorId::new(0));
                let ask = || arena.allocate(ChunkClass::ReadWrite, pages(1)).map(drop);
                assert_eq!(ask(), Err(DmaError::Replenishing { bytes: PAGE_BYTES }));
                let mark = arena.want_space();
                assert_eq!(ask(), Err(DmaError::Replenishing { bytes: PAGE_BYTES }));
                parking.send(()).expect("the test is listening");
                block_on(arena.space_changed(mark));
                done.send(ask()).expect("the test is listening");
            })
        };
        parked_signal
            .recv_timeout(LOST_WAKE_TIMEOUT)
            .expect("the allocator was told to wait for the growth");

        assert!(matches!(
            arena.grow_one(&mut grow, ChunkClass::ReadWrite),
            Err(GrowError::Refused)
        ));
        drop(grow);
        assert_eq!(
            finished
                .recv_timeout(LOST_WAKE_TIMEOUT)
                .expect("a refused growth did not wake the parked allocator"),
            Err(DmaError::Exhausted { bytes: PAGE_BYTES }),
            "once the lock is free the refusal is the allocation's own"
        );
        waiter.join().expect("the waiter finished");
        source.take_back(taken);
    }

    #[test]
    fn a_reused_header_serves_only_its_new_class() {
        let (arena, _) = elastic(4 * PAGE_BYTES, 4, 1);
        block_on(arena.reserve(ChunkClass::ReadWrite, 1)).expect("host memory");
        assert_eq!(block_on(arena.retire_empty(0)), 1);
        block_on(arena.reserve(ChunkClass::Read, 1)).expect("host memory");
        assert_eq!(
            arena.counters.slots.load(Ordering::Relaxed),
            1,
            "the retired header was reused, not a new slot"
        );
        assert_eq!(arena.stats().chunks_by_class, [0, 1, 0]);

        let (read_start, read_end) = chunk_range(&arena, 0);
        let page = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("the arena grows a read-write chunk");
        assert!(
            !(read_start..read_end).contains(&(page.as_ptr() as usize)),
            "a read-write page came out of the chunk that reused a read-write header"
        );
        assert_eq!(arena.stats().chunks_by_class, [0, 1, 1]);
    }

    #[test]
    fn dropping_an_arena_gives_back_only_its_live_chunks() {
        let (arena, source) = elastic(4 * PAGE_BYTES, 4, 1);
        block_on(arena.reserve(ChunkClass::ReadWrite, 3)).expect("host memory");
        assert_eq!(block_on(arena.retire_empty(2)), 1);
        assert_eq!((source.lends(), source.returns()), (3, 1));
        drop(arena);
        assert_eq!(
            source.returns(),
            3,
            "the arena gave back exactly lent minus returned"
        );
    }

    #[test]
    fn a_fixed_arena_never_lends_for_a_request_class() {
        let (arena, source) = empty_arena(4 * PAGE_BYTES, 4, Fixed, 1);
        block_on(arena.reserve(ChunkClass::ReadWrite, 1)).expect("host memory");
        let held: Vec<_> = (0..4)
            .map(|_| {
                arena
                    .allocate(ChunkClass::ReadWrite, pages(1))
                    .expect("a page of the reserved chunk")
            })
            .collect();
        let lends = source.lends();
        assert_eq!(
            arena.allocate(ChunkClass::ReadWrite, pages(1)).unwrap_err(),
            DmaError::Exhausted { bytes: PAGE_BYTES }
        );
        assert_eq!(source.lends(), lends, "a refusal lent nothing");

        let resident = arena
            .allocate(ChunkClass::Resident, pages(1))
            .expect("a resident allocation still grows");
        assert_eq!(source.lends(), lends + 1);
        assert_eq!(arena.stats().chunks_by_class, [1, 0, 1]);
        drop((held, resident));
    }

    #[test]
    fn a_refused_allocator_is_not_woken_by_its_own_attempt() {
        let (arena, source) = elastic(4 * PAGE_BYTES, 1, 1);
        let _taken = source.lend(4 * PAGE_BYTES).expect("the one chunk");
        let refused = DmaError::Exhausted { bytes: PAGE_BYTES };
        assert_eq!(
            arena.allocate(ChunkClass::ReadWrite, pages(1)).unwrap_err(),
            refused
        );
        let mark = arena.want_space();
        assert_eq!(
            arena.allocate(ChunkClass::ReadWrite, pages(1)).unwrap_err(),
            refused
        );
        assert_eq!(
            arena.space_mark(),
            mark,
            "the second attempt raised the signal its caller parks on"
        );
        assert!(arena.short_of_space(), "the arena is still armed");
    }

    type SharedArenas = DmaArenas<TestSource, TestDomain<Elastic>>;

    /// Two elastic arenas of one source of `capacity` chunks.
    fn shared(
        chunk_bytes: usize,
        capacity: usize,
        processors: usize,
    ) -> (Arc<SharedArenas>, [Arc<TestArena<Elastic>>; 2], TestSource) {
        let source = TestSource::new(chunk_bytes, capacity);
        let mut arenas = DmaArenas::new(source.clone());
        let domain = || TestDomain {
            source: source.clone(),
            arena: Weak::new(),
            accesses: StdMutex::new(Vec::new()),
        };
        let first = arenas.arena(domain(), chunk_bytes, processors);
        let second = arenas.arena(domain(), chunk_bytes, processors);
        (Arc::new(arenas), [first, second], source)
    }

    #[test]
    fn a_refused_growth_retries_when_the_source_takes_a_chunk_back() {
        let (arenas, [x, y], source) = shared(4 * PAGE_BYTES, 2, 2);
        // X takes the whole source.
        let held: Vec<_> = (0..8)
            .map(|_| {
                x.allocate(ChunkClass::ReadWrite, pages(1))
                    .expect("a page of X")
            })
            .collect();
        assert_eq!(source.lends(), 2);

        // Y is refused: its bit is set and the demand signal moved.
        let before = arenas.demand_mark();
        assert_eq!(
            y.allocate(ChunkClass::ReadWrite, pages(1)).unwrap_err(),
            DmaError::Exhausted { bytes: PAGE_BYTES }
        );
        assert_ne!(arenas.demand_mark(), before, "the refusal raised the kick");
        assert_eq!(
            y.refused.load(Ordering::Acquire),
            1 << ChunkClass::ReadWrite.index()
        );

        let (parking, parked_signal) = mpsc::channel();
        let (maintaining, maintenance_parked) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let waiter = {
            let y = y.clone();
            thread::spawn(move || {
                test_processor_identity::set(ProcessorId::new(1));
                let parked = AtomicUsize::new(0);
                let page = allocate_waiting(&y, &parked, || {
                    parking.send(()).expect("the test is listening");
                });
                done.send(()).expect("the test is listening");
                drop(page);
            })
        };
        parked_signal
            .recv_timeout(LOST_WAKE_TIMEOUT)
            .expect("Y's allocator was refused at a dry source");

        let maintenance = {
            let (arenas, source) = (arenas.clone(), source.clone());
            thread::spawn(move || {
                test_processor_identity::set(ProcessorId::new(0));
                let mark = block_on(arenas.grow_refused())
                    .expect("Y's lend stays refused while X holds the source");
                maintaining.send(()).expect("the test is listening");
                block_on(source.chunk_returned(mark));
                assert!(
                    block_on(arenas.grow_refused()).is_none(),
                    "the retry after a chunk came back grew Y"
                );
            })
        };
        maintenance_parked
            .recv_timeout(LOST_WAKE_TIMEOUT)
            .expect("the maintenance pass retried and learned the lend stayed refused");
        assert!(
            finished.recv_timeout(Duration::from_millis(50)).is_err(),
            "Y's allocator found a page while the source was dry"
        );

        drop(held);
        assert_eq!(block_on(x.retire_empty(0)), 2);
        finished
            .recv_timeout(LOST_WAKE_TIMEOUT)
            .expect("a chunk that came back did not reach the refused allocator");
        waiter.join().expect("the waiter finished");
        maintenance.join().expect("the maintenance pass finished");
        assert_eq!(y.stats().chunks_by_class, [0, 0, 1]);
    }

    #[test]
    fn no_allocator_stays_parked_while_the_arena_or_the_source_has_room() {
        const WAITERS: usize = 6;
        const FREERS: usize = 2;
        const ROUNDS: usize = 1500;
        // Chunks of four pages, three of them for two arenas: either one
        // can take them all, so each is refused while the other holds
        // them, and the loop below is what moves them.
        let (arenas, arena, source) = shared(4 * PAGE_BYTES, 3, WAITERS + 1);
        let parked = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));

        let (senders, receivers): (Vec<_>, Vec<_>) =
            (0..FREERS).map(|_| mpsc::channel::<ArenaBuffer>()).unzip();
        let freers: Vec<_> = receivers
            .into_iter()
            .map(|receiver| thread::spawn(move || receiver.into_iter().for_each(drop)))
            .collect();
        let waiters: Vec<_> = (0..WAITERS)
            .map(|me| {
                let (arena, parked, finished) =
                    (arena[me % 2].clone(), parked.clone(), finished.clone());
                let to_free = senders[me % FREERS].clone();
                thread::spawn(move || {
                    test_processor_identity::set(ProcessorId::new(me as u16));
                    for _ in 0..ROUNDS {
                        let page = allocate_waiting(&arena, &parked, || {});
                        to_free.send(page).expect("the freer is alive");
                    }
                    finished.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();
        drop(senders);
        let maintenance = {
            let (arenas, finished) = (arenas.clone(), finished.clone());
            thread::spawn(move || {
                test_processor_identity::set(ProcessorId::new(WAITERS as u16));
                while finished.load(Ordering::SeqCst) < WAITERS {
                    block_on(arenas.grow_refused());
                    block_on(arenas.retire_empty(0));
                    thread::yield_now();
                }
            })
        };

        let deadline = std::time::Instant::now() + LOST_WAKE_TIMEOUT;
        while finished.load(Ordering::SeqCst) < WAITERS && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            finished.load(Ordering::SeqCst),
            WAITERS,
            "{} allocators are parked with {} of {} chunks lent: a wake was lost",
            parked.load(Ordering::SeqCst),
            source.lends() - source.returns(),
            3
        );
        for waiter in waiters {
            waiter.join().expect("a waiter finished");
        }
        maintenance.join().expect("the maintenance loop finished");
        for freer in freers {
            freer.join().expect("a freer finished");
        }
        for arena in &arena {
            assert_eq!(arena.stats().pages_in_use, 0);
        }
        block_on(arenas.retire_empty(0));
        assert_eq!(source.lends(), source.returns());
    }

    fn fragment_layout(bytes: usize, align: usize) -> Layout {
        Layout::from_size_align(bytes, align).expect("layout")
    }

    /// The fragment row of `processor` for `class`, decoded.
    fn row_of<S: ChunkSource, D: ChunkDomain, G: ChunkGrowth>(
        arena: &DmaArena<S, D, G>,
        processor: usize,
        class: ChunkClass,
    ) -> Option<RunRow> {
        let index = class.fragment_index().expect("a request class");
        RunRow::unpack(arena.frags[processor][index].row.load(Ordering::SeqCst))
    }

    /// The count of the run `run` of the chunk in `slot`.
    fn run_count<S: ChunkSource, D: ChunkDomain, G: ChunkGrowth>(
        arena: &DmaArena<S, D, G>,
        slot: usize,
        run: usize,
    ) -> u32 {
        arena.chunk(slot).fragment_refs[run].load(Ordering::SeqCst)
    }

    #[test]
    fn a_row_packs_every_field_at_its_widest() {
        let widest = RunRow {
            slot: MAX_CHUNKS - 1,
            run: MAX_RUNS_PER_CHUNK - 1,
            offset: FRAGMENT_RUN_BYTES,
            held: BIAS,
        };
        assert_eq!(RunRow::unpack(widest.pack()), Some(widest));
        let narrowest = RunRow {
            slot: 0,
            run: 0,
            offset: 0,
            held: 1,
        };
        assert_eq!(RunRow::unpack(narrowest.pack()), Some(narrowest));
        assert_eq!(RunRow::unpack(RunRow::EMPTY), None);
        assert_eq!(HELD_BITS + OFFSET_BITS + RUN_BITS + SLOT_BITS, u64::BITS);
    }

    #[test]
    fn fragments_share_a_run() {
        let (arena, _) = arena(16 * PAGE_BYTES, &[ChunkClass::ReadWrite], 1);
        let layout = fragment_layout(100, 64);
        let first = arena
            .allocate(ChunkClass::ReadWrite, layout)
            .expect("a fragment");
        let second = arena
            .allocate(ChunkClass::ReadWrite, layout)
            .expect("another fragment");

        let (a, b) = (first.as_ptr() as usize, second.as_ptr() as usize);
        assert_eq!((a % 64, b % 64), (0, 0), "a fragment is aligned as asked");
        assert!(a + 100 <= b || b + 100 <= a, "the fragments overlap");
        let (base, _) = chunk_range(&arena, 0);
        for start in [a, b] {
            assert!(
                (base..base + FRAGMENT_RUN_BYTES).contains(&start),
                "{start:#x} is outside the first run"
            );
        }
        assert_eq!(first.len(), 100);
        assert_eq!(
            arena.device_range(first.as_ref()),
            Ok(first.device_address())
        );
        assert_eq!(arena.stats().pages_in_use, FRAGMENT_RUN_PAGES);
        assert_eq!(bitmap_pages_in_use(&arena), FRAGMENT_RUN_PAGES);
        assert_eq!(
            run_count(&arena, 0, 0),
            BIAS,
            "the row's units and the fragments' make the starting count"
        );
        let row = row_of(&arena, 0, ChunkClass::ReadWrite).expect("the row caches the run");
        assert_eq!(row.held, BIAS - 2, "each fragment spent a unit");
    }

    #[test]
    fn a_run_is_freed_when_its_last_fragment_drops_on_another_thread() {
        let (arena, _) = elastic(16 * PAGE_BYTES, 2, 1);
        let layout = fragment_layout(3000, 1);
        let mut old: Vec<_> = (0..10)
            .map(|_| {
                arena
                    .allocate(ChunkClass::ReadWrite, layout)
                    .expect("a fragment")
            })
            .collect();
        assert_eq!(arena.stats().pages_in_use, FRAGMENT_RUN_PAGES);

        // The eleventh does not fit what is left of the run, so a new run
        // replaces it in the row and the old one's unspent units go.
        let eleventh = arena
            .allocate(ChunkClass::ReadWrite, layout)
            .expect("a fragment of a new run");
        assert_eq!(arena.stats().pages_in_use, 2 * FRAGMENT_RUN_PAGES);
        assert_eq!(run_count(&arena, 0, 0), 10, "only the fragments' units");
        drop(eleventh);
        block_on(arena.retire_empty(1));
        assert_eq!(
            arena.stats().pages_in_use,
            FRAGMENT_RUN_PAGES,
            "the idle run went with its row"
        );

        let last = old.pop().expect("ten fragments");
        thread::spawn(move || drop(old))
            .join()
            .expect("the drop finished");
        assert_eq!(
            arena.stats().pages_in_use,
            FRAGMENT_RUN_PAGES,
            "a run with a fragment left is not freed"
        );
        thread::spawn(move || drop(last))
            .join()
            .expect("the drop finished");
        assert_eq!(arena.stats().pages_in_use, 0);
        assert_eq!(bitmap_pages_in_use(&arena), 0);
        assert_eq!(run_count(&arena, 0, 0), 0);
    }

    #[test]
    fn a_page_or_more_bypasses_the_cache() {
        let (arena, _) = elastic(16 * PAGE_BYTES, 2, 1);
        let page = arena
            .allocate(ChunkClass::ReadWrite, fragment_layout(PAGE_BYTES, 1))
            .expect("a page");
        let more = arena
            .allocate(ChunkClass::ReadWrite, fragment_layout(5000, 1))
            .expect("two pages");
        let aligned = arena
            .allocate(ChunkClass::ReadWrite, fragment_layout(100, 2 * PAGE_BYTES))
            .expect("a page aligned past one");
        assert_eq!(row_of(&arena, 0, ChunkClass::ReadWrite), None);
        assert_eq!(arena.stats().pages_in_use, 4);
        assert!((0..2).all(|run| run_count(&arena, 0, run) == 0));
        assert_eq!(aligned.as_ptr() as usize % (2 * PAGE_BYTES), 0);

        // Resident memory is whole pages for every length.
        let resident = arena
            .allocate(ChunkClass::Resident, fragment_layout(100, 1))
            .expect("a resident page");
        assert_eq!(arena.stats().pages_in_use, 5);
        assert_eq!(row_of(&arena, 0, ChunkClass::Read), None);

        // One byte under a page is the largest fragment.
        let fragment = arena
            .allocate(ChunkClass::ReadWrite, fragment_layout(PAGE_BYTES - 1, 1))
            .expect("a fragment");
        let row = row_of(&arena, 0, ChunkClass::ReadWrite).expect("the row caches a run");
        assert_eq!(row.offset, PAGE_BYTES - 1);
        drop((page, more, aligned, resident, fragment));
    }

    #[test]
    fn an_allocation_under_a_page_takes_a_page_when_no_run_is_free() {
        // One reserved chunk of two runs, which never grows for this class.
        let (arena, _) = arena(16 * PAGE_BYTES, &[ChunkClass::ReadWrite], 1);
        let (base, _) = chunk_range(&arena, 0);
        let first_run = arena
            .allocate(ChunkClass::ReadWrite, pages(FRAGMENT_RUN_PAGES))
            .expect("the first run's pages");
        let in_second_run = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("a page");
        assert_eq!(
            in_second_run.as_ptr() as usize,
            base + FRAGMENT_RUN_BYTES,
            "the page is in the second run, so neither run is free whole"
        );

        let small = arena
            .allocate(ChunkClass::ReadWrite, fragment_layout(100, 1))
            .expect("a page serves an allocation no run is free for");
        assert_eq!(small.len(), 100);
        assert_eq!(small.as_ptr() as usize % PAGE_BYTES, 0);
        assert!(small.as_ref().iter().all(|&byte| byte == 0));
        assert_eq!(row_of(&arena, 0, ChunkClass::ReadWrite), None);
        assert_eq!(arena.stats().pages_in_use, FRAGMENT_RUN_PAGES + 2);
        assert!((0..2).all(|run| run_count(&arena, 0, run) == 0));

        // Its drop frees the page, and a run that is free whole again is
        // cached by the next one.
        drop((small, in_second_run));
        assert_eq!(arena.stats().pages_in_use, FRAGMENT_RUN_PAGES);
        let fragment = arena
            .allocate(ChunkClass::ReadWrite, fragment_layout(100, 1))
            .expect("a fragment");
        let row = row_of(&arena, 0, ChunkClass::ReadWrite).expect("the row caches the run");
        assert_eq!(row.run, 1);
        assert_eq!(arena.stats().pages_in_use, 2 * FRAGMENT_RUN_PAGES);
        drop((first_run, fragment));
    }

    #[test]
    fn an_idle_cached_run_does_not_pin_its_chunk() {
        let (arena, source) = elastic(16 * PAGE_BYTES, 2, 1);
        let fragment = arena
            .allocate(ChunkClass::ReadWrite, fragment_layout(100, 1))
            .expect("a fragment");
        assert_eq!(
            block_on(arena.retire_empty(0)),
            0,
            "a chunk with a live fragment stays"
        );
        assert!(
            row_of(&arena, 0, ChunkClass::ReadWrite).is_some(),
            "a run with a fragment outstanding is not idle"
        );

        drop(fragment);
        assert_eq!(
            arena.stats().pages_in_use,
            FRAGMENT_RUN_PAGES,
            "the row still caches the run"
        );
        assert_eq!(block_on(arena.retire_empty(0)), 1);
        assert_eq!(row_of(&arena, 0, ChunkClass::ReadWrite), None);
        assert_eq!(arena.stats().chunks, 0);
        assert_eq!(source.returns(), 1);
    }

    #[test]
    fn fragments_carved_and_freed_on_other_threads_while_rows_detach_leave_every_count_exact() {
        const THREADS: usize = 4;
        const ROUNDS: usize = 4000;
        const ALIGNS: [usize; 4] = [1, 8, 64, 512];
        let (arena, source) = elastic(32 * PAGE_BYTES, 8, THREADS + 1);

        let (senders, receivers): (Vec<_>, Vec<_>) = (0..THREADS)
            .map(|_| mpsc::channel::<(ArenaBuffer, u8)>())
            .unzip();
        let all_sent = AtomicUsize::new(0);
        thread::scope(|scope| {
            for (me, receiver) in receivers.into_iter().enumerate() {
                let (arena, all_sent) = (&arena, &all_sent);
                let next = senders[(me + 1) % THREADS].clone();
                scope.spawn(move || {
                    test_processor_identity::set(ProcessorId::new(me as u16));
                    let check = |(buffer, tag): (ArenaBuffer, u8)| {
                        assert!(
                            buffer.as_ref().iter().all(|&byte| byte == tag),
                            "a fragment's bytes were written by someone else"
                        );
                    };
                    let mut random = (me as u64 + 1).wrapping_mul(NOISE_MULTIPLIER);
                    let mut sent = 0;
                    while sent < ROUNDS {
                        for received in receiver.try_iter() {
                            check(received);
                        }
                        random ^= random << 13;
                        random ^= random >> 7;
                        random ^= random << 17;
                        let bytes = 1 + (random >> 8) as usize % 3000;
                        let align = ALIGNS[(random >> 32) as usize % ALIGNS.len()];
                        let layout = fragment_layout(bytes, align);
                        match arena.allocate(ChunkClass::ReadWrite, layout) {
                            Ok(mut buffer) => {
                                assert_eq!(buffer.as_ptr() as usize % align, 0);
                                assert!(
                                    buffer.as_ref().iter().all(|&byte| byte == 0),
                                    "a fragment came back dirty"
                                );
                                let tag = 1 + (sent % 250) as u8;
                                buffer.as_mut().fill(tag);
                                if random.is_multiple_of(8) {
                                    thread::yield_now();
                                }
                                next.send((buffer, tag)).expect("the receiver is alive");
                                sent += 1;
                            }
                            Err(DmaError::Exhausted { .. } | DmaError::Replenishing { .. }) => {
                                thread::yield_now();
                            }
                            Err(other) => panic!("{other}"),
                        }
                    }
                    all_sent.fetch_add(1, Ordering::AcqRel);
                    while all_sent.load(Ordering::Acquire) < THREADS {
                        for received in receiver.try_iter() {
                            check(received);
                        }
                        thread::yield_now();
                    }
                    for received in receiver.try_iter() {
                        check(received);
                    }
                });
            }
            // The detach: every idle row, and the chunks that frees, while
            // the carvers run.
            scope.spawn(|| {
                test_processor_identity::set(ProcessorId::new(THREADS as u16));
                while all_sent.load(Ordering::Acquire) < THREADS {
                    block_on(arena.retire_empty(0));
                    thread::yield_now();
                }
            });
        });
        drop(senders);

        // Every fragment is dropped; what is left is the runs the rows
        // cache.
        block_on(arena.retire_empty(0));
        let stats = arena.stats();
        assert_eq!(stats.pages_in_use, 0);
        assert_eq!(
            bitmap_pages_in_use(&arena),
            0,
            "a run was freed twice or never"
        );
        assert_eq!(stats.chunks, 0, "a cached run still pins a chunk");
        for slot in 0..arena.counters.slots.load(Ordering::Acquire) {
            for run in 0..arena.chunk(slot).fragment_refs.len() {
                assert_eq!(run_count(&arena, slot, run), 0, "slot {slot} run {run}");
            }
        }
        for processor in 0..=THREADS {
            assert_eq!(row_of(&arena, processor, ChunkClass::ReadWrite), None);
        }
        assert_eq!(source.lends(), source.returns());
    }

    #[test]
    fn a_fragment_after_its_run_was_reused_reads_zero() {
        let (arena, _) = elastic(16 * PAGE_BYTES, 2, 1);
        let mut first = arena
            .allocate(ChunkClass::ReadWrite, fragment_layout(100, 1))
            .expect("a fragment");
        let (base, _) = chunk_range(&arena, 0);
        first.as_mut().fill(0xaa);
        let address = first.as_ptr();
        drop(first);
        // Frees the idle run and keeps the chunk.
        assert_eq!(block_on(arena.retire_empty(1)), 0);
        assert_eq!(arena.stats().pages_in_use, 0);
        // The device wrote the whole run after the free.
        // SAFETY: the source's block outlives the arena, so the chunk is
        // live host memory, and no buffer of the arena covers the run.
        unsafe {
            ptr::with_exposed_provenance_mut::<u8>(base).write_bytes(0xaa, FRAGMENT_RUN_BYTES)
        };

        let again = arena
            .allocate(ChunkClass::ReadWrite, fragment_layout(100, 1))
            .expect("a fragment of the reused run");
        assert_eq!(again.as_ptr(), address, "the run was reused");
        assert!(again.as_ref().iter().all(|&byte| byte == 0));
        drop(again);

        assert_eq!(block_on(arena.retire_empty(1)), 0);
        // SAFETY: as above.
        unsafe {
            ptr::with_exposed_provenance_mut::<u8>(base).write_bytes(0xaa, FRAGMENT_RUN_BYTES)
        };
        let overwritten = arena
            .allocate_overwritten(ChunkClass::ReadWrite, fragment_layout(100, 1))
            .expect("a fragment of the reused run");
        assert!(
            overwritten.as_ref().iter().all(|&byte| byte == 0xaa),
            "an overwritten fragment is not zeroed"
        );
    }

    #[test]
    fn dropping_an_arena_gives_back_the_run_its_row_caches() {
        let (arena, source) = arena(4 * PAGE_BYTES, &[ChunkClass::Read], 1);
        let fragment = arena
            .allocate(ChunkClass::Read, fragment_layout(1, 1))
            .expect("a fragment");
        drop(fragment);
        assert_eq!(arena.stats().pages_in_use, 4, "the run is the whole chunk");
        drop(arena);
        assert_eq!(source.returns(), 1);
    }

    #[test]
    #[should_panic(expected = "dropped with 4 live pages in 1 of its 1 chunks")]
    fn dropping_an_arena_with_a_live_fragment_panics() {
        let (arena, _) = arena(4 * PAGE_BYTES, &[ChunkClass::Read], 1);
        let _fragment = arena
            .allocate(ChunkClass::Read, fragment_layout(1, 1))
            .expect("a fragment");
        drop(arena);
    }
}
