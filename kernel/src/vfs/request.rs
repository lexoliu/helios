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

use super::{
    BtrfsFs, BtrfsService, BtrfsWriteOptions, ErofsError, ErofsFs, HostShare, MemoryFs, MountError,
    MountGrant, MountSharing, MountedFs, Namespace, Storage,
};
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
    /// A btrfs image on the disk with this serial.
    Btrfs { serial: BlockSerial },
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
    #[error("disk {serial:?} is read-only but a writable mount was requested")]
    ReadOnlyDisk { serial: BlockSerial },
    #[error("the image on disk {serial:?} cannot be mounted: {error}")]
    Erofs {
        serial: BlockSerial,
        error: ErofsError,
    },
    #[error("invalid compression option: {0}")]
    CompressionOption(#[from] helios_btrfs::write::CompressionOptionError),
    #[error("a compression policy applies only to a btrfs mount")]
    CompressionUnsupported,
    #[error("a commit interval applies only to a btrfs mount")]
    CommitUnsupported,
    #[error("the btrfs image on disk {serial:?} cannot be mounted: {error}")]
    Btrfs {
        serial: BlockSerial,
        error: super::BtrfsMountError,
    },
    #[error(transparent)]
    Mount(#[from] MountError),
}

/// One mount user space asks for.
pub struct MountRequest<'a> {
    pub source: MountSource,
    /// The guest path to attach the filesystem at.
    pub point: &'a str,
    /// Attach for reading only. The boot image and an EROFS disk are
    /// read-only whatever this says.
    pub read_only: bool,
    /// Whether the mount is private or shared, for as long as it exists.
    pub sharing: MountSharing,
    /// How a writable btrfs mount writes; only btrfs takes a compression
    /// policy or a commit interval.
    pub write: BtrfsWriteOptions,
}

/// Attaches the filesystem `request` names, under `authority`, from the
/// storage the machine `attached`. The caller must spawn any returned
/// service on the kernel executor before serving filesystem requests.
pub async fn request_mount<S: Storage>(
    authority: &ProcessAuthority,
    namespace: &Namespace<S>,
    attached: AttachedStorage<S>,
    request: MountRequest<'_>,
) -> Result<Option<BtrfsService<S::Disk>>, MountRequestError> {
    let MountRequest {
        source,
        point,
        read_only,
        sharing,
        write,
    } = request;
    let cap = authority.derive_mount_cap(point)?;
    if write.compress.is_some() && !matches!(source, MountSource::Btrfs { .. }) {
        return Err(MountRequestError::CompressionUnsupported);
    }
    if write.commit.is_some() && !matches!(source, MountSource::Btrfs { .. }) {
        return Err(MountRequestError::CommitUnsupported);
    }
    let mut service = None;
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
        MountSource::Btrfs { serial } => {
            let disk = attached
                .disks
                .and_then(|disks| disks.disk_by_serial(serial.as_bytes()))
                .ok_or(MountRequestError::DiskUnavailable { serial })?;
            if !read_only && disk.is_read_only() {
                return Err(MountRequestError::ReadOnlyDisk { serial });
            }
            let fs = if read_only {
                BtrfsFs::mount(disk, || namespace.table().allocate_device())
                    .await
                    .map_err(|error| MountRequestError::Btrfs { serial, error })?
            } else {
                let (fs, task) =
                    BtrfsFs::mount_writable(disk, || namespace.table().allocate_device(), write)
                        .await
                        .map_err(|error| MountRequestError::Btrfs { serial, error })?;
                service = Some(task);
                fs
            };
            MountedFs::Btrfs(fs)
        }
    };
    let rights = match source {
        MountSource::BootImage | MountSource::Disk { .. } => FileSystemRights::READ,
        MountSource::Memory | MountSource::HostShare | MountSource::Btrfs { .. } if read_only => {
            FileSystemRights::READ
        }
        MountSource::Memory | MountSource::HostShare | MountSource::Btrfs { .. } => {
            FileSystemRights::READ | FileSystemRights::WRITE
        }
    };
    namespace
        .mount(&cap, fs, MountGrant { rights, sharing })
        .await?;
    Ok(service)
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
