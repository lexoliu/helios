//! Memory a program shares with hardware or with another program, and
//! the places it shows up.
//!
//! Two objects carry every zero-copy path a program has to the machine:
//!
//! * A [`Buffer`] is a pinned, physically contiguous run of frames taken
//!   from the user pool on behalf of the instance that asked for it. It
//!   has no address of its own. It is what a device is told to read and
//!   what a second instance is handed, and it lives until its last
//!   holder lets go: the creating instance's resource, a device
//!   attachment (a display resource's backing, a blob or command buffer
//!   the 3D engine still reads, a granted device's ring, a stream's
//!   period buffers), a compositor's view.
//! * A view is a buffer, or a device's own region, shown at a span of
//!   one instance's linear memory that the instance chose: whole wasm
//!   pages below `memory.size`. The address space substitutes the
//!   backing of that span in one step and puts zeroed memory back when
//!   the view ends. Views belong to the store's [`ViewTable`] and die
//!   with it, synchronously, before the runtime resets the memory slot;
//!   the buffer behind a view may outlive it for as long as a device
//!   still reads it.
//!
//! Every byte a program reaches through either is inside its own memory
//! by the runtime's own account, so a scalar access and a bulk one are
//! bounded identically.
//!
//! # Concurrency contract
//!
//! A [`ViewTable`] is store data: it is owned by the one task that runs
//! its instance, every operation on it runs on that task, and it takes
//! no lock. That task is also the only one that ever substitutes,
//! restores or withdraws a span of the instance's memory, which is the
//! caller-exclusivity `hal::vmm::AddressSpace::substitute_memory`
//! requires: a view is only taken on an unshared memory, so no other
//! processor runs code that reads the span while it changes. The
//! address space shoots every other processor down before each of those
//! calls returns.
//!
//! A [`Buffer`] is `Send + Sync` and is cloned and dropped from any
//! processor: by the store's task, by a device owner task on the
//! processor that brought the device up, by the compositor's task. The
//! count is a [`triomphe::Arc`]'s atomic, and whichever holder lets go
//! last frees the run on the processor it happens to be on. Freeing
//! changes no translation, so it takes no page-table lock and needs no
//! shootdown: every view of the run has already ended — each view holds
//! a reference — and every device that read it has already been told to
//! stop, because each device attachment holds one too. A buffer is
//! never dropped from interrupt context: the user pool's free path is
//! task context only.

#[cfg(test)]
mod tests;
mod view;

use helios_hal::device::DmaPlacement;
use helios_hal::iommu::PhysicalRange;
use helios_hal::pmm::{PhysFrame, PhysFrameRange};
use helios_hal::vmm::{AddressSpaceError, VirtAddr};
use thiserror::Error;
use triomphe::Arc;

use crate::device::device_vm_hooks;

pub use view::{GuestMemory, MAX_VIEWS, Span, ViewError, ViewGroup, ViewId, ViewTable};

/// Every pixel format a Helios display engine latches is four bytes
/// wide, so a frame's size is its area times this and its stride is its
/// width times this.
pub const BYTES_PER_PIXEL: usize = 4;

/// Bytes in one wasm page, which is the unit a span is counted in.
///
/// Fixed by the WebAssembly core specification (§2.5.4, "memory
/// instances": the page size is 64 KiB) for every memory without the
/// custom-page-sizes proposal, which no Helios engine enables.
pub const WASM_PAGE_BYTES: u64 = 64 * 1024;

/// Why a buffer could not be allocated.
///
/// Kept apart rather than folded into one fault because the owning
/// service answers each differently: an empty request is a bug in the
/// caller, a size whose rounding overflows is a request no memory could
/// back, and no memory is the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum BufferError {
    /// A buffer of no bytes, which describes no memory any device could
    /// read.
    #[error("a buffer of no bytes describes no memory")]
    Empty,
    /// The size, rounded up to the address space's granule, does not
    /// fit the address space.
    #[error("a buffer of that size cannot be described")]
    TooLarge,
    /// The machine has no contiguous run of that size left, or none the
    /// placement accepts.
    #[error("no contiguous run of memory left")]
    OutOfMemory,
    /// The address space refused the allocation for another reason.
    #[error("the address space refused the run: {0}")]
    AddressSpace(AddressSpaceError),
}

/// One run of frames, freed when its last holder lets go.
struct Run {
    physical: PhysicalRange,
    /// The alignment the run was allocated with: a contiguous run is one
    /// allocation, and the allocator is owed the layout it produced.
    align: u64,
}

impl Drop for Run {
    fn drop(&mut self) {
        (device_vm_hooks().free_run)(self.physical, self.align).unwrap_or_else(|error| {
            // The run's last holder is gone, so nothing can show it or
            // read it any more; an address space that will not take it
            // back has lost track of what it handed out, and the next
            // allocation could be served out of these frames twice.
            panic!(
                "a buffer of {} bytes at {:#x} the address space would not take back: {error}",
                self.physical.bytes, self.physical.start
            )
        });
    }
}

/// A pinned, physically contiguous run of frames with no address of its
/// own.
///
/// Cloning shares the run; the run goes back to the pool when the last
/// clone is dropped. See the module's concurrency contract for which
/// processors that may be.
#[derive(Clone)]
pub struct Buffer {
    run: Arc<Run>,
}

impl core::fmt::Debug for Buffer {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Buffer")
            .field("start", &self.run.physical.start)
            .field("bytes", &self.run.physical.bytes)
            .finish()
    }
}

impl Buffer {
    /// Take a zeroed run of at least `bytes` from the user pool, placed
    /// as `placement` requires.
    ///
    /// The run is charged the way every user-pool allocation is: to the
    /// instance whose turn the executing processor is in, which is the
    /// instance that asked. Its size is `bytes` rounded up to the
    /// address space's mapping granule, and its alignment is at least
    /// that granule, so any view of it changes whole mappings.
    pub fn allocate(bytes: u64, placement: DmaPlacement) -> Result<Self, BufferError> {
        if bytes == 0 {
            return Err(BufferError::Empty);
        }
        let granule = mapping_granule();
        // `bytes` derives from something a guest named — a mode, a
        // span, a blob size — so the rounding is checked: an unchecked
        // one wraps in a release build and would answer a request for
        // most of the address space with a run of a few pages, which
        // the hardware would then be told to read.
        let bytes = bytes
            .checked_next_multiple_of(granule)
            .ok_or(BufferError::TooLarge)?;
        let length = usize::try_from(bytes).map_err(|_| BufferError::TooLarge)?;
        let placement = DmaPlacement {
            align: placement.align.max(granule),
            limit: placement.limit,
        };
        let physical =
            (device_vm_hooks().allocate_run)(length, placement).map_err(|error| match error {
                AddressSpaceError::OutOfFrames => BufferError::OutOfMemory,
                other => BufferError::AddressSpace(other),
            })?;
        Ok(Self {
            run: Arc::new(Run {
                physical,
                align: placement.align,
            }),
        })
    }

    /// Take a zeroed run of at least `bytes` that a device is told to
    /// read by physical address.
    ///
    /// What bounds it is the machine's memory rather than a translation
    /// unit's window: the display engine and the sound device are told
    /// the run's physical address directly.
    pub fn pinned(bytes: u64) -> Result<Self, BufferError> {
        Self::allocate(
            bytes,
            DmaPlacement {
                align: mapping_granule(),
                limit: u64::MAX,
            },
        )
    }

    /// How many bytes the run covers, which is the request rounded up to
    /// the address space's mapping granule.
    pub fn bytes(&self) -> u64 {
        self.run.physical.bytes
    }

    /// The physical bytes behind the run.
    pub fn physical(&self) -> PhysicalRange {
        self.run.physical
    }

    /// The run as the frames a device's backing list names.
    pub fn frames(&self) -> PhysFrameRange {
        PhysFrameRange {
            start: PhysFrame::from_phys_addr(
                usize::try_from(self.run.physical.start)
                    .expect("a run the address space handed out fits its own addresses"),
            ),
            frame_count: usize::try_from(self.run.physical.bytes / PhysFrame::SIZE as u64)
                .expect("a run the address space handed out fits its own addresses"),
        }
    }

    /// Where the run appears in the kernel's own address space.
    ///
    /// A view of the run is in some instance's linear memory, which no
    /// device's DMA pool translates and which only that instance's task
    /// is looking at. The kernel's own alias is the address every
    /// backend's pool does translate, so it is the one a kernel-side
    /// producer writes through and the one a driver is handed.
    pub fn kernel_alias(&self) -> VirtAddr {
        (device_vm_hooks().kernel_alias)(self.frames().start)
    }

    /// Whether `other` shares this buffer's run.
    pub fn same_run(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.run, &other.run)
    }

    /// Keep the run out of circulation for the rest of the machine's
    /// run, on purpose.
    ///
    /// For a device attachment the device would not give back: a
    /// display engine that refused to destroy the resource this run
    /// backs may still be latching it, and a run freed under it would
    /// be handed to the next instance that asks while the device puts
    /// those bytes on the screen. The device is not trusted to have let
    /// go, so the attachment's reference is never dropped: the other
    /// holders — views, the creating instance's resource — still let go
    /// of theirs, and the run never reaches a count of zero. The bytes
    /// are lost to the pool, which is the cost of a device that
    /// misbehaved, and the caller says so at error level.
    pub fn leak(self) {
        core::mem::forget(self);
    }
}

/// The smallest unit the address space can change a mapping at, which
/// every buffer is sized and aligned to.
///
/// # Panics
///
/// Panics when the backend reports a granule that is not a power of two
/// of at least one frame, or that does not divide a wasm page: a view
/// at a finer granularity than the hardware maps would put two objects
/// in one page, and a granule a wasm page is not a multiple of could not
/// show anything at a span at all.
pub(crate) fn mapping_granule() -> u64 {
    let granule = (device_vm_hooks().mapping_granule)();
    assert!(
        granule >= PhysFrame::SIZE as u64
            && granule.is_power_of_two()
            && WASM_PAGE_BYTES.is_multiple_of(granule),
        "an address space maps at a power-of-two granule of at least one frame that divides a \
         wasm page, not {granule}"
    );
    granule
}
