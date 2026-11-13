pub use crate::io::{IoError, IoResult};

use crate::resource::{KernelResource, ResourceRights};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::bitflags;
use bytes::Bytes;
use core::future::Future;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;

bitflags! {
    /// What a mount of a filesystem may do with it: a mount without
    /// `WRITE` refuses every mutating operation whatever the filesystem
    /// itself would allow.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

// The filesystem contract.
//
// It covers the POSIX file and directory operations (positional I/O,
// `stat`, directory iteration, links, `fsync`) and follows the split the
// Linux VFS draws between an inode, a dentry and an open file:
//
// - The *inode* layer is this trait. A filesystem answers for the objects
//   it stores, named by a [`FsPath`] relative to its own root, and reports
//   their identity ([`NodeId`]: the device and inode numbers `st_dev` and
//   `st_ino` carry) and [`Metadata`].
// - The *dentry* layer, turning a full path into a filesystem and a path
//   inside it, belongs to whoever composes filesystems: the kernel's mount
//   table resolves `..`, mount crossings and symbolic links before a
//   filesystem sees a path, so an [`FsPath`] never holds `.`, `..` or an
//   empty segment.
// - The *open file* layer, an offset and the rights a descriptor was opened
//   with, is the caller's: every data operation here is positional.
//
// Concurrency contract: every method takes `&self` and may be called from
// any processor at once; an implementation serialises its own state.
// Futures borrow their arguments only for the call, so a caller that must
// own a transfer across polls clones the filesystem (every implementation
// is a cheap handle) and the path into an owning future.

/// Errors a filesystem operation reports.
///
/// The set is the POSIX errno subset a filesystem operation can fail
/// with; a layer above maps each variant onto its own codes.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum FsError {
    #[error("no such file or directory")]
    NoEntry,
    #[error("file exists")]
    Exists,
    #[error("not a directory")]
    NotDirectory,
    #[error("is a directory")]
    IsDirectory,
    #[error("directory not empty")]
    NotEmpty,
    #[error("read-only filesystem object")]
    ReadOnly,
    #[error("operation not permitted")]
    NotPermitted,
    #[error("link or rename across filesystems")]
    CrossDevice,
    /// The object is in use in a way that forbids the change: a mount
    /// point, a directory holding one, or a mount something still holds.
    #[error("resource busy")]
    Busy,
    #[error("too many levels of symbolic links")]
    Loop,
    #[error("invalid argument")]
    Invalid,
    #[error("value too large for the filesystem")]
    Overflow,
    #[error("name is not valid UTF-8")]
    IllegalByteSequence,
    /// A name longer than the filesystem can store or carry.
    #[error("file name too long")]
    NameTooLong,
    #[error("operation not supported by this filesystem")]
    Unsupported,
    #[error("input/output error")]
    Io,
}

pub type FsResult<T> = Result<T, FsError>;

/// What kind of object a path names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Directory,
    File,
    Symlink,
}

/// Names one filesystem instance: what `st_dev` reports.
///
/// Two objects on the same device with the same inode number are the same
/// object. Zero is reserved, so a zeroed field is never a valid device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(u64);

impl DeviceId {
    /// Panics on zero, the reserved value.
    pub const fn new(raw: u64) -> Self {
        assert!(raw != 0, "device id zero is reserved");
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// The identity of one filesystem object: `st_dev` and `st_ino`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId {
    pub device: DeviceId,
    pub inode: u64,
}

impl NodeId {
    pub const fn new(device: DeviceId, inode: u64) -> Self {
        Self { device, inode }
    }
}

/// What `stat` reports about one object.
///
/// Times are nanoseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub id: NodeId,
    pub kind: NodeKind,
    pub link_count: u64,
    /// Bytes of a file, bytes of a symbolic link's target, zero for a
    /// directory.
    pub size: u64,
    pub access_nanos: u64,
    pub modified_nanos: u64,
    pub status_nanos: u64,
}

bitflags! {
    /// How [`FileSystem::open`] treats the object at a path.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct OpenFlags: u8 {
        /// Create a regular file when nothing exists at the path.
        const CREATE = 1 << 0;
        /// With `CREATE`, fail with [`FsError::Exists`] when something
        /// already exists at the path.
        const EXCLUSIVE = 1 << 1;
        /// Truncate an existing regular file to zero bytes.
        const TRUNCATE = 1 << 2;
        /// The object must be a directory.
        const DIRECTORY = 1 << 3;
    }
}

/// Where [`FileSystem::prepare_write`] puts its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WritePosition {
    /// At this offset, zero-filling any gap past the end of the file.
    At(u64),
    /// At the end of the file as the filesystem sees it when the write
    /// arrives.
    End,
}

/// What a [`PreparedWrite::submit`] leaves of its write.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteProgress<W> {
    /// The round landed and bytes remain: the next round, staged and
    /// waiting for its own submit.
    More(W),
    /// Every byte landed; the write began at this offset.
    Done(u64),
}

/// A write [`FileSystem::prepare_write`] staged.
///
/// A write lands in rounds. Each round stages [`Self::staged_len`] bytes
/// of the write in the filesystem's own destination, and its submit
/// fills and sends exactly those. A filesystem that keeps its bytes in
/// memory stages the whole write in one round; one that sends them
/// elsewhere bounds a round by the destinations it can hold at once,
/// never by the write's length, so a write larger than that lands in
/// several rounds over one prepare. The rounds' lengths sum to the
/// prepared length, and the rounds land in order at consecutive offsets.
///
/// Dropping a prepared write before its submit releases what it staged
/// and leaves the file as it was: the bytes of earlier rounds have
/// landed, this round's have not. Dropping a submit's future part way
/// may leave part of its round written.
///
/// Concurrency: a prepared write is owned by the task that prepared it
/// and is moved, never shared; it may be submitted on another
/// processor. Two writes to one file staged at once land in whatever
/// order their submits do.
pub trait PreparedWrite: Send + Sized {
    /// The bytes the next submit takes: all of the write's remaining
    /// bytes, or the most this filesystem stages in one round.
    fn staged_len(&self) -> usize;

    /// Fills this round and sends it.
    ///
    /// `fill` runs before this returns, never from the future, so it
    /// may read memory that is valid only until the next await (a
    /// guest's) and need not be `Send`. It is called once for each of the
    /// round's destination regions, in write order, with the region's
    /// offset from the start of the whole write; the regions cover the
    /// round's [`Self::staged_len`] bytes exactly. A round refused before
    /// its bytes are needed does not run it.
    fn submit<F>(self, fill: F) -> impl Future<Output = FsResult<WriteProgress<Self>>> + Send
    where
        F: FnMut(usize, &mut [u8]);
}

/// The write of a filesystem that refuses every write before it is
/// staged.
impl PreparedWrite for core::convert::Infallible {
    fn staged_len(&self) -> usize {
        match *self {}
    }

    fn submit<F>(self, fill: F) -> impl Future<Output = FsResult<WriteProgress<Self>>> + Send
    where
        F: FnMut(usize, &mut [u8]),
    {
        // Not `async fn`: that would carry `fill`, which need not be
        // `Send`, into the future.
        drop(fill);
        async move { match self {} }
    }
}

/// Writes `len` bytes at `position` and resolves with the offset they
/// landed at, staging them through [`FileSystem::prepare_write`] and
/// landing every round.
///
/// `source` copies the bytes of the write that start at its first
/// argument into its second, straight into the filesystem's destination,
/// so a write copies its bytes once. It runs between awaits, never
/// across one, and once per destination region; the memory it reads must
/// stay valid for the whole call.
pub async fn write_with<F, S>(
    fs: &F,
    path: FsPath<'_>,
    position: WritePosition,
    len: usize,
    now: u64,
    mut source: S,
) -> FsResult<u64>
where
    F: FileSystem,
    S: FnMut(usize, &mut [u8]),
{
    let mut write = fs.prepare_write(path, position, len, now).await?;
    loop {
        match write.submit(&mut source).await? {
            WriteProgress::More(next) => write = next,
            WriteProgress::Done(landed) => return Ok(landed),
        }
    }
}

/// Where a directory listing resumes.
///
/// Opaque to the caller: it is [`DirCookie::START`] or a value a previous
/// [`DirEntry::next`] returned, and it stays valid across calls the way a
/// POSIX `telldir` position does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DirCookie(u64);

impl DirCookie {
    pub const START: Self = Self(0);

    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: NodeKind,
    /// The entry's inode number, or zero from a filesystem whose listing
    /// does not carry one (a 9p share lists names and types only).
    pub inode: u64,
    /// The cookie that resumes the listing after this entry.
    pub next: DirCookie,
}

/// A path relative to the root of one filesystem.
///
/// Holds no leading `/`, no empty segment, and no `.` or `..`: those are
/// resolved by the layer that composes filesystems, before a path reaches
/// one. The empty path names the filesystem's root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FsPath<'a>(&'a str);

impl<'a> FsPath<'a> {
    pub const ROOT: FsPath<'static> = FsPath("");

    /// Validates `path`, or `None` when it carries a leading or trailing
    /// `/`, an empty segment, `.` or `..`.
    pub fn new(path: &'a str) -> Option<Self> {
        if path.is_empty() {
            return Some(Self(path));
        }
        path.split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
            .then_some(Self(path))
    }

    pub const fn as_str(self) -> &'a str {
        self.0
    }

    pub const fn is_root(self) -> bool {
        self.0.is_empty()
    }

    /// The parent directory and the final name, or `None` for the root.
    pub fn split_last(self) -> Option<(FsPath<'a>, &'a str)> {
        if self.is_root() {
            return None;
        }
        Some(match self.0.rsplit_once('/') {
            Some((parent, name)) => (Self(parent), name),
            None => (FsPath::ROOT, self.0),
        })
    }

    /// Whether `self` is `ancestor` or lies below it.
    pub fn starts_with(self, ancestor: FsPath<'_>) -> bool {
        if ancestor.is_root() {
            return true;
        }
        match self.0.strip_prefix(ancestor.0) {
            Some(rest) => rest.is_empty() || rest.starts_with('/'),
            None => false,
        }
    }
}

/// One filesystem: the operations every mount kind implements.
///
/// Mutating operations take `now`, the caller's wall-clock time in
/// nanoseconds since the Unix epoch, which a filesystem without a clock of
/// its own stamps on what it changes; one that keeps its own time (a host
/// share) ignores it. Symbolic links are never followed here: `stat`
/// describes the link itself, and resolving a link is the caller's job.
pub trait FileSystem: Clone + Send + Sync + 'static {
    /// Describes the object at `path` without following a final symbolic
    /// link.
    fn stat<'a>(&'a self, path: FsPath<'a>)
    -> impl Future<Output = FsResult<Metadata>> + Send + 'a;

    /// Opens the object at `path` as `flags` asks, creating or truncating
    /// a regular file, and describes what was opened.
    fn open<'a>(
        &'a self,
        path: FsPath<'a>,
        flags: OpenFlags,
        now: u64,
    ) -> impl Future<Output = FsResult<Metadata>> + Send + 'a;

    /// Reads at most `max_bytes` from `offset`. An empty result is end of
    /// file.
    fn read_at<'a>(
        &'a self,
        path: FsPath<'a>,
        offset: u64,
        max_bytes: usize,
    ) -> impl Future<Output = FsResult<Bytes>> + Send + 'a;

    /// The write [`Self::prepare_write`] stages.
    type Write<'a>: PreparedWrite
    where
        Self: 'a;

    /// Stages a write of `len` bytes at `position`: everything the write
    /// needs before its bytes (resolving the file, an append's offset,
    /// the destination the bytes go into) is done here, and the bytes
    /// themselves go in through [`PreparedWrite::submit`].
    ///
    /// A filesystem that keeps its bytes in memory resolves at once; one
    /// that sends them elsewhere awaits whatever its destination needs
    /// first. A write refused here has run no filler.
    fn prepare_write<'a>(
        &'a self,
        path: FsPath<'a>,
        position: WritePosition,
        len: usize,
        now: u64,
    ) -> impl Future<Output = FsResult<Self::Write<'a>>> + Send + 'a;

    /// Truncates or zero-extends a regular file to `size` bytes.
    fn set_size<'a>(
        &'a self,
        path: FsPath<'a>,
        size: u64,
        now: u64,
    ) -> impl Future<Output = FsResult<()>> + Send + 'a;

    /// Sets the access and modification times; `None` leaves one as it is.
    fn set_times<'a>(
        &'a self,
        path: FsPath<'a>,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
        now: u64,
    ) -> impl Future<Output = FsResult<()>> + Send + 'a;

    /// Lists at most `max_entries` entries of the directory at `path`,
    /// resuming at `cookie`. Fewer than `max_entries` means the listing is
    /// complete. `.` and `..` are not listed.
    fn read_dir<'a>(
        &'a self,
        path: FsPath<'a>,
        cookie: DirCookie,
        max_entries: usize,
    ) -> impl Future<Output = FsResult<Vec<DirEntry>>> + Send + 'a;

    fn create_directory<'a>(
        &'a self,
        path: FsPath<'a>,
        now: u64,
    ) -> impl Future<Output = FsResult<()>> + Send + 'a;

    /// Removes an empty directory.
    fn remove_directory<'a>(
        &'a self,
        path: FsPath<'a>,
    ) -> impl Future<Output = FsResult<()>> + Send + 'a;

    /// Removes a non-directory name.
    fn unlink<'a>(&'a self, path: FsPath<'a>) -> impl Future<Output = FsResult<()>> + Send + 'a;

    /// Moves `source` to `destination` within this filesystem.
    fn rename<'a>(
        &'a self,
        source: FsPath<'a>,
        destination: FsPath<'a>,
        now: u64,
    ) -> impl Future<Output = FsResult<()>> + Send + 'a;

    /// Gives the non-directory at `existing` the further name `link`.
    fn link<'a>(
        &'a self,
        existing: FsPath<'a>,
        link: FsPath<'a>,
        now: u64,
    ) -> impl Future<Output = FsResult<()>> + Send + 'a;

    /// Creates a symbolic link at `link` whose target text is `target`.
    fn symlink<'a>(
        &'a self,
        target: &'a str,
        link: FsPath<'a>,
        now: u64,
    ) -> impl Future<Output = FsResult<()>> + Send + 'a;

    /// The target text of the symbolic link at `path`.
    fn read_link<'a>(
        &'a self,
        path: FsPath<'a>,
    ) -> impl Future<Output = FsResult<String>> + Send + 'a;

    /// Makes the file's data and metadata durable.
    fn sync<'a>(&'a self, path: FsPath<'a>) -> impl Future<Output = FsResult<()>> + Send + 'a;

    /// Makes the file's data durable, and the metadata needed to read it
    /// back.
    fn sync_data<'a>(&'a self, path: FsPath<'a>) -> impl Future<Output = FsResult<()>> + Send + 'a;
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
    /// Most buffers one request may be scattered across: the device's
    /// limit, or the driver's when the driver carries fewer.
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

/// The memory one block request carries to or from the device.
///
/// A request owns its buffer from the moment the device accepts it until
/// the device answers: the driver keeps the buffer with the request in
/// its in-flight table, and the context that drains the answer calls
/// [`BlockBuffer::complete`] exactly once, before the buffer goes back to
/// its waiter or, when nobody waits any more, is dropped. Dropping the
/// future that submitted a request therefore never frees memory the
/// device may still read or write, and never leaves the buffer's owner
/// without the answer. This is Linux's bio, which holds its pages and
/// whose `bi_end_io` runs from the completion path, and io_uring's owned
/// buffers as tokio-uring and monoio hand them to the kernel.
///
/// A driver refuses a request it cannot issue (a missing right, a range
/// past the end, a shape the device cannot carry) by calling `complete`
/// with the error, so every buffer a submission took is completed once.
/// A buffer is dropped uncompleted only when the submission future is
/// dropped before the device accepted the request: then the device never
/// saw it.
///
/// Concurrency contract: `complete` runs on any processor, either in the
/// device's interrupt handler or in whichever task drained the device's
/// answer, and with no lock of the driver held. It must not block, must
/// not take a lock an interrupt handler cannot take, and must not wait on
/// another processor. Dropping a buffer the driver no longer needs runs
/// in the same contexts.
pub trait BlockBuffer: Send + 'static {
    /// The memory the request transfers, as runs laid end to end: the
    /// first run holds the request's first bytes. Their total is a whole
    /// number of logical blocks.
    ///
    /// Until `complete` runs, the runs stay where they are, nobody but
    /// the device writes them, and for a read nobody reads them either.
    fn segments(&self) -> impl Iterator<Item = NonNull<[u8]>> + '_;

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
/// holder fills and reads them: a scratch page, as opposed to a cache's
/// own blocks.
pub trait OwnedBlockBuffer: BlockBuffer + Sized {
    /// Has the buffer raise `flag` in [`BlockBuffer::complete`], after
    /// whatever else completing it does. A buffer dropped uncompleted —
    /// its submission dropped before the device accepted it — never
    /// raises it.
    fn raise_on_answer(&mut self, flag: AnswerFlag);

    /// A buffer of `len` zero bytes, or `None` when one buffer cannot
    /// carry that many.
    fn zeroed(len: usize) -> Option<Self>;

    /// Bytes the buffer holds.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies `src` over the buffer's first `src.len()` bytes.
    ///
    /// # Panics
    ///
    /// When `src` is longer than the buffer.
    fn copy_from(&mut self, src: &[u8]);

    /// Copies the buffer's first `dst.len()` bytes into `dst`.
    ///
    /// # Panics
    ///
    /// When `dst` is longer than the buffer.
    fn copy_to(&self, dst: &mut [u8]);
}

/// A finished request: the device's answer, and the buffer the request
/// carried, handed back to the waiter.
#[derive(Debug)]
pub struct BlockCompletion<Buffer> {
    pub result: IoResult<()>,
    pub buffer: Buffer,
}

/// A block device.
///
/// Concurrency contract: every operation may be called from any task on
/// any processor, concurrently. A data request is submitted in two
/// steps. Submission takes the buffer, waits only for room in the
/// device's queue, and resolves to a waiter once the device has the
/// request; the waiter resolves to the answer. Handing the device the
/// request and resolving are one step, so a submission dropped before it
/// resolves never reached the device and drops its buffer uncompleted.
/// Dropping the waiter
/// detaches it and changes nothing about the request: the device still
/// carries it out, the driver still completes the buffer (see
/// [`BlockBuffer`]) and then drops it. The operations that carry no
/// caller memory — flush, discard, write-zeroes, identification — are
/// one future each, and dropping one in flight detaches it the same way.
pub trait BlockDevice: Send + Sync {
    /// The buffers this device's data requests carry.
    type Buffer: BlockBuffer;

    /// A request the device has accepted, until its answer is collected.
    type Waiter<'a>: Future<Output = BlockCompletion<Self::Buffer>> + Send + 'a
    where
        Self: 'a;

    /// Submits a read of the run of logical blocks that starts at
    /// `block_id` into `buffer`.
    ///
    /// The run travels as one request, so `buffer` fits one:
    /// [`BlockGeometry::max_segments`] runs of at most
    /// [`BlockGeometry::max_segment_bytes`] each once its segments are
    /// cut at that size. A buffer that does not is refused.
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
    ) -> impl Future<Output = BlockCompletion<Self::Buffer>> + Send + '_ {
        async move { self.submit_read(block_id, buffer).await.await }
    }

    /// [`BlockDevice::submit_write`] and its waiter, as one future.
    fn write(
        &self,
        block_id: usize,
        buffer: Self::Buffer,
    ) -> impl Future<Output = BlockCompletion<Self::Buffer>> + Send + '_ {
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

#[cfg(test)]
mod tests {
    use super::FsPath;

    #[test]
    fn fs_path_refuses_what_resolution_must_remove() {
        for refused in ["/a", "a/", "a//b", ".", "a/./b", "..", "a/../b"] {
            assert_eq!(FsPath::new(refused), None, "{refused:?} must be refused");
        }
        for accepted in ["", "a", "a/b", "a/.b", "a..b"] {
            assert!(
                FsPath::new(accepted).is_some(),
                "{accepted:?} must be accepted"
            );
        }
    }

    #[test]
    fn fs_path_splits_and_compares_by_segment() {
        let path = FsPath::new("usr/lib/python").expect("valid path");
        let (parent, name) = path.split_last().expect("not the root");
        assert_eq!((parent.as_str(), name), ("usr/lib", "python"));
        assert_eq!(
            FsPath::new("bin").and_then(FsPath::split_last),
            Some((FsPath::ROOT, "bin"))
        );
        assert_eq!(FsPath::ROOT.split_last(), None);
        assert!(path.starts_with(FsPath::new("usr").expect("valid path")));
        assert!(path.starts_with(FsPath::ROOT));
        assert!(!path.starts_with(FsPath::new("us").expect("valid path")));
    }
}
