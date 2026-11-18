//! Btrfs over the block cache. Reads pin a published immutable view under a
//! short lock, released before I/O. A kernel task exclusively owns the
//! running transaction: a change is visible once its batch is applied and
//! durable at the next commit, which a sync, the commit interval, the dirty
//! bound or the unmount triggers. Per-inode write leases serialize write
//! rounds without blocking unrelated files or the service.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc as SharedArc, Weak as SharedWeak};
use alloc::vec::Vec;
use core::num::NonZeroUsize;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use bytes::{Bytes, BytesMut};
use helios_btrfs::{
    BlockSink, BlockSource, DirPosition, EntryTarget, Error, FileKind, Filesystem, Inode,
    READ_ALIGNMENT, Timestamp,
};
use helios_hal::fs::{
    BlockDevice, BlockSerial, DeviceId, DirCookie, DirEntry, FileSystem, FsError, FsPath, FsResult,
    Metadata, NodeId, NodeKind, OpenFlags, RenameMode, WritePosition,
};
use helios_hal::io::{IoError, IoResult};
use triomphe::Arc;

use crate::{
    BlockCacheError, BlockPages, CACHE_BLOCK_BYTES, CachedDisk, ProgressSignal, RootEntropyHandle,
};
use helios_btrfs::write::{
    CompressionPolicy, Compressor, DEFAULT_DIRTY_METADATA_LIMIT, DEFAULT_MAX_INLINE, EncodeError,
    WriteError, Writer,
};

mod prepared;
mod writer;
pub use prepared::BtrfsWrite;
pub use writer::BtrfsService;
use writer::{Operation, Response};

/// Runtime inputs for writable disk mounts.
#[derive(Clone)]
pub struct BtrfsWriteOptions {
    pub entropy: RootEntropyHandle,
    pub concurrency: NonZeroUsize,
    /// Linux's `compress=<algorithm>[:<level>]` mount option: the algorithm
    /// every compressible inode's new data is written with. `None` writes
    /// every extent uncompressed.
    pub compress: Option<CompressionPolicy>,
    /// When the mount commits without a sync asking: Linux's defaults when
    /// `None`, and a request naming it for a source other than btrfs is
    /// refused. The mount takes it as its starting value;
    /// [`BtrfsFs::set_commit_policy`] changes it later.
    pub commit: Option<CommitPolicy>,
    /// Linux's `max_inline=` mount option, in bytes: the most a file's
    /// stored data may keep inline in its metadata item. `None` takes
    /// Linux's default ([`DEFAULT_MAX_INLINE`]), and a request naming it for
    /// a source other than btrfs is refused. [`BtrfsFs::set_max_inline`]
    /// changes it later.
    pub max_inline: Option<u64>,
    /// Wall-clock nanoseconds since the Unix epoch at the mount request:
    /// what the replay of a log tree a crash left stamps on what it unlinks.
    pub now: u64,
}

/// What the machine keeps of a writable btrfs mount, whatever its device
/// type, so that it can commit every one before it powers off. It holds the
/// mount's queue weakly: an unmounted or dropped mount is not kept alive by
/// it, and its end is still awaited.
#[derive(Clone)]
pub(crate) struct WritableMount {
    writer: SharedWeak<writer::Client>,
    exit: SharedArc<writer::ServiceExit>,
}

impl WritableMount {
    /// Whether the mount's service has ended, so nothing is left to commit.
    pub(crate) fn has_ended(&self) -> bool {
        self.exit.has_ended()
    }

    /// Makes what the mount holds durable: a sync of a live mount, or the
    /// end of one that is going away, whose service commits as it ends.
    pub(crate) async fn settle(&self) {
        match self.writer.upgrade() {
            Some(writer) => {
                if let Err(error) = writer.execute("", 0, Operation::Sync).await {
                    tracing::error!(?error, "btrfs commit before power-off failed");
                }
            }
            None => self.exit.ended().await,
        }
    }
}

/// When a writable mount commits its running transaction beyond the syncs
/// that ask for one: operator settings, adjustable at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitPolicy {
    /// Linux's `commit=` mount option: the longest a change waits after the
    /// running transaction first changed. Zero commits each batch.
    pub interval: Duration,
    /// Dirty metadata the running transaction holds before it commits.
    pub dirty_metadata_limit: u64,
}

/// `BTRFS_DEFAULT_COMMIT_INTERVAL`, 30 seconds in `fs/btrfs/fs.h`.
pub const DEFAULT_COMMIT_INTERVAL: Duration = Duration::from_secs(30);

/// `BTRFS_WARNING_COMMIT_INTERVAL`, 300 seconds in `fs/btrfs/fs.h`: a longer
/// `commit=` is taken with a warning.
const WARNING_COMMIT_INTERVAL: u32 = 300;

impl CommitPolicy {
    /// The policy Linux's `commit=<seconds>` mount option gives, as
    /// `btrfs_parse_param` reads it: zero is the default interval, and an
    /// interval past [`WARNING_COMMIT_INTERVAL`] is warned about and kept.
    pub fn from_commit_option(seconds: u32) -> Self {
        if seconds > WARNING_COMMIT_INTERVAL {
            tracing::warn!(seconds, "excessive commit interval, use with care");
        }
        Self {
            interval: match seconds {
                0 => DEFAULT_COMMIT_INTERVAL,
                seconds => Duration::from_secs(u64::from(seconds)),
            },
            ..Self::default()
        }
    }
}

impl Default for CommitPolicy {
    /// Linux's defaults.
    fn default() -> Self {
        Self {
            interval: DEFAULT_COMMIT_INTERVAL,
            dirty_metadata_limit: DEFAULT_DIRTY_METADATA_LIMIT,
        }
    }
}

/// A mount's operator settings as they stand: its [`CommitPolicy`] and its
/// `max_inline`. The operator writes them from any processor. The mount's
/// service re-reads the commit policy on every change, which applies the new
/// values to the running transaction at once, and reads `max_inline` for
/// every batch of mutations.
struct LivePolicy {
    interval_nanos: AtomicU64,
    dirty_metadata_limit: AtomicU64,
    max_inline: AtomicU64,
    /// Raised after every commit policy change, so the service re-arms its
    /// deadline.
    changed: ProgressSignal,
}

impl LivePolicy {
    fn new(policy: CommitPolicy, max_inline: u64) -> Self {
        let live = Self {
            interval_nanos: AtomicU64::new(0),
            dirty_metadata_limit: AtomicU64::new(0),
            max_inline: AtomicU64::new(max_inline),
            changed: ProgressSignal::new(),
        };
        live.store(policy);
        live
    }

    fn max_inline(&self) -> u64 {
        self.max_inline.load(Ordering::Acquire)
    }

    fn store(&self, policy: CommitPolicy) {
        // An interval past u64 nanoseconds, some 584 years, never expires.
        let nanos = u64::try_from(policy.interval.as_nanos()).unwrap_or(u64::MAX);
        self.interval_nanos.store(nanos, Ordering::Release);
        self.dirty_metadata_limit
            .store(policy.dirty_metadata_limit, Ordering::Release);
        self.changed.signal();
    }

    fn load(&self) -> CommitPolicy {
        CommitPolicy {
            interval: Duration::from_nanos(self.interval_nanos.load(Ordering::Acquire)),
            dirty_metadata_limit: self.dirty_metadata_limit.load(Ordering::Acquire),
        }
    }
}
type Snapshot<Device> = Filesystem<SharedArc<Image<Device>>>;

#[derive(Debug, thiserror::Error)]
pub enum BtrfsMountError {
    #[error("disk already has an incompatible mount")]
    Busy,
    #[error(transparent)]
    Read(#[from] Error),
    #[error(transparent)]
    Write(#[from] WriteError),
    #[error("the mount's compression policy cannot be encoded: {0}")]
    Compression(#[from] EncodeError),
}

// A writable mount excludes independent read snapshots: those snapshots have
// no shared retirement accounting with its writer. Clones of this mount share
// both the image lease and the library's generation pins.
enum MountLease {
    Read {
        _lease: crate::OwnedRawRwLockReadLease,
    },
    Write {
        _lease: crate::OwnedRawRwLockWriteLease,
    },
}

struct Image<Device> {
    disk: CachedDisk<Device>,
    _lease: MountLease,
}

impl<Device: BlockDevice<Buffer = BlockPages>> BlockSource for Image<Device> {
    fn size_bytes(&self) -> u64 {
        self.disk.capacity_blocks() * CACHE_BLOCK_BYTES as u64
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> IoResult<()> {
        self.disk
            .read_at(offset, buf)
            .await
            .map_err(|error| self.io_error(error))
    }
}

impl<Device: BlockDevice<Buffer = BlockPages>> Image<Device> {
    fn io_error(&self, error: BlockCacheError) -> IoError {
        tracing::warn!(%error, serial = ?self.disk.serial(), "btrfs device operation failed");
        match error {
            BlockCacheError::Device(error) => error,
            BlockCacheError::OutOfRange { .. } | BlockCacheError::ByteRangeOutOfRange { .. } => {
                IoError::OutOfBounds
            }
            BlockCacheError::ReadOnly => IoError::ReadOnly,
            BlockCacheError::Aborted => IoError::DeviceFault,
        }
    }
}

impl<Device: BlockDevice<Buffer = BlockPages>> BlockSink for Image<Device> {
    async fn stage(&self, block: u64, bytes: &[u8; READ_ALIGNMENT as usize]) -> IoResult<()> {
        let mut cached = self
            .disk
            .overwrite(block)
            .await
            .map_err(|error| self.io_error(error))?;
        cached.copy_from_slice(bytes);
        Ok(())
    }

    async fn commit(&self, block: u64, bytes: &[u8; READ_ALIGNMENT as usize]) -> IoResult<()> {
        self.disk
            .commit(block, |cached| cached.copy_from_slice(bytes))
            .await
            .map_err(|error| self.io_error(error))
    }
}

// Both transports address complete cache blocks; no partial-block write can
// accidentally make a superblock visible before its transaction is durable.
const _: () = assert!(READ_ALIGNMENT as usize == CACHE_BLOCK_BYTES);

/// A cloneable Btrfs mount with concurrent snapshot reads.
pub struct BtrfsFs<Device: BlockDevice<Buffer = BlockPages>> {
    inner: Arc<Inner<Device>>,
}

struct Inner<Device: BlockDevice<Buffer = BlockPages>> {
    reader: Arc<spin::Mutex<Snapshot<Device>>>,
    /// A writable mount's service queue, shared with the machine's list of
    /// writable mounts (`WritableMount`) by a weak reference.
    writer: Option<SharedArc<writer::Client>>,
    /// The commit policy a writable mount's service follows.
    policy: Option<Arc<LivePolicy>>,
    devices: BTreeMap<u64, (DeviceId, u64)>,
    serial: BlockSerial,
    writes: prepared::Leases,
    write_round: usize,
}

impl<Device: BlockDevice<Buffer = BlockPages>> Clone for BtrfsFs<Device> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<Device: BlockDevice<Buffer = BlockPages>> BtrfsFs<Device> {
    /// Validates the image and assigns each subvolume an identity from
    /// the same allocator as every other mount on this machine.
    pub async fn mount(
        disk: CachedDisk<Device>,
        mut allocate_device: impl FnMut() -> DeviceId + Send,
    ) -> Result<Self, BtrfsMountError> {
        let serial = disk.serial();
        let lease = disk.read_mount().ok_or(BtrfsMountError::Busy)?;
        let reader = Filesystem::mount(SharedArc::new(Image {
            disk,
            _lease: MountLease::Read { _lease: lease },
        }))
        .await?;
        let mut devices = BTreeMap::new();
        reader
            .subvolumes(|tree, root_inode| {
                devices.insert(tree, (allocate_device(), root_inode));
            })
            .await?;
        Ok(Self {
            inner: Arc::new(Inner {
                reader: Arc::new(spin::Mutex::new(reader)),
                writer: None,
                policy: None,
                devices,
                serial,
                writes: Arc::new(spin::Mutex::new(BTreeMap::new())),
                write_round: CACHE_BLOCK_BYTES,
            }),
        })
    }

    /// Construct a writable mount and its single-owner service. The caller
    /// runs the service on the kernel executor for the mount's lifetime.
    pub async fn mount_writable(
        disk: CachedDisk<Device>,
        mut allocate_device: impl FnMut() -> DeviceId + Send,
        options: BtrfsWriteOptions,
    ) -> Result<(Self, BtrfsService<Device>), BtrfsMountError> {
        let BtrfsWriteOptions {
            entropy,
            concurrency,
            compress,
            commit,
            max_inline,
            now,
        } = options;
        let compressor = Compressor::new(compress)?;
        let serial = disk.serial();
        let lease = disk.write_mount().ok_or(BtrfsMountError::Busy)?;
        let writer = Writer::mount(
            Image {
                disk,
                _lease: MountLease::Write { _lease: lease },
            },
            writer::EntropyUuids(entropy),
            concurrency,
            writer::timestamp(now),
        )
        .await?;
        let reader = writer.snapshot()?;
        let mut devices = BTreeMap::new();
        reader
            .subvolumes(|tree, root_inode| {
                devices.insert(tree, (allocate_device(), root_inode));
            })
            .await?;
        let reader = Arc::new(spin::Mutex::new(reader));
        let (client, receiver, exit) = writer::channel(concurrency);
        let policy = Arc::new(LivePolicy::new(
            commit.unwrap_or_default(),
            max_inline.unwrap_or(DEFAULT_MAX_INLINE),
        ));
        let service = BtrfsService {
            writer,
            receiver,
            published: reader.clone(),
            concurrency,
            compressor,
            policy: policy.clone(),
            _exit: exit,
        };
        Ok((
            Self {
                inner: Arc::new(Inner {
                    reader,
                    writer: Some(SharedArc::new(client)),
                    policy: Some(policy),
                    devices,
                    serial,
                    writes: Arc::new(spin::Mutex::new(BTreeMap::new())),
                    write_round: CACHE_BLOCK_BYTES.saturating_mul(concurrency.get()),
                }),
            },
            service,
        ))
    }

    /// The commit policy of a writable mount; `None` for a read-only one.
    pub fn commit_policy(&self) -> Option<CommitPolicy> {
        self.inner.policy.as_ref().map(|policy| policy.load())
    }

    /// Changes a writable mount's commit policy, as Linux's
    /// `mount -o remount,commit=`: the running transaction is judged by the
    /// new values at once. A read-only mount has none to change.
    pub fn set_commit_policy(&self, policy: CommitPolicy) -> FsResult<()> {
        self.inner
            .policy
            .as_ref()
            .ok_or(FsError::ReadOnly)?
            .store(policy);
        Ok(())
    }

    /// The `max_inline` of a writable mount; `None` for a read-only one.
    pub fn max_inline(&self) -> Option<u64> {
        self.inner.policy.as_ref().map(|policy| policy.max_inline())
    }

    /// Changes a writable mount's `max_inline`, as Linux's
    /// `mount -o remount,max_inline=`: the next mutation stores inline data
    /// under the new limit, and what is already stored stays as it is. A
    /// read-only mount has none to change.
    pub fn set_max_inline(&self, bytes: u64) -> FsResult<()> {
        self.inner
            .policy
            .as_ref()
            .ok_or(FsError::ReadOnly)?
            .max_inline
            .store(bytes, Ordering::Release);
        Ok(())
    }

    /// Ends a writable mount, as Linux's unmount: it resolves once the
    /// mount's service has committed what it held, or failed to, and let the
    /// device go; every request after it fails. A read-only mount has
    /// nothing to end.
    pub async fn unmount(&self) {
        if let Some(writer) = &self.inner.writer {
            writer.unmount().await;
        }
    }

    /// The handle the machine keeps on a writable mount, to commit it
    /// before power-off.
    pub(crate) fn writable(&self) -> Option<WritableMount> {
        self.inner.writer.as_ref().map(|writer| WritableMount {
            writer: SharedArc::downgrade(writer),
            exit: writer.exit().clone(),
        })
    }

    fn snapshot(&self) -> Snapshot<Device> {
        self.inner.reader.lock().clone()
    }

    async fn inode(&self, reader: &Snapshot<Device>, path: FsPath<'_>) -> FsResult<Inode> {
        let mut inode = reader.root().await.map_err(|e| self.fs_error(e))?;
        if !path.is_root() {
            // VFS already normalized the path and owns symlink traversal.
            for name in path.as_str().split('/') {
                inode = reader
                    .lookup(&inode, name.as_bytes())
                    .await
                    .map_err(|e| self.fs_error(e))?
                    .ok_or(FsError::NoEntry)?;
            }
        }
        Ok(inode)
    }

    async fn mutate(&self, path: FsPath<'_>, now: u64, operation: Operation) -> FsResult<()> {
        self.inner
            .writer
            .as_ref()
            .ok_or(FsError::ReadOnly)?
            .execute(path.as_str(), now, operation)
            .await
            .map(drop)
    }

    fn metadata(&self, inode: &Inode) -> FsResult<Metadata> {
        self.inode_metadata(inode.tree(), inode.number(), *inode.metadata())
    }

    fn inode_metadata(
        &self,
        tree: u64,
        number: u64,
        metadata: helios_btrfs::Metadata,
    ) -> FsResult<Metadata> {
        let device = self.inner.devices.get(&tree).ok_or(FsError::Io)?;
        let kind = node_kind(metadata.kind)?;
        Ok(Metadata {
            id: NodeId::new(device.0, number),
            kind,
            link_count: u64::from(metadata.nlink),
            size: if kind == NodeKind::Directory {
                0
            } else {
                metadata.size
            },
            access_nanos: nanos(metadata.atime)?,
            modified_nanos: nanos(metadata.mtime)?,
            status_nanos: nanos(metadata.ctime)?,
        })
    }

    fn fs_error(&self, error: Error) -> FsError {
        match error {
            Error::NotFound { .. } => FsError::NoEntry,
            Error::WrongKind {
                expected: FileKind::Directory,
                ..
            } => FsError::NotDirectory,
            error => {
                tracing::warn!(%error, serial = ?self.inner.serial, "btrfs image is malformed or unreadable");
                FsError::Io
            }
        }
    }
}

fn node_kind(kind: FileKind) -> FsResult<NodeKind> {
    match kind {
        FileKind::Regular => Ok(NodeKind::File),
        FileKind::Directory => Ok(NodeKind::Directory),
        FileKind::Symlink => Ok(NodeKind::Symlink),
        FileKind::CharDevice => Ok(NodeKind::CharacterDevice),
        FileKind::BlockDevice => Ok(NodeKind::BlockDevice),
        FileKind::Fifo => Ok(NodeKind::Fifo),
        FileKind::Socket => Ok(NodeKind::Socket),
    }
}

fn nanos(time: Timestamp) -> FsResult<u64> {
    u64::try_from(time.seconds)
        .ok()
        .and_then(|s| s.checked_mul(1_000_000_000))
        .and_then(|s| s.checked_add(u64::from(time.nanoseconds)))
        .ok_or(FsError::Overflow)
}

impl<Device: BlockDevice<Buffer = BlockPages> + 'static> FileSystem for BtrfsFs<Device> {
    type Write = BtrfsWrite<Device>;

    async fn stat(&self, path: FsPath<'_>) -> FsResult<Metadata> {
        self.metadata(&self.inode(&self.snapshot(), path).await?)
    }

    async fn open(&self, path: FsPath<'_>, flags: OpenFlags, now: u64) -> FsResult<Metadata> {
        if let Some(writer) = &self.inner.writer {
            return match writer
                .execute(path.as_str(), now, Operation::Open { flags })
                .await?
            {
                Response::Opened { inode, metadata } => {
                    self.inode_metadata(inode.tree(), inode.number(), metadata)
                }
                Response::Done | Response::Written(_) => {
                    unreachable!("open request returns inode metadata")
                }
            };
        }
        let metadata = match self.stat(path).await {
            Ok(metadata) => metadata,
            Err(FsError::NoEntry) if flags.contains(OpenFlags::CREATE) => {
                return Err(FsError::ReadOnly);
            }
            Err(error) => return Err(error),
        };
        if flags.contains(OpenFlags::CREATE | OpenFlags::EXCLUSIVE) {
            return Err(FsError::Exists);
        }
        if let Some(refusal) = metadata.kind.open_refusal() {
            return Err(refusal);
        }
        if flags.contains(OpenFlags::DIRECTORY) && metadata.kind != NodeKind::Directory {
            return Err(FsError::NotDirectory);
        }
        if flags.contains(OpenFlags::TRUNCATE) {
            return Err(FsError::ReadOnly);
        }
        Ok(metadata)
    }

    async fn read_at(&self, path: FsPath<'_>, offset: u64, max_bytes: usize) -> FsResult<Bytes> {
        let reader = self.snapshot();
        let inode = self.inode(&reader, path).await?;
        node_kind(inode.kind())?.require_regular()?;
        let len = inode
            .metadata()
            .size
            .saturating_sub(offset)
            .min(max_bytes as u64) as usize;
        let mut bytes = BytesMut::zeroed(len);
        let count = reader
            .read(&inode, offset, &mut bytes)
            .await
            .map_err(|e| self.fs_error(e))?;
        if count != len {
            return Err(FsError::Io);
        }
        Ok(bytes.freeze())
    }

    async fn read_dir(
        &self,
        path: FsPath<'_>,
        cookie: DirCookie,
        max_entries: usize,
    ) -> FsResult<Vec<DirEntry>> {
        let reader = self.snapshot();
        let inode = self.inode(&reader, path).await?;
        let mut entries = Vec::new();
        let mut failure = None;
        reader
            .read_dir(&inode, DirPosition(cookie.raw()), |entry| {
                if entries.len() == max_entries {
                    return ControlFlow::Break(());
                }
                let convert = || -> FsResult<DirEntry> {
                    let number = match entry.target {
                        EntryTarget::Inode(number) => number,
                        EntryTarget::Subvolume(tree) => {
                            self.inner.devices.get(&tree).ok_or(FsError::Io)?.1
                        }
                    };
                    Ok(DirEntry {
                        name: String::from_utf8(entry.name.to_vec())
                            .map_err(|_| FsError::IllegalByteSequence)?,
                        kind: node_kind(entry.kind)?,
                        inode: number,
                        next: DirCookie::new(
                            entry.position.0.checked_add(1).ok_or(FsError::Overflow)?,
                        ),
                    })
                };
                match convert() {
                    Ok(entry) => {
                        entries.push(entry);
                        ControlFlow::Continue(())
                    }
                    Err(error) => {
                        failure = Some(error);
                        ControlFlow::Break(())
                    }
                }
            })
            .await
            .map_err(|e| self.fs_error(e))?;
        match failure {
            Some(error) => Err(error),
            None => Ok(entries),
        }
    }

    async fn read_link(&self, path: FsPath<'_>) -> FsResult<String> {
        let reader = self.snapshot();
        let inode = self.inode(&reader, path).await?;
        if inode.kind() != FileKind::Symlink {
            return Err(FsError::Invalid);
        }
        // BTRFS_PATH_NAME_MAX bounds a symlink's contents before allocating.
        if inode.metadata().size > 4096 {
            return Err(FsError::Io);
        }
        let mut target = alloc::vec![0; inode.metadata().size as usize];
        let count = reader
            .read_link(&inode, &mut target)
            .await
            .map_err(|e| self.fs_error(e))?;
        target.truncate(count);
        String::from_utf8(target).map_err(|_| FsError::IllegalByteSequence)
    }

    async fn prepare_write(
        &self,
        path: FsPath<'_>,
        position: WritePosition,
        len: usize,
        now: u64,
    ) -> FsResult<Self::Write> {
        prepared::prepare(self, path, position, len, now).await
    }
    async fn set_size(&self, path: FsPath<'_>, size: u64, now: u64) -> FsResult<()> {
        self.mutate(path, now, Operation::Truncate { size }).await
    }
    async fn set_times(
        &self,
        path: FsPath<'_>,
        access: Option<u64>,
        modified: Option<u64>,
        now: u64,
    ) -> FsResult<()> {
        self.mutate(path, now, Operation::Times { access, modified })
            .await
    }
    async fn create_directory(&self, path: FsPath<'_>, now: u64) -> FsResult<()> {
        self.mutate(path, now, Operation::CreateDirectory).await
    }
    async fn remove_directory(&self, path: FsPath<'_>, now: u64) -> FsResult<()> {
        self.mutate(path, now, Operation::Remove { directory: true })
            .await
    }
    async fn unlink(&self, path: FsPath<'_>, now: u64) -> FsResult<()> {
        self.mutate(path, now, Operation::Remove { directory: false })
            .await
    }
    async fn rename(
        &self,
        source: FsPath<'_>,
        destination: FsPath<'_>,
        mode: RenameMode,
        now: u64,
    ) -> FsResult<()> {
        self.mutate(
            source,
            now,
            Operation::Rename {
                destination: String::from(destination.as_str()),
                mode,
            },
        )
        .await
    }
    async fn link(&self, source: FsPath<'_>, destination: FsPath<'_>, now: u64) -> FsResult<()> {
        self.mutate(
            source,
            now,
            Operation::Link {
                destination: String::from(destination.as_str()),
            },
        )
        .await
    }
    async fn symlink(&self, target: &str, path: FsPath<'_>, now: u64) -> FsResult<()> {
        self.mutate(
            path,
            now,
            Operation::Symlink {
                target: String::from(target),
            },
        )
        .await
    }
    async fn sync(&self, path: FsPath<'_>) -> FsResult<()> {
        if self.inner.writer.is_some() {
            self.mutate(path, 0, Operation::Sync).await
        } else {
            self.inode(&self.snapshot(), path).await.map(drop)
        }
    }
    async fn sync_data(&self, path: FsPath<'_>) -> FsResult<()> {
        self.sync(path).await
    }
}

#[cfg(test)]
mod tests;
