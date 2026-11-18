//! Transaction-owned space reservations derived from allocation metadata.
//! Mount walks immutable trees. A transaction then owns its free-range map
//! exclusively; it never holds a lock across I/O and never returns retired
//! committed extents to this map before publication/read-side retirement.

use alloc::vec::Vec;
use core::ops::Range;

use rpds::{RedBlackTreeMapSync, VectorSync};
use thiserror::Error;

use crate::allocation::data::{DataDefect, DataExtent};
use crate::allocation::{AllocationDefect, MetadataExtent};
use crate::free_space::{FreeGroup, FreeSpaceDefect, Intervals};
use crate::fs::find_root;
use crate::ondisk::{item_type, objectid};
use crate::source::BlockSource;
use crate::tree::write::{MetadataAllocator, WriteError};
use crate::tree::{Cursor, TreeRoot};
use crate::volume::Volume;
use crate::{Error, Key};

// BTRFS_BLOCK_GROUP_{DATA,SYSTEM,METADATA}, from btrfs_tree.h.
pub(crate) const DATA: u64 = 1;
pub(crate) const SYSTEM: u64 = 2;
pub(crate) const METADATA: u64 = 4;
// BTRFS_FEATURE_COMPAT_RO_BLOCK_GROUP_TREE, from btrfs.h.
const BLOCK_GROUP_TREE: u64 = 1 << 3;

#[derive(Debug, Error)]
pub enum SpaceError {
    #[error(transparent)]
    Read(#[from] Error),
    #[error("allocation tree {tree}, item {key}: {defect}")]
    Corrupt {
        tree: u64,
        key: Key,
        defect: SpaceDefect,
    },
    #[error("no free block-group range can hold {bytes} bytes with flags {flags:#x}")]
    Exhausted { bytes: u64, flags: u64 },
    #[error("data reservation length {bytes} must be a nonzero multiple of sector size {sector}")]
    DataLength { bytes: u64, sector: u64 },
    /// A data extent claimed at a given address is not free there: outside
    /// every data block group, unaligned, or overlapping an allocation.
    #[error("data range {address:#x}+{bytes:#x} is not free in a data block group")]
    NotFree { address: u64, bytes: u64 },
}

#[derive(Debug, Error)]
pub enum SpaceDefect {
    #[error(transparent)]
    FreeSpace(#[from] FreeSpaceDefect),
    #[error(transparent)]
    Allocation(#[from] AllocationDefect),
    #[error(transparent)]
    Data(#[from] DataDefect),
    #[error("truncated allocation record")]
    Truncated,
    #[error("unaligned, empty, or overflowing extent range")]
    Range,
    #[error("block group does not match its chunk")]
    ChunkMismatch,
    #[error("allocated extent is outside a block group or overlaps another extent")]
    ExtentOverlap,
    #[error("extent has no references or inconsistent flags")]
    ExtentHeader,
    #[error("block-group used-byte accounting underflows or exceeds its length")]
    Accounting,
    #[error(
        "block group reports {declared} allocated bytes but the extent tree accounts for {found}"
    )]
    UsedBytes { declared: u64, found: u64 },
}

#[derive(Clone)]
#[cfg_attr(test, derive(Debug, PartialEq))]
struct Group {
    start: u64,
    length: u64,
    flags: u64,
    declared_used: u64,
    found_used: u64,
    free: Intervals,
    excluded_free: Intervals,
    /// Allocated by the running transaction: not yet published, so never
    /// removed by it.
    fresh: bool,
}

impl Group {
    /// Nothing in the group is allocated, reserved, or held back for a reader:
    /// every byte is free, or lies in a superblock `stripe`, which is free by
    /// definition. Ranges `Space::pin` took out for a reader are neither.
    fn is_unused(&self, stripes: &[Range<u64>]) -> bool {
        let free: u64 = self.free.iter().map(|range| range.end - range.start).sum();
        let end = self.start + self.length;
        let reserved: u64 = stripes
            .iter()
            .map(|stripe| {
                stripe
                    .end
                    .min(end)
                    .saturating_sub(stripe.start.max(self.start))
            })
            .sum();
        !self.fresh
            && self.declared_used == 0
            && self.found_used == 0
            && free + reserved == self.length
    }

    fn contains(&self, range: &Range<u64>) -> bool {
        self.start <= range.start && range.end <= self.start + self.length
    }

    /// A reserved mirror stripe may already contain an extent-tree reservation.
    /// Remove only its intersections with the remaining free intervals.
    fn exclude(&mut self, range: &Range<u64>) {
        let overlaps: Vec<_> = self
            .free
            .iter()
            .filter(|free| free.start < range.end && free.end > range.start)
            .map(|free| free.start.max(range.start)..free.end.min(range.end))
            .collect();
        for overlap in overlaps {
            self.excluded_free
                .release(overlap.clone())
                .expect("excluded ranges are disjoint");
            assert!(self.free.occupy(overlap));
        }
    }
}

/// The extent item/back-reference changes the transaction must publish for a
/// newly reserved metadata block. No record is written by the allocator alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MetadataReservation {
    pub(crate) tree: u64,
    pub(crate) level: u8,
    pub(crate) first: Key,
}

#[derive(Clone)]
#[cfg_attr(test, derive(Debug))]
pub(crate) struct Space {
    groups: VectorSync<Group>,
    group_root: TreeRoot,
    free_root: Option<TreeRoot>,
    nodesize: u64,
    sector: u64,
    metadata: RedBlackTreeMapSync<u64, MetadataReservation>,
    data: RedBlackTreeMapSync<u64, u64>,
    protected_data: RedBlackTreeMapSync<u64, u64>,
    deferred_data: RedBlackTreeMapSync<u64, u64>,
    /// Private data this transaction released while a reader's view of the
    /// running transaction still reaches it: unallocated in the generation
    /// being built, but unavailable to this writer until no such view lives.
    held_data: RedBlackTreeMapSync<u64, u64>,
}

/// The allocation state a tree-root-independent comparison sees: what a
/// committed generation's disk image decodes to.
#[cfg(test)]
impl PartialEq for Space {
    fn eq(&self, other: &Self) -> bool {
        self.groups == other.groups
            && self.nodesize == other.nodesize
            && self.sector == other.sector
            && self.metadata == other.metadata
            && self.data == other.data
            && self.protected_data == other.protected_data
            && self.deferred_data == other.deferred_data
            && self.held_data == other.held_data
    }
}

impl Space {
    /// Prior private data remains reachable from the operation checkpoint.
    /// Its addresses cannot be recycled until the operation is accepted.
    pub(crate) fn begin_operation(&mut self) {
        self.protected_data = self.data.clone();
    }

    /// Ends an operation. Private data it released returns to the free
    /// ranges unless `viewed` says a reader's view of the running
    /// transaction still reaches it; data held for such a view earlier
    /// returns once `viewed` no longer names it.
    pub(crate) fn accept_operation(&mut self, viewed: impl Fn(u64) -> bool) {
        let deferred = core::mem::take(&mut self.deferred_data);
        self.protected_data = RedBlackTreeMapSync::new_sync();
        for (&address, &length) in &deferred {
            if viewed(address) {
                self.held_data.insert_mut(address, length);
            } else {
                self.release_private(address..address + length);
            }
        }
        let released: Vec<(u64, u64)> = self
            .held_data
            .iter()
            .filter(|&(&address, _)| !viewed(address))
            .map(|(&address, &length)| (address, length))
            .collect();
        for (address, length) in released {
            self.held_data.remove_mut(&address);
            self.release_private(address..address + length);
        }
    }

    /// The private data extents a view of the running transaction taken now
    /// can reach.
    pub(crate) fn private_data(&self) -> RedBlackTreeMapSync<u64, u64> {
        self.data.clone()
    }

    #[cfg(test)]
    pub(crate) fn held_data_bytes(&self) -> u64 {
        self.held_data.values().sum()
    }

    /// Takes the data still held for views as the transaction commits: free
    /// in the committed generation, so the caller keeps it from reuse
    /// through the reader pins of the generation the views started from.
    pub(crate) fn take_held(&mut self) -> RedBlackTreeMapSync<u64, u64> {
        core::mem::take(&mut self.held_data)
    }

    /// Reader-pinned storage is free on disk but unavailable to this writer.
    #[cfg(feature = "write")]
    pub(crate) fn pin(&mut self, range: Range<u64>) {
        for index in 0..self.groups.len() {
            let group = &mut self.groups[index];
            group.exclude(&range);
        }
    }

    /// Walks the allocation trees once and validates them: used-byte
    /// accounting, extent overlap, chunk agreement.
    pub(crate) async fn load_committed<S: BlockSource>(
        volume: &Volume<S>,
        roots: TreeRoot,
    ) -> Result<Self, SpaceError> {
        let sb = volume.superblock();
        let extent_root = find_root(volume, roots, objectid::EXTENT_TREE).await?.root;
        let group_root = if sb.compat_ro_flags() & BLOCK_GROUP_TREE != 0 {
            find_root(volume, roots, objectid::BLOCK_GROUP_TREE)
                .await?
                .root
        } else {
            extent_root
        };
        // BTRFS_FEATURE_COMPAT_RO_FREE_SPACE_TREE.
        let free_root = if sb.compat_ro_flags() & 1 != 0 {
            Some(
                find_root(volume, roots, objectid::FREE_SPACE_TREE)
                    .await?
                    .root,
            )
        } else {
            None
        };
        let mut groups = Vec::new();
        let mut cursor = Cursor::ceiling(volume, group_root, Key::new(0, 0, 0)).await?;
        while let (Some(key), Some(item)) = (cursor.key(), cursor.item()) {
            if key.item_type == item_type::BLOCK_GROUP_ITEM {
                let defect = |defect| SpaceError::Corrupt {
                    tree: group_root.tree,
                    key,
                    defect,
                };
                if item.len() < 24 {
                    return Err(defect(SpaceDefect::Truncated));
                }
                let used = u64::from_le_bytes(item[..8].try_into().unwrap());
                let flags = u64::from_le_bytes(item[16..24].try_into().unwrap());
                if !volume
                    .chunks()
                    .matches_group(key.objectid, key.offset, flags)
                {
                    return Err(defect(SpaceDefect::ChunkMismatch));
                }
                groups.push(Group {
                    start: key.objectid,
                    length: key.offset,
                    flags,
                    declared_used: used,
                    found_used: 0,
                    free: Intervals::whole(key.objectid..key.objectid + key.offset),
                    excluded_free: Intervals::default(),
                    fresh: false,
                });
            }
            cursor.advance(volume).await?;
        }
        let mut space = Self {
            groups: groups.into_iter().collect(),
            group_root,
            free_root,
            nodesize: u64::from(sb.nodesize()),
            sector: u64::from(sb.sectorsize()),
            metadata: RedBlackTreeMapSync::new_sync(),
            data: RedBlackTreeMapSync::new_sync(),
            protected_data: RedBlackTreeMapSync::new_sync(),
            deferred_data: RedBlackTreeMapSync::new_sync(),
            held_data: RedBlackTreeMapSync::new_sync(),
        };
        let mut cursor = Cursor::ceiling(volume, extent_root, Key::new(0, 0, 0)).await?;
        while let (Some(key), Some(item)) = (cursor.key(), cursor.item()) {
            if key.item_type == item_type::EXTENT_ITEM || key.item_type == item_type::METADATA_ITEM
            {
                let defect = |defect| SpaceError::Corrupt {
                    tree: extent_root.tree,
                    key,
                    defect,
                };
                if item.len() < 24 {
                    return Err(defect(SpaceDefect::Truncated));
                }
                let refs = u64::from_le_bytes(item[..8].try_into().unwrap());
                let flags = u64::from_le_bytes(item[16..24].try_into().unwrap());
                // BTRFS_EXTENT_FLAG_DATA=1 and TREE_BLOCK=2 are mutually exclusive.
                if refs == 0
                    || !matches!(flags & 3, 1 | 2)
                    || (key.item_type == item_type::METADATA_ITEM && flags & 3 != 2)
                {
                    return Err(defect(SpaceDefect::ExtentHeader));
                }
                if flags & 2 != 0 {
                    MetadataExtent::parse(key, item, space.nodesize)
                        .map_err(|error| defect(error.into()))?;
                } else {
                    DataExtent::parse(key, item, sb.sectorsize())
                        .map_err(|error| defect(error.into()))?;
                }
                let length = if key.item_type == item_type::METADATA_ITEM {
                    space.nodesize
                } else {
                    key.offset
                };
                let end = key
                    .objectid
                    .checked_add(length)
                    .ok_or_else(|| defect(SpaceDefect::Range))?;
                if length == 0
                    || !key.objectid.is_multiple_of(space.sector)
                    || !length.is_multiple_of(space.sector)
                {
                    return Err(defect(SpaceDefect::Range));
                }
                let range = key.objectid..end;
                let index = space
                    .groups
                    .iter()
                    .position(|group| group.contains(&range))
                    .ok_or_else(|| defect(SpaceDefect::ExtentOverlap))?;
                let group = &mut space.groups[index];
                if !group.free.occupy(range) {
                    return Err(defect(SpaceDefect::ExtentOverlap));
                }
                group.found_used += length;
            }
            cursor.advance(volume).await?;
        }
        for group in &space.groups {
            if group.declared_used != group.found_used {
                return Err(SpaceError::Corrupt {
                    tree: group_root.tree,
                    key: Key::new(group.start, item_type::BLOCK_GROUP_ITEM, group.length),
                    defect: SpaceDefect::UsedBytes {
                        declared: group.declared_used,
                        found: group.found_used,
                    },
                });
            }
        }
        Ok(space)
    }

    /// The committed state with this transaction's exclusions applied:
    /// superblock mirror stripes are never allocated.
    #[cfg(test)]
    pub(crate) async fn load<S: BlockSource>(
        volume: &Volume<S>,
        roots: TreeRoot,
    ) -> Result<Self, SpaceError> {
        let mut space = Self::load_committed(volume, roots).await?;
        space.exclude_superblock_stripes(volume.chunks());
        Ok(space)
    }

    pub(crate) fn exclude_superblock_stripes(&mut self, chunks: &crate::chunk::ChunkMap) {
        let excluded = chunks.superblock_stripes();
        for index in 0..self.groups.len() {
            for range in &excluded {
                self.groups[index].exclude(range);
            }
        }
    }

    /// Point the carried state at the roots of the generation a transaction
    /// starts from. The allocation tree roots move with every commit.
    pub(crate) fn rebind(&mut self, roots: &crate::roots::Roots) -> Result<(), WriteError> {
        self.group_root = roots.get(self.group_root.tree)?;
        if let Some(free) = &mut self.free_root {
            *free = roots.get(free.tree)?;
        }
        Ok(())
    }

    /// The state the next generation's disk image will decode to: each group's
    /// free ranges with this transaction's reservations occupied and its
    /// retired extents released, its usage as declared by the block-group
    /// items, and no private reservations. Mirror stripes and reader pins are
    /// not applied; the next transaction applies them again.
    pub(crate) fn committed(
        &self,
        retired: &RedBlackTreeMapSync<u64, u64>,
    ) -> Result<Self, SpaceError> {
        let free = self.committed_free(retired)?;
        let usage = self.group_usage(retired)?;
        let groups = self
            .groups
            .iter()
            .zip(free)
            .zip(usage)
            .map(|((group, free), (_, used))| Group {
                start: group.start,
                length: group.length,
                flags: group.flags,
                declared_used: used,
                found_used: used,
                free: free.free,
                excluded_free: Intervals::default(),
                fresh: false,
            })
            .collect();
        Ok(Self {
            groups,
            group_root: self.group_root,
            free_root: self.free_root,
            nodesize: self.nodesize,
            sector: self.sector,
            metadata: RedBlackTreeMapSync::new_sync(),
            data: RedBlackTreeMapSync::new_sync(),
            protected_data: RedBlackTreeMapSync::new_sync(),
            deferred_data: RedBlackTreeMapSync::new_sync(),
            held_data: RedBlackTreeMapSync::new_sync(),
        })
    }

    fn reserve(&mut self, length: u64, flags: u64) -> Result<u64, SpaceError> {
        for index in 0..self.groups.len() {
            let group = &self.groups[index];
            if group.flags & flags == 0 {
                continue;
            }
            let found = group
                .free
                .iter()
                .find(|range| range.end - range.start >= length)
                .map(|range| range.start);
            if let Some(start) = found {
                assert!(self.groups[index].free.occupy(start..start + length));
                return Ok(start);
            }
        }
        Err(SpaceError::Exhausted {
            bytes: length,
            flags,
        })
    }

    /// Whether some block group with a `kind` bit has a free range of `length`.
    pub(crate) fn can_reserve(&self, length: u64, kind: u64) -> bool {
        self.largest_free(kind) >= length
    }

    /// The largest free range in any block group with a `kind` bit.
    pub(crate) fn largest_free(&self, kind: u64) -> u64 {
        self.groups
            .iter()
            .filter(|group| group.flags & kind != 0)
            .flat_map(|group| group.free.iter())
            .map(|range| range.end - range.start)
            .max()
            .unwrap_or(0)
    }

    /// Total and allocatable bytes of the block groups with a `kind` bit. What
    /// is not allocatable is used, reserved, pinned for a reader or a
    /// superblock mirror stripe; Linux's `btrfs_space_info_used` counts all of
    /// those as used.
    pub(crate) fn totals(&self, kind: u64) -> (u64, u64) {
        self.groups
            .iter()
            .filter(|group| group.flags & kind != 0)
            .fold((0, 0), |(total, free), group| {
                let available: u64 = group.free.iter().map(|range| range.end - range.start).sum();
                (total + group.length, free + available)
            })
    }

    /// The flags of the last block group with a `kind` bit: the profile a new
    /// chunk of that kind takes, as Linux derives it from the profiles the
    /// filesystem already has.
    pub(crate) fn latest_flags(&self, kind: u64) -> Option<u64> {
        self.groups
            .iter()
            .rev()
            .find(|group| group.flags & kind != 0)
            .map(|group| group.flags)
    }

    /// The groups `btrfs_delete_unused_bgs` would delete: unused ones, except
    /// that a group is kept while it is the last of its type (`flags` holding
    /// the data, system or metadata bit) and while this transaction made it.
    /// A group a reader still pins is not unused: the pinned extents are
    /// absent from its free ranges.
    pub(crate) fn unused_groups(&self, chunks: &crate::chunk::ChunkMap) -> Vec<(u64, u64, u64)> {
        let stripes = chunks.superblock_stripes();
        let mut per_type = [DATA, SYSTEM, METADATA].map(|kind| {
            self.groups
                .iter()
                .filter(|group| group.flags & kind != 0)
                .count()
        });
        let index = |flags: u64| {
            [DATA, SYSTEM, METADATA]
                .iter()
                .position(|&kind| flags & kind != 0)
        };
        let mut unused = Vec::new();
        for group in self.groups.iter().filter(|group| group.is_unused(&stripes)) {
            let Some(kind) = index(group.flags) else {
                continue;
            };
            if per_type[kind] > 1 {
                per_type[kind] -= 1;
                unused.push((group.start, group.length, group.flags));
            }
        }
        unused
    }

    /// Drops a group `unused_groups` named, so no later reservation can take a
    /// block from it.
    pub(crate) fn remove_group(&mut self, start: u64) {
        self.groups = self
            .groups
            .iter()
            .filter(|group| group.start != start)
            .cloned()
            .collect();
    }

    /// Adds the block group of a chunk this transaction allocated. Its logical
    /// range is above every existing group, so order is preserved.
    pub(crate) fn push_group(
        &mut self,
        start: u64,
        length: u64,
        flags: u64,
        chunks: &crate::chunk::ChunkMap,
    ) {
        let end = start + length;
        assert!(
            self.groups
                .last()
                .is_none_or(|last| last.start + last.length <= start),
            "a new block group starts above every existing one"
        );
        let mut group = Group {
            start,
            length,
            flags,
            declared_used: 0,
            found_used: 0,
            free: Intervals::whole(start..end),
            excluded_free: Intervals::default(),
            fresh: true,
        };
        for range in chunks.superblock_stripes() {
            if range.start < end && start < range.end {
                group.exclude(&range);
            }
        }
        self.groups.push_back_mut(group);
    }

    pub(crate) fn reserve_data(&mut self, length: u64) -> Result<u64, SpaceError> {
        if length == 0 || !length.is_multiple_of(self.sector) {
            return Err(SpaceError::DataLength {
                bytes: length,
                sector: self.sector,
            });
        }
        let address = self.reserve(length, DATA)?;
        self.data.insert_mut(address, length);
        Ok(address)
    }

    /// Reserves the data extent at `address`, which another writer already
    /// filled: Linux's `btrfs_alloc_logged_file_extent`, which takes a logged
    /// extent out of the free space (`__exclude_logged_extent`) and accounts
    /// it to its block group. The range may have been excluded by [`Self::pin`]
    /// beforehand, as a mixed block group's logged extents are; every other
    /// byte of it must be free.
    pub(crate) fn reserve_data_at(&mut self, address: u64, length: u64) -> Result<(), SpaceError> {
        let not_free = || SpaceError::NotFree {
            address,
            bytes: length,
        };
        let end = address.checked_add(length).ok_or_else(not_free)?;
        if length == 0
            || !address.is_multiple_of(self.sector)
            || !length.is_multiple_of(self.sector)
        {
            return Err(not_free());
        }
        let range = address..end;
        let index = self
            .groups
            .iter()
            .position(|group| group.flags & DATA != 0 && group.contains(&range))
            .ok_or_else(not_free)?;
        let group = &self.groups[index];
        let pinned: Vec<Range<u64>> = group
            .excluded_free
            .iter()
            .filter(|excluded| excluded.start < end && address < excluded.end)
            .map(|excluded| excluded.start.max(address)..excluded.end.min(end))
            .collect();
        let mut free = Vec::new();
        let mut next = address;
        for part in &pinned {
            if next < part.start {
                free.push(next..part.start);
            }
            next = part.end;
        }
        if next < end {
            free.push(next..end);
        }
        let fits = free.iter().all(|part| {
            group
                .free
                .iter()
                .any(|range| range.start <= part.start && part.end <= range.end)
        });
        if !fits {
            return Err(not_free());
        }
        let group = &mut self.groups[index];
        for part in pinned {
            assert!(
                group.excluded_free.occupy(part),
                "a pinned part is excluded"
            );
        }
        for part in free {
            assert!(group.free.occupy(part), "a checked part is free");
        }
        self.data.insert_mut(address, length);
        Ok(())
    }

    /// Only unpublished allocations can be canceled. A committed extent must
    /// stay pinned for readers even after its final reference is removed.
    pub(crate) fn cancel_data(&mut self, address: u64) -> bool {
        let Some(length) = self.data.get(&address).copied() else {
            return false;
        };
        self.data.remove_mut(&address);
        if self.protected_data.contains_key(&address) {
            self.deferred_data.insert_mut(address, length);
        } else {
            self.release_private(address..address + length);
        }
        true
    }

    fn release_private(&mut self, range: Range<u64>) {
        let index = self
            .groups
            .iter()
            .position(|group| group.contains(&range))
            .expect("reservation belongs to its block group");
        let group = &mut self.groups[index];
        group
            .free
            .release(range)
            .expect("private reservation is allocated");
    }

    /// Free-space-tree state after commit. Unlike allocation availability, this
    /// includes free superblock stripes, extents retired from the old roots,
    /// and private data held for a view of the running transaction.
    pub(crate) fn committed_free(
        &self,
        retired: &RedBlackTreeMapSync<u64, u64>,
    ) -> Result<Vec<FreeGroup>, SpaceError> {
        self.groups
            .iter()
            .map(|group| {
                let range = group.start..group.start + group.length;
                let mut free = group.free.clone();
                for returned in group.excluded_free.iter().chain(
                    retired
                        .range(range.clone())
                        .chain(self.held_data.range(range.clone()))
                        .map(|(&address, &length)| address..address + length),
                ) {
                    free.release(returned)
                        .map_err(|defect| SpaceError::Corrupt {
                            tree: objectid::FREE_SPACE_TREE,
                            key: Key::new(group.start, item_type::FREE_SPACE_INFO, group.length),
                            defect: defect.into(),
                        })?;
                }
                Ok(FreeGroup::new(range, free))
            })
            .collect()
    }

    pub(crate) fn free_root(&self) -> Option<TreeRoot> {
        self.free_root
    }

    pub(crate) fn group_root(&self) -> TreeRoot {
        self.group_root
    }

    /// Usage in the next committed generation. Retired extents reduce disk
    /// accounting but remains absent from the reusable free-range map.
    pub(crate) fn group_usage(
        &self,
        retired: &RedBlackTreeMapSync<u64, u64>,
    ) -> Result<Vec<(Key, u64)>, SpaceError> {
        self.groups
            .iter()
            .map(|group| {
                let range = group.start..group.start + group.length;
                let added = self.metadata.range(range.clone()).count() as u64 * self.nodesize
                    + self
                        .data
                        .range(range.clone())
                        .map(|(_, &length)| length)
                        .sum::<u64>();
                let removed = retired.range(range).map(|(_, &length)| length).sum::<u64>();
                let key = Key::new(group.start, item_type::BLOCK_GROUP_ITEM, group.length);
                let used = group
                    .declared_used
                    .checked_add(added)
                    .and_then(|used| used.checked_sub(removed))
                    .filter(|used| *used <= group.length)
                    .ok_or(SpaceError::Corrupt {
                        tree: self.group_root.tree,
                        key,
                        defect: SpaceDefect::Accounting,
                    })?;
                Ok((key, used))
            })
            .collect()
    }

    /// How many tree blocks this transaction has allocated: each is an
    /// extent item its commit inserts.
    pub(crate) fn metadata_reservations(&self) -> usize {
        self.metadata.size()
    }

    pub(crate) fn metadata(&self) -> impl Iterator<Item = (u64, MetadataReservation)> + '_ {
        self.metadata
            .iter()
            .map(|(&address, &record)| (address, record))
    }

    /// Cancel only allocations owned by this transaction. Published extents
    /// have no entry here and cannot accidentally become reusable this way.
    pub(crate) fn cancel_metadata(&mut self, address: u64) -> bool {
        if !self.metadata.remove_mut(&address) {
            return false;
        }
        self.release_private(address..address + self.nodesize);
        true
    }

    /// Every group's start, length and flags, in address order.
    #[cfg(test)]
    pub(crate) fn group_summary(&self) -> Vec<(u64, u64, u64)> {
        self.groups
            .iter()
            .map(|group| (group.start, group.length, group.flags))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn available_metadata(&self) -> u64 {
        self.groups
            .iter()
            .filter(|group| group.flags & METADATA != 0)
            .flat_map(|group| group.free.iter())
            .map(|range| range.end - range.start)
            .sum()
    }
}

impl MetadataAllocator for Space {
    fn revise(&mut self, address: u64, first: Key) {
        self.metadata
            .get_mut(&address)
            .expect("overlay block owns a reservation")
            .first = first;
    }

    fn cancel(&mut self, address: u64) {
        assert!(
            self.cancel_metadata(address),
            "overlay block owns a reservation"
        );
    }

    fn allocate(&mut self, tree: u64, level: u8, first: Key) -> Result<u64, WriteError> {
        let flags = if tree == objectid::CHUNK_TREE {
            SYSTEM
        } else {
            METADATA
        };
        let address = self.reserve(self.nodesize, flags)?;
        self.metadata
            .insert_mut(address, MetadataReservation { tree, level, first });
        Ok(address)
    }
}
