//! EROFS images on a mount-source disk, mounted read-only.
//!
//! [`ErofsFs`] adapts `erofs-rs`'s async reader to the filesystem
//! contract. The reader asks for bytes through [`DiskImage`], whose
//! positional read is [`CachedDisk::read_at`]: every structure the reader
//! parses arrives through the kernel's block cache, and the reader keeps
//! no cache of its own beyond the superblock it validated at mount.
//!
//! The image is untrusted device input. The reader validates every
//! structure it decodes and answers a malformed one with an error, which
//! fails the operation that reached it; [`ErofsFs::mount`] verifies the
//! superblock checksum, which the reader does not, before the filesystem
//! exists.
//!
//! # Concurrency contract
//!
//! An [`ErofsFs`] is an immutable, cloneable handle: the reader holds the
//! parsed superblock and nothing it writes after mount, so every
//! processor resolves paths and reads files through one at once, under
//! the block cache's own contract (a hit takes no lock, a miss one bucket
//! stripe).

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;

use bytes::{Bytes, BytesMut};
use crc::{CRC_32_ISCSI, Crc};
use erofs_rs::DirPosition;
use erofs_rs::r#async::EroFS;
use erofs_rs::backend::AsyncImage;
use erofs_rs::types::{DirentFileType, FeatureCompat, FileType, Inode, SUPER_BLOCK_OFFSET};
use helios_hal::fs::{
    BlockDevice, BlockSerial, DeviceId, DirCookie, DirEntry, FileSystem, FsError, FsPath, FsResult,
    Metadata, NodeId, NodeKind, OpenFlags, WritePosition,
};
use thiserror::Error;
use triomphe::Arc;

use crate::{BlockCacheError, BlockPages, CachedDisk};

/// CRC-32C, the checksum EROFS stores in its superblock.
const CASTAGNOLI: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

/// Where the checksum sits in the superblock: after the 32-bit magic
/// (`struct erofs_super_block` in Linux's `fs/erofs/erofs_fs.h`).
const CHECKSUM_FIELD: core::ops::Range<usize> = 4..8;

/// Bytes the checksum loop reads from the cache at a time: one stack
/// buffer, so verifying an image allocates nothing.
const CHECKSUM_CHUNK: usize = 512;

/// Why an EROFS image could not be mounted.
#[derive(Debug, Error)]
pub enum ErofsError {
    #[error("reading the image failed: {0}")]
    Device(#[from] BlockCacheError),
    #[error(
        "the superblock checksum is {stored:#010x} but the superblock's bytes give {computed:#010x}"
    )]
    Checksum { stored: u32, computed: u32 },
    #[error("the image is not a valid EROFS image: {0}")]
    Format(erofs_rs::Error),
}

/// A mount-source disk as the reader sees it: bytes at offsets.
pub struct DiskImage<Device> {
    disk: CachedDisk<Device>,
}

impl<Device> AsyncImage for DiskImage<Device>
where
    Device: BlockDevice<Buffer = BlockPages>,
{
    async fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> erofs_rs::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        self.disk.read_at(offset, buf).await.map_err(|error| {
            tracing::warn!(
                %error,
                serial = ?self.disk.serial(),
                offset,
                len = buf.len(),
                "EROFS image read failed"
            );
            match error {
                BlockCacheError::ByteRangeOutOfRange {
                    offset,
                    len,
                    capacity_bytes,
                } => erofs_rs::Error::OutOfRange(offset.saturating_add(len as u64), capacity_bytes),
                error => {
                    erofs_rs::Error::CorruptedData(format!("disk read at byte {offset}: {error}"))
                }
            }
        })
    }
}

/// An EROFS image, mounted read-only. Clones share it.
pub struct ErofsFs<Device: BlockDevice<Buffer = BlockPages>> {
    inner: Arc<ErofsInner<Device>>,
}

impl<Device: BlockDevice<Buffer = BlockPages>> Clone for ErofsFs<Device> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct ErofsInner<Device: BlockDevice<Buffer = BlockPages>> {
    reader: EroFS<DiskImage<Device>>,
    device: DeviceId,
    serial: BlockSerial,
}

impl<Device> ErofsFs<Device>
where
    Device: BlockDevice<Buffer = BlockPages>,
{
    /// Reads and validates the image on `disk`, verifies its superblock
    /// checksum when the image carries one, and mounts it as `device`.
    pub async fn mount(disk: CachedDisk<Device>, device: DeviceId) -> Result<Self, ErofsError> {
        let serial = disk.serial();
        // The superblock is read once through the cache before the reader
        // sees it, so a disk that cannot be read fails with the cache's
        // own error rather than the reader's account of it.
        let mut header = [0; CHECKSUM_FIELD.end];
        disk.read_at(SUPER_BLOCK_OFFSET, &mut header).await?;
        let reader = EroFS::new(DiskImage { disk: disk.clone() })
            .await
            .map_err(ErofsError::Format)?;
        let super_block = reader.super_block();
        if super_block
            .feature_compat_flags()
            .contains(FeatureCompat::SB_CSUM)
        {
            verify_checksum(&disk, super_block.checksum, super_block.blk_size_bits).await?;
        }
        Ok(Self {
            inner: Arc::new(ErofsInner {
                reader,
                device,
                serial,
            }),
        })
    }

    fn reader(&self) -> &EroFS<DiskImage<Device>> {
        &self.inner.reader
    }

    /// The inode at `path`, without following a final symbolic link.
    async fn inode(&self, path: FsPath<'_>) -> FsResult<Inode> {
        self.reader()
            .lookup(path.as_str())
            .await
            .map_err(|error| self.fs_error(error))?
            .ok_or(FsError::NoEntry)
    }

    fn metadata(&self, inode: &Inode) -> FsResult<Metadata> {
        let kind = node_kind(inode.file_type())?;
        let (seconds, nanos) = inode.modified_unix();
        let modified_nanos = u64::try_from(seconds)
            .ok()
            .and_then(|seconds| seconds.checked_mul(1_000_000_000))
            .and_then(|nanos_of_seconds| nanos_of_seconds.checked_add(u64::from(nanos)))
            .ok_or(FsError::Overflow)?;
        Ok(Metadata {
            id: NodeId::new(self.inner.device, inode.id()),
            kind,
            link_count: u64::from(inode.nlink()),
            size: match kind {
                NodeKind::Directory => 0,
                NodeKind::File | NodeKind::Symlink => inode.data_size(),
            },
            access_nanos: modified_nanos,
            modified_nanos,
            status_nanos: modified_nanos,
        })
    }

    /// What an operation answers for a reader error: the structural
    /// answers the reader gives in so many words, and an I/O error for
    /// everything that means the image or the disk is bad, which is
    /// logged with the reader's account first.
    fn fs_error(&self, error: erofs_rs::Error) -> FsError {
        match error {
            erofs_rs::Error::PathNotFound(_) => FsError::NoEntry,
            erofs_rs::Error::NotADirectory(_) => FsError::NotDirectory,
            error => {
                tracing::warn!(
                    %error,
                    serial = ?self.inner.serial,
                    "EROFS image is malformed or unreadable"
                );
                FsError::Io
            }
        }
    }
}

/// The kernel's kind for an inode's type. The kernel's filesystem
/// contract names no device, FIFO or socket node, so an image's are
/// refused rather than passed off as something they are not.
fn node_kind(file_type: FileType) -> FsResult<NodeKind> {
    match file_type {
        FileType::Directory => Ok(NodeKind::Directory),
        FileType::RegularFile => Ok(NodeKind::File),
        FileType::Symlink => Ok(NodeKind::Symlink),
        FileType::Fifo
        | FileType::Socket
        | FileType::CharacterDevice
        | FileType::BlockDevice
        | FileType::Unknown => Err(FsError::Unsupported),
    }
}

/// Verifies the superblock checksum: CRC-32C over the superblock's block
/// from the superblock on, with the checksum field read as zero, stored
/// without the final inversion (`erofs_superblock_csum_verify` in
/// Linux's `fs/erofs/super.c`).
async fn verify_checksum<Device>(
    disk: &CachedDisk<Device>,
    stored: u32,
    block_size_bits: u8,
) -> Result<(), ErofsError>
where
    Device: BlockDevice<Buffer = BlockPages>,
{
    // The reader accepted the superblock, so the block size is one it
    // supports; Linux covers a block's worth past the superblock's
    // offset, or the whole block when it is no larger than that offset.
    let block_bytes = 1_u64 << block_size_bits;
    let covered = if block_bytes > SUPER_BLOCK_OFFSET {
        block_bytes - SUPER_BLOCK_OFFSET
    } else {
        block_bytes
    };
    let mut digest = CASTAGNOLI.digest();
    let mut chunk = [0; CHECKSUM_CHUNK];
    let mut read = 0;
    while read < covered {
        let len = (covered - read).min(CHECKSUM_CHUNK as u64) as usize;
        let bytes = &mut chunk[..len];
        disk.read_at(SUPER_BLOCK_OFFSET + read, bytes).await?;
        if read == 0 {
            bytes[CHECKSUM_FIELD].fill(0);
        }
        digest.update(bytes);
        read += len as u64;
    }
    let computed = !digest.finalize();
    if computed != stored {
        return Err(ErofsError::Checksum { stored, computed });
    }
    Ok(())
}

impl<Device> FileSystem for ErofsFs<Device>
where
    Device: BlockDevice<Buffer = BlockPages> + 'static,
{
    async fn stat(&self, path: FsPath<'_>) -> FsResult<Metadata> {
        let inode = self.inode(path).await?;
        self.metadata(&inode)
    }

    async fn open(&self, path: FsPath<'_>, flags: OpenFlags, _now: u64) -> FsResult<Metadata> {
        let metadata = match self.stat(path).await {
            Ok(metadata) => metadata,
            Err(FsError::NoEntry) if flags.contains(OpenFlags::CREATE) => {
                return Err(FsError::ReadOnly);
            }
            Err(error) => return Err(error),
        };
        if flags.contains(OpenFlags::CREATE | OpenFlags::EXCLUSIVE) {
            return Err(FsError::Exists);
        }
        if flags.contains(OpenFlags::DIRECTORY) && metadata.kind != NodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        if flags.contains(OpenFlags::TRUNCATE) {
            return Err(FsError::ReadOnly);
        }
        Ok(metadata)
    }

    async fn read_at(&self, path: FsPath<'_>, offset: u64, max_bytes: usize) -> FsResult<Bytes> {
        let inode = self.inode(path).await?;
        match node_kind(inode.file_type())? {
            NodeKind::File => {}
            NodeKind::Directory => return Err(FsError::IsDirectory),
            NodeKind::Symlink => return Err(FsError::Invalid),
        }
        let size = inode.data_size();
        if offset >= size {
            return Ok(Bytes::new());
        }
        // The file's own size bounds the buffer, whatever the caller asked.
        let len = usize::try_from(size - offset).map_or(max_bytes, |left| left.min(max_bytes));
        let file = self
            .reader()
            .open_inode_file(inode)
            .map_err(|error| self.fs_error(error))?;
        let mut buffer = BytesMut::zeroed(len);
        let mut filled = 0;
        while filled < len {
            let read = file
                .read_at(&mut buffer[filled..], offset + filled as u64)
                .await
                .map_err(|error| self.fs_error(error))?;
            if read == 0 {
                // The inode's size promised bytes its data does not hold.
                tracing::warn!(
                    serial = ?self.inner.serial,
                    inode = inode.id(),
                    offset = offset + filled as u64,
                    size,
                    "EROFS file ends before its recorded size"
                );
                return Err(FsError::Io);
            }
            filled += read;
        }
        Ok(buffer.freeze())
    }

    fn write_with<'a, F>(
        &'a self,
        _: FsPath<'a>,
        _: WritePosition,
        _: usize,
        _: u64,
        _: F,
    ) -> impl Future<Output = FsResult<u64>> + Send + 'a
    where
        F: FnOnce(&mut [u8]),
    {
        core::future::ready(Err(FsError::ReadOnly))
    }

    async fn set_size(&self, _: FsPath<'_>, _: u64, _: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn set_times(
        &self,
        _: FsPath<'_>,
        _: Option<u64>,
        _: Option<u64>,
        _: u64,
    ) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn read_dir(
        &self,
        path: FsPath<'_>,
        cookie: DirCookie,
        max_entries: usize,
    ) -> FsResult<Vec<DirEntry>> {
        let inode = self.inode(path).await?;
        if node_kind(inode.file_type())? != NodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        let mut listing = self
            .reader()
            .read_dir_inode_at(inode, DirPosition::new(cookie.raw()))
            .await
            .map_err(|error| match error {
                // A cookie this directory never handed out.
                erofs_rs::Error::OutOfRange(..) => FsError::Invalid,
                error => self.fs_error(error),
            })?;
        let mut entries = Vec::new();
        while entries.len() < max_entries {
            let Some(entry) = listing
                .next_entry()
                .await
                .map_err(|error| self.fs_error(error))?
            else {
                break;
            };
            let name = String::from_utf8(entry.file_name().to_vec())
                .map_err(|_| FsError::IllegalByteSequence)?;
            let kind = match entry.file_type() {
                DirentFileType::Directory => NodeKind::Directory,
                DirentFileType::RegularFile => NodeKind::File,
                DirentFileType::Symlink => NodeKind::Symlink,
                // The entry carries no type of its own; the inode does.
                DirentFileType::Unknown => {
                    let inode = self
                        .reader()
                        .get_inode(entry.nid())
                        .await
                        .map_err(|error| self.fs_error(error))?;
                    node_kind(inode.file_type())?
                }
                DirentFileType::CharacterDevice
                | DirentFileType::BlockDevice
                | DirentFileType::Fifo
                | DirentFileType::Socket => return Err(FsError::Unsupported),
            };
            entries.push(DirEntry {
                name,
                kind,
                inode: entry.nid(),
                next: DirCookie::new(listing.position().raw()),
            });
        }
        Ok(entries)
    }

    async fn create_directory(&self, _: FsPath<'_>, _: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn remove_directory(&self, _: FsPath<'_>) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn unlink(&self, _: FsPath<'_>) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn rename(&self, _: FsPath<'_>, _: FsPath<'_>, _: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn link(&self, _: FsPath<'_>, _: FsPath<'_>, _: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn symlink(&self, _: &str, _: FsPath<'_>, _: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }

    async fn read_link(&self, path: FsPath<'_>) -> FsResult<String> {
        let inode = self.inode(path).await?;
        if node_kind(inode.file_type())? != NodeKind::Symlink {
            return Err(FsError::Invalid);
        }
        let target = self
            .reader()
            .read_link_inode(inode)
            .await
            .map_err(|error| self.fs_error(error))?;
        String::from_utf8(target.into_vec()).map_err(|_| FsError::IllegalByteSequence)
    }

    async fn sync(&self, path: FsPath<'_>) -> FsResult<()> {
        // Nothing here is ever written, so everything is already durable.
        self.inode(path).await.map(drop)
    }

    async fn sync_data(&self, path: FsPath<'_>) -> FsResult<()> {
        self.inode(path).await.map(drop)
    }
}

#[cfg(test)]
mod tests;
