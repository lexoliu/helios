//! Transaction-owned root items. Updating their tree pointers does not publish
//! them: the root tree itself remains in the CoW overlay until superblock commit.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use rpds::RedBlackTreeMapSync;

use zerocopy::IntoBytes;

use crate::items::RootRecord;
use crate::ondisk::{RawRootItem, RawRootTail, item_type, objectid, view};
use crate::source::BlockSource;
use crate::space::Space;
use crate::tree::write::{Trees, WriteError};
use crate::tree::{Cursor, TreeRoot};
use crate::uuid_tree::{self, UuidTree};
use crate::volume::Volume;
use crate::{Error, Key};

#[derive(Clone)]
struct RootItem {
    key: Key,
    root: TreeRoot,
    initial_used: u64,
    dirty: bool,
    bytes: Vec<u8>,
}

impl RootItem {
    fn internal(root: TreeRoot) -> Self {
        let mut bytes = vec![0; size_of::<RawRootItem>() + size_of::<RawRootTail>()];
        let mut head = *view::<RawRootItem>(&bytes).expect("root item has its full size");
        head.refs.set(1);
        bytes[..size_of::<RawRootItem>()].copy_from_slice(head.as_bytes());
        Self {
            key: Key::new(root.tree, item_type::ROOT_ITEM, 0),
            root,
            initial_used: 0,
            dirty: false,
            bytes,
        }
    }

    fn parse(key: Key, bytes: &[u8], sector: u32) -> Result<Self, Error> {
        let record = RootRecord::parse(key, bytes, sector).map_err(|defect| Error::Item {
            tree: objectid::ROOT_TREE,
            key,
            defect,
        })?;
        let head: &RawRootItem = view(bytes).expect("validated root item");
        Ok(Self {
            key,
            root: record.root,
            initial_used: head.bytes_used.get(),
            dirty: false,
            bytes: bytes.to_vec(),
        })
    }

    /// Returns the replacement UUID when an old-format or invalid extension
    /// needs initialization. Roots::sync updates the UUID index from these fields
    /// before the enclosing transaction can reach a stable root catalogue.
    fn encode(&mut self, used: u64, new_uuid: &mut impl FnMut() -> [u8; 16]) -> Option<[u8; 16]> {
        let head_size = size_of::<RawRootItem>();
        let full_size = head_size + size_of::<RawRootTail>();
        let mut head = *view::<RawRootItem>(&self.bytes).expect("validated root item");
        let valid_tail = view::<RawRootTail>(&self.bytes[head_size..])
            .is_some_and(|tail| tail.generation_v2.get() == head.generation.get());
        let initialized = if valid_tail {
            None
        } else {
            if self.bytes.len() < full_size {
                self.bytes.resize(full_size, 0);
            }
            self.bytes[head_size..full_size].fill(0);
            Some(new_uuid())
        };
        let mut tail =
            *view::<RawRootTail>(&self.bytes[head_size..]).expect("initialized root extension");
        if let Some(uuid) = initialized {
            tail.uuid = uuid;
        }
        head.generation.set(self.root.generation);
        head.bytenr.set(self.root.bytenr);
        head.level = self.root.level;
        head.bytes_used.set(used);
        tail.generation_v2.set(self.root.generation);
        self.bytes[..head_size].copy_from_slice(head.as_bytes());
        self.bytes[head_size..full_size].copy_from_slice(tail.as_bytes());
        initialized
    }
}

#[derive(Clone)]
pub(crate) struct Roots {
    pub(crate) root: TreeRoot,
    entries: RedBlackTreeMapSync<u64, RootItem>,
    uuid_tree: Option<Arc<UuidTree>>,
}

impl Roots {
    pub(crate) async fn load<S: BlockSource>(
        volume: &Volume<S>,
        root: TreeRoot,
    ) -> Result<Self, WriteError> {
        let mut entries = RedBlackTreeMapSync::new_sync();
        let mut cursor = Cursor::ceiling(volume, root, Key::new(0, 0, 0)).await?;
        while let (Some(key), Some(bytes)) = (cursor.key(), cursor.item()) {
            if key.item_type == item_type::ROOT_ITEM {
                // Cursor order selects the newest root-item offset for a tree,
                // matching the read path's find_root lookup.
                entries.insert_mut(
                    key.objectid,
                    RootItem::parse(key, bytes, volume.superblock().sectorsize())?,
                );
            }
            cursor.advance(volume).await?;
        }
        let uuid_tree = if let Some(entry) = entries.get(&objectid::UUID_TREE) {
            Some(Arc::new(UuidTree::load(volume, entry.root).await?))
        } else {
            None
        };
        Ok(Self {
            root,
            entries,
            uuid_tree,
        })
    }

    pub(crate) fn get(&self, tree: u64) -> Result<TreeRoot, WriteError> {
        self.entries
            .get(&tree)
            .map(|entry| entry.root)
            .ok_or_else(|| Error::MissingRoot { tree }.into())
    }

    /// Bytes of tree blocks tree `tree` had when this transaction started; 0
    /// for a tree it does not have.
    pub(crate) fn committed_used(&self, tree: u64) -> u64 {
        self.entries
            .get(&tree)
            .map_or(0, |entry| entry.initial_used)
    }

    /// Tree `tree`'s root as this transaction has moved it, with its root
    /// directory: what the root tree will record for it at commit.
    pub(crate) fn record(&self, tree: u64) -> Result<RootRecord, Error> {
        let entry = self.entries.get(&tree).ok_or(Error::MissingRoot { tree })?;
        Ok(RootRecord {
            root: entry.root,
            root_dirid: view::<RawRootItem>(&entry.bytes)
                .expect("validated root item")
                .root_dirid
                .get(),
        })
    }

    #[cfg(feature = "write")]
    pub(crate) fn root_directory(&self, tree: u64) -> Result<u64, WriteError> {
        let entry = self.entries.get(&tree).ok_or(Error::MissingRoot { tree })?;
        Ok(view::<RawRootItem>(&entry.bytes)
            .expect("validated root item")
            .root_dirid
            .get())
    }

    #[cfg(feature = "write")]
    pub(crate) fn check_writable(&self, tree: u64) -> Result<(), WriteError> {
        let entry = self.entries.get(&tree).ok_or(Error::MissingRoot { tree })?;
        let head = view::<RawRootItem>(&entry.bytes).expect("validated root item");
        // BTRFS_ROOT_SUBVOL_RDONLY is bit zero of the root item's flags.
        if head.flags.get() & 1 != 0 {
            return Err(WriteError::ReadOnlySubvolume { tree });
        }
        Ok(())
    }

    /// Whether tree `tree` is a subvolume that is still referenced: one
    /// `btrfs_get_fs_root` opens when asked to check its root refs.
    #[cfg(feature = "write")]
    pub(crate) fn is_live(&self, tree: u64) -> bool {
        self.entries.get(&tree).is_some_and(|entry| {
            view::<RawRootItem>(&entry.bytes)
                .expect("validated root item")
                .refs
                .get()
                != 0
        })
    }

    /// Has the commit rewrite tree `tree`'s root item even when the tree did
    /// not move, as `btrfs_record_root_in_trans` does: the root tree then
    /// changes, so the commit publishes a new generation.
    #[cfg(feature = "write")]
    pub(crate) fn record_in_transaction(&mut self, tree: u64) -> Result<(), WriteError> {
        self.entries
            .get_mut(&tree)
            .ok_or(Error::MissingRoot { tree })?
            .dirty = true;
        Ok(())
    }

    pub(crate) fn set(&mut self, root: TreeRoot) -> Result<(), WriteError> {
        self.entries
            .get_mut(&root.tree)
            .ok_or(Error::MissingRoot { tree: root.tree })?
            .root = root;
        Ok(())
    }

    #[cfg(feature = "write")]
    pub(crate) fn directory_size(
        &mut self,
        tree: u64,
        inode: u64,
        size: u64,
    ) -> Result<(), WriteError> {
        let entry = self
            .entries
            .get_mut(&tree)
            .ok_or(Error::MissingRoot { tree })?;
        let mut head = *view::<RawRootItem>(&entry.bytes).expect("validated root item");
        if head.root_dirid.get() == inode && head.inode.size.get() != size {
            head.inode.size.set(size);
            entry.bytes[..size_of::<RawRootItem>()].copy_from_slice(head.as_bytes());
            entry.dirty = true;
        }
        Ok(())
    }

    /// Root bytes_used counts tree blocks reachable through that root, including
    /// shared blocks. A replaced committed node stops contributing even while
    /// its physical extent remains pinned for another root or an older reader.
    pub(crate) async fn sync<S: BlockSource>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
        new_uuid: &mut impl FnMut() -> [u8; 16],
    ) -> Result<bool, WriteError> {
        let size = u64::from(volume.superblock().nodesize());
        let mut changes = BTreeMap::<u64, (u64, u64)>::new();
        for (_, record) in space.metadata() {
            changes.entry(record.tree).or_default().0 += size;
        }
        for block in trees.retired() {
            changes.entry(block.tree).or_default().1 += size;
        }
        let mut changed = false;
        let entries = self.entries.clone();
        for (&tree, entry) in &entries {
            let (added, removed) = changes.get(&tree).copied().unwrap_or_default();
            let used = entry
                .initial_used
                .checked_add(added)
                .and_then(|used| used.checked_sub(removed))
                .ok_or(WriteError::RootUsage {
                    tree,
                    used: entry.initial_used,
                    added,
                    removed,
                })?;
            let head: &RawRootItem = view(&entry.bytes).expect("validated root item");
            if !entry.dirty
                && head.bytenr.get() == entry.root.bytenr
                && head.generation.get() == entry.root.generation
                && head.level == entry.root.level
                && head.bytes_used.get() == used
            {
                continue;
            }
            let entry = self.entries.get_mut(&tree).expect("catalogued root");
            entry.encode(used, new_uuid);
            self.root = trees
                .apply(volume, self.root, entry.key, Some(&entry.bytes), space)
                .await?;
            entry.dirty = false;
            changed = true;
        }
        let desired = self.uuid_records();
        if self.uuid_tree.is_none() && !desired.is_empty() {
            let root = trees
                .create(volume, self.root, objectid::UUID_TREE, space)
                .await?;
            self.entries.insert_mut(root.tree, RootItem::internal(root));
            self.uuid_tree = Some(Arc::new(UuidTree::empty(root)));
            changed = true;
        }
        if let Some(index) = &mut self.uuid_tree {
            changed |= Arc::make_mut(index)
                .sync(volume, trees, space, desired)
                .await?;
            self.entries
                .get_mut(&objectid::UUID_TREE)
                .expect("UUID tree has a root item")
                .root = index.root;
        }
        Ok(changed)
    }

    fn uuid_records(&self) -> BTreeMap<Key, Vec<u8>> {
        let mut records = BTreeMap::<Key, Vec<u8>>::new();
        for (&tree, entry) in &self.entries {
            if tree != objectid::FS_TREE
                && !(objectid::FIRST_FREE..=objectid::LAST_FREE).contains(&tree)
            {
                continue;
            }
            let head: &RawRootItem = view(&entry.bytes).expect("validated root item");
            if head.refs.get() == 0 {
                continue;
            }
            let Some(tail) = view::<RawRootTail>(&entry.bytes[size_of::<RawRootItem>()..]) else {
                continue;
            };
            if tail.generation_v2.get() != head.generation.get() {
                continue;
            }
            for (uuid, kind) in [
                (tail.uuid, item_type::UUID_SUBVOL),
                (tail.received_uuid, item_type::UUID_RECEIVED),
            ] {
                if uuid != [0; 16] {
                    records
                        .entry(uuid_tree::key(uuid, kind))
                        .or_default()
                        .extend_from_slice(&tree.to_le_bytes());
                }
            }
        }
        records
    }
}

#[cfg(test)]
mod tests {
    use btrfs_disk::items::RootItem as DiskRoot;

    use super::*;

    #[test]
    fn uuid_catalogue_preserves_collisions_and_removes_obsolete_mappings() {
        let mut roots = Roots {
            root: TreeRoot {
                tree: 1,
                bytenr: 4096,
                generation: 42,
                level: 0,
            },
            entries: RedBlackTreeMapSync::new_sync(),
            uuid_tree: None,
        };
        for tree in [objectid::FS_TREE, objectid::FIRST_FREE] {
            let bytes = DiskRoot::new_internal(42, 1 << 20, 0).to_bytes();
            let mut entry =
                RootItem::parse(Key::new(tree, item_type::ROOT_ITEM, 0), &bytes, 4096).unwrap();
            let offset = size_of::<RawRootItem>();
            let mut tail = *view::<RawRootTail>(&entry.bytes[offset..]).unwrap();
            tail.uuid = [1; 16];
            tail.received_uuid = [2; 16];
            entry.bytes[offset..].copy_from_slice(tail.as_bytes());
            roots.entries.insert_mut(tree, entry);
        }
        let both: Vec<_> = [objectid::FS_TREE, objectid::FIRST_FREE]
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect();
        let shared = uuid_tree::key([1; 16], item_type::UUID_SUBVOL);
        let received = uuid_tree::key([2; 16], item_type::UUID_RECEIVED);
        let records = roots.uuid_records();
        assert_eq!(records[&shared], both);
        assert_eq!(records[&received], both);

        let entry = roots.entries.get_mut(&objectid::FS_TREE).unwrap();
        let offset = size_of::<RawRootItem>();
        let mut tail = *view::<RawRootTail>(&entry.bytes[offset..]).unwrap();
        tail.uuid = [3; 16];
        tail.received_uuid = [0; 16];
        entry.bytes[offset..].copy_from_slice(tail.as_bytes());
        let records = roots.uuid_records();
        assert_eq!(records[&shared], objectid::FIRST_FREE.to_le_bytes());
        assert_eq!(records[&received], objectid::FIRST_FREE.to_le_bytes());
        assert_eq!(
            records[&uuid_tree::key([3; 16], item_type::UUID_SUBVOL)],
            objectid::FS_TREE.to_le_bytes()
        );

        let entry = roots.entries.get_mut(&objectid::FIRST_FREE).unwrap();
        let mut head = *view::<RawRootItem>(&entry.bytes).unwrap();
        head.refs.set(0);
        entry.bytes[..size_of::<RawRootItem>()].copy_from_slice(head.as_bytes());
        let records = roots.uuid_records();
        assert!(!records.contains_key(&shared));
        assert!(!records.contains_key(&received));
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn root_updates_preserve_valid_extensions_and_unrelated_fields() {
        let mut reference = DiskRoot::new_internal(42, 1 << 20, 0);
        reference.bytes_used = 4096;
        reference.byte_limit = 1 << 30;
        reference.ctransid = 31;
        reference.otransid = 7;
        reference.last_snapshot = 23;
        reference.inode_data.fill(0x25);
        let mut bytes = reference.to_bytes();
        let uuid_offset = size_of::<RawRootItem>() + size_of::<u64>();
        bytes[uuid_offset..uuid_offset + 16].fill(0x63);
        bytes.extend_from_slice(&[0x91; 16]);
        let before = DiskRoot::parse(&bytes).unwrap();
        let mut item = RootItem::parse(Key::new(5, item_type::ROOT_ITEM, 0), &bytes, 4096).unwrap();
        item.root.bytenr += 4096;
        item.root.generation += 1;
        item.root.level = 1;
        assert!(
            item.encode(12288, &mut || panic!("valid extension needs no UUID"))
                .is_none()
        );
        let after = DiskRoot::parse(&item.bytes).unwrap();
        assert_eq!(after.bytenr, item.root.bytenr);
        assert_eq!(after.generation, 43);
        assert_eq!(after.generation_v2, 43);
        assert_eq!(after.level, 1);
        assert_eq!(after.bytes_used, 12288);
        assert_eq!(after.uuid, before.uuid);
        assert_eq!(after.parent_uuid, before.parent_uuid);
        assert_eq!(after.received_uuid, before.received_uuid);
        assert_eq!(after.inode_data, before.inode_data);
        assert_eq!(after.byte_limit, before.byte_limit);
        assert_eq!(after.last_snapshot, before.last_snapshot);
        assert_eq!(after.ctransid, before.ctransid);
        assert_eq!(after.otransid, before.otransid);
        let full_size = size_of::<RawRootItem>() + size_of::<RawRootTail>();
        assert_eq!(&item.bytes[full_size..], &[0x91; 16]);
    }

    #[test]
    fn legacy_and_stale_root_extensions_get_one_new_uuid() {
        let mut reference = DiskRoot::new_internal(42, 1 << 20, 0);
        reference.bytes_used = 4096;
        reference.generation_v2 = 41;
        reference.ctransid = 29;
        reference.otransid = 5;
        let bytes = reference.to_bytes();
        for length in [
            size_of::<RawRootItem>(),
            size_of::<RawRootItem>() + 9,
            bytes.len(),
        ] {
            let mut item =
                RootItem::parse(Key::new(5, item_type::ROOT_ITEM, 0), &bytes[..length], 4096)
                    .unwrap();
            item.root.generation = 43;
            let uuid = [0x47; 16];
            assert_eq!(item.encode(4096, &mut || uuid), Some(uuid));
            let parsed = DiskRoot::parse(&item.bytes).unwrap();
            assert_eq!(parsed.uuid.as_bytes(), &uuid);
            assert_eq!(parsed.generation_v2, 43);
            assert_eq!(parsed.ctransid, 0);
            assert_eq!(parsed.otransid, 0);
            item.root.generation = 44;
            assert!(
                item.encode(4096, &mut || panic!("extension is already initialized"))
                    .is_none()
            );
            assert_eq!(DiskRoot::parse(&item.bytes).unwrap().uuid.as_bytes(), &uuid);
        }
    }
}
