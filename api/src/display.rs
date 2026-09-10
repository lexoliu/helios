//! The machine's display, and the frame buffers a program draws into.
//!
//! Exactly one program holds the display at a time. [`Display::claim`]
//! takes it; dropping what that returns — or dying — hands every surface
//! back and leaves the outputs blank.
//!
//! Pixels never go through a call. A surface's frame buffer is memory
//! the kernel pinned inside this program's own linear memory, so
//! [`Surface::pixels`] is an ordinary mutable slice and drawing a frame
//! is writing to it. [`Surface::present`] then costs one round trip and
//! copies nothing: the display engine reads the same bytes.

use std::vec::Vec;

use thiserror::Error;

use crate::bindings::helios::system::display as raw;
use crate::wit_bindgen::StreamReader;

pub use crate::bindings::helios::system::display::{
    DisplayChange, FrameToken, Mode, PixelFormat, Placement, Point, Rect, Scanout,
};

/// Why a display request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum DisplayError {
    #[error("this machine has no display device")]
    Unavailable,
    #[error("another program already holds the display")]
    AlreadyClaimed,
    #[error("this program does not hold the display")]
    NotClaimed,
    #[error("the display has no such output")]
    NoSuchScanout,
    #[error("the display engine does not drive this mode")]
    UnsupportedMode,
    #[error("the region does not lie inside the surface")]
    OutOfBounds,
    #[error("this claim already holds as many surfaces as it may")]
    TooManySurfaces,
    #[error("this program's display window has no room left")]
    WindowExhausted,
    #[error("no contiguous run of memory left for a frame buffer")]
    OutOfMemory,
    #[error("the display engine faulted")]
    DeviceFault,
}

impl From<raw::Error> for DisplayError {
    fn from(error: raw::Error) -> Self {
        match error {
            raw::Error::Unavailable => Self::Unavailable,
            raw::Error::AlreadyClaimed => Self::AlreadyClaimed,
            raw::Error::NotClaimed => Self::NotClaimed,
            raw::Error::NoSuchScanout => Self::NoSuchScanout,
            raw::Error::UnsupportedMode => Self::UnsupportedMode,
            raw::Error::OutOfBounds => Self::OutOfBounds,
            raw::Error::TooManySurfaces => Self::TooManySurfaces,
            raw::Error::WindowExhausted => Self::WindowExhausted,
            raw::Error::OutOfMemory => Self::OutOfMemory,
            raw::Error::DeviceFault => Self::DeviceFault,
        }
    }
}

/// Every pixel format this contract carries is four bytes wide.
pub const BYTES_PER_PIXEL: usize = 4;

/// The hardware cursor plane is a fixed 64 by 64 pixels.
pub const CURSOR_WIDTH: u32 = 64;
/// The hardware cursor plane is a fixed 64 by 64 pixels.
pub const CURSOR_HEIGHT: u32 = 64;
/// Bytes one cursor image occupies.
pub const CURSOR_BYTES: usize =
    (CURSOR_WIDTH as usize) * (CURSOR_HEIGHT as usize) * BYTES_PER_PIXEL;

/// This program's hold on the machine's display.
pub struct Display {
    raw: raw::Display,
}

impl Display {
    /// Take exclusive ownership of the machine's display.
    ///
    /// The second caller is refused rather than queued: a program
    /// waiting for a display another program holds is a provisioning
    /// mistake, not a shortage.
    pub fn claim() -> Result<Self, DisplayError> {
        raw::claim()
            .map(|raw| Self { raw })
            .map_err(DisplayError::from)
    }

    /// Every output the device presents right now.
    pub async fn scanouts(&self) -> Result<Vec<Scanout>, DisplayError> {
        self.raw.scanouts().await.map_err(DisplayError::from)
    }

    /// The modes `scanout` will accept, preferred first.
    pub async fn modes(&self, scanout: u32) -> Result<Vec<Mode>, DisplayError> {
        self.raw.modes(scanout).await.map_err(DisplayError::from)
    }

    /// The mode this output would rather be driven at.
    pub async fn preferred_mode(&self, scanout: u32) -> Result<Mode, DisplayError> {
        self.modes(scanout)
            .await?
            .into_iter()
            .next()
            .ok_or(DisplayError::NoSuchScanout)
    }

    /// Create a frame buffer of `mode` in `format` and point `scanout`
    /// at it.
    pub async fn create(
        &self,
        scanout: u32,
        mode: Mode,
        format: PixelFormat,
    ) -> Result<Surface, DisplayError> {
        let raw = self
            .raw
            .create(scanout, mode, format)
            .await
            .map_err(DisplayError::from)?;
        let buffer = raw.buffer();
        Ok(Surface { raw, mode, buffer })
    }

    /// Every change to the device's set of outputs, from now on.
    pub fn changed(&self) -> StreamReader<DisplayChange> {
        self.raw.changed()
    }
}

/// One frame buffer on one output.
pub struct Surface {
    raw: raw::Surface,
    mode: Mode,
    buffer: Placement,
}

impl Surface {
    /// The mode this surface was created at.
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    /// Where the frame buffer sits in this program's linear memory.
    pub const fn placement(&self) -> Placement {
        self.buffer
    }

    /// How many bytes one row of this surface occupies.
    pub const fn stride(&self) -> usize {
        (self.mode.width as usize) * BYTES_PER_PIXEL
    }

    /// The frame buffer itself.
    ///
    /// Rows are [`Surface::stride`] bytes, top row first, with no
    /// padding between them. What is written here is what the display
    /// engine reads when [`Surface::present`] names it — there is no
    /// staging copy anywhere.
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
        // below it while it holds the display.
        unsafe { core::slice::from_raw_parts_mut(offset as *mut u8, length) }
    }

    /// Publish the pixels written into `region`.
    ///
    /// Resolves when the display engine has taken the frame.
    pub async fn present(&self, region: Rect) -> Result<FrameToken, DisplayError> {
        self.raw.present(region).await.map_err(DisplayError::from)
    }

    /// Publish the whole surface.
    pub async fn present_all(&self) -> Result<FrameToken, DisplayError> {
        self.present(Rect {
            x: 0,
            y: 0,
            width: self.mode.width,
            height: self.mode.height,
        })
        .await
    }

    /// Every frame this surface publishes, one item per flush the
    /// display engine completes.
    pub fn vsync(&self) -> StreamReader<FrameToken> {
        self.raw.vsync()
    }

    /// Put `image` on the cursor plane of this surface's output.
    ///
    /// `image` is exactly [`CURSOR_BYTES`] bytes: 64 by 64 pixels in
    /// this surface's format, row-major with no padding. It is taken by
    /// value because the call hands it to the kernel: a borrow would
    /// only move the copy one level up.
    pub async fn set_cursor(&self, image: Vec<u8>, hotspot: Point) -> Result<(), DisplayError> {
        self.raw
            .set_cursor(image, hotspot)
            .await
            .map_err(DisplayError::from)
    }

    /// Move the pointer without touching its image.
    ///
    /// This costs no pixels and no frame-buffer traffic: it is served on
    /// the display engine's own cursor queue, so it never waits behind a
    /// frame this surface is presenting.
    pub async fn move_cursor(&self, position: Point) -> Result<(), DisplayError> {
        self.raw
            .move_cursor(position)
            .await
            .map_err(DisplayError::from)
    }

    /// Take the pointer off this surface's output entirely.
    pub async fn hide_cursor(&self) -> Result<(), DisplayError> {
        self.raw.hide_cursor().await.map_err(DisplayError::from)
    }
}
