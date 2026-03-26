pub use crate::io::IoResult;
use alloc::vec::Vec;
use core::future::Future;

pub trait FileSystem: Send + Sync {
    type Directory: Directory;
    fn open(&self, path: &str);
    fn root(&self) -> Self::Directory;
}

pub trait File: Send {
    fn read(&mut self, buf: &mut [u8]) -> impl Future<Output = usize> + Send;
    fn write(&mut self, buf: &[u8]) -> impl Future<Output = usize> + Send;
}

pub enum DirectoryEntry<Directory, File> {
    Directory(Directory),
    File(File),
}

pub trait Directory {
    type File: File;
    fn list(&self) -> impl Future<Output = Vec<DirectoryEntry<Self, Self::File>>>
    where
        Self: Sized;
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
}
