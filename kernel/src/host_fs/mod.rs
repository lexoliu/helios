//! Host filesystem client and supporting types.
//!
//! `client` implements the in-kernel 9p client used to drive a virtio
//! host share, and `cache` holds the bounded, time-limited tables that
//! keep it from re-walking the share for every question. `share` exposes the mount tag and guest-side path
//! helpers shared with the wasmtime host bindings. `unsupported`
//! provides a stub `HostFileSystem` that backends without a
//! virtio-9p device can use to satisfy the `HostFs` type parameter
//! without per-backend boilerplate.

mod cache;
mod client;
mod share;
mod unsupported;

pub use cache::HostFsCacheStats;
pub use client::{FilledHostFsWrite, HostFsClient, HostFsTransport, HostFsWrite};
pub use share::{HOST_SHARE_GUEST_MOUNT_PATH, HOST_SHARE_MOUNT_TAG};
pub use unsupported::UnsupportedHostFileSystem;
