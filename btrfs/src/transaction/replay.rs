//! Log replay: the state an fsync left in a log tree, applied to the
//! committed trees in one transaction before a writable mount is published.
//!
//! A port of Linux's `btrfs_recover_log_trees` (`fs/btrfs/tree-log.c` as of
//! v7.3), with each piece following the Linux function its comment names. The
//! log root tree holds one root item per subvolume, keyed `(TREE_LOG,
//! ROOT_ITEM, subvolume)`, naming that subvolume's log tree. Replay walks
//! every log tree once per stage:
//!
//! - `LOG_WALK_PIN_ONLY` pins every log tree block, which no extent item
//!   records, so nothing the replay allocates can take its place;
//! - `LOG_WALK_REPLAY_INODES` copies inode items, first deleting the
//!   directory entries and xattrs the log is authoritative for and no
//!   longer holds;
//! - `LOG_WALK_REPLAY_DIR_INDEX` links the directory entries the log holds;
//! - `LOG_WALK_REPLAY_ALL` replays inode references, xattrs and file
//!   extents, then fixes the link count of every inode the replay touched.
//!
//! An item the replay does not implement fails the mount with
//! [`LogReplayError::UnsupportedItem`], and so does any other failure: the
//! transaction is discarded and the device keeps its previous generation.
//!
//! Linux keeps the inodes whose links it must recount as orphan items under
//! `BTRFS_TREE_LOG_FIXUP_OBJECTID` in the subvolume, and leaves the ones it
//! finds with no links to the orphan cleanup that follows the mount. Here the
//! set lives in the replay itself and those inodes are deleted in the same
//! transaction, so the committed generation never records either.

mod extents;
mod names;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use thiserror::Error;

use super::Transaction;
use super::file::aligned_end;
use super::namespace::time;
use crate::items::{ExtentData, FileExtent, Metadata, RecordKind, RootRecord, dir_records};
use crate::ondisk::{
    RawFileExtentBody, RawFileExtentHead, RawInodeItem, RawTimespec, item_type, objectid, view,
};
use crate::source::{BlockSink, BlockSource};
use crate::tree::write::WriteError;
use crate::tree::{Cursor, Expected, Node, NodeSource, TreeRoot};
use crate::volume::Volume;
use crate::{Error, FileKind, ItemDefect, Key, Timestamp};

/// `BTRFS_FEATURE_INCOMPAT_MIXED_GROUPS`, from `btrfs.h`.
const INCOMPAT_MIXED_GROUPS: u64 = 1 << 2;

/// Why a pending log could not be replayed.
#[derive(Debug, Error)]
pub enum LogReplayError {
    /// The log of `subvolume` holds an item whose type replay does not
    /// implement; replaying the rest would leave a partial state.
    #[error(
        "the log of subvolume {subvolume} holds item {key}, a type log replay does not implement"
    )]
    UnsupportedItem { subvolume: u64, key: Key },
    /// The log root tree holds an item that is not a subvolume's log root.
    #[error("the log root tree holds item {key}, which is no subvolume's log root")]
    UnsupportedRoot { key: Key },
    /// The log root tree names no log tree, which no fsync writes.
    #[error("the log root tree names no log tree")]
    Empty,
    /// A directory entry the replay would have to add or remove links a
    /// subvolume, which log replay does not implement.
    #[error("directory entry {key} in subvolume {subvolume} links a subvolume")]
    SubvolumeEntry { subvolume: u64, key: Key },
    /// An inline extent overlaps the range a logged extent replaces only in
    /// part, which Linux's `btrfs_drop_extents` refuses as well.
    #[error(
        "inline extent of inode {inode} in subvolume {subvolume} overlaps {start:#x}..{end:#x} only in part"
    )]
    InlineOverlap {
        subvolume: u64,
        inode: u64,
        start: u64,
        end: u64,
    },
}

/// The log a crash left behind the committed superblock: its log root tree.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingLog {
    pub(crate) bytenr: u64,
    pub(crate) level: u8,
}

/// The stages after pinning, in the order Linux runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// `LOG_WALK_REPLAY_INODES`.
    Inodes,
    /// `LOG_WALK_REPLAY_DIR_INDEX`.
    DirIndex,
    /// `LOG_WALK_REPLAY_ALL`.
    All,
}

/// How replay's unlink changes the link count of the inode it unlinks.
/// Linux raises the count before some unlinks (`inc_nlink`) so that the one
/// `btrfs_unlink_inode` drops leaves it where it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Links {
    Kept,
    Dropped,
}

/// What the replay carries from item to item, log to log and stage to stage:
/// Linux's `walk_control`.
struct Walk {
    stage: Stage,
    /// The subvolume being replayed into, and its log tree.
    tree: u64,
    log: TreeRoot,
    /// The generation every log block carries: the transaction after the
    /// committed one, whose fsync wrote the log.
    newest: u64,
    /// The items of an inode the log records with no links are skipped
    /// (`ignore_cur_inode`), until the next inode item.
    ignore: bool,
    /// What the replay stamps on the inodes and directories it unlinks
    /// from: Linux's current time.
    when: RawTimespec,
    /// Per subvolume, the inodes whose link counts the replay recounts at
    /// its end (`link_to_fixup_dir`).
    fixup: BTreeMap<u64, BTreeSet<u64>>,
}

/// Reads log tree blocks, which are one generation newer than the
/// superblock.
pub(super) struct LogNodes<'a, S> {
    volume: &'a Volume<S>,
    newest: u64,
}

impl<S: BlockSource> NodeSource for LogNodes<'_, S> {
    async fn read_node(&self, logical: u64, expected: Expected) -> Result<Node, Error> {
        Node::read_until(self.volume, logical, expected, self.newest).await
    }
}

/// A defect of item `key` of a log tree.
fn log_defect(key: Key) -> impl Fn(ItemDefect) -> Error {
    move |defect| Error::Item {
        tree: objectid::TREE_LOG,
        key,
        defect,
    }
}

/// The inode item a log or subvolume item holds, validated.
fn inode_record(tree: u64, key: Key, bytes: &[u8]) -> Result<(RawInodeItem, Metadata), Error> {
    let metadata = Metadata::parse(bytes).map_err(|defect| Error::Item { tree, key, defect })?;
    Ok((
        *view::<RawInodeItem>(bytes).expect("validated inode"),
        metadata,
    ))
}

impl<S: BlockSink> Transaction<S> {
    fn log_nodes(&self, newest: u64) -> LogNodes<'_, S> {
        LogNodes {
            volume: &self.volume,
            newest,
        }
    }

    /// Replays `log` into this transaction, stamping `when` on what its
    /// unlinks change: `btrfs_recover_log_trees`. The caller commits.
    pub(crate) async fn replay_log(
        &mut self,
        log: PendingLog,
        when: Timestamp,
    ) -> Result<(), WriteError> {
        let newest = self
            .volume
            .superblock()
            .generation()
            .checked_add(1)
            .ok_or(WriteError::GenerationOverflow)?;
        // `btrfs_replay_log` reads the log root tree's root block at the
        // generation after the superblock's.
        let root_tree = TreeRoot {
            tree: objectid::TREE_LOG,
            bytenr: log.bytenr,
            level: log.level,
            generation: newest,
        };
        let logs = self.log_trees(root_tree, newest).await?;

        // LOG_WALK_PIN_ONLY: the log root tree, then each log tree. A log
        // whose subvolume is gone has only its root block pinned.
        self.pin_log(root_tree, newest).await?;
        let nodesize = u64::from(self.volume.superblock().nodesize());
        for &(subvolume, tree) in &logs {
            if self.roots.is_live(subvolume) {
                self.pin_log(tree, newest).await?;
            } else {
                self.space.pin(tree.bytenr..tree.bytenr + nodesize);
            }
        }
        let live: Vec<(u64, TreeRoot)> = logs
            .into_iter()
            .filter(|(subvolume, _)| self.roots.is_live(*subvolume))
            .collect();
        if live.is_empty() {
            // The commit clears the log root only by publishing a new
            // generation, which needs a changed root tree.
            self.prepare_mutation().await?;
            self.roots.record_in_transaction(objectid::FS_TREE)?;
        }
        let mut walk = Walk {
            stage: Stage::Inodes,
            tree: objectid::FS_TREE,
            log: root_tree,
            newest,
            ignore: false,
            when: time(objectid::FS_TREE, objectid::FIRST_FREE, when)?,
            fixup: BTreeMap::new(),
        };
        for stage in [Stage::Inodes, Stage::DirIndex, Stage::All] {
            walk.stage = stage;
            for &(tree, log) in &live {
                walk.tree = tree;
                walk.log = log;
                self.roots.record_in_transaction(tree)?;
                self.walk_log(&mut walk).await?;
                if stage == Stage::All {
                    self.fixup_link_counts(&mut walk).await?;
                }
            }
        }
        Ok(())
    }

    /// The log trees the log root tree names, highest subvolume first, the
    /// order `btrfs_recover_log_trees` searches them in.
    async fn log_trees(
        &self,
        root_tree: TreeRoot,
        newest: u64,
    ) -> Result<Vec<(u64, TreeRoot)>, WriteError> {
        let nodes = self.log_nodes(newest);
        let sector = self.volume.superblock().sectorsize();
        let mut cursor = Cursor::ceiling(&nodes, root_tree, Key::new(0, 0, 0)).await?;
        let mut logs = Vec::new();
        while let (Some(key), Some(item)) = (cursor.key(), cursor.item()) {
            if !key.is(objectid::TREE_LOG, item_type::ROOT_ITEM) {
                return Err(LogReplayError::UnsupportedRoot { key }.into());
            }
            let record = RootRecord::parse(key, item, sector).map_err(log_defect(key))?;
            logs.push((key.offset, record.root));
            cursor.advance(&nodes).await?;
        }
        if logs.is_empty() {
            return Err(LogReplayError::Empty.into());
        }
        logs.reverse();
        Ok(logs)
    }

    /// Pins every block of the tree at `root` against reuse, and on a
    /// filesystem with mixed block groups every data extent its leaves
    /// reference: `process_one_buffer` with `btrfs_pin_extent_for_log_replay`
    /// and `btrfs_exclude_logged_extents`. Each block is visited once, however
    /// many parents point at it.
    async fn pin_log(&mut self, root: TreeRoot, newest: u64) -> Result<(), WriteError> {
        let nodesize = u64::from(self.volume.superblock().nodesize());
        let sector = self.volume.superblock().sectorsize();
        let mixed = self.volume.superblock().incompat_flags() & INCOMPAT_MIXED_GROUPS != 0;
        let mut visited = BTreeSet::new();
        let mut pending = vec![(
            root.bytenr,
            Expected {
                level: root.level,
                generation: root.generation,
                first_key: None,
            },
        )];
        while let Some((address, expected)) = pending.pop() {
            if !visited.insert(address) {
                continue;
            }
            self.space.pin(address..address + nodesize);
            // Linux reads a leaf here only to exclude its logged extents.
            if expected.level == 0 && !mixed {
                continue;
            }
            let node = self.log_nodes(newest).read_node(address, expected).await?;
            if node.level() > 0 {
                for slot in 0..node.nritems() {
                    let (child, generation) = node.child(slot);
                    pending.push((
                        child,
                        Expected {
                            level: node.level() - 1,
                            generation,
                            first_key: Some(node.key(slot)),
                        },
                    ));
                }
                continue;
            }
            for slot in 0..node.nritems() {
                let key = node.key(slot);
                if key.item_type != item_type::EXTENT_DATA {
                    continue;
                }
                let bytes = node.item(slot);
                let extent = FileExtent::parse(key, bytes, sector).map_err(log_defect(key))?;
                if matches!(extent.data, ExtentData::Inline { .. }) {
                    continue;
                }
                let body = view::<RawFileExtentBody>(&bytes[size_of::<RawFileExtentHead>()..])
                    .expect("validated extent body");
                if body.disk_bytenr.get() != 0 {
                    let start = body.disk_bytenr.get();
                    self.space.pin(start..start + body.disk_num_bytes.get());
                }
            }
        }
        Ok(())
    }

    /// Replays every item of `walk.log` for `walk.stage`, in key order:
    /// `walk_log_tree` with `replay_one_buffer` on each leaf.
    async fn walk_log(&mut self, walk: &mut Walk) -> Result<(), WriteError> {
        let mut cursor =
            Cursor::ceiling(&self.log_nodes(walk.newest), walk.log, Key::new(0, 0, 0)).await?;
        while let (Some(key), Some(item)) = (cursor.key(), cursor.item()) {
            let item = item.to_vec();
            self.replay_item(walk, key, &item).await?;
            cursor.advance(&self.log_nodes(walk.newest)).await?;
        }
        Ok(())
    }

    /// One log item, for the stage `walk` is in: the body of
    /// `replay_one_buffer`.
    async fn replay_item(
        &mut self,
        walk: &mut Walk,
        key: Key,
        item: &[u8],
    ) -> Result<(), WriteError> {
        match key.item_type {
            item_type::INODE_ITEM
            | item_type::INODE_REF
            | item_type::INODE_EXTREF
            | item_type::XATTR_ITEM
            | item_type::DIR_INDEX
            | item_type::DIR_LOG_INDEX
            | item_type::EXTENT_DATA => {}
            // Linux derives directory items from directory indices and
            // reads only index ranges: a log from an older kernel that
            // holds either kind has them ignored, as here.
            item_type::DIR_ITEM | item_type::DIR_LOG_ITEM => {}
            // Read by the extent replay that needs them.
            item_type::EXTENT_CSUM if key.objectid == objectid::EXTENT_CSUM => {}
            _ => {
                return Err(LogReplayError::UnsupportedItem {
                    subvolume: walk.tree,
                    key,
                }
                .into());
            }
        }
        // `btrfs_start_transaction(tree_root, 0)` reserves nothing up front,
        // and Linux's allocator adds a chunk when the replay's blocks need
        // one. Each item gets the metadata growth every mutation gets.
        self.prepare_mutation().await?;
        if key.item_type == item_type::INODE_ITEM {
            let (_, metadata) = inode_record(objectid::TREE_LOG, key, item)?;
            // A tmpfile, or an inode whose last link went before an fsync
            // through an open descriptor: replaying it is pointless, and the
            // directory replay deletes it.
            if metadata.nlink == 0 {
                walk.ignore = true;
                return Ok(());
            }
            walk.ignore = false;
            if walk.stage == Stage::Inodes {
                self.replay_inode(walk, key, item, metadata.kind).await?;
            }
        }
        if walk.ignore {
            return Ok(());
        }
        if key.item_type == item_type::DIR_INDEX && walk.stage == Stage::DirIndex {
            self.replay_dir_index(walk, key, item).await?;
        }
        if walk.stage != Stage::All {
            return Ok(());
        }
        match key.item_type {
            item_type::XATTR_ITEM => {
                for record in dir_records(item, RecordKind::Xattr) {
                    record.map_err(log_defect(key))?;
                }
                self.overwrite_item(walk, key, item).await
            }
            item_type::INODE_REF | item_type::INODE_EXTREF => {
                self.add_inode_ref(walk, key, item).await
            }
            item_type::EXTENT_DATA => self.replay_extent(walk, key, item).await,
            _ => Ok(()),
        }
    }

    /// The `LOG_WALK_REPLAY_INODES` handling of an inode item.
    async fn replay_inode(
        &mut self,
        walk: &mut Walk,
        key: Key,
        item: &[u8],
        kind: FileKind,
    ) -> Result<(), WriteError> {
        let inode = key.objectid;
        self.replay_xattr_deletes(walk, inode).await?;
        if kind == FileKind::Directory {
            self.replay_dir_deletes(walk, inode, Some(walk.log)).await?;
        }
        self.overwrite_item(walk, key, item).await?;
        // Prealloc extents can lie beyond i_size before an fsync. Drop them
        // now rather than after the extents replay, which would drop the
        // ones the log holds as well.
        if kind == FileKind::Regular {
            let sector = u64::from(self.volume.superblock().sectorsize());
            let (mut raw, _) = self.inode_item(walk.tree, inode).await?;
            let from = aligned_end(raw.size.get(), sector)?;
            let found = self
                .drop_extents(walk, inode, from..u64::MAX - u64::MAX % sector)
                .await?;
            raw.nbytes.set(
                raw.nbytes
                    .get()
                    .checked_sub(found)
                    .ok_or(WriteError::FileOverflow)?,
            );
            self.update_inode(walk.tree, inode, raw).await?;
        }
        self.link_to_fixup(walk, inode).await
    }

    /// Writes inode `inode` as `btrfs_update_inode` does: in the running
    /// transaction.
    async fn update_inode(
        &mut self,
        tree: u64,
        inode: u64,
        mut raw: RawInodeItem,
    ) -> Result<(), WriteError> {
        raw.transid.set(self.trees.generation());
        self.put_inode(tree, inode, raw).await
    }

    /// Whether the subvolume holds inode `inode`: `btrfs_iget_logging`
    /// failing with `-ENOENT` or not.
    async fn inode_exists(&self, tree: u64, inode: u64) -> Result<bool, WriteError> {
        match self.inode_item(tree, inode).await {
            Ok(_) => Ok(true),
            Err(WriteError::Read(Error::MissingInode { .. })) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The item at `key` in the log being replayed.
    async fn log_item(&self, walk: &Walk, key: Key) -> Result<Option<Vec<u8>>, WriteError> {
        let cursor = Cursor::floor(&self.log_nodes(walk.newest), walk.log, key).await?;
        if cursor.key() != Some(key) {
            return Ok(None);
        }
        Ok(cursor.item().map(<[u8]>::to_vec))
    }

    /// Copies a log item into the subvolume: `overwrite_item`. An inode
    /// item keeps the subvolume's byte count, which the extent replay then
    /// adjusts, and a directory its size, which the entry replay adjusts;
    /// one logged with generation zero only records that the inode exists.
    async fn overwrite_item(
        &mut self,
        walk: &Walk,
        key: Key,
        item: &[u8],
    ) -> Result<(), WriteError> {
        let tree = walk.tree;
        let root = self.roots.get(tree)?;
        let existing = self.trees.get(&self.volume, root, key).await?;
        let is_inode = key.item_type == item_type::INODE_ITEM;
        if !is_inode {
            if existing.as_deref() == Some(item) {
                return Ok(());
            }
            return self.put_item(tree, key, Some(&item.to_vec())).await;
        }
        let (mut source, _) = inode_record(objectid::TREE_LOG, key, item)?;
        let directory = |raw: &RawInodeItem| raw.mode.get() & 0o170_000 == 0o040_000;
        let regular = |raw: &RawInodeItem| raw.mode.get() & 0o170_000 == 0o100_000;
        let Some(existing) = existing else {
            // A new inode: its bytes and a directory's size come from what
            // the replay adds.
            source.nbytes.set(0);
            if directory(&source) {
                source.size.set(0);
            }
            if source.generation.get() == 0 {
                source.generation.set(self.trees.generation());
            }
            return self.put_inode(tree, key.objectid, source).await;
        };
        if existing.as_slice() == item {
            return Ok(());
        }
        let (mut current, _) = inode_record(tree, key, &existing)?;
        if existing.len() == item.len() {
            source.nbytes = current.nbytes;
            if directory(&source) {
                source.size.set(0);
            }
        }
        if source.generation.get() == 0 {
            if regular(&source) && regular(&current) && source.size.get() != 0 {
                current.size = source.size;
                return self.put_inode(tree, key.objectid, current).await;
            }
            return Ok(());
        }
        if directory(&source) && directory(&current) {
            source.size = current.size;
        }
        self.put_inode(tree, key.objectid, source).await
    }
}
