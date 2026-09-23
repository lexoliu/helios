extern crate alloc;

use alloc::string::String;
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
    fn stat_path(
        &self,
        _path: &str,
    ) -> impl core::future::Future<Output = Result<HostMetadata, HostFsError>> + Send + '_ {
        unsupported()
    }
    fn read_dir(
        &self,
        _path: &str,
    ) -> impl core::future::Future<Output = Result<Vec<HostDirEntry>, HostFsError>> + Send + '_
    {
        unsupported()
    }
    fn read_file(
        &self,
        _path: &str,
    ) -> impl core::future::Future<Output = Result<Vec<u8>, HostFsError>> + Send + '_ {
        unsupported()
    }
    fn read_file_range(
        &self,
        _path: &str,
        _offset: u64,
        _max_bytes: u32,
    ) -> impl core::future::Future<Output = Result<Vec<u8>, HostFsError>> + Send + '_ {
        unsupported()
    }
    fn write_file(
        &self,
        _path: &str,
        _offset: u64,
        _bytes: &[u8],
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        unsupported()
    }
    fn append_file(
        &self,
        _path: &str,
        _bytes: &[u8],
    ) -> impl core::future::Future<Output = Result<u64, HostFsError>> + Send + '_ {
        unsupported()
    }
    fn sync_file(
        &self,
        _path: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        unsupported()
    }
    fn truncate_file(
        &self,
        _path: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        unsupported()
    }
    fn set_file_size(
        &self,
        _path: &str,
        _size: u64,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        unsupported()
    }
    fn set_times(
        &self,
        _path: &str,
        _access_nanos: Option<u64>,
        _modified_nanos: Option<u64>,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        unsupported()
    }
    fn create_file(
        &self,
        _path: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        unsupported()
    }
    fn create_directory(
        &self,
        _path: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        unsupported()
    }
    fn remove(
        &self,
        _path: &str,
        _directory: bool,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        unsupported()
    }
    fn rename(
        &self,
        _source: &str,
        _destination: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        unsupported()
    }
    fn hard_link(
        &self,
        _source: &str,
        _destination: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        unsupported()
    }
    fn symlink(
        &self,
        _target: &str,
        _link_path: &str,
    ) -> impl core::future::Future<Output = Result<(), HostFsError>> + Send + '_ {
        unsupported()
    }
    fn read_link(
        &self,
        _path: &str,
    ) -> impl core::future::Future<Output = Result<String, HostFsError>> + Send + '_ {
        unsupported()
    }
}
