//! The views one instance's linear memory carries.
//!
//! See the parent module for what a view is and its concurrency
//! contract: the table is store data, every call runs on the store's own
//! task, and it never locks.

use alloc::vec::Vec;

use helios_hal::device::DeviceRegion;
use helios_hal::vmm::{AddressSpaceError, PageFlags, VirtAddr, VirtRange};
use thiserror::Error;

use crate::device::device_vm_hooks;

use super::{Buffer, WASM_PAGE_BYTES};

/// Views one store may hold at once.
///
/// The sum of what each path that shows something bounds itself to: a
/// display claim's frame buffers, the windows a compositor shows (a
/// client holds fewer), a 3D claim's command buffers, blobs and
/// aperture placements, and a granted device's regions and rings. The
/// bound is what keeps the table's allocation — kernel heap a guest's
/// requests drive — a size the kernel chose rather than one a guest
/// did.
pub const MAX_VIEWS: usize = crate::display::MAX_PINNED_FRAMES
    + crate::surface::MAX_LIVE_SURFACES
    + crate::gpu::MAX_GPU_VIEWS
    + crate::device::MAX_GRANT_REGIONS
    + crate::device::MAX_DMA_BUFFERS;

/// A run of whole wasm pages of the calling instance's linear memory, as
/// the guest named it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the first page.
    pub offset: u64,
    /// How many bytes the run covers.
    pub length: u64,
}

impl Span {
    pub const fn new(offset: u64, length: u64) -> Self {
        Self { offset, length }
    }

    /// Whether this span and `other` share a byte.
    const fn overlaps(self, other: Self) -> bool {
        // Both ends are already known not to overflow: a span is only
        // recorded once its end has been computed against the memory.
        self.offset < other.offset + other.length && other.offset < self.offset + self.length
    }
}

/// The calling instance's linear memory as it stands at the call.
///
/// Read from the runtime on every call that takes a span, never cached:
/// what a span is checked against is the memory's *current* size, which
/// the guest grows whenever it likes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestMemory {
    base: VirtAddr,
    bytes: u64,
}

impl GuestMemory {
    /// The memory based at `base`, `bytes` long right now.
    pub const fn new(base: VirtAddr, bytes: u64) -> Self {
        Self { base, bytes }
    }

    pub const fn base(&self) -> VirtAddr {
        self.base
    }

    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Why a view could not be shown or ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum ViewError {
    /// The instance has no single unshared linear memory: its component
    /// lowers through a shared (threads) memory, through more than one
    /// memory, or through none. A shared memory is read by other
    /// processors while a substitution would rewrite it, which the
    /// address space's contract forbids, so nothing is ever shown in
    /// one.
    #[error("this instance has no unshared linear memory to show anything in")]
    NoUnsharedMemory,
    /// The span is empty, or its offset or length is not a whole number
    /// of wasm pages.
    #[error("a span is a non-empty run of whole wasm pages")]
    Misaligned,
    /// The span reaches past the memory's current size.
    #[error("the span reaches past the memory's current size of {memory_bytes} bytes")]
    OutOfBounds { memory_bytes: u64 },
    /// The span is not the length what is shown requires.
    #[error("the span is {given} bytes and what is shown needs exactly {expected}")]
    WrongLength { expected: u64, given: u64 },
    /// The span overlaps a view this memory already carries.
    #[error("the span overlaps something already shown in this memory")]
    Overlap,
    /// The store already holds [`MAX_VIEWS`] views.
    #[error("a store holds at most {MAX_VIEWS} views")]
    TooManyViews,
    /// A device region that does not start and end on a frame boundary,
    /// which a view would have to share a page with whatever the device
    /// decodes next to it.
    #[error("the device region does not start and end on a frame boundary")]
    UnalignedRegion,
    /// No live view has this identity.
    #[error("no such view is shown in this memory")]
    NotShown,
    /// Ending the view needs `bytes` of fresh frames to put back, and
    /// the pool has none. The view is still shown: the instance is out
    /// of memory, and what follows is what follows any user OOM.
    #[error("no memory left to put {bytes} bytes back behind the view")]
    OutOfMemory { bytes: u64 },
    /// The address space refused the substitution.
    #[error("the address space refused the view: {0}")]
    AddressSpace(AddressSpaceError),
}

impl ViewError {
    /// Whether this is a refusal of the span itself — the interface's
    /// `bad-span` — rather than of the store's capacity or the machine.
    pub const fn is_bad_span(self) -> bool {
        matches!(
            self,
            Self::NoUnsharedMemory
                | Self::Misaligned
                | Self::OutOfBounds { .. }
                | Self::WrongLength { .. }
                | Self::Overlap
        )
    }
}

/// The identity of one live view, unique within its table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewId(u64);

/// A set of views that end together.
///
/// A granted device's regions and rings are shown for as long as the
/// grant lives, and a 3D claim's aperture placements for as long as the
/// claim does; both end every view in the set at once when the holder
/// lets go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewGroup(u64);

/// One live view.
struct Shown {
    id: ViewId,
    group: Option<ViewGroup>,
    /// The span the guest named.
    span: Span,
    /// The part of it whose backing was substituted, which is what
    /// is shown rounded up to the address space's granule.
    substituted: VirtRange,
    /// The run shown there, held for as long as the view lives; absent
    /// for a device region, which was never anyone's to free.
    _buffer: Option<Buffer>,
}

/// The views one store's linear memory carries.
///
/// Dropping the table withdraws every view, leaving the spans
/// uncommitted: that is the path a dying store takes, and the runtime
/// resets the memory slot only after it.
pub struct ViewTable {
    views: Vec<Shown>,
    next: u64,
}

impl Default for ViewTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewTable {
    pub const fn new() -> Self {
        Self {
            views: Vec::new(),
            next: 0,
        }
    }

    /// How many views are live.
    pub fn len(&self) -> usize {
        self.views.len()
    }

    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    /// Whether `id` is a live view.
    pub fn is_shown(&self, id: ViewId) -> bool {
        self.views.iter().any(|view| view.id == id)
    }

    /// How many live views are in `group`.
    pub fn group_len(&self, group: ViewGroup) -> usize {
        self.views
            .iter()
            .filter(|view| view.group == Some(group))
            .count()
    }

    /// A fresh set for views that end together.
    pub fn group(&mut self) -> ViewGroup {
        self.next += 1;
        ViewGroup(self.next)
    }

    /// Check `span` against `memory` for something `object_bytes` long,
    /// and answer where it starts in the address space.
    ///
    /// Everything the interface's span rule says: whole wasm pages, not
    /// empty, inside the memory's current size, exactly the object's
    /// size rounded up to wasm pages, and overlapping no live view.
    /// Called before anything is allocated for the object, and again by
    /// the `show` calls themselves.
    pub fn validate(
        &self,
        memory: GuestMemory,
        span: Span,
        object_bytes: u64,
    ) -> Result<VirtAddr, ViewError> {
        if span.length == 0
            || !span.offset.is_multiple_of(WASM_PAGE_BYTES)
            || !span.length.is_multiple_of(WASM_PAGE_BYTES)
        {
            return Err(ViewError::Misaligned);
        }
        let end = span
            .offset
            .checked_add(span.length)
            .filter(|end| *end <= memory.bytes)
            .ok_or(ViewError::OutOfBounds {
                memory_bytes: memory.bytes,
            })?;
        let expected = object_bytes
            .checked_next_multiple_of(WASM_PAGE_BYTES)
            .filter(|expected| *expected != 0)
            .ok_or(ViewError::WrongLength {
                expected: 0,
                given: span.length,
            })?;
        if span.length != expected {
            return Err(ViewError::WrongLength {
                expected,
                given: span.length,
            });
        }
        if self.views.iter().any(|view| view.span.overlaps(span)) {
            return Err(ViewError::Overlap);
        }
        if self.views.len() >= MAX_VIEWS {
            return Err(ViewError::TooManyViews);
        }
        let start = (memory.base.raw() as u64)
            .checked_add(span.offset)
            .and_then(|start| usize::try_from(start).ok())
            .ok_or(ViewError::OutOfBounds {
                memory_bytes: memory.bytes,
            })?;
        debug_assert!(end <= memory.bytes);
        Ok(VirtAddr::new(start))
    }

    /// Show `buffer` at `span` of `memory`, in place of what the guest
    /// had there.
    ///
    /// Only the buffer's own pages are substituted; where the buffer
    /// ends before the span does, the rest of the span stays the
    /// program's memory. The view holds a reference to the buffer until
    /// it ends.
    pub fn show_buffer(
        &mut self,
        memory: GuestMemory,
        span: Span,
        buffer: &Buffer,
        group: Option<ViewGroup>,
    ) -> Result<ViewId, ViewError> {
        let start = self.validate(memory, span, buffer.bytes())?;
        let substituted = VirtRange::new(start, to_usize(buffer.bytes()));
        (device_vm_hooks().substitute_memory)(
            substituted,
            buffer.physical(),
            PageFlags::READ | PageFlags::WRITE,
        )
        .map_err(ViewError::AddressSpace)?;
        Ok(self.record(group, span, substituted, Some(buffer.clone())))
    }

    /// Show `region` — a device's own bytes — at `span` of `memory`, in
    /// place of what the guest had there, with the attributes the region
    /// carries.
    pub fn show_device(
        &mut self,
        memory: GuestMemory,
        span: Span,
        region: DeviceRegion,
        group: Option<ViewGroup>,
    ) -> Result<ViewId, ViewError> {
        if !region.is_frame_aligned() {
            return Err(ViewError::UnalignedRegion);
        }
        let start = self.validate(memory, span, region.physical.bytes)?;
        let substituted = VirtRange::new(start, to_usize(region.physical.bytes));
        (device_vm_hooks().substitute_device)(substituted, region)
            .map_err(ViewError::AddressSpace)?;
        Ok(self.record(group, span, substituted, None))
    }

    /// End the view `id`: the span becomes ordinary zeroed memory again,
    /// with the permissions it had, and the view's reference to its
    /// buffer goes.
    ///
    /// # Panics
    ///
    /// Panics when the address space says the span is not substituted.
    /// The table recorded it as shown, so the two disagree about what
    /// backs the instance's memory, and the instance's next access would
    /// reach whatever the address space thinks is there.
    pub fn end(&mut self, id: ViewId) -> Result<(), ViewError> {
        let index = self
            .views
            .iter()
            .position(|view| view.id == id)
            .ok_or(ViewError::NotShown)?;
        restore(self.views[index].substituted)?;
        self.views.swap_remove(index);
        Ok(())
    }

    /// End every view in `group`.
    ///
    /// Stops at the first view the pool cannot refill, leaving it and
    /// every view after it shown, for the reason [`Self::end`] does.
    pub fn end_group(&mut self, group: ViewGroup) -> Result<(), ViewError> {
        while let Some(index) = self.views.iter().position(|view| view.group == Some(group)) {
            restore(self.views[index].substituted)?;
            self.views.swap_remove(index);
        }
        Ok(())
    }

    fn record(
        &mut self,
        group: Option<ViewGroup>,
        span: Span,
        substituted: VirtRange,
        buffer: Option<Buffer>,
    ) -> ViewId {
        self.next += 1;
        let id = ViewId(self.next);
        self.views.push(Shown {
            id,
            group,
            span,
            substituted,
            _buffer: buffer,
        });
        id
    }
}

impl Drop for ViewTable {
    /// The dying store's path: every span is left uncommitted, which
    /// allocates nothing and so cannot fail for want of memory, and the
    /// views' references to their buffers go with them.
    ///
    /// # Panics
    ///
    /// Panics when the address space refuses, for the reason
    /// [`ViewTable::end`] does: the runtime is about to hand this memory
    /// slot to the next instance, and a substitution it does not know
    /// about would hand that instance a device's memory.
    fn drop(&mut self) {
        for view in self.views.drain(..) {
            (device_vm_hooks().withdraw)(view.substituted).unwrap_or_else(|error| {
                panic!(
                    "a view at {:#x} the address space would not withdraw: {error}",
                    view.substituted.start.raw()
                )
            });
        }
    }
}

/// Put zeroed memory back behind `substituted`.
fn restore(substituted: VirtRange) -> Result<(), ViewError> {
    match (device_vm_hooks().restore)(substituted) {
        Ok(()) => Ok(()),
        Err(AddressSpaceError::OutOfFrames) => Err(ViewError::OutOfMemory {
            bytes: substituted.byte_len as u64,
        }),
        Err(error) => panic!(
            "a view at {:#x} the address space would not end: {error}",
            substituted.start.raw()
        ),
    }
}

fn to_usize(bytes: u64) -> usize {
    usize::try_from(bytes).expect("an object already allocated or published fits the address space")
}
