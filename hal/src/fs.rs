pub use crate::io::{IoError, IoResult};

use crate::resource::{KernelResource, ResourceRights};
use alloc::vec::Vec;
use bitflags::bitflags;
use core::future::Future;

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct FileSystemRights: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct FileRights: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const EXECUTE = 1 << 2;
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct DirectoryRights: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct BlockDeviceRights: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
    }
}

macro_rules! impl_resource_rights {
    ($($rights:ty),+ $(,)?) => {
        $(
            impl ResourceRights for $rights {
                fn contains(self, other: Self) -> bool {
                    (self.bits() & other.bits()) == other.bits()
                }
            }
        )+
    };
}

impl_resource_rights!(
    FileSystemRights,
    FileRights,
    DirectoryRights,
    BlockDeviceRights,
);

pub type FileSystemHandle<T> = KernelResource<T, FileSystemRights>;
pub type FileHandle<T> = KernelResource<T, FileRights>;
pub type DirectoryHandle<T> = KernelResource<T, DirectoryRights>;
pub type BlockDeviceHandle<T> = KernelResource<T, BlockDeviceRights>;

/// Virtio 9p mount tag used by platform tooling and backends to identify the
/// optional host-share device.
pub const HOST_SHARE_MOUNT_TAG: &str = "hostshare";

/// Filesystem lookup result with concrete, already-rights-clamped resources.
///
/// The resource itself carries its own rights, so callers do not need to keep a
/// second side table to know what they may do with the returned handle.
pub enum DirectoryEntry<Directory, File> {
    Directory(Directory),
    File(File),
}

pub trait DirectoryEntryExt<Directory, File> {
    fn directory(self) -> Option<Directory>;
    fn file(self) -> Option<File>;
}

impl<Directory, File> DirectoryEntryExt<Directory, File> for DirectoryEntry<Directory, File> {
    fn directory(self) -> Option<Directory> {
        match self {
            Self::Directory(directory) => Some(directory),
            Self::File(_) => None,
        }
    }

    fn file(self) -> Option<File> {
        match self {
            Self::Directory(_) => None,
            Self::File(file) => Some(file),
        }
    }
}

pub trait FileSystem: Send + Sync {
    type Directory: Directory;
    type File: File;

    fn open(
        &self,
        path: &str,
    ) -> impl Future<Output = IoResult<Option<DirectoryEntry<Self::Directory, Self::File>>>> + Send;

    fn create_directory(
        &self,
        path: &str,
    ) -> impl Future<Output = IoResult<Self::Directory>> + Send;

    fn remove(&self, path: &str) -> impl Future<Output = IoResult<()>> + Send;

    fn rename(&self, source: &str, destination: &str) -> impl Future<Output = IoResult<()>> + Send;
}

pub trait File: Send {
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = IoResult<usize>> + Send;
    fn write(&mut self, buf: &[u8]) -> impl Future<Output = IoResult<usize>> + Send;

    fn truncate(&mut self) -> impl Future<Output = IoResult<()>> + Send;
}

pub trait Directory: Send {
    type File: File;

    fn list(&self) -> impl Future<Output = IoResult<Vec<DirectoryEntry<Self, Self::File>>>> + Send
    where
        Self: Sized;

    fn create_directory(&self, path: &str) -> impl Future<Output = IoResult<Self>> + Send
    where
        Self: Sized;

    fn open_file(&self, path: &str) -> impl Future<Output = IoResult<Option<Self::File>>> + Send;

    fn remove(&self, path: &str) -> impl Future<Output = IoResult<()>> + Send;

    fn rename(&self, source: &str, destination: &str) -> impl Future<Output = IoResult<()>> + Send;
}

pub trait BlockDevice: Send + Sync {
    fn read_block(
        &self,
        block_id: usize,
        buf: &mut [u8],
    ) -> impl Future<Output = IoResult<()>> + Send;

    fn write_block(&self, block_id: usize, buf: &[u8])
    -> impl Future<Output = IoResult<()>> + Send;

    fn block_size(&self) -> usize;

    fn block_count(&self) -> usize;
}
