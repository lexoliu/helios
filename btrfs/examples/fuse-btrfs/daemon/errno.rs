//! The errno each filesystem failure means.
//!
//! A variant that reports damage to the image or a defect in the writer is
//! `EIO` and is logged with its name. Every variant is listed, so a new one
//! fails the build here until someone decides what it means.

use std::fmt::Display;

use fuser::Errno;
use helios_btrfs::write::{ChunkError, SpaceError, WriteError};
use helios_btrfs::{Error, FileKind};
use tracing::error;

/// Logs a corruption or internal defect and answers `EIO`.
fn defect(variant: &'static str, error: &impl Display) -> Errno {
    error!(variant, %error, "the filesystem reported a defect");
    Errno::EIO
}

pub fn read_errno(error: &Error) -> Errno {
    match error {
        Error::NotFound { .. } => Errno::ENOENT,
        Error::SymlinkLimit { .. } => Errno::ELOOP,
        Error::WrongKind {
            expected: FileKind::Directory,
            ..
        } => Errno::ENOTDIR,
        Error::WrongKind {
            found: FileKind::Directory,
            ..
        } => Errno::EISDIR,
        Error::WrongKind { .. } => Errno::EINVAL,
        Error::Io { .. } => defect("Io", error),
        Error::NoValidSuperblock { .. } => defect("NoValidSuperblock", error),
        Error::UnsupportedIncompat { .. } => defect("UnsupportedIncompat", error),
        Error::MultiDevice { .. } => defect("MultiDevice", error),
        Error::UnsupportedProfile { .. } => defect("UnsupportedProfile", error),
        Error::LogReplayRequired { .. } => defect("LogReplayRequired", error),
        Error::SysChunkArray { .. } => defect("SysChunkArray", error),
        Error::Chunk { .. } => defect("Chunk", error),
        Error::Unmapped { .. } => defect("Unmapped", error),
        Error::TreeBlock { .. } => defect("TreeBlock", error),
        Error::TreeOrder { .. } => defect("TreeOrder", error),
        Error::Item { .. } => defect("Item", error),
        Error::MissingRoot { .. } => defect("MissingRoot", error),
        Error::MissingInode { .. } => defect("MissingInode", error),
        Error::DataChecksum { .. } => defect("DataChecksum", error),
        Error::MissingDataChecksum { .. } => defect("MissingDataChecksum", error),
        Error::Decompress { .. } => defect("Decompress", error),
    }
}

pub fn write_errno(error: &WriteError) -> Errno {
    match error {
        WriteError::InvalidPermissions { .. } => Errno::EINVAL,
        WriteError::StaleHandle { .. } => Errno::ESTALE,
        WriteError::InodeFlags { .. } => Errno::EPERM,
        WriteError::SetId { .. } => Errno::EPERM,
        WriteError::UnsupportedCompatRo { .. } => Errno::EROFS,
        WriteError::QuotaTreePresent => Errno::EROFS,
        WriteError::ReadOnlySubvolume { .. } => Errno::EROFS,
        WriteError::CloneAlignment
        | WriteError::CloneSourceRange
        | WriteError::CloneOverlap
        | WriteError::CloneChecksumPolicy
        | WriteError::FileRange { .. } => Errno::EINVAL,
        WriteError::InvalidName => Errno::EINVAL,
        WriteError::Exists { .. } => Errno::EEXIST,
        WriteError::IdentifierExhausted { .. } => Errno::ENOSPC,
        WriteError::LinkLimit { .. } => Errno::EMLINK,
        WriteError::NoEntry { .. } => Errno::ENOENT,
        WriteError::NotEmpty { .. } => Errno::ENOTEMPTY,
        WriteError::SubvolumeLink => Errno::EPERM,
        WriteError::CrossSubvolume => Errno::EXDEV,
        WriteError::DirectoryCycle => Errno::EINVAL,
        WriteError::SymlinkTarget {
            length, maximum, ..
        } if length > maximum => Errno::ENAMETOOLONG,
        WriteError::SymlinkTarget { .. } => Errno::EINVAL,
        WriteError::DirectoryLink => Errno::EPERM,
        WriteError::FileOverflow => Errno::EFBIG,
        WriteError::NoSpace { .. } => Errno::ENOSPC,
        WriteError::Chunk(ChunkError::NoDeviceSpace { .. }) => Errno::ENOSPC,
        WriteError::Chunk(ChunkError::SystemArrayFull { .. }) => Errno::ENOSPC,
        WriteError::Chunk(ChunkError::UnsupportedProfile { .. }) => {
            defect("Chunk::UnsupportedProfile", error)
        }
        WriteError::Chunk(ChunkError::MissingDevice { .. }) => {
            defect("Chunk::MissingDevice", error)
        }
        WriteError::Chunk(ChunkError::NotInSystemArray { .. }) => {
            defect("Chunk::NotInSystemArray", error)
        }
        WriteError::Chunk(ChunkError::Truncated { .. }) => defect("Chunk::Truncated", error),
        WriteError::Chunk(ChunkError::Map { .. }) => defect("Chunk::Map", error),
        WriteError::Read(error) => read_errno(error),
        WriteError::Space(SpaceError::Read(error)) => read_errno(error),
        WriteError::Space(SpaceError::Exhausted { .. }) => Errno::ENOSPC,
        WriteError::Space(SpaceError::Corrupt { .. }) => defect("Space::Corrupt", error),
        WriteError::Space(SpaceError::DataLength { .. }) => defect("Space::DataLength", error),
        WriteError::Space(SpaceError::NotFree { .. }) => defect("Space::NotFree", error),
        WriteError::ForeignHandle => defect("ForeignHandle", error),
        WriteError::Encode(_) => defect("Encode", error),
        WriteError::CompressionWithoutChecksums => defect("CompressionWithoutChecksums", error),
        WriteError::NamespaceMismatch { .. } => defect("NamespaceMismatch", error),
        WriteError::NotRegularExtent { .. } => defect("NotRegularExtent", error),
        WriteError::WriterPoisoned => defect("WriterPoisoned", error),
        WriteError::CommitSuperblock(_) => defect("CommitSuperblock", error),
        WriteError::StaleSnapshot => defect("StaleSnapshot", error),
        WriteError::GenerationOverflow => defect("GenerationOverflow", error),
        WriteError::StagingRange { .. } => defect("StagingRange", error),
        WriteError::Item(_) => defect("Item", error),
        WriteError::FreeSpace { .. } => defect("FreeSpace", error),
        WriteError::Uuid { .. } => defect("Uuid", error),
        WriteError::Allocation { .. } => defect("Allocation", error),
        WriteError::Data { .. } => defect("Data", error),
        WriteError::Checksum { .. } => defect("Checksum", error),
        WriteError::RootUsage { .. } => defect("RootUsage", error),
        WriteError::MissingExtent { .. } => defect("MissingExtent", error),
        WriteError::Height { .. } => defect("Height", error),
        WriteError::LogReplay(_) => defect("LogReplay", error),
    }
}
