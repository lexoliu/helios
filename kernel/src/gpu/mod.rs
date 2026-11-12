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
//! * [`instance`] is the claiming instance's side: the claim, and the set
//!   of views its command buffers, guest blobs and aperture placements
//!   are shown through, which ends when the claim does.
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
//! Nowhere the kernel owns. A command buffer or a guest blob's storage
//! is a [`crate::Buffer`] pinned from the claiming instance's own pool
//! and shown at a span of its linear memory the instance chose, so
//! writing one is ordinary stores and submitting it is one message. A
//! mapped host-3D blob is not this machine's memory at all: it is a
//! region of the aperture the display engine decodes, shown at a span
//! of the same instance's linear memory the way a granted device's
//! registers are.
//!
//! # Concurrency contract
//!
//! Stated per part: [`service`] for the claim word and the queues,
//! [`owner`] for the tasks, [`crate::buffer`] for the buffers and views.
//! The one rule that spans them is the release: a claim is let go by a
//! drop, which cannot await anything, so the renderer is neither held
//! nor free until the owner task has taken the resources back. The
//! memory takes care of itself: every request that has the device read
//! a buffer — a blob's creation, a submission — carries a reference to
//! it, the owner task's record of a guest blob holds one until the
//! device has destroyed the blob, and so a run goes back to the pool
//! only after the renderer has stopped reading it, on whichever
//! processor drops the last reference.

mod instance;
mod owner;
mod service;
#[cfg(test)]
mod tests;

use helios_hal::display::Gpu3dError;
use thiserror::Error;

use crate::buffer::{BufferError, ViewError};

/// Views one 3D claim is budgeted in its store's view table.
///
/// A view per command buffer, per guest-backed blob and per aperture
/// placement, for the contexts one plugin drives. It is this path's
/// share of [`crate::MAX_VIEWS`], which is what keeps the table an
/// allocation of a size the kernel chose.
pub const MAX_GPU_VIEWS: usize = 32;

/// Contexts one claim may open at once.
pub const MAX_GPU_CONTEXTS: usize = 8;

/// Blobs one claim may hold at once.
pub const MAX_GPU_BLOBS: usize = 32;

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
    /// This claim already shows [`MAX_GPU_VIEWS`] things in the
    /// instance's memory, or the store holds as many views as it may.
    #[error("a 3D claim shows at most {MAX_GPU_VIEWS} buffers in its instance's memory")]
    TooManyViews,
    /// The blob's parameters do not describe a resource this engine can
    /// create, or it names a context or blob this claim does not hold.
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
    /// The span is not whole pages inside the instance's memory, is not
    /// the length the request needs, or overlaps something already
    /// shown.
    #[error("nothing can be shown at that span: {0}")]
    BadSpan(ViewError),
    /// A span was given for a blob with no pages of its own, or left
    /// out for one that has them.
    #[error("a blob takes a span exactly when it has pages of its own")]
    BlobSpanMismatch,
    /// The blob's host storage is already shown in the instance's
    /// memory.
    #[error("the blob is already mapped")]
    AlreadyMapped,
    /// The blob's host storage is not shown in the instance's memory.
    #[error("the blob is not mapped")]
    NotMapped,
    /// The memory could not be pinned, or a span could not be given its
    /// memory back.
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

/// A buffer the pool refused, in the vocabulary a plugin acts on.
impl From<BufferError> for Gpu3dServiceError {
    fn from(error: BufferError) -> Self {
        match error {
            // A blob of no bytes is not a blob, and one whose size no
            // address space can describe is not one the device accepts.
            BufferError::Empty | BufferError::TooLarge => Self::InvalidBlob,
            BufferError::OutOfMemory => Self::OutOfMemory,
            // The address space refused a run the kernel described
            // correctly: a wiring fault rather than a shortage.
            BufferError::AddressSpace(_) => Self::DeviceFault,
        }
    }
}

/// A view the store refused, in the vocabulary a plugin acts on.
impl From<ViewError> for Gpu3dServiceError {
    fn from(error: ViewError) -> Self {
        match error {
            ViewError::NoUnsharedMemory
            | ViewError::Misaligned
            | ViewError::OutOfBounds { .. }
            | ViewError::WrongLength { .. }
            | ViewError::Overlap => Self::BadSpan(error),
            ViewError::TooManyViews => Self::TooManyViews,
            ViewError::OutOfMemory { .. } => Self::OutOfMemory,
            // An aperture placement the engine published off a frame
            // boundary is the engine describing it wrongly, and an
            // address space that refuses a span the table checked is a
            // wiring fault.
            ViewError::UnalignedRegion | ViewError::AddressSpace(_) => Self::DeviceFault,
            // A claim ends only views it recorded, through a path that
            // never converts the answer: the claim and the store's views
            // disagree.
            ViewError::NotShown => {
                panic!("a 3D claim's view answered {error}, which only a kernel defect produces")
            }
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
