//! The 2D display contract.
//!
//! A display device owns scanouts — the outputs a machine presents — and
//! a hardware cursor plane that rides above whatever a scanout shows.
//! Both are hardware facts rather than a consumer's vocabulary: a
//! scanout is a thing that latches pixels from memory and drives them
//! out, and a cursor plane is a small overlay the display engine
//! composites without touching the frame buffer underneath. virtio-gpu
//! implements them, and so do the display controllers of real silicon,
//! so the value types and the device trait live here and the concrete
//! driver encodes them onto its own wire format.
//!
//! Frame buffers are never allocated here. A [`DisplayDevice`] is handed
//! the physical pages a caller already owns and attaches them as the
//! backing store of a device-side resource; the pages stay the caller's
//! for as long as the frame buffer exists, and destroying the frame
//! buffer hands them back untouched. A driver that allocated the pixels
//! itself would own memory the kernel has to account for, would fix the
//! frame buffer's lifetime to the device's, and would put a second
//! allocator in the one place the kernel most wants a single one.
//!
//! # SMP contract
//!
//! Every method takes `&self` and may be called from any processor and
//! from several tasks at once. The asynchronous ones park on the
//! device's completion notification rather than spinning, and an
//! implementation serialises access to its own rings internally. The
//! ordering between two concurrent calls is the ordering the device
//! sees; a caller that needs one operation to precede another awaits the
//! first.

use core::future::Future;

use arrayvec::ArrayVec;
use thiserror::Error;

use crate::io::IoError;
use crate::pmm::PhysFrameRange;

/// Largest number of scanouts a display device may present.
///
/// The bound is the hardware's: virtio-gpu fixes its display-info reply
/// at sixteen entries, and no display engine Helios targets drives more
/// heads than that. It is what makes [`ScanoutList`] a value rather than
/// an allocation.
pub const MAX_SCANOUTS: usize = 16;

/// Largest number of physical ranges one frame buffer's backing store
/// may be built from.
///
/// A frame buffer is a single logical surface, so its backing wants to
/// be as close to contiguous as the caller's frame allocator can make
/// it; the bound is what keeps the attach request a value on the
/// caller's stack instead of an allocation whose size the device
/// chooses. A caller with a more fragmented allocation asks its frame
/// allocator for a contiguous run rather than passing the fragments on.
pub const MAX_BACKING_RANGES: usize = 64;

/// One output the display device drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScanoutId(u32);

impl ScanoutId {
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    pub const fn index(self) -> u32 {
        self.0
    }
}

/// A frame buffer the device holds on the caller's behalf.
///
/// The identifier is opaque: it names a device-side resource together
/// with the backing the caller attached to it, and only the device that
/// issued it can interpret it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FramebufferId(u32);

impl FramebufferId {
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// A pixel position on a scanout or inside a frame buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point {
    pub x: u32,
    pub y: u32,
}

impl Point {
    pub const fn new(x: u32, y: u32) -> Self {
        Self { x, y }
    }
}

/// A rectangular region of a frame buffer or of a scanout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The rectangle covering a whole mode, at the origin.
    pub const fn of(mode: DisplayMode) -> Self {
        Self::new(0, 0, mode.width, mode.height)
    }

    /// Whether this rectangle lies entirely inside `mode`.
    ///
    /// Saturating rather than wrapping arithmetic: a rectangle whose
    /// right edge overflows is outside every mode, and wrapping would
    /// report it as inside one.
    pub const fn fits_in(self, mode: DisplayMode) -> bool {
        self.x.saturating_add(self.width) <= mode.width
            && self.y.saturating_add(self.height) <= mode.height
    }
}

/// The pixel geometry of one scanout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
}

impl DisplayMode {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Bytes one frame of this mode occupies in `format`.
    ///
    /// `None` when the product overflows the address space, which is a
    /// mode no memory could back rather than a mode to round down.
    pub const fn frame_bytes(self, format: PixelFormat) -> Option<usize> {
        let Some(pixels) = (self.width as usize).checked_mul(self.height as usize) else {
            return None;
        };
        pixels.checked_mul(format.bytes_per_pixel())
    }
}

/// How a display device reads the bytes of one pixel.
///
/// The set is the eight 32-bit orderings a 2D display engine latches
/// directly; `X` names a byte the engine ignores and `A` one it reads as
/// alpha. Anything else — a packed 16-bit format, a planar layout, a
/// compressed surface — is not a format this contract carries, because
/// no scanout Helios drives presents one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgrx8888,
    Bgra8888,
    Xrgb8888,
    Argb8888,
    Rgbx8888,
    Rgba8888,
    Xbgr8888,
    Abgr8888,
}

impl PixelFormat {
    /// Every format this contract carries is four bytes wide.
    pub const fn bytes_per_pixel(self) -> usize {
        4
    }
}

/// One scanout as the device currently presents it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanoutInfo {
    pub id: ScanoutId,
    /// Where this output sits in the device's desktop, and how large it
    /// is. A single-head machine reports the whole desktop at the
    /// origin.
    pub geometry: Rect,
    /// Whether the host is presenting this output at all. A disabled
    /// scanout still has geometry — the last the host published — and
    /// still accepts a frame buffer.
    pub enabled: bool,
}

/// Every scanout a device presents, in scanout order.
pub type ScanoutList = ArrayVec<ScanoutInfo, MAX_SCANOUTS>;

/// The pointer image on the hardware cursor plane.
///
/// The plane's dimensions are fixed by the hardware rather than chosen
/// by the caller, so a cursor is a frame buffer of exactly
/// [`CursorImage::WIDTH`] by [`CursorImage::HEIGHT`] pixels together
/// with the point inside it that the pointer actually points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorImage {
    /// The frame buffer holding the pointer's pixels. It must have been
    /// created at exactly the cursor plane's size.
    pub framebuffer: FramebufferId,
    /// Which pixel of that image sits under the pointer position.
    pub hotspot: Point,
}

impl CursorImage {
    /// Width of the hardware cursor plane, in pixels.
    pub const WIDTH: u32 = 64;
    /// Height of the hardware cursor plane, in pixels.
    pub const HEIGHT: u32 = 64;

    /// The mode a cursor frame buffer has to be created with.
    pub const MODE: DisplayMode = DisplayMode::new(Self::WIDTH, Self::HEIGHT);
}

/// Why a display operation did not happen.
///
/// The variants are the answers a display engine gives, kept apart
/// rather than folded into one fault: "the scanout you named does not
/// exist" is a caller bug and "the device is out of memory" is a
/// resource condition, and a caller that has to tell them apart cannot
/// do it from a single code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum DisplayError {
    /// The device refused the request without naming a reason.
    #[error("the display device refused the request without naming a reason")]
    Unspecified,
    /// The device could not find room for the resource.
    #[error("the display device is out of memory")]
    OutOfMemory,
    /// No such output on this device.
    #[error("scanout {} does not exist on this device", .0.index())]
    UnknownScanout(ScanoutId),
    /// No such frame buffer on this device.
    #[error("frame buffer {} does not exist on this device", .0.raw())]
    UnknownFramebuffer(FramebufferId),
    /// The device rejected one of the request's parameters.
    #[error("the display device rejected a request parameter")]
    InvalidParameter,
    /// The device answered with a code that does not belong to this
    /// request, which is a device fault rather than a refusal.
    #[error("the display device answered with the unexpected code {code:#x}")]
    UnexpectedResponse { code: u32 },
    /// The region does not lie inside the frame buffer it names.
    #[error(
        "region {width}x{height}+{x}+{y} does not lie inside a {buffer_width}x{buffer_height} frame buffer"
    )]
    RegionOutOfBounds {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        buffer_width: u32,
        buffer_height: u32,
    },
    /// The cursor plane is a fixed size and this frame buffer is not it.
    #[error(
        "a cursor image must be {}x{} pixels, not {width}x{height}",
        CursorImage::WIDTH,
        CursorImage::HEIGHT
    )]
    CursorSize { width: u32, height: u32 },
    /// The backing store is built from more ranges than one attach
    /// request carries.
    #[error("a frame buffer's backing may span at most {limit} ranges, not {ranges}")]
    TooManyBackingRanges { ranges: usize, limit: usize },
    /// The device holds as many frame buffers as it has room to track.
    #[error("the display device already holds its maximum of {limit} frame buffers")]
    TooManyFramebuffers { limit: usize },
    /// The transport underneath the display engine failed.
    #[error("display transport: {0}")]
    Transport(#[from] IoError),
}

pub type DisplayResult<T> = Result<T, DisplayError>;

/// A device that presents frame buffers on one or more scanouts and
/// carries a hardware cursor plane.
///
/// The trait is deliberately resource-level rather than surface-level:
/// which surface is composited where, what a window is, and who owns the
/// pointer are kernel and compositor concerns that no display engine
/// knows about, and a contract that named them would have to be
/// reimplemented per device.
pub trait DisplayDevice: Send + Sync + 'static {
    /// The outputs the device presents right now.
    fn scanouts(&self) -> impl Future<Output = DisplayResult<ScanoutList>> + Send + '_;

    /// The mode the host would rather `scanout` were driven at.
    ///
    /// This is the display's own answer — the preferred timing of the
    /// attached monitor where the device reports one, and the geometry
    /// the host published otherwise — not a mode the caller has to
    /// accept. A caller is free to create a frame buffer of any size and
    /// scale it onto the scanout.
    fn preferred_mode(
        &self,
        scanout: ScanoutId,
    ) -> impl Future<Output = DisplayResult<DisplayMode>> + Send + '_;

    /// Creates a frame buffer of `mode` in `format` over pages the
    /// caller owns.
    ///
    /// `backing` must cover at least [`DisplayMode::frame_bytes`], in
    /// the order the pixels are laid out: the device reads the ranges as
    /// one flat span, row-major from the first range's first byte. The
    /// pages stay the caller's; the device only reads them, and only
    /// between this call and [`DisplayDevice::destroy_framebuffer`].
    fn create_framebuffer<'a>(
        &'a self,
        mode: DisplayMode,
        format: PixelFormat,
        backing: &'a [PhysFrameRange],
    ) -> impl Future<Output = DisplayResult<FramebufferId>> + Send + 'a;

    /// Releases a frame buffer and hands its backing pages back.
    ///
    /// On return the device holds no reference to the caller's pages.
    fn destroy_framebuffer(
        &self,
        framebuffer: FramebufferId,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_;

    /// Points `scanout` at `source`, a region of `framebuffer`.
    ///
    /// The region is scaled onto the scanout by the display engine when
    /// it does not match the output's geometry.
    fn set_scanout(
        &self,
        scanout: ScanoutId,
        framebuffer: FramebufferId,
        source: Rect,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_;

    /// Takes whatever `scanout` is showing off it.
    ///
    /// The output keeps its geometry and stays a scanout the device
    /// presents; what it loses is the frame buffer it was latching. The
    /// operation exists because releasing a frame buffer is not enough:
    /// a scanout still pointed at a resource the caller is about to
    /// destroy would leave the display engine reading pages that have
    /// gone back to their owner's pool, so an owner that is letting go
    /// blanks its outputs before it destroys anything.
    fn blank_scanout(
        &self,
        scanout: ScanoutId,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_;

    /// Publishes the pixels the caller wrote into `region` of
    /// `framebuffer`.
    ///
    /// Two things have to happen and both are the device's: the bytes
    /// have to reach the device's own copy of the resource, and the
    /// scanouts showing it have to be told to re-read. A caller that did
    /// only the first would write pixels nobody displays.
    fn flush(
        &self,
        framebuffer: FramebufferId,
        region: Rect,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_;

    /// Puts `image` on the cursor plane of `scanout` at `position`.
    ///
    /// The image's pixels are published as part of this call, so a
    /// caller that has just redrawn the pointer does not flush it
    /// separately.
    fn set_cursor(
        &self,
        scanout: ScanoutId,
        position: Point,
        image: CursorImage,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_;

    /// Takes the pointer off `scanout` entirely.
    fn hide_cursor(
        &self,
        scanout: ScanoutId,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_;

    /// Moves the pointer on `scanout` without touching its image.
    ///
    /// This is the whole reason a cursor plane exists: it is the one
    /// display update that costs no pixels and no frame buffer traffic,
    /// so pointer motion never waits on a compositor.
    fn move_cursor(
        &self,
        scanout: ScanoutId,
        position: Point,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_;

    /// Resolves when the device's set of scanouts changes.
    ///
    /// A monitor plugged in, unplugged, or resized by the host is the
    /// event; what changed is read back with [`DisplayDevice::scanouts`].
    fn display_changed(&self) -> impl Future<Output = ()> + Send + '_;
}

/// A shared display device is a display device.
///
/// A backend hands the same driver to two owners at once — the kernel
/// task that follows the display topology and the interrupt route that
/// wakes it — so the shared handle satisfies the contract without every
/// backend writing the same eleven forwarding methods.
impl<Device: DisplayDevice + ?Sized> DisplayDevice for alloc::sync::Arc<Device> {
    fn scanouts(&self) -> impl Future<Output = DisplayResult<ScanoutList>> + Send + '_ {
        Device::scanouts(self)
    }

    fn preferred_mode(
        &self,
        scanout: ScanoutId,
    ) -> impl Future<Output = DisplayResult<DisplayMode>> + Send + '_ {
        Device::preferred_mode(self, scanout)
    }

    fn create_framebuffer<'a>(
        &'a self,
        mode: DisplayMode,
        format: PixelFormat,
        backing: &'a [PhysFrameRange],
    ) -> impl Future<Output = DisplayResult<FramebufferId>> + Send + 'a {
        Device::create_framebuffer(self, mode, format, backing)
    }

    fn destroy_framebuffer(
        &self,
        framebuffer: FramebufferId,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_ {
        Device::destroy_framebuffer(self, framebuffer)
    }

    fn set_scanout(
        &self,
        scanout: ScanoutId,
        framebuffer: FramebufferId,
        source: Rect,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_ {
        Device::set_scanout(self, scanout, framebuffer, source)
    }

    fn blank_scanout(
        &self,
        scanout: ScanoutId,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_ {
        Device::blank_scanout(self, scanout)
    }

    fn flush(
        &self,
        framebuffer: FramebufferId,
        region: Rect,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_ {
        Device::flush(self, framebuffer, region)
    }

    fn set_cursor(
        &self,
        scanout: ScanoutId,
        position: Point,
        image: CursorImage,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_ {
        Device::set_cursor(self, scanout, position, image)
    }

    fn hide_cursor(
        &self,
        scanout: ScanoutId,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_ {
        Device::hide_cursor(self, scanout)
    }

    fn move_cursor(
        &self,
        scanout: ScanoutId,
        position: Point,
    ) -> impl Future<Output = DisplayResult<()>> + Send + '_ {
        Device::move_cursor(self, scanout, position)
    }

    fn display_changed(&self) -> impl Future<Output = ()> + Send + '_ {
        Device::display_changed(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{CursorImage, DisplayMode, PixelFormat, Rect};

    #[test]
    fn a_frame_is_four_bytes_a_pixel_in_every_format() {
        let mode = DisplayMode::new(1280, 800);
        for format in [
            PixelFormat::Bgrx8888,
            PixelFormat::Bgra8888,
            PixelFormat::Xrgb8888,
            PixelFormat::Argb8888,
            PixelFormat::Rgbx8888,
            PixelFormat::Rgba8888,
            PixelFormat::Xbgr8888,
            PixelFormat::Abgr8888,
        ] {
            assert_eq!(mode.frame_bytes(format), Some(1280 * 800 * 4));
        }
    }

    #[test]
    fn a_mode_no_memory_could_back_reports_no_size() {
        let mode = DisplayMode::new(u32::MAX, u32::MAX);
        assert_eq!(mode.frame_bytes(PixelFormat::Bgrx8888), None);
    }

    #[test]
    fn a_region_is_inside_a_mode_only_when_both_edges_are() {
        let mode = DisplayMode::new(1920, 1080);
        assert!(Rect::of(mode).fits_in(mode));
        assert!(Rect::new(1900, 1070, 20, 10).fits_in(mode));
        assert!(!Rect::new(1900, 1070, 21, 10).fits_in(mode));
        assert!(!Rect::new(0, 0, 1921, 1080).fits_in(mode));
    }

    /// A right edge that wraps is outside every mode; wrapping
    /// arithmetic would report it as inside one.
    #[test]
    fn a_region_whose_edge_overflows_is_inside_nothing() {
        let mode = DisplayMode::new(1920, 1080);
        assert!(!Rect::new(u32::MAX, 0, 16, 16).fits_in(mode));
        assert!(!Rect::new(0, u32::MAX, 16, 16).fits_in(mode));
    }

    #[test]
    fn a_cursor_frame_buffer_is_the_size_of_the_plane() {
        assert_eq!(
            CursorImage::MODE,
            DisplayMode::new(CursorImage::WIDTH, CursorImage::HEIGHT)
        );
        assert_eq!(
            CursorImage::MODE.frame_bytes(PixelFormat::Bgra8888),
            Some(64 * 64 * 4)
        );
    }
}
