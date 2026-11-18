//! Attaching filesystems to the guest path tree.
//!
//! A mount is seen by every program. Attaching or detaching one takes the
//! mount authority a [`crate::programs::mount_grant`] hands a child, and
//! mutate-directory authority over the mount point.
//!
//! A mount is granted as private or shared ([`MountSharing`]), and that
//! never changes while it exists. A WASI program holds the mount's rights
//! either way and sees no owner, group or mode; the choice decides what a
//! Linux container sees of the filesystem.

use crate::bindings::helios::system::mounts as raw;
pub use crate::bindings::helios::system::mounts::{
    MountError, MountErrorKind, MountRequest, MountSharing, MountSource,
};

/// Attaches `source` at `path`, a directory path in the caller's authority,
/// granted as `sharing`. `compress` is the value of Linux's `compress=`
/// option for a writable btrfs mount, such as `zstd:3`, and `commit` the
/// value of its `commit=` option, in seconds.
pub async fn mount(
    source: MountSource,
    path: impl Into<String>,
    read_only: bool,
    sharing: MountSharing,
    compress: Option<String>,
    commit: Option<u32>,
) -> Result<(), MountError> {
    raw::mount(MountRequest {
        source,
        path: path.into(),
        read_only,
        sharing,
        compress,
        commit,
    })
    .await
}

/// Detaches the filesystem mounted at `path`.
pub async fn unmount(path: impl Into<String>) -> Result<(), MountError> {
    raw::unmount(path.into()).await
}
