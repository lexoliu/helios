//! The namespace half of log replay: directory entries, inode references,
//! xattr and directory deletions, and the link-count fixup, each after the
//! `fs/btrfs/tree-log.c` (v7.3) function its comment names.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::ops::Range;

use super::{Links, LogReplayError, Walk, log_defect};
use crate::csum::{inode_ref_hash, name_hash};
use crate::items::{DirRecord, RecordKind, dir_index_record, dir_records};
use crate::ondisk::{RawDirItem, item_type, objectid};
use crate::source::BlockSink;
use crate::transaction::Transaction;
use crate::transaction::namespace::{Reference, entry_record, references};
use crate::tree::write::WriteError;
use crate::tree::{Cursor, TreeRoot};
use crate::{Error, FileKind, ItemDefect, Key};

/// `BTRFS_FT_DIR`, the directory entry type of a directory.
const ENTRY_DIRECTORY: u8 = 2;

/// Whether `bytes`, an item of `tree` at `key` holding directory or xattr
/// records, holds one named `name`: the record's byte range and the record.
fn named<'a>(
    tree: u64,
    key: Key,
    bytes: &'a [u8],
    kind: RecordKind,
    name: &[u8],
) -> Result<Option<(Range<usize>, DirRecord<'a>)>, Error> {
    let mut position = 0;
    for record in dir_records(bytes, kind) {
        let record = record.map_err(|defect| Error::Item { tree, key, defect })?;
        let end = position + size_of::<RawDirItem>() + record.name.len() + record.data.len();
        if record.name == name {
            return Ok(Some((position..end, record)));
        }
        position = end;
    }
    Ok(None)
}

/// A name of an inode reference, owned.
struct Name {
    parent: u64,
    index: u64,
    bytes: Vec<u8>,
}

impl From<Reference<'_>> for Name {
    fn from(reference: Reference<'_>) -> Self {
        Self {
            parent: reference.parent,
            index: reference.index,
            bytes: reference.name.to_vec(),
        }
    }
}

/// Every name an inode reference item of `tree` at `key` holds.
fn names(tree: u64, key: Key, bytes: &[u8]) -> Result<Vec<Name>, Error> {
    references(tree, key, bytes)
        .map(|record| record.map(|(_, reference)| reference.into()))
        .collect()
}

impl<S: BlockSink> Transaction<S> {
    /// Records `inode` for the link-count fixup, raising its count once so
    /// that the unlinks before the fixup cannot take it to zero:
    /// `link_to_fixup_dir`.
    pub(super) async fn link_to_fixup(
        &mut self,
        walk: &mut Walk,
        inode: u64,
    ) -> Result<(), WriteError> {
        let tree = walk.tree;
        let (mut raw, _) = self.inode_item(tree, inode).await?;
        if walk.fixup.entry(tree).or_default().insert(inode) {
            let nlink = match raw.nlink.get() {
                0 => 1,
                nlink => nlink
                    .checked_add(1)
                    .ok_or(WriteError::LinkLimit { tree, inode })?,
            };
            raw.nlink.set(nlink);
            self.update_inode(tree, inode, raw).await?;
        }
        Ok(())
    }

    /// Removes the entry `name` of directory `dir`, which names `inode`:
    /// `unlink_inode_for_log_replay`, which is `btrfs_unlink_inode`. The
    /// directory item, the inode reference and the directory index go, the
    /// directory shrinks, and both inodes are stamped with the replay's time.
    async fn unlink_for_replay(
        &mut self,
        walk: &Walk,
        dir: u64,
        inode: u64,
        name: &[u8],
        links: Links,
    ) -> Result<(), WriteError> {
        let tree = walk.tree;
        let root = self.roots.get(tree)?;
        let no_entry = WriteError::NoEntry { tree, parent: dir };
        let hash_key = Key::new(dir, item_type::DIR_ITEM, name_hash(name));
        let mut entries = self
            .trees
            .get(&self.volume, root, hash_key)
            .await?
            .ok_or(no_entry)?;
        let (range, _) = named(tree, hash_key, &entries, RecordKind::Entry, name)?
            .ok_or(WriteError::NoEntry { tree, parent: dir })?;
        entries.drain(range);
        self.put_item(tree, hash_key, (!entries.is_empty()).then_some(&entries))
            .await?;
        let (reference_key, index, rest) = self.without_reference(tree, dir, inode, name).await?;
        self.put_item(tree, reference_key, (!rest.is_empty()).then_some(&rest))
            .await?;
        let index_key = Key::new(dir, item_type::DIR_INDEX, index);
        let root = self.roots.get(tree)?;
        if self
            .trees
            .get(&self.volume, root, index_key)
            .await?
            .is_none()
        {
            return Err(WriteError::NamespaceMismatch {
                tree,
                key: index_key,
            });
        }
        self.put_item(tree, index_key, None).await?;

        let (mut directory, _) = self.inode_item(tree, dir).await?;
        directory.size.set(
            directory
                .size
                .get()
                .checked_sub(2 * name.len() as u64)
                .ok_or(WriteError::FileOverflow)?,
        );
        directory
            .sequence
            .set(directory.sequence.get().wrapping_add(1));
        directory.mtime = walk.when;
        directory.ctime = walk.when;
        self.update_inode(tree, dir, directory).await?;

        let (mut raw, _) = self.inode_item(tree, inode).await?;
        raw.sequence.set(raw.sequence.get().wrapping_add(1));
        raw.ctime = walk.when;
        if links == Links::Dropped {
            raw.nlink.set(
                raw.nlink
                    .get()
                    .checked_sub(1)
                    .ok_or(WriteError::NamespaceMismatch {
                        tree,
                        key: reference_key,
                    })?,
            );
        }
        self.update_inode(tree, inode, raw).await
    }

    /// Adds the entry `name` of directory `dir` at directory index `index`,
    /// naming `inode`, with its inode reference when `backref` is set:
    /// `btrfs_add_link` while a log is recovering, which leaves the link
    /// count and the directory's times to the replay. Returns false, having
    /// changed nothing, when the directory already holds the name
    /// (`-EEXIST`).
    async fn link_for_replay(
        &mut self,
        tree: u64,
        dir: u64,
        inode: u64,
        name: &[u8],
        index: u64,
        backref: bool,
    ) -> Result<bool, WriteError> {
        if inode == objectid::FIRST_FREE {
            // `btrfs_add_link` links a subvolume root through a root ref.
            return Err(LogReplayError::SubvolumeEntry {
                subvolume: tree,
                key: Key::new(dir, item_type::DIR_INDEX, index),
            }
            .into());
        }
        let (_, metadata) = self.inode_item(tree, inode).await?;
        let root = self.roots.get(tree)?;
        let hash_key = Key::new(dir, item_type::DIR_ITEM, name_hash(name));
        let mut entries = self
            .trees
            .get(&self.volume, root, hash_key)
            .await?
            .unwrap_or_default();
        if named(tree, hash_key, &entries, RecordKind::Entry, name)?.is_some() {
            return Ok(false);
        }
        let index_key = Key::new(dir, item_type::DIR_INDEX, index);
        if self
            .trees
            .get(&self.volume, root, index_key)
            .await?
            .is_some()
        {
            return Err(WriteError::NamespaceMismatch {
                tree,
                key: index_key,
            });
        }
        if backref {
            let (key, bytes) = self.new_reference(tree, dir, inode, index, name).await?;
            self.put_item(tree, key, Some(&bytes)).await?;
        }
        let record = entry_record(inode, metadata.kind, self.trees.generation(), name);
        entries.extend_from_slice(&record);
        self.put_item(tree, hash_key, Some(&entries)).await?;
        self.put_item(tree, index_key, Some(&record)).await?;
        let (mut directory, _) = self.inode_item(tree, dir).await?;
        directory.size.set(
            directory
                .size
                .get()
                .checked_add(2 * name.len() as u64)
                .ok_or(WriteError::FileOverflow)?,
        );
        directory
            .sequence
            .set(directory.sequence.get().wrapping_add(1));
        self.update_inode(tree, dir, directory).await?;
        Ok(true)
    }

    /// The location and entry type of the entry `name` of directory `dir`:
    /// `btrfs_lookup_dir_item`.
    async fn entry_named(
        &self,
        tree: u64,
        dir: u64,
        name: &[u8],
    ) -> Result<Option<(Key, u8)>, WriteError> {
        let root = self.roots.get(tree)?;
        let key = Key::new(dir, item_type::DIR_ITEM, name_hash(name));
        let Some(bytes) = self.trees.get(&self.volume, root, key).await? else {
            return Ok(None);
        };
        Ok(named(tree, key, &bytes, RecordKind::Entry, name)?
            .map(|(_, record)| (record.location, record.entry_type)))
    }

    /// The location and entry type of directory index `index` of `dir` when
    /// it holds `name`: `btrfs_lookup_dir_index_item`.
    async fn index_named(
        &self,
        tree: u64,
        dir: u64,
        index: u64,
        name: &[u8],
    ) -> Result<Option<(Key, u8)>, WriteError> {
        let root = self.roots.get(tree)?;
        let key = Key::new(dir, item_type::DIR_INDEX, index);
        let Some(bytes) = self.trees.get(&self.volume, root, key).await? else {
            return Ok(None);
        };
        let record =
            dir_index_record(&bytes).map_err(|defect| Error::Item { tree, key, defect })?;
        Ok((record.name == name).then_some((record.location, record.entry_type)))
    }

    /// Whether `name` at index `index` of `dir` already names `inode`:
    /// `inode_in_dir`.
    async fn inode_in_dir(
        &self,
        tree: u64,
        dir: u64,
        inode: u64,
        index: u64,
        name: &[u8],
    ) -> Result<bool, WriteError> {
        let Some((location, _)) = self.index_named(tree, dir, index, name).await? else {
            return Ok(false);
        };
        if location.objectid != inode {
            return Ok(false);
        }
        Ok(self
            .entry_named(tree, dir, name)
            .await?
            .is_some_and(|(location, _)| location.objectid == inode))
    }

    /// Whether the log's inode reference item at `key` holds `name`, under
    /// `parent` for an extended one: `backref_in_log`.
    async fn backref_in_log(
        &self,
        walk: &Walk,
        key: Key,
        parent: u64,
        name: &[u8],
    ) -> Result<bool, WriteError> {
        let Some(bytes) = self.log_item(walk, key).await? else {
            return Ok(false);
        };
        let extended = key.item_type == item_type::INODE_EXTREF;
        for record in references(objectid::TREE_LOG, key, &bytes) {
            let (_, record) = record?;
            if record.name == name && (!extended || record.parent == parent) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Unlinks the entry `name` of `dir` that names `location`, recording
    /// that inode for the fixup: `drop_one_dir_item`.
    async fn drop_one_dir_item(
        &mut self,
        walk: &mut Walk,
        dir: u64,
        location: Key,
        name: &[u8],
    ) -> Result<(), WriteError> {
        if location.item_type == item_type::ROOT_ITEM {
            return Err(LogReplayError::SubvolumeEntry {
                subvolume: walk.tree,
                key: Key::new(dir, item_type::DIR_ITEM, name_hash(name)),
            }
            .into());
        }
        let inode = location.objectid;
        self.link_to_fixup(walk, inode).await?;
        self.unlink_for_replay(walk, dir, inode, name, Links::Dropped)
            .await
    }

    /// Links one directory index entry of the log, unlinking what the
    /// subvolume holds in its place: `replay_one_dir_item` and
    /// `replay_one_name`. An entry for an inode the subvolume does not hold
    /// is skipped, and so is one the log's inode references add later.
    pub(super) async fn replay_dir_index(
        &mut self,
        walk: &mut Walk,
        key: Key,
        item: &[u8],
    ) -> Result<(), WriteError> {
        let tree = walk.tree;
        let dir = key.objectid;
        let record = dir_index_record(item).map_err(log_defect(key))?;
        if record.location.item_type == item_type::ROOT_ITEM {
            return Err(LogReplayError::SubvolumeEntry {
                subvolume: tree,
                key,
            }
            .into());
        }
        let (location, entry_type, name) = (record.location, record.entry_type, record.name);
        self.inode_item(tree, dir).await?;
        let exists = self.inode_exists(tree, location.objectid).await?;

        let mut entry_matches = false;
        if let Some((found, found_type)) = self.entry_named(tree, dir, name).await? {
            if found == location && found_type == entry_type {
                entry_matches = true;
            } else if exists {
                self.drop_one_dir_item(walk, dir, found, name).await?;
            }
        }
        let mut index_matches = false;
        if let Some((found, found_type)) = self.index_named(tree, dir, key.offset, name).await? {
            if found == location && found_type == entry_type {
                index_matches = true;
            } else if exists {
                self.drop_one_dir_item(walk, dir, found, name).await?;
            }
        }
        if entry_matches && index_matches {
            return Ok(());
        }
        // The inode's reference in the log adds the entry later.
        if self
            .backref_in_log(
                walk,
                Key::new(location.objectid, item_type::INODE_REF, dir),
                dir,
                name,
            )
            .await?
            || self
                .backref_in_log(
                    walk,
                    Key::new(
                        location.objectid,
                        item_type::INODE_EXTREF,
                        inode_ref_hash(dir, name),
                    ),
                    dir,
                    name,
                )
                .await?
        {
            return Ok(());
        }
        // `insert_one_name`: an entry for a missing inode, or a name the
        // directory already holds, is not added.
        let added = exists
            && self
                .link_for_replay(tree, dir, location.objectid, name, key.offset, true)
                .await?;
        // A non-directory may have gained a link the uncommitted
        // transaction added; recount it.
        if added && entry_type != ENTRY_DIRECTORY {
            self.link_to_fixup(walk, location.objectid).await?;
        }
        Ok(())
    }

    /// Unlinks every entry of `dir` in a range of directory indices the log
    /// is authoritative for that the log does not hold, or every entry when
    /// `log` is `None`: `replay_dir_deletes`.
    pub(super) async fn replay_dir_deletes(
        &mut self,
        walk: &mut Walk,
        dir: u64,
        log: Option<TreeRoot>,
    ) -> Result<(), WriteError> {
        let tree = walk.tree;
        // The directory may first appear in this log.
        if !self.inode_exists(tree, dir).await? {
            return Ok(());
        }
        let mut start = 0;
        loop {
            let end = match log {
                None => u64::MAX,
                Some(log) => match self.find_dir_range(walk, log, dir, &mut start).await? {
                    Some(end) => end,
                    None => break,
                },
            };
            let mut offset = start;
            loop {
                let root = self.roots.get(tree)?;
                let Some((found, item)) = self
                    .trees
                    .ceiling(
                        &self.volume,
                        root,
                        Key::new(dir, item_type::DIR_INDEX, offset),
                    )
                    .await?
                else {
                    break;
                };
                if !found.is(dir, item_type::DIR_INDEX) {
                    return Ok(());
                }
                if found.offset > end {
                    break;
                }
                self.check_item_in_log(walk, log, dir, found, &item).await?;
                if found.offset == u64::MAX {
                    break;
                }
                offset = found.offset + 1;
            }
            if end == u64::MAX {
                break;
            }
            start = end + 1;
        }
        Ok(())
    }

    /// The range of directory indices of `dir` the log is authoritative for
    /// that holds `start`, or else the next one: `find_dir_range`. `start`
    /// moves to the range's first index; its last is returned.
    async fn find_dir_range(
        &self,
        walk: &Walk,
        log: TreeRoot,
        dir: u64,
        start: &mut u64,
    ) -> Result<Option<u64>, WriteError> {
        if *start == u64::MAX {
            return Ok(None);
        }
        let target = Key::new(dir, item_type::DIR_LOG_INDEX, *start);
        let nodes = self.log_nodes(walk.newest);
        let mut cursor = Cursor::floor(&nodes, log, target).await?;
        let (Some(key), Some(item)) = (cursor.key(), cursor.item()) else {
            return Ok(None);
        };
        if key > target {
            return Ok(None);
        }
        if key.is(dir, item_type::DIR_LOG_INDEX) {
            let end = range_end(key, item)?;
            if *start >= key.offset && *start <= end {
                *start = key.offset;
                return Ok(Some(end));
            }
        }
        cursor.advance(&nodes).await?;
        let (Some(key), Some(item)) = (cursor.key(), cursor.item()) else {
            return Ok(None);
        };
        if !key.is(dir, item_type::DIR_LOG_INDEX) {
            return Ok(None);
        }
        let end = range_end(key, item)?;
        *start = key.offset;
        Ok(Some(end))
    }

    /// Unlinks the subvolume's directory index entry `dir_key` of `dir`
    /// unless the log holds it: `check_item_in_log`.
    async fn check_item_in_log(
        &mut self,
        walk: &mut Walk,
        log: Option<TreeRoot>,
        dir: u64,
        dir_key: Key,
        item: &[u8],
    ) -> Result<(), WriteError> {
        let tree = walk.tree;
        let record = dir_index_record(item).map_err(|defect| Error::Item {
            tree,
            key: dir_key,
            defect,
        })?;
        if log.is_some()
            && let Some(logged) = self.log_item(walk, dir_key).await?
            && dir_index_record(&logged).map_err(log_defect(dir_key))?.name == record.name
        {
            return Ok(());
        }
        if record.location.item_type == item_type::ROOT_ITEM {
            return Err(LogReplayError::SubvolumeEntry {
                subvolume: tree,
                key: dir_key,
            }
            .into());
        }
        let (inode, name) = (record.location.objectid, record.name.to_vec());
        self.link_to_fixup(walk, inode).await?;
        self.unlink_for_replay(walk, dir, inode, &name, Links::Kept)
            .await
    }

    /// Deletes every xattr of `inode` the log does not hold:
    /// `replay_xattr_deletes`.
    pub(super) async fn replay_xattr_deletes(
        &mut self,
        walk: &Walk,
        inode: u64,
    ) -> Result<(), WriteError> {
        let tree = walk.tree;
        let mut search = Key::new(inode, item_type::XATTR_ITEM, 0);
        'again: loop {
            let root = self.roots.get(tree)?;
            loop {
                let Some((key, mut bytes)) = self.trees.ceiling(&self.volume, root, search).await?
                else {
                    return Ok(());
                };
                if !key.is(inode, item_type::XATTR_ITEM) {
                    return Ok(());
                }
                let mut position = 0;
                let mut missing = None;
                for record in dir_records(&bytes, RecordKind::Xattr) {
                    let record = record.map_err(|defect| Error::Item { tree, key, defect })?;
                    let end =
                        position + size_of::<RawDirItem>() + record.name.len() + record.data.len();
                    if !self.xattr_in_log(walk, inode, record.name).await? {
                        missing = Some(position..end);
                        break;
                    }
                    position = end;
                }
                if let Some(range) = missing {
                    bytes.drain(range);
                    self.put_item(tree, key, (!bytes.is_empty()).then_some(&bytes))
                        .await?;
                    search = key;
                    continue 'again;
                }
                match key.offset.checked_add(1) {
                    Some(next) => search = Key::new(inode, item_type::XATTR_ITEM, next),
                    None => return Ok(()),
                }
            }
        }
    }

    /// Whether the log holds xattr `name` of `inode`: `btrfs_lookup_xattr`.
    async fn xattr_in_log(&self, walk: &Walk, inode: u64, name: &[u8]) -> Result<bool, WriteError> {
        let key = Key::new(inode, item_type::XATTR_ITEM, name_hash(name));
        let Some(bytes) = self.log_item(walk, key).await? else {
            return Ok(false);
        };
        Ok(named(objectid::TREE_LOG, key, &bytes, RecordKind::Xattr, name)?.is_some())
    }

    /// Replays one inode reference item of the log: `add_inode_ref`. Each
    /// name the directory does not hold yet is linked after the names that
    /// conflict with it are unlinked; then the names the subvolume's item
    /// holds and the log's does not are unlinked, and the log's item
    /// replaces it.
    pub(super) async fn add_inode_ref(
        &mut self,
        walk: &mut Walk,
        key: Key,
        item: &[u8],
    ) -> Result<(), WriteError> {
        let tree = walk.tree;
        let inode = key.objectid;
        let extended = key.item_type == item_type::INODE_EXTREF;
        let logged = names(objectid::TREE_LOG, key, item)?;
        let Some(first) = logged.first() else {
            return Err(log_defect(key)(ItemDefect::Size {
                needed: 1,
                found: 0,
            })
            .into());
        };
        // Not every parent of an inode need be in the log; the fixup counts
        // what remains.
        if !self.inode_exists(tree, first.parent).await? {
            return Ok(());
        }
        self.inode_item(tree, inode).await?;
        let mut dir = Some(first.parent);
        for name in &logged {
            let parent = match dir {
                Some(parent) => parent,
                None if self.inode_exists(tree, name.parent).await? => name.parent,
                None => continue,
            };
            if !self
                .inode_in_dir(tree, parent, inode, name.index, &name.bytes)
                .await?
            {
                if !self
                    .resolve_ref_conflicts(walk, parent, inode, name.index, &name.bytes)
                    .await?
                {
                    return Ok(());
                }
                if !self
                    .link_for_replay(tree, parent, inode, &name.bytes, name.index, false)
                    .await?
                {
                    return Err(WriteError::Exists { tree, parent });
                }
                let (raw, _) = self.inode_item(tree, inode).await?;
                self.update_inode(tree, inode, raw).await?;
            }
            dir = (!extended).then_some(parent);
        }
        self.unlink_old_inode_refs(walk, key, &logged).await?;
        self.overwrite_item(walk, key, item).await
    }

    /// Unlinks what stands in the way of linking `name` of `inode` into
    /// `dir` at `index`: the names of `inode` in `dir` the log does not
    /// hold, and any entry already at that index or with that name:
    /// `__add_inode_ref`, `unlink_refs_not_in_log` and
    /// `unlink_extrefs_not_in_log`. Returns false for the root directory's
    /// reference to itself, which is never replaced.
    async fn resolve_ref_conflicts(
        &mut self,
        walk: &mut Walk,
        dir: u64,
        inode: u64,
        index: u64,
        name: &[u8],
    ) -> Result<bool, WriteError> {
        let tree = walk.tree;
        'again: loop {
            let root = self.roots.get(tree)?;
            let reference_key = Key::new(inode, item_type::INODE_REF, dir);
            if let Some(bytes) = self.trees.get(&self.volume, root, reference_key).await? {
                if inode == dir {
                    return Ok(false);
                }
                for victim in names(tree, reference_key, &bytes)? {
                    if !self
                        .backref_in_log(walk, reference_key, dir, &victim.bytes)
                        .await?
                    {
                        self.unlink_for_replay(walk, dir, inode, &victim.bytes, Links::Kept)
                            .await?;
                        continue 'again;
                    }
                }
            }
            let extended_key = Key::new(inode, item_type::INODE_EXTREF, inode_ref_hash(dir, name));
            if let Some(bytes) = self.trees.get(&self.volume, root, extended_key).await? {
                let entries = names(tree, extended_key, &bytes)?;
                if entries
                    .iter()
                    .any(|entry| entry.parent == dir && entry.bytes == name)
                {
                    for victim in entries.iter().filter(|entry| entry.parent == dir) {
                        let victim_key = Key::new(
                            inode,
                            item_type::INODE_EXTREF,
                            inode_ref_hash(dir, &victim.bytes),
                        );
                        if !self
                            .backref_in_log(walk, victim_key, dir, &victim.bytes)
                            .await?
                        {
                            self.unlink_for_replay(walk, dir, inode, &victim.bytes, Links::Kept)
                                .await?;
                            continue 'again;
                        }
                    }
                }
            }
            break;
        }
        if let Some((location, _)) = self.index_named(tree, dir, index, name).await? {
            self.drop_one_dir_item(walk, dir, location, name).await?;
        }
        if let Some((location, _)) = self.entry_named(tree, dir, name).await? {
            self.drop_one_dir_item(walk, dir, location, name).await?;
        }
        Ok(true)
    }

    /// Unlinks every name the subvolume's inode reference item at `key`
    /// holds that `logged`, the log's item, does not: `unlink_old_inode_refs`.
    async fn unlink_old_inode_refs(
        &mut self,
        walk: &Walk,
        key: Key,
        logged: &[Name],
    ) -> Result<(), WriteError> {
        let tree = walk.tree;
        let extended = key.item_type == item_type::INODE_EXTREF;
        'again: loop {
            let root = self.roots.get(tree)?;
            let Some(bytes) = self.trees.get(&self.volume, root, key).await? else {
                return Ok(());
            };
            for name in names(tree, key, &bytes)? {
                let kept = logged.iter().any(|entry| {
                    entry.bytes == name.bytes && (!extended || entry.parent == name.parent)
                });
                if !kept {
                    self.inode_item(tree, name.parent).await?;
                    self.unlink_for_replay(
                        walk,
                        name.parent,
                        key.objectid,
                        &name.bytes,
                        Links::Dropped,
                    )
                    .await?;
                    continue 'again;
                }
            }
            return Ok(());
        }
    }

    /// Sets the link count of every inode recorded for the fixup to the
    /// names its references hold, and deletes the ones left with none, a
    /// directory after its entries: `fixup_inode_link_counts` and
    /// `fixup_inode_link_count`, then what the orphan cleanup does for the
    /// orphan items Linux inserts.
    pub(super) async fn fixup_link_counts(&mut self, walk: &mut Walk) -> Result<(), WriteError> {
        let tree = walk.tree;
        let mut orphans = BTreeSet::new();
        // Highest inode first, and every pass looks again from the top: a
        // directory's deletion records the inodes it unlinks.
        while let Some(inode) = walk.fixup.get_mut(&tree).and_then(BTreeSet::pop_last) {
            let (mut raw, metadata) = self.inode_item(tree, inode).await?;
            let nlink = self.count_links(tree, inode).await?;
            if nlink != raw.nlink.get() {
                raw.nlink.set(nlink);
                self.update_inode(tree, inode, raw).await?;
            }
            if nlink == 0 {
                if metadata.kind == FileKind::Directory {
                    self.replay_dir_deletes(walk, inode, None).await?;
                }
                orphans.insert(inode);
            }
        }
        for inode in orphans {
            self.remove_inode(tree, inode).await?;
        }
        Ok(())
    }

    /// The names all of `inode`'s reference and extended reference items
    /// hold: `count_inode_refs` and `count_inode_extrefs`.
    async fn count_links(&self, tree: u64, inode: u64) -> Result<u32, WriteError> {
        let root = self.roots.get(tree)?;
        let mut count = 0_u32;
        let mut search = Key::new(inode, item_type::INODE_REF, 0);
        while let Some((key, bytes)) = self.trees.ceiling(&self.volume, root, search).await? {
            if key.objectid != inode
                || !(item_type::INODE_REF..=item_type::INODE_EXTREF).contains(&key.item_type)
            {
                break;
            }
            for record in references(tree, key, &bytes) {
                record?;
                count = count
                    .checked_add(1)
                    .ok_or(WriteError::LinkLimit { tree, inode })?;
            }
            let Some(next) = key.offset.checked_add(1) else {
                if key.item_type == item_type::INODE_EXTREF {
                    break;
                }
                search = Key::new(inode, item_type::INODE_EXTREF, 0);
                continue;
            };
            search = Key::new(key.objectid, key.item_type, next);
        }
        Ok(count)
    }
}

/// The last directory index of a log's `DIR_LOG_INDEX` range item:
/// `btrfs_dir_log_item`, one `end` field.
fn range_end(key: Key, item: &[u8]) -> Result<u64, Error> {
    let bytes: [u8; 8] = item.try_into().map_err(|_| {
        log_defect(key)(ItemDefect::Size {
            needed: 8,
            found: item.len(),
        })
    })?;
    Ok(u64::from_le_bytes(bytes))
}
