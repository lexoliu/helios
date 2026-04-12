extern crate alloc;

use alloc::vec::Vec;

use crate::{HostDirEntry, HostFileSystem, HostFsError, HostMetadata};

/// A [`HostFileSystem`] implementation that rejects every operation with
/// [`helios_hal::io::IoError::Unsupported`].
///
/// Backends that do not provide a host share filesystem (e.g. x86 without
/// virtio-9p) can use this as their `HostFs` type parameter instead of
/// writing per-backend stub code.
#[derive(Clone)]
pub struct UnsupportedHostFileSystem;

fn unsupported<T>() -> core::future::Ready<Result<T, HostFsError>> {
    core::future::ready(Err(HostFsError::Transport(
        helios_hal::io::IoError::Unsupported,
    )))
}

impl HostFileSystem for UnsupportedHostFileSystem {
    type StatFuture<'a> = core::future::Ready<Result<HostMetadata, HostFsError>>;
    type ReadDirFuture<'a> = core::future::Ready<Result<Vec<HostDirEntry>, HostFsError>>;
    type ReadFileFuture<'a> = core::future::Ready<Result<Vec<u8>, HostFsError>>;
    type ReadFileRangeFuture<'a> = core::future::Ready<Result<Vec<u8>, HostFsError>>;
    type WriteFileFuture<'a> = core::future::Ready<Result<(), HostFsError>>;
    type TruncateFileFuture<'a> = core::future::Ready<Result<(), HostFsError>>;
    type CreateFileFuture<'a> = core::future::Ready<Result<(), HostFsError>>;
    type CreateDirectoryFuture<'a> = core::future::Ready<Result<(), HostFsError>>;
    type RemoveFuture<'a> = core::future::Ready<Result<(), HostFsError>>;
    type RenameFuture<'a> = core::future::Ready<Result<(), HostFsError>>;

    fn stat_path(&self, _path: &str) -> Self::StatFuture<'_> { unsupported() }
    fn read_dir(&self, _path: &str) -> Self::ReadDirFuture<'_> { unsupported() }
    fn read_file(&self, _path: &str) -> Self::ReadFileFuture<'_> { unsupported() }
    fn read_file_range(&self, _path: &str, _offset: u64, _max_bytes: u32) -> Self::ReadFileRangeFuture<'_> { unsupported() }
    fn write_file(&self, _path: &str, _offset: u64, _bytes: &[u8]) -> Self::WriteFileFuture<'_> { unsupported() }
    fn truncate_file(&self, _path: &str) -> Self::TruncateFileFuture<'_> { unsupported() }
    fn create_file(&self, _path: &str) -> Self::CreateFileFuture<'_> { unsupported() }
    fn create_directory(&self, _path: &str) -> Self::CreateDirectoryFuture<'_> { unsupported() }
    fn remove(&self, _path: &str, _directory: bool) -> Self::RemoveFuture<'_> { unsupported() }
    fn rename(&self, _source: &str, _destination: &str) -> Self::RenameFuture<'_> { unsupported() }
}
