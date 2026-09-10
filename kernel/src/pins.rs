//! The pages one instance has pinned inside its own linear memory.
//!
//! Two paths put pages there and they want the same thing. A display
//! frame buffer is the claiming instance's own memory: pinned,
//! physically contiguous pages committed from the user pool, placed at a
//! fixed offset inside that instance's linear memory, and handed to the
//! display engine as the backing store of its resource. A compositor
//! surface is the client's own memory in exactly the same sense, plus a
//! second view of the same run inside the compositor, so that the
//! compositor composes from the bytes the client wrote rather than from
//! a copy of them.
//!
//! Both live in a window — a span of the reservation above everything
//! the instance can grow into — for the same reason a granted device's
//! registers do: a `memory.grow` that landed on a frame buffer would
//! hand the display engine, or the compositor, whatever the instance put
//! there next.
//!
//! Nothing is copied through the kernel on the way to the screen, and
//! nothing the kernel owns grows when a compositor asks for a larger
//! surface.
//!
//! # Concurrency contract
//!
//! An arena belongs to the one task running its instance's store and is
//! never shared, so it needs no lock. Every commit, map and release goes
//! through the address space, which invalidates the local translation
//! cache and shoots down every other processor that has run in the space
//! before it returns.

use arrayvec::ArrayVec;
use helios_hal::device::DmaPlacement;
use helios_hal::iommu::PhysicalRange;
use helios_hal::pmm::{PhysFrame, PhysFrameRange};
use helios_hal::vmm::PageFlags;
use thiserror::Error;

use crate::device::{DeviceWindow, device_vm_hooks};

/// Frame buffers one display claim may hold at once, cursor planes
/// included.
///
/// A compositor builds its scanout surfaces once and presents into them;
/// the bound is what keeps the arena a value on the store's own stack
/// rather than an allocation whose size a guest chooses.
pub const MAX_PINNED_FRAMES: usize = 16;

/// Every pixel format a Helios display engine latches is four bytes
/// wide, so a frame's size is its area times this and its stride is its
/// width times this.
pub const BYTES_PER_PIXEL: usize = 4;

/// Why an arena refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum PinError {
    /// A zero-byte run has nothing to pin.
    #[error("a pinned run covers at least one page")]
    Empty,
    /// The arena already holds as many runs as it may.
    #[error("this arena holds as many pinned runs as it may")]
    TooMany,
    /// The window has no room left.
    #[error("this instance's window has no room left")]
    WindowExhausted,
    /// The machine has no contiguous run of that size left, or the
    /// instance's own accounting refused it.
    #[error("no contiguous run of memory left")]
    OutOfMemory,
    /// The address space would not hand this instance a second view of a
    /// run another instance committed. A backend that cannot express one
    /// says so here.
    #[error("this address space cannot map memory another instance holds")]
    ShareRefused,
}

/// Where a run's pages came from, which is what says how to give them
/// back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PinBacking {
    /// Committed from this instance's pool. Releasing it hands the
    /// pages back, with the alignment the allocator was given.
    Owned { align: u64 },
    /// A second view of a run another instance committed. Releasing it
    /// drops the view and frees nothing: the pages go back when their
    /// owner's arena lets them go.
    Shared,
}

/// One pinned, physically contiguous run inside an instance's linear
/// memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinnedFrame {
    /// Byte offset of the run in the instance's linear memory.
    pub offset: u64,
    /// How many bytes it covers, which is the request rounded up to
    /// whole mapping granules.
    pub bytes: u64,
    /// The physical pages behind it, which are what the display engine
    /// is told to read and what a second instance is handed a view of.
    pub backing: PhysFrameRange,
    kind: PinBacking,
}

impl PinnedFrame {
    /// The physical bytes behind this run.
    pub const fn physical(&self) -> PhysicalRange {
        PhysicalRange::new(self.backing.start.phys_addr() as u64, self.bytes)
    }

    /// Whether this is a view of somebody else's run rather than a run
    /// of this instance's own.
    pub const fn is_shared(&self) -> bool {
        matches!(self.kind, PinBacking::Shared)
    }
}

/// One window of one instance, as a bump arena.
///
/// Runs are handed out in the order they are asked for. Releasing the
/// most recent one gives its span back — which is the whole of what a
/// compositor changing mode does — and releasing an older one leaves its
/// span held until the arena ends, exactly as a granted device's pinned
/// rings are held for as long as the grant. An arena that churns through
/// its window is answered with [`PinError::WindowExhausted`] rather than
/// quietly reusing memory the display engine, or a compositor, may still
/// be reading.
pub struct PinnedFrames<const CAPACITY: usize> {
    window: DeviceWindow,
    cursor: u64,
    frames: ArrayVec<PinnedFrame, CAPACITY>,
    pinned_bytes: u64,
}

impl<const CAPACITY: usize> PinnedFrames<CAPACITY> {
    pub fn new(window: DeviceWindow) -> Self {
        Self {
            window,
            cursor: 0,
            frames: ArrayVec::new(),
            pinned_bytes: 0,
        }
    }

    /// The window these runs live in.
    pub const fn window(&self) -> DeviceWindow {
        self.window
    }

    /// How many bytes of the instance's memory this arena has pinned,
    /// views included.
    pub const fn pinned_bytes(&self) -> u64 {
        self.pinned_bytes
    }

    /// How many runs are held.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Commit a physically contiguous run of at least `bytes` from this
    /// instance's own pool.
    pub fn pin(&mut self, bytes: u64) -> Result<PinnedFrame, PinError> {
        let (offset, bytes, granule) = self.carve_for(bytes)?;
        let virt = self.window.range_at(offset, bytes);
        let first = (device_vm_hooks().commit_contiguous)(
            virt,
            PageFlags::READ | PageFlags::WRITE,
            DmaPlacement {
                align: granule,
                // The display engine is told the run's physical address
                // directly, so what bounds it is the machine's memory
                // rather than a translation unit's window.
                limit: u64::MAX,
            },
        )
        .map_err(|_| PinError::OutOfMemory)?;
        Ok(self.record(offset, bytes, first, PinBacking::Owned { align: granule }))
    }

    /// Map `physical` — a run another instance committed — into this
    /// instance's window.
    ///
    /// Nothing is allocated and nothing is charged: the pages belong to
    /// whoever pinned them, and this arena holds only the view.
    pub fn map(&mut self, physical: PhysicalRange) -> Result<PinnedFrame, PinError> {
        let (offset, bytes, _granule) = self.carve_for(physical.bytes)?;
        if bytes != physical.bytes {
            // A run whose owner rounded it to a different granule than
            // this instance maps at cannot be shared: the tail page
            // would carry bytes that are not the surface's.
            return Err(PinError::ShareRefused);
        }
        let virt = self.window.range_at(offset, bytes);
        (device_vm_hooks().map_shared)(virt, physical, PageFlags::READ | PageFlags::WRITE)
            .map_err(|_| PinError::ShareRefused)?;
        Ok(self.record(
            offset,
            bytes,
            PhysFrame::from_phys_addr(physical.start as usize),
            PinBacking::Shared,
        ))
    }

    /// Hand one run back.
    ///
    /// Its span returns to the arena only when it is the most recent
    /// one; otherwise the pages are released and the span stays held
    /// until the arena ends, exactly as a granted device's pinned rings
    /// are held for as long as the grant. Either way the instance's pool
    /// has its pages back on return — or, for a view, has lost its last
    /// path to somebody else's.
    ///
    /// # Panics
    ///
    /// Panics when the address space refuses to release the run. The
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

    /// Carve a span for a run of `bytes`, rounded up to the granule.
    fn carve_for(&mut self, bytes: u64) -> Result<(u64, u64, u64), PinError> {
        if bytes == 0 {
            return Err(PinError::Empty);
        }
        if self.frames.is_full() {
            return Err(PinError::TooMany);
        }
        let granule = self.granule();
        // `bytes` is derived from a geometry a guest named, so the
        // rounding is checked: an unchecked one wraps in a release build
        // and would answer a request for most of the address space with
        // a run of a few pages, which the display engine would then be
        // told to scan out.
        let bytes = bytes
            .checked_next_multiple_of(granule)
            .ok_or(PinError::WindowExhausted)?;
        let offset = self.carve(bytes, granule)?;
        Ok((offset, bytes, granule))
    }

    fn record(
        &mut self,
        offset: u64,
        bytes: u64,
        first: PhysFrame,
        kind: PinBacking,
    ) -> PinnedFrame {
        let frame = PinnedFrame {
            offset: self.window.offset() + offset,
            bytes,
            backing: PhysFrameRange {
                start: first,
                frame_count: usize::try_from(bytes / (PhysFrame::SIZE as u64))
                    .expect("a window span fits the address space"),
            },
            kind,
        };
        self.frames.push(frame);
        self.pinned_bytes += bytes;
        frame
    }

    /// The smallest unit the address space can change a mapping at,
    /// which is what every run is aligned and sized to.
    fn granule(&self) -> u64 {
        let granule = (device_vm_hooks().mapping_granule)();
        assert!(
            granule >= PhysFrame::SIZE as u64 && granule.is_power_of_two(),
            "an address space maps at a power-of-two granule of at least one frame, not {granule}"
        );
        granule
    }

    fn carve(&mut self, bytes: u64, align: u64) -> Result<u64, PinError> {
        let start = self
            .cursor
            .checked_next_multiple_of(align)
            .ok_or(PinError::WindowExhausted)?;
        let end = start.checked_add(bytes).ok_or(PinError::WindowExhausted)?;
        if end > self.window.bytes() {
            return Err(PinError::WindowExhausted);
        }
        self.cursor = end;
        Ok(start)
    }
}

impl<const CAPACITY: usize> Drop for PinnedFrames<CAPACITY> {
    fn drop(&mut self) {
        for frame in &self.frames {
            release(self.window, *frame);
        }
    }
}

/// Give one run's pages back to the instance's pool, or drop a view of
/// somebody else's.
///
/// # Panics
///
/// Panics when the address space refuses, for the reason
/// [`PinnedFrames::unpin`] documents.
fn release(window: DeviceWindow, frame: PinnedFrame) {
    let offset = frame.offset - window.offset();
    let virt = window.range_at(offset, frame.bytes);
    let hooks = device_vm_hooks();
    let released = match frame.kind {
        PinBacking::Owned { align } => (hooks.release_contiguous)(virt, align),
        PinBacking::Shared => (hooks.unmap_shared)(virt),
    };
    released.unwrap_or_else(|error| {
        panic!("a pinned run the address space would not release: {error}")
    });
}
