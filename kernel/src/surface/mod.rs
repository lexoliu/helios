//! Windows on the desktop, and the two instances that share each one's
//! pixels.
//!
//! The kernel owns no compositor and draws nothing. What it owns is the
//! meeting point between a program that wants a window and the one
//! instance that composes the desktop:
//!
//! * [`service`] is the registry and the queues. It mints a surface
//!   identity, holds the events the compositor routed to it, and carries
//!   what a dying client hands back. No trait object and no lock crosses
//!   the boundary into the compositor's store.
//! * [`instance`] is a client's side: the pinned, physically contiguous
//!   pages its window lives in, inside its own linear memory, and the
//!   surfaces it holds.
//!
//! The compositor itself is a user-mode wasm program like any other. It
//! is reached through a [`crate::ProviderSlot`], the way
//! `wasi:http/client.send` reaches the `http-client` plugin, and the
//! supervisor on the other end of that slot is what calls into it.
//!
//! # Where the pixels are
//!
//! In the client's linear memory and in the compositor's, at the same
//! time. One physically contiguous run is committed from the client's
//! pool into the client's surface window, and the same run is mapped a
//! second time into the compositor's. A client writes its window with
//! ordinary stores, `commit` is one message, and the compositor composes
//! from the bytes the client wrote. The kernel never sees a pixel.
//!
//! # What a surface's pages cost, and when they come back
//!
//! They are the client's, charged to the client's pool, and they come
//! back when the client's arena ends — which is when the instance dies —
//! not when one surface of many is dropped. That is the rule
//! [`crate::PinnedFrames`] already states for a display frame buffer,
//! and it is here for the same reason: a span released while somebody
//! else still holds a view of it would put whatever the pool hands out
//! next in front of the compositor. A client that churns through windows
//! exhausts its own surface window and is told so.
//!
//! # Concurrency contract
//!
//! Stated per part: [`service`] for the registry and the queues,
//! [`instance`] for the arena. The one rule that spans them is the
//! release: a client's surfaces are let go by a drop, which cannot await
//! anything, so the drop hands the whole arena to the compositor's
//! supervisor and raises the return signal. The supervisor takes the
//! windows off the desktop, drops its views of their pages, and only
//! then lets the arena go, which is the point at which the pages are the
//! client's pool's again.

mod instance;
mod service;
#[cfg(test)]
mod tests;

use thiserror::Error;

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
    /// The instance's surface window has no room left.
    #[error("this instance's surface window has no room left")]
    WindowExhausted,
    /// The frame buffer could not be pinned, or the compositor could not
    /// be given a view of it.
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

impl From<crate::PinError> for SurfaceServiceError {
    fn from(error: crate::PinError) -> Self {
        match error {
            crate::PinError::Empty => Self::UnsupportedSize,
            crate::PinError::TooMany => Self::TooManySurfaces,
            crate::PinError::WindowExhausted => Self::WindowExhausted,
            // A machine that cannot hand the compositor a view of the
            // client's pages cannot compose the client's window at all,
            // and the client's answer is the same as having no memory
            // for it: this window cannot be backed here.
            crate::PinError::OutOfMemory | crate::PinError::ShareRefused => Self::OutOfMemory,
        }
    }
}
