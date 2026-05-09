//! Host filesystem client and supporting types.
//!
//! `client` implements the in-kernel 9p client used to drive a virtio
//! host share. `share` exposes the mount tag and guest-side path
//! helpers shared with the wasmtime host bindings. `unsupported`
//! provides a stub `HostFileSystem` that backends without a
//! virtio-9p device can use to satisfy the `HostFs` type parameter
//! without per-backend boilerplate.

mod client;
mod share;
mod unsupported;

pub use client::{HostFsClient, HostFsTransport};
pub use share::{HOST_SHARE_GUEST_MOUNT_PATH, HOST_SHARE_MOUNT_TAG, guest_host_share_path};
pub use unsupported::UnsupportedHostFileSystem;
