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
//! decide anything (the bitmaps, the state words, the window table) is on
//! the kernel heap, so a device that scribbles on its own free pages
//! corrupts bytes the next [`DmaArena::allocate`] zeroes anyway.
//!
//! # Concurrency contract
//!
//! | Operation | Runs | Synchronisation |
//! |---|---|---|
//! | allocate, one bitmap word or fewer (up to 64 pages) | task context, any processor | Lock-free: a CAS on the chunk's state word to enter, then a CAS on one bitmap word. [`DmaArena::allocate_overwritten`] is the same without the zeroing pass over the run. |
//! | allocate, a run spanning bitmap words | task context, any processor | The arena's `runs` spin lock, which only these allocations take, so two of them cannot livelock. The bits are still claimed by CAS, so a concurrent single-word claim or a free composes without it. No interrupt handler reaches the lock. |
//! | free (the drop of an [`ArenaBuffer`]) | any processor, task context or interrupt handler | Wait-free: atomics of the buffer's own chunk only. No lock, no wake. |
//! | [`DmaArena::device_range`] | task context, any processor | Lock-free: an Acquire load of the window table, a hash, and a short probe. |
//! | [`DmaArena::adopt`] | task context | The arena's `grow` async mutex: the only writer of the window table and of the chunk headers. |
//!
//! No interrupt handler allocates: every handler that touches DMA memory
//! only frees it. A buffer holds a pointer to its chunk header and nothing
//! else of the arena, so its drop cannot reach a lock.
//!
//! # Cache lines
//!
//! Each chunk's state word sits on its own line. It is written by the
//! processors that allocate from the chunk (usually one, through its hint)
//! and by whichever processor frees into it. Each processor's allocation
//! hints sit on a line of their own and are written only by tasks running
//! on that processor. The chunk count is written only under `grow`.

extern crate alloc;

use alloc::boxed::Box;
use core::alloc::Layout;
use core::fmt;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::current_processor;
use helios_hal::dma::{DeviceAddress, DmaBuffer, DmaError};
use helios_hal::pmm::PhysFrame;

use crate::exec::{Mutex, MutexGuard};

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

/// Slots in a new window table.
const INITIAL_WINDOW_SLOTS: usize = 16;

/// `2^64` divided by the golden ratio: the multiplier of Fibonacci hashing
/// (Knuth, TAOCP vol. 3, 6.4), which spreads consecutive chunk numbers
/// across the table.
const FIBONACCI_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;

/// The pages in use in a chunk's state word.
const COUNT_MASK: u64 = u32::MAX as u64;

/// The chunk is being emptied for reclaim: nothing new is allocated from
/// it.
const DRAINING: u64 = 1 << 32;

/// The chunk is being retired: it is empty and nothing may enter it.
const RETIRING: u64 = 1 << 33;

/// The flags that refuse an allocation.
const REFUSING: u64 = DRAINING | RETIRING;

/// A block of memory lent to an arena.
///
/// `start` is the kernel address of the first byte, which is also the
/// block's physical address: the key the arena's window table is indexed
/// by.
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

    const fn index(self) -> usize {
        match self {
            Self::Resident => 0,
            Self::Read => 1,
            Self::ReadWrite => 2,
        }
    }
}

/// A snapshot of an arena's occupancy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArenaStats {
    /// Chunks the arena holds.
    pub chunks: usize,
    /// Pages in all of them.
    pub pages: usize,
    /// Pages handed out and not yet freed.
    pub pages_in_use: usize,
    /// Chunks with no page in use.
    pub empty_chunks: usize,
    /// Chunks recorded in the empty set. Every chunk with no page in use
    /// is in it; one that has been re-entered since may be too.
    pub noted_empty_chunks: usize,
}

/// One chunk's out-of-band record, type-stable for the life of the arena.
///
/// The state word holds the pages in use in its low 32 bits and the
/// `DRAINING` and `RETIRING` flags above them.
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
    /// The index of the chunk's [`ChunkClass`]. Written like `base`.
    class: AtomicU8,
    /// The word of the arena's empty set holding this chunk's bit.
    empty_word: NonNull<AtomicU64>,
    empty_bit: u64,
}

impl Chunk {
    fn new(pages: usize, empty_word: NonNull<AtomicU64>, empty_bit: u64) -> Self {
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
            class: AtomicU8::new(0),
            empty_word,
            empty_bit,
        }
    }

    /// Reserves `pages` of the chunk's capacity, or refuses when the chunk
    /// is draining, retiring or cannot hold them.
    fn enter(&self, pages: usize) -> bool {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            if state & REFUSING != 0 || (state & COUNT_MASK) as usize + pages > self.pages as usize
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

/// A segment of the chunk table: headers, and the bits that note which of
/// them have been empty.
struct Segment {
    chunks: Box<[Chunk]>,
    empty: Box<[AtomicU64]>,
}

impl Segment {
    fn new(headers: usize, pages_per_chunk: usize) -> Self {
        let empty: Box<[AtomicU64]> = (0..headers.div_ceil(WORD_PAGES))
            .map(|_| AtomicU64::new(0))
            .collect();
        let chunks = (0..headers)
            .map(|header| {
                Chunk::new(
                    pages_per_chunk,
                    NonNull::from(&empty[header / WORD_PAGES]),
                    1 << (header % WORD_PAGES),
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

/// One entry of the window table.
struct WindowSlot {
    /// The chunk number, or `0` for an empty slot. A chunk is aligned to
    /// its own size and is never at address zero, so its number is never
    /// `0`.
    key: AtomicUsize,
    device: AtomicU64,
}

/// One open-addressed table of chunk windows. A table is filled and then
/// replaced by a larger one; the replaced table is kept, so a reader that
/// loaded it keeps a valid view.
struct WindowTable {
    slots: Box<[WindowSlot]>,
    entries: AtomicUsize,
    /// The table this one replaced, owned so it outlives every reader.
    _previous: Option<Box<WindowTable>>,
}

impl WindowTable {
    fn new(slots: usize) -> Self {
        debug_assert!(slots.is_power_of_two());
        Self {
            slots: (0..slots)
                .map(|_| WindowSlot {
                    key: AtomicUsize::new(0),
                    device: AtomicU64::new(0),
                })
                .collect(),
            entries: AtomicUsize::new(0),
            _previous: None,
        }
    }

    fn first_probe(&self, key: usize) -> usize {
        let shift = u64::BITS - self.slots.len().ilog2();
        ((key as u64).wrapping_mul(FIBONACCI_MULTIPLIER) >> shift) as usize
    }

    /// Records a window. Only the table's single writer calls this, and
    /// the table has a free slot because it is never more than half full.
    fn place(&self, key: usize, device: u64) {
        let mask = self.slots.len() - 1;
        let mut at = self.first_probe(key);
        while self.slots[at].key.load(Ordering::Relaxed) != 0 {
            at = (at + 1) & mask;
        }
        let slot = &self.slots[at];
        slot.device.store(device, Ordering::Relaxed);
        slot.key.store(key, Ordering::Release);
        self.entries.fetch_add(1, Ordering::Relaxed);
    }

    fn lookup(&self, key: usize) -> Option<u64> {
        let mask = self.slots.len() - 1;
        let mut at = self.first_probe(key);
        loop {
            let slot = &self.slots[at];
            match slot.key.load(Ordering::Acquire) {
                found if found == key => return Some(slot.device.load(Ordering::Relaxed)),
                0 => return None,
                _ => at = (at + 1) & mask,
            }
        }
    }
}

/// Chunk number to the device's address of the chunk, with lock-free
/// readers and a single writer.
///
/// A reader loads the current table once. The chunk it looks for was
/// inserted before the buffer it holds a slice of was allocated, so any
/// table it can load holds that chunk. A doubling therefore never needs to
/// wait for readers: the doubled table is complete before it is published
/// and the old ones are kept until the arena drops. Their sizes form a
/// geometric series, so together they take at most as much as the final
/// table.
struct WindowTables {
    current: AtomicPtr<WindowTable>,
}

impl WindowTables {
    fn new() -> Self {
        Self {
            current: AtomicPtr::new(Box::into_raw(Box::new(WindowTable::new(
                INITIAL_WINDOW_SLOTS,
            )))),
        }
    }

    fn lookup(&self, key: usize) -> Option<u64> {
        // SAFETY: the pointer is always a live table: a replaced table is
        // owned by its replacement and nothing is freed before `drop`.
        unsafe { &*self.current.load(Ordering::Acquire) }.lookup(key)
    }

    /// Inserts a window, doubling the table first when it would pass half
    /// full. The `grow` guard makes the caller the only writer.
    fn insert(&self, _grow: &MutexGuard<'_, ()>, key: usize, device: u64) {
        let current = self.current.load(Ordering::Relaxed);
        // SAFETY: as in `lookup`.
        let table = unsafe { &*current };
        if (table.entries.load(Ordering::Relaxed) + 1) * 2 <= table.slots.len() {
            table.place(key, device);
            return;
        }
        let mut doubled = WindowTable::new(table.slots.len() * 2);
        for slot in table.slots.iter() {
            let key = slot.key.load(Ordering::Relaxed);
            if key != 0 {
                doubled.place(key, slot.device.load(Ordering::Relaxed));
            }
        }
        doubled.place(key, device);
        // SAFETY: `current` came from `Box::into_raw` and is replaced
        // below, so the doubled table takes over its ownership. The old
        // table does not move: it stays in its own allocation.
        doubled._previous = Some(unsafe { Box::from_raw(current) });
        self.current
            .store(Box::into_raw(Box::new(doubled)), Ordering::Release);
    }
}

impl Drop for WindowTables {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `Box::into_raw` and `&mut self`
        // excludes every reader.
        drop(unsafe { Box::from_raw(*self.current.get_mut()) });
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

/// The memory one device reaches.
pub struct DmaArena<S: ChunkSource> {
    chunk_bytes: usize,
    pages_per_chunk: usize,
    /// The chunk table. A segment is allocated and published (Release)
    /// under `grow` and freed only when the arena drops.
    segments: [AtomicPtr<Segment>; SEGMENT_COUNT],
    /// Chunks published. Written only under `grow`.
    chunks: CachePadded<AtomicUsize>,
    windows: WindowTables,
    /// Per processor and class, the slot of the chunk the last allocation
    /// there came from.
    hints: Box<[CachePadded<[AtomicU32; ChunkClass::COUNT]>]>,
    /// Serialises the allocations that span bitmap words.
    runs: spin::Mutex<()>,
    /// Serialises the writers of the chunk table and the window table.
    grow: Mutex<()>,
    source: S,
}

impl<S: ChunkSource> DmaArena<S> {
    /// An arena with no chunks, for `processors` processors, whose chunks
    /// are `chunk_bytes` each.
    ///
    /// `chunk_bytes` is a power of two and at least a page, and at least
    /// as large as the largest run the arena will be asked for.
    pub fn new(source: S, chunk_bytes: usize, processors: usize) -> Self {
        assert!(
            chunk_bytes.is_power_of_two() && chunk_bytes >= PAGE_BYTES,
            "a DMA chunk is a power of two of at least a page, not {chunk_bytes} bytes"
        );
        assert!(
            chunk_bytes / PAGE_BYTES <= u32::MAX as usize,
            "a DMA chunk of {chunk_bytes} bytes has more pages than its state word counts"
        );
        Self {
            chunk_bytes,
            pages_per_chunk: chunk_bytes / PAGE_BYTES,
            segments: core::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            chunks: CachePadded::new(AtomicUsize::new(0)),
            windows: WindowTables::new(),
            hints: (0..processors)
                .map(|_| CachePadded::new(core::array::from_fn(|_| AtomicU32::new(0))))
                .collect(),
            runs: spin::Mutex::new(()),
            grow: Mutex::new(()),
            source,
        }
    }

    /// Bytes in each chunk.
    pub fn chunk_bytes(&self) -> usize {
        self.chunk_bytes
    }

    /// Adds a chunk the device already reaches at `device`.
    ///
    /// The chunk is `chunk_bytes` long and aligned to its own size. It
    /// becomes allocatable only after its window is published, so a buffer
    /// from it always resolves in [`DmaArena::device_range`].
    pub async fn adopt(&self, chunk: LentChunk, device: DeviceAddress, class: ChunkClass) {
        let grow = self.grow.lock().await;
        assert_eq!(
            chunk.bytes, self.chunk_bytes,
            "a chunk of {} bytes was adopted by an arena of {}-byte chunks",
            chunk.bytes, self.chunk_bytes
        );
        assert!(
            chunk.start != 0 && chunk.start.is_multiple_of(self.chunk_bytes),
            "a chunk at {:#x} is not aligned to its {} bytes",
            chunk.start,
            self.chunk_bytes
        );
        let key = chunk.start / self.chunk_bytes;
        assert!(
            self.windows.lookup(key).is_none(),
            "the chunk at {:#x} was adopted twice",
            chunk.start
        );
        let slot = self.chunks.load(Ordering::Relaxed);
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
            )));
            self.segments[index].store(segment, Ordering::Release);
        }
        // SAFETY: the segment is live until the arena drops.
        let header = &unsafe { &*segment }.chunks[position];
        header.base.store(chunk.start, Ordering::Relaxed);
        header.device.store(device.get(), Ordering::Relaxed);
        header.class.store(class.index() as u8, Ordering::Relaxed);
        self.windows.insert(&grow, key, device.get());
        header.note_empty();
        self.chunks.store(slot + 1, Ordering::Release);
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
        let align_pages = layout.align().div_ceil(PAGE_BYTES);
        let hint = self.hint(class);
        let first = hint.load(Ordering::Relaxed) as usize;
        let chunks = self.chunks.load(Ordering::Acquire);
        for step in 0..chunks {
            let slot = (first + step) % chunks;
            let chunk = self.chunk(slot);
            if usize::from(chunk.class.load(Ordering::Relaxed)) != class.index() {
                continue;
            }
            let Some(first_page) = self.claim(chunk, pages, align_pages) else {
                continue;
            };
            if slot != first {
                hint.store(slot as u32, Ordering::Relaxed);
            }
            return Ok(self.buffer(chunk, first_page, pages, bytes, fill));
        }
        Err(DmaError::Exhausted { bytes })
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
        let Some(device) = self.windows.lookup(address / self.chunk_bytes) else {
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
        let chunks = self.chunks.load(Ordering::Acquire);
        let mut stats = ArenaStats {
            chunks,
            pages: chunks * self.pages_per_chunk,
            pages_in_use: 0,
            empty_chunks: 0,
            noted_empty_chunks: 0,
        };
        for slot in 0..chunks {
            let in_use = self.chunk(slot).pages_in_use();
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

    /// Claims `pages` in `chunk`: enters it, then takes the bits. A
    /// refusal leaves the chunk as it was.
    fn claim(&self, chunk: &Chunk, pages: usize, align_pages: usize) -> Option<usize> {
        if !chunk.enter(pages) {
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

    fn buffer(
        &self,
        chunk: &Chunk,
        first_page: usize,
        pages: usize,
        bytes: usize,
        fill: Fill,
    ) -> ArenaBuffer {
        let offset = first_page * PAGE_BYTES;
        let base = chunk.base.load(Ordering::Relaxed);
        let ptr = ptr::with_exposed_provenance_mut::<u8>(base + offset);
        if fill == Fill::Zeroed {
            // SAFETY: the bitmap gave this task the run, which lies inside
            // the chunk the source lent, so nothing else holds a reference
            // to it. The device may write it, which a byte write
            // tolerates.
            unsafe { ptr.write_bytes(0, pages * PAGE_BYTES) };
        }
        ArenaBuffer {
            ptr: NonNull::new(ptr).expect("a chunk's pages are never at address zero"),
            len: bytes,
            device: DeviceAddress::new(chunk.device.load(Ordering::Relaxed) + offset as u64),
            chunk: NonNull::from(chunk),
            first_page: first_page as u32,
            pages: pages as u32,
        }
    }
}

impl<S: ChunkSource> Drop for DmaArena<S> {
    /// Gives every chunk back to the source.
    ///
    /// A page still live means a driver outlived its device's memory, and
    /// the device may be writing it: the arena panics. It leaks its chunk
    /// table first, so the buffers that outlive the panic still free into
    /// valid headers, and it gives no chunk back.
    fn drop(&mut self) {
        let chunks = *self.chunks.get_mut();
        let mut live_pages = 0;
        let mut live_chunks = 0;
        for slot in 0..chunks {
            let in_use = self.chunk(slot).pages_in_use();
            live_pages += in_use;
            live_chunks += usize::from(in_use != 0);
        }
        assert!(
            live_pages == 0,
            "a DMA arena was dropped with {live_pages} live pages in {live_chunks} of its \
             {chunks} chunks"
        );
        for slot in 0..chunks {
            let base = self.chunk(slot).base.load(Ordering::Relaxed);
            self.source
                .take_back(LentChunk::new(base, self.chunk_bytes));
        }
        for segment in &mut self.segments {
            let segment = *segment.get_mut();
            if !segment.is_null() {
                // SAFETY: the pointer came from `Box::into_raw` in
                // `adopt`, and no page is live, so no header is read
                // again.
                drop(unsafe { Box::from_raw(segment) });
            }
        }
    }
}

/// Pages a driver holds. Dropping it frees them.
///
/// The drop reaches only the atomics of the buffer's chunk, so it is
/// wait-free and safe from an interrupt handler on any processor.
pub struct ArenaBuffer {
    ptr: NonNull<u8>,
    len: usize,
    device: DeviceAddress,
    chunk: NonNull<Chunk>,
    first_page: u32,
    pages: u32,
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
        chunk.release(self.first_page as usize, self.pages as usize);
        chunk.leave(self.pages as usize);
    }
}

#[cfg(test)]
mod tests {
    use alloc::alloc::{alloc_zeroed, dealloc};
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::sync::atomic::AtomicBool;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::mpsc;
    use std::thread;

    use futures_lite::future::block_on;
    use helios_hal::cpu::ProcessorId;

    use super::*;
    use crate::test_processor_identity;

    #[derive(Default)]
    struct ChunkCounts {
        lent: AtomicUsize,
        returned: AtomicUsize,
    }

    /// Aligned host blocks, each followed by a guard region the arena never
    /// owns, so a test can build a slice that runs past a chunk's end
    /// without leaving allocated memory.
    struct HeapChunks {
        counts: Arc<ChunkCounts>,
    }

    impl HeapChunks {
        fn layout(bytes: usize) -> Layout {
            Layout::from_size_align(bytes * 2, bytes).expect("chunk layout")
        }
    }

    impl ChunkSource for HeapChunks {
        fn lend(&self, bytes: usize) -> Option<LentChunk> {
            // SAFETY: the layout is not zero-sized.
            let ptr = unsafe { alloc_zeroed(Self::layout(bytes)) };
            if ptr.is_null() {
                return None;
            }
            self.counts.lent.fetch_add(1, Ordering::Relaxed);
            Some(LentChunk::new(ptr.expose_provenance(), bytes))
        }

        fn take_back(&self, chunk: LentChunk) {
            self.counts.returned.fetch_add(1, Ordering::Relaxed);
            // SAFETY: `lend` allocated this block with this layout.
            unsafe {
                dealloc(
                    ptr::with_exposed_provenance_mut(chunk.start()),
                    Self::layout(chunk.bytes()),
                )
            };
        }
    }

    /// The device's address of the chunk in slot `slot`: not the host
    /// address, and not contiguous with its neighbours.
    fn device_of(slot: usize, chunk_bytes: usize) -> u64 {
        0x8000_0000_0000 + (slot * chunk_bytes * 4) as u64
    }

    /// An arena of one chunk per entry of `classes`.
    fn arena(
        chunk_bytes: usize,
        classes: &[ChunkClass],
        processors: usize,
    ) -> (DmaArena<HeapChunks>, Arc<ChunkCounts>) {
        let counts = Arc::new(ChunkCounts::default());
        let source = HeapChunks {
            counts: counts.clone(),
        };
        let arena = DmaArena::new(
            HeapChunks {
                counts: counts.clone(),
            },
            chunk_bytes,
            processors,
        );
        for (slot, &class) in classes.iter().enumerate() {
            let chunk = source.lend(chunk_bytes).expect("host memory");
            block_on(arena.adopt(
                chunk,
                DeviceAddress::new(device_of(slot, chunk_bytes)),
                class,
            ));
        }
        (arena, counts)
    }

    fn pages(count: usize) -> Layout {
        Layout::from_size_align(count * PAGE_BYTES, 1).expect("layout")
    }

    /// The bits set in every bitmap, less the padding past each chunk's
    /// last page.
    fn bitmap_pages_in_use<S: ChunkSource>(arena: &DmaArena<S>) -> usize {
        (0..arena.stats().chunks)
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

    fn chunk_range<S: ChunkSource>(arena: &DmaArena<S>, slot: usize) -> (usize, usize) {
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
    fn a_draining_chunk_takes_no_new_page() {
        let (arena, _) = arena(4 * PAGE_BYTES, &[ChunkClass::ReadWrite], 1);
        arena.chunk(0).state.fetch_or(DRAINING, Ordering::Relaxed);
        assert_eq!(
            arena.allocate(ChunkClass::ReadWrite, pages(1)).unwrap_err(),
            DmaError::Exhausted { bytes: PAGE_BYTES }
        );
        assert_eq!(arena.stats().pages_in_use, 0, "a refusal leaves no count");
        arena.chunk(0).state.fetch_and(!DRAINING, Ordering::Relaxed);
        arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("the chunk serves again");
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
            device_of(0, chunk_bytes) + offset as u64
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
                device_of(0, chunk_bytes) + (chunk_bytes - PAGE_BYTES) as u64
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
        let (arena, counts) = arena(chunk_bytes, &[ChunkClass::ReadWrite; CHUNKS], THREADS);

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
                    let mut random = (me as u64 + 1).wrapping_mul(FIBONACCI_MULTIPLIER);
                    let mut sent = 0;
                    while sent < ROUNDS {
                        for received in receiver.try_iter() {
                            check(received);
                        }
                        random ^= random << 13;
                        random ^= random >> 7;
                        random ^= random << 17;
                        let run = if random.is_multiple_of(16) {
                            70
                        } else {
                            1 + (random >> 8) as usize % 9
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
            assert_eq!(arena.chunk(slot).state.load(Ordering::Acquire), 0);
        }
        drop(arena);
        assert_eq!(counts.returned.load(Ordering::Relaxed), CHUNKS);
    }

    #[test]
    fn a_reader_racing_a_doubling_never_misses_a_live_chunk() {
        const LIVE: usize = 8;
        const TOTAL: usize = 4096;
        let device = |key: usize| 0x1000 * key as u64;
        let windows = WindowTables::new();
        let grow = Mutex::new(());
        let grow = block_on(grow.lock());
        for key in 1..=LIVE {
            windows.insert(&grow, key, device(key));
        }

        let stop = AtomicBool::new(false);
        let passes = AtomicUsize::new(0);
        thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    loop {
                        for key in 1..=LIVE {
                            assert_eq!(
                                windows.lookup(key),
                                Some(device(key)),
                                "a live chunk went missing during a doubling"
                            );
                        }
                        passes.fetch_add(1, Ordering::Relaxed);
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                });
            }
            // The inserts double the table nine times, from 16 slots to
            // 8192. The writer waits for a reader's pass after each batch:
            // a scheduler that starts the readers late would otherwise let
            // every doubling finish before the first lookup.
            let keys: Vec<usize> = (LIVE + 1..=TOTAL).collect();
            for batch in keys.chunks(64) {
                let seen = passes.load(Ordering::Relaxed);
                for &key in batch {
                    windows.insert(&grow, key, device(key));
                }
                while passes.load(Ordering::Relaxed) == seen {
                    thread::yield_now();
                }
            }
            stop.store(true, Ordering::Relaxed);
        });
        for key in 1..=TOTAL {
            assert_eq!(windows.lookup(key), Some(device(key)));
        }
        assert_eq!(windows.lookup(TOTAL + 1), None);
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
        let (arena, counts) = arena(4 * PAGE_BYTES, &[ChunkClass::ReadWrite], 1);
        let mut page = arena
            .allocate(ChunkClass::ReadWrite, pages(1))
            .expect("a page");
        assert!(catch_unwind(AssertUnwindSafe(move || drop(arena))).is_err());
        assert_eq!(
            counts.returned.load(Ordering::Relaxed),
            0,
            "a chunk with a live page is not given back"
        );
        page.as_mut().fill(1);
        drop(page);
    }

    #[test]
    fn dropping_an_idle_arena_gives_every_chunk_back() {
        let (arena, counts) = arena(4 * PAGE_BYTES, &[ChunkClass::Read; 20], 1);
        assert_eq!(
            arena.stats().chunks,
            20,
            "the chunk table grew past a segment"
        );
        let page = arena.allocate(ChunkClass::Read, pages(2)).expect("a page");
        drop(page);
        drop(arena);
        assert_eq!(counts.lent.load(Ordering::Relaxed), 20);
        assert_eq!(counts.returned.load(Ordering::Relaxed), 20);
    }
}
