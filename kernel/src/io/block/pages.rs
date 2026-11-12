//! The memory a kernel block request carries: whole pages of the kernel
//! heap, owned by the request until the device answers.
//!
//! Every block device the kernel drives carries [`BlockPages`], so one
//! device type serves the block cache and the scratch disk alike. The
//! bytes of a request are kernel memory, never memory a device reaches:
//! the driver copies them to and from memory of its own
//! ([`BlockBuffer::fill`] and [`BlockBuffer::deliver`]), so the device
//! never sees the buffer itself. A request's bytes are either the
//! request's own — a scratch or swap buffer its holder fills or reads — or
//! the block cache's entries, which the request holds through the
//! entries' state words: locked for a read, in write-back for a write. A
//! block of a write is one of two sources: the page its entry holds, which
//! `fill` copies, or the compressed page its entry holds, which `fill`
//! decompresses straight into the run the driver hands it, so a write-back
//! allocates no page whatever the memory pressure. A cache request also
//! carries its [`CacheRun`], which the driver's completion path completes,
//! so the entries are finished by the answer itself rather than by the task
//! that submitted it.
//!
//! Concurrency contract: a buffer has one owner at a time — its builder,
//! then the driver, then the waiter or nobody — and is never shared while
//! a request carries it. `fill` and `deliver` only copy: `fill` runs in
//! the driver's submit path, in task context on whichever processor
//! submits, which need not be the one that started the write; `deliver`
//! runs in the driver's completion path, which is the device's interrupt
//! handler or the task that drained the answer, and a page a read fills
//! is locked by its run until `complete` publishes it. A source `fill`
//! reads stays alive and unwritten from the moment the write starts until
//! the answer: the run holds the entry's `WRITEBACK` bit, a share and a
//! pin, and no path frees, replaces or writes a slot in that state (see
//! the cache's `start_writeback`). `fill` allocates no page and takes no
//! lock an interrupt handler can reach: decompression uses the executing
//! processor's workspace in the compressed pool, behind a spin lock that
//! only tasks on that processor take. The decoder in that workspace is
//! built on the processor's first decompression and keeps its tables, so
//! that is the one kernel-heap allocation a write-back can still make.
//! The completion and the drop of a
//! buffer may run in the device's interrupt handler, so neither blocks nor
//! takes a lock an interrupt handler cannot take: the cache's half is the
//! run's lock-free state transitions.

extern crate alloc;

use alloc::boxed::Box;
use arrayvec::ArrayVec;
use core::ptr::NonNull;

use helios_hal::fs::{AnswerFlag, BlockBuffer, OwnedBlockBuffer};
use helios_hal::io::IoResult;

use super::cache::{CACHE_BLOCK_BYTES, CacheRun, MAX_RUN_BLOCKS};
use crate::memory::CompressedPage;

/// A page buffer, aligned to its size so that it never straddles a page.
#[repr(C, align(4096))]
pub(super) struct Page(pub(super) [u8; CACHE_BLOCK_BYTES]);

const _: () = assert!(
    core::mem::size_of::<Page>() == CACHE_BLOCK_BYTES
        && core::mem::align_of::<Page>() == CACHE_BLOCK_BYTES,
    "a block buffer page is one page, aligned to a page"
);

/// Where one block of a cache request's bytes lives.
#[derive(Clone, Copy)]
enum Source {
    /// The page buffer of a cache entry the run holds.
    Page(NonNull<Page>),
    /// The compressed page of a cache entry the run holds in write-back:
    /// a write's `fill` decompresses it into the driver's run.
    Compressed(NonNull<CompressedPage>),
}

impl Page {
    /// A zeroed page from the kernel heap.
    pub(super) fn zeroed() -> Box<Self> {
        // SAFETY: all zeroes is a valid `[u8; N]`, the page's only field.
        unsafe { Box::<Self>::new_zeroed().assume_init() }
    }
}

/// The kernel's [`BlockBuffer`]: either up to [`MAX_RUN_BLOCKS`] cache
/// blocks of a cache request, laid end to end, or one run of heap bytes the
/// buffer owns.
pub struct BlockPages {
    /// The blocks a cache request transfers, in block order.
    pages: ArrayVec<Source, MAX_RUN_BLOCKS>,
    /// The bytes of a buffer built by [`OwnedBlockBuffer::zeroed`]; `None`
    /// for a cache request.
    own: Option<Box<[u8]>>,
    /// The cache blocks the request fills or writes, when the block cache
    /// issued it.
    run: Option<CacheRun>,
    /// Raised once the request is over, when the holder asked to learn
    /// it (see [`OwnedBlockBuffer::raise_on_answer`]).
    answered: Option<AnswerFlag>,
}

// SAFETY: the sources are cache slots the run holds through their
// entries' state words, which no other owner touches while the buffer
// lives; nothing in it is tied to a thread.
unsafe impl Send for BlockPages {}
// SAFETY: a shared buffer only reads: its byte view reads memory it owns,
// and its sources are addresses, not accesses.
unsafe impl Sync for BlockPages {}

impl BlockPages {
    /// A request buffer for a cache run, whose pages the cache pushes
    /// next.
    pub(super) fn for_run(run: CacheRun) -> Self {
        Self {
            pages: ArrayVec::new(),
            own: None,
            run: Some(run),
            answered: None,
        }
    }

    /// Appends a page the request borrows from a cache entry the run
    /// holds.
    pub(super) fn push_borrowed(&mut self, page: NonNull<Page>) {
        self.pages.push(Source::Page(page));
    }

    /// Appends the compressed page of a cache entry the run holds in
    /// write-back, for `fill` to decompress.
    pub(super) fn push_compressed(&mut self, page: NonNull<CompressedPage>) {
        self.pages.push(Source::Compressed(page));
    }

    /// The cache blocks a cache request names.
    fn run(&self) -> &CacheRun {
        self.run
            .as_ref()
            .expect("a block request built by the cache carries its run")
    }

    /// The disk, first cache block and block count of a cache request.
    pub(super) fn run_target(&self) -> (usize, u64, usize) {
        let run = self.run();
        (run.disk(), run.first_block(), run.blocks())
    }

    pub(super) fn run_blocks(&self) -> usize {
        self.run().blocks()
    }

    pub(super) fn run_id(&self, index: usize) -> u32 {
        self.run().id(index)
    }

    /// The bytes of a buffer its holder fills or reads.
    fn own_bytes(&self) -> &[u8] {
        self.own
            .as_deref()
            .expect("only a buffer made of its own bytes is filled or read by its holder")
    }
}

impl BlockBuffer for BlockPages {
    fn len(&self) -> usize {
        self.own
            .as_deref()
            .map_or(self.pages.len() * CACHE_BLOCK_BYTES, <[u8]>::len)
    }

    fn fill(&mut self, to: &mut [u8]) {
        debug_assert_eq!(to.len(), self.len(), "a write is filled whole");
        if let Some(own) = &self.own {
            to.copy_from_slice(own);
            return;
        }
        let (blocks, _) = to.as_chunks_mut::<CACHE_BLOCK_BYTES>();
        for (source, to) in self.pages.iter().zip(blocks) {
            match *source {
                // SAFETY: the page is a cache entry's the run holds in
                // write-back, so nothing writes or frees it while the
                // request carries it.
                Source::Page(page) => to.copy_from_slice(&unsafe { page.as_ref() }.0),
                // SAFETY: the compressed page is a cache entry's the run
                // holds in write-back, which no path frees or replaces
                // until the write is answered.
                Source::Compressed(page) => unsafe { page.as_ref() }.decompress_into(to),
            }
        }
    }

    fn deliver(&mut self, from: &[u8]) {
        debug_assert_eq!(from.len(), self.len(), "a read is delivered whole");
        if let Some(own) = &mut self.own {
            own.copy_from_slice(from);
            return;
        }
        let (blocks, _) = from.as_chunks::<CACHE_BLOCK_BYTES>();
        for (source, from) in self.pages.iter_mut().zip(blocks) {
            let Source::Page(page) = source else {
                panic!("a read request names a compressed block")
            };
            // SAFETY: the page is a cache entry's the run holds locked for
            // this read, which no reader sees before `complete` publishes
            // it, so this is the only access.
            unsafe { page.as_mut() }.0.copy_from_slice(from);
        }
    }

    fn complete(&mut self, result: IoResult<()>) {
        if let Some(run) = self.run.as_mut() {
            run.complete(result);
        }
        if let Some(answered) = self.answered.take() {
            answered.raise();
        }
    }
}

impl OwnedBlockBuffer for BlockPages {
    fn raise_on_answer(&mut self, flag: AnswerFlag) {
        self.answered = Some(flag);
    }

    fn zeroed(len: usize) -> Self {
        Self {
            pages: ArrayVec::new(),
            own: Some(alloc::vec![0; len].into_boxed_slice()),
            run: None,
            answered: None,
        }
    }
}

impl AsRef<[u8]> for BlockPages {
    fn as_ref(&self) -> &[u8] {
        self.own_bytes()
    }
}

impl AsMut<[u8]> for BlockPages {
    fn as_mut(&mut self) -> &mut [u8] {
        self.own
            .as_deref_mut()
            .expect("only a buffer made of its own bytes is filled or read by its holder")
    }
}
