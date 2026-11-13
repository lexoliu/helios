//! The storage drivers a backend brings to the mount table.
//!
//! A mount kind that reaches a device names the device's type, and only
//! the backend knows it: the host share is the backend's 9p client, a
//! disk is the backend's block device. [`Storage`] carries both as
//! associated types of one marker the backend defines, so the mount
//! table, the namespaces over it and the runtime adapter that resolves
//! paths through them take one type parameter for all of a machine's
//! storage, and every request still reaches its device by static
//! dispatch.

use helios_hal::fs::{
    BlockCompletion, BlockDevice, BlockDeviceCapabilities, BlockGeometry, BlockQueueTopology,
    BlockRange, BlockSerial, IoResult,
};

use crate::{BlockPages, HostFileSystem, UnsupportedHostFileSystem};

/// The storage drivers of one machine.
///
/// Implemented by a marker type the backend defines; it is only named,
/// never constructed for what it holds.
pub trait Storage: Clone + Send + Sync + 'static {
    /// The host share's client.
    type HostShare: HostFileSystem;
    /// The block device a mount-source disk is, behind the block cache.
    type Disk: BlockDevice<Buffer = BlockPages> + 'static;
}

/// The disk type of a machine that has none: no value of it exists, so
/// nothing can mount one.
#[derive(Clone, Copy, Debug)]
pub enum NoDisks {}

impl BlockDevice for NoDisks {
    type Buffer = BlockPages;
    type Waiter<'a>
        = core::future::Pending<BlockCompletion<BlockPages>>
    where
        Self: 'a;

    async fn submit_read(&self, _: usize, _: BlockPages) -> Self::Waiter<'_> {
        match *self {}
    }

    async fn submit_write(&self, _: usize, _: BlockPages) -> Self::Waiter<'_> {
        match *self {}
    }

    async fn flush(&self) -> IoResult<()> {
        match *self {}
    }

    async fn discard(&self, _: BlockRange) -> IoResult<()> {
        match *self {}
    }

    async fn write_zeroes(&self, _: BlockRange) -> IoResult<()> {
        match *self {}
    }

    async fn serial(&self) -> IoResult<BlockSerial> {
        match *self {}
    }

    fn geometry(&self) -> BlockGeometry {
        match *self {}
    }

    fn capabilities(&self) -> BlockDeviceCapabilities {
        match *self {}
    }

    fn queue_topology(&self) -> BlockQueueTopology {
        match *self {}
    }

    fn is_read_only(&self) -> bool {
        match *self {}
    }
}

/// The storage of a machine with neither a host share nor a disk: the
/// kernel's own tests, and a backend that brings up neither.
#[derive(Clone, Copy, Debug)]
pub struct UnsupportedStorage;

impl Storage for UnsupportedStorage {
    type HostShare = UnsupportedHostFileSystem;
    type Disk = NoDisks;
}
