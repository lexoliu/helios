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

bitflags! {
    /// Operations a block device implements beyond plain read and write.
    ///
    /// These are hardware properties of the device, not driver policy: a
    /// device either commits a volatile write cache on request, accepts
    /// deallocation hints, and writes runs of zeroes without carrying the
    /// data, or it does not. Callers query the capability instead of
    /// discovering it from a failed request.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct BlockDeviceCapabilities: u8 {
        /// The device has a volatile write cache that
        /// [`BlockDevice::flush`] commits. A device without this bit
        /// has no such cache: a completed write is already durable.
        const FLUSH = 1 << 0;
        /// The device accepts [`BlockDevice::discard`].
        const DISCARD = 1 << 1;
        /// The device accepts [`BlockDevice::write_zeroes`].
        const WRITE_ZEROES = 1 << 2;
    }
}

/// A half-open run of logical blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRange {
    /// First logical block of the run.
    pub start_block: usize,
    /// Number of logical blocks in the run.
    pub block_count: usize,
}

impl BlockRange {
    pub const fn new(start_block: usize, block_count: usize) -> Self {
        Self {
            start_block,
            block_count,
        }
    }

    /// One past the last block of the run, or `None` on overflow.
    pub const fn end_block(self) -> Option<usize> {
        self.start_block.checked_add(self.block_count)
    }

    pub const fn is_empty(self) -> bool {
        self.block_count == 0
    }
}

/// Longest device serial the block contract carries.
///
/// Long enough for every serial a platform block device reports today
/// (virtio-blk hands back 20 bytes) and small enough to live in a value
/// type, so identifying a device never allocates.
pub const BLOCK_SERIAL_MAX_BYTES: usize = 32;

/// The serial a block device reports for itself.
///
/// This is how a caller tells two otherwise identical devices apart: the
/// platform names the disk it means, and the kernel matches on the name
/// rather than on the order the bus happened to enumerate.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct BlockSerial {
    bytes: [u8; BLOCK_SERIAL_MAX_BYTES],
    len: usize,
}

impl BlockSerial {
    /// Builds a serial from the bytes a device reported, dropping the
    /// trailing NUL padding devices use to fill their fixed-size field.
    ///
    /// Fails when the device reports more bytes than the contract
    /// carries: silently truncating an identifier would make two
    /// different devices compare equal.
    pub fn new(reported: &[u8]) -> Option<Self> {
        let trimmed = match reported.iter().position(|byte| *byte == 0) {
            Some(end) => &reported[..end],
            None => reported,
        };
        if trimmed.len() > BLOCK_SERIAL_MAX_BYTES {
            return None;
        }
        let mut bytes = [0_u8; BLOCK_SERIAL_MAX_BYTES];
        bytes[..trimmed.len()].copy_from_slice(trimmed);
        Some(Self {
            bytes,
            len: trimmed.len(),
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// The serial as text, or `None` when the device reported bytes that
    /// are not UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(self.as_bytes()).ok()
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl core::fmt::Debug for BlockSerial {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.as_str() {
            Some(text) => write!(formatter, "{text:?}"),
            None => write!(formatter, "{:x?}", self.as_bytes()),
        }
    }
}

/// What a block device reports about its own addressing and the shape of
/// the requests it prefers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockGeometry {
    /// Size of one logical block: the unit every block address and every
    /// transfer length is expressed in.
    pub logical_block_bytes: usize,
    /// Base-two logarithm of the number of logical blocks in one
    /// physical block. Zero when a physical block is a logical block.
    pub physical_block_exp: u8,
    /// Smallest transfer the device prefers, in logical blocks. Zero
    /// when the device states no preference.
    pub min_io_blocks: u32,
    /// Transfer size the device performs best at, in logical blocks.
    /// Zero when the device states no preference.
    pub opt_io_blocks: u32,
    /// Most buffers one request may be scattered across.
    pub max_segments: u32,
    /// Most bytes one buffer of a request may carry.
    pub max_segment_bytes: u32,
    /// Capacity in logical blocks.
    pub capacity_blocks: usize,
}

impl BlockGeometry {
    /// Bytes of one physical block: the granularity a write is atomic at.
    pub const fn physical_block_bytes(self) -> usize {
        self.logical_block_bytes << self.physical_block_exp
    }

    /// Capacity in bytes.
    pub const fn capacity_bytes(self) -> u64 {
        self.capacity_blocks as u64 * self.logical_block_bytes as u64
    }
}

/// How many requests a block device can have in flight, and across how
/// many independent queues it spreads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockQueueTopology {
    /// Independent request queues. More than one means requests
    /// submitted from different processors do not contend.
    pub queues: usize,
    /// Requests one queue may keep in flight.
    pub depth: usize,
}

pub trait BlockDevice: Send + Sync {
    fn read_block(
        &self,
        block_id: usize,
        buf: &mut [u8],
    ) -> impl Future<Output = IoResult<()>> + Send;

    fn write_block(&self, block_id: usize, buf: &[u8])
    -> impl Future<Output = IoResult<()>> + Send;

    /// Commits the device's volatile write cache.
    ///
    /// A device without [`BlockDeviceCapabilities::FLUSH`] has no
    /// volatile cache, so every completed write is already durable and
    /// this resolves immediately.
    fn flush(&self) -> impl Future<Output = IoResult<()>> + Send;

    /// Tells the device the blocks in `range` are no longer in use.
    ///
    /// The contents afterwards are unspecified. Fails with
    /// [`IoError::Unsupported`] on a device without
    /// [`BlockDeviceCapabilities::DISCARD`].
    fn discard(&self, range: BlockRange) -> impl Future<Output = IoResult<()>> + Send;

    /// Writes zeroes over `range` without carrying the zero bytes to the
    /// device.
    ///
    /// Fails with [`IoError::Unsupported`] on a device without
    /// [`BlockDeviceCapabilities::WRITE_ZEROES`].
    fn write_zeroes(&self, range: BlockRange) -> impl Future<Output = IoResult<()>> + Send;

    /// Addressing and request-shape properties of this device.
    fn geometry(&self) -> BlockGeometry;

    /// Reads the serial the device reports for itself.
    ///
    /// Reaching the device is a request like any other, so this is async
    /// and may fail with [`IoError::Unsupported`] on a device that does
    /// not answer identification requests.
    fn serial(&self) -> impl Future<Output = IoResult<BlockSerial>> + Send;

    /// Operations this device implements beyond read and write.
    fn capabilities(&self) -> BlockDeviceCapabilities;

    /// Request queues and pipeline depth this device is driven with.
    fn queue_topology(&self) -> BlockQueueTopology;

    fn block_size(&self) -> usize {
        self.geometry().logical_block_bytes
    }

    fn block_count(&self) -> usize {
        self.geometry().capacity_blocks
    }
}
