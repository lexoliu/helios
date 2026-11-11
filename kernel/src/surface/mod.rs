//! Windows on the desktop, and the two instances that share each one's
//! pixels.
//!
//! The kernel owns no compositor and draws nothing. What it owns is the
//! meeting point between a program that wants a window and the one
//! instance that composes the desktop:
//!
//! * [`service`] is the registry and the queues. It mints a surface
//!   identity, holds the events the compositor routed to it, and carries
//!   the identities of a dying client's windows. No trait object and no
//!   lock crosses the boundary into the compositor's store.
//! * [`instance`] is each instance's side: the windows a client holds,
//!   and the views the compositor holds of them.
//!
//! The compositor itself is a user-mode wasm program like any other. It
//! is reached through a [`crate::ProviderSlot`], the same write-once
//! typed hand-off the HTTP client plugin is reached through, and the
//! supervisor on the other end of that slot is what calls into it. Which
//! protocol names that slot carries is the adapter's business, not this
//! module's.
//!
//! # Where the pixels are
//!
//! In the client's linear memory and in the compositor's, at the same
//! time. One [`crate::Buffer`] is pinned from the client's pool and
//! shown at the span the client named; the compositor names a span of
//! its own memory when it is told about the window, and the same buffer
//! is shown there. A client writes its window with ordinary stores,
//! `commit` is one message, and the compositor composes from the bytes
//! the client wrote. The kernel never sees a pixel.
//!
//! # What a surface's pages cost, and when they come back
//!
//! They are the client's, charged to the client's pool, and they come
//! back when the last view of them ends. Each view holds a reference to
//! the buffer, so a client that drops a window, or dies, while the
//! compositor is still composing from it leaves the run pinned behind
//! the compositor's view until the supervisor ends that view; nothing
//! the pool hands out next can appear in front of the compositor.
//!
//! # Concurrency contract
//!
//! Stated per part: [`service`] for the registry and the queues,
//! [`instance`] for the store-side records, [`crate::buffer`] for the
//! frame buffers and views. The one rule that spans them is how a
//! client lets a window go — by dying, by dropping the resource, or by
//! cancelling a `create` the compositor was told about: each is a drop,
//! which cannot await anything, so the drop hands the windows'
//! identities to the compositor's supervisor and raises the return
//! signal. The supervisor ends the compositor's view of each and then
//! tells the compositor the window is gone; the buffer goes back to the
//! client's pool when that view's reference does.

mod instance;
mod service;
#[cfg(test)]
mod tests;

use thiserror::Error;

use crate::buffer::{BufferError, ViewError};

pub use instance::SurfaceOwnership;
pub use service::{
    MAX_INSTANCE_SURFACES, MAX_LIVE_SURFACES, ReturnedSurfaces, SURFACE_EVENT_QUEUE_DEPTH,
    SURFACE_REQUEST_QUEUE_DEPTH, SurfaceCreate, SurfaceEvents, SurfaceGeometry, SurfaceId,
    SurfaceRect, SurfaceRequest, SurfaceService, SurfaceShared,
};

/// Why a surface request was refused.
///
/// The variants are kept apart rather than folded into one fault because
/// a client acts on them differently: "this image ships no compositor"
/// is a provisioning answer, "your rectangle is outside your surface" is
/// a bug in the caller, and "the compositor died" is a thing to retry
/// with a new surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum SurfaceServiceError {
    /// This kernel image ships no compositor plugin, so no window can
    /// exist.
    #[error("this kernel image ships no compositor")]
    Unavailable,
    /// The compositor died and has not come back. Every surface it was
    /// composing died with it.
    #[error("the compositor is not running")]
    NoCompositor,
    /// A surface of that size has no pixels, or more of them than any
    /// memory could back.
    #[error("a surface of that size cannot be backed")]
    UnsupportedSize,
    /// This instance already holds as many surfaces as it may.
    #[error("an instance holds at most {MAX_INSTANCE_SURFACES} surfaces")]
    TooManySurfaces,
    /// The rectangle does not lie inside the surface it names.
    #[error("the region does not lie inside the surface")]
    OutOfBounds,
    /// The span is not whole pages inside the memory it names, is not
    /// the length the window needs, or overlaps something already shown
    /// there — the client's span, or the one the compositor answered
    /// `create` with.
    #[error("the window cannot be shown at that span: {0}")]
    BadSpan(ViewError),
    /// The compositor answered a request with its own `bad-span`, which
    /// carries no reason the kernel can name.
    #[error("the compositor refused a span")]
    CompositorBadSpan,
    /// The frame buffer could not be pinned.
    #[error("no contiguous run of memory left for a window")]
    OutOfMemory,
    /// A handle that outlived its surface.
    #[error("this surface is gone")]
    Gone,
    /// Somebody other than the compositor tried to deliver input to a
    /// surface.
    #[error("only the compositor may deliver input to a surface")]
    NotTheCompositor,
}

/// A frame buffer the pool refused, in the vocabulary a client acts on.
impl From<BufferError> for SurfaceServiceError {
    fn from(error: BufferError) -> Self {
        match error {
            BufferError::Empty | BufferError::TooLarge => Self::UnsupportedSize,
            // An address space that refuses a run it was asked for
            // correctly cannot back this window at all, and the client's
            // answer is the same as having no memory for it.
            BufferError::OutOfMemory | BufferError::AddressSpace(_) => Self::OutOfMemory,
        }
    }
}

/// A view the store refused, in the vocabulary a client acts on.
impl From<ViewError> for SurfaceServiceError {
    fn from(error: ViewError) -> Self {
        match error {
            ViewError::NoUnsharedMemory
            | ViewError::Misaligned
            | ViewError::OutOfBounds { .. }
            | ViewError::WrongLength { .. }
            | ViewError::Overlap => Self::BadSpan(error),
            ViewError::TooManyViews => Self::TooManySurfaces,
            // A window that cannot be shown for want of memory, or that
            // the address space refused at a span the table had
            // checked, cannot be backed here.
            ViewError::OutOfMemory { .. } | ViewError::AddressSpace(_) => Self::OutOfMemory,
            // A frame buffer is never a device region, and a surface ends
            // only views it recorded: either answer means the surface
            // table and the store's views disagree.
            ViewError::UnalignedRegion | ViewError::NotShown => {
                panic!("a surface's view answered {error}, which only a kernel defect produces")
            }
        }
    }
}
