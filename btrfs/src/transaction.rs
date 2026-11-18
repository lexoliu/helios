//! A single-owner transaction. Its owner serializes writers and retains the
//! mounted generation's reader pins; this module never reuses retired extents.
//!
//! The transaction runs across many mutations: each one applies to it and
//! is visible through [`Transaction::view`] at once, and a commit writes them
//! all durably as one generation.

pub(crate) mod chunks;
mod extents;
pub(crate) mod file;
pub(crate) mod namespace;
pub(crate) mod ownership;
pub(crate) mod reflink;
pub(crate) mod replay;
pub(crate) mod reserve;

use core::num::NonZeroUsize;
use futures::future;

use self::namespace::Name;
use self::reflink::FileId;
use crate::accounting::{Accounting, Allocation};
use crate::commit::CommitImage;
use crate::data::{File, Reader};
use crate::fs::Snapshot;
use crate::ondisk::objectid;
use crate::retirement::Retirements;
use crate::roots::Roots;
use crate::source::{BlockSink, PendingWrites, READ_ALIGNMENT};
use crate::space::Space;
use crate::tree::TreeRoot;
use crate::tree::write::{Trees, WriteError};
use crate::volume::{Mapping, Volume};
use crate::{EntryTarget, Metadata};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// Produced only once the new roots are durable, or for an unchanged view.
pub(crate) struct Committed {
    pub(crate) snapshot: Arc<Snapshot>,
    /// The allocation state of `snapshot`'s generation; `None` for an unchanged
    /// view, whose state is the one the transaction started from.
    pub(crate) allocation: Option<Allocation>,
}

pub(crate) struct Transaction<S> {
    generation: Arc<Snapshot>,
    // Inode generations distinguish reuse across commits. Within one commit,
    // keep allocated numbers monotonic even after an inode is deleted again.
    inode_high_water: BTreeMap<u64, u64>,
    pub(crate) volume: Volume<S>,
    image: CommitImage,
    pub(crate) roots: Roots,
    /// The chunk tree's root. The superblock, not the root tree, records it.
    chunk_root: TreeRoot,
    /// Device ranges of chunks this transaction removed. They stay out of
    /// reach of chunk allocation until the transaction publishes
    /// (`btrfs_dev_extent_hole` skips `pending_chunks`). Only `commit` removes
    /// chunks, so no operation checkpoint covers it.
    released: Vec<core::ops::Range<u64>>,
    pub(crate) trees: Trees,
    pub(crate) space: Space,
    pub(crate) accounting: Accounting,
}

/// Private operation state. Inode allocation high-water marks deliberately
/// remain outside rollback: a discarded creation cannot lend its identity to
/// another inode in the same transaction generation.
pub(crate) struct Checkpoint {
    image: CommitImage,
    roots: Roots,
    chunk_root: TreeRoot,
    mapping: Arc<Mapping>,
    trees: Trees,
    space: Space,
    accounting: Accounting,
}

impl<S: BlockSink> Transaction<S> {
    /// The committed generation this transaction started from.
    pub(crate) fn base(&self) -> &Arc<Snapshot> {
        &self.generation
    }

    /// Bytes of private tree blocks this transaction holds in memory, every
    /// one of which its commit writes.
    pub(crate) fn dirty_metadata_bytes(&self) -> u64 {
        self.trees.block_count() as u64 * u64::from(self.volume.superblock().nodesize())
    }

    /// A reader's view of this transaction as it stands: its private tree
    /// blocks, its moved roots and the private data they reach. The writer
    /// keeps that data from reuse while the view lives.
    pub(crate) fn view(&self) -> Result<Arc<Snapshot>, WriteError> {
        Snapshot::running(
            &self.generation,
            self.volume.mapping(),
            &self.roots,
            self.accounting.checksum_root(),
            &self.trees,
            self.space.private_data(),
        )
    }

    pub(crate) fn checkpoint(&mut self) -> Checkpoint {
        let saved = Checkpoint {
            image: self.image.clone(),
            roots: self.roots.clone(),
            chunk_root: self.chunk_root,
            mapping: self.volume.mapping(),
            trees: self.trees.clone(),
            space: self.space.clone(),
            accounting: self.accounting.clone(),
        };
        self.space.begin_operation();
        saved
    }

    pub(crate) fn restore(&mut self, saved: Checkpoint) {
        self.image = saved.image;
        self.roots = saved.roots;
        self.chunk_root = saved.chunk_root;
        self.volume.install(saved.mapping);
        self.trees = saved.trees;
        self.space = saved.space;
        self.accounting = saved.accounting;
    }

    pub(crate) async fn inode_metadata(&self, file: FileId) -> Result<Metadata, WriteError> {
        self.inode_item(file.tree, file.inode)
            .await
            .map(|(_, metadata)| metadata)
    }

    pub(crate) async fn lookup_file(&self, name: Name<'_>) -> Result<Option<FileId>, WriteError> {
        Ok(match self.lookup_target(name).await? {
            Some((EntryTarget::Inode(inode), _)) => Some(FileId {
                tree: name.tree,
                inode,
            }),
            Some((EntryTarget::Subvolume(tree), _)) => Some(FileId {
                tree,
                inode: self.roots.root_directory(tree)?,
            }),
            None => None,
        })
    }

    /// Read the transaction's current inode and extents, including staged data
    /// and unpublished checksum items from earlier writes in this transaction.
    pub(crate) async fn read_file(
        &self,
        tree: u64,
        inode: u64,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, WriteError> {
        let root = self.roots.get(tree)?;
        let (_, metadata) = self.file_inode(tree, inode).await?;
        let nodes = self.trees.reader(&self.volume);
        Ok(
            Reader::new(&self.volume, &nodes, self.accounting.checksum_root())
                .read(
                    File {
                        root,
                        number: inode,
                        metadata: &metadata,
                    },
                    offset,
                    output,
                )
                .await?,
        )
    }

    /// Starts from the writer's committed allocation state for `pin`'s
    /// generation. Only the roots are read from disk; the per-transaction
    /// exclusions (mirror stripes, reader pins) are applied to a clone.
    pub(crate) async fn new(
        volume: Volume<S>,
        pin: Arc<Snapshot>,
        retirements: &mut Retirements,
        committed: &Allocation,
    ) -> Result<Self, WriteError> {
        let root = pin.root_tree;
        let (image, roots) =
            future::try_join(volume.commit_image(), Roots::load(&volume, root)).await?;
        let generation = volume
            .superblock()
            .generation()
            .checked_add(1)
            .ok_or(WriteError::GenerationOverflow)?;
        let mut space = committed.space.clone();
        space.rebind(&roots)?;
        space.exclude_superblock_stripes(volume.chunks());
        retirements.protect(&pin, &mut space);
        let mut free = committed.free.clone();
        if let Some(free) = &mut free {
            free.rebind(roots.get(objectid::FREE_SPACE_TREE)?);
        }
        let accounting = Accounting::with_free(
            &volume,
            roots.get(objectid::EXTENT_TREE)?,
            &space,
            roots.get(objectid::CSUM_TREE)?,
            free,
        );
        let trees = Trees::new(generation, volume.superblock().csum_type());
        let chunk = volume.superblock().chunk_tree();
        Ok(Self {
            generation: pin,
            inode_high_water: BTreeMap::new(),
            volume,
            image,
            roots,
            released: Vec::new(),
            chunk_root: TreeRoot {
                tree: objectid::CHUNK_TREE,
                bytenr: chunk.bytenr,
                level: chunk.level,
                generation: chunk.generation,
            },
            space,
            accounting,
            trees,
        })
    }

    /// Bytes used once this transaction commits, as the superblock's
    /// `bytes_used` will record them.
    pub(crate) fn bytes_used(&self) -> Result<u64, WriteError> {
        self.accounting.bytes_used(&self.space)
    }

    /// Consume the overlay. Any failure/cancellation prevents in-memory root
    /// publication; BlockSink's commit contract poisons failed durable commits.
    /// Blocks this transaction retired, and private data it released while a
    /// view still reached it, are recorded in `retirements` against the
    /// generation it started from.
    pub(crate) async fn commit(
        mut self,
        retirements: &mut Retirements,
        mut uuid: impl FnMut() -> [u8; 16],
        concurrency: NonZeroUsize,
    ) -> Result<Committed, WriteError> {
        // Block groups nothing uses leave first, as the cleaner does after a
        // commit; what they free is out of reach of this commit's chunks.
        self.remove_unused_groups().await?;
        // A metadata chunk the commit may need exists before it starts.
        if self.trees.blocks().next().is_some() {
            self.grow_metadata().await?;
        }
        loop {
            self.accounting
                .sync_allocations(&self.volume, &mut self.trees, &mut self.space)
                .await?;
            self.roots.set(self.accounting.root)?;
            self.roots.set(self.accounting.group_root())?;
            self.roots.set(self.accounting.checksum_root())?;
            if let Some(root) = self.accounting.free_root() {
                self.roots.set(root)?;
            }
            if !self
                .roots
                .sync(&self.volume, &mut self.trees, &mut self.space, &mut uuid)
                .await?
            {
                break;
            }
        }
        if self.trees.blocks().next().is_none() {
            return Ok(Committed {
                snapshot: self.generation.clone(),
                allocation: None,
            });
        }
        let allocation = self.accounting.committed(&self.space)?;
        self.image.prepare(
            &self.roots,
            self.chunk_root,
            self.accounting.bytes_used(&self.space)?,
        )?;
        let superblock = self
            .image
            .superblock(self.volume.superblock().csum_type())?;
        let snapshot =
            self.generation
                .advance(superblock, self.volume.shared_chunks(), &self.roots)?;
        // Register before the first await that can publish the new generation.
        // Cancellation or a failed mirror write must not lose these pins.
        let mut retired = self.accounting.take_retired();
        for (address, length) in &self.space.take_held() {
            retired.insert_mut(*address, *length);
        }
        retirements.record(&self.generation, retired);
        // Bound the total in-flight I/O by staging one block per concurrent
        // stream item. Volume::stage handles this block's physical copies.
        let mut writes = PendingWrites::new(concurrency);
        for (logical, bytes) in self.trees.blocks() {
            for (index, bytes) in bytes.chunks(READ_ALIGNMENT as usize).enumerate() {
                writes
                    .push(self.volume.stage(
                        logical + index as u64 * READ_ALIGNMENT,
                        bytes,
                        NonZeroUsize::MIN,
                    ))
                    .await?;
            }
        }
        writes.finish().await?;
        self.volume.publish(self.image).await?;
        Ok(Committed {
            snapshot,
            allocation: Some(allocation),
        })
    }
}
