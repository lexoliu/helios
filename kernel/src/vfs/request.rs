//! Mounts user space asks for.
//!
//! A request names a source and a mount point; the caller's
//! [`ProcessAuthority`] must hold the mount right and mutate-directory
//! authority over the point. The kernel builds the filesystem the source
//! names and attaches it through [`Namespace::mount`], so the point is
//! checked against what lies beneath it exactly as a kernel mount is.

use alloc::string::String;

use helios_hal::fs::{BlockSerial, FileSystemRights};
use thiserror::Error;

use super::{ErofsError, ErofsFs, HostShare, MemoryFs, MountError, MountedFs, Namespace, Storage};
use crate::{BlockCache, ProcessAuthority, ProcessAuthorityError};

/// A filesystem user space can ask the kernel to attach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountSource {
    /// A new, empty in-memory filesystem.
    Memory,
    /// The boot image, read-only whatever the request says.
    BootImage,
    /// The host share.
    HostShare,
    /// The EROFS image on the mount-source disk that reported `serial`,
    /// read-only whatever the request says.
    Disk { serial: BlockSerial },
}

/// The storage a machine attached, which a request's source is built
/// from.
pub struct AttachedStorage<S: Storage> {
    /// The host share's client, when the platform attached one.
    pub host_share: Option<S::HostShare>,
    /// The block cache over the mount-source disks, when there are any.
    pub disks: Option<BlockCache<S::Disk>>,
}

/// Why a mount or unmount request was refused.
#[derive(Debug, Error)]
pub enum MountRequestError {
    #[error("mount authority refused the request: {0}")]
    Authority(#[from] ProcessAuthorityError),
    #[error("the host share is not attached to this machine")]
    HostShareUnavailable,
    #[error("no mount-source disk reported the serial {0:?}")]
    InvalidSerial(String),
    #[error("no mount-source disk reported the serial {serial:?}")]
    DiskUnavailable { serial: BlockSerial },
    #[error("the image on disk {serial:?} cannot be mounted: {error}")]
    Erofs {
        serial: BlockSerial,
        error: ErofsError,
    },
    #[error(transparent)]
    Mount(#[from] MountError),
}

/// Attaches the filesystem `source` names at `point`, under `authority`,
/// from the storage the machine `attached`.
pub async fn request_mount<S: Storage>(
    authority: &ProcessAuthority,
    namespace: &Namespace<S>,
    attached: AttachedStorage<S>,
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
            attached
                .host_share
                .ok_or(MountRequestError::HostShareUnavailable)?,
        )),
        MountSource::Disk { serial } => {
            let disk = attached
                .disks
                .and_then(|disks| disks.disk_by_serial(serial.as_bytes()))
                .ok_or(MountRequestError::DiskUnavailable { serial })?;
            let fs = ErofsFs::mount(disk, namespace.table().allocate_device())
                .await
                .map_err(|error| MountRequestError::Erofs { serial, error })?;
            MountedFs::Erofs(fs)
        }
    };
    let rights = match source {
        MountSource::BootImage | MountSource::Disk { .. } => FileSystemRights::READ,
        MountSource::Memory | MountSource::HostShare if read_only => FileSystemRights::READ,
        MountSource::Memory | MountSource::HostShare => {
            FileSystemRights::READ | FileSystemRights::WRITE
        }
    };
    namespace.mount(&cap, fs, rights).await?;
    Ok(())
}

/// Detaches the filesystem at `point`, under `authority`.
pub async fn request_unmount<S: Storage>(
    authority: &ProcessAuthority,
    namespace: &Namespace<S>,
    point: &str,
) -> Result<(), MountRequestError> {
    let cap = authority.derive_unmount_cap(point)?;
    namespace.unmount(&cap).await?;
    Ok(())
}
