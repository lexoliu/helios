//! Metadata reservations, after Linux's (`fs/btrfs/block-rsv.c`,
//! `fs/btrfs/space-info.c`, `start_transaction` in `fs/btrfs/transaction.c`).
//!
//! A mutation counts the tree items it may touch, as the Linux operation
//! that does the same counts its `num_items`, and reserves before it changes
//! anything:
//!
//! - what it may COW itself, `btrfs_calc_insert_metadata_size` of its items
//!   (one root-to-leaf path with room to split per item), which its own
//!   allocations consume; and
//! - what the commit will do for it, `btrfs_calc_delayed_ref_bytes` of its
//!   items: the extent-tree and free-space-tree updates Linux runs as
//!   delayed refs, which here run only at commit.
//!
//! The running transaction holds, for its commit, what Linux's
//! `delayed_refs_rsv` holds: `btrfs_calc_delayed_ref_bytes` of every ref
//! still to run (`btrfs_update_delayed_refs_rsv` sizes it by the ref heads
//! a transaction added), which here is one extent item per tree block the
//! transaction allocated and one per committed block it retired.
//!
//! A mutation fits when its own bytes, the commit hold it may grow to, and
//! the commit's own reserve ([`Transaction::metadata_reserve`], Linux's
//! global block reserve) fit in the free metadata space. One that does not
//! fit is first given what a commit and then a new chunk free, the order of
//! Linux's metadata flush states, where `FLUSH_DELAYED_REFS` (a commit here)
//! precedes `ALLOC_CHUNK`; one that still does not fit is refused before it
//! changes anything. An applied mutation that leaves less free than the
//! commit's hold and reserve is rolled back, so a commit always has the
//! space for the work it accepted.

use super::Transaction;
use crate::ondisk::{RawHeader, RawItem, objectid};
use crate::source::BlockSink;
use crate::space::{METADATA, SpaceError};
use crate::tree::write::WriteError;

/// `BTRFS_MAX_LEVEL`.
const MAX_LEVEL: u64 = 8;

/// `BTRFS_MAX_EXTENT_SIZE`, `SZ_128M` in `fs/btrfs/fs.h`: the longest data
/// extent, so a write of more than this takes another extent item.
const MAX_EXTENT_SIZE: u64 = 128 << 20;

/// `BTRFS_UNLINK_METADATA_UNITS` in `fs/btrfs/fs.h`.
const UNLINK_ITEMS: u64 = 6;

/// `struct btrfs_item`, the header of every leaf item.
const ITEM_HEADER_BYTES: u64 = size_of::<RawItem>() as u64;

/// The largest extent-tree item one tree-block ref adds: a non-skinny
/// EXTENT_ITEM's header, `btrfs_extent_item` (24 bytes),
/// `btrfs_tree_block_info` (18) and one `btrfs_extent_inline_ref` with its
/// offset (9 + 8), from `btrfs_tree.h`.
const EXTENT_REF_ITEM_BYTES: u64 = ITEM_HEADER_BYTES + 24 + 18 + 9 + 8;

/// `BTRFS_FREE_SPACE_BITMAP_SIZE` and `BTRFS_FREE_SPACE_BITMAP_BITS` in
/// `fs/btrfs/free-space-tree.h`: a bitmap item's payload and the sectors it
/// covers.
const BITMAP_BYTES: u64 = 256;
const BITS_PER_BITMAP: u64 = BITMAP_BYTES * 8;

/// The tree items one mutation may touch, counted as Linux counts them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Units {
    /// Items inserted, deleted or updated anywhere in a tree: each may COW a
    /// path and split it.
    pub(crate) insert: u64,
    /// Items only rewritten in place: each may COW a path, never split it.
    pub(crate) update: u64,
    /// The mutation frees space, so it may take the part of the global
    /// reserve kept for it, as `btrfs_start_transaction_fallback_global_rsv`
    /// lets an unlink do: a full filesystem can still be emptied.
    pub(crate) frees: bool,
}

impl Units {
    pub(crate) const fn items(insert: u64) -> Self {
        Self {
            insert,
            update: 0,
            frees: false,
        }
    }

    /// `btrfs_new_inode_prepare` (`fs/btrfs/inode.c`): the inode item, with
    /// its inode ref in the same batch, the dir item and dir index, and the
    /// parent inode update.
    pub(crate) const CREATE: Self = Self::items(4);
    /// A creation in a directory with a default ACL: `btrfs_new_inode_prepare`
    /// adds one item for the inherited default ACL and one for the access
    /// ACL.
    pub(crate) const CREATE_INHERITING_ACLS: Self = Self::items(Self::CREATE.insert + 2);
    /// `btrfs_symlink`: a creation and its inline extent.
    pub(crate) const SYMLINK: Self = Self::items(5);
    /// `btrfs_link`: the inode and its inode ref, the dir item and dir
    /// index, and the parent inode.
    pub(crate) const LINK: Self = Self::items(5);
    /// `BTRFS_UNLINK_METADATA_UNITS` (`fs/btrfs/fs.h`): the orphan item,
    /// dir item, dir index, inode ref, inode and parent inode.
    pub(crate) const UNLINK: Self = Self {
        frees: true,
        ..Self::items(UNLINK_ITEMS)
    };
    /// `btrfs_dirty_inode`'s one inode update, for times.
    pub(crate) const INODE_UPDATE: Self = Self::items(1);
    /// `btrfs_setxattr_trans` (`fs/btrfs/xattr.c`), which starts its
    /// transaction with 2 items: the xattr item, set or removed, and the
    /// inode update.
    pub(crate) const XATTR: Self = Self::items(2);
    /// A change of owner: the inode update of `btrfs_setattr`, and the
    /// `security.capability` removal `ATTR_KILL_PRIV` makes through
    /// `security_inode_killpriv`, which is a `btrfs_setxattr_trans` of its
    /// own in Linux and is counted as one here.
    pub(crate) const OWNER: Self = Self::items(Self::INODE_UPDATE.insert + Self::XATTR.insert);
    /// A change of mode: the inode update of `btrfs_setattr`, and the access
    /// ACL `posix_acl_chmod` sets again through `btrfs_set_acl`, a
    /// `btrfs_setxattr_trans` of its own in Linux.
    pub(crate) const MODE: Self = Self::items(Self::INODE_UPDATE.insert + Self::XATTR.insert);
    /// `btrfs_truncate`: the truncate slack space and the inode update.
    pub(crate) const TRUNCATE: Self = Self::items(2);
    /// `btrfs_replace_file_extents` with an extent to put in: the inode, the
    /// extents dropped and the extent added.
    pub(crate) const CLONE: Self = Self::items(3);

    /// `btrfs_rename`: the old parent, the inode and its two refs, four dir
    /// entries, the new parent when it differs, and the five items of a
    /// replaced target, which is counted whether or not one exists.
    pub(crate) const fn rename(across_directories: bool) -> Self {
        Self::items(1 + 3 + 4 + across_directories as u64 + 5)
    }
}

impl<S: BlockSink> Transaction<S> {
    fn nodesize(&self) -> u64 {
        u64::from(self.volume.superblock().nodesize())
    }

    /// `btrfs_calc_insert_metadata_size`.
    fn insert_size(&self, items: u64) -> u64 {
        self.nodesize() * MAX_LEVEL * 2 * items
    }

    /// `btrfs_calc_metadata_size`.
    fn update_size(&self, items: u64) -> u64 {
        self.nodesize() * MAX_LEVEL * items
    }

    /// `btrfs_calc_delayed_ref_bytes`: doubled when the free-space tree has
    /// to change as well.
    fn delayed_ref_size(&self, items: u64) -> u64 {
        let bytes = self.insert_size(items);
        if self.accounting.free_root().is_some() {
            bytes * 2
        } else {
            bytes
        }
    }

    /// The units of a data write of `length` bytes, as Linux's
    /// `calc_inode_reservations`: one extent item per `BTRFS_MAX_EXTENT_SIZE`
    /// (`count_max_extents`), the checksum leaves of checksummed data
    /// (`btrfs_csum_bytes_to_leaves`, with `csums_per_leaf` as
    /// `BTRFS_MAX_ITEM_SIZE / csum_size`), and one inode update.
    pub(crate) fn write_units(&self, length: u64, checksummed: bool) -> Units {
        let extents = length.div_ceil(MAX_EXTENT_SIZE);
        let csum_leaves = if checksummed {
            let item_space =
                self.nodesize() - (size_of::<RawHeader>() + size_of::<RawItem>()) as u64;
            let csums_per_leaf = item_space / self.volume.superblock().csum_type().size() as u64;
            let sectors = length.div_ceil(u64::from(self.volume.superblock().sectorsize()));
            sectors.div_ceil(csums_per_leaf)
        } else {
            0
        };
        Units {
            insert: extents + csum_leaves,
            update: 1,
            frees: false,
        }
    }

    /// The refs this transaction's commit still has to run.
    fn outstanding_refs(&self) -> u64 {
        (self.space.metadata_reservations() + self.trees.retired().len()) as u64
    }

    /// What running `refs` at commit may allocate: Linux's delayed-ref size,
    /// which sizes each ref as a path of its own, but never more than the
    /// commit can allocate when it runs them all as one batch. That batch
    /// COWs each committed block of the trees it edits at most once (a
    /// private block is rewritten in place), and grows them by at most the
    /// leaves its new items fill, counted twice for the internal nodes that
    /// split above them.
    fn commit_bytes(&self, refs: u64) -> u64 {
        let nodesize = self.nodesize();
        let superblock = self.volume.superblock();
        let mut trees = [
            Some(objectid::EXTENT_TREE),
            self.accounting.free_root().map(|root| root.tree),
            Some(self.accounting.group_root().tree),
        ];
        if trees[2] == trees[0] {
            trees[2] = None;
        }
        let cow: u64 = trees
            .into_iter()
            .flatten()
            .map(|tree| self.roots.committed_used(tree))
            .sum();
        let mut grown = refs * EXTENT_REF_ITEM_BYTES;
        if self.accounting.free_root().is_some() {
            // Every ref may split a free extent, and every group may turn
            // into bitmaps covering all of it.
            let sectors = superblock.total_bytes() / u64::from(superblock.sectorsize());
            grown += refs * ITEM_HEADER_BYTES
                + sectors.div_ceil(BITS_PER_BITMAP) * (BITMAP_BYTES + ITEM_HEADER_BYTES);
        }
        // A leaf that splits leaves both halves at least half full.
        let half_leaf = (nodesize - size_of::<RawHeader>() as u64) / 2;
        let batch = cow + 2 * grown.div_ceil(half_leaf) * nodesize;
        self.delayed_ref_size(refs).min(batch)
    }

    /// The global reserve a mutation leaves alone: the commit's own trees,
    /// and, unless the mutation frees space, the unlink's fallback, which
    /// Linux's `btrfs_update_global_block_rsv` adds as
    /// `BTRFS_UNLINK_METADATA_UNITS` inserted items and their delayed refs.
    fn global_reserve(&self, units: Units) -> u64 {
        let commit = self.metadata_reserve();
        if units.frees {
            commit
        } else {
            commit + self.insert_size(UNLINK_ITEMS) + self.delayed_ref_size(UNLINK_ITEMS)
        }
    }

    /// The free metadata a mutation of `units` needs before it starts.
    fn needed(&self, units: Units) -> u64 {
        let items = units.insert + units.update;
        self.insert_size(units.insert)
            + self.update_size(units.update)
            + self.commit_bytes(self.outstanding_refs() + items)
            + self.global_reserve(units)
    }

    /// The free metadata the commit needs for the work already applied,
    /// with what a mutation of `units` leaves of the global reserve.
    fn held(&self, units: Units) -> u64 {
        self.commit_bytes(self.outstanding_refs()) + self.global_reserve(units)
    }

    fn free_metadata(&self) -> u64 {
        self.space.totals(METADATA).1
    }

    /// Whether a mutation of `units` fits.
    pub(crate) fn reserves(&self, units: Units) -> bool {
        self.needed(units) <= self.free_metadata()
    }

    /// The refusal of a mutation that does not fit.
    pub(crate) fn exhausted(&self, units: Units) -> WriteError {
        SpaceError::Exhausted {
            bytes: self.needed(units),
            flags: METADATA,
        }
        .into()
    }

    /// Refuses an applied mutation of `units` that left the commit less than
    /// it needs for everything the transaction now holds.
    pub(crate) fn check_commit_hold(&self, units: Units) -> Result<(), WriteError> {
        let held = self.held(units);
        if held > self.free_metadata() {
            return Err(SpaceError::Exhausted {
                bytes: held,
                flags: METADATA,
            }
            .into());
        }
        Ok(())
    }
}
