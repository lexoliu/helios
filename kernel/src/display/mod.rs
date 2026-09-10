//! The machine's display, and the one instance that may draw on it.
//!
//! The kernel owns the display device — it has to, because a monitor
//! plugged in or resized announces itself through an interrupt somebody
//! must consume — and hands the *right to draw* to exactly one instance
//! at a time. That split is what the three parts here are:
//!
//! * [`owner`] holds the device. Three tasks: one following the
//!   topology, one serving the display engine's control queue, one
//!   serving its cursor queue.
//! * [`service`] is how everything else reaches those tasks: a claim
//!   word, two bounded queues, and the signal a claim's release travels
//!   on. No trait object and no lock crosses that boundary.
//! * [`pins`] is the claiming instance's side: the pinned, physically
//!   contiguous pages its frame buffers live in, inside its own linear
//!   memory, which are what the display engine is told to read.
//!
//! # Where the pixels are
//!
//! Nowhere the kernel owns. A surface's frame buffer is committed from
//! the user pool into the claiming instance's display window and handed
//! to the display engine as the backing store of its resource, so a
//! compositor writes a frame with ordinary stores and `present` is one
//! copy into the device's own copy of the resource plus one flush. The
//! kernel never sees a pixel, and a compositor that asks for a larger
//! surface grows its own accounting rather than the kernel's.
//!
//! # Concurrency contract
//!
//! Stated per part: [`service`] for the claim word and the queues,
//! [`owner`] for the tasks, [`pins`] for the arena. The one rule that
//! spans them is the release: a claim is let go by a drop, which cannot
//! await anything, so the display is neither held nor free until the
//! owner task has taken the resources back — and the pages themselves
//! are handed to that task rather than freed by the drop, because the
//! display engine may still be reading them when the instance dies.

mod instance;
mod owner;
mod pins;
mod service;
#[cfg(test)]
mod tests;

use helios_hal::display::MAX_SCANOUTS;
use thiserror::Error;

pub use instance::DisplayOwnership;
pub use owner::install_display_device;
pub use pins::{DisplayPins, MAX_PINNED_FRAMES, PinnedFrame};
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
    /// The instance's display window has no room left.
    #[error("this instance's display window has no room left")]
    WindowExhausted,
    /// The frame buffer could not be pinned.
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

/// Outputs one claim may point at something at once.
///
/// The bound is the device's: no display engine Helios targets drives
/// more heads than [`MAX_SCANOUTS`].
pub const MAX_CLAIMED_SCANOUTS: usize = MAX_SCANOUTS;
