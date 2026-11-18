//! One owner applies every mutation to a running transaction; readers own
//! independent immutable views of it. No mutex is held across I/O.
//!
//! # Concurrency contract
//!
//! - The [`Writer`] is single-owner. It holds the running transaction, the
//!   last durable generation, the carried allocation state and the reader
//!   pins; every mutation and every commit takes `&mut Writer`, so they are
//!   serialized by ownership, never by a lock.
//! - A mutation applies to the running transaction and is visible the moment
//!   it is accepted: acceptance takes a view of the transaction, which every
//!   later [`Writer::snapshot`] hands out. A view is a handful of persistent
//!   collection roots, so taking one costs reference-count increments, and a
//!   reader holding one reads without the writer and on any processor.
//! - A view keeps what it reaches readable for as long as it lives: the
//!   transaction's private tree blocks are in the view itself, committed
//!   blocks the transaction retires stay pinned through the generation the
//!   transaction started from, and private data the transaction releases is
//!   held from reuse until no live view reaches it.
//! - [`Writer::commit`] makes the running transaction durable as one
//!   generation. Its caller decides when; Linux commits on `sync`, `fsync`
//!   and `fdatasync`, when its `commit=` interval expires, when the dirty
//!   state reaches its bound ([`DEFAULT_DIRTY_METADATA_LIMIT`] unless the
//!   operator chose another), and at unmount. The changes a
//!   commit carries were already visible, so a commit that fails, or whose
//!   future is dropped, cannot withdraw them: the writer is poisoned and the
//!   device keeps the previous complete generation.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::num::NonZeroUsize;
use core::ops::{Deref, DerefMut};
use helios_hal::fs::RenameMode;

use super::{Filesystem, Snapshot, find_root};
use crate::accounting::Allocation;
use crate::compress::policy::{Compressor, inode_compresses};
use crate::compress::write::{Encoder, Plain};
use crate::ondisk::objectid;
use crate::retirement::Retirements;
use crate::source::BlockSink;
use crate::transaction::file::WriteOptions;
use crate::transaction::namespace::{Attributes as RawAttributes, Name as RawName};
use crate::transaction::reflink::{CloneRange as RawCloneRange, FileId};
use crate::transaction::reserve::Units;
use crate::transaction::{Checkpoint, Transaction};
use crate::tree::write::WriteError;
use crate::{DeviceNumber, Error, FileKind, Metadata, Timestamp};

/// The dirty metadata a running transaction holds before it is committed,
/// unless the mount's operator chose another bound: Linux's
/// `BTRFS_DIRTY_METADATA_THRESH`, `SZ_32M` in `fs/btrfs/fs.h`, past which
/// `btrfs_btree_balance_dirty` writes the btree's dirty blocks back. The
/// running transaction keeps its private blocks in memory until commit, so
/// a commit is what writes them back here.
pub const DEFAULT_DIRTY_METADATA_LIMIT: u64 = 32 << 20;

/// What the running transaction holds that its commit writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dirty {
    /// Bytes of private tree blocks held in memory.
    pub metadata_bytes: u64,
}

impl Dirty {
    /// Nothing waits for a commit.
    pub const fn is_clean(self) -> bool {
        self.metadata_bytes == 0
    }
}

#[derive(Debug)]
struct MountIdentity;

/// A mount-bound inode identity. Its generation detects reuse after deletion.
#[derive(Clone, Debug)]
pub struct InodeHandle {
    mount: Arc<MountIdentity>,
    id: FileId,
    generation: u64,
}

impl InodeHandle {
    pub fn tree(&self) -> u64 {
        self.id.tree
    }
    pub fn number(&self) -> u64 {
        self.id.inode
    }
}

/// Permissions and ownership for a newly created entry. File type is selected
/// by create_file, create_directory, symlink or create_special, never by
/// permission bits. Every bit of `07777` is stored as given, the set-id and
/// sticky bits included, on every kind of inode.
#[derive(Clone, Copy, Debug)]
pub struct Attributes {
    pub permissions: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Attributes {
    fn raw(self, kind: u32) -> Result<RawAttributes, WriteError> {
        self.raw_with_device(kind, 0)
    }

    fn raw_with_device(self, kind: u32, rdev: u64) -> Result<RawAttributes, WriteError> {
        if self.permissions & !0o7777 != 0 {
            return Err(WriteError::InvalidPermissions {
                permissions: self.permissions,
            });
        }
        Ok(RawAttributes {
            mode: kind | self.permissions,
            uid: self.uid,
            gid: self.gid,
            rdev,
        })
    }
}

/// A node `mknod` makes that holds no data: a device node, a FIFO or a
/// socket, as `btrfs_mknod` in `fs/btrfs/inode.c` creates them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Special {
    CharDevice(DeviceNumber),
    BlockDevice(DeviceNumber),
    Fifo,
    Socket,
}

impl Special {
    /// The file type bits of the mode, and the inode item's `rdev`.
    fn mode_and_rdev(self) -> (u32, u64) {
        match self {
            Self::CharDevice(device) => (0o020_000, device.inode_item()),
            Self::BlockDevice(device) => (0o060_000, device.inode_item()),
            Self::Fifo => (0o010_000, 0),
            Self::Socket => (0o140_000, 0),
        }
    }
}

#[derive(Clone, Copy)]
pub struct Name<'a> {
    pub parent: &'a InodeHandle,
    pub bytes: &'a [u8],
}

#[derive(Clone, Copy)]
pub struct CloneRange<'a> {
    pub source: &'a InodeHandle,
    pub destination: &'a InodeHandle,
    pub source_offset: u64,
    pub destination_offset: u64,
    pub length: u64,
}

pub struct Writer<S, U> {
    /// The last durable generation; `None` once a commit failed or was
    /// dropped, which poisons the writer.
    committed: Option<Filesystem<Arc<S>>>,
    /// Allocation state of `committed`'s generation, loaded and validated
    /// once at mount and replaced only by a commit that published.
    allocation: Allocation,
    retirements: Retirements,
    running: Option<Running<S>>,
    mount: Arc<MountIdentity>,
    /// What every commit needs, the ones a mutation forces included.
    uuids: U,
    concurrency: NonZeroUsize,
}

/// Where a commit gets the UUID of a root record that has none: Linux's
/// `generate_random_uuid`, which a caller with entropy supplies.
pub trait RootUuids: Send {
    fn next_uuid(&mut self) -> [u8; 16];
}

impl<F: FnMut() -> [u8; 16] + Send> RootUuids for F {
    fn next_uuid(&mut self) -> [u8; 16] {
        self()
    }
}

/// The space a mounted filesystem has and uses, as the running transaction
/// leaves it: what `statfs` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub total_bytes: u64,
    pub used_bytes: u64,
}

/// The transaction every mutation joins until the next commit.
struct Running<S> {
    inner: Transaction<Arc<S>>,
    /// The view as of the last accepted mutation; `None` until one is, when
    /// the committed generation is still what readers see.
    view: Option<Arc<Snapshot>>,
    /// Every view taken of this transaction that a reader may still hold.
    views: Vec<Weak<Snapshot>>,
}

impl<S: BlockSink, U: RootUuids> Writer<S, U> {
    /// Mounts `source` for writing. Every commit takes root UUIDs from
    /// `uuids` and keeps at most `concurrency` block writes in flight.
    pub async fn mount(source: S, uuids: U, concurrency: NonZeroUsize) -> Result<Self, WriteError> {
        let current = Filesystem::mount(Arc::new(source)).await?;
        current.superblock().check_writable()?;
        // Linux enables quotas by creating the quota tree. Its qgroup counts
        // go stale under any write this writer makes, so refuse until the
        // writer maintains them (lexoliu/helios#1043).
        match find_root(
            &current.volume,
            current.snapshot.root_tree,
            objectid::QUOTA_TREE,
        )
        .await
        {
            Ok(_) => return Err(WriteError::QuotaTreePresent),
            Err(Error::MissingRoot { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        let allocation = Allocation::load(&current.volume, current.snapshot.root_tree).await?;
        Ok(Self {
            committed: Some(current),
            allocation,
            retirements: Retirements::default(),
            running: None,
            mount: Arc::new(MountIdentity),
            uuids,
            concurrency,
        })
    }

    /// The space the filesystem has, and what it uses once the running
    /// transaction commits: a write is visible at once, and so is its space.
    pub fn usage(&self) -> Result<Usage, WriteError> {
        let committed = self.committed.as_ref().ok_or(WriteError::WriterPoisoned)?;
        let superblock = committed.superblock();
        Ok(Usage {
            total_bytes: superblock.total_bytes(),
            used_bytes: match &self.running {
                Some(running) => running.inner.bytes_used()?,
                None => superblock.bytes_used(),
            },
        })
    }

    #[cfg(test)]
    pub(crate) fn committed_allocation(&self) -> &Allocation {
        &self.allocation
    }

    /// What a reader sees now: the running transaction as of its last
    /// accepted mutation, or the last durable generation when nothing has
    /// changed since.
    pub fn snapshot(&self) -> Result<Filesystem<Arc<S>>, WriteError> {
        let committed = self.committed.as_ref().ok_or(WriteError::WriterPoisoned)?;
        Ok(
            match self
                .running
                .as_ref()
                .and_then(|running| Some((running, running.view.clone()?)))
            {
                Some((running, snapshot)) => Filesystem {
                    snapshot,
                    volume: running.inner.volume.clone(),
                },
                None => committed.clone(),
            },
        )
    }

    /// What the next commit writes.
    pub fn dirty(&self) -> Dirty {
        Dirty {
            metadata_bytes: self
                .running
                .as_ref()
                .map_or(0, |running| running.inner.dirty_metadata_bytes()),
        }
    }

    /// Joins the running transaction, starting one from the last durable
    /// generation when none runs, as `btrfs_join_transaction` does. Dropping
    /// the handle leaves every mutation it accepted in the transaction.
    pub async fn join(&mut self) -> Result<WriteTransaction<'_, S, U>, WriteError> {
        self.start().await?;
        Ok(WriteTransaction { writer: self })
    }

    /// Starts a running transaction from the last durable generation unless
    /// one runs.
    async fn start(&mut self) -> Result<(), WriteError> {
        let committed = self.committed.as_ref().ok_or(WriteError::WriterPoisoned)?;
        if self.running.is_none() {
            let (volume, pin) = (committed.volume.clone(), committed.snapshot.clone());
            let inner =
                Transaction::new(volume, pin, &mut self.retirements, &self.allocation).await?;
            self.running = Some(Running {
                inner,
                view: None,
                views: Vec::new(),
            });
        }
        Ok(())
    }

    /// Makes every mutation the running transaction holds durable as one
    /// generation, which readers then see. With nothing running, or nothing
    /// changed, nothing is written.
    ///
    /// The changes were visible before this call, so a failure, or dropping
    /// the future before it finishes, poisons the writer instead of
    /// discarding them; the device keeps the previous generation.
    pub async fn commit(&mut self) -> Result<(), WriteError> {
        let mut current = self.committed.take().ok_or(WriteError::WriterPoisoned)?;
        let Some(running) = self
            .running
            .take()
            .filter(|running| running.inner.dirty_metadata_bytes() != 0)
        else {
            // A clean transaction holds nothing the last commit did not.
            self.committed = Some(current);
            return Ok(());
        };
        let uuids = &mut self.uuids;
        let mut committed = running
            .inner
            .commit(
                &mut self.retirements,
                || uuids.next_uuid(),
                self.concurrency,
            )
            .await?;
        if let Some(allocation) = committed.allocation.take() {
            self.allocation = allocation;
        }
        current.publish(committed);
        self.committed = Some(current);
        Ok(())
    }
}

/// A handle on the running transaction, as Linux's `btrfs_trans_handle`.
/// Each mutation borrows it exclusively. An error or cancellation restores
/// that operation's checkpoint, preserving earlier accepted mutations.
/// Dropping the handle ends it without a commit; [`Self::commit`] commits.
pub struct WriteTransaction<'a, S, U> {
    writer: &'a mut Writer<S, U>,
}

impl<S: BlockSink, U: RootUuids> WriteTransaction<'_, S, U> {
    fn running(&self) -> &Running<S> {
        self.writer
            .running
            .as_ref()
            .expect("a handle exists only while its transaction runs")
    }

    fn running_mut(&mut self) -> &mut Running<S> {
        self.writer
            .running
            .as_mut()
            .expect("a handle exists only while its transaction runs")
    }

    fn inner(&self) -> &Transaction<Arc<S>> {
        &self.running().inner
    }

    fn inner_mut(&mut self) -> &mut Transaction<Arc<S>> {
        &mut self.running_mut().inner
    }

    #[cfg(test)]
    pub(crate) fn edits(&mut self) -> &mut Transaction<Arc<S>> {
        self.inner_mut()
    }

    /// Publishes `view`, taken after an accepted mutation, and returns the
    /// private data released since to the allocator once no live view
    /// reaches it.
    fn install(&mut self, view: Arc<Snapshot>) {
        let running = self.running_mut();
        running.view = Some(view.clone());
        running.views.retain(|view| view.strong_count() != 0);
        let live: Vec<Arc<Snapshot>> = running.views.iter().filter_map(Weak::upgrade).collect();
        running
            .inner
            .space
            .accept_operation(|address| live.iter().any(|view| view.reaches_private_data(address)));
        running.views.push(Arc::downgrade(&view));
    }

    async fn load_handle(&self, id: FileId) -> Result<InodeHandle, WriteError> {
        let metadata = self.inner().inode_metadata(id).await?;
        Ok(InodeHandle {
            mount: self.writer.mount.clone(),
            id,
            generation: metadata.generation,
        })
    }

    #[cfg(test)]
    pub(crate) async fn fixture_handle(&self, id: FileId) -> Result<InodeHandle, WriteError> {
        self.load_handle(id).await
    }

    async fn checked(&self, handle: &InodeHandle) -> Result<Metadata, WriteError> {
        if !Arc::ptr_eq(&self.writer.mount, &handle.mount) {
            return Err(WriteError::ForeignHandle);
        }
        let stale = || WriteError::StaleHandle {
            tree: handle.id.tree,
            inode: handle.id.inode,
            generation: handle.generation,
        };
        let metadata = match self.inner().inode_metadata(handle.id).await {
            Err(WriteError::Read(Error::MissingInode { .. })) => return Err(stale()),
            result => result?,
        };
        if metadata.generation != handle.generation {
            return Err(stale());
        }
        Ok(metadata)
    }

    async fn entry_name<'b>(&self, name: Name<'b>) -> Result<RawName<'b>, WriteError> {
        let metadata = self.checked(name.parent).await?;
        if metadata.kind != FileKind::Directory {
            return Err(Error::WrongKind {
                tree: name.parent.tree(),
                inode: name.parent.number(),
                expected: FileKind::Directory,
                found: metadata.kind,
            }
            .into());
        }
        Ok(RawName {
            tree: name.parent.tree(),
            parent: name.parent.number(),
            bytes: name.bytes,
        })
    }

    pub async fn root(&self) -> Result<InodeHandle, WriteError> {
        let record = self.inner().base().default_subvolume;
        self.load_handle(FileId {
            tree: record.root.tree,
            inode: record.root_dirid,
        })
        .await
    }

    pub async fn metadata(&self, handle: &InodeHandle) -> Result<Metadata, WriteError> {
        self.checked(handle).await
    }

    pub async fn lookup(
        &self,
        parent: &InodeHandle,
        bytes: &[u8],
    ) -> Result<Option<InodeHandle>, WriteError> {
        let name = self.entry_name(Name { parent, bytes }).await?;
        match self.inner().lookup_file(name).await? {
            Some(id) => self.load_handle(id).await.map(Some),
            None => Ok(None),
        }
    }

    pub async fn read_file(
        &self,
        file: &InodeHandle,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, WriteError> {
        self.checked(file).await?;
        let file = file.id;
        self.inner()
            .read_file(file.tree, file.inode, offset, output)
            .await
    }

    /// Writes `data` at `offset` and returns the bytes stored, as `write(2)`:
    /// fewer than `data.len()` only when the device ran out of free sectors
    /// partway, never zero for a nonempty write, which fails instead.
    pub async fn write_file<E: Encoder>(
        &mut self,
        file: &InodeHandle,
        offset: u64,
        data: &[u8],
        options: WriteOptions<E>,
    ) -> Result<usize, WriteError> {
        // Counted as checksummed: a NODATASUM inode reserves more than it
        // uses, never less.
        let units = self.inner().write_units(data.len() as u64, true);
        let mut operation = Mutation::new(self, units).await?;
        operation.checked(file).await?;
        let file = file.id;
        operation.inner_mut().roots.check_writable(file.tree)?;
        let written = operation
            .inner_mut()
            .write_file(file.tree, file.inode, offset, data, options)
            .await?;
        operation.accept()?;
        Ok(written)
    }

    /// [`Self::write_file`] with the mount's compression policy. An inode that
    /// Linux does not compress (`NODATASUM`, `NODATACOW` or `NOCOMPRESS`) is
    /// written uncompressed, where `write_file` with an encoder refuses it.
    pub async fn write_file_with_policy(
        &mut self,
        file: &InodeHandle,
        offset: u64,
        data: &[u8],
        when: Timestamp,
        concurrency: NonZeroUsize,
        compressor: &mut Compressor,
    ) -> Result<usize, WriteError> {
        let compresses = inode_compresses(self.metadata(file).await?.flags);
        macro_rules! write {
            ($encoder:expr) => {
                self.write_file(
                    file,
                    offset,
                    data,
                    WriteOptions {
                        when,
                        concurrency,
                        encoder: $encoder,
                    },
                )
                .await
            };
        }
        match compressor {
            Compressor::Zlib(encoder) if compresses => write!(encoder),
            Compressor::Lzo(encoder) if compresses => write!(encoder),
            Compressor::Zstd(encoder) if compresses => write!(&mut **encoder),
            _ => write!(Plain),
        }
    }

    pub async fn truncate_file(
        &mut self,
        file: &InodeHandle,
        size: u64,
        when: Timestamp,
        concurrency: NonZeroUsize,
    ) -> Result<(), WriteError> {
        let mut operation = Mutation::new(self, Units::TRUNCATE).await?;
        operation.checked(file).await?;
        let file = file.id;
        operation.inner_mut().roots.check_writable(file.tree)?;
        operation
            .inner_mut()
            .truncate_file(file.tree, file.inode, size, when, concurrency)
            .await?;
        operation.accept()
    }

    pub async fn clone_range(
        &mut self,
        range: CloneRange<'_>,
        when: Timestamp,
        concurrency: NonZeroUsize,
    ) -> Result<(), WriteError> {
        let mut operation = Mutation::new(self, Units::CLONE).await?;
        operation.checked(range.source).await?;
        operation.checked(range.destination).await?;
        let range = RawCloneRange {
            source: range.source.id,
            destination: range.destination.id,
            source_offset: range.source_offset,
            destination_offset: range.destination_offset,
            length: range.length,
        };
        operation
            .inner_mut()
            .roots
            .check_writable(range.destination.tree)?;
        operation
            .inner_mut()
            .clone_range(range, when, concurrency)
            .await?;
        operation.accept()
    }

    pub async fn set_times(
        &mut self,
        file: &InodeHandle,
        access: Option<Timestamp>,
        modified: Option<Timestamp>,
        changed: Timestamp,
    ) -> Result<(), WriteError> {
        let mut operation = Mutation::new(self, Units::INODE_UPDATE).await?;
        operation.checked(file).await?;
        let file = file.id;
        operation.inner_mut().roots.check_writable(file.tree)?;
        operation
            .inner_mut()
            .set_times(file.tree, file.inode, access, modified, changed)
            .await?;
        operation.accept()
    }

    /// Replaces the permission bits (`07777`, the set-id and sticky bits
    /// included on every kind of inode) and stamps `changed`.
    pub async fn set_permissions(
        &mut self,
        file: &InodeHandle,
        permissions: u32,
        changed: Timestamp,
    ) -> Result<(), WriteError> {
        let mut operation = Mutation::new(self, Units::INODE_UPDATE).await?;
        operation.checked(file).await?;
        let file = file.id;
        operation.inner_mut().roots.check_writable(file.tree)?;
        operation
            .inner_mut()
            .set_permissions(file.tree, file.inode, permissions, changed)
            .await?;
        operation.accept()
    }

    /// Changes the owner and the group, each left as it is when `None`, and
    /// stamps `changed`. A non-directory loses its set-id bits.
    pub async fn set_owner(
        &mut self,
        file: &InodeHandle,
        uid: Option<u32>,
        gid: Option<u32>,
        changed: Timestamp,
    ) -> Result<(), WriteError> {
        let mut operation = Mutation::new(self, Units::INODE_UPDATE).await?;
        operation.checked(file).await?;
        let file = file.id;
        operation.inner_mut().roots.check_writable(file.tree)?;
        operation
            .inner_mut()
            .set_owner(file.tree, file.inode, uid, gid, changed)
            .await?;
        operation.accept()
    }

    pub async fn create_file(
        &mut self,
        name: Name<'_>,
        attributes: Attributes,
        when: Timestamp,
    ) -> Result<InodeHandle, WriteError> {
        self.create(name, attributes.raw(0o100000)?, when).await
    }

    pub async fn create_directory(
        &mut self,
        name: Name<'_>,
        attributes: Attributes,
        when: Timestamp,
    ) -> Result<InodeHandle, WriteError> {
        self.create(name, attributes.raw(0o040000)?, when).await
    }

    /// Creates a device node, a FIFO or a socket, as `mknod`. The node holds
    /// no data; a device node keeps the number it names.
    pub async fn create_special(
        &mut self,
        name: Name<'_>,
        special: Special,
        attributes: Attributes,
        when: Timestamp,
    ) -> Result<InodeHandle, WriteError> {
        let (kind, rdev) = special.mode_and_rdev();
        self.create(name, attributes.raw_with_device(kind, rdev)?, when)
            .await
    }

    async fn create(
        &mut self,
        name: Name<'_>,
        attributes: RawAttributes,
        when: Timestamp,
    ) -> Result<InodeHandle, WriteError> {
        let mut operation = Mutation::new(self, Units::CREATE).await?;
        let name = operation.entry_name(name).await?;
        operation.inner_mut().roots.check_writable(name.tree)?;
        let inode = operation
            .inner_mut()
            .create_node(name, attributes, when)
            .await?;
        let handle = operation
            .load_handle(FileId {
                tree: name.tree,
                inode,
            })
            .await?;
        operation.accept()?;
        Ok(handle)
    }

    pub async fn hard_link(
        &mut self,
        name: Name<'_>,
        file: &InodeHandle,
        when: Timestamp,
    ) -> Result<(), WriteError> {
        let mut operation = Mutation::new(self, Units::LINK).await?;
        operation.checked(file).await?;
        let name = operation.entry_name(name).await?;
        if file.tree() != name.tree {
            return Err(WriteError::CrossSubvolume);
        }
        let inode = file.number();
        operation.inner_mut().roots.check_writable(name.tree)?;
        operation.inner_mut().hard_link(name, inode, when).await?;
        operation.accept()
    }

    pub async fn symlink(
        &mut self,
        name: Name<'_>,
        target: &[u8],
        attributes: Attributes,
        when: Timestamp,
    ) -> Result<InodeHandle, WriteError> {
        let mut operation = Mutation::new(self, Units::SYMLINK).await?;
        let name = operation.entry_name(name).await?;
        operation.inner_mut().roots.check_writable(name.tree)?;
        let inode = operation
            .inner_mut()
            .symlink(name, target, attributes.raw(0o120000)?, when)
            .await?;
        let handle = operation
            .load_handle(FileId {
                tree: name.tree,
                inode,
            })
            .await?;
        operation.accept()?;
        Ok(handle)
    }

    pub async fn rename(
        &mut self,
        source: Name<'_>,
        destination: Name<'_>,
        mode: RenameMode,
        when: Timestamp,
    ) -> Result<(), WriteError> {
        let across = source.parent.id != destination.parent.id;
        let mut operation = Mutation::new(self, Units::rename(across)).await?;
        let source = operation.entry_name(source).await?;
        let destination = operation.entry_name(destination).await?;
        operation.inner_mut().roots.check_writable(source.tree)?;
        operation
            .inner_mut()
            .roots
            .check_writable(destination.tree)?;
        operation
            .inner_mut()
            .rename(source, destination, mode, when)
            .await?;
        operation.accept()
    }

    pub async fn unlink_name(&mut self, name: Name<'_>, when: Timestamp) -> Result<(), WriteError> {
        let mut operation = Mutation::new(self, Units::UNLINK).await?;
        let name = operation.entry_name(name).await?;
        operation.inner_mut().roots.check_writable(name.tree)?;
        operation.inner_mut().unlink_name(name, when).await?;
        operation.accept()
    }

    /// Commits the running transaction, as [`Writer::commit`].
    pub async fn commit(self) -> Result<(), WriteError> {
        self.writer.commit().await
    }

    /// Reserves the metadata a mutation of `units` may need (`reserve`).
    /// What does not fit is given what Linux's metadata flush would free
    /// first: the commit, which runs the work Linux keeps as delayed refs,
    /// then a new chunk. A mutation that still does not fit is refused
    /// before it changes anything.
    async fn reserve(&mut self, units: Units) -> Result<(), WriteError> {
        if self.inner().reserves(units) {
            return Ok(());
        }
        if !self.writer.dirty().is_clean() {
            self.writer.commit().await?;
            self.writer.start().await?;
            if self.inner().reserves(units) {
                return Ok(());
            }
        }
        let mut chunk = Mutation::checkpointed(self, Units::default());
        chunk.inner_mut().force_metadata_chunk().await?;
        chunk.accept()?;
        if self.inner().reserves(units) {
            return Ok(());
        }
        Err(self.inner().exhausted(units))
    }
}

/// The mutation future owns this guard. Its drop restores the complete private
/// state after the future's in-flight I/O has been dropped. No reader has seen
/// that state, and committed extents remain pinned throughout rollback.
struct Mutation<'op, 'tx, S: BlockSink, U: RootUuids> {
    transaction: &'op mut WriteTransaction<'tx, S, U>,
    saved: Option<Checkpoint>,
    /// What the mutation reserved: its acceptance checks the commit still
    /// has what it needs.
    units: Units,
}

impl<'op, 'tx, S: BlockSink, U: RootUuids> Mutation<'op, 'tx, S, U> {
    /// Reserves the operation's metadata, checkpoints the transaction, then
    /// makes the chunk room the operation may need: a failure after the
    /// checkpoint restores it like any other.
    async fn new(
        transaction: &'op mut WriteTransaction<'tx, S, U>,
        units: Units,
    ) -> Result<Self, WriteError> {
        transaction.reserve(units).await?;
        let operation = Self::checkpointed(transaction, units);
        operation.transaction.inner_mut().prepare_mutation().await?;
        Ok(operation)
    }

    fn checkpointed(transaction: &'op mut WriteTransaction<'tx, S, U>, units: Units) -> Self {
        let saved = Some(transaction.inner_mut().checkpoint());
        Self {
            transaction,
            saved,
            units,
        }
    }

    /// Accepts the operation and makes it visible to readers. One that left
    /// the commit less than its work needs is refused and rolled back.
    fn accept(mut self) -> Result<(), WriteError> {
        self.transaction.inner().check_commit_hold(self.units)?;
        let view = self.transaction.inner().view()?;
        self.saved = None;
        self.transaction.install(view);
        Ok(())
    }
}

impl<'tx, S: BlockSink, U: RootUuids> Deref for Mutation<'_, 'tx, S, U> {
    type Target = WriteTransaction<'tx, S, U>;
    fn deref(&self) -> &Self::Target {
        self.transaction
    }
}

impl<S: BlockSink, U: RootUuids> DerefMut for Mutation<'_, '_, S, U> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.transaction
    }
}

impl<S: BlockSink, U: RootUuids> Drop for Mutation<'_, '_, S, U> {
    fn drop(&mut self) {
        if let Some(saved) = self.saved.take() {
            self.transaction.inner_mut().restore(saved);
        }
    }
}
