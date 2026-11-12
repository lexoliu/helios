//! The memory a kernel block request carries: whole pages, owned by the
//! request until the device answers.
//!
//! Every block device the kernel drives carries [`BlockPages`], so one
//! device type serves the block cache and the scratch disk alike. The
//! pages of a request are either the request's own — a scratch buffer
//! its holder fills or reads, a copy the cache staged for a write — or
//! the block cache's entries, which the request holds through the
//! entries' state words: locked for a read, in write-back for a write.
//! A cache request also carries its [`CacheRun`], which the driver's
//! completion path completes, so the entries are finished by the answer
//! itself rather than by the task that submitted it.
//!
//! Concurrency contract: a buffer has one owner at a time — its builder,
//! then the driver, then the waiter or nobody — and is never shared. Its
//! completion and its drop may run in the device's interrupt handler, so
//! neither blocks nor takes a lock an interrupt handler cannot take: the
//! cache's half is the run's lock-free state transitions, and freeing an
//! owned page goes to the kernel heap, whose lock is interrupt-safe.

extern crate alloc;

use alloc::boxed::Box;
use arrayvec::ArrayVec;
use core::ptr::NonNull;

use helios_hal::fs::{AnswerFlag, BlockBuffer, OwnedBlockBuffer};
use helios_hal::io::IoResult;

use super::cache::{CACHE_BLOCK_BYTES, CacheRun, MAX_RUN_BLOCKS};

/// A page buffer, aligned to its size so that it never straddles a page
/// and a device can be handed it as one segment.
#[repr(C, align(4096))]
pub(super) struct Page(pub(super) [u8; CACHE_BLOCK_BYTES]);

const _: () = assert!(
    core::mem::size_of::<Page>() == CACHE_BLOCK_BYTES
        && core::mem::align_of::<Page>() == CACHE_BLOCK_BYTES,
    "a block buffer page is one page, aligned to a page"
);

const _: () = assert!(
    MAX_RUN_BLOCKS <= u32::BITS as usize,
    "a request's owned pages are one bit each in a u32"
);

impl Page {
    /// A zeroed page from the kernel heap.
    pub(super) fn zeroed() -> Box<Self> {
        // SAFETY: all zeroes is a valid `[u8; N]`, the page's only field.
        unsafe { Box::<Self>::new_zeroed().assume_init() }
    }
}

/// The kernel's [`BlockBuffer`]: up to [`MAX_RUN_BLOCKS`] pages, laid end
/// to end.
pub struct BlockPages {
    pages: ArrayVec<NonNull<Page>, MAX_RUN_BLOCKS>,
    /// Bit `i` set: page `i` is this buffer's own, freed with it.
    owned: u32,
    /// Bytes the request transfers; every page but the last is whole.
    len: usize,
    /// The cache blocks the request fills or writes, when the block cache
    /// issued it.
    run: Option<CacheRun>,
    /// Raised once the request is over, when the holder asked to learn
    /// it (see [`OwnedBlockBuffer::raise_on_answer`]).
    answered: Option<AnswerFlag>,
}

// SAFETY: the pages are either the buffer's own or cache pages the run
// holds through their entries' state words, which no other owner touches
// while the buffer lives; nothing in it is tied to a thread.
unsafe impl Send for BlockPages {}

impl BlockPages {
    /// A request buffer for a cache run, whose pages the cache pushes
    /// next.
    pub(super) fn for_run(run: CacheRun) -> Self {
        Self {
            pages: ArrayVec::new(),
            owned: 0,
            len: 0,
            run: Some(run),
            answered: None,
        }
    }

    /// Appends a page the request borrows from a cache entry the run
    /// holds.
    pub(super) fn push_borrowed(&mut self, page: NonNull<Page>) {
        self.pages.push(page);
        self.len += CACHE_BLOCK_BYTES;
    }

    /// Appends a page the request owns.
    pub(super) fn push_owned(&mut self, page: Box<Page>) {
        self.owned |= 1 << self.pages.len();
        self.pages.push(NonNull::from(Box::leak(page)));
        self.len += CACHE_BLOCK_BYTES;
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

    /// The page at `index`, which this buffer owns.
    fn own_page(&self, index: usize) -> NonNull<Page> {
        assert!(
            self.run.is_none() && self.owned & (1 << index) != 0,
            "only a buffer's own pages are filled or read by its holder"
        );
        self.pages[index]
    }
}

impl Drop for BlockPages {
    fn drop(&mut self) {
        // The run first: it gives its entries back, as answered or as
        // never seen by the device, and stops counting staged pages.
        drop(self.run.take());
        for (index, page) in self.pages.drain(..).enumerate() {
            if self.owned & (1 << index) != 0 {
                // SAFETY: an owned page came from `Box::leak` in
                // `push_owned` or `zeroed`, and is freed once, here.
                drop(unsafe { Box::from_raw(page.as_ptr()) });
            }
        }
    }
}

impl BlockBuffer for BlockPages {
    fn segments(&self) -> impl Iterator<Item = NonNull<[u8]>> + '_ {
        let len = self.len;
        self.pages.iter().enumerate().map(move |(index, page)| {
            let start = index * CACHE_BLOCK_BYTES;
            NonNull::slice_from_raw_parts(page.cast::<u8>(), (len - start).min(CACHE_BLOCK_BYTES))
        })
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

    fn zeroed(len: usize) -> Option<Self> {
        let count = len.div_ceil(CACHE_BLOCK_BYTES);
        if count > MAX_RUN_BLOCKS {
            return None;
        }
        let mut buffer = Self {
            pages: ArrayVec::new(),
            owned: 0,
            len: 0,
            run: None,
            answered: None,
        };
        for _ in 0..count {
            buffer.push_owned(Page::zeroed());
        }
        buffer.len = len;
        Some(buffer)
    }

    fn len(&self) -> usize {
        self.len
    }

    fn copy_from(&mut self, src: &[u8]) {
        assert!(
            src.len() <= self.len,
            "{} bytes do not fit a {}-byte block buffer",
            src.len(),
            self.len
        );
        for (index, chunk) in src.chunks(CACHE_BLOCK_BYTES).enumerate() {
            let mut page = self.own_page(index);
            // SAFETY: the page is this buffer's own and no request holds
            // the buffer, so nothing else reads or writes it.
            let page = unsafe { page.as_mut() };
            page.0[..chunk.len()].copy_from_slice(chunk);
        }
    }

    fn copy_to(&self, dst: &mut [u8]) {
        assert!(
            dst.len() <= self.len,
            "a {}-byte block buffer cannot fill {} bytes",
            self.len,
            dst.len()
        );
        for (index, chunk) in dst.chunks_mut(CACHE_BLOCK_BYTES).enumerate() {
            let page = self.own_page(index);
            // SAFETY: as in `copy_from`; the page is only read.
            let page = unsafe { page.as_ref() };
            chunk.copy_from_slice(&page.0[..chunk.len()]);
        }
    }
}
