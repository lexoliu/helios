//! The machine's 3D engine, and the one instance that may drive it.
//!
//! The same shape the display path has, over the other half of the same
//! hardware. The kernel owns the rendering side of the display engine
//! and hands the *right to render* to exactly one instance at a time:
//!
//! * [`owner`] holds the device. Two tasks: one serving everything that
//!   changes what the renderer holds, one serving the command streams a
//!   context submits.
//! * [`service`] is how everything else reaches those tasks: a claim
//!   word, two bounded queues, and the signal a claim's release travels
//!   on. No trait object and no lock crosses that boundary.
//! * [`instance`] is the claiming instance's side: the pinned pages its
//!   command buffers and guest blobs live in, and the windows the host's
//!   own storage is mapped into, both inside its own linear memory.
//!
//! # What the kernel does not do
//!
//! Interpret anything. A capability set is bytes the device produced and
//! the guest consumes; a command buffer is bytes the guest produced and
//! the renderer consumes. The kernel checks that a request names memory
//! its author owns, moves it, and says when a fence has passed.
//!
//! # Where the memory is
//!
//! Nowhere the kernel owns. A command buffer is pinned, physically
//! contiguous pages committed from the claiming instance's own pool into
//! its own linear memory, so writing one is ordinary stores and
//! submitting it is one message. A mapped host-3D blob is not this
//! machine's memory at all: it is a window the display engine decodes,
//! placed in the same instance's linear memory through the mechanism a
//! granted device's registers are mapped through.
//!
//! # Concurrency contract
//!
//! Stated per part: [`service`] for the claim word and the queues,
//! [`owner`] for the tasks, [`crate::pins`] for the arena. The one rule
//! that spans them is the release: a claim is let go by a drop, which
//! cannot await anything, so the renderer is neither held nor free until
//! the owner task has taken the resources back — and the memory itself
//! is handed to that task rather than freed by the drop, because the
//! host renderer may still be reading a guest blob when the instance
//! dies.

mod instance;
mod owner;
mod service;
#[cfg(test)]
mod tests;

use helios_hal::display::Gpu3dError;
use thiserror::Error;

use crate::pins::{PinError, PinnedFrames};

/// Pinned runs one 3D claim may hold at once.
///
/// A run per command buffer, a run per guest-backed blob and a window
/// per mapped host blob, for the contexts one plugin drives. The bound
/// is what keeps the arena a value on the store's own stack rather than
/// an allocation whose size a guest chooses.
pub const MAX_GPU_PINS: usize = 32;

/// Contexts one claim may open at once.
pub const MAX_GPU_CONTEXTS: usize = 8;

/// Blobs one claim may hold at once.
pub const MAX_GPU_BLOBS: usize = 32;

/// The arena one 3D claim pins its command buffers and maps its host
/// blobs in.
pub type GpuPins = PinnedFrames<MAX_GPU_PINS>;

pub use instance::Gpu3dOwnership;
pub use owner::install_gpu3d_device;
pub(crate) use service::{BlobSpec, Gpu3dRequest, SubmitRequest};
pub use service::{ContextRecord, Gpu3dClaim, Gpu3dSender, Gpu3dService};

/// Why a 3D request was refused.
///
/// Kept apart from [`crate::display::DisplayServiceError`] rather than
/// folded into it, for the reason the two hal contracts are apart: the
/// rendering half refuses for reasons the scanout half has no vocabulary
/// for, and a plugin that has to tell "this machine has no renderer"
/// from "your command buffer is outside your own memory" cannot do it
/// from a code that means "unsupported mode".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum Gpu3dServiceError {
    /// This machine has no display device, or the kernel never brought
    /// one up.
    #[error("this machine has no display device")]
    Unavailable,
    /// The display engine this machine has renders nothing.
    #[error("this machine's display engine offers no 3D support")]
    NoRenderer,
    /// Another instance holds the renderer, or its last owner's
    /// resources have not been handed back yet.
    #[error("another instance already holds the 3D engine")]
    AlreadyClaimed,
    /// A handle that outlived a reclaim names a renderer its instance no
    /// longer holds.
    #[error("this instance does not hold the 3D engine")]
    NotClaimed,
    /// The device carries no such capability set, or not at that
    /// version.
    #[error("this display engine carries no such capability set")]
    NoSuchCapset,
    /// The host renderer does not speak the context type that was asked
    /// for.
    #[error("the host renderer does not speak this context type")]
    UnsupportedContext,
    /// This claim already holds as many contexts as it may.
    #[error("a 3D claim holds at most {MAX_GPU_CONTEXTS} contexts")]
    TooManyContexts,
    /// This claim already holds as many blobs as it may.
    #[error("a 3D claim holds at most {MAX_GPU_BLOBS} blobs")]
    TooManyBlobs,
    /// The blob's parameters do not describe a resource this engine can
    /// create, or it is being mapped or unmapped in a state it is not
    /// in.
    #[error("the blob request does not describe a resource this engine can create")]
    InvalidBlob,
    /// The engine's host-visible aperture has no room left.
    #[error("the display engine's host-visible aperture has no room left")]
    ApertureExhausted,
    /// The submission names memory outside anything this claim pinned.
    #[error("the command buffer does not lie inside this claim's own memory")]
    OutOfBounds,
    /// A fence is a point on an increasing timeline, and this one does
    /// not come after the last one this context submitted.
    #[error("a fence must come after every fence this context has already submitted")]
    FenceNotIncreasing,
    /// The instance's 3D window has no room left.
    #[error("this instance's 3D window has no room left")]
    WindowExhausted,
    /// The memory could not be pinned or mapped.
    #[error("no contiguous run of memory left for a command buffer")]
    OutOfMemory,
    /// The display engine failed rather than refused.
    #[error("the display engine faulted")]
    DeviceFault,
    /// The kernel's 3D owner stopped serving requests, which happens
    /// only when the machine is going down.
    #[error("the kernel's 3D owner stopped serving requests")]
    Closed,
}

impl From<PinError> for Gpu3dServiceError {
    fn from(error: PinError) -> Self {
        match error {
            // A command buffer of no bytes is not a command buffer, and
            // neither is a blob of none.
            PinError::Empty => Self::InvalidBlob,
            PinError::TooMany => Self::TooManyBlobs,
            PinError::WindowExhausted => Self::WindowExhausted,
            PinError::OutOfMemory => Self::OutOfMemory,
            // A 3D claim asks for a second view of somebody else's run
            // only when it maps a window the display engine published,
            // and an address space that refuses one has been handed a
            // region the engine described wrongly.
            PinError::ShareRefused => Self::DeviceFault,
        }
    }
}

impl From<Gpu3dError> for Gpu3dServiceError {
    fn from(error: Gpu3dError) -> Self {
        match error {
            Gpu3dError::Unsupported => Self::NoRenderer,
            Gpu3dError::UnknownCapset { .. } => Self::NoSuchCapset,
            Gpu3dError::OutOfMemory => Self::OutOfMemory,
            Gpu3dError::ApertureExhausted => Self::ApertureExhausted,
            Gpu3dError::InvalidBlob | Gpu3dError::NotMappable(_) | Gpu3dError::InvalidParameter => {
                Self::InvalidBlob
            }
            // A submission too long for the wire's command-buffer field
            // is a bounds refusal: the WIT names it `out-of-bounds`, the
            // same word a range past a resource's end gets.
            Gpu3dError::CommandBufferLength { .. } => Self::OutOfBounds,
            Gpu3dError::TooMany { .. } => Self::TooManyBlobs,
            // Everything left is the display engine failing rather than
            // refusing: a context or a blob the kernel's own bookkeeping
            // named and the device does not have, a caching contract the
            // kernel cannot keep, a code that belongs to no request, a
            // transport fault. A plugin cannot do anything about any of
            // them beyond giving the renderer back.
            Gpu3dError::UnknownContext(_)
            | Gpu3dError::UnknownBlob(_)
            | Gpu3dError::UnsupportedCaching { .. }
            | Gpu3dError::UnexpectedResponse { .. }
            | Gpu3dError::Unspecified
            | Gpu3dError::TooManyBackingRanges { .. }
            | Gpu3dError::Transport(_) => Self::DeviceFault,
        }
    }
}
