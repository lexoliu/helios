//! Extent-tree edits owned by the enclosing filesystem transaction. These edits
//! change references, never allocator availability: committed blocks stay pinned
//! until publication and the last reader of their generation have completed.

mod cow;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use rpds::RedBlackTreeMapSync;

use crate::Key;
use crate::allocation::data::{DataBackref, DataDefect, DataExtent, DataReference};
use crate::allocation::{AllocationDefect, MetadataExtent, MetadataReference};
use crate::checksums::Checksums;
use crate::free_space::FreeTree;
use crate::ondisk::{item_type, objectid};
use crate::source::BlockSource;
use crate::space::{MetadataReservation, Space, SpaceDefect, SpaceError};
use crate::tree::TreeRoot;
use crate::tree::write::{MetadataAllocator, Trees, WriteError};
use crate::volume::Volume;

#[derive(Clone, Copy)]
pub(crate) struct MetadataBlock {
    pub(crate) address: u64,
    pub(crate) level: u8,
}

pub(crate) struct DataChange<R> {
    pub(crate) key: Key,
    pub(crate) reference: R,
    pub(crate) delta: i64,
}

#[cfg(feature = "write")]
pub(crate) struct DataWrite<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) reference: DataReference,
    pub(crate) checksummed: bool,
}

/// Allocation state of one committed generation, kept by the writer across
/// transactions so that a transaction never rebuilds it from disk: the
/// per-group free ranges and the free-space tree's records. Every collection
/// is persistent, so a transaction starts from an O(1) clone and the writer
/// replaces its copy only when a commit has published.
///
/// The tree roots held inside are stale by one generation; a transaction
/// rebinds them to the roots it starts from. Mount loads and validates this
/// once, from the allocation trees.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) struct Allocation {
    pub(crate) space: Space,
    pub(crate) free: Option<FreeTree>,
}

impl Allocation {
    pub(crate) async fn load<S: BlockSource>(
        volume: &Volume<S>,
        root: TreeRoot,
    ) -> Result<Self, WriteError> {
        let space = Space::load_committed(volume, root).await?;
        let free = Self::load_free(volume, &space).await?;
        Ok(Self { space, free })
    }

    async fn load_free<S: BlockSource>(
        volume: &Volume<S>,
        space: &Space,
    ) -> Result<Option<FreeTree>, WriteError> {
        let Some(root) = space.free_root() else {
            return Ok(None);
        };
        let expected = space.committed_free(&RedBlackTreeMapSync::new_sync())?;
        Ok(Some(FreeTree::load(volume, root, &expected).await?))
    }
}

#[derive(Clone)]
pub(crate) struct Accounting {
    pub(crate) root: TreeRoot,
    nodesize: u64,
    groups: TreeRoot,
    free_space: Option<FreeTree>,
    checksums: Checksums,
    group_used: RedBlackTreeMapSync<Key, u64>,
    retired_extents: RedBlackTreeMapSync<u64, u64>,
    registered: RedBlackTreeMapSync<u64, MetadataReservation>,
    processed_retired: usize,
}

impl Accounting {
    /// Stage newly allocated data with the inode’s checksum policy. Failure aborts the enclosing
    /// transaction; no published root can reach the reservation yet.
    #[cfg(feature = "write")]
    pub(crate) async fn stage_data<S: crate::source::BlockSink>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
        data: DataWrite<'_>,
        concurrency: core::num::NonZeroUsize,
    ) -> Result<Key, WriteError> {
        let DataWrite {
            bytes,
            reference,
            checksummed,
        } = data;
        let key = self
            .allocate_data(volume, trees, space, bytes.len() as u64, reference)
            .await?;
        if checksummed {
            self.checksums
                .set(volume, trees, space, key.objectid, bytes)
                .await?;
        }
        volume.stage(key.objectid, bytes, concurrency).await?;
        Ok(key)
    }

    /// Reserve an unpublished data extent and register its first file mapping.
    /// The transaction must stage the data and its checksums before commit.
    pub(crate) async fn allocate_data<S: BlockSource>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
        length: u64,
        reference: DataReference,
    ) -> Result<Key, WriteError> {
        let address = space.reserve_data(length)?;
        let key = Key::new(address, item_type::EXTENT_ITEM, length);
        self.insert_data(volume, trees, space, key, reference)
            .await?;
        Ok(key)
    }

    /// Takes the data extent `key` names, which is already on disk but in no
    /// extent item, and registers its first file mapping: Linux's
    /// `btrfs_alloc_logged_file_extent`, which log replay calls for an extent
    /// the log references and the extent tree does not yet hold.
    pub(crate) async fn adopt_data<S: BlockSource>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
        key: Key,
        reference: DataReference,
    ) -> Result<(), WriteError> {
        space.reserve_data_at(key.objectid, key.offset)?;
        self.insert_data(volume, trees, space, key, reference).await
    }

    /// Inserts the extent item of a data extent this transaction reserved,
    /// with its one reference, as `alloc_reserved_file_extent` does.
    async fn insert_data<S: BlockSource>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
        key: Key,
        reference: DataReference,
    ) -> Result<(), WriteError> {
        let extent = DataExtent::new(trees.generation(), reference);
        self.root = trees
            .apply(volume, self.root, key, Some(&extent.encode()), space)
            .await?;
        Ok(())
    }

    /// Replaces the checksums of `logical`'s sectors with `digests`, one per
    /// sector: what log replay copies from the log's checksum items.
    pub(crate) async fn store_checksums<S: BlockSource>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
        logical: u64,
        digests: &[u8],
    ) -> Result<(), WriteError> {
        self.checksums
            .store(volume, trees, space, logical, digests)
            .await
    }

    /// Change a counted data reference. The signed delta represents file extent
    /// items, not bytes. A zero result retires the whole physical allocation;
    /// it does not make that range reusable before commit/read-side retirement.
    pub(crate) async fn update_data<S: BlockSource, R: DataBackref>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
        change: DataChange<R>,
    ) -> Result<bool, WriteError> {
        let DataChange {
            key,
            reference,
            delta,
        } = change;
        let defect = |defect| WriteError::Data { key, defect };
        let bytes = trees
            .get(volume, self.root, key)
            .await?
            .ok_or_else(|| defect(DataDefect::Missing))?;
        let mut extent =
            DataExtent::parse(key, &bytes, volume.superblock().sectorsize()).map_err(defect)?;
        let inline = reference.inline(&extent);
        let initial = reference.key(key.objectid);
        let mut search = initial;
        let mut vacant = initial;
        let mut external = None;
        // Deleting a colliding key can leave a hole before another matching
        // reference. Search the remaining suffix rather than stopping at holes.
        while let Some((found, bytes)) = trees.ceiling(volume, self.root, search).await? {
            if found.objectid != initial.objectid || found.item_type != initial.item_type {
                break;
            }
            if initial.item_type == item_type::SHARED_DATA_REF && found != initial {
                break;
            }
            if found == vacant {
                vacant.offset = vacant
                    .offset
                    .checked_add(1)
                    .ok_or_else(|| defect(DataDefect::Count))?;
            }
            if let Some(count) = reference.decode(&bytes).map_err(defect)?
                && external.replace((found, count)).is_some()
            {
                return Err(defect(DataDefect::Duplicate));
            }
            let Some(offset) = found.offset.checked_add(1) else {
                break;
            };
            search.offset = offset;
            if initial.item_type == item_type::SHARED_DATA_REF {
                break;
            }
        }
        if inline.is_some() && external.is_some() {
            return Err(defect(DataDefect::Duplicate));
        }
        let old = inline.or(external.map(|(_, count)| count)).unwrap_or(0);
        let inline_total: u64 = extent
            .roots
            .values()
            .chain(extent.parents.values())
            .map(|&count| u64::from(count))
            .sum();
        if inline_total + u64::from(external.map_or(0, |(_, count)| count)) > extent.refs {
            return Err(defect(DataDefect::Count));
        }
        let count = i64::from(old)
            .checked_add(delta)
            .and_then(|count| u32::try_from(count).ok())
            .ok_or_else(|| defect(DataDefect::Count))?;
        let refs = extent
            .refs
            .checked_add_signed(delta)
            .ok_or_else(|| defect(DataDefect::Count))?;
        if delta == 0 {
            return Ok(false);
        }
        if inline.is_some() {
            reference.set_inline(&mut extent, count);
        } else {
            let reference_key = external.map_or(vacant, |(key, _)| key);
            let payload = (count != 0).then(|| reference.encode(count));
            self.root = trees
                .apply(volume, self.root, reference_key, payload.as_deref(), space)
                .await?;
        }
        extent.refs = refs;
        if refs == 0 {
            let first = Key::new(key.objectid, item_type::TREE_BLOCK_REF, 0);
            if let Some((remaining, _)) = trees.ceiling(volume, self.root, first).await?
                && remaining.objectid == key.objectid
                && remaining.item_type <= item_type::SHARED_DATA_REF
            {
                return Err(defect(DataDefect::Count));
            }
        }
        let payload = (refs != 0).then(|| extent.encode());
        self.root = trees
            .apply(volume, self.root, key, payload.as_deref(), space)
            .await?;
        if refs == 0 {
            self.checksums
                .remove(
                    volume,
                    trees,
                    space,
                    key.objectid..key.objectid + key.offset,
                )
                .await?;
            if !space.cancel_data(key.objectid) {
                self.retired_extents.insert_mut(key.objectid, key.offset);
            }
        }
        Ok(refs == 0)
    }

    #[cfg(test)]
    pub(crate) async fn new<S: BlockSource>(
        volume: &Volume<S>,
        root: TreeRoot,
        space: &Space,
        checksum_root: TreeRoot,
    ) -> Result<Self, WriteError> {
        let free_space = Allocation::load_free(volume, space).await?;
        Ok(Self::with_free(
            volume,
            root,
            space,
            checksum_root,
            free_space,
        ))
    }

    /// Start from a committed free-space tree instead of walking the disk.
    pub(crate) fn with_free<S: BlockSource>(
        volume: &Volume<S>,
        root: TreeRoot,
        space: &Space,
        checksum_root: TreeRoot,
        free_space: Option<FreeTree>,
    ) -> Self {
        Self {
            root,
            nodesize: u64::from(volume.superblock().nodesize()),
            groups: space.group_root(),
            free_space,
            checksums: Checksums {
                root: checksum_root,
            },
            group_used: RedBlackTreeMapSync::new_sync(),
            retired_extents: RedBlackTreeMapSync::new_sync(),
            registered: RedBlackTreeMapSync::new_sync(),
            processed_retired: 0,
        }
    }

    /// Reconcile transaction-owned allocations, including blocks allocated by
    /// these extent-tree edits. Private tree blocks keep their addresses, so
    /// further passes are needed only for structural changes and first keys.
    /// An error aborts the enclosing transaction, like any other tree edit.
    pub(crate) async fn sync_allocations<S: BlockSource>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
    ) -> Result<(), WriteError> {
        // BTRFS_FEATURE_INCOMPAT_SKINNY_METADATA, from btrfs.h.
        let skinny = volume.superblock().incompat_flags() & (1 << 8) != 0;
        loop {
            self.sync_references(volume, trees, space).await?;
            let free_changed = if let Some(free) = &mut self.free_space {
                free.sync(volume, trees, space, &self.retired_extents)
                    .await?
            } else {
                false
            };
            let groups_changed = self.sync_groups(volume, trees, space).await?;
            let current: BTreeMap<_, _> = space.metadata().collect();
            let removed: Vec<_> = self
                .registered
                .iter()
                .filter(|(address, _)| !current.contains_key(address))
                .map(|(&address, &record)| (address, record))
                .collect();
            let changed: Vec<_> = current
                .iter()
                .filter(|(address, record)| self.registered.get(address) != Some(record))
                .map(|(&address, &record)| (address, record))
                .collect();
            if removed.is_empty() && changed.is_empty() && !groups_changed && !free_changed {
                if self.processed_retired == trees.retired().len() {
                    return Ok(());
                }
                continue;
            }
            for (address, record) in removed {
                let extent = metadata_extent(trees.generation(), record, skinny);
                self.root = trees
                    .apply(
                        volume,
                        self.root,
                        extent.key(address, self.nodesize),
                        None,
                        space,
                    )
                    .await?;
                self.registered.remove_mut(&address);
            }
            for (address, record) in changed {
                let extent = metadata_extent(trees.generation(), record, skinny);
                let key = extent.key(address, self.nodesize);
                if let Some(&old) = self.registered.get(&address) {
                    let old_key = metadata_extent(trees.generation(), old, skinny)
                        .key(address, self.nodesize);
                    if old_key != key {
                        self.root = trees.apply(volume, self.root, old_key, None, space).await?;
                    }
                }
                self.root = trees
                    .apply(volume, self.root, key, Some(&extent.encode()), space)
                    .await?;
                self.registered.insert_mut(address, record);
            }
        }
    }

    pub(crate) fn free_root(&self) -> Option<TreeRoot> {
        self.free_space.as_ref().map(|free| free.root)
    }

    /// The allocation state the committed generation will have, valid once the
    /// accounting loop has converged. Call before `take_retired`.
    pub(crate) fn committed(&self, space: &Space) -> Result<Allocation, WriteError> {
        Ok(Allocation {
            space: space.committed(&self.retired_extents)?,
            free: self.free_space.clone(),
        })
    }

    pub(crate) fn checksum_root(&self) -> TreeRoot {
        self.checksums.root
    }

    pub(crate) fn bytes_used(&self, space: &Space) -> Result<u64, WriteError> {
        Ok(space
            .group_usage(&self.retired_extents)?
            .into_iter()
            .map(|(_, used)| used)
            .sum())
    }

    #[cfg(feature = "write")]
    pub(crate) fn take_retired(&mut self) -> RedBlackTreeMapSync<u64, u64> {
        core::mem::take(&mut self.retired_extents)
    }

    pub(crate) fn group_root(&self) -> TreeRoot {
        if self.groups.tree == objectid::EXTENT_TREE {
            self.root
        } else {
            self.groups
        }
    }

    async fn sync_groups<S: BlockSource>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
    ) -> Result<bool, WriteError> {
        let mut changed = false;
        for (key, used) in space.group_usage(&self.retired_extents)? {
            if self.group_used.get(&key) == Some(&used) {
                continue;
            }
            let root = self.group_root();
            let mut bytes = trees
                .get(volume, root, key)
                .await?
                .ok_or(SpaceError::Corrupt {
                    tree: root.tree,
                    key,
                    defect: SpaceDefect::ChunkMismatch,
                })?;
            if bytes.len() < 24 {
                return Err(SpaceError::Corrupt {
                    tree: root.tree,
                    key,
                    defect: SpaceDefect::Truncated,
                }
                .into());
            }
            if u64::from_le_bytes(bytes[..8].try_into().unwrap()) != used {
                bytes[..8].copy_from_slice(&used.to_le_bytes());
                self.set_group_item(volume, trees, space, key, Some(&bytes))
                    .await?;
                changed = true;
            }
            self.group_used.insert_mut(key, used);
        }
        Ok(changed)
    }

    /// Forget the usage recorded for a block group that no longer exists.
    pub(crate) fn forget_group(&mut self, key: &Key) {
        self.group_used.remove_mut(key);
    }

    /// Insert, replace or delete a block-group item in whichever tree holds
    /// them, the extent tree or the block-group tree.
    pub(crate) async fn set_group_item<S: BlockSource>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
        key: Key,
        value: Option<&[u8]>,
    ) -> Result<(), WriteError> {
        let root = trees
            .apply(volume, self.group_root(), key, value, space)
            .await?;
        if root.tree == objectid::EXTENT_TREE {
            self.root = root;
        } else {
            self.groups = root;
        }
        Ok(())
    }

    /// Read the current extent record, including earlier reference edits.
    pub(crate) async fn metadata<S: BlockSource>(
        &self,
        volume: &Volume<S>,
        trees: &Trees,
        block: MetadataBlock,
    ) -> Result<(Key, MetadataExtent), WriteError> {
        let mut key = Key::new(
            block.address,
            item_type::METADATA_ITEM,
            u64::from(block.level),
        );
        let bytes = if let Some(bytes) = trees.get(volume, self.root, key).await? {
            bytes
        } else {
            // Enabling skinny metadata does not rewrite pre-existing fat items.
            key = Key::new(block.address, item_type::EXTENT_ITEM, self.nodesize);
            trees
                .get(volume, self.root, key)
                .await?
                .ok_or(WriteError::MissingExtent {
                    address: block.address,
                    level: block.level,
                })?
        };
        let defect = |defect| WriteError::Allocation { key, defect };
        let extent = MetadataExtent::parse(key, &bytes, self.nodesize).map_err(defect)?;
        if extent.level() != block.level {
            return Err(defect(AllocationDefect::Metadata));
        }
        Ok((key, extent))
    }

    /// Add a root or parent reference to an existing committed block. Both the
    /// count and external key live in the overlay; any failure aborts it.
    pub(crate) async fn add_metadata<S: BlockSource, A: MetadataAllocator>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        allocator: &mut A,
        block: MetadataBlock,
        reference: MetadataReference,
    ) -> Result<(), WriteError> {
        let (key, mut extent) = self.metadata(volume, trees, block).await?;
        let reference_key = reference.key(block.address);
        let external = trees.get(volume, self.root, reference_key).await?;
        extent
            .add_external(reference, external.as_deref())
            .map_err(|defect| WriteError::Allocation { key, defect })?;
        self.root = trees
            .apply(volume, self.root, reference_key, Some(&[]), allocator)
            .await?;
        self.root = trees
            .apply(volume, self.root, key, Some(&extent.encode()), allocator)
            .await?;
        Ok(())
    }

    /// Mark a block after its outgoing references have been converted to
    /// parent references. This never edits the committed block's own header.
    pub(crate) async fn set_full_backrefs<S: BlockSource, A: MetadataAllocator>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        allocator: &mut A,
        block: MetadataBlock,
    ) -> Result<(), WriteError> {
        let (key, mut extent) = self.metadata(volume, trees, block).await?;
        if !extent.full_backrefs() {
            extent.set_full_backrefs();
            self.root = trees
                .apply(volume, self.root, key, Some(&extent.encode()), allocator)
                .await?;
        }
        Ok(())
    }

    /// Removes a committed block reference. Private allocation cancellation is
    /// handled by `sync_allocations`. Returns true only for its last reference. As with any tree
    /// edit, an error aborts the enclosing transaction, including earlier edits.
    pub(crate) async fn remove_metadata<S: BlockSource, A: MetadataAllocator>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        allocator: &mut A,
        block: MetadataBlock,
        reference: MetadataReference,
    ) -> Result<bool, WriteError> {
        let (key, mut extent) = self.metadata(volume, trees, block).await?;
        let defect = |defect| WriteError::Allocation { key, defect };
        let reference_key = reference.key(block.address);
        let external = trees.get(volume, self.root, reference_key).await?;
        let last = extent
            .remove(reference, external.as_deref())
            .map_err(defect)?;
        if external.is_some() {
            self.root = trees
                .apply(volume, self.root, reference_key, None, allocator)
                .await?;
        }
        let payload = (!last).then(|| extent.encode());
        self.root = trees
            .apply(volume, self.root, key, payload.as_deref(), allocator)
            .await?;
        if last {
            self.retired_extents
                .insert_mut(block.address, self.nodesize);
        }
        Ok(last)
    }
}

fn metadata_extent(generation: u64, record: MetadataReservation, skinny: bool) -> MetadataExtent {
    MetadataExtent::new(
        generation,
        record.tree,
        record.level,
        (!skinny).then_some(record.first),
    )
}
