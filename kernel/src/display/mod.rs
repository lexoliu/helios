//! The machine's display, and the one instance that may draw on it.
//!
//! The kernel owns the display device — it has to, because a monitor
//! plugged in or resized announces itself through an interrupt somebody
//! must consume — and hands the *right to draw* to exactly one instance
//! at a time. That split is what the parts here are:
//!
//! * [`owner`] holds the device. Three tasks: one following the
//!   topology, one serving the display engine's control queue, one
//!   serving its cursor queue.
//! * [`service`] is how everything else reaches those tasks: a claim
//!   word, two bounded queues, and the signal a claim's release travels
//!   on. No trait object and no lock crosses that boundary.
//! * [`instance`] is the claiming instance's side: the claim its store
//!   holds.
//!
//! # Where the pixels are
//!
//! Nowhere the kernel owns. A surface's frame buffer is a
//! [`crate::Buffer`] pinned from the claiming instance's own pool and
//! shown at a span of its linear memory the compositor chose; the same
//! run is the backing store of the display engine's resource, so a
//! compositor writes a frame with ordinary stores — `memory.copy` and
//! `memory.fill` included — and `present` is one copy into the device's
//! own copy of the resource plus one flush. The kernel never sees a
//! pixel, and a compositor that asks for a larger surface grows its own
//! accounting rather than the kernel's.
//!
//! # Concurrency contract
//!
//! Stated per part: [`service`] for the claim word and the queues,
//! [`owner`] for the tasks, [`crate::buffer`] for the frame buffers. The
//! one rule that spans them is the release: a claim is let go by a
//! drop, which cannot await anything, so the display is neither held
//! nor free until the owner task has taken the resources back. The
//! owner task's record of each resource holds a reference to its frame
//! buffer, and drops it only after the display engine has destroyed the
//! resource, so a frame whose instance has died is freed by the owner
//! task, after the device let go, on the owner task's processor.

mod instance;
mod owner;
mod service;
#[cfg(test)]
mod tests;

use helios_hal::display::MAX_SCANOUTS;
use thiserror::Error;

use crate::buffer::{BufferError, ViewError};

pub use instance::DisplayOwnership;
pub use owner::install_display_device;

/// Frame buffers one claim may hold at once, cursor planes included.
///
/// A compositor builds its surfaces once and presents into them; the
/// bound is what keeps the owner task's record of them a value of fixed
/// size rather than an allocation whose size a guest chooses.
pub const MAX_PINNED_FRAMES: usize = 16;

pub(crate) use service::{ControlRequest, CursorRequest};
pub use service::{
    DisplayClaim, DisplaySender, DisplayService, FrameToken, REQUEST_QUEUE_DEPTH, SequenceSignal,
};

/// Why a display request was refused.
///
/// The variants are kept apart rather than folded into one fault
/// because a compositor acts on them differently: "somebody else has
/// the display" is a provisioning answer, "your rectangle is outside
/// your surface" is a bug in the caller, and "the display engine
/// answered with something that does not belong to this request" is a
/// device fault nobody can retry around.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum DisplayServiceError {
    /// This machine has no display device, or the kernel never brought
    /// one up.
    #[error("this machine has no display device")]
    Unavailable,
    /// Another instance holds the display, or its last owner's
    /// resources have not been handed back yet.
    #[error("another instance already holds the display")]
    AlreadyClaimed,
    /// A handle that outlived a reclaim names a display its instance no
    /// longer holds.
    #[error("this instance does not hold the display")]
    NotClaimed,
    /// No such output on this device.
    #[error("scanout {0} does not exist on this device")]
    NoSuchScanout(u32),
    /// The mode or pixel format is not one this display engine drives,
    /// or the frame it describes is larger than any memory could back.
    #[error("the display engine does not drive this mode")]
    UnsupportedMode,
    /// The rectangle does not lie inside the surface it names.
    #[error("the region does not lie inside the surface")]
    OutOfBounds,
    /// This claim already holds as many frame buffers as it may.
    #[error("a display claim holds at most {MAX_PINNED_FRAMES} frame buffers")]
    TooManySurfaces,
    /// The span is not whole pages inside the instance's memory, is not
    /// the length the frame needs, or overlaps something already shown.
    #[error("the frame buffer cannot be shown at that span: {0}")]
    BadSpan(ViewError),
    /// The frame buffer could not be pinned, or the span could not be
    /// given its memory back.
    #[error("no contiguous run of memory left for a frame buffer")]
    OutOfMemory,
    /// The display engine failed rather than refused.
    #[error("the display engine faulted")]
    DeviceFault,
    /// The kernel's display owner stopped serving requests, which
    /// happens only when the machine is going down.
    #[error("the kernel's display owner stopped serving requests")]
    Closed,
}

/// A frame buffer the pool refused, in the vocabulary a compositor
/// acts on.
impl From<BufferError> for DisplayServiceError {
    fn from(error: BufferError) -> Self {
        match error {
            // A mode with no pixels in it, or more than any address
            // space can describe, is a mode no display engine drives,
            // which is the answer a compositor can act on.
            BufferError::Empty | BufferError::TooLarge => Self::UnsupportedMode,
            BufferError::OutOfMemory => Self::OutOfMemory,
            // The address space refused a run the kernel described
            // correctly: a wiring fault rather than a shortage.
            BufferError::AddressSpace(_) => Self::DeviceFault,
        }
    }
}

/// A view the store refused, in the vocabulary a compositor acts on.
impl From<ViewError> for DisplayServiceError {
    fn from(error: ViewError) -> Self {
        match error {
            ViewError::NoUnsharedMemory
            | ViewError::Misaligned
            | ViewError::OutOfBounds { .. }
            | ViewError::WrongLength { .. }
            | ViewError::Overlap => Self::BadSpan(error),
            ViewError::TooManyViews => Self::TooManySurfaces,
            ViewError::OutOfMemory { .. } => Self::OutOfMemory,
            // The address space refused a substitution at a span the
            // table had already checked: a wiring fault rather than
            // anything the compositor asked for.
            ViewError::AddressSpace(_) => Self::DeviceFault,
            // A frame buffer is memory, never a device region, and
            // showing one never names a view that already exists: either
            // answer means the display path and the store's views
            // disagree.
            ViewError::UnalignedRegion | ViewError::NotShown => {
                panic!(
                    "a display frame buffer's view answered {error}, which only a kernel defect \
                     produces"
                )
            }
        }
    }
}

/// Outputs one claim may point at something at once.
///
/// The bound is the device's: no display engine Helios targets drives
/// more heads than [`MAX_SCANOUTS`].
pub const MAX_CLAIMED_SCANOUTS: usize = MAX_SCANOUTS;
