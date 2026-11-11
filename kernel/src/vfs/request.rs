//! Mounts user space asks for.
//!
//! A request names a source and a mount point; the caller's
//! [`ProcessAuthority`] must hold the mount right and mutate-directory
//! authority over the point. The kernel builds the filesystem the source
//! names and attaches it through [`Namespace::mount`], so the point is
//! checked against what lies beneath it exactly as a kernel mount is.

use helios_hal::fs::FileSystemRights;
use thiserror::Error;

use super::{HostShare, MemoryFs, MountError, MountedFs, Namespace};
use crate::{HostFileSystem, ProcessAuthority, ProcessAuthorityError};

/// A filesystem user space can ask the kernel to attach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountSource {
    /// A new, empty in-memory filesystem.
    Memory,
    /// The boot image, read-only whatever the request says.
    BootImage,
    /// The host share.
    HostShare,
}

/// Why a mount or unmount request was refused.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MountRequestError {
    #[error("mount authority refused the request: {0}")]
    Authority(#[from] ProcessAuthorityError),
    #[error("the host share is not attached to this machine")]
    HostShareUnavailable,
    #[error(transparent)]
    Mount(#[from] MountError),
}

/// Attaches the filesystem `source` names at `point`, under `authority`.
///
/// `host` is the machine's host-share client, when one is attached.
pub async fn request_mount<H: HostFileSystem>(
    authority: &ProcessAuthority,
    namespace: &Namespace<H>,
    host: Option<H>,
    source: MountSource,
    point: &str,
    read_only: bool,
) -> Result<(), MountRequestError> {
    let cap = authority.derive_mount_cap(point)?;
    let fs = match source {
        MountSource::Memory => {
            MountedFs::Memory(MemoryFs::new(namespace.table().allocate_device()))
        }
        MountSource::BootImage => MountedFs::Boot(namespace.table().boot().clone()),
        MountSource::HostShare => MountedFs::Host(HostShare::new(
            host.ok_or(MountRequestError::HostShareUnavailable)?,
        )),
    };
    let rights = if read_only || source == MountSource::BootImage {
        FileSystemRights::READ
    } else {
        FileSystemRights::READ | FileSystemRights::WRITE
    };
    namespace.mount(&cap, fs, rights).await?;
    Ok(())
}

/// Detaches the filesystem at `point`, under `authority`.
pub async fn request_unmount<H: HostFileSystem>(
    authority: &ProcessAuthority,
    namespace: &Namespace<H>,
    point: &str,
) -> Result<(), MountRequestError> {
    let cap = authority.derive_unmount_cap(point)?;
    namespace.unmount(&cap).await?;
    Ok(())
}
