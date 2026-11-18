//! Log replay over images whose log tree these tests write the way a Linux
//! fsync leaves one: the committed trees from a writer's commit, then a log
//! root tree and one log tree per subvolume in blocks no extent item
//! records, logged data in extents the extent tree does not hold yet, and the
//! superblock pointing at the log. Each test covers one stage of the replay.
//! The `#[ignore]`d test holds every replayed image to `btrfs check`, which
//! needs btrfs-progs. Images a real Linux workload leaves are replayed by
//! `tests/images/log_replay.rs`.

extern crate std;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;
use core::ops::ControlFlow;
use std::os::unix::fs::FileExt;

use ferrosys::{Metadata as Format, Timestamp as FormatTime, TreeBuilder};
use futures_lite::future::block_on;
use zerocopy::{FromBytes, FromZeros, IntoBytes};

use super::write_tests::{
    Disk, FixtureUuids, MOUNTED, assert_btrfs_check, filesystem_image_tree_file, mount_writer,
};
use super::writer::Writer;
use super::{Filesystem, find_root};
use crate::accounting::Allocation;
use crate::checksums;
use crate::csum::{inode_ref_hash, name_hash};
use crate::ondisk::{
    RawDirItem, RawFileExtentBody, RawFileExtentHead, RawInodeExtref, RawInodeItem, RawInodeRef,
    RawKey, RawRootItem, RawRootTail, RawSuperblock, RawTimespec, item_type, objectid,
};
use crate::retirement::Retirements;
use crate::source::BlockSink;
use crate::space::Space;
use crate::superblock::MIRROR_OFFSETS;
use crate::transaction::Transaction;
use crate::transaction::file::WriteOptions;
use crate::transaction::namespace::{Attributes, Name};
use crate::transaction::replay::{LogReplayError, PendingLog};
use crate::tree::edit::{Header, Leaf};
use crate::tree::write::{MetadataAllocator, WriteError};
use crate::tree::{Cursor, Expected, Node};
use crate::{DirPosition, Error, FileKind, Key, Timestamp};

/// When the baselines' files were made.
const WHEN: Timestamp = Timestamp {
    seconds: 1_700_000_100,
    nanoseconds: 7,
};
const SECTOR: usize = 4096;
/// Directory entry types: `BTRFS_FT_REG_FILE` and `BTRFS_FT_DIR`.
const ENTRY_FILE: u8 = 1;
const ENTRY_DIRECTORY: u8 = 2;

/// Deterministic bytes, distinct per `seed`.
fn pattern(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A formatted image file.
struct Image {
    named: tempfile::NamedTempFile,
}

impl Image {
    fn new() -> Self {
        let named = tempfile::NamedTempFile::new().unwrap();
        let builder =
            TreeBuilder::new().root(Format::new(0o755, FormatTime::from_secs(1_700_000_000)));
        drop(filesystem_image_tree_file(
            true,
            true,
            builder,
            named.reopen().unwrap(),
        ));
        Self { named }
    }

    fn disk(&self) -> Disk {
        Disk(self.named.reopen().unwrap())
    }

    async fn writer(&self) -> Writer<Disk, FixtureUuids> {
        mount_writer(self.disk()).await.unwrap()
    }

    async fn reader(&self) -> Filesystem<Disk> {
        Filesystem::mount(self.disk()).await.unwrap()
    }

    fn check(&self, check: bool) {
        if check {
            assert_btrfs_check(self.named.path());
        }
    }
}

/// Creates a directory, or a regular file holding `content`, in the
/// writer's running transaction.
async fn create<S: BlockSink>(
    edits: &mut Transaction<S>,
    tree: u64,
    parent: u64,
    entry: &str,
    content: Option<&[u8]>,
) -> u64 {
    let mode = if content.is_some() {
        0o100644
    } else {
        0o040755
    };
    let inode = edits
        .create_node(
            Name {
                tree,
                parent,
                bytes: entry.as_bytes(),
            },
            Attributes {
                mode,
                uid: 1000,
                gid: 1000,
            },
            WHEN,
        )
        .await
        .unwrap();
    if let Some(content) = content {
        let concurrency = std::thread::available_parallelism().unwrap();
        edits
            .write_file(
                tree,
                inode,
                0,
                content,
                WriteOptions::plain(WHEN, concurrency),
            )
            .await
            .unwrap();
    }
    inode
}

/// Sets xattr `name` of `inode`, an item of one record.
async fn set_xattr<S: BlockSink>(
    edits: &mut Transaction<S>,
    tree: u64,
    inode: u64,
    name: &str,
    value: &[u8],
) {
    let key = Key::new(inode, item_type::XATTR_ITEM, name_hash(name.as_bytes()));
    let root = edits.roots.get(tree).unwrap();
    let root = edits
        .trees
        .apply(
            &edits.volume,
            root,
            key,
            Some(&xattr(name, value)),
            &mut edits.space,
        )
        .await
        .unwrap();
    edits.roots.set(root).unwrap();
}

/// The default subvolume and its root directory.
async fn root_of(writer: &Writer<Disk, FixtureUuids>) -> (u64, u64) {
    let root = writer.snapshot().unwrap().root().await.unwrap();
    (root.tree(), root.number())
}

/// The committed item at `key` of tree `tree`.
async fn item(fs: &Filesystem<Disk>, tree: u64, key: Key) -> Option<Vec<u8>> {
    let root = find_root(&fs.volume, fs.snapshot.root_tree, tree)
        .await
        .unwrap()
        .root;
    let cursor = Cursor::floor(&fs.volume, root, key).await.unwrap();
    (cursor.key() == Some(key)).then(|| cursor.item().unwrap().to_vec())
}

async fn committed_inode(fs: &Filesystem<Disk>, inode: u64) -> RawInodeItem {
    let bytes = item(
        fs,
        objectid::FS_TREE,
        Key::new(inode, item_type::INODE_ITEM, 0),
    )
    .await
    .unwrap();
    *RawInodeItem::ref_from_prefix(&bytes).unwrap().0
}

/// The data extent the committed file extent of `inode` at `offset` maps.
async fn committed_extent(fs: &Filesystem<Disk>, inode: u64, offset: u64) -> RawFileExtentBody {
    let bytes = item(
        fs,
        objectid::FS_TREE,
        Key::new(inode, item_type::EXTENT_DATA, offset),
    )
    .await
    .unwrap();
    assert_eq!(
        RawFileExtentHead::ref_from_prefix(&bytes)
            .unwrap()
            .0
            .extent_type,
        1,
        "expected a regular extent"
    );
    *RawFileExtentBody::ref_from_prefix(&bytes[size_of::<RawFileExtentHead>()..])
        .unwrap()
        .0
}

/// The reference count of the data extent at `address` of `length` bytes;
/// `None` once the extent tree no longer holds it.
async fn extent_refs(fs: &Filesystem<Disk>, address: u64, length: u64) -> Option<u64> {
    item(
        fs,
        objectid::EXTENT_TREE,
        Key::new(address, item_type::EXTENT_ITEM, length),
    )
    .await
    .map(|bytes| u64::from_le_bytes(bytes[..8].try_into().unwrap()))
}

fn timespec(when: Timestamp) -> RawTimespec {
    RawTimespec {
        sec: (when.seconds as u64).into(),
        nsec: when.nanoseconds.into(),
    }
}

/// A logged inode item as `fill_inode_item` writes it.
fn inode(generation: u64, transid: u64, mode: u32, size: u64, nlink: u32) -> RawInodeItem {
    let mut raw = RawInodeItem::new_zeroed();
    raw.generation.set(generation);
    raw.transid.set(transid);
    raw.size.set(size);
    raw.nbytes.set(size.next_multiple_of(SECTOR as u64));
    raw.nlink.set(nlink);
    raw.mode.set(mode);
    raw.uid.set(1000);
    raw.gid.set(1000);
    let time = timespec(Timestamp {
        seconds: 1_700_000_200,
        nanoseconds: 0,
    });
    raw.atime = time;
    raw.ctime = time;
    raw.mtime = time;
    raw.otime = time;
    raw
}

fn inode_ref(entries: &[(u64, &str)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (index, name) in entries {
        let record = RawInodeRef {
            index: (*index).into(),
            name_len: (name.len() as u16).into(),
        };
        bytes.extend_from_slice(record.as_bytes());
        bytes.extend_from_slice(name.as_bytes());
    }
    bytes
}

fn entry(inode: u64, entry_type: u8, transid: u64, name: &str) -> Vec<u8> {
    let record = RawDirItem {
        location: RawKey {
            objectid: inode.into(),
            item_type: item_type::INODE_ITEM,
            offset: 0.into(),
        },
        transid: transid.into(),
        data_len: 0.into(),
        name_len: (name.len() as u16).into(),
        entry_type,
    };
    let mut bytes = Vec::from(record.as_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

fn xattr(name: &str, value: &[u8]) -> Vec<u8> {
    let record = RawDirItem {
        location: RawKey {
            objectid: 0.into(),
            item_type: 0,
            offset: 0.into(),
        },
        transid: 0.into(),
        data_len: (value.len() as u16).into(),
        name_len: (name.len() as u16).into(),
        // BTRFS_FT_XATTR.
        entry_type: 8,
    };
    let mut bytes = Vec::from(record.as_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(value);
    bytes
}

fn regular(generation: u64, disk: u64, disk_length: u64, offset: u64, length: u64) -> Vec<u8> {
    let mut head = RawFileExtentHead::new_zeroed();
    head.generation.set(generation);
    head.ram_bytes.set(disk_length);
    head.extent_type = 1;
    let mut body = RawFileExtentBody::new_zeroed();
    body.disk_bytenr.set(disk);
    body.disk_num_bytes.set(disk_length);
    body.offset.set(offset);
    body.num_bytes.set(length);
    let mut bytes = Vec::from(head.as_bytes());
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

fn inline(generation: u64, data: &[u8]) -> Vec<u8> {
    let mut head = RawFileExtentHead::new_zeroed();
    head.generation.set(generation);
    head.ram_bytes.set(data.len() as u64);
    let mut bytes = Vec::from(head.as_bytes());
    bytes.extend_from_slice(data);
    bytes
}

/// A log under construction: its items per subvolume, the data extents it
/// filled, and the allocation state it takes its blocks from.
struct Log<'a> {
    fs: &'a Filesystem<Disk>,
    space: Space,
    /// The transaction after the committed one, which wrote the log.
    generation: u64,
    trees: BTreeMap<u64, BTreeMap<Key, Vec<u8>>>,
    data: Vec<(u64, Vec<u8>)>,
}

impl<'a> Log<'a> {
    async fn new(fs: &'a Filesystem<Disk>) -> Self {
        Self {
            fs,
            space: Space::load(&fs.volume, fs.snapshot.root_tree)
                .await
                .unwrap(),
            generation: fs.superblock().generation() + 1,
            trees: BTreeMap::new(),
            data: Vec::new(),
        }
    }

    fn put(&mut self, tree: u64, key: Key, bytes: impl AsRef<[u8]>) {
        self.trees
            .entry(tree)
            .or_default()
            .insert(key, bytes.as_ref().to_vec());
    }

    fn inode(&mut self, number: u64, raw: RawInodeItem) {
        self.put(
            objectid::FS_TREE,
            Key::new(number, item_type::INODE_ITEM, 0),
            raw.as_bytes(),
        );
    }

    /// The checksums of `data` at `address`, as the log's checksum item.
    fn checksums(&mut self, tree: u64, address: u64, data: &[u8]) {
        let algorithm = self.fs.superblock().csum_type();
        let mut digests = Vec::new();
        for sector in data.as_chunks::<SECTOR>().0 {
            digests.extend_from_slice(algorithm.digest(sector).as_bytes());
        }
        self.put(tree, checksums::key(address), digests);
    }

    /// Writes `data` to a data extent no extent item records, as the
    /// uncommitted transaction did before its fsync, and logs its checksums.
    fn data(&mut self, tree: u64, data: &[u8]) -> u64 {
        assert!(data.len().is_multiple_of(SECTOR));
        let address = self.space.reserve_data(data.len() as u64).unwrap();
        self.checksums(tree, address, data);
        self.data.push((address, data.to_vec()));
        address
    }

    fn write_block(&self, file: &std::fs::File, address: u64, bytes: &[u8]) {
        for physical in self
            .fs
            .volume
            .chunks()
            .map(address, bytes.len() as u64)
            .unwrap()
        {
            file.write_all_at(bytes, physical).unwrap();
        }
    }

    /// Writes the log into `image` and points its superblocks at it.
    /// Returns the log's tree blocks.
    async fn write(mut self, image: &Image) -> BTreeSet<u64> {
        let file = image.named.reopen().unwrap();
        let superblock = self.fs.superblock();
        let root = self.fs.snapshot.root_tree;
        let template = Node::read(
            &self.fs.volume,
            root.bytenr,
            Expected {
                level: root.level,
                generation: root.generation,
                first_key: None,
            },
        )
        .await
        .unwrap();
        let mut blocks = BTreeSet::new();
        let mut leaf = |space: &mut Space, items: &BTreeMap<Key, Vec<u8>>| {
            let mut leaf = Leaf::empty(Header::new_tree(&template));
            for (key, bytes) in items {
                leaf.set(*key, bytes).unwrap();
            }
            let partitions = leaf.partitions();
            assert_eq!(partitions.len(), 1, "a test log fits one leaf");
            let first = *items.keys().next().unwrap();
            let address = space.allocate(objectid::TREE_LOG, 0, first).unwrap();
            blocks.insert(address);
            let bytes = leaf.encode(
                partitions[0].clone(),
                address,
                self.generation,
                objectid::TREE_LOG,
                superblock.csum_type(),
            );
            (address, bytes)
        };
        let mut roots = BTreeMap::new();
        let trees = core::mem::take(&mut self.trees);
        for (tree, items) in &trees {
            let (address, bytes) = leaf(&mut self.space, items);
            self.write_block(&file, address, &bytes);
            let mut record = vec![0; size_of::<RawRootItem>() + size_of::<RawRootTail>()];
            let head = RawRootItem::mut_from_prefix(&mut record).unwrap().0;
            head.generation.set(self.generation);
            head.bytenr.set(address);
            head.level = 0;
            head.refs.set(0);
            roots.insert(
                Key::new(objectid::TREE_LOG, item_type::ROOT_ITEM, *tree),
                record,
            );
        }
        let (log_root, bytes) = leaf(&mut self.space, &roots);
        self.write_block(&file, log_root, &bytes);
        for (address, data) in &self.data {
            self.write_block(&file, *address, data);
        }
        let length = file.metadata().unwrap().len();
        for offset in MIRROR_OFFSETS {
            if offset + 4096 > length {
                continue;
            }
            let mut bytes = [0_u8; 4096];
            file.read_exact_at(&mut bytes, offset).unwrap();
            let raw = RawSuperblock::mut_from_prefix(&mut bytes).unwrap().0;
            raw.log_root.set(log_root);
            raw.log_root_level = 0;
            let digest = superblock.csum_type().digest(&bytes[32..]);
            bytes[..digest.as_bytes().len()].copy_from_slice(digest.as_bytes());
            file.write_all_at(&bytes, offset).unwrap();
        }
        file.sync_all().unwrap();
        blocks
    }
}

/// A new file fsynced with its data, checksums and an xattr, a clone of a
/// committed extent, and the parent directory logged only as existing.
async fn new_files(check: bool) {
    let image = Image::new();
    let mut writer = image.writer().await;
    let (tree, root) = root_of(&writer).await;
    let original = pattern(2 * SECTOR, 3);
    let (docs, source) = {
        let mut tx = writer.join().await.unwrap();
        let edits = tx.edits();
        let docs = create(edits, tree, root, "文档", None).await;
        let source = create(edits, tree, root, "元.bin", Some(&original)).await;
        (docs, source)
    };
    writer.commit().await.unwrap();
    drop(writer);
    let generation = {
        let fs = image.reader().await;
        let generation = fs.superblock().generation();
        let mut log = Log::new(&fs).await;
        let transid = log.generation;
        let shared = committed_extent(&fs, source, 0).await;
        // The parent, logged with generation zero: it exists, nothing more.
        let mut parent = committed_inode(&fs, docs).await;
        parent.generation.set(0);
        parent.transid.set(transid);
        log.inode(docs, parent);

        let (file, clone) = (source + 1, source + 2);
        let content = pattern(3 * SECTOR, 9);
        let mut raw = inode(transid, transid, 0o100640, 10_000, 1);
        raw.nbytes.set(3 * SECTOR as u64);
        log.inode(file, raw);
        log.put(
            tree,
            Key::new(file, item_type::INODE_REF, docs),
            inode_ref(&[(2, "日本語ファイル.txt")]),
        );
        log.put(
            tree,
            Key::new(
                file,
                item_type::XATTR_ITEM,
                name_hash("user.작성자".as_bytes()),
            ),
            xattr("user.작성자", "한국어".as_bytes()),
        );
        let address = log.data(tree, &content);
        log.put(
            tree,
            Key::new(file, item_type::EXTENT_DATA, 0),
            regular(
                transid,
                address,
                content.len() as u64,
                0,
                content.len() as u64,
            ),
        );
        // A clone of the committed file before the fsync: its extent is in
        // the extent tree already, and gains a reference.
        log.inode(
            clone,
            inode(transid, transid, 0o100644, original.len() as u64, 1),
        );
        log.put(
            tree,
            Key::new(clone, item_type::INODE_REF, docs),
            inode_ref(&[(3, "クローン.bin")]),
        );
        log.put(
            tree,
            Key::new(clone, item_type::EXTENT_DATA, 0),
            regular(
                transid,
                shared.disk_bytenr.get(),
                shared.disk_num_bytes.get(),
                0,
                original.len() as u64,
            ),
        );
        log.checksums(tree, shared.disk_bytenr.get(), &original);
        log.write(&image).await;
        generation
    };

    // A read-only mount refuses the pending log; the writer replays it.
    assert!(matches!(
        Filesystem::mount(image.disk()).await,
        Err(Error::LogReplayRequired { .. })
    ));
    let writer = image.writer().await;
    let view = writer.snapshot().unwrap();
    assert_eq!(view.superblock().generation(), generation + 1);
    assert_eq!(view.superblock().log_tree(), None);
    drop(writer);
    // Mounting again finds nothing to replay and commits nothing.
    let again = image.writer().await;
    assert_eq!(
        again.snapshot().unwrap().superblock().generation(),
        generation + 1
    );
    drop(again);

    let fs = image.reader().await;
    let file = fs
        .resolve("/文档/日本語ファイル.txt".as_bytes())
        .await
        .unwrap();
    let metadata = *file.metadata();
    assert_eq!(
        (
            metadata.kind,
            metadata.size,
            metadata.nbytes,
            metadata.nlink
        ),
        (FileKind::Regular, 10_000, 3 * SECTOR as u64, 1)
    );
    let mut bytes = vec![0; 10_000];
    assert_eq!(fs.read(&file, 0, &mut bytes).await.unwrap(), 10_000);
    assert_eq!(bytes, pattern(3 * SECTOR, 9)[..10_000]);
    let mut value = [0; 64];
    let length = fs
        .xattr(&file, "user.작성자".as_bytes(), &mut value)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&value[..length], "한국어".as_bytes());

    let clone = fs.resolve("/文档/クローン.bin".as_bytes()).await.unwrap();
    let mut bytes = vec![0; original.len()];
    fs.read(&clone, 0, &mut bytes).await.unwrap();
    assert_eq!(bytes, original);
    let shared = committed_extent(&fs, source, 0).await;
    assert_eq!(
        extent_refs(&fs, shared.disk_bytenr.get(), shared.disk_num_bytes.get()).await,
        Some(2)
    );
    let docs = fs.resolve("/文档".as_bytes()).await.unwrap();
    let names = "日本語ファイル.txt".len() + "クローン.bin".len();
    assert_eq!(docs.metadata().size, 2 * names as u64);
    assert_eq!(docs.metadata().nlink, 1);
    image.check(check);
}

#[test]
fn replay_creates_fsynced_files_with_their_data_and_xattrs() {
    block_on(new_files(false));
}

/// A file renamed into another directory and fsynced after its old
/// directory was logged: the log's index range drops the old name.
async fn rename(check: bool) {
    let image = Image::new();
    let mut writer = image.writer().await;
    let (tree, root) = root_of(&writer).await;
    let content = pattern(SECTOR + 17, 5);
    let (source, target, file) = {
        let mut tx = writer.join().await.unwrap();
        let edits = tx.edits();
        let source = create(edits, tree, root, "源", None).await;
        let target = create(edits, tree, root, "目标", None).await;
        let file = create(edits, tree, source, "旧名.txt", Some(&content)).await;
        (source, target, file)
    };
    writer.commit().await.unwrap();
    drop(writer);
    {
        let fs = image.reader().await;
        let mut log = Log::new(&fs).await;
        let transid = log.generation;
        let mut directory = committed_inode(&fs, source).await;
        directory.transid.set(transid);
        log.inode(source, directory);
        // Index 2, the old name's, is no longer in the directory.
        log.put(
            tree,
            Key::new(source, item_type::DIR_LOG_INDEX, 2),
            2_u64.to_le_bytes(),
        );
        let mut raw = committed_inode(&fs, file).await;
        raw.transid.set(transid);
        log.inode(file, raw);
        log.put(
            tree,
            Key::new(file, item_type::INODE_REF, target),
            inode_ref(&[(2, "新しい名前.txt")]),
        );
        log.write(&image).await;
    }
    drop(image.writer().await);
    let fs = image.reader().await;
    let moved = fs.resolve("/目标/新しい名前.txt".as_bytes()).await.unwrap();
    assert_eq!(moved.number(), file);
    assert_eq!(moved.metadata().nlink, 1);
    let mut bytes = vec![0; content.len()];
    fs.read(&moved, 0, &mut bytes).await.unwrap();
    assert_eq!(bytes, content);
    assert!(fs.resolve("/源/旧名.txt".as_bytes()).await.is_err());
    let old = fs.resolve("/源".as_bytes()).await.unwrap();
    assert_eq!(old.metadata().size, 0);
    // The directory's deletions replay before its logged inode item is
    // copied, so the times the log holds win over the replay's own.
    assert_eq!(old.metadata().mtime, WHEN);
    image.check(check);
}

#[test]
fn replay_moves_a_renamed_file_and_drops_its_old_name() {
    block_on(rename(false));
}

/// A directory fsynced after an unlink, and a file fsynced after a new hard
/// link: the unlinked inode is deleted with its data, the link counted.
async fn links(check: bool) {
    let image = Image::new();
    let mut writer = image.writer().await;
    let (tree, root) = root_of(&writer).await;
    let (dir, removed, kept, original) = {
        let mut tx = writer.join().await.unwrap();
        let edits = tx.edits();
        let dir = create(edits, tree, root, "目录", None).await;
        let removed = create(edits, tree, dir, "删除.txt", Some(&pattern(2 * SECTOR, 1))).await;
        let kept = create(edits, tree, dir, "保留.txt", Some(b"kept")).await;
        let original = create(edits, tree, root, "原本.txt", Some(&pattern(2 * SECTOR, 2))).await;
        (dir, removed, kept, original)
    };
    writer.commit().await.unwrap();
    drop(writer);
    let freed = {
        let fs = image.reader().await;
        let freed = committed_extent(&fs, removed, 0).await;
        let mut log = Log::new(&fs).await;
        let transid = log.generation;
        let mut directory = committed_inode(&fs, dir).await;
        directory.transid.set(transid);
        log.inode(dir, directory);
        log.put(
            tree,
            Key::new(dir, item_type::DIR_LOG_INDEX, 0),
            u64::MAX.to_le_bytes(),
        );
        log.put(
            tree,
            Key::new(dir, item_type::DIR_INDEX, 3),
            entry(kept, ENTRY_FILE, transid, "保留.txt"),
        );
        let mut raw = committed_inode(&fs, original).await;
        raw.transid.set(transid);
        raw.nlink.set(2);
        log.inode(original, raw);
        log.put(
            tree,
            Key::new(original, item_type::INODE_REF, root),
            inode_ref(&[(3, "原本.txt"), (4, "링크.txt")]),
        );
        log.write(&image).await;
        freed
    };
    drop(image.writer().await);
    let fs = image.reader().await;
    assert!(fs.resolve("/目录/删除.txt".as_bytes()).await.is_err());
    assert!(fs.inode(tree, removed).await.is_err());
    assert_eq!(
        extent_refs(&fs, freed.disk_bytenr.get(), freed.disk_num_bytes.get()).await,
        None
    );
    assert_eq!(
        fs.resolve("/目录/保留.txt".as_bytes())
            .await
            .unwrap()
            .number(),
        kept
    );
    let link = fs.resolve("/링크.txt".as_bytes()).await.unwrap();
    assert_eq!(link.number(), original);
    assert_eq!(link.metadata().nlink, 2);
    assert_eq!(
        fs.resolve("/原本.txt".as_bytes()).await.unwrap().number(),
        original
    );
    image.check(check);
}

#[test]
fn replay_links_hard_links_and_deletes_unlinked_inodes() {
    block_on(links(false));
}

/// A directory fsynced after `rm -rf` of a subdirectory and an `rmdir`: the
/// link-count fixup deletes the whole subtree.
async fn directories(check: bool) {
    let image = Image::new();
    let mut writer = image.writer().await;
    let (tree, root) = root_of(&writer).await;
    let (parent, inner) = {
        let mut tx = writer.join().await.unwrap();
        let edits = tx.edits();
        let parent = create(edits, tree, root, "父", None).await;
        let child = create(edits, tree, parent, "子目录", None).await;
        create(edits, tree, child, "孫.txt", Some(&pattern(2 * SECTOR, 4))).await;
        create(edits, tree, parent, "空", None).await;
        (parent, [child, child + 1, child + 2])
    };
    writer.commit().await.unwrap();
    drop(writer);
    {
        let fs = image.reader().await;
        let mut log = Log::new(&fs).await;
        let mut directory = committed_inode(&fs, parent).await;
        directory.transid.set(log.generation);
        log.inode(parent, directory);
        log.put(
            tree,
            Key::new(parent, item_type::DIR_LOG_INDEX, 0),
            u64::MAX.to_le_bytes(),
        );
        log.write(&image).await;
    }
    drop(image.writer().await);
    let fs = image.reader().await;
    let emptied = fs.resolve("/父".as_bytes()).await.unwrap();
    assert_eq!(emptied.metadata().size, 0);
    let mut entries = 0;
    fs.read_dir(&emptied, DirPosition::START, |_| {
        entries += 1;
        ControlFlow::Continue(())
    })
    .await
    .unwrap();
    assert_eq!(entries, 0);
    for inode in inner {
        assert!(
            matches!(fs.inode(tree, inode).await, Err(Error::MissingInode { .. })),
            "inode {inode}"
        );
    }
    image.check(check);
}

#[test]
fn replay_deletes_a_removed_directory_tree() {
    block_on(directories(false));
}

/// An overwrite of part of a committed extent, a file rewritten to inline
/// data, and a file truncated before its fsync.
async fn extents(check: bool) {
    let image = Image::new();
    let mut writer = image.writer().await;
    let (tree, root) = root_of(&writer).await;
    let (old, small_old, long_old) = (
        pattern(3 * SECTOR, 11),
        pattern(2 * SECTOR, 12),
        pattern(3 * SECTOR, 13),
    );
    let (split, small, truncated) = {
        let mut tx = writer.join().await.unwrap();
        let edits = tx.edits();
        (
            create(edits, tree, root, "한국어.bin", Some(&old)).await,
            create(edits, tree, root, "小.txt", Some(&small_old)).await,
            create(edits, tree, root, "截断.bin", Some(&long_old)).await,
        )
    };
    writer.commit().await.unwrap();
    drop(writer);
    let new = pattern(SECTOR, 21);
    let short = pattern(100, 22);
    let (kept, dropped) = {
        let fs = image.reader().await;
        let kept = committed_extent(&fs, split, 0).await;
        let dropped = committed_extent(&fs, small, 0).await;
        let mut log = Log::new(&fs).await;
        let transid = log.generation;
        let mut raw = committed_inode(&fs, split).await;
        raw.transid.set(transid);
        log.inode(split, raw);
        let address = log.data(tree, &new);
        log.put(
            tree,
            Key::new(split, item_type::EXTENT_DATA, SECTOR as u64),
            regular(transid, address, SECTOR as u64, 0, SECTOR as u64),
        );

        let mut raw = committed_inode(&fs, small).await;
        raw.transid.set(transid);
        raw.size.set(short.len() as u64);
        log.inode(small, raw);
        log.put(
            tree,
            Key::new(small, item_type::EXTENT_DATA, 0),
            inline(transid, &short),
        );

        let mut raw = committed_inode(&fs, truncated).await;
        raw.transid.set(transid);
        raw.size.set(SECTOR as u64);
        log.inode(truncated, raw);
        log.write(&image).await;
        (kept, dropped)
    };
    drop(image.writer().await);
    let fs = image.reader().await;
    let read = async |path: &str| {
        let inode = fs.resolve(path.as_bytes()).await.unwrap();
        let mut bytes = vec![0; inode.metadata().size as usize];
        fs.read(&inode, 0, &mut bytes).await.unwrap();
        (bytes, *inode.metadata())
    };
    let (bytes, metadata) = read("/한국어.bin").await;
    let mut expected = old.clone();
    expected[SECTOR..2 * SECTOR].copy_from_slice(&new);
    assert_eq!(bytes, expected);
    assert_eq!(metadata.nbytes, 3 * SECTOR as u64);
    // Both retained halves of the committed extent reference it.
    assert_eq!(
        extent_refs(&fs, kept.disk_bytenr.get(), kept.disk_num_bytes.get()).await,
        Some(2)
    );
    let (bytes, metadata) = read("/小.txt").await;
    assert_eq!(bytes, short);
    assert_eq!(metadata.nbytes, short.len() as u64);
    assert_eq!(
        extent_refs(&fs, dropped.disk_bytenr.get(), dropped.disk_num_bytes.get()).await,
        None
    );
    let (bytes, metadata) = read("/截断.bin").await;
    assert_eq!(bytes, long_old[..SECTOR]);
    assert_eq!(metadata.nbytes, SECTOR as u64);
    image.check(check);
}

#[test]
fn replay_splits_drops_and_replaces_extents() {
    block_on(extents(false));
}

/// An inode fsynced after one xattr was removed and another set.
async fn xattrs(check: bool) {
    let image = Image::new();
    let mut writer = image.writer().await;
    let (tree, root) = root_of(&writer).await;
    let file = {
        let mut tx = writer.join().await.unwrap();
        let edits = tx.edits();
        let file = create(edits, tree, root, "属性.txt", Some(b"x")).await;
        set_xattr(edits, tree, file, "user.删除", b"gone").await;
        set_xattr(edits, tree, file, "user.保持", b"kept").await;
        file
    };
    writer.commit().await.unwrap();
    drop(writer);
    {
        let fs = image.reader().await;
        let mut log = Log::new(&fs).await;
        let mut raw = committed_inode(&fs, file).await;
        raw.transid.set(log.generation);
        log.inode(file, raw);
        for (name, value) in [("user.保持", "kept"), ("user.新しい", "새")] {
            log.put(
                tree,
                Key::new(file, item_type::XATTR_ITEM, name_hash(name.as_bytes())),
                xattr(name, value.as_bytes()),
            );
        }
        log.write(&image).await;
    }
    drop(image.writer().await);
    let fs = image.reader().await;
    let inode = fs.resolve("/属性.txt".as_bytes()).await.unwrap();
    let mut names = Vec::new();
    fs.list_xattrs(&inode, |name| {
        names.push(name.to_vec());
        ControlFlow::Continue(())
    })
    .await
    .unwrap();
    names.sort();
    let mut expected = vec![
        "user.保持".as_bytes().to_vec(),
        "user.新しい".as_bytes().to_vec(),
    ];
    expected.sort();
    assert_eq!(names, expected);
    let mut value = [0; 16];
    let length = fs
        .xattr(&inode, "user.新しい".as_bytes(), &mut value)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&value[..length], "새".as_bytes());
    image.check(check);
}

#[test]
fn replay_deletes_xattrs_the_log_no_longer_holds() {
    block_on(xattrs(false));
}

#[test]
fn an_unimplemented_log_item_refuses_the_mount_and_changes_nothing() {
    block_on(async {
        let image = Image::new();
        let generation = {
            let fs = image.reader().await;
            let mut log = Log::new(&fs).await;
            let root = fs.root().await.unwrap().number();
            let mut raw = committed_inode(&fs, root).await;
            raw.transid.set(log.generation);
            log.inode(root, raw);
            // BTRFS_VERITY_DESC_ITEM_KEY, which needs fs-verity.
            log.put(objectid::FS_TREE, Key::new(root, 36, 0), [0; 8]);
            log.write(&image).await;
            fs.superblock().generation()
        };
        let refused = mount_writer(image.disk()).await;
        assert!(
            matches!(
                refused,
                Err(WriteError::LogReplay(LogReplayError::UnsupportedItem {
                    subvolume: objectid::FS_TREE,
                    key,
                })) if key.item_type == 36
            ),
            "{:?}",
            refused.err()
        );
        let fs = Filesystem::mount_committed(image.disk()).await.unwrap();
        assert_eq!(fs.superblock().generation(), generation);
        assert!(fs.superblock().log_tree().is_some());
    });
}

#[test]
fn a_log_of_a_missing_subvolume_is_skipped_and_cleared() {
    block_on(async {
        let image = Image::new();
        let generation = {
            let fs = image.reader().await;
            let mut log = Log::new(&fs).await;
            let transid = log.generation;
            log.put(
                objectid::FIRST_FREE + 44,
                Key::new(objectid::FIRST_FREE, item_type::INODE_ITEM, 0),
                inode(transid, transid, 0o040755, 0, 1).as_bytes(),
            );
            log.write(&image).await;
            fs.superblock().generation()
        };
        drop(image.writer().await);
        let fs = image.reader().await;
        assert_eq!(fs.superblock().generation(), generation + 1);
        assert_eq!(fs.superblock().log_tree(), None);
    });
}

#[test]
fn replay_never_allocates_over_a_log_block() {
    block_on(async {
        let image = Image::new();
        let blocks = {
            let fs = image.reader().await;
            let mut log = Log::new(&fs).await;
            let transid = log.generation;
            let root = fs.root().await.unwrap().number();
            for index in 0..4 {
                let number = objectid::FIRST_FREE + 1 + index;
                let name = std::format!("ファイル{index}");
                log.inode(number, inode(transid, transid, 0o100644, 0, 1));
                log.put(
                    objectid::FS_TREE,
                    Key::new(number, item_type::INODE_REF, root),
                    inode_ref(&[(2 + index, &name)]),
                );
            }
            log.write(&image).await
        };
        let fs = Filesystem::mount_committed(image.disk()).await.unwrap();
        let allocation = Allocation::load(&fs.volume, fs.snapshot.root_tree)
            .await
            .unwrap();
        let mut retirements = Retirements::default();
        let mut transaction = Transaction::new(
            fs.volume.view(&fs.snapshot.mapping),
            fs.snapshot.clone(),
            &mut retirements,
            &allocation,
        )
        .await
        .unwrap();
        let (bytenr, level) = fs.superblock().log_tree().unwrap();
        transaction
            .replay_log(PendingLog { bytenr, level }, MOUNTED)
            .await
            .unwrap();
        let allocated: BTreeSet<u64> = transaction
            .space
            .metadata()
            .map(|(address, _)| address)
            .collect();
        assert!(!allocated.is_empty());
        assert!(allocated.is_disjoint(&blocks), "{allocated:?} {blocks:?}");
    });
}

/// A directory fsynced after a hard link into it replaced a name, and after
/// `mkdir`; a file fsynced with a link its extended reference records.
async fn entries(check: bool) {
    let image = Image::new();
    let mut writer = image.writer().await;
    let (tree, root) = root_of(&writer).await;
    let (shelf, linked, replaced, extended) = {
        let mut tx = writer.join().await.unwrap();
        let edits = tx.edits();
        let shelf = create(edits, tree, root, "书架", None).await;
        let linked = create(edits, tree, root, "本.txt", Some(b"book")).await;
        let replaced = create(edits, tree, shelf, "棚.txt", Some(&pattern(2 * SECTOR, 6))).await;
        let extended = create(edits, tree, root, "확장.txt", Some(b"ext")).await;
        (shelf, linked, replaced, extended)
    };
    writer.commit().await.unwrap();
    drop(writer);
    let fs = image.reader().await;
    // The extended reference feature mkfs enables by default.
    assert_ne!(fs.superblock().incompat_flags() & (1 << 6), 0);
    let created = extended + 1;
    {
        let mut log = Log::new(&fs).await;
        let transid = log.generation;
        let mut directory = committed_inode(&fs, shelf).await;
        directory.transid.set(transid);
        log.inode(shelf, directory);
        // The log is authoritative from index 3, so the old name at index 2
        // survives the deletions and conflicts with the logged one.
        log.put(
            tree,
            Key::new(shelf, item_type::DIR_LOG_INDEX, 3),
            u64::MAX.to_le_bytes(),
        );
        // `ln -f 本.txt 书架/棚.txt` took index 3; `mkdir 书架/新しい` 4.
        log.put(
            tree,
            Key::new(shelf, item_type::DIR_INDEX, 3),
            entry(linked, ENTRY_FILE, transid, "棚.txt"),
        );
        log.inode(created, inode(transid, transid, 0o040755, 0, 1));
        log.put(
            tree,
            Key::new(shelf, item_type::DIR_INDEX, 4),
            entry(created, ENTRY_DIRECTORY, transid, "新しい"),
        );
        log.put(
            tree,
            Key::new(created, item_type::INODE_REF, shelf),
            inode_ref(&[(4, "新しい")]),
        );
        // A logged inode carries every reference it has.
        let mut raw = committed_inode(&fs, extended).await;
        raw.transid.set(transid);
        raw.nlink.set(2);
        log.inode(extended, raw);
        log.put(
            tree,
            Key::new(extended, item_type::INODE_REF, root),
            inode_ref(&[(4, "확장.txt")]),
        );
        let name = "extref名";
        let mut record = Vec::from(
            RawInodeExtref {
                parent: root.into(),
                index: 6.into(),
                name_len: (name.len() as u16).into(),
            }
            .as_bytes(),
        );
        record.extend_from_slice(name.as_bytes());
        log.put(
            tree,
            Key::new(
                extended,
                item_type::INODE_EXTREF,
                inode_ref_hash(root, name.as_bytes()),
            ),
            record,
        );
        log.write(&image).await;
    }
    drop(fs);
    drop(image.writer().await);
    let fs = image.reader().await;
    let link = fs.resolve("/书架/棚.txt".as_bytes()).await.unwrap();
    assert_eq!(link.number(), linked);
    assert_eq!(link.metadata().nlink, 2);
    assert!(matches!(
        fs.inode(tree, replaced).await,
        Err(Error::MissingInode { .. })
    ));
    let made = fs.resolve("/书架/新しい".as_bytes()).await.unwrap();
    assert_eq!(
        (made.number(), made.kind(), made.metadata().nlink),
        (created, FileKind::Directory, 1)
    );
    let shelf = fs.resolve("/书架".as_bytes()).await.unwrap();
    assert_eq!(
        shelf.metadata().size,
        2 * ("棚.txt".len() + "新しい".len()) as u64
    );
    // The replaced name was unlinked after the directory's logged inode was
    // copied, at the time the writer mounted.
    assert_eq!(shelf.metadata().mtime, MOUNTED);
    let other = fs.resolve("/extref名".as_bytes()).await.unwrap();
    assert_eq!(other.number(), extended);
    assert_eq!(other.metadata().nlink, 2);
    assert_eq!(
        fs.resolve("/확장.txt".as_bytes()).await.unwrap().number(),
        extended
    );
    image.check(check);
}

#[test]
fn replay_links_directory_entries_and_extended_references() {
    block_on(entries(false));
}

/// A setuid regular file with a `security.*` attribute, a character device
/// node and a symlink logged by Linux: replay preserves their mode, owner,
/// device number, target and security attribute intact.
async fn authority(check: bool) {
    let image = Image::new();
    let fs = image.reader().await;
    let root = fs.root().await.unwrap().number();
    let tree = objectid::FS_TREE;
    let (suid, device, link) = (
        objectid::FIRST_FREE + 1,
        objectid::FIRST_FREE + 2,
        objectid::FIRST_FREE + 3,
    );
    let label = b"system_u:object_r:helios_t:s0";
    let target = "日本語ターゲット";
    {
        let mut log = Log::new(&fs).await;
        let transid = log.generation;
        let mut raw = inode(transid, transid, 0o104755, 0, 1);
        raw.uid.set(0);
        raw.gid.set(42);
        log.inode(suid, raw);
        log.put(
            tree,
            Key::new(suid, item_type::INODE_REF, root),
            inode_ref(&[(2, "suid")]),
        );
        log.put(
            tree,
            Key::new(suid, item_type::XATTR_ITEM, name_hash(b"security.helios")),
            xattr("security.helios", label),
        );

        let mut raw = inode(transid, transid, 0o020660, 0, 1);
        raw.uid.set(0);
        raw.gid.set(6);
        raw.rdev.set(0x0103);
        log.inode(device, raw);
        log.put(
            tree,
            Key::new(device, item_type::INODE_REF, root),
            inode_ref(&[(3, "设备")]),
        );

        let mut raw = inode(transid, transid, 0o120777, target.len() as u64, 1);
        raw.nbytes.set(target.len() as u64);
        log.inode(link, raw);
        log.put(
            tree,
            Key::new(link, item_type::INODE_REF, root),
            inode_ref(&[(4, "リンク")]),
        );
        log.put(
            tree,
            Key::new(link, item_type::EXTENT_DATA, 0),
            inline(transid, target.as_bytes()),
        );
        log.write(&image).await;
    }
    drop(fs);
    drop(image.writer().await);
    let fs = image.reader().await;

    let suid_inode = fs.resolve(b"/suid").await.unwrap();
    let meta = *suid_inode.metadata();
    assert_eq!(
        (meta.kind, meta.permissions, meta.uid, meta.gid, meta.nlink),
        (FileKind::Regular, 0o4755, 0, 42, 1)
    );
    let mut value = [0; 64];
    let length = fs
        .xattr(&suid_inode, b"security.helios", &mut value)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&value[..length], label);

    let dev_inode = fs.resolve("/设备".as_bytes()).await.unwrap();
    let meta = *dev_inode.metadata();
    assert_eq!(
        (meta.kind, meta.permissions, meta.uid, meta.gid, meta.rdev),
        (FileKind::CharDevice, 0o0660, 0, 6, 0x0103)
    );

    let root_dir = fs.root().await.unwrap();
    let link_inode = fs
        .lookup(&root_dir, "リンク".as_bytes())
        .await
        .unwrap()
        .unwrap();
    let meta = *link_inode.metadata();
    assert_eq!(
        (meta.kind, meta.permissions, meta.size, meta.nbytes),
        (
            FileKind::Symlink,
            0o0777,
            target.len() as u64,
            target.len() as u64
        )
    );
    let mut read_target = [0; 64];
    let target_len = fs.read_link(&link_inode, &mut read_target).await.unwrap();
    assert_eq!(&read_target[..target_len], target.as_bytes());
    image.check(check);
}

#[test]
fn replay_preserves_linux_authority_devices_and_symlinks() {
    block_on(authority(false));
}

#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn replayed_images_pass_btrfs_check() {
    block_on(async {
        new_files(true).await;
        rename(true).await;
        links(true).await;
        directories(true).await;
        extents(true).await;
        xattrs(true).await;
        entries(true).await;
        authority(true).await;
    });
}
