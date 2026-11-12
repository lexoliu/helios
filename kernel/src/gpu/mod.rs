//! The machine's 3D engine, and the instances that render on it.
//!
//! The same shape the display path has, over the other half of the same
//! hardware, with one difference: the renderer multiplexes contexts by
//! itself, so the kernel hands out no exclusive right to it. Any number
//! of instances open a *session* on the engine at once, and each owns
//! exactly the contexts, blobs, views and aperture placements it made:
//!
//! * [`owner`] holds the device. Two tasks: one serving everything that
//!   changes what the renderer holds, for every session, and one
//!   posting the command streams every context submits.
//! * [`service`] is how everything else reaches those tasks: two bounded
//!   queues, one record per session, and the signal a session's end
//!   travels on. No trait object and no lock crosses that boundary.
//! * [`instance`] is one instance's side: its session, and the set of
//!   views its command buffers, guest blobs and aperture placements are
//!   shown through, which ends when the session does.
//!
//! # What the kernel does not do
//!
//! Interpret anything. A capability set is bytes the device produced and
//! the guest consumes; a command buffer is bytes the guest produced and
//! the renderer consumes. The kernel checks that a request names memory
//! its author owns and resources its session made, moves it, and says
//! when a fence has passed.
//!
//! # Where the memory is
//!
//! Nowhere the kernel owns. A command buffer or a guest blob's storage
//! is a [`crate::Buffer`] pinned from the instance's own pool and shown
//! at a span of its linear memory the instance chose, so writing one is
//! ordinary stores and submitting it is one message. A mapped host-3D
//! blob is not this machine's memory at all: it is a region of the
//! aperture the display engine decodes, shown at a span of the same
//! instance's linear memory the way a granted device's registers are,
//! under the memory type the host asked for.
//!
//! # Concurrency contract
//!
//! Stated per part: [`service`] for the sessions and the queues,
//! [`owner`] for the tasks, [`crate::buffer`] for the buffers and views.
//! The one rule that spans them is the release: a session ends by a
//! drop, which cannot await anything, so what it made is neither held
//! nor free until the owner task has taken it back. The memory takes
//! care of itself: every request that has the device read a buffer — a
//! blob's creation, a submission — carries a reference to it, the owner
//! task's record of a guest blob holds one until the device has
//! destroyed the blob, and so a run goes back to the pool only after the
//! renderer has stopped reading it, on whichever processor drops the
//! last reference.

mod instance;
mod owner;
mod service;
#[cfg(test)]
mod tests;

use helios_hal::display::{Gpu3dError, Gpu3dTable};
use thiserror::Error;

use crate::buffer::{BufferError, ViewError};

/// Views one session is budgeted in its store's view table.
///
/// A view per command buffer, per guest-backed blob and per aperture
/// placement, for the contexts one plugin drives. It is this path's
/// share of [`crate::MAX_VIEWS`], which is what keeps the table an
/// allocation of a size the kernel chose.
pub const MAX_GPU_VIEWS: usize = 32;

/// Contexts one instance may hold at once.
pub const MAX_GPU_CONTEXTS: usize = 8;

/// Blobs one instance may hold at once.
pub const MAX_GPU_BLOBS: usize = 32;

/// Contexts the whole machine's sessions may hold at once: the capacity
/// of the owner task's context table.
///
/// Four instances at their own bound. The figure is what keeps the
/// table a value; a machine whose renderers want more is one whose
/// owner task should be given a larger table, not one where an instance
/// is told its own bound when the machine's was hit.
pub const MAX_MACHINE_GPU_CONTEXTS: usize = 4 * MAX_GPU_CONTEXTS;

/// Blobs the whole machine's sessions may hold at once: the capacity of
/// the owner task's blob table, four instances at their own bound.
pub const MAX_MACHINE_GPU_BLOBS: usize = 4 * MAX_GPU_BLOBS;

/// Bytes one `execute` carries inline.
///
/// `execute` exists for the short streams a renderer sends outside its
/// command buffers: Venus's ring-setup calls run to tens of bytes, and
/// Mesa encodes `vkCreateRingMESA` — the largest of them — into a
/// 256-byte stack buffer before it submits it (`vn_ring_create` in
/// `src/virtio/vulkan/vn_ring.c`). A stream longer than that belongs in
/// a command buffer.
pub const MAX_EXECUTE_BYTES: usize = 256;

pub use instance::Gpu3dOwnership;
pub use owner::install_gpu3d_device;
pub use service::{
    BlobShared, ContextRecord, ContextShared, Gpu3dSender, Gpu3dService, Gpu3dSession,
    RingTimeline, ScanoutHold, SessionId,
};
pub(crate) use service::{BlobSpec, Gpu3dRequest, InlineCommands, SubmitCommands, SubmitRequest};

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
    /// The handle was made under a session its instance has since ended
    /// by dropping its last `gpu` handle; everything that session made
    /// has been handed back.
    #[error("the gpu session this handle was made under has ended")]
    SessionEnded,
    /// The device carries no such capability set, or not at that
    /// version.
    #[error("this display engine carries no such capability set")]
    NoSuchCapset,
    /// The host renderer does not speak the context type that was asked
    /// for.
    #[error("the host renderer does not speak this context type")]
    UnsupportedContext,
    /// This instance already holds as many contexts as it may.
    #[error("an instance holds at most {MAX_GPU_CONTEXTS} contexts")]
    TooManyContexts,
    /// This instance already holds as many blobs as it may.
    #[error("an instance holds at most {MAX_GPU_BLOBS} blobs")]
    TooManyBlobs,
    /// Every instance's contexts together fill the machine's table.
    #[error("the machine's renderers hold at most {MAX_MACHINE_GPU_CONTEXTS} contexts")]
    ContextsExhausted,
    /// Every instance's blobs together fill the machine's table.
    #[error("the machine's renderers hold at most {MAX_MACHINE_GPU_BLOBS} blobs")]
    BlobsExhausted,
    /// This session already shows [`MAX_GPU_VIEWS`] things in the
    /// instance's memory, or the store holds as many views as it may.
    #[error("a gpu session shows at most {MAX_GPU_VIEWS} buffers in its instance's memory")]
    TooManyViews,
    /// The blob's parameters do not describe a resource this engine can
    /// create, or it names a context or blob this instance does not
    /// hold.
    #[error("the blob request does not describe a resource this engine can create")]
    InvalidBlob,
    /// The engine's host-visible aperture has no room left.
    #[error("the display engine's host-visible aperture has no room left")]
    ApertureExhausted,
    /// The submission names memory outside the command buffer it names,
    /// or an inline stream longer than [`MAX_EXECUTE_BYTES`].
    #[error("the command stream does not lie inside what may carry it")]
    OutOfBounds,
    /// The ring count is not one to sixty-four, or the ring is not one
    /// of the context's.
    #[error("no such ring on this context")]
    BadRing,
    /// A fence is a point on an increasing timeline, and this one does
    /// not come after the last one this context submitted on its ring.
    #[error("a fence must come after every fence already submitted on its ring")]
    StaleFence,
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
    /// The display engine failed rather than refused, or refused a
    /// command of this context after its submission had resolved: the
    /// context is faulted and answers this to every later call.
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
            // A session ends only views it recorded, through a path that
            // never converts the answer: the session and the store's
            // views disagree.
            ViewError::NotShown => {
                panic!("a gpu session's view answered {error}, which only a kernel defect produces")
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
            Gpu3dError::UnknownRing { .. } => Self::BadRing,
            // A submission too long for the wire's command-buffer field
            // is a bounds refusal: the WIT names it `out-of-bounds`, the
            // same word a range past a resource's end gets.
            Gpu3dError::CommandBufferLength { .. } => Self::OutOfBounds,
            // The driver's own tables are the machine's, shared by every
            // session, so filling one is the machine-wide bound.
            Gpu3dError::TooMany {
                table: Gpu3dTable::Contexts,
                ..
            } => Self::ContextsExhausted,
            Gpu3dError::TooMany {
                table: Gpu3dTable::Blobs,
                ..
            } => Self::BlobsExhausted,
            // Everything left is the display engine failing rather than
            // refusing: more capability sets than any renderer
            // publishes, a context or a blob the kernel's own
            // bookkeeping named and the device does not have, a caching
            // mode the specification gives no meaning to, a code that
            // belongs to no request, a transport fault. A plugin cannot
            // do anything about any of them beyond letting the context
            // go.
            Gpu3dError::TooMany {
                table: Gpu3dTable::Capsets,
                ..
            }
            | Gpu3dError::UnknownContext(_)
            | Gpu3dError::UnknownBlob(_)
            | Gpu3dError::UnknownCaching { .. }
            | Gpu3dError::UnexpectedResponse { .. }
            | Gpu3dError::Unspecified
            | Gpu3dError::TooManyBackingRanges { .. }
            | Gpu3dError::Transport(_) => Self::DeviceFault,
        }
    }
}
