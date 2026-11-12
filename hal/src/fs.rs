pub use crate::io::{IoError, IoResult};

use crate::dma::BufResult;
use crate::resource::{KernelResource, ResourceRights};
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::bitflags;
use core::future::Future;
use core::sync::atomic::{AtomicBool, Ordering};

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
    /// Most bytes one request may transfer: a whole number of logical
    /// blocks. It is the device's limit, the driver's chain bound and the
    /// longest contiguous run the driver's own memory serves
    /// ([`crate::dma::DmaMemory::max_run_bytes`]), whichever is least.
    pub max_transfer_bytes: usize,
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

/// The memory one block request carries to or from the device.
///
/// A request owns its buffer from the moment the device accepts it until
/// the device answers, the ownership [`crate::dma`]'s "Buffers a request
/// owns" states for every request that transfers data: the driver keeps
/// the buffer with the request in its in-flight table, and the context
/// that drains the answer calls [`BlockBuffer::complete`] exactly once,
/// before the buffer goes back to its waiter in the request's
/// [`BufResult`] or, when nobody waits any more, is dropped. Dropping the
/// future that submitted a request therefore never frees memory the
/// device may still read or write, and never leaves the buffer's owner
/// without the answer. This is Linux's bio, which holds its pages and
/// whose `bi_end_io` runs from the completion path, and io_uring's owned
/// buffers as tokio-uring and monoio hand them to the kernel.
///
/// A request names no address. The buffer gives the bytes of a write to
/// the driver by [`BlockBuffer::fill`] and takes the bytes of a read from
/// it by [`BlockBuffer::deliver`]; the driver moves them between the
/// buffer and memory of its own that the device reaches, so a device
/// never sees the consumer's memory, and the consumer's memory never
/// holds bytes a device may still write. This is a bounce buffer, as
/// Linux's swiotlb makes of a streaming mapping and Xen's blkfront makes
/// of the pages it grants an untrusted backend.
///
/// A driver refuses a request it cannot issue (a missing right, a range
/// past the end, a shape the device cannot carry) by calling `complete`
/// with the error, so every buffer a submission took is completed once.
/// A buffer is dropped uncompleted only when the submission future is
/// dropped before the device accepted the request: then the device never
/// saw it.
///
/// Concurrency contract: `len` may be called from any context. `fill`
/// runs once per write, in the submitting task's context and on its
/// processor, before the device is given the request and with no lock of
/// the driver held, so it may copy as much as one request carries.
/// `deliver` runs once per successful read, before `complete(Ok(()))` and
/// never on a failure, in the contexts `complete` runs in: on any
/// processor, either in the device's interrupt handler or in whichever
/// task drained the device's answer, with no lock of the driver held. It
/// only copies: it must not block, must not allocate, must not take a lock
/// an interrupt handler cannot take, and must not wait on another
/// processor. `complete` runs in the same contexts under the same
/// restrictions, and so does dropping a buffer the driver no longer
/// needs.
pub trait BlockBuffer: Send + 'static {
    /// Bytes the request transfers: a whole number of logical blocks.
    fn len(&self) -> usize;

    /// Whether the request transfers nothing.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Writes the bytes of a write into `to`, which is exactly
    /// [`BlockBuffer::len`] bytes of the driver's own memory. Called
    /// before the device is given the request.
    fn fill(&mut self, to: &mut [u8]);

    /// Takes the bytes of a read as the device wrote them: `from` is
    /// exactly [`BlockBuffer::len`] bytes of the driver's own memory.
    /// Called once, before `complete(Ok(()))`, and never when the request
    /// fails: a failed or short read delivers nothing.
    fn deliver(&mut self, from: &[u8]);

    /// The device's answer to the request this buffer carried, or the
    /// driver's refusal of it.
    fn complete(&mut self, result: IoResult<()>);
}

/// A flag the buffer of one request raises once the request is over: the
/// device answered it, or the driver refused it, so nothing of it can
/// still reach the device's blocks. It is raised whether or not a task
/// still waits, which is what lets the holder of a dropped submission's
/// storage learn when that storage is safe to reuse.
///
/// Concurrency contract: one buffer raises the flag, from the contexts
/// [`BlockBuffer::complete`] runs in, with a single release store; any
/// number of clones on any processor read it lock-free.
#[derive(Clone, Debug, Default)]
pub struct AnswerFlag(Arc<AtomicBool>);

impl AnswerFlag {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the request answered.
    pub fn raise(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether the request is over. Once this answers `true`, everything
    /// the device did for the request happened before.
    pub fn is_raised(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// A [`BlockBuffer`] whose bytes belong to the buffer itself, so its
/// holder fills and reads them through its byte views: a scratch or swap
/// page, as opposed to a cache's own blocks.
///
/// Its bytes are one run of kernel memory, and a request copies them to and
/// from the driver's own memory like every other request
/// ([`BlockBuffer::fill`], [`BlockBuffer::deliver`]). Its byte views are
/// that run's, and like every request buffer's they are not touched while a
/// request carries the buffer.
pub trait OwnedBlockBuffer: BlockBuffer + AsRef<[u8]> + AsMut<[u8]> {
    /// Has the buffer raise `flag` in [`BlockBuffer::complete`], after
    /// whatever else completing it does. A buffer dropped uncompleted —
    /// its submission dropped before the device accepted it — never
    /// raises it.
    fn raise_on_answer(&mut self, flag: AnswerFlag);

    /// A buffer of `len` zero bytes: one run of kernel memory.
    fn zeroed(len: usize) -> Self;
}

/// A block device.
///
/// Concurrency contract: every operation may be called from any task on
/// any processor, concurrently. A data request is submitted in two
/// steps. Submission takes the buffer, waits only for room in the
/// device's queue, and resolves to a waiter once the device has the
/// request; the waiter resolves to the answer, which hands the buffer
/// back whatever the answer was. Handing the device the request and
/// resolving are one step, so a submission dropped before it resolves
/// never reached the device and drops its buffer uncompleted. Dropping
/// the waiter detaches it and changes nothing about the request: the
/// device still carries it out, the driver still completes the buffer
/// (see [`BlockBuffer`]) and then drops it. The operations that carry no
/// caller memory — flush, discard, write-zeroes, identification — are
/// one future each, and dropping one in flight detaches it the same way.
///
/// A block request's failure always carries its buffer back
/// ([`crate::dma::BufError::buffers`] is `Some`): a driver whose device
/// broke the queue contract resets the device before it fails the
/// requests in flight, so no device is still using a buffer it hands
/// back.
pub trait BlockDevice: Send + Sync {
    /// The buffers this device's data requests carry.
    type Buffer: BlockBuffer;

    /// A request the device has accepted, until its answer is collected.
    type Waiter<'a>: Future<Output = BufResult<(), Self::Buffer>> + Send + 'a
    where
        Self: 'a;

    /// Submits a read of the run of logical blocks that starts at
    /// `block_id` into `buffer`.
    ///
    /// The run travels as one request, so `buffer` fits one: its
    /// [`BlockBuffer::len`] is at most [`BlockGeometry::max_transfer_bytes`].
    /// A buffer that is longer is refused.
    fn submit_read(
        &self,
        block_id: usize,
        buffer: Self::Buffer,
    ) -> impl Future<Output = Self::Waiter<'_>> + Send + '_;

    /// Submits a write of `buffer` over the run of logical blocks that
    /// starts at `block_id`, with the request shape
    /// [`BlockDevice::submit_read`] describes.
    fn submit_write(
        &self,
        block_id: usize,
        buffer: Self::Buffer,
    ) -> impl Future<Output = Self::Waiter<'_>> + Send + '_;

    /// [`BlockDevice::submit_read`] and its waiter, as one future.
    fn read(
        &self,
        block_id: usize,
        buffer: Self::Buffer,
    ) -> impl Future<Output = BufResult<(), Self::Buffer>> + Send + '_ {
        async move { self.submit_read(block_id, buffer).await.await }
    }

    /// [`BlockDevice::submit_write`] and its waiter, as one future.
    fn write(
        &self,
        block_id: usize,
        buffer: Self::Buffer,
    ) -> impl Future<Output = BufResult<(), Self::Buffer>> + Send + '_ {
        async move { self.submit_write(block_id, buffer).await.await }
    }

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

    /// Whether the device refuses every write, as a medium the platform
    /// attached read-only does.
    ///
    /// A caller that would only ever fail on its first write — a
    /// read-write mount, a swap area — asks here instead.
    fn is_read_only(&self) -> bool;

    fn block_size(&self) -> usize {
        self.geometry().logical_block_bytes
    }

    fn block_count(&self) -> usize {
        self.geometry().capacity_blocks
    }
}
