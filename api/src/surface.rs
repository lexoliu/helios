//! Windows on the desktop, and the pixels a program draws into them.
//!
//! A program that wants to be seen asks the desktop for a
//! [`Surface`]. The compositor composes it; this program owns its
//! pixels, and nothing is copied on the way to the screen: the frame
//! buffer is whole pages of this program's own heap that the kernel
//! backs with pinned frames, and shows at the compositor's own pages as
//! well, so [`Surface::pixels`] is an ordinary mutable slice —
//! `copy_from_slice` and `fill` work on it — and publishing a frame is
//! [`Surface::commit`], one round trip that copies nothing.
//!
//! Dropping the surface takes the window off the desktop.

use std::vec::Vec;

use thiserror::Error;

use crate::bindings::helios::system::surface as raw;
use crate::memory::{Backed, Block, BlockError};
use crate::wit_bindgen::StreamReader;

pub use crate::bindings::helios::system::surface::{InputEvent, Rect, Size};
pub use crate::memory::Span;

/// Why a surface request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum SurfaceError {
    #[error("this machine ships no compositor")]
    Unavailable,
    #[error("the compositor is not running")]
    NoCompositor,
    #[error("a window of that size cannot be backed")]
    UnsupportedSize,
    #[error("this program already holds as many windows as it may")]
    TooManySurfaces,
    #[error("the region does not lie inside the window")]
    OutOfBounds,
    #[error("the window's pages are not a whole-page run of this program's memory")]
    BadSpan,
    #[error("no contiguous run of memory left for a window")]
    OutOfMemory,
    #[error("this window is gone")]
    Gone,
    #[error("only the compositor may deliver input to a window")]
    NotTheCompositor,
    #[error("this program's heap cannot supply the window's pixels: {0}")]
    Heap(#[from] BlockError),
}

impl From<raw::Error> for SurfaceError {
    fn from(error: raw::Error) -> Self {
        match error {
            raw::Error::Unavailable => Self::Unavailable,
            raw::Error::NoCompositor => Self::NoCompositor,
            raw::Error::UnsupportedSize => Self::UnsupportedSize,
            raw::Error::TooManySurfaces => Self::TooManySurfaces,
            raw::Error::OutOfBounds => Self::OutOfBounds,
            raw::Error::BadSpan => Self::BadSpan,
            raw::Error::OutOfMemory => Self::OutOfMemory,
            raw::Error::Gone => Self::Gone,
            raw::Error::NotTheCompositor => Self::NotTheCompositor,
        }
    }
}

/// Every pixel of a surface is four bytes, in the display's own
/// `bgrx8888` ordering.
pub use crate::memory::BYTES_PER_PIXEL;

/// One window on the desktop.
///
/// The kernel resource and the heap pages its pixels are shown in are
/// one value (see [`Backed`]): dropping the surface takes the window off
/// the desktop, and only then gives the pages back to the allocator.
pub struct Surface {
    window: Backed<raw::Surface>,
    size: Size,
}

impl Surface {
    /// Ask the desktop for a window of `width` by `height` pixels.
    ///
    /// The pixels are taken from this program's heap, sized to the
    /// window, and start zeroed.
    pub async fn create(width: u32, height: u32) -> Result<Self, SurfaceError> {
        let block = Block::for_pixels(width, height)?;
        let raw = raw::create(width, height, block.span())
            .await
            .map_err(SurfaceError::from)?;
        let size = raw.size();
        Ok(Self {
            window: Backed::new(raw, block),
            size,
        })
    }

    /// The size this window was created at.
    pub const fn size(&self) -> Size {
        self.size
    }

    /// The pages of this program's memory the pixels are shown at.
    pub fn span(&self) -> Span {
        self.window.span()
    }

    /// How many bytes one row of this window occupies.
    pub const fn stride(&self) -> usize {
        (self.size.width as usize) * BYTES_PER_PIXEL
    }

    /// The frame buffer itself.
    ///
    /// Rows are [`Surface::stride`] bytes, top row first, with no
    /// padding between them. What is written here is what the compositor
    /// composes when [`Surface::commit`] names it — there is no staging
    /// copy anywhere. The slice is exactly `stride * height` bytes of
    /// this program's own heap, valid for as long as the borrow of the
    /// surface.
    pub fn pixels(&mut self) -> &mut [u8] {
        self.window.bytes_mut()
    }

    /// Publish the pixels written into `region`.
    pub async fn commit(&self, region: Rect) -> Result<(), SurfaceError> {
        self.window
            .resource()
            .commit(region)
            .await
            .map_err(SurfaceError::from)
    }

    /// Publish the whole window.
    pub async fn commit_all(&self) -> Result<(), SurfaceError> {
        self.commit(Rect {
            x: 0,
            y: 0,
            width: self.size.width,
            height: self.size.height,
        })
        .await
    }

    /// Every input event the compositor routed to this window, from now
    /// on.
    ///
    /// The stream ends when the window leaves the desktop, so a reader
    /// that runs dry has been told the window is gone rather than that
    /// the user stopped typing.
    pub fn events(&self) -> StreamReader<InputEvent> {
        self.window.resource().events()
    }
}

/// Hand `events` to the window `id` names.
///
/// The compositor's way back: it holds every input device and decides
/// which window has focus, and this is how what it decided reaches the
/// program that owns the window. Every caller other than the compositor
/// is refused with [`SurfaceError::NotTheCompositor`].
pub async fn deliver(id: u64, events: Vec<InputEvent>) -> Result<(), SurfaceError> {
    raw::deliver(id, events).await.map_err(SurfaceError::from)
}
