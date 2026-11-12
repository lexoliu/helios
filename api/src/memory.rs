//! Memory a program shares with the kernel, and the one way to hold it.
//!
//! Anything a program shares with hardware or with another program
//! without a copy — a frame buffer, a window's pixels, a command buffer,
//! a blob — is shown inside the program's own linear memory, at whole
//! wasm pages the program chose. [`Block`] is those pages: a run taken
//! from the program's allocator, aligned to a page and rounded up to
//! whole pages, that yields the [`Span`] the kernel is told to show
//! something at.
//!
//! [`Backed`] is what every wrapper in this crate that shows something
//! in memory is made of. It holds the kernel resource and the block in
//! one value, so the block cannot be freed while the resource is alive.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ptr::NonNull;

use thiserror::Error;

pub use crate::bindings::helios::system::memory::Span;

/// The size of one wasm page, which is the granule every span is
/// measured in.
pub const PAGE_BYTES: usize = 65536;

/// Bytes one pixel of every display and window surface occupies.
pub const BYTES_PER_PIXEL: usize = 4;

/// Why a block could not be taken from the allocator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum BlockError {
    #[error("a block of no bytes has no pages to show anything at")]
    Empty,
    #[error("a block of that size is more than any memory could hold")]
    TooLarge,
    #[error("this program's heap cannot supply {bytes} bytes of whole pages")]
    OutOfMemory { bytes: u64 },
}

/// A run of whole wasm pages taken from this program's allocator.
///
/// The run starts on a page boundary and covers the requested size
/// rounded up to whole pages, zeroed. It is ordinary memory: the program
/// reads and writes [`Block::bytes`] like any other allocation. While
/// something is shown at its [`Block::span`] the pages are that thing's
/// bytes instead, and when the thing ends they are zeroed ordinary
/// memory again.
///
/// A block is never given to the kernel without a holder that outlives
/// the showing: wrappers hold it through [`Backed`], and a compositor
/// that answers `create` with a span keeps the block until `destroy`.
///
/// A call that names the span and does not complete — it failed, or the
/// program dropped its future while it was in flight — leaves nothing
/// shown there: the kernel shows something at a span only in the call's
/// last step, together with the resource that owns the showing, and
/// whatever it had already asked a device or another program for on the
/// call's behalf is handed back without the program. So the block of a
/// call that did not return a resource is ordinary memory, and freeing it
/// is correct.
pub struct Block {
    pages: NonNull<u8>,
    /// What the program asked for, which is what [`Block::bytes`] covers.
    size: usize,
    /// The whole pages the block owns, which is what the kernel is told.
    reserved: usize,
}

// SAFETY: a block uniquely owns its pages, as a `Box<[u8]>` does; the
// pointer is an ownership handle, not a borrow of anything.
unsafe impl Send for Block {}
// SAFETY: shared access to a block only reads its pages.
unsafe impl Sync for Block {}

impl Block {
    /// Take `bytes` of zeroed memory, rounded up to whole pages.
    pub fn zeroed(bytes: u64) -> Result<Self, BlockError> {
        if bytes == 0 {
            return Err(BlockError::Empty);
        }
        let size = usize::try_from(bytes).map_err(|_| BlockError::TooLarge)?;
        let reserved = size
            .checked_next_multiple_of(PAGE_BYTES)
            .ok_or(BlockError::TooLarge)?;
        let layout =
            Layout::from_size_align(reserved, PAGE_BYTES).map_err(|_| BlockError::TooLarge)?;
        // SAFETY: `reserved` is not zero, because `bytes` is not.
        let pages = NonNull::new(unsafe { alloc_zeroed(layout) })
            .ok_or(BlockError::OutOfMemory { bytes })?;
        Ok(Self {
            pages,
            size,
            reserved,
        })
    }

    /// Take at least `bytes` of zeroed memory, all of its whole pages.
    ///
    /// For what the kernel pins as the whole span rather than as a
    /// size inside it, such as a command buffer: [`Block::bytes`] then
    /// covers every page, because every page is the object's.
    pub fn whole_pages(bytes: u64) -> Result<Self, BlockError> {
        let size = usize::try_from(bytes)
            .map_err(|_| BlockError::TooLarge)?
            .checked_next_multiple_of(PAGE_BYTES)
            .ok_or(BlockError::TooLarge)?;
        Self::zeroed(size as u64)
    }

    /// Take the pixels of a `width` by `height` surface.
    pub fn for_pixels(width: u32, height: u32) -> Result<Self, BlockError> {
        let bytes = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL as u64))
            .ok_or(BlockError::TooLarge)?;
        Self::zeroed(bytes)
    }

    /// The pages, as the kernel is told about them.
    pub fn span(&self) -> Span {
        Span {
            offset: self.pages.as_ptr() as usize as u64,
            length: self.reserved as u64,
        }
    }

    /// How many bytes were asked for, which is how many [`Block::bytes`]
    /// covers.
    pub const fn len(&self) -> usize {
        self.size
    }

    /// Always false: a block has at least one page.
    pub const fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// The bytes the block was asked for.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `pages` is `reserved >= size` bytes this block owns,
        // initialised because they were allocated zeroed and the kernel
        // only ever substitutes memory that is initialised.
        unsafe { core::slice::from_raw_parts(self.pages.as_ptr(), self.size) }
    }

    /// The bytes the block was asked for, writable.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `bytes`, and `&mut self` makes this the only
        // reference into the pages.
        unsafe { core::slice::from_raw_parts_mut(self.pages.as_ptr(), self.size) }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.reserved, PAGE_BYTES)
            .expect("the layout was valid when the block was allocated");
        // SAFETY: `pages` came from `alloc_zeroed` with this layout and
        // is freed exactly once, here.
        unsafe { dealloc(self.pages.as_ptr(), layout) };
    }
}

/// A kernel resource and the block its memory is shown in.
///
/// # Invariant
///
/// While the resource is alive the kernel shows something at the
/// block's span, and the pages are that thing's bytes: if the allocator
/// took them back, it would hand a device's or another program's memory
/// to the next allocation. So the resource is released first, which
/// makes the span ordinary zeroed memory again before the call returns,
/// and the block is freed second. This type is the only place that
/// order is written: the block is private, never moved out and never
/// dropped separately, and `resource` is declared before `block` because
/// fields drop in declaration order.
///
/// Before there is a resource there is nothing to order: a call that
/// does not return one has shown nothing at the span (see [`Block`]), so
/// a wrapper whose call failed or was cancelled drops the block on its
/// own.
pub struct Backed<R> {
    resource: R,
    block: Block,
}

impl<R> Backed<R> {
    /// Hold `resource`, which the kernel shows at `block`'s span.
    pub const fn new(resource: R, block: Block) -> Self {
        Self { resource, block }
    }

    /// The kernel resource.
    pub const fn resource(&self) -> &R {
        &self.resource
    }

    /// The pages, as the kernel was told about them.
    pub fn span(&self) -> Span {
        self.block.span()
    }

    /// How many bytes of the pages are the resource's.
    pub const fn len(&self) -> usize {
        self.block.len()
    }

    /// Always false: a block has at least one page.
    pub const fn is_empty(&self) -> bool {
        self.block.is_empty()
    }

    /// The bytes shown.
    pub fn bytes(&self) -> &[u8] {
        self.block.bytes()
    }

    /// The bytes shown, writable.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.block.bytes_mut()
    }
}
