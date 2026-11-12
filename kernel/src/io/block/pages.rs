//! The memory a kernel block request carries: pages of the disk's own
//! device memory, owned by the request until the device answers.
//!
//! Every block device the kernel drives carries [`BlockPages`], so one
//! device type serves the block cache and the scratch disk alike. Every
//! byte a request carries is memory its disk allocated
//! ([`BlockDevice::memory`]), never the kernel heap; the driver copies it
//! to and from memory of its own for the device ([`BlockBuffer::fill`] and
//! [`BlockBuffer::deliver`]), so the device never reaches the buffer
//! itself. The memory of a request is either the request's own — a
//! scratch or swap buffer its holder fills or reads, a page the cache
//! staged for a write — or the block cache's entries, whose pages the
//! cache allocated from the same disk and which the request holds through
//! the entries' state words: locked for a read, in write-back for a
//! write. A cache request also carries its [`CacheRun`], which the
//! driver's completion path completes, so the entries are finished by the
//! answer itself rather than by the task that submitted it.
//!
//! Concurrency contract: a buffer has one owner at a time — its builder,
//! then the driver, then the waiter or nobody — and is never shared while
//! a request carries it. Its completion and its drop may run in the
//! device's interrupt handler, so neither blocks nor takes a lock an
//! interrupt handler cannot take: the cache's half is the run's lock-free
//! state transitions, and the memory it owns is freed through the
//! device's [`BlockDevice::Memory`], whose free path the trait requires to
//! be one an interrupt handler may take. Today every backend's pool is
//! heap-backed, so that free is a `dealloc` into the kernel heap, which
//! takes the heap's `IrqSafeMutex` (`KernelAllocator::heap` in the
//! kernel's `lib.rs`) with the local processor's interrupts masked.

extern crate alloc;

use alloc::vec::Vec;
use arrayvec::ArrayVec;
use core::ptr::NonNull;

use helios_hal::dma::DmaBuffer;
use helios_hal::fs::{AnswerFlag, BlockBuffer, BlockDevice, OwnedBlockBuffer};
use helios_hal::io::IoResult;

use super::cache::{CACHE_BLOCK_BYTES, CacheRun, MAX_RUN_BLOCKS};

/// The request buffer of a block device the kernel drives: [`BlockPages`]
/// made of that device's own memory.
pub type DevicePages<Device> = BlockPages<<Device as BlockDevice>::Memory>;

/// The kernel's [`BlockBuffer`]: either up to [`MAX_RUN_BLOCKS`] cache
/// pages of a cache request, laid end to end, or one run of device memory
/// the buffer owns.
pub struct BlockPages<Memory> {
    /// The first byte of each whole page a cache request transfers, in
    /// block order: the entries' own pages and the ones it staged.
    pages: ArrayVec<NonNull<u8>, MAX_RUN_BLOCKS>,
    /// Pages the cache staged for this write, which the request owns and
    /// frees with it. On the heap because only a write of compressed
    /// blocks stages any, and room for a whole run inline would put
    /// [`MAX_RUN_BLOCKS`] device buffers in every in-flight request slot
    /// of the driver.
    staged: Vec<Memory>,
    /// The memory of a buffer built by [`OwnedBlockBuffer::owning`], its
    /// one segment; `None` for a cache request.
    owned: Option<Memory>,
    /// The cache blocks the request fills or writes, when the block cache
    /// issued it.
    run: Option<CacheRun<Memory>>,
    /// Raised once the request is over, when the holder asked to learn
    /// it (see [`OwnedBlockBuffer::raise_on_answer`]).
    answered: Option<AnswerFlag>,
}

// SAFETY: the pages are the buffer's own memory or cache pages the run
// holds through their entries' state words, which no other owner touches
// while the buffer lives; nothing in it is tied to a thread.
unsafe impl<Memory: Send> Send for BlockPages<Memory> {}
// SAFETY: a shared buffer only reads: its byte view reads memory it owns,
// and its pages are addresses, not accesses.
unsafe impl<Memory: Sync> Sync for BlockPages<Memory> {}

/// The first byte of `memory`, taken from the buffer's own pointer so a
/// device write through it is not a write through a shared borrow.
fn start(memory: &impl DmaBuffer) -> NonNull<u8> {
    NonNull::new(memory.as_ptr()).expect("a device memory buffer names memory")
}

impl<Memory: DmaBuffer> BlockPages<Memory> {
    /// A request buffer for a cache run, whose pages the cache pushes
    /// next.
    pub(super) fn for_run(run: CacheRun<Memory>, staged: usize) -> Self {
        Self {
            pages: ArrayVec::new(),
            staged: Vec::with_capacity(staged),
            owned: None,
            run: Some(run),
            answered: None,
        }
    }

    /// Appends a page the request borrows from a cache entry the run
    /// holds.
    pub(super) fn push_borrowed(&mut self, page: &Memory) {
        self.pages.push(start(page));
    }

    /// Appends a page the cache staged for this write, which the request
    /// owns from here.
    pub(super) fn push_staged(&mut self, page: Memory) {
        self.pages.push(start(&page));
        self.staged.push(page);
    }

    /// The cache blocks a cache request names.
    fn run(&self) -> &CacheRun<Memory> {
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

    /// The memory of a buffer its holder fills or reads.
    fn own_memory(&self) -> &Memory {
        self.owned
            .as_ref()
            .expect("only a buffer made of its own memory is filled or read by its holder")
    }
}

impl<Memory> Drop for BlockPages<Memory> {
    fn drop(&mut self) {
        // The run first: it gives its entries back, as answered or as
        // never seen by the device, and stops counting staged pages. The
        // memory the buffer owns is freed after it, with the fields.
        drop(self.run.take());
    }
}

/// The request's memory laid end to end: the buffer's own run, then the
/// cache pages of a cache request in block order.
///
/// Concurrency contract: the buffer has one owner at a time and no request
/// holds it while `fill` or `deliver` run, so neither synchronises with
/// anything: `fill` runs in the submitting task before the device is given
/// the request, `deliver` in the driver's completion path, which is the
/// device's interrupt handler or the task that drained the answer. Both
/// are copies into or out of memory the request owns or holds through its
/// entries' state words, and take no lock and allocate nothing.
impl<Memory: DmaBuffer + AsRef<[u8]> + AsMut<[u8]>> BlockBuffer for BlockPages<Memory> {
    fn len(&self) -> usize {
        self.owned.as_ref().map_or(0, DmaBuffer::len) + self.pages.len() * CACHE_BLOCK_BYTES
    }

    fn fill(&mut self, to: &mut [u8]) {
        debug_assert_eq!(to.len(), self.len(), "a write is filled whole");
        let (own, pages) = to.split_at_mut(self.owned.as_ref().map_or(0, DmaBuffer::len));
        if let Some(memory) = &self.owned {
            own.copy_from_slice(memory.as_ref());
        }
        let (blocks, _) = pages.as_chunks_mut::<CACHE_BLOCK_BYTES>();
        for (page, to) in self.pages.iter().zip(blocks) {
            // SAFETY: the page is a cache entry's the run holds, or one
            // the request staged, so nothing writes it while the request
            // carries it, and it is a whole cache block.
            to.copy_from_slice(unsafe {
                core::slice::from_raw_parts(page.as_ptr(), CACHE_BLOCK_BYTES)
            });
        }
    }

    fn deliver(&mut self, from: &[u8]) {
        debug_assert_eq!(from.len(), self.len(), "a read is delivered whole");
        let (own, pages) = from.split_at(self.owned.as_ref().map_or(0, DmaBuffer::len));
        if let Some(memory) = &mut self.owned {
            memory.as_mut().copy_from_slice(own);
        }
        let (blocks, _) = pages.as_chunks::<CACHE_BLOCK_BYTES>();
        for (page, from) in self.pages.iter().zip(blocks) {
            // SAFETY: the page is a cache entry's the run holds locked for
            // this read, which no reader sees before `complete` publishes
            // it, so this is the only access; it is a whole cache block.
            unsafe {
                core::slice::from_raw_parts_mut(page.as_ptr(), CACHE_BLOCK_BYTES)
                    .copy_from_slice(from);
            }
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

impl<Memory> OwnedBlockBuffer for BlockPages<Memory>
where
    Memory: DmaBuffer + AsRef<[u8]> + AsMut<[u8]>,
{
    type Memory = Memory;

    fn raise_on_answer(&mut self, flag: AnswerFlag) {
        self.answered = Some(flag);
    }

    fn owning(memory: Memory) -> Self {
        Self {
            pages: ArrayVec::new(),
            staged: Vec::new(),
            owned: Some(memory),
            run: None,
            answered: None,
        }
    }
}

impl<Memory: DmaBuffer + AsRef<[u8]>> AsRef<[u8]> for BlockPages<Memory> {
    fn as_ref(&self) -> &[u8] {
        self.own_memory().as_ref()
    }
}

impl<Memory: DmaBuffer + AsMut<[u8]>> AsMut<[u8]> for BlockPages<Memory> {
    fn as_mut(&mut self) -> &mut [u8] {
        self.owned
            .as_mut()
            .expect("only a buffer made of its own memory is filled or read by its holder")
            .as_mut()
    }
}
