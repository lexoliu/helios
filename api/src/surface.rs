//! Windows on the desktop, and the pixels a program draws into them.
//!
//! A program that wants to be seen asks the desktop for a
//! [`Surface`]. The compositor composes it; this program owns its
//! pixels, and nothing is copied on the way to the screen: the frame
//! buffer is memory the kernel pinned inside this program's own linear
//! memory and mapped into the compositor's as well, so
//! [`Surface::pixels`] is an ordinary mutable slice and publishing a
//! frame is [`Surface::commit`] — one round trip that copies nothing.
//!
//! Dropping the surface takes the window off the desktop.

use std::vec::Vec;

use thiserror::Error;

use crate::bindings::helios::system::surface as raw;
use crate::wit_bindgen::StreamReader;

pub use crate::bindings::helios::system::surface::{InputEvent, Placement, Rect, Size};

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
    #[error("this program's surface window has no room left")]
    WindowExhausted,
    #[error("no contiguous run of memory left for a window")]
    OutOfMemory,
    #[error("this window is gone")]
    Gone,
    #[error("only the compositor may deliver input to a window")]
    NotTheCompositor,
}

impl From<raw::Error> for SurfaceError {
    fn from(error: raw::Error) -> Self {
        match error {
            raw::Error::Unavailable => Self::Unavailable,
            raw::Error::NoCompositor => Self::NoCompositor,
            raw::Error::UnsupportedSize => Self::UnsupportedSize,
            raw::Error::TooManySurfaces => Self::TooManySurfaces,
            raw::Error::OutOfBounds => Self::OutOfBounds,
            raw::Error::WindowExhausted => Self::WindowExhausted,
            raw::Error::OutOfMemory => Self::OutOfMemory,
            raw::Error::Gone => Self::Gone,
            raw::Error::NotTheCompositor => Self::NotTheCompositor,
        }
    }
}

/// Every pixel of a surface is four bytes, in the display's own
/// `bgrx8888` ordering.
pub const BYTES_PER_PIXEL: usize = 4;

/// One window on the desktop.
pub struct Surface {
    raw: raw::Surface,
    size: Size,
    buffer: Placement,
}

impl Surface {
    /// Ask the desktop for a window of `width` by `height` pixels.
    pub async fn create(width: u32, height: u32) -> Result<Self, SurfaceError> {
        let raw = raw::create(width, height)
            .await
            .map_err(SurfaceError::from)?;
        let size = raw.size();
        let buffer = raw.buffer();
        Ok(Self { raw, size, buffer })
    }

    /// The size this window was created at.
    pub const fn size(&self) -> Size {
        self.size
    }

    /// Where the frame buffer sits in this program's linear memory.
    pub const fn placement(&self) -> Placement {
        self.buffer
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
    /// copy anywhere.
    ///
    /// # Panics
    ///
    /// Panics when the kernel placed the frame buffer somewhere this
    /// program cannot address, which would mean the two disagree about
    /// the size of a pointer.
    pub fn pixels(&mut self) -> &mut [u8] {
        let offset = usize::try_from(self.buffer.offset)
            .expect("the kernel places a frame buffer inside this program's address space");
        let length = usize::try_from(self.buffer.length)
            .expect("the kernel places a frame buffer inside this program's address space");
        // SAFETY: the kernel pinned `length` bytes of physically
        // contiguous memory at `offset` in this program's linear memory
        // and mapped them readable and writable for as long as this
        // surface exists. Nothing else in this program can reach them:
        // the offset is above everything the allocator can ever hand
        // out, because the kernel caps this program's memory growth
        // below it while it holds a window.
        unsafe { core::slice::from_raw_parts_mut(offset as *mut u8, length) }
    }

    /// Publish the pixels written into `region`.
    pub async fn commit(&self, region: Rect) -> Result<(), SurfaceError> {
        self.raw.commit(region).await.map_err(SurfaceError::from)
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
        self.raw.events()
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
