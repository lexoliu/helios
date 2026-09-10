//! The pages one instance has pinned for the display it holds.
//!
//! A display frame buffer is the claiming instance's own memory: pinned,
//! physically contiguous pages committed from the user pool, placed at a
//! fixed offset inside that instance's linear memory, and handed to the
//! display engine as the backing store of its resource. Nothing is
//! copied through the kernel on the way to the screen, and nothing the
//! kernel owns grows when a compositor asks for a larger surface.
//!
//! The pages go in the display window — the span immediately below the
//! device window, above everything the instance can grow into — for the
//! same reason a granted device's registers do: a `memory.grow` that
//! landed on a frame buffer would hand the display engine whatever the
//! instance put there next.
//!
//! # Concurrency contract
//!
//! An arena belongs to the one task running its instance's store and is
//! never shared, so it needs no lock. Every commit and release goes
//! through the address space, which invalidates the local translation
//! cache and shoots down every other processor that has run in the space
//! before it returns.

use arrayvec::ArrayVec;
use helios_hal::device::DmaPlacement;
use helios_hal::pmm::{PhysFrame, PhysFrameRange};
use helios_hal::vmm::PageFlags;

use crate::device::{DeviceWindow, device_vm_hooks};

use super::DisplayServiceError;

/// Frame buffers one claim may hold at once, cursor planes included.
///
/// A compositor builds its surfaces once and presents into them; the
/// bound is what keeps the arena a value on the store's own stack rather
/// than an allocation whose size a guest chooses.
pub const MAX_PINNED_FRAMES: usize = 16;

/// One pinned, physically contiguous frame buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinnedFrame {
    /// Byte offset of the frame buffer in the instance's linear memory.
    pub offset: u64,
    /// How many bytes it covers, which is the frame rounded up to whole
    /// mapping granules.
    pub bytes: u64,
    /// The physical pages behind it, which are what the display engine
    /// is told to read.
    pub backing: PhysFrameRange,
    /// The alignment the commit asked for. A contiguous run is one
    /// allocation, and an allocator that was given a size and an
    /// alignment has to be given them back.
    align: u64,
}

/// The display window of one instance, as a bump arena.
///
/// Frames are handed out in the order they are asked for. Releasing the
/// most recent one gives its span back — which is the whole of what a
/// compositor changing mode does — and releasing an older one leaves its
/// span held until the claim ends, exactly as a granted device's pinned
/// rings are held for as long as the grant. A claim that churns through
/// the window is answered with [`DisplayServiceError::WindowExhausted`]
/// rather than quietly reusing memory the display engine may still be
/// reading.
pub struct DisplayPins {
    window: DeviceWindow,
    cursor: u64,
    frames: ArrayVec<PinnedFrame, MAX_PINNED_FRAMES>,
    pinned_bytes: u64,
}

impl DisplayPins {
    pub fn new(window: DeviceWindow) -> Self {
        Self {
            window,
            cursor: 0,
            frames: ArrayVec::new(),
            pinned_bytes: 0,
        }
    }

    /// The window these frames live in.
    pub const fn window(&self) -> DeviceWindow {
        self.window
    }

    /// How many bytes of the instance's memory this claim has pinned.
    pub const fn pinned_bytes(&self) -> u64 {
        self.pinned_bytes
    }

    /// How many frames are pinned.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Commit a physically contiguous frame buffer of at least `bytes`.
    pub fn pin(&mut self, bytes: u64) -> Result<PinnedFrame, DisplayServiceError> {
        if bytes == 0 {
            return Err(DisplayServiceError::UnsupportedMode);
        }
        if self.frames.is_full() {
            return Err(DisplayServiceError::TooManySurfaces);
        }
        let granule = self.granule();
        // `bytes` is derived from a mode the guest named, so the
        // rounding is checked: an unchecked one wraps in a release build
        // and would answer a request for most of the address space with
        // a buffer of a few pages, which the display engine would then
        // be told to scan out.
        let bytes = bytes
            .checked_next_multiple_of(granule)
            .ok_or(DisplayServiceError::WindowExhausted)?;
        let offset = self.carve(bytes, granule)?;
        let virt = self.window.range_at(offset, bytes);
        let first = (device_vm_hooks().commit_contiguous)(
            virt,
            PageFlags::READ | PageFlags::WRITE,
            DmaPlacement {
                align: granule,
                // The display engine is told the frame's physical
                // address directly, so what bounds it is the machine's
                // memory rather than a translation unit's window.
                limit: u64::MAX,
            },
        )
        .map_err(|_| DisplayServiceError::OutOfMemory)?;
        let frame = PinnedFrame {
            offset: self.window.offset() + offset,
            bytes,
            backing: PhysFrameRange {
                start: first,
                frame_count: usize::try_from(bytes / (PhysFrame::SIZE as u64))
                    .expect("a window span fits the address space"),
            },
            align: granule,
        };
        self.frames.push(frame);
        self.pinned_bytes += bytes;
        Ok(frame)
    }

    /// Hand `frame` back.
    ///
    /// Its span returns to the arena only when it is the most recent
    /// one; otherwise the pages are released and the span stays held
    /// until the claim ends. Either way the instance's pool has its
    /// pages back on return.
    ///
    /// # Panics
    ///
    /// Panics when the address space refuses to release the commit. The
    /// kernel cannot then prove the pages are the instance's again, and
    /// handing them to the next allocation would be a silent corruption.
    pub fn unpin(&mut self, frame: PinnedFrame) {
        let Some(index) = self.frames.iter().position(|held| *held == frame) else {
            return;
        };
        self.frames.remove(index);
        self.pinned_bytes -= frame.bytes;
        let offset = frame.offset - self.window.offset();
        release(self.window, frame);
        if offset + frame.bytes == self.cursor {
            self.cursor = offset;
        }
    }

    /// The smallest unit the address space can change a mapping at,
    /// which is what every frame is aligned and sized to.
    fn granule(&self) -> u64 {
        let granule = (device_vm_hooks().mapping_granule)();
        assert!(
            granule >= PhysFrame::SIZE as u64 && granule.is_power_of_two(),
            "an address space maps at a power-of-two granule of at least one frame, not {granule}"
        );
        granule
    }

    fn carve(&mut self, bytes: u64, align: u64) -> Result<u64, DisplayServiceError> {
        let start = self
            .cursor
            .checked_next_multiple_of(align)
            .ok_or(DisplayServiceError::WindowExhausted)?;
        let end = start
            .checked_add(bytes)
            .ok_or(DisplayServiceError::WindowExhausted)?;
        if end > self.window.bytes() {
            return Err(DisplayServiceError::WindowExhausted);
        }
        self.cursor = end;
        Ok(start)
    }
}

impl Drop for DisplayPins {
    fn drop(&mut self) {
        for frame in &self.frames {
            release(self.window, *frame);
        }
    }
}

/// Give one frame's pages back to the instance's pool.
///
/// # Panics
///
/// Panics when the address space refuses, for the reason
/// [`DisplayPins::unpin`] documents.
fn release(window: DeviceWindow, frame: PinnedFrame) {
    let offset = frame.offset - window.offset();
    let virt = window.range_at(offset, frame.bytes);
    (device_vm_hooks().release_contiguous)(virt, frame.align).unwrap_or_else(|error| {
        panic!("a display frame buffer the address space would not release: {error}")
    });
}
