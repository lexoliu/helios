pub use crate::io::IoResult;
use crate::resource::KernelResource;
use alloc::vec::Vec;
use bitflags::bitflags;
use core::future::Future;

bitflags! {
    pub struct FileSystemRights: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
    }
}

pub trait FileSystem: Send + Sync + KernelResource<Rights = FileSystemRights> {
    type Directory: Directory;
    fn open(&self, path: &str);
    fn root(&self) -> Self::Directory;
}

bitflags! {
    pub struct FileRights: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const EXECUTE = 1 << 2;
    }
}

pub trait File: Send + KernelResource<Rights = FileRights> {
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = usize> + Send;
    fn write(&mut self, buf: &[u8]) -> impl Future<Output = usize> + Send;
}

pub enum DirectoryEntry<Directory, File> {
    Directory(Directory),
    File(File),
}

bitflags! {
    pub struct DirectoryEntryRights: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
    }
}

pub trait Directory: Send + KernelResource<Rights = DirectoryEntryRights> {
    type File: File;
    fn list(&self) -> impl Future<Output = Vec<DirectoryEntry<Self, Self::File>>>
    where
        Self: Sized;
}

bitflags! {
    pub struct BlockDeviceRights:u8{

    }
}

pub trait BlockDevice: Send + Sync + KernelResource<Rights = BlockDeviceRights> {
    fn read_block(
        &self,
        block_id: usize,
        buf: &mut [u8],
    ) -> impl Future<Output = IoResult<()>> + Send;

    fn write_block(&self, block_id: usize, buf: &[u8])
    -> impl Future<Output = IoResult<()>> + Send;

    fn block_size(&self) -> usize;
}
