//! Namespace edits share a transaction with inode and extent changes. Names,
//! directory indices and backward references become visible in one commit.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use helios_hal::fs::RenameMode;
use zerocopy::{FromZeros, IntoBytes};

use super::Transaction;
use crate::csum::{inode_ref_hash, name_hash};
use crate::items::{INODE_APPEND, INODE_IMMUTABLE, RecordKind, dir_index_record, dir_records};
use crate::ondisk::{
    NAME_MAX, RawDirItem, RawFileExtentHead, RawHeader, RawInodeExtref, RawInodeItem, RawInodeRef,
    RawItem, RawKey, RawTimespec, item_type, objectid, view,
};
use crate::source::BlockSink;
use crate::tree::write::WriteError;
use crate::{EntryTarget, Error, FileKind, ItemDefect, Key, Metadata, Timestamp};

#[derive(Clone, Copy)]
pub(crate) struct Name<'a> {
    pub(crate) tree: u64,
    pub(crate) parent: u64,
    pub(crate) bytes: &'a [u8],
}

pub(crate) struct Attributes {
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

fn validate_name(name: &[u8]) -> Result<(), WriteError> {
    if name.is_empty()
        || name.len() > NAME_MAX
        || name == b"."
        || name == b".."
        || name.iter().any(|byte| *byte == 0 || *byte == b'/')
    {
        return Err(WriteError::InvalidName);
    }
    Ok(())
}

pub(super) fn check_flags(
    tree: u64,
    inode: u64,
    flags: u64,
    forbidden: u64,
) -> Result<(), WriteError> {
    let flags = flags & forbidden;
    if flags != 0 {
        return Err(WriteError::InodeFlags { tree, inode, flags });
    }
    Ok(())
}

pub(super) fn time(tree: u64, inode: u64, when: Timestamp) -> Result<RawTimespec, WriteError> {
    if when.nanoseconds >= 1_000_000_000 {
        return Err(Error::Item {
            tree,
            key: Key::new(inode, item_type::INODE_ITEM, 0),
            defect: ItemDefect::Nanoseconds(when.nanoseconds),
        }
        .into());
    }
    Ok(RawTimespec {
        sec: (when.seconds as u64).into(),
        nsec: when.nanoseconds.into(),
    })
}

/// One name of an inode reference or extended reference item.
#[derive(Clone, Copy)]
pub(super) struct Reference<'a> {
    pub(super) parent: u64,
    pub(super) index: u64,
    pub(super) name: &'a [u8],
}

/// The names an inode reference (`key` typed `INODE_REF`, its offset the
/// parent) or extended reference item holds, each with the byte range it
/// occupies, validated as it is reached.
pub(super) fn references(
    tree: u64,
    key: Key,
    bytes: &[u8],
) -> impl Iterator<Item = Result<(core::ops::Range<usize>, Reference<'_>), Error>> {
    let extended = key.item_type == item_type::INODE_EXTREF;
    let header_size = if extended {
        size_of::<RawInodeExtref>()
    } else {
        size_of::<RawInodeRef>()
    };
    let mut position = 0;
    core::iter::from_fn(move || {
        if position >= bytes.len() {
            return None;
        }
        let payload = &bytes[position..];
        let malformed = |defect| Error::Item { tree, key, defect };
        let short = ItemDefect::Size {
            needed: header_size,
            found: payload.len(),
        };
        let fields = if extended {
            view::<RawInodeExtref>(payload).map(|raw| {
                (
                    raw.parent.get(),
                    raw.index.get(),
                    usize::from(raw.name_len.get()),
                )
            })
        } else {
            view::<RawInodeRef>(payload)
                .map(|raw| (key.offset, raw.index.get(), usize::from(raw.name_len.get())))
        };
        let Some((parent, index, length)) = fields else {
            position = bytes.len();
            return Some(Err(malformed(short)));
        };
        if length == 0 || length > NAME_MAX {
            position = bytes.len();
            return Some(Err(malformed(ItemDefect::NameLength(length))));
        }
        let end = position + header_size + length;
        let Some(name) = bytes.get(position + header_size..end) else {
            position = bytes.len();
            return Some(Err(malformed(ItemDefect::Size {
                needed: header_size + length,
                found: payload.len(),
            })));
        };
        let range = position..end;
        position = end;
        Some(Ok((
            range,
            Reference {
                parent,
                index,
                name,
            },
        )))
    })
}

/// A directory item or index record naming `inode` of `kind`, written in
/// transaction `transid`.
pub(super) fn entry_record(inode: u64, kind: FileKind, transid: u64, name: &[u8]) -> Vec<u8> {
    let record = RawDirItem {
        location: RawKey {
            objectid: inode.into(),
            item_type: item_type::INODE_ITEM,
            offset: 0.into(),
        },
        transid: transid.into(),
        data_len: 0.into(),
        name_len: (name.len() as u16).into(),
        entry_type: entry_type(kind),
    };
    let mut bytes = Vec::from(record.as_bytes());
    bytes.extend_from_slice(name);
    bytes
}

pub(super) fn entry_type(kind: FileKind) -> u8 {
    match kind {
        FileKind::Regular => 1,
        FileKind::Directory => 2,
        FileKind::CharDevice => 3,
        FileKind::BlockDevice => 4,
        FileKind::Fifo => 5,
        FileKind::Socket => 6,
        FileKind::Symlink => 7,
    }
}

impl<S: BlockSink> Transaction<S> {
    pub(crate) async fn inode_item(
        &self,
        tree: u64,
        inode: u64,
    ) -> Result<(RawInodeItem, Metadata), WriteError> {
        let root = self.roots.get(tree)?;
        let key = Key::new(inode, item_type::INODE_ITEM, 0);
        let bytes = self
            .trees
            .get(&self.volume, root, key)
            .await?
            .ok_or(Error::MissingInode { tree, inode })?;
        let metadata =
            Metadata::parse(&bytes).map_err(|defect| Error::Item { tree, key, defect })?;
        Ok((
            *view::<RawInodeItem>(&bytes).expect("validated inode"),
            metadata,
        ))
    }

    pub(super) async fn put_inode(
        &mut self,
        tree: u64,
        inode: u64,
        raw: RawInodeItem,
    ) -> Result<(), WriteError> {
        let root = self.roots.get(tree)?;
        let root = self
            .trees
            .apply(
                &self.volume,
                root,
                Key::new(inode, item_type::INODE_ITEM, 0),
                Some(raw.as_bytes()),
                &mut self.space,
            )
            .await?;
        self.roots.set(root)?;
        if raw.mode.get() & 0o170_000 == 0o040_000 {
            self.roots.directory_size(tree, inode, raw.size.get())?;
        }
        Ok(())
    }

    pub(crate) async fn set_times(
        &mut self,
        tree: u64,
        inode: u64,
        access: Option<Timestamp>,
        modified: Option<Timestamp>,
        changed: Timestamp,
    ) -> Result<(), WriteError> {
        let (mut raw, metadata) = self.inode_item(tree, inode).await?;
        check_flags(tree, inode, metadata.flags, INODE_IMMUTABLE | INODE_APPEND)?;
        let changed = time(tree, inode, changed)?;
        if let Some(access) = access {
            raw.atime = time(tree, inode, access)?;
        }
        if let Some(modified) = modified {
            raw.mtime = time(tree, inode, modified)?;
        }
        raw.ctime = changed;
        raw.transid.set(self.trees.generation());
        raw.sequence.set(raw.sequence.get().wrapping_add(1));
        self.put_inode(tree, inode, raw).await
    }

    pub(crate) async fn create_node(
        &mut self,
        name: Name<'_>,
        attributes: Attributes,
        when: Timestamp,
    ) -> Result<u64, WriteError> {
        validate_name(name.bytes)?;
        super::ownership::refuse_set_id(attributes.mode)?;
        let tree = name.tree;
        let next = if let Some(last) = self.inode_high_water.get(&tree) {
            last.checked_add(1)
        } else {
            let root = self.roots.get(tree)?;
            let last = self
                .trees
                .floor(
                    &self.volume,
                    root,
                    Key::new(objectid::LAST_FREE, u8::MAX, u64::MAX),
                )
                .await?;
            last.map_or(Some(objectid::FIRST_FREE), |(key, _)| {
                key.objectid.checked_add(1)
            })
        };
        let inode = next
            .filter(|number| *number <= objectid::LAST_FREE)
            .ok_or(WriteError::IdentifierExhausted { tree })?;
        self.inode_high_water.insert(tree, inode);
        let timestamp = time(tree, inode, when)?;
        let mut raw = RawInodeItem::new_zeroed();
        raw.generation.set(self.trees.generation());
        raw.transid.set(self.trees.generation());
        raw.mode.set(attributes.mode);
        raw.uid.set(attributes.uid);
        raw.gid.set(attributes.gid);
        raw.atime = timestamp;
        raw.ctime = timestamp;
        raw.mtime = timestamp;
        raw.otime = timestamp;
        let metadata = Metadata::parse(raw.as_bytes()).map_err(|defect| Error::Item {
            tree,
            key: Key::new(inode, item_type::INODE_ITEM, 0),
            defect,
        })?;
        self.attach(name, inode, raw, metadata.kind, when).await?;
        Ok(inode)
    }

    pub(crate) async fn hard_link(
        &mut self,
        name: Name<'_>,
        inode: u64,
        when: Timestamp,
    ) -> Result<(), WriteError> {
        let (raw, metadata) = self.inode_item(name.tree, inode).await?;
        if metadata.kind == FileKind::Directory {
            return Err(WriteError::DirectoryLink);
        }
        self.attach(name, inode, raw, metadata.kind, when).await
    }

    pub(crate) async fn symlink(
        &mut self,
        name: Name<'_>,
        target: &[u8],
        mut attributes: Attributes,
        when: Timestamp,
    ) -> Result<u64, WriteError> {
        // Linux PATH_MAX includes the terminating NUL; an inline symlink must
        // also fit in one leaf alongside its item and extent headers.
        let maximum = 4095.min(
            self.volume.superblock().nodesize() as usize
                - size_of::<RawHeader>()
                - size_of::<RawItem>()
                - size_of::<RawFileExtentHead>(),
        );
        if target.is_empty() || target.len() > maximum || target.contains(&0) {
            return Err(WriteError::SymlinkTarget {
                length: target.len(),
                maximum,
            });
        }
        attributes.mode = (attributes.mode & 0o7777) | 0o120000;
        let inode = self.create_node(name, attributes, when).await?;
        let mut head = RawFileExtentHead::new_zeroed();
        head.generation.set(self.trees.generation());
        head.ram_bytes.set(target.len() as u64);
        let mut bytes = Vec::from(head.as_bytes());
        bytes.extend_from_slice(target);
        let root = self.roots.get(name.tree)?;
        let root = self
            .trees
            .apply(
                &self.volume,
                root,
                Key::new(inode, item_type::EXTENT_DATA, 0),
                Some(&bytes),
                &mut self.space,
            )
            .await?;
        self.roots.set(root)?;
        let (mut raw, _) = self.inode_item(name.tree, inode).await?;
        raw.size.set(target.len() as u64);
        raw.nbytes.set(target.len() as u64);
        self.put_inode(name.tree, inode, raw).await?;
        Ok(inode)
    }

    pub(crate) async fn rename(
        &mut self,
        source: Name<'_>,
        destination: Name<'_>,
        mode: RenameMode,
        when: Timestamp,
    ) -> Result<(), WriteError> {
        if source.tree != destination.tree {
            return Err(WriteError::CrossSubvolume);
        }
        let (inode, kind) = self
            .lookup_entry(source)
            .await?
            .ok_or(WriteError::NoEntry {
                tree: source.tree,
                parent: source.parent,
            })?;
        let existing = self.lookup_entry(destination).await?;
        match (mode, existing) {
            (RenameMode::NoReplace, Some(_)) => {
                return Err(WriteError::Exists {
                    tree: destination.tree,
                    parent: destination.parent,
                });
            }
            (RenameMode::Exchange, None) => {
                return Err(WriteError::NoEntry {
                    tree: destination.tree,
                    parent: destination.parent,
                });
            }
            (RenameMode::Exchange, Some(other)) => {
                return self
                    .exchange(source, (inode, kind), destination, other, when)
                    .await;
            }
            (RenameMode::Replace | RenameMode::NoReplace, _) => {}
        }
        if existing.is_some_and(|(other, _)| other == inode) {
            return Ok(());
        }
        if kind == FileKind::Directory {
            self.check_directory_move(source.tree, inode, destination.parent)
                .await?;
        }
        if let Some((_, other_kind)) = existing {
            if (kind == FileKind::Directory) != (other_kind == FileKind::Directory) {
                return Err(Error::WrongKind {
                    tree: destination.tree,
                    inode,
                    expected: kind,
                    found: other_kind,
                }
                .into());
            }
            self.unlink_name(destination, when).await?;
        }
        let (raw, _) = self.inode_item(source.tree, inode).await?;
        self.attach(destination, inode, raw, kind, when).await?;
        self.detach_name(source, when).await
    }

    /// Swaps what two existing names refer to. Each name keeps its place in
    /// its directory, its directory index and its length, so no directory
    /// grows or shrinks and no link count changes; the entries now point at
    /// the other inode and the two inodes' back-references trade parents,
    /// which is all a directory's `..` is in btrfs.
    async fn exchange(
        &mut self,
        first: Name<'_>,
        (first_inode, first_kind): (u64, FileKind),
        second: Name<'_>,
        (second_inode, second_kind): (u64, FileKind),
        when: Timestamp,
    ) -> Result<(), WriteError> {
        if first_inode == second_inode {
            return Ok(());
        }
        let tree = first.tree;
        // Each directory must land outside itself.
        if first_kind == FileKind::Directory {
            self.check_directory_move(tree, first_inode, second.parent)
                .await?;
        }
        if second_kind == FileKind::Directory {
            self.check_directory_move(tree, second_inode, first.parent)
                .await?;
        }
        for parent in [first.parent, second.parent] {
            let (_, metadata) = self.inode_item(tree, parent).await?;
            check_flags(tree, parent, metadata.flags, INODE_IMMUTABLE | INODE_APPEND)?;
        }
        for inode in [first_inode, second_inode] {
            let (_, metadata) = self.inode_item(tree, inode).await?;
            check_flags(tree, inode, metadata.flags, INODE_IMMUTABLE | INODE_APPEND)?;
        }
        let (first_key, first_index, first_rest) = self
            .without_reference(tree, first.parent, first_inode, first.bytes)
            .await?;
        self.put_item(
            tree,
            first_key,
            (!first_rest.is_empty()).then_some(&first_rest),
        )
        .await?;
        let (second_key, second_index, second_rest) = self
            .without_reference(tree, second.parent, second_inode, second.bytes)
            .await?;
        self.put_item(
            tree,
            second_key,
            (!second_rest.is_empty()).then_some(&second_rest),
        )
        .await?;
        let (key, bytes) = self
            .new_reference(tree, second.parent, first_inode, second_index, second.bytes)
            .await?;
        self.put_item(tree, key, Some(&bytes)).await?;
        let (key, bytes) = self
            .new_reference(tree, first.parent, second_inode, first_index, first.bytes)
            .await?;
        self.put_item(tree, key, Some(&bytes)).await?;
        self.retarget(first, first_index, first_inode, second_inode, second_kind)
            .await?;
        self.retarget(second, second_index, second_inode, first_inode, first_kind)
            .await?;
        let timestamp = time(tree, first.parent, when)?;
        let parents = if first.parent == second.parent {
            &[first.parent][..]
        } else {
            &[first.parent, second.parent][..]
        };
        for (inode, directory) in parents
            .iter()
            .map(|parent| (*parent, true))
            .chain([(first_inode, false), (second_inode, false)])
        {
            let (mut raw, _) = self.inode_item(tree, inode).await?;
            raw.transid.set(self.trees.generation());
            raw.sequence.set(raw.sequence.get().wrapping_add(1));
            raw.ctime = timestamp;
            if directory {
                raw.mtime = timestamp;
            }
            self.put_inode(tree, inode, raw).await?;
        }
        Ok(())
    }

    pub(super) async fn put_item(
        &mut self,
        tree: u64,
        key: Key,
        value: Option<&Vec<u8>>,
    ) -> Result<(), WriteError> {
        let root = self.roots.get(tree)?;
        let root = self
            .trees
            .apply(
                &self.volume,
                root,
                key,
                value.map(Vec::as_slice),
                &mut self.space,
            )
            .await?;
        self.roots.set(root)
    }

    /// Makes the existing entry `name`, which sits at directory index
    /// `index` and refers to `from`, refer to `inode` of `kind` instead.
    async fn retarget(
        &mut self,
        name: Name<'_>,
        index: u64,
        from: u64,
        inode: u64,
        kind: FileKind,
    ) -> Result<(), WriteError> {
        let Name {
            tree,
            parent,
            bytes: name,
        } = name;
        let root = self.roots.get(tree)?;
        let hash_key = Key::new(parent, item_type::DIR_ITEM, name_hash(name));
        let mismatch = WriteError::NamespaceMismatch {
            tree,
            key: hash_key,
        };
        let mut names = self
            .trees
            .get(&self.volume, root, hash_key)
            .await?
            .ok_or(mismatch)?;
        let mut position = 0;
        let mut found = None;
        for record in dir_records(&names, RecordKind::Entry) {
            let record = record.map_err(|defect| Error::Item {
                tree,
                key: hash_key,
                defect,
            })?;
            if record.name == name {
                found = Some((position, record.location.objectid));
                break;
            }
            position += size_of::<RawDirItem>() + record.name.len();
        }
        let (position, current) = found.ok_or(WriteError::NamespaceMismatch {
            tree,
            key: hash_key,
        })?;
        let index_key = Key::new(parent, item_type::DIR_INDEX, index);
        let mut indexed = self.trees.get(&self.volume, root, index_key).await?.ok_or(
            WriteError::NamespaceMismatch {
                tree,
                key: index_key,
            },
        )?;
        let record = dir_index_record(&indexed).map_err(|defect| Error::Item {
            tree,
            key: index_key,
            defect,
        })?;
        if current != from || record.name != name || record.location.objectid != from {
            return Err(WriteError::NamespaceMismatch {
                tree,
                key: index_key,
            });
        }
        let generation = self.trees.generation();
        let point = |bytes: &mut [u8]| {
            let mut head = *view::<RawDirItem>(bytes).expect("validated record");
            head.location = RawKey {
                objectid: inode.into(),
                item_type: item_type::INODE_ITEM,
                offset: 0.into(),
            };
            head.entry_type = entry_type(kind);
            head.transid.set(generation);
            bytes[..size_of::<RawDirItem>()].copy_from_slice(head.as_bytes());
        };
        point(&mut names[position..]);
        point(&mut indexed);
        self.put_item(tree, hash_key, Some(&names)).await?;
        self.put_item(tree, index_key, Some(&indexed)).await
    }

    async fn check_directory_move(
        &self,
        tree: u64,
        inode: u64,
        mut parent: u64,
    ) -> Result<(), WriteError> {
        let root = self.roots.get(tree)?;
        let mut visited = BTreeSet::new();
        loop {
            if parent == inode || !visited.insert(parent) {
                return Err(WriteError::DirectoryCycle);
            }
            let key = Key::new(parent, item_type::INODE_REF, 0);
            let reference = self
                .trees
                .ceiling(&self.volume, root, key)
                .await?
                .ok_or(WriteError::NamespaceMismatch { tree, key })?;
            if !reference.0.is(parent, item_type::INODE_REF) {
                return Err(WriteError::NamespaceMismatch { tree, key });
            }
            // A subvolume root's sole parent reference points to itself.
            if reference.0.offset == parent {
                return Ok(());
            }
            parent = reference.0.offset;
        }
    }

    async fn lookup_entry(&self, name: Name<'_>) -> Result<Option<(u64, FileKind)>, WriteError> {
        match self.lookup_target(name).await? {
            Some((EntryTarget::Inode(inode), kind)) => Ok(Some((inode, kind))),
            Some((EntryTarget::Subvolume(_), _)) => Err(WriteError::SubvolumeLink),
            None => Ok(None),
        }
    }

    pub(super) async fn lookup_target(
        &self,
        name: Name<'_>,
    ) -> Result<Option<(EntryTarget, FileKind)>, WriteError> {
        validate_name(name.bytes)?;
        let (_, metadata) = self.inode_item(name.tree, name.parent).await?;
        if metadata.kind != FileKind::Directory {
            return Err(Error::WrongKind {
                tree: name.tree,
                inode: name.parent,
                expected: FileKind::Directory,
                found: metadata.kind,
            }
            .into());
        }
        let root = self.roots.get(name.tree)?;
        let key = Key::new(name.parent, item_type::DIR_ITEM, name_hash(name.bytes));
        let Some(bytes) = self.trees.get(&self.volume, root, key).await? else {
            return Ok(None);
        };
        for record in dir_records(&bytes, RecordKind::Entry) {
            let record = record.map_err(|defect| Error::Item {
                tree: name.tree,
                key,
                defect,
            })?;
            if record.name == name.bytes {
                return Ok(Some((record.target(), record.kind())));
            }
        }
        Ok(None)
    }

    pub(crate) async fn unlink_name(
        &mut self,
        name: Name<'_>,
        when: Timestamp,
    ) -> Result<(), WriteError> {
        let (inode, kind) = self.lookup_entry(name).await?.ok_or(WriteError::NoEntry {
            tree: name.tree,
            parent: name.parent,
        })?;
        if kind == FileKind::Directory {
            let root = self.roots.get(name.tree)?;
            if let Some((key, _)) = self
                .trees
                .ceiling(&self.volume, root, Key::new(inode, item_type::DIR_ITEM, 0))
                .await?
                && key.is(inode, item_type::DIR_ITEM)
            {
                return Err(WriteError::NotEmpty {
                    tree: name.tree,
                    inode,
                });
            }
        }
        self.detach_name(name, when).await
    }

    async fn detach_name(&mut self, name: Name<'_>, when: Timestamp) -> Result<(), WriteError> {
        validate_name(name.bytes)?;
        let Name {
            tree,
            parent,
            bytes: name,
        } = name;
        let timestamp = time(tree, parent, when)?;
        let (mut directory, parent_metadata) = self.inode_item(tree, parent).await?;
        check_flags(
            tree,
            parent,
            parent_metadata.flags,
            INODE_IMMUTABLE | INODE_APPEND,
        )?;
        if parent_metadata.kind != FileKind::Directory {
            return Err(Error::WrongKind {
                tree,
                inode: parent,
                expected: FileKind::Directory,
                found: parent_metadata.kind,
            }
            .into());
        }
        let root = self.roots.get(tree)?;
        let hash_key = Key::new(parent, item_type::DIR_ITEM, name_hash(name));
        let mut bytes = self
            .trees
            .get(&self.volume, root, hash_key)
            .await?
            .ok_or(WriteError::NoEntry { tree, parent })?;
        let mut position = 0;
        let mut found = None;
        for record in dir_records(&bytes, RecordKind::Entry) {
            let record = record.map_err(|defect| Error::Item {
                tree,
                key: hash_key,
                defect,
            })?;
            let end = position + size_of::<RawDirItem>() + record.name.len();
            if record.name == name {
                found = Some((position..end, record.location, record.kind()));
            }
            position = end;
        }
        let (range, location, kind) = found.ok_or(WriteError::NoEntry { tree, parent })?;
        if location.item_type == item_type::ROOT_ITEM {
            return Err(WriteError::SubvolumeLink);
        }
        let inode = location.objectid;
        let (mut raw, metadata) = self.inode_item(tree, inode).await?;
        check_flags(tree, inode, metadata.flags, INODE_IMMUTABLE | INODE_APPEND)?;
        if metadata.kind != kind || raw.nlink.get() == 0 {
            return Err(WriteError::NamespaceMismatch {
                tree,
                key: hash_key,
            });
        }
        let (reference_key, index, reference) =
            self.without_reference(tree, parent, inode, name).await?;
        let index_key = Key::new(parent, item_type::DIR_INDEX, index);
        let indexed = self.trees.get(&self.volume, root, index_key).await?.ok_or(
            WriteError::NamespaceMismatch {
                tree,
                key: index_key,
            },
        )?;
        let record = dir_index_record(&indexed).map_err(|defect| Error::Item {
            tree,
            key: index_key,
            defect,
        })?;
        if record.name != name || record.location != location || record.kind() != kind {
            return Err(WriteError::NamespaceMismatch {
                tree,
                key: index_key,
            });
        }
        directory.size.set(
            directory
                .size
                .get()
                .checked_sub(2 * name.len() as u64)
                .ok_or(WriteError::FileOverflow)?,
        );
        bytes.drain(range);
        let mut root = self
            .trees
            .apply(
                &self.volume,
                root,
                hash_key,
                (!bytes.is_empty()).then_some(bytes.as_slice()),
                &mut self.space,
            )
            .await?;
        root = self
            .trees
            .apply(&self.volume, root, index_key, None, &mut self.space)
            .await?;
        root = self
            .trees
            .apply(
                &self.volume,
                root,
                reference_key,
                (!reference.is_empty()).then_some(reference.as_slice()),
                &mut self.space,
            )
            .await?;
        self.roots.set(root)?;
        raw.nlink.set(raw.nlink.get() - 1);
        if raw.nlink.get() == 0 {
            self.remove_inode(tree, inode).await?;
        } else {
            raw.transid.set(self.trees.generation());
            raw.sequence.set(raw.sequence.get().wrapping_add(1));
            raw.ctime = timestamp;
            self.put_inode(tree, inode, raw).await?;
        }
        directory.transid.set(self.trees.generation());
        directory
            .sequence
            .set(directory.sequence.get().wrapping_add(1));
        directory.ctime = timestamp;
        directory.mtime = timestamp;
        self.put_inode(tree, parent, directory).await
    }

    pub(super) async fn without_reference(
        &self,
        tree: u64,
        parent: u64,
        inode: u64,
        name: &[u8],
    ) -> Result<(Key, u64, Vec<u8>), WriteError> {
        let root = self.roots.get(tree)?;
        for key in [
            Key::new(inode, item_type::INODE_REF, parent),
            Key::new(inode, item_type::INODE_EXTREF, inode_ref_hash(parent, name)),
        ] {
            let Some(mut bytes) = self.trees.get(&self.volume, root, key).await? else {
                continue;
            };
            let mut found = None;
            for record in references(tree, key, &bytes) {
                let (range, record) = record?;
                if record.parent == parent && record.name == name {
                    found = Some((range, record.index));
                    break;
                }
            }
            if let Some((range, index)) = found {
                bytes.drain(range);
                return Ok((key, index, bytes));
            }
        }
        Err(WriteError::NamespaceMismatch {
            tree,
            key: Key::new(inode, item_type::INODE_REF, parent),
        })
    }

    async fn attach(
        &mut self,
        name: Name<'_>,
        inode: u64,
        mut raw: RawInodeItem,
        kind: FileKind,
        when: Timestamp,
    ) -> Result<(), WriteError> {
        validate_name(name.bytes)?;
        let Name {
            tree,
            parent,
            bytes: name,
        } = name;
        let timestamp = time(tree, inode, when)?;
        let (mut directory, metadata) = self.inode_item(tree, parent).await?;
        check_flags(tree, parent, metadata.flags, INODE_IMMUTABLE)?;
        check_flags(tree, inode, raw.flags.get(), INODE_IMMUTABLE | INODE_APPEND)?;
        if metadata.kind != FileKind::Directory {
            return Err(Error::WrongKind {
                tree,
                inode: parent,
                expected: FileKind::Directory,
                found: metadata.kind,
            }
            .into());
        }
        let root = self.roots.get(tree)?;
        let hash_key = Key::new(parent, item_type::DIR_ITEM, name_hash(name));
        let mut names = self
            .trees
            .get(&self.volume, root, hash_key)
            .await?
            .unwrap_or_default();
        for record in dir_records(&names, RecordKind::Entry) {
            let record = record.map_err(|defect| Error::Item {
                tree,
                key: hash_key,
                defect,
            })?;
            if record.name == name {
                return Err(WriteError::Exists { tree, parent });
            }
        }
        let index = match self
            .trees
            .floor(
                &self.volume,
                root,
                Key::new(parent, item_type::DIR_INDEX, u64::MAX),
            )
            .await?
        {
            Some((key, _)) if key.is(parent, item_type::DIR_INDEX) => key
                .offset
                .checked_add(1)
                .ok_or(WriteError::IdentifierExhausted { tree })?,
            // btrfs reserves directory indices zero and one for dot entries.
            _ => 2,
        };
        raw.nlink.set(
            raw.nlink
                .get()
                .checked_add(1)
                .ok_or(WriteError::LinkLimit { tree, inode })?,
        );
        directory.size.set(
            directory
                .size
                .get()
                .checked_add(2 * name.len() as u64)
                .ok_or(WriteError::FileOverflow)?,
        );
        let bytes = entry_record(inode, kind, self.trees.generation(), name);
        names.extend_from_slice(&bytes);
        let (reference_key, reference) =
            self.new_reference(tree, parent, inode, index, name).await?;
        let mut root = self
            .trees
            .apply(&self.volume, root, hash_key, Some(&names), &mut self.space)
            .await?;
        root = self
            .trees
            .apply(
                &self.volume,
                root,
                Key::new(parent, item_type::DIR_INDEX, index),
                Some(&bytes),
                &mut self.space,
            )
            .await?;
        root = self
            .trees
            .apply(
                &self.volume,
                root,
                reference_key,
                Some(&reference),
                &mut self.space,
            )
            .await?;
        self.roots.set(root)?;
        directory.transid.set(self.trees.generation());
        directory
            .sequence
            .set(directory.sequence.get().wrapping_add(1));
        directory.mtime = timestamp;
        directory.ctime = timestamp;
        raw.transid.set(self.trees.generation());
        raw.sequence.set(raw.sequence.get().wrapping_add(1));
        raw.ctime = timestamp;
        self.put_inode(tree, parent, directory).await?;
        self.put_inode(tree, inode, raw).await
    }

    pub(super) async fn new_reference(
        &self,
        tree: u64,
        parent: u64,
        inode: u64,
        index: u64,
        name: &[u8],
    ) -> Result<(Key, Vec<u8>), WriteError> {
        let root = self.roots.get(tree)?;
        let key = Key::new(inode, item_type::INODE_REF, parent);
        let mut bytes = self
            .trees
            .get(&self.volume, root, key)
            .await?
            .unwrap_or_default();
        let capacity = self.volume.superblock().nodesize() as usize
            - size_of::<RawHeader>()
            - size_of::<RawItem>();
        if bytes.len() + size_of::<RawInodeRef>() + name.len() <= capacity {
            let record = RawInodeRef {
                index: index.into(),
                name_len: (name.len() as u16).into(),
            };
            bytes.extend_from_slice(record.as_bytes());
            bytes.extend_from_slice(name);
            return Ok((key, bytes));
        }
        // BTRFS_FEATURE_INCOMPAT_EXTENDED_IREF permits references keyed by
        // parent/name hash once the ordinary parent-keyed item fills a leaf.
        if self.volume.superblock().incompat_flags() & (1 << 6) == 0 {
            return Err(WriteError::LinkLimit { tree, inode });
        }
        let key = Key::new(inode, item_type::INODE_EXTREF, inode_ref_hash(parent, name));
        let mut bytes = self
            .trees
            .get(&self.volume, root, key)
            .await?
            .unwrap_or_default();
        if bytes.len() + size_of::<RawInodeExtref>() + name.len() > capacity {
            return Err(WriteError::LinkLimit { tree, inode });
        }
        let record = RawInodeExtref {
            parent: parent.into(),
            index: index.into(),
            name_len: (name.len() as u16).into(),
        };
        bytes.extend_from_slice(record.as_bytes());
        bytes.extend_from_slice(name);
        Ok((key, bytes))
    }
}
