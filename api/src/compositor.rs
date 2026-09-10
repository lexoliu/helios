//! Export side of `helios:system/compositor`, for the one component that
//! composes the desktop.
//!
//! A compositor implements [`Guest`] and hands its type to
//! `helios_api::bindings::export!`. The kernel calls `run` once, as a
//! task of the compositor's own store, and `create`, `commit` and
//! `destroy` whenever a client asks for a window, publishes pixels in
//! one, or lets one go — all of them concurrently with `run`.
//!
//! The mirror of [`crate::http::handler`]: `helios:system/surface` in
//! the kernel is a forwarder, and this is what it forwards to.

pub use crate::bindings::exports::helios::system::compositor::{
    Error, Guest, Placement, Rect, SurfaceId,
};
