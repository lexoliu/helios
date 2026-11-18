extern crate std;

#[cfg(feature = "write")]
mod xattr;

use alloc::collections::BTreeMap;
#[cfg(feature = "write")]
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
#[cfg(feature = "write")]
use core::num::NonZeroUsize;
use std::fs::File;
use std::os::unix::fs::FileExt;

use btrfs_disk::tree::TreeBlock;
use ferrosys::btrfs::{FormatOptions, format_to};
use ferrosys::{Metadata, Timestamp, TreeBuilder};
use futures_lite::future::block_on;
#[cfg(feature = "write")]
use helios_hal::fs::RenameMode;
use helios_hal::io::{IoError, IoResult};

use super::Filesystem;
use crate::source::BlockSource;
#[cfg(feature = "write")]
use crate::transaction::file::{DEFAULT_MAX_INLINE, WriteOptions};
use crate::tree::edit::{Header, Leaf};
use crate::tree::write::{MetadataAllocator, Trees, WriteError};
use crate::tree::{Expected, Node, TreeRoot};
use crate::{ChecksumType, Key};

pub(super) struct Disk(pub(super) File);

#[cfg(feature = "write")]
struct FaultDisk {
    disk: Disk,
    fail_at: usize,
    operations: std::sync::Arc<std::sync::Mutex<Vec<(bool, u64)>>>,
    poisoned: std::sync::atomic::AtomicBool,
}

#[cfg(feature = "write")]
impl FaultDisk {
    fn operation(&self, commit: bool, block: u64) -> IoResult<()> {
        use std::sync::atomic::Ordering;
        if self.poisoned.load(Ordering::SeqCst) {
            return Err(IoError::DeviceFault);
        }
        let mut operations = self.operations.lock().unwrap();
        operations.push((commit, block));
        if operations.len() == self.fail_at {
            self.poisoned.store(true, Ordering::SeqCst);
            return Err(IoError::DeviceFault);
        }
        Ok(())
    }
}

#[cfg(feature = "write")]
impl BlockSource for FaultDisk {
    fn size_bytes(&self) -> u64 {
        self.disk.size_bytes()
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> IoResult<()> {
        self.disk.read_at(offset, buf).await
    }
}

#[cfg(feature = "write")]
impl crate::source::BlockSink for FaultDisk {
    async fn stage(&self, block: u64, bytes: &[u8; 4096]) -> IoResult<()> {
        self.operation(false, block)?;
        self.disk.stage(block, bytes).await
    }

    async fn commit(&self, block: u64, bytes: &[u8; 4096]) -> IoResult<()> {
        self.operation(true, block)?;
        self.disk.commit(block, bytes).await
    }
}

/// The fixtures' root records all carry valid UUIDs, so no commit asks for
/// one.
#[cfg(feature = "write")]
pub(super) struct FixtureUuids;

#[cfg(feature = "write")]
impl super::writer::RootUuids for FixtureUuids {
    fn next_uuid(&mut self) -> [u8; 16] {
        panic!("formatted fixtures carry valid root UUIDs")
    }
}

/// A writer over `source`, committing one block write at a time.
#[cfg(feature = "write")]
pub(super) async fn mount_writer<S: crate::source::BlockSink>(
    source: S,
) -> Result<super::writer::Writer<S, FixtureUuids>, WriteError> {
    super::writer::Writer::mount(source, FixtureUuids, NonZeroUsize::MIN).await
}

/// The committed allocation state of `fs`'s generation, loaded from disk.
#[cfg(feature = "write")]
async fn allocation_of<S: BlockSource>(fs: &Filesystem<S>) -> crate::accounting::Allocation {
    crate::accounting::Allocation::load(&fs.volume, fs.snapshot.root_tree)
        .await
        .unwrap()
}

#[cfg(feature = "write")]
async fn change_root_mode<S: crate::source::BlockSink>(
    fs: &Filesystem<S>,
) -> Result<(TreeRoot, crate::transaction::Committed), WriteError> {
    change_root_mode_with_retirements(fs, &mut crate::retirement::Retirements::default()).await
}

#[cfg(feature = "write")]
async fn change_root_mode_with_retirements<S: crate::source::BlockSink>(
    fs: &Filesystem<S>,
    retirements: &mut crate::retirement::Retirements,
) -> Result<(TreeRoot, crate::transaction::Committed), WriteError> {
    use crate::transaction::Transaction;

    let mut transaction = Transaction::new(
        fs.volume.view(&fs.snapshot.mapping),
        fs.snapshot.clone(),
        retirements,
        &allocation_of(fs).await,
    )
    .await?;
    let root = edit_root_mode(fs, &mut transaction).await?;
    let committed = transaction
        .commit(
            retirements,
            || panic!("formatted fixture already has valid UUIDs"),
            std::thread::available_parallelism().unwrap(),
        )
        .await?;
    Ok((root, committed))
}

#[cfg(feature = "write")]
async fn edit_root_mode<S: BlockSource, T>(
    fs: &Filesystem<S>,
    transaction: &mut crate::transaction::Transaction<T>,
) -> Result<TreeRoot, WriteError> {
    use crate::ondisk::{RawInodeItem, item_type, objectid, view};
    use zerocopy::IntoBytes;
    let inode = fs.resolve(b"/").await?;
    let root = transaction.roots.get(objectid::FS_TREE)?;
    let key = Key::new(inode.number(), item_type::INODE_ITEM, 0);
    let bytes = transaction.trees.get(&fs.volume, root, key).await?.unwrap();
    let mut raw = *view::<RawInodeItem>(&bytes).unwrap();
    raw.mode.set((raw.mode.get() & !0o777) | 0o700);
    raw.transid.set(fs.superblock().generation() + 1);
    let root = transaction
        .trees
        .apply(
            &fs.volume,
            root,
            key,
            Some(raw.as_bytes()),
            &mut transaction.space,
        )
        .await?;
    transaction.roots.set(root)?;
    Ok(root)
}

#[cfg(feature = "write")]
#[test]
fn commits_advance_generations_and_a_failed_publication_poisons() {
    use crate::source::BlockSink;

    block_on(async {
        let (_, image) = filesystem_image();
        let mut writer = mount_writer(Disk(image)).await.unwrap();
        let old = writer.snapshot().unwrap();
        let generation = old.superblock().generation();
        for next in generation + 1..=generation + 2 {
            let snapshot = writer.snapshot().unwrap();
            let mut transaction = writer.join().await.unwrap();
            edit_root_mode(&snapshot, transaction.edits())
                .await
                .unwrap();
            transaction.commit().await.unwrap();
            assert_eq!(writer.snapshot().unwrap().superblock().generation(), next);
            assert_eq!(
                writer
                    .snapshot()
                    .unwrap()
                    .root()
                    .await
                    .unwrap()
                    .metadata()
                    .permissions,
                0o700
            );
            assert_eq!(old.root().await.unwrap().metadata().permissions, 0o755);
        }

        // Fail exactly the first superblock operation after staging completes.
        struct FailCommit(Disk);
        impl BlockSource for FailCommit {
            fn size_bytes(&self) -> u64 {
                self.0.size_bytes()
            }
            async fn read_at(&self, offset: u64, buf: &mut [u8]) -> IoResult<()> {
                self.0.read_at(offset, buf).await
            }
        }
        impl BlockSink for FailCommit {
            async fn stage(&self, block: u64, bytes: &[u8; 4096]) -> IoResult<()> {
                self.0.stage(block, bytes).await
            }
            async fn commit(&self, _: u64, _: &[u8; 4096]) -> IoResult<()> {
                Err(IoError::DeviceFault)
            }
        }
        let (_, image) = filesystem_image();
        let mut writer = mount_writer(FailCommit(Disk(image))).await.unwrap();
        let old = writer.snapshot().unwrap();
        let mut transaction = writer.join().await.unwrap();
        edit_root_mode(&old, transaction.edits()).await.unwrap();
        assert!(transaction.commit().await.is_err());
        assert!(matches!(writer.snapshot(), Err(WriteError::WriterPoisoned)));
        assert!(matches!(
            writer.join().await,
            Err(WriteError::WriterPoisoned)
        ));
        assert_eq!(old.root().await.unwrap().metadata().permissions, 0o755);
    });
}

#[cfg(feature = "write")]
#[test]
fn reflink_shares_allocations_and_isolates_later_mutations() {
    block_on(exercise_reflink_matrix(|_, _| {}));
}

#[cfg(feature = "write")]
#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn reflink_passes_btrfs_check_and_restore() {
    block_on(exercise_reflink_matrix(assert_reflink_image));
}

#[cfg(feature = "write")]
fn assert_reflink_image(path: &std::path::Path, expected: &BTreeMap<&str, Vec<u8>>) {
    assert_btrfs_check(path);
    let directory = tempfile::tempdir().unwrap();
    let output = std::process::Command::new("btrfs")
        .arg("restore")
        .arg(path)
        .arg(directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "btrfs restore failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for (name, bytes) in expected {
        assert_eq!(std::fs::read(directory.path().join(name)).unwrap(), *bytes);
    }
    assert_eq!(
        std::fs::read_dir(directory.path()).unwrap().count(),
        expected.len()
    );
}

#[cfg(feature = "write")]
async fn exercise_reflink_matrix(
    mut verify: impl FnMut(&std::path::Path, &BTreeMap<&str, Vec<u8>>),
) {
    use crate::compress::write::{Lzo, Plain, Zlib, Zstd};
    use miniz_oxide::deflate::CompressionLevel;
    for no_holes in [true, false] {
        exercise_reflink(no_holes, Plain, &mut verify).await;
        exercise_reflink(
            no_holes,
            Zlib::new(CompressionLevel::DefaultLevel),
            &mut verify,
        )
        .await;
        exercise_reflink(no_holes, Lzo::new(), &mut verify).await;
        exercise_reflink(no_holes, Zstd::new(3).unwrap(), &mut verify).await;
    }
}

#[cfg(feature = "write")]
async fn assert_reflink_contents<S: BlockSource>(
    fs: &Filesystem<S>,
    expected: &BTreeMap<&str, Vec<u8>>,
) {
    let root = fs.root().await.unwrap();
    for (&name, bytes) in expected {
        let inode = fs.lookup(&root, name.as_bytes()).await.unwrap().unwrap();
        assert_eq!(inode.metadata().size, bytes.len() as u64);
        let mut output = vec![0; bytes.len()];
        assert_eq!(fs.read(&inode, 0, &mut output).await.unwrap(), bytes.len());
        assert_eq!(&output, bytes, "file {name}");
    }
}

#[cfg(feature = "write")]
async fn disk_allocation<S: BlockSource>(
    fs: &Filesystem<S>,
    inode: &super::Inode,
    offset: u64,
) -> (u64, u64) {
    use btrfs_disk::items::{FileExtentBody, FileExtentItem};
    let volume = fs.volume.view(&inode.snapshot.mapping);
    let cursor = crate::tree::Cursor::floor(
        &volume,
        inode.root,
        Key::new(
            inode.number(),
            crate::ondisk::item_type::EXTENT_DATA,
            offset,
        ),
    )
    .await
    .unwrap();
    let FileExtentBody::Regular {
        disk_bytenr,
        disk_num_bytes,
        ..
    } = FileExtentItem::parse(cursor.item().unwrap()).unwrap().body
    else {
        panic!("regular extent expected");
    };
    (disk_bytenr, disk_num_bytes)
}

#[cfg(feature = "write")]
async fn exercise_reflink<E: crate::compress::write::Encoder>(
    no_holes: bool,
    mut encoder: E,
    mut verify: impl FnMut(&std::path::Path, &BTreeMap<&str, Vec<u8>>),
) {
    use crate::transaction::namespace::Name;
    use crate::transaction::reflink::{CloneRange, FileId};
    let timestamp = Timestamp::from_secs(1_700_000_000);
    let named = tempfile::NamedTempFile::new().unwrap();
    let mut builder = TreeBuilder::new().root(Metadata::new(0o755, timestamp));
    let mut expected = BTreeMap::new();
    for name in [
        "source",
        "clone",
        "small",
        "small-clone",
        "tail",
        "prealloc-clone",
    ] {
        builder = builder.file(
            alloc::format!("/{name}"),
            Vec::new(),
            Metadata::new(0o644, timestamp),
        );
        expected.insert(name, Vec::new());
    }
    let mut options = FormatOptions::new([0x53; 16], timestamp);
    if !no_holes {
        options.plan.incompat_flags = options
            .plan
            .incompat_flags
            .without(ferrosys::btrfs::ondisk::IncompatFlags::NO_HOLES);
        options.plan.compat_ro_flags = options
            .plan
            .compat_ro_flags
            .without(ferrosys::btrfs::ondisk::CompatRoFlags::BLOCK_GROUP_TREE);
    }
    let (formatted, _) = filesystem_image_options(builder, named.reopen().unwrap(), options);
    let parent = formatted.root().await.unwrap();
    let last = crate::tree::Cursor::floor(
        &formatted.volume,
        parent.root,
        Key::new(crate::ondisk::objectid::LAST_FREE, u8::MAX, u64::MAX),
    )
    .await
    .unwrap()
    .key()
    .unwrap()
    .objectid;
    let index = crate::tree::Cursor::floor(
        &formatted.volume,
        parent.root,
        Key::new(
            parent.number(),
            crate::ondisk::item_type::DIR_INDEX,
            u64::MAX,
        ),
    )
    .await
    .unwrap()
    .key()
    .unwrap()
    .offset
        + 1;
    let mut external =
        btrfs_transaction::filesystem::Filesystem::open(named.reopen().unwrap()).unwrap();
    let mut transaction =
        btrfs_transaction::transaction::Transaction::start(&mut external).unwrap();
    let timestamp = btrfs_disk::items::Timespec {
        sec: 1_700_000_000,
        nsec: 0,
    };
    let mut attributes = btrfs_transaction::inode::InodeArgs::new(transaction.transid, 0o100644)
        .with_uniform_time(timestamp);
    attributes.size = 32768;
    transaction
        .create_inode(&mut external, parent.tree(), last + 1, &attributes)
        .unwrap();
    transaction
        .link_dir_entry(
            &mut external,
            parent.tree(),
            parent.number(),
            last + 1,
            b"prealloc",
            1,
            index,
            timestamp,
        )
        .unwrap();
    let allocation = transaction
        .reserve_data_extent(&mut external, 32768, parent.tree(), last + 1, 0)
        .unwrap();
    let mapping = btrfs_disk::items::FileExtentItem::to_bytes_regular(
        transaction.transid,
        32768,
        btrfs_disk::items::CompressionType::None,
        true,
        allocation,
        32768,
        0,
        32768,
    );
    transaction
        .insert_file_extent(&mut external, parent.tree(), last + 1, 0, &mapping)
        .unwrap();
    transaction
        .update_inode_nbytes(&mut external, parent.tree(), last + 1, 32768)
        .unwrap();
    transaction.commit(&mut external).unwrap();
    drop(external);
    drop(formatted);
    expected.insert("prealloc", vec![0; 32768]);
    let mut writer = mount_writer(Disk(named.reopen().unwrap())).await.unwrap();
    let base = writer.snapshot().unwrap();
    let root = base.root().await.unwrap();
    let mut ids = BTreeMap::new();
    for name in expected.keys() {
        let inode = base.lookup(&root, name.as_bytes()).await.unwrap().unwrap();
        ids.insert(
            *name,
            FileId {
                tree: inode.tree(),
                inode: inode.number(),
            },
        );
    }
    let concurrency = std::thread::available_parallelism().unwrap();
    let when = crate::Timestamp {
        seconds: 1_700_000_100,
        nanoseconds: 123,
    };
    expected.insert(
        "source",
        (0..32891).map(|index| (index % 251) as u8).collect(),
    );
    expected.insert("clone", vec![0xaa; 24576]);
    expected.insert("small", "中文・日本語・한국어".as_bytes().to_vec());
    expected.insert("tail", vec![0x5c; 8192]);
    let mut tx = writer.join().await.unwrap();
    for (&name, bytes) in &expected {
        if name == "prealloc" {
            continue;
        }
        let id = ids[name];
        tx.edits()
            .write_file(
                id.tree,
                id.inode,
                0,
                bytes,
                WriteOptions {
                    when,
                    concurrency,
                    encoder: &mut encoder,
                    max_inline: DEFAULT_MAX_INLINE,
                },
            )
            .await
            .unwrap();
    }
    let tail = ids["tail"];
    tx.edits()
        .truncate_file(
            tail.tree,
            tail.inode,
            17,
            WriteOptions::plain(when, concurrency),
        )
        .await
        .unwrap();
    expected.get_mut("tail").unwrap().truncate(17);
    tx.commit().await.unwrap();
    verify(named.path(), &expected);
    assert_reflink_contents(&writer.snapshot().unwrap(), &expected).await;
    let initial = writer.snapshot().unwrap();
    let original_inode = initial.resolve(b"/source").await.unwrap();
    let original_bytes = expected["source"].clone();
    let original_allocation = disk_allocation(&initial, &original_inode, 0).await;
    let old_tail = initial.resolve(b"/tail").await.unwrap();
    let tail_allocation = disk_allocation(&initial, &old_tail, 0).await;
    let mut tx = writer.join().await.unwrap();
    let edits = tx.edits();
    for (source_offset, destination_offset, length, expected_error) in [
        (0, 4096, 8192, WriteError::CloneOverlap),
        (1, 65536, 4096, WriteError::CloneAlignment),
        (0, 65537, 4096, WriteError::CloneAlignment),
        (0, 65536, 17, WriteError::CloneAlignment),
        (32768, 65536, 4096, WriteError::CloneSourceRange),
    ] {
        let error = edits
            .clone_range(
                CloneRange {
                    source: ids["source"],
                    destination: ids["source"],
                    source_offset,
                    destination_offset,
                    length,
                },
                WriteOptions::plain(when, concurrency),
            )
            .await
            .unwrap_err();
        assert_eq!(
            core::mem::discriminant(&error),
            core::mem::discriminant(&expected_error)
        );
    }
    let mut unchanged = vec![0; original_bytes.len()];
    edits
        .read_file(ids["source"].tree, ids["source"].inode, 0, &mut unchanged)
        .await
        .unwrap();
    assert_eq!(unchanged, original_bytes);
    for (source, destination, source_offset, destination_offset, length) in [
        ("source", "clone", 8192, 0, 16384),
        ("prealloc", "prealloc-clone", 4096, 0, 16384),
        ("source", "clone", 32768, 65536, 123),
        ("source", "source", 0, 65536, 8192),
        ("small", "small-clone", 0, 0, expected["small"].len()),
        ("tail", "tail", 0, 4096, 17),
    ] {
        let copied = expected[source][source_offset..source_offset + length].to_vec();
        edits
            .clone_range(
                CloneRange {
                    source: ids[source],
                    destination: ids[destination],
                    source_offset: source_offset as u64,
                    destination_offset: destination_offset as u64,
                    length: length as u64,
                },
                WriteOptions::plain(when, concurrency),
            )
            .await
            .unwrap();
        let bytes = expected.get_mut(destination).unwrap();
        bytes.resize(bytes.len().max(destination_offset + length), 0);
        bytes[destination_offset..destination_offset + length].copy_from_slice(&copied);
    }
    tx.commit().await.unwrap();
    verify(named.path(), &expected);
    assert_reflink_contents(&writer.snapshot().unwrap(), &expected).await;
    let cloned = writer.snapshot().unwrap();
    let cloned_inode = cloned.resolve(b"/clone").await.unwrap();
    assert_eq!(
        disk_allocation(&cloned, &cloned_inode, 0).await,
        original_allocation
    );
    assert_eq!(
        disk_allocation(&cloned, &cloned_inode, 65536).await,
        original_allocation
    );
    let preallocated = cloned.resolve(b"/prealloc-clone").await.unwrap();
    assert_eq!(
        disk_allocation(&cloned, &preallocated, 0).await,
        (allocation, 32768)
    );
    let tail_inode = cloned.resolve(b"/tail").await.unwrap();
    assert_eq!(
        disk_allocation(&cloned, &tail_inode, 4096).await,
        tail_allocation
    );
    let mut tx = writer.join().await.unwrap();
    for (name, offset, bytes) in [
        ("source", 9000, vec![0x77; 2000]),
        ("prealloc", 5000, vec![0x99; 256]),
        ("clone", 1, vec![0x31; 31]),
    ] {
        let id = ids[name];
        tx.edits()
            .write_file(
                id.tree,
                id.inode,
                offset as u64,
                &bytes,
                WriteOptions::plain(when, concurrency),
            )
            .await
            .unwrap();
        expected.get_mut(name).unwrap()[offset..offset + bytes.len()].copy_from_slice(&bytes);
    }
    tx.commit().await.unwrap();
    verify(named.path(), &expected);
    assert_reflink_contents(&writer.snapshot().unwrap(), &expected).await;
    for names in [
        &["source", "small", "tail", "prealloc"][..],
        &["clone", "small-clone", "prealloc-clone"][..],
    ] {
        let mut tx = writer.join().await.unwrap();
        for name in names {
            tx.edits()
                .unlink_name(
                    Name {
                        tree: root.tree(),
                        parent: root.number(),
                        bytes: name.as_bytes(),
                    },
                    when,
                )
                .await
                .unwrap();
            expected.remove(name);
        }
        tx.commit().await.unwrap();
        verify(named.path(), &expected);
        assert_reflink_contents(&writer.snapshot().unwrap(), &expected).await;
        let current = writer.snapshot().unwrap();
        for (&name, bytes) in &expected {
            let inode = current
                .lookup(&current.root().await.unwrap(), name.as_bytes())
                .await
                .unwrap()
                .unwrap();
            let mut output = vec![0; bytes.len()];
            current.read(&inode, 0, &mut output).await.unwrap();
            assert_eq!(&output, bytes);
        }
        let mut old = vec![0; original_bytes.len()];
        current.read(&original_inode, 0, &mut old).await.unwrap();
        assert_eq!(old, original_bytes);
    }
}

#[cfg(feature = "write")]
#[test]
fn rename_and_symlink_preserve_identity_and_reject_parent_cycles() {
    block_on(exercise_namespace_rename(|_| {}));
}

#[cfg(feature = "write")]
#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn rename_and_symlink_pass_btrfs_check() {
    block_on(exercise_namespace_rename(assert_btrfs_check));
}

#[cfg(feature = "write")]
async fn exercise_namespace_rename(mut verify: impl FnMut(&std::path::Path)) {
    use crate::transaction::namespace::{Attributes, Name};
    let timestamp = Timestamp::from_secs(1_700_000_000);
    let named = tempfile::NamedTempFile::new().unwrap();
    let builder = TreeBuilder::new().root(Metadata::new(0o755, timestamp));
    drop(filesystem_image_tree_file(
        true,
        true,
        builder,
        named.reopen().unwrap(),
    ));
    let mut writer = mount_writer(Disk(named.reopen().unwrap())).await.unwrap();
    let base = writer.snapshot().unwrap();
    let root = base.root().await.unwrap();
    let tree = root.tree();
    let root_number = root.number();
    let concurrency = std::thread::available_parallelism().unwrap();
    let when = crate::Timestamp {
        seconds: 1_700_000_100,
        nanoseconds: 123,
    };
    let name = |parent, bytes| Name {
        tree,
        parent,
        bytes,
    };
    let attributes = |mode| Attributes {
        mode,
        uid: 1000,
        gid: 1000,
        umask: 0,
    };
    let mut tx = writer.join().await.unwrap();
    let edits = tx.edits();
    let first = edits
        .create_node(
            name(root_number, "中文".as_bytes()),
            attributes(0o040755),
            when,
        )
        .await
        .unwrap();
    let second = edits
        .create_node(
            name(root_number, "日本語".as_bytes()),
            attributes(0o040755),
            when,
        )
        .await
        .unwrap();
    let child = edits
        .create_node(name(first, "한국어".as_bytes()), attributes(0o040755), when)
        .await
        .unwrap();
    let file = edits
        .create_node(name(first, b"file"), attributes(0o100644), when)
        .await
        .unwrap();
    let target = edits
        .create_node(name(second, b"target"), attributes(0o100644), when)
        .await
        .unwrap();
    let content = vec![0x37; 12307];
    edits
        .write_file(
            tree,
            file,
            0,
            &content,
            WriteOptions::plain(when, concurrency),
        )
        .await
        .unwrap();
    edits
        .write_file(
            tree,
            target,
            0,
            b"replaced",
            WriteOptions::plain(when, concurrency),
        )
        .await
        .unwrap();
    edits
        .symlink(name(first, b"link"), b"../target", attributes(0o777), when)
        .await
        .unwrap();
    edits
        .hard_link(name(second, b"alias"), file, when)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    verify(named.path());
    let previous = writer.snapshot().unwrap();
    let replaced = previous.resolve("/日本語/target".as_bytes()).await.unwrap();
    let mut tx = writer.join().await.unwrap();
    let edits = tx.edits();
    assert!(matches!(
        edits
            .rename(
                name(root_number, "中文".as_bytes()),
                name(child, b"loop"),
                RenameMode::Replace,
                when
            )
            .await,
        Err(WriteError::DirectoryCycle)
    ));
    // Renaming two links to the same inode leaves both names intact.
    edits
        .rename(
            name(first, b"file"),
            name(second, b"alias"),
            RenameMode::Replace,
            when,
        )
        .await
        .unwrap();
    edits
        .rename(
            name(first, b"file"),
            name(second, b"target"),
            RenameMode::Replace,
            when,
        )
        .await
        .unwrap();
    edits
        .rename(
            name(root_number, "中文".as_bytes()),
            name(second, b"moved"),
            RenameMode::Replace,
            when,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    verify(named.path());
    let current = writer.snapshot().unwrap();
    let moved = current.resolve("/日本語/moved".as_bytes()).await.unwrap();
    assert_eq!(moved.number(), first);
    assert_eq!(
        current
            .resolve("/日本語/moved/한국어".as_bytes())
            .await
            .unwrap()
            .number(),
        child
    );
    assert!(
        current
            .lookup(&current.root().await.unwrap(), "中文".as_bytes())
            .await
            .unwrap()
            .is_none()
    );
    for path in ["/日本語/target", "/日本語/alias", "/日本語/moved/link"] {
        let inode = current.resolve(path.as_bytes()).await.unwrap();
        assert_eq!(inode.number(), file);
        assert_eq!(inode.metadata().nlink, 2);
        let mut output = vec![0; content.len()];
        current.read(&inode, 0, &mut output).await.unwrap();
        assert_eq!(output, content);
    }
    let mut old_bytes = [0; 8];
    current.read(&replaced, 0, &mut old_bytes).await.unwrap();
    assert_eq!(&old_bytes, b"replaced");
}

#[cfg(feature = "write")]
#[test]
fn namespace_creation_and_hard_links_publish_together() {
    block_on(exercise_namespace_links(|_| {}));
}

#[cfg(feature = "write")]
#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn namespace_links_pass_btrfs_check() {
    block_on(exercise_namespace_links(assert_btrfs_check));
}

#[cfg(feature = "write")]
async fn exercise_namespace_links(mut verify: impl FnMut(&std::path::Path)) {
    use crate::transaction::namespace::{Attributes, Name};
    let timestamp = Timestamp::from_secs(1_700_000_000);
    let named = tempfile::NamedTempFile::new().unwrap();
    let tree = TreeBuilder::new().root(Metadata::new(0o755, timestamp));
    drop(filesystem_image_tree_file(
        true,
        true,
        tree,
        named.reopen().unwrap(),
    ));
    verify(named.path());
    let mut writer = mount_writer(Disk(named.reopen().unwrap())).await.unwrap();
    let old = writer.snapshot().unwrap();
    let parent = old.root().await.unwrap();
    let tree = parent.tree();
    let concurrency = std::thread::available_parallelism().unwrap();
    let when = crate::Timestamp {
        seconds: 1_700_000_100,
        nanoseconds: 123,
    };
    let mut tx = writer.join().await.unwrap();
    let edits = tx.edits();
    let mut directories = Vec::new();
    for name in ["中文", "日本語", "한국어"] {
        let inode = edits
            .create_node(
                Name {
                    tree,
                    parent: parent.number(),
                    bytes: name.as_bytes(),
                },
                Attributes {
                    mode: 0o040755,
                    uid: 1000,
                    gid: 1000,
                    umask: 0,
                },
                when,
            )
            .await
            .unwrap();
        directories.push(inode);
    }
    let name = Name {
        tree,
        parent: directories[0],
        bytes: b"data.bin",
    };
    let inode = edits
        .create_node(
            name,
            Attributes {
                mode: 0o100640,
                uid: 1000,
                gid: 1000,
                umask: 0,
            },
            when,
        )
        .await
        .unwrap();
    let content: Vec<_> = (0..12307).map(|index| (index % 251) as u8).collect();
    edits
        .write_file(
            tree,
            inode,
            0,
            &content,
            WriteOptions::plain(when, concurrency),
        )
        .await
        .unwrap();
    assert!(matches!(
        edits.hard_link(name, inode, when).await,
        Err(WriteError::Exists { .. })
    ));
    let mut links = Vec::new();
    // Long names exceed one leaf's ordinary INODE_REF capacity, exercising
    // EXTENDED_IREF and multileaf directory indices without huge fixtures.
    for index in 0..100 {
        let name = alloc::format!("{index:03}-{}", "x".repeat(196));
        edits
            .hard_link(
                Name {
                    tree,
                    parent: directories[0],
                    bytes: name.as_bytes(),
                },
                inode,
                when,
            )
            .await
            .unwrap();
        links.push(name);
    }
    for &directory in &directories[1..] {
        edits
            .hard_link(
                Name {
                    tree,
                    parent: directory,
                    bytes: b"alias",
                },
                inode,
                when,
            )
            .await
            .unwrap();
    }
    let root = edits.roots.get(tree).unwrap();
    let reference = edits
        .trees
        .ceiling(
            &old.volume,
            root,
            Key::new(inode, crate::ondisk::item_type::INODE_EXTREF, 0),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        reference
            .0
            .is(inode, crate::ondisk::item_type::INODE_EXTREF)
    );
    tx.commit().await.unwrap();
    verify(named.path());
    let current = writer.snapshot().unwrap();
    for path in ["/中文/data.bin", "/日本語/alias", "/한국어/alias"] {
        let file = current.resolve(path.as_bytes()).await.unwrap();
        assert_eq!(file.number(), inode);
        assert_eq!(file.metadata().nlink, 103);
        assert_eq!((file.metadata().uid, file.metadata().gid), (1000, 1000));
        let mut output = vec![0; content.len()];
        assert_eq!(
            current.read(&file, 0, &mut output).await.unwrap(),
            content.len()
        );
        assert_eq!(output, content);
    }
    let directory = current.resolve("/中文".as_bytes()).await.unwrap();
    for name in &links {
        assert_eq!(
            current
                .lookup(&directory, name.as_bytes())
                .await
                .unwrap()
                .unwrap()
                .number(),
            inode
        );
    }
    assert!(
        old.lookup(&parent, "中文".as_bytes())
            .await
            .unwrap()
            .is_none()
    );
    let old_file = current.resolve("/中文/data.bin".as_bytes()).await.unwrap();
    for stage in 0..3 {
        let mut tx = writer.join().await.unwrap();
        let edits = tx.edits();
        if stage == 0 {
            assert!(matches!(
                edits
                    .unlink_name(
                        Name {
                            tree,
                            parent: parent.number(),
                            bytes: "中文".as_bytes()
                        },
                        when
                    )
                    .await,
                Err(WriteError::NotEmpty { .. })
            ));
            edits
                .unlink_name(
                    Name {
                        tree,
                        parent: directories[0],
                        bytes: b"data.bin",
                    },
                    when,
                )
                .await
                .unwrap();
        }
        if stage < 2 {
            for name in &links[stage * 50..(stage + 1) * 50] {
                edits
                    .unlink_name(
                        Name {
                            tree,
                            parent: directories[0],
                            bytes: name.as_bytes(),
                        },
                        when,
                    )
                    .await
                    .unwrap();
            }
            if stage == 1 {
                edits
                    .unlink_name(
                        Name {
                            tree,
                            parent: directories[1],
                            bytes: b"alias",
                        },
                        when,
                    )
                    .await
                    .unwrap();
            }
        } else {
            edits
                .unlink_name(
                    Name {
                        tree,
                        parent: directories[2],
                        bytes: b"alias",
                    },
                    when,
                )
                .await
                .unwrap();
            for name in ["中文", "日本語", "한국어"] {
                edits
                    .unlink_name(
                        Name {
                            tree,
                            parent: parent.number(),
                            bytes: name.as_bytes(),
                        },
                        when,
                    )
                    .await
                    .unwrap();
            }
        }
        tx.commit().await.unwrap();
        verify(named.path());
        let view = writer.snapshot().unwrap();
        if stage < 2 {
            let remaining = view.resolve("/한국어/alias".as_bytes()).await.unwrap();
            assert_eq!(remaining.metadata().nlink, if stage == 0 { 52 } else { 1 });
            let mut output = vec![0; content.len()];
            view.read(&remaining, 0, &mut output).await.unwrap();
            assert_eq!(output, content);
        } else {
            let root = view.root().await.unwrap();
            assert_eq!(root.metadata().size, 0);
            assert!(
                view.lookup(&root, "中文".as_bytes())
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let mut output = vec![0; content.len()];
        assert_eq!(
            view.read(&old_file, 0, &mut output).await.unwrap(),
            content.len()
        );
        assert_eq!(output, content);
    }
}

#[cfg(feature = "write")]
#[test]
fn file_writes_preserve_partial_sectors_and_transaction_visibility() {
    for no_holes in [true, false] {
        block_on(exercise_file_writes(
            no_holes,
            crate::compress::write::Plain,
            |_, _| {},
        ));
    }
}

#[cfg(feature = "write")]
#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn file_writes_pass_btrfs_check() {
    for no_holes in [true, false] {
        block_on(exercise_file_writes(
            no_holes,
            crate::compress::write::Plain,
            assert_file_image,
        ));
    }
}

#[cfg(feature = "write")]
#[test]
fn compressed_file_writes_preserve_overwrites_and_truncation() {
    block_on(exercise_compressed_writes(|_, _| {}));
}

#[cfg(feature = "write")]
#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn compressed_file_writes_pass_btrfs_check_and_restore() {
    block_on(exercise_compressed_writes(assert_file_image));
}

#[cfg(feature = "write")]
async fn exercise_compressed_writes(mut verify: impl FnMut(&std::path::Path, &[u8])) {
    use crate::compress::write::{Lzo, Zlib, Zstd};
    use miniz_oxide::deflate::CompressionLevel;
    for no_holes in [true, false] {
        exercise_file_writes(
            no_holes,
            Zlib::new(CompressionLevel::DefaultLevel),
            &mut verify,
        )
        .await;
        exercise_file_writes(no_holes, Lzo::new(), &mut verify).await;
        exercise_file_writes(no_holes, Zstd::new(3).unwrap(), &mut verify).await;
    }
}

#[cfg(feature = "write")]
fn assert_file_image(path: &std::path::Path, expected: &[u8]) {
    assert_btrfs_check(path);
    let directory = tempfile::tempdir().unwrap();
    let output = std::process::Command::new("btrfs")
        .arg("restore")
        .arg(path)
        .arg(directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "btrfs restore failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(directory.path().join("data.bin")).unwrap(),
        expected
    );
}

#[cfg(feature = "write")]
async fn exercise_file_writes<E: crate::compress::write::Encoder>(
    no_holes: bool,
    mut encoder: E,
    mut verify: impl FnMut(&std::path::Path, &[u8]),
) {
    let timestamp = Timestamp::from_secs(1_700_000_000);
    let named = tempfile::NamedTempFile::new().unwrap();
    let tree = TreeBuilder::new()
        .root(Metadata::new(0o755, timestamp))
        .file("/data.bin", Vec::new(), Metadata::new(0o644, timestamp));
    let mut options = FormatOptions::new([0x53; 16], timestamp);
    if !no_holes {
        options.plan.incompat_flags = options
            .plan
            .incompat_flags
            .without(ferrosys::btrfs::ondisk::IncompatFlags::NO_HOLES);
        options.plan.compat_ro_flags = options
            .plan
            .compat_ro_flags
            .without(ferrosys::btrfs::ondisk::CompatRoFlags::BLOCK_GROUP_TREE);
    }
    drop(filesystem_image_options(
        tree,
        named.reopen().unwrap(),
        options,
    ));
    verify(named.path(), &[]);
    let mut writer = mount_writer(Disk(named.reopen().unwrap())).await.unwrap();
    let initial = writer.snapshot().unwrap();
    let initial_inode = initial.resolve(b"/data.bin").await.unwrap();
    let mut expected = Vec::new();
    let concurrency = std::thread::available_parallelism().unwrap();
    let when = crate::Timestamp {
        seconds: 1_700_000_100,
        nanoseconds: 123,
    };
    // Hash successive counters into a deterministic, high-entropy full extent.
    // Unlike the repeated-byte windows this should not save a disk sector.
    let incompressible: Vec<u8> = (0..crate::ondisk::MAX_COMPRESSED_EXTENT / 8)
        .flat_map(|index| {
            use core::hash::Hasher;
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            hash.write_u64(index);
            hash.finish().to_le_bytes()
        })
        .collect();
    for round in 0..2 {
        let previous = writer.snapshot().unwrap();
        let inode = previous.resolve(b"/data.bin").await.unwrap();
        let previous_bytes = expected.clone();
        let mut transaction = writer.join().await.unwrap();
        for (offset, bytes) in [
            (0, b"hello".to_vec()),
            (2, "中文・日本語・한국어".as_bytes().to_vec()),
            (300, vec![0x23; 71]),
            (
                4093,
                (0..(128 * 1024 + 37))
                    .map(|index| (index % 251) as u8)
                    .collect(),
            ),
            (8190, vec![0x55; 12307]),
            (150003 + round * 10000, vec![0xa7; 31]),
            (0, vec![0x71; 13]),
            (262144, incompressible.clone()),
        ] {
            let edits = transaction.edits();
            assert_eq!(
                edits
                    .write_file(
                        inode.tree(),
                        inode.number(),
                        offset as u64,
                        &bytes,
                        WriteOptions {
                            when,
                            concurrency,
                            encoder: &mut encoder,
                            max_inline: DEFAULT_MAX_INLINE,
                        }
                    )
                    .await
                    .unwrap(),
                bytes.len()
            );
            expected.resize(expected.len().max(offset + bytes.len()), 0);
            expected[offset..offset + bytes.len()].copy_from_slice(&bytes);
            let mut output = vec![0; expected.len() + 17];
            assert_eq!(
                edits
                    .read_file(inode.tree(), inode.number(), 0, &mut output)
                    .await
                    .unwrap(),
                expected.len()
            );
            assert_eq!(&output[..expected.len()], expected.as_slice());
        }
        transaction.commit().await.unwrap();
        verify(named.path(), &expected);
        let current = writer.snapshot().unwrap();
        let current_inode = current.resolve(b"/data.bin").await.unwrap();
        if let Some(algorithm) = E::ALGORITHM {
            let mut cursor = crate::tree::Cursor::ceiling(
                &current.volume,
                current_inode.root,
                Key::new(
                    current_inode.number(),
                    crate::ondisk::item_type::EXTENT_DATA,
                    0,
                ),
            )
            .await
            .unwrap();
            let mut compressed = false;
            while let Some(key) = cursor.key() {
                if !key.is(
                    current_inode.number(),
                    crate::ondisk::item_type::EXTENT_DATA,
                ) {
                    break;
                }
                let extent = crate::items::FileExtent::parse(
                    key,
                    cursor.item().unwrap(),
                    current.superblock().sectorsize(),
                )
                .unwrap();
                if key.offset == 262144 {
                    assert!(matches!(
                        extent.data,
                        crate::items::ExtentData::Regular {
                            compression: None,
                            ..
                        }
                    ));
                }
                compressed |= matches!(extent.data, crate::items::ExtentData::Regular { compression: Some(value), .. } if value == algorithm);
                cursor.advance(&current.volume).await.unwrap();
            }
            assert!(
                compressed,
                "the repetitive file should have compressed extents"
            );
            let bit = match algorithm {
                crate::Compression::Zlib => 0,
                crate::Compression::Lzo => 1 << 3,
                crate::Compression::Zstd => 1 << 4,
            };
            assert_eq!(current.superblock().incompat_flags() & bit, bit);
        }

        assert_eq!(current_inode.metadata().mtime, when);
        let mut output = vec![0; expected.len()];
        assert_eq!(
            current.read(&current_inode, 0, &mut output).await.unwrap(),
            expected.len()
        );
        assert_eq!(output, expected);
        let mut old_output = vec![0; previous_bytes.len() + 1];
        assert_eq!(
            current.read(&inode, 0, &mut old_output).await.unwrap(),
            previous_bytes.len()
        );
        assert_eq!(
            &old_output[..previous_bytes.len()],
            previous_bytes.as_slice()
        );
        assert_eq!(
            current.read(&initial_inode, 0, &mut output).await.unwrap(),
            0
        );
    }
    for size in [8193, 150037, 300, 4097, 0, 17, 3, 0] {
        let previous = writer.snapshot().unwrap();
        let inode = previous.resolve(b"/data.bin").await.unwrap();
        let old = expected.clone();
        let mut transaction = writer.join().await.unwrap();
        let edits = transaction.edits();
        edits
            .truncate_file(
                inode.tree(),
                inode.number(),
                size as u64,
                WriteOptions::plain(when, concurrency),
            )
            .await
            .unwrap();
        expected.resize(size, 0);
        let mut output = vec![0xff; size + 17];
        assert_eq!(
            edits
                .read_file(inode.tree(), inode.number(), 0, &mut output)
                .await
                .unwrap(),
            size
        );
        assert_eq!(&output[..size], expected.as_slice());
        transaction.commit().await.unwrap();
        verify(named.path(), &expected);
        let current = writer.snapshot().unwrap();
        let current_inode = current.resolve(b"/data.bin").await.unwrap();
        output.fill(0xff);
        assert_eq!(
            current.read(&current_inode, 0, &mut output).await.unwrap(),
            size
        );
        assert_eq!(&output[..size], expected.as_slice());
        let mut old_output = vec![0; old.len()];
        assert_eq!(
            current.read(&inode, 0, &mut old_output).await.unwrap(),
            old.len()
        );
        assert_eq!(old_output, old);
    }
}

#[cfg(feature = "write")]
#[test]
fn extent_cuts_preserve_sides_count_references_and_release_the_last_mapping() {
    use btrfs_disk::items::CompressionType;
    for compression in [
        None,
        Some(CompressionType::Zlib),
        Some(CompressionType::Lzo),
        Some(CompressionType::Zstd),
    ] {
        block_on(exercise_extent_cuts(compression, false, |_| {}));
    }
    block_on(exercise_extent_cuts(None, true, |_| {}));
}

#[cfg(feature = "write")]
async fn exercise_extent_cuts(
    compression: Option<btrfs_disk::items::CompressionType>,
    preallocated: bool,
    mut verify: impl FnMut(&std::path::Path),
) {
    use crate::ondisk::{RawFileExtentBody, RawFileExtentHead, RawInodeItem, item_type, view};
    use zerocopy::IntoBytes;

    let content: Vec<_> = (0..32768)
        .map(|index| if preallocated { 0 } else { (index % 251) as u8 })
        .collect();
    let timestamp = Timestamp::from_secs(1_700_000_000);
    let named = tempfile::NamedTempFile::new().unwrap();
    let tree = TreeBuilder::new().root(Metadata::new(0o755, timestamp));
    let (base, _) = filesystem_image_tree_file(true, true, tree, named.reopen().unwrap());
    let root_inode = base.root().await.unwrap().number();
    let tree_id = base.default_subvolume();
    drop(base);
    let mut external =
        btrfs_transaction::filesystem::Filesystem::open(named.reopen().unwrap()).unwrap();
    let mut tx = btrfs_transaction::transaction::Transaction::start(&mut external).unwrap();
    // The first regular inode follows the root inode in this empty tree.
    let inode = root_inode + 1;
    let when = btrfs_disk::items::Timespec {
        sec: 1_700_000_000,
        nsec: 0,
    };
    let mut args =
        btrfs_transaction::inode::InodeArgs::new(tx.transid, 0o100_644).with_uniform_time(when);
    args.size = content.len() as u64;
    tx.create_inode(&mut external, tree_id, inode, &args)
        .unwrap();
    // Directory indices 0 and 1 are reserved for dot and dot-dot.
    tx.link_dir_entry(
        &mut external,
        tree_id,
        root_inode,
        inode,
        b"data.bin",
        1,
        2,
        when,
    )
    .unwrap();
    if preallocated {
        let length = content.len() as u64;
        let logical = tx
            .reserve_data_extent(&mut external, length, tree_id, inode, 0)
            .unwrap();
        let item = btrfs_disk::items::FileExtentItem::to_bytes_regular(
            tx.transid,
            length,
            btrfs_disk::items::CompressionType::None,
            true,
            logical,
            length,
            0,
            length,
        );
        tx.insert_file_extent(&mut external, tree_id, inode, 0, &item)
            .unwrap();
        tx.update_inode_nbytes(&mut external, tree_id, inode, length as i64)
            .unwrap();
    } else {
        tx.write_file_data(
            &mut external,
            tree_id,
            inode,
            0,
            &content,
            false,
            compression,
        )
        .unwrap();
    }
    tx.commit(&mut external).unwrap();
    drop(external);
    verify(named.path());
    let mut writer = mount_writer(Disk(named.reopen().unwrap())).await.unwrap();
    let old = writer.snapshot().unwrap();
    let inode = old.resolve(b"/data.bin").await.unwrap();
    let mut expected = content.clone();
    let mut data_key = None;
    for (offset, range, references) in [
        (0, 8192..16384, 2),
        (0, 0..8192, 1),
        (16384, 24576..32768, 1),
        (16384, 16384..24576, 0),
    ] {
        let current = writer.snapshot().unwrap();
        let mut transaction = writer.join().await.unwrap();
        let edits = transaction.edits();
        let root = edits.roots.get(inode.tree()).unwrap();
        let key = Key::new(inode.number(), item_type::EXTENT_DATA, offset);
        let bytes = edits
            .trees
            .get(&current.volume, root, key)
            .await
            .unwrap()
            .unwrap();
        let head = view::<RawFileExtentHead>(&bytes).unwrap();
        assert_eq!(head.extent_type, if preallocated { 2 } else { 1 });
        assert_eq!(
            head.compression,
            match compression {
                None => 0,
                Some(btrfs_disk::items::CompressionType::Zlib) => 1,
                Some(btrfs_disk::items::CompressionType::Lzo) => 2,
                Some(btrfs_disk::items::CompressionType::Zstd) => 3,
                _ => panic!("fixture compression"),
            }
        );
        let body = view::<RawFileExtentBody>(&bytes[size_of::<RawFileExtentHead>()..]).unwrap();
        let allocation = Key::new(
            body.disk_bytenr.get(),
            item_type::EXTENT_ITEM,
            body.disk_num_bytes.get(),
        );
        assert_eq!(*data_key.get_or_insert(allocation), allocation);
        let removed = edits
            .cut_regular_extent(inode.tree(), key, range.clone())
            .await
            .unwrap();
        assert_eq!(removed, range.end - range.start);
        let root = edits.roots.get(inode.tree()).unwrap();
        let inode_key = Key::new(inode.number(), item_type::INODE_ITEM, 0);
        let bytes = edits
            .trees
            .get(&current.volume, root, inode_key)
            .await
            .unwrap()
            .unwrap();
        let mut raw = *view::<RawInodeItem>(&bytes).unwrap();
        raw.nbytes.set(raw.nbytes.get() - removed);
        raw.transid.set(edits.trees.generation());
        let root = edits
            .trees
            .apply(
                &current.volume,
                root,
                inode_key,
                Some(raw.as_bytes()),
                &mut edits.space,
            )
            .await
            .unwrap();
        edits.roots.set(root).unwrap();
        let allocation_bytes = edits
            .trees
            .get(&current.volume, edits.accounting.root, allocation)
            .await
            .unwrap();
        if references == 0 {
            assert!(allocation_bytes.is_none());
        } else {
            assert_eq!(
                btrfs_disk::items::ExtentItem::parse(
                    &allocation_bytes.unwrap(),
                    &btrfs_disk::tree::DiskKey {
                        objectid: allocation.objectid,
                        key_type: btrfs_disk::tree::KeyType::ExtentItem,
                        offset: allocation.offset
                    }
                )
                .unwrap()
                .refs,
                references
            );
        }
        expected[range.start as usize..range.end as usize].fill(0);
        let mut preview = vec![0; content.len()];
        assert_eq!(
            edits
                .read_file(inode.tree(), inode.number(), 0, &mut preview)
                .await
                .unwrap(),
            content.len()
        );
        assert_eq!(preview, expected);
        transaction.commit().await.unwrap();
        verify(named.path());
        let current = writer.snapshot().unwrap();
        let current_inode = current.resolve(b"/data.bin").await.unwrap();
        let mut output = vec![0; content.len()];
        assert_eq!(
            current.read(&current_inode, 0, &mut output).await.unwrap(),
            content.len()
        );
        assert_eq!(output, expected);
        assert_eq!(
            current.read(&inode, 0, &mut output).await.unwrap(),
            content.len()
        );
        assert_eq!(output, content);
    }
    let current = writer.snapshot().unwrap();
    assert!(matches!(
        current
            .lookup_checksums(&current.snapshot, data_key.unwrap().objectid, content.len())
            .await,
        Err(crate::Error::MissingDataChecksum { .. })
    ));
}

#[cfg(feature = "write")]
#[test]
fn inode_flags_preserve_immutable_and_append_only_semantics() {
    block_on(exercise_inode_flags(|_| {}));
}

#[cfg(feature = "write")]
#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn inode_flag_mutations_pass_btrfs_check() {
    block_on(exercise_inode_flags(assert_btrfs_check));
}

#[cfg(feature = "write")]
async fn exercise_inode_flags(mut verify: impl FnMut(&std::path::Path)) {
    use super::writer::Attributes;
    use super::writer::{CloneRange, Name};
    use crate::transaction::reflink::FileId;
    use btrfs_disk::items::{InodeFlags, Timespec};
    let stamp = Timestamp::from_secs(1_700_000_000);
    let named = tempfile::NamedTempFile::new().unwrap();
    let tree = TreeBuilder::new().root(Metadata::new(0o755, stamp));
    let (formatted, _) = filesystem_image_tree_file(true, true, tree, named.reopen().unwrap());
    let root = formatted.root().await.unwrap();
    let tree = root.tree();
    let first = root.number() + 1;
    drop(formatted);
    let mut external =
        btrfs_transaction::filesystem::Filesystem::open(named.reopen().unwrap()).unwrap();
    let mut fixture = btrfs_transaction::transaction::Transaction::start(&mut external).unwrap();
    let initial = b"initial";
    let timestamp = Timespec {
        sec: 1_700_000_000,
        nsec: 0,
    };
    let mut ids = BTreeMap::new();
    for (index, (name, mode, flags, parent)) in [
        ("immutable", 0o100644, InodeFlags::IMMUTABLE, root.number()),
        ("append", 0o100644, InodeFlags::APPEND, root.number()),
        (
            "immutable-dir",
            0o040755,
            InodeFlags::IMMUTABLE,
            root.number(),
        ),
        ("append-dir", 0o040755, InodeFlags::APPEND, root.number()),
        ("child", 0o100644, InodeFlags::empty(), first + 3),
        ("plain", 0o100644, InodeFlags::empty(), root.number()),
    ]
    .into_iter()
    .enumerate()
    {
        let inode = first + index as u64;
        let directory = mode & 0o170000 == 0o040000;
        let mut attributes = btrfs_transaction::inode::InodeArgs::new(fixture.transid, mode)
            .with_uniform_time(timestamp);
        attributes.flags = flags;
        if !directory {
            attributes.size = initial.len() as u64;
        }
        fixture
            .create_inode(&mut external, tree, inode, &attributes)
            .unwrap();
        if !directory {
            fixture
                .insert_inline_extent(&mut external, tree, inode, 0, initial, None)
                .unwrap();
        }
        fixture
            .link_dir_entry(
                &mut external,
                tree,
                parent,
                inode,
                name.as_bytes(),
                if directory { 2 } else { 1 },
                index as u64 + 2,
                timestamp,
            )
            .unwrap();
        ids.insert(name, FileId { tree, inode });
    }
    fixture.commit(&mut external).unwrap();
    drop(external);
    verify(named.path());
    let mut writer = mount_writer(Disk(named.reopen().unwrap())).await.unwrap();
    let base = writer.snapshot().unwrap();
    let root = base.root().await.unwrap();
    let handles = writer.join().await.unwrap();
    let root_handle = handles.root().await.unwrap();
    let raw_ids = ids;
    let mut ids = BTreeMap::new();
    for (name, id) in raw_ids {
        ids.insert(name, handles.fixture_handle(id).await.unwrap());
    }
    let when = crate::Timestamp {
        seconds: 1_700_000_100,
        nanoseconds: 123,
    };
    let concurrency = std::thread::available_parallelism().unwrap();
    let name = |parent, bytes| Name { parent, bytes };
    let attributes = || Attributes {
        permissions: 0o644,
        uid: 1000,
        gid: 1000,
        umask: 0,
    };
    macro_rules! refused {
        ($tx:ident, $operation:expr) => {{
            let mut $tx = writer.join().await.unwrap();
            assert!(matches!(
                $operation.await,
                Err(WriteError::InodeFlags { .. })
            ));
            assert_eq!(writer.snapshot().unwrap().snapshot, base.snapshot);
        }};
    }
    for label in ["immutable", "append"] {
        let file = &ids[label];
        refused!(
            tx,
            tx.write_file(file, 0, b"replace", WriteOptions::plain(when, concurrency))
        );
        refused!(
            tx,
            tx.truncate_file(file, 0, WriteOptions::plain(when, concurrency))
        );
        refused!(tx, tx.set_times(file, Some(when), None, when));
        refused!(tx, tx.set_permissions(file, 0o600, when));
        refused!(tx, tx.set_owner(file, Some(1), Some(2), when));
        refused!(tx, tx.hard_link(name(&root_handle, b"alias"), file, when));
        refused!(
            tx,
            tx.unlink_name(name(&root_handle, label.as_bytes()), when)
        );
        refused!(
            tx,
            tx.rename(
                name(&root_handle, label.as_bytes()),
                name(&root_handle, b"renamed"),
                RenameMode::Replace,
                when
            )
        );
        refused!(
            tx,
            tx.clone_range(
                CloneRange {
                    source: &ids["plain"],
                    destination: file,
                    source_offset: 0,
                    destination_offset: 0,
                    length: initial.len() as u64
                },
                WriteOptions::plain(when, concurrency)
            )
        );
    }
    refused!(
        tx,
        tx.write_file(
            &ids["immutable"],
            initial.len() as u64,
            b"more",
            WriteOptions::plain(when, concurrency)
        )
    );
    let immutable_dir = &ids["immutable-dir"];
    refused!(
        tx,
        tx.create_file(name(immutable_dir, b"new"), attributes(), when)
    );
    refused!(
        tx,
        tx.hard_link(name(immutable_dir, b"alias"), &ids["plain"], when)
    );
    refused!(
        tx,
        tx.symlink(name(immutable_dir, b"link"), b"target", attributes(), when)
    );
    let append_dir = &ids["append-dir"];
    refused!(tx, tx.unlink_name(name(append_dir, b"child"), when));
    refused!(
        tx,
        tx.rename(
            name(append_dir, b"child"),
            name(&root_handle, b"moved"),
            RenameMode::Replace,
            when
        )
    );
    assert!(
        writer
            .snapshot()
            .unwrap()
            .lookup(&root, b"moved")
            .await
            .unwrap()
            .is_none()
    );
    verify(named.path());
    let mut tx = writer.join().await.unwrap();
    tx.write_file(
        &ids["append"],
        initial.len() as u64,
        b" added",
        WriteOptions::plain(when, concurrency),
    )
    .await
    .unwrap();
    let _ = tx
        .create_file(name(append_dir, b"new"), attributes(), when)
        .await
        .unwrap();
    tx.hard_link(name(append_dir, b"alias"), &ids["plain"], when)
        .await
        .unwrap();
    let _ = tx
        .symlink(name(append_dir, b"link"), b"child", attributes(), when)
        .await
        .unwrap();
    // Immutable sources may be cloned; the new file has independent flags.
    tx.clone_range(
        CloneRange {
            source: &ids["immutable"],
            destination: &ids["plain"],
            source_offset: 0,
            destination_offset: 0,
            length: initial.len() as u64,
        },
        WriteOptions::plain(when, concurrency),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    verify(named.path());
    let current = writer.snapshot().unwrap();
    let inode = current.resolve(b"/append").await.unwrap();
    let mut output = [0; 13];
    assert_eq!(
        current.read(&inode, 0, &mut output).await.unwrap(),
        output.len()
    );
    assert_eq!(&output, b"initial added");
    let immutable = current.resolve(b"/immutable").await.unwrap();
    assert_eq!(immutable.metadata().flags, InodeFlags::IMMUTABLE.bits());
    let mut output = [0; 7];
    current.read(&immutable, 0, &mut output).await.unwrap();
    assert_eq!(&output, initial);
    current.resolve(b"/append-dir/new").await.unwrap();
    current.resolve(b"/append-dir/alias").await.unwrap();
    assert!(base.lookup(&root, b"renamed").await.unwrap().is_none());
}

#[cfg(feature = "write")]
#[test]
fn reader_accepts_compat_ro_features_that_the_writer_refuses() {
    use btrfs_disk::superblock::{read_superblock, write_superblock_all_mirrors};
    block_on(async {
        for (bit, name) in [(1 << 2, "verity"), (1 << 63, "undefined")] {
            let (formatted, mut image) = filesystem_image();
            drop(formatted);
            let mut superblock = read_superblock(&mut image, 0).unwrap();
            superblock.compat_ro_flags |= bit;
            write_superblock_all_mirrors(&mut image, &superblock.to_bytes()).unwrap();
            let reader = Filesystem::mount(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            assert_eq!(reader.superblock().compat_ro_flags() & bit, bit);
            assert_eq!(
                reader.root().await.unwrap().kind(),
                crate::FileKind::Directory
            );
            assert!(matches!(mount_writer(Disk(image)).await,
                Err(WriteError::UnsupportedCompatRo { name: found_name, bit: found_bit })
                if found_name == name && found_bit == bit));
        }
    });
}

/// Whether any extent of inode `number` in the committed `fs` is compressed
/// with `algorithm`, and whether any is compressed with another or stored plain.
#[cfg(feature = "write")]
async fn extent_compressions<S: BlockSource>(
    fs: &Filesystem<S>,
    tree: u64,
    number: u64,
) -> Vec<Option<crate::Compression>> {
    use crate::items::{ExtentData, FileExtent};
    use crate::ondisk::item_type;
    let root = crate::fs::find_root(&fs.volume, fs.snapshot.root_tree, tree)
        .await
        .unwrap()
        .root;
    let mut cursor = crate::tree::Cursor::ceiling(
        &fs.volume,
        root,
        Key::new(number, item_type::EXTENT_DATA, 0),
    )
    .await
    .unwrap();
    let mut found = Vec::new();
    while let Some(key) = cursor.key() {
        if !key.is(number, item_type::EXTENT_DATA) {
            break;
        }
        let extent =
            FileExtent::parse(key, cursor.item().unwrap(), fs.superblock().sectorsize()).unwrap();
        if let ExtentData::Regular { compression, .. } = extent.data {
            found.push(compression);
        }
        cursor.advance(&fs.volume).await.unwrap();
    }
    found
}

#[cfg(feature = "write")]
#[test]
fn a_policy_compresses_only_inodes_that_linux_would_compress() {
    use super::writer::{Attributes, Name};
    use crate::Compression;
    use crate::compress::policy::{CompressionPolicy, Compressor};
    use crate::items::{INODE_NOCOMPRESS, INODE_NODATACOW, INODE_NODATASUM};
    use crate::ondisk::{RawInodeItem, item_type, view};
    use zerocopy::IntoBytes;

    block_on(async {
        let concurrency = std::thread::available_parallelism().unwrap();
        let when = crate::Timestamp {
            seconds: 1_700_000_100,
            nanoseconds: 0,
        };
        let data: Vec<u8> = b"compressible line of text\n"
            .iter()
            .copied()
            .cycle()
            .take(96 * 1024)
            .collect();
        for (policy, expected) in [
            (None, None),
            (
                Some(CompressionPolicy::new(Compression::Zlib, 0)),
                Some(Compression::Zlib),
            ),
            (
                Some(CompressionPolicy::new(Compression::Zlib, 9)),
                Some(Compression::Zlib),
            ),
            (
                Some(CompressionPolicy::new(Compression::Lzo, 0)),
                Some(Compression::Lzo),
            ),
            (
                Some(CompressionPolicy::new(Compression::Zstd, 0)),
                Some(Compression::Zstd),
            ),
            (
                Some(CompressionPolicy::new(Compression::Zstd, -3)),
                Some(Compression::Zstd),
            ),
        ] {
            let (_, image) = filesystem_image();
            let mut writer = mount_writer(Disk(image)).await.unwrap();
            let mut compressor = Compressor::new(policy).unwrap();
            let base = writer.snapshot().unwrap();
            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            let mut files = Vec::new();
            for (name, flags) in [
                (&b"plain"[..], 0),
                (b"nocompress", INODE_NOCOMPRESS),
                (b"nodatasum", INODE_NODATASUM),
                (b"nodatacow", INODE_NODATACOW | INODE_NODATASUM),
            ] {
                let file = tx
                    .create_file(
                        Name {
                            parent: &root,
                            bytes: name,
                        },
                        Attributes {
                            permissions: 0o644,
                            uid: 0,
                            gid: 0,
                            umask: 0,
                        },
                        when,
                    )
                    .await
                    .unwrap();
                let edits = tx.edits();
                let tree = edits.roots.get(file.tree()).unwrap();
                let key = Key::new(file.number(), item_type::INODE_ITEM, 0);
                let bytes = edits
                    .trees
                    .get(&base.volume, tree, key)
                    .await
                    .unwrap()
                    .unwrap();
                let mut raw = *view::<RawInodeItem>(&bytes).unwrap();
                raw.flags.set(flags);
                let tree = edits
                    .trees
                    .apply(
                        &base.volume,
                        tree,
                        key,
                        Some(raw.as_bytes()),
                        &mut edits.space,
                    )
                    .await
                    .unwrap();
                edits.roots.set(tree).unwrap();
                files.push((name, file));
            }
            for (_, file) in &files {
                assert_eq!(
                    tx.write_file_with_policy(
                        file,
                        0,
                        &data,
                        super::writer::MountPolicy {
                            when,
                            concurrency,
                            compressor: &mut compressor,
                            max_inline: DEFAULT_MAX_INLINE,
                        },
                    )
                    .await
                    .unwrap(),
                    data.len()
                );
            }
            tx.commit().await.unwrap();
            let current = writer.snapshot().unwrap();
            for (name, handle) in &files {
                let inode = current.resolve(&[b"/", *name].concat()).await.unwrap();
                let mut output = vec![0; data.len()];
                assert_eq!(
                    current.read(&inode, 0, &mut output).await.unwrap(),
                    data.len()
                );
                assert_eq!(output, data);
                let extents = extent_compressions(&current, handle.tree(), handle.number()).await;
                assert!(!extents.is_empty());
                let want = if name == b"plain" { expected } else { None };
                assert!(
                    extents.iter().all(|found| *found == want),
                    "{policy:?} {}: {extents:?}",
                    String::from_utf8_lossy(name)
                );
            }
        }
    });
}

#[cfg(feature = "write")]
#[test]
fn writer_refuses_a_filesystem_with_a_quota_tree_and_the_reader_keeps_working() {
    use crate::ondisk::{item_type, objectid};
    block_on(async {
        let (_, image) = filesystem_image();
        let mut writer = mount_writer(Disk(image.try_clone().unwrap()))
            .await
            .unwrap();
        let old = writer.snapshot().unwrap();
        let mut tx = writer.join().await.unwrap();
        let edits = tx.edits();
        // Linux enables quotas by adding a root item for tree 8; the item's
        // content does not matter to the refusal, so borrow the checksum
        // tree's.
        let (_, item) = edits
            .trees
            .floor(
                &old.volume,
                edits.roots.root,
                Key::new(objectid::CSUM_TREE, item_type::ROOT_ITEM, u64::MAX),
            )
            .await
            .unwrap()
            .unwrap();
        edits.roots.root = edits
            .trees
            .apply(
                &old.volume,
                edits.roots.root,
                Key::new(objectid::QUOTA_TREE, item_type::ROOT_ITEM, 0),
                Some(&item),
                &mut edits.space,
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let reader = Filesystem::mount(Disk(image.try_clone().unwrap()))
            .await
            .unwrap();
        assert_eq!(
            reader.root().await.unwrap().kind(),
            crate::FileKind::Directory
        );
        assert!(matches!(
            mount_writer(Disk(image)).await,
            Err(WriteError::QuotaTreePresent)
        ));
    });
}

#[cfg(feature = "write")]
#[test]
fn reflink_from_readonly_subvolume_preserves_source() {
    use super::writer::CloneRange;

    use ferrosys::btrfs::SubvolumeRequest;
    block_on(async {
        let stamp = Timestamp::from_secs(1_700_000_000);
        let directory = Metadata::new(0o755, stamp);
        let file = Metadata::new(0o644, stamp);
        let contents = vec![0x4b; 8192];
        let tree = TreeBuilder::new()
            .root(directory)
            .directory("/ro", directory)
            .file("/ro/source", contents.clone(), file)
            .directory("/rw", directory)
            .file("/rw/clone", Vec::new(), file);
        let options = FormatOptions::new([0x53; 16], stamp)
            .subvolume(SubvolumeRequest::new(b"/ro".to_vec(), [0x31; 16]).read_only(true))
            .subvolume(SubvolumeRequest::new(b"/rw".to_vec(), [0x32; 16]));
        let (formatted, image) =
            filesystem_image_options(tree, tempfile::tempfile().unwrap(), options);
        drop(formatted);
        let mut writer = mount_writer(Disk(image)).await.unwrap();
        let base = writer.snapshot().unwrap();
        let source = base.resolve(b"/ro/source").await.unwrap();
        let handles = writer.join().await.unwrap();
        let root_handle = handles.root().await.unwrap();
        let ro = handles.lookup(&root_handle, b"ro").await.unwrap().unwrap();
        let rw = handles.lookup(&root_handle, b"rw").await.unwrap().unwrap();
        let source_id = handles.lookup(&ro, b"source").await.unwrap().unwrap();
        let destination_id = handles.lookup(&rw, b"clone").await.unwrap().unwrap();
        let when = crate::Timestamp {
            seconds: 1_700_000_100,
            nanoseconds: 0,
        };
        let concurrency = std::thread::available_parallelism().unwrap();
        let mut tx = writer.join().await.unwrap();
        tx.clone_range(
            CloneRange {
                source: &source_id,
                destination: &destination_id,
                source_offset: 0,
                destination_offset: 0,
                length: contents.len() as u64,
            },
            WriteOptions::plain(when, concurrency),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let cloned = writer.snapshot().unwrap();
        let cloned_inode = cloned.resolve(b"/rw/clone").await.unwrap();
        assert_eq!(
            disk_allocation(&cloned, &cloned_inode, 0).await,
            disk_allocation(&base, &source, 0).await
        );
        let mut output = vec![0; contents.len()];
        cloned.read(&cloned_inode, 0, &mut output).await.unwrap();
        assert_eq!(output, contents);
        let replacement = vec![0x82; contents.len()];
        let mut tx = writer.join().await.unwrap();
        tx.write_file(
            &destination_id,
            0,
            &replacement,
            WriteOptions::plain(when, concurrency),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let current = writer.snapshot().unwrap();
        current
            .read(
                &current.resolve(b"/ro/source").await.unwrap(),
                0,
                &mut output,
            )
            .await
            .unwrap();
        assert_eq!(output, contents);
        current
            .read(
                &current.resolve(b"/rw/clone").await.unwrap(),
                0,
                &mut output,
            )
            .await
            .unwrap();
        assert_eq!(output, replacement);
    });
}

#[cfg(feature = "write")]
#[test]
fn owned_mutations_preserve_readonly_subvolumes() {
    use super::writer::Attributes;
    use super::writer::{CloneRange, Name};

    block_on(async {
        let stamp = Timestamp::from_secs(1_700_000_000);
        let contents = b"read-only contents";
        let tree = TreeBuilder::new().root(Metadata::new(0o755, stamp)).file(
            "/data",
            contents.to_vec(),
            Metadata::new(0o644, stamp),
        );
        let (formatted, image) = filesystem_image_tree(true, true, tree);
        let tree = formatted.root().await.unwrap().tree();
        drop(formatted);
        let mut external =
            btrfs_transaction::filesystem::Filesystem::open(image.try_clone().unwrap()).unwrap();
        let mut fixture =
            btrfs_transaction::transaction::Transaction::start(&mut external).unwrap();
        fixture.set_root_readonly(&mut external, tree).unwrap();
        fixture.commit(&mut external).unwrap();
        drop(external);
        let mut writer = mount_writer(Disk(image)).await.unwrap();
        let base = writer.snapshot().unwrap();
        let handles = writer.join().await.unwrap();
        let root_handle = handles.root().await.unwrap();
        let file = handles
            .lookup(&root_handle, b"data")
            .await
            .unwrap()
            .unwrap();
        let name = |bytes| Name {
            parent: &root_handle,
            bytes,
        };
        let attributes = || Attributes {
            permissions: 0o644,
            uid: 0,
            gid: 0,
            umask: 0,
        };
        let when = crate::Timestamp {
            seconds: 1_700_000_100,
            nanoseconds: 0,
        };
        let concurrency = std::thread::available_parallelism().unwrap();
        macro_rules! refused {
            ($tx:ident, $operation:expr) => {{
                let mut $tx = writer.join().await.unwrap();
                assert!(matches!($operation.await, Err(WriteError::ReadOnlySubvolume { tree: rejected }) if rejected == tree));
                assert_eq!(writer.snapshot().unwrap().snapshot, base.snapshot);
            }};
        }
        refused!(
            tx,
            tx.write_file(
                &file,
                0,
                b"replacement",
                WriteOptions::plain(when, concurrency)
            )
        );
        refused!(
            tx,
            tx.truncate_file(&file, 0, WriteOptions::plain(when, concurrency))
        );
        refused!(tx, tx.set_times(&file, Some(when), Some(when), when));
        refused!(tx, tx.create_file(name(b"new"), attributes(), when));
        refused!(tx, tx.hard_link(name(b"alias"), &file, when));
        refused!(tx, tx.symlink(name(b"link"), b"data", attributes(), when));
        refused!(
            tx,
            tx.rename(name(b"data"), name(b"renamed"), RenameMode::Replace, when)
        );
        refused!(tx, tx.unlink_name(name(b"data"), when));
        refused!(
            tx,
            tx.clone_range(
                CloneRange {
                    source: &file,
                    destination: &file,
                    source_offset: 0,
                    destination_offset: 4096,
                    length: contents.len() as u64,
                },
                WriteOptions::plain(when, concurrency)
            )
        );
        let tx = writer.join().await.unwrap();
        let mut output = [0; 18];
        assert_eq!(
            tx.read_file(&file, 0, &mut output).await.unwrap(),
            contents.len()
        );
        assert_eq!(&output, contents);
        tx.commit().await.unwrap();
        assert_eq!(writer.snapshot().unwrap().snapshot, base.snapshot);
    });
}

#[cfg(feature = "write")]
#[test]
fn handles_reject_foreign_mounts_and_outlive_their_handle_and_commit() {
    use super::writer::Attributes;
    use super::writer::Name;
    block_on(async {
        let (_, image) = filesystem_image();
        let (_, other_image) = filesystem_image();
        let mut writer = mount_writer(Disk(image)).await.unwrap();
        let mut other = mount_writer(Disk(other_image)).await.unwrap();
        let other_tx = other.join().await.unwrap();
        let foreign = other_tx.root().await.unwrap();
        let base = writer.snapshot().unwrap();
        let when = crate::Timestamp {
            seconds: 1_700_000_100,
            nanoseconds: 0,
        };
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let attributes = || Attributes {
            permissions: 0o644,
            uid: 0,
            gid: 0,
            umask: 0,
        };
        assert!(matches!(
            tx.metadata(&foreign).await,
            Err(WriteError::ForeignHandle)
        ));
        assert!(matches!(
            tx.create_file(
                Name {
                    parent: &foreign,
                    bytes: b"foreign"
                },
                attributes(),
                when
            )
            .await,
            Err(WriteError::ForeignHandle)
        ));
        assert_eq!(writer.snapshot().unwrap().snapshot, base.snapshot);
        let name = Name {
            parent: &root,
            bytes: b"file",
        };
        let mut tx = writer.join().await.unwrap();
        let created = tx.create_file(name, attributes(), when).await.unwrap();
        // A rejected operation keeps the creation before it, and ending the
        // handle keeps both in the running transaction.
        assert!(matches!(
            tx.create_file(name, attributes(), when).await,
            Err(WriteError::Exists { .. })
        ));
        assert_ne!(writer.snapshot().unwrap().snapshot, base.snapshot);
        let mut tx = writer.join().await.unwrap();
        assert!(matches!(
            tx.create_file(name, attributes(), when).await,
            Err(WriteError::Exists { .. })
        ));
        let concurrency = std::thread::available_parallelism().unwrap();
        assert_eq!(
            tx.write_file(
                &created,
                0,
                "写入-書き込み-쓰기".as_bytes(),
                WriteOptions::plain(when, concurrency)
            )
            .await
            .unwrap(),
            "写入-書き込み-쓰기".len()
        );
        tx.commit().await.unwrap();
        // A handle made before the commit names the same inode after it.
        let tx = writer.join().await.unwrap();
        assert_eq!(
            tx.metadata(&created).await.unwrap().size,
            "写入-書き込み-쓰기".len() as u64
        );
    });
}

#[cfg(feature = "write")]
#[test]
fn handles_follow_commits_but_reject_reused_inode_numbers() {
    use super::writer::Attributes;
    use super::writer::Name;
    block_on(async {
        let (_, image) = filesystem_image();
        let mut writer = mount_writer(Disk(image)).await.unwrap();
        let when = crate::Timestamp {
            seconds: 1_700_000_100,
            nanoseconds: 0,
        };
        let concurrency = std::thread::available_parallelism().unwrap();
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let name = Name {
            parent: &root,
            bytes: b"file",
        };
        let attributes = || Attributes {
            permissions: 0o644,
            uid: 0,
            gid: 0,
            umask: 0,
        };
        let original = tx.create_file(name, attributes(), when).await.unwrap();
        tx.commit().await.unwrap();
        let mut tx = writer.join().await.unwrap();
        let old_generation = tx.metadata(&original).await.unwrap().generation;
        let _ = tx
            .write_file(&original, 0, b"old", WriteOptions::plain(when, concurrency))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let mut tx = writer.join().await.unwrap();
        assert_eq!(
            tx.metadata(&original).await.unwrap().generation,
            old_generation
        );
        tx.unlink_name(name, when).await.unwrap();
        assert!(matches!(
            tx.metadata(&original).await,
            Err(WriteError::StaleHandle { .. })
        ));
        let replacement = tx.create_file(name, attributes(), when).await.unwrap();
        assert_eq!(replacement.number(), original.number());
        assert_ne!(
            tx.metadata(&replacement).await.unwrap().generation,
            old_generation
        );
        assert!(matches!(
            tx.metadata(&original).await,
            Err(WriteError::StaleHandle { .. })
        ));
        tx.commit().await.unwrap();
        let mut tx = writer.join().await.unwrap();
        assert!(matches!(
            tx.metadata(&original).await,
            Err(WriteError::StaleHandle { .. })
        ));
        let resolved = tx.lookup(&root, b"file").await.unwrap().unwrap();
        assert_eq!(
            tx.metadata(&resolved).await.unwrap(),
            tx.metadata(&replacement).await.unwrap()
        );
        tx.unlink_name(name, when).await.unwrap();
        let newer = tx.create_file(name, attributes(), when).await.unwrap();
        assert_eq!(newer.number(), replacement.number());
        // A second creation in this same generation must not reuse its identity.
        tx.unlink_name(name, when).await.unwrap();
        let newest = tx.create_file(name, attributes(), when).await.unwrap();
        assert_ne!(newest.number(), newer.number());
        assert!(matches!(
            tx.metadata(&newer).await,
            Err(WriteError::StaleHandle { .. })
        ));
        assert!(matches!(
            tx.write_file(
                &original,
                0,
                b"wrong file",
                WriteOptions::plain(when, concurrency)
            )
            .await,
            Err(WriteError::StaleHandle { .. })
        ));
        let current = writer.snapshot().unwrap();
        assert_eq!(current.resolve(b"/file").await.unwrap().metadata().size, 0);
    });
}

#[cfg(feature = "write")]
#[test]
fn owned_mutations_commit_as_one_generation() {
    use super::writer::Attributes;
    use super::writer::{CloneRange, Name};

    block_on(async {
        let (_, image) = filesystem_image();
        let mut writer = mount_writer(Disk(image)).await.unwrap();
        let base = writer.snapshot().unwrap();
        let root = base.root().await.unwrap();
        let when = crate::Timestamp {
            seconds: 1_700_000_100,
            nanoseconds: 123,
        };
        let concurrency = std::thread::available_parallelism().unwrap();
        let mut tx = writer.join().await.unwrap();
        let root_handle = tx.root().await.unwrap();
        let name = |bytes| Name {
            parent: &root_handle,
            bytes,
        };
        let attributes = || Attributes {
            permissions: 0o644,
            uid: 1000,
            gid: 1000,
            umask: 0,
        };

        let source = tx
            .create_file(name(b"source"), attributes(), when)
            .await
            .unwrap();
        let removed = tx
            .create_file(name(b"temporary"), attributes(), when)
            .await
            .unwrap();
        tx.unlink_name(name(b"temporary"), when).await.unwrap();
        let replacement = tx
            .create_file(name(b"temporary"), attributes(), when)
            .await
            .unwrap();
        assert!(replacement.number() > removed.number());
        tx.unlink_name(name(b"temporary"), when).await.unwrap();
        let bytes = vec![0x53; 16384];
        let count = tx
            .write_file(&source, 0, &bytes, WriteOptions::plain(when, concurrency))
            .await
            .unwrap();
        assert_eq!(count, bytes.len());
        tx.truncate_file(&source, 8192, WriteOptions::plain(when, concurrency))
            .await
            .unwrap();
        let destination = tx
            .create_file(name(b"clone"), attributes(), when)
            .await
            .unwrap();
        tx.clone_range(
            CloneRange {
                source: &source,
                destination: &destination,
                source_offset: 0,
                destination_offset: 0,
                length: 8192,
            },
            WriteOptions::plain(when, concurrency),
        )
        .await
        .unwrap();
        tx.hard_link(name(b"alias"), &source, when).await.unwrap();
        tx.rename(name(b"source"), name(b"renamed"), RenameMode::Replace, when)
            .await
            .unwrap();
        let _ = tx
            .symlink(name(b"link"), b"renamed", attributes(), when)
            .await
            .unwrap();
        tx.unlink_name(name(b"alias"), when).await.unwrap();
        let access = crate::Timestamp {
            seconds: 1_600_000_000,
            nanoseconds: 456,
        };
        let modified = crate::Timestamp {
            seconds: 1_650_000_000,
            nanoseconds: 789,
        };
        tx.set_times(&source, Some(access), None, when)
            .await
            .unwrap();
        tx.set_times(&destination, None, Some(modified), when)
            .await
            .unwrap();
        let mut output = vec![0; 8192];
        assert_eq!(
            tx.read_file(&destination, 0, &mut output).await.unwrap(),
            output.len()
        );
        assert_eq!(output, bytes[..8192]);
        tx.commit().await.unwrap();
        let current = writer.snapshot().unwrap();
        assert_eq!(
            current.superblock().generation(),
            base.superblock().generation() + 1
        );
        for path in [b"/renamed".as_slice(), b"/clone".as_slice()] {
            let inode = current.resolve(path).await.unwrap();
            current.read(&inode, 0, &mut output).await.unwrap();
            assert_eq!(output, bytes[..8192]);
        }
        let source_metadata = *current.resolve(b"/renamed").await.unwrap().metadata();
        let clone_metadata = *current.resolve(b"/clone").await.unwrap().metadata();
        assert_eq!(source_metadata.atime, access);
        assert_eq!(source_metadata.mtime, when);
        assert_eq!(source_metadata.ctime, when);
        assert_eq!(clone_metadata.atime, when);
        assert_eq!(clone_metadata.mtime, modified);
        assert_eq!(clone_metadata.ctime, when);
        let link = current
            .lookup(&current.root().await.unwrap(), b"link")
            .await
            .unwrap()
            .unwrap();
        let mut target = [0; 7];
        assert_eq!(current.read_link(&link, &mut target).await.unwrap(), 7);
        assert_eq!(&target, b"renamed");
        assert!(base.lookup(&root, b"renamed").await.unwrap().is_none());
        assert!(
            current
                .lookup(&current.root().await.unwrap(), b"alias")
                .await
                .unwrap()
                .is_none()
        );
    });
}

#[cfg(feature = "write")]
#[test]
fn failed_and_canceled_mutations_restore_only_their_operation() {
    exercise_operation_rollback(|_| {});
}

#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn operation_rollback_passes_btrfs_check() {
    exercise_operation_rollback(assert_btrfs_check);
}

fn exercise_operation_rollback(mut verify: impl FnMut(&std::path::Path)) {
    use super::writer::Attributes;
    use super::writer::Name;
    use crate::source::BlockSink;

    use core::future::pending;
    use futures_lite::future::poll_once;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct InterruptedDisk {
        disk: Disk,
        stage_action: Arc<AtomicUsize>,
        entered: Arc<AtomicUsize>,
    }
    impl BlockSource for InterruptedDisk {
        fn size_bytes(&self) -> u64 {
            self.disk.size_bytes()
        }
        async fn read_at(&self, offset: u64, buf: &mut [u8]) -> IoResult<()> {
            self.disk.read_at(offset, buf).await
        }
    }
    impl BlockSink for InterruptedDisk {
        async fn stage(&self, block: u64, bytes: &[u8; 4096]) -> IoResult<()> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.disk.stage(block, bytes).await?;
            match self.stage_action.load(Ordering::SeqCst) {
                0 => Ok(()),
                1 => Err(IoError::DeviceFault),
                2 => pending().await,
                _ => panic!("unknown fixture stage action"),
            }
        }
        async fn commit(&self, block: u64, bytes: &[u8; 4096]) -> IoResult<()> {
            self.disk.commit(block, bytes).await
        }
    }
    block_on(async {
        for (action, private) in [(1, false), (2, false), (1, true), (2, true)] {
            let stamp = Timestamp::from_secs(1_700_000_000);
            let original = "中文・日本語・한국어".as_bytes();
            let contents = if private {
                original.repeat(512)
            } else {
                original.to_vec()
            };
            let tree = TreeBuilder::new().root(Metadata::new(0o755, stamp)).file(
                "/data",
                original.to_vec(),
                Metadata::new(0o644, stamp),
            );
            let named = tempfile::NamedTempFile::new().unwrap();
            let (_, image) = filesystem_image_options(
                tree,
                named.reopen().unwrap(),
                FormatOptions::new([0x53; 16], stamp),
            );
            verify(named.path());
            let stage_action = Arc::new(AtomicUsize::new(0));
            let entered = Arc::new(AtomicUsize::new(0));
            let mut writer = mount_writer(InterruptedDisk {
                disk: Disk(image),
                stage_action: stage_action.clone(),
                entered: entered.clone(),
            })
            .await
            .unwrap();
            let base = writer.snapshot().unwrap();
            let handles = writer.join().await.unwrap();
            let root_handle = handles.root().await.unwrap();
            let file = handles
                .lookup(&root_handle, b"data")
                .await
                .unwrap()
                .unwrap();
            let when = crate::Timestamp {
                seconds: 1_700_000_100,
                nanoseconds: 123,
            };
            let concurrency = std::thread::available_parallelism().unwrap();
            let name = Name {
                parent: &root_handle,
                bytes: b"pending",
            };
            let attributes = || Attributes {
                permissions: 0o644,
                uid: 1000,
                gid: 1000,
                umask: 0,
            };
            let mut tx = writer.join().await.unwrap();
            let _ = tx.create_file(name, attributes(), when).await.unwrap();
            if private {
                tx.write_file(&file, 0, &contents, WriteOptions::plain(when, concurrency))
                    .await
                    .unwrap();
            }
            stage_action.store(action, Ordering::SeqCst);
            entered.store(0, Ordering::SeqCst);
            let payload = vec![0x71; contents.len().max(8192)];
            // Fail after bytes reach storage, covering both inline promotion
            // and replacement of an extent allocated earlier in this batch.
            if action == 1 {
                assert!(
                    tx.write_file(&file, 0, &payload, WriteOptions::plain(when, concurrency))
                        .await
                        .is_err()
                );
            } else {
                let mut write = core::pin::pin!(tx.write_file(
                    &file,
                    0,
                    &payload,
                    WriteOptions::plain(when, concurrency)
                ));
                assert!(poll_once(write.as_mut()).await.is_none());
            }
            assert!(entered.load(Ordering::SeqCst) > 0);
            assert!(tx.lookup(&root_handle, b"pending").await.unwrap().is_some());
            let mut output = vec![0; contents.len()];
            tx.read_file(&file, 0, &mut output).await.unwrap();
            assert_eq!(output, contents);
            stage_action.store(0, Ordering::SeqCst);
            // A succeeding replacement must release the held private extent,
            // while leaving allocation and free-space accounting consistent.
            tx.write_file(&file, 0, &contents, WriteOptions::plain(when, concurrency))
                .await
                .unwrap();
            // The earlier creation survived, and the same transaction remains
            // usable after either an I/O error or cancellation mid-mutation.
            tx.create_file(
                Name {
                    parent: &root_handle,
                    bytes: b"after",
                },
                attributes(),
                when,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            let current = writer.snapshot().unwrap();
            assert_eq!(
                current.superblock().generation(),
                base.superblock().generation() + 1
            );
            assert!(
                current
                    .lookup(&current.root().await.unwrap(), b"pending")
                    .await
                    .unwrap()
                    .is_some()
            );
            current
                .read(&current.resolve(b"/data").await.unwrap(), 0, &mut output)
                .await
                .unwrap();
            assert_eq!(output, contents);
            verify(named.path());
        }
    });
}

#[cfg(feature = "write")]
#[test]
fn a_dropped_commit_poisons_the_writer_in_staging_and_in_publication() {
    use crate::source::BlockSink;
    use core::future::pending;

    use futures_lite::future::poll_once;

    struct PausingDisk {
        disk: Disk,
        pause_stage: bool,
    }
    impl BlockSource for PausingDisk {
        fn size_bytes(&self) -> u64 {
            self.disk.size_bytes()
        }
        async fn read_at(&self, offset: u64, buf: &mut [u8]) -> IoResult<()> {
            self.disk.read_at(offset, buf).await
        }
    }
    impl BlockSink for PausingDisk {
        async fn stage(&self, block: u64, bytes: &[u8; 4096]) -> IoResult<()> {
            if self.pause_stage {
                pending::<()>().await;
            }
            self.disk.stage(block, bytes).await
        }
        async fn commit(&self, _: u64, _: &[u8; 4096]) -> IoResult<()> {
            pending().await
        }
    }
    block_on(async {
        for pause_stage in [true, false] {
            let (_, image) = filesystem_image();
            let mut writer = mount_writer(PausingDisk {
                disk: Disk(image),
                pause_stage,
            })
            .await
            .unwrap();
            let old = writer.snapshot().unwrap();
            let mut transaction = writer.join().await.unwrap();
            edit_root_mode(&old, transaction.edits()).await.unwrap();
            {
                let mut commit = core::pin::pin!(transaction.commit());
                assert!(poll_once(commit.as_mut()).await.is_none());
            }
            // The running transaction's changes were the writer's to make
            // durable; once its commit is gone they cannot be withdrawn.
            assert!(matches!(writer.snapshot(), Err(WriteError::WriterPoisoned)));
            assert!(matches!(
                writer.join().await,
                Err(WriteError::WriterPoisoned)
            ));
            assert_eq!(old.root().await.unwrap().metadata().permissions, 0o755);
        }
    });
}

#[cfg(feature = "write")]
#[test]
fn successful_commits_publish_live_views_without_remounting() {
    block_on(async {
        let (mut fs, image) = filesystem_image();
        let initial = fs.superblock().generation();
        let old = fs.root().await.unwrap();
        let mut retirements = crate::retirement::Retirements::default();
        for generation in initial + 1..=initial + 3 {
            let (_, committed) = change_root_mode_with_retirements(&fs, &mut retirements)
                .await
                .unwrap();
            fs.publish(committed);
            assert_eq!(fs.superblock().generation(), generation);
            assert_eq!(fs.root().await.unwrap().metadata().permissions, 0o700);
            assert_eq!(
                fs.load_inode(old.root, old.number, &old.snapshot)
                    .await
                    .unwrap(),
                old
            );
            let independent = Filesystem::mount(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            assert_eq!(fs.snapshot, independent.snapshot);
        }
    });
}

#[cfg(feature = "write")]
#[test]
fn transaction_io_failures_recover_a_complete_generation() {
    block_on(async {
        // First record the complete write sequence, then fail each boundary.
        let mut count = 0;
        let mut first_commit = 0;
        for fail_at in core::iter::once(usize::MAX).chain(1..) {
            if fail_at != usize::MAX && fail_at > count {
                break;
            }
            let (_, image) = filesystem_image();
            let operations = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let fs = Filesystem::mount(FaultDisk {
                disk: Disk(image.try_clone().unwrap()),
                fail_at,
                operations: operations.clone(),
                poisoned: std::sync::atomic::AtomicBool::new(false),
            })
            .await
            .unwrap();
            let generation = fs.superblock().generation();
            let result = change_root_mode(&fs).await;
            let operations = operations.lock().unwrap().clone();
            if fail_at == usize::MAX {
                result.unwrap();
                count = operations.len();
                first_commit = operations.iter().position(|(commit, _)| *commit).unwrap() + 1;
                assert!(
                    operations[..first_commit - 1]
                        .iter()
                        .all(|(commit, _)| !commit)
                );
                let published: Vec<_> = operations[first_commit - 1..]
                    .iter()
                    .map(|(commit, block)| {
                        assert!(*commit);
                        block * crate::source::READ_ALIGNMENT
                    })
                    .collect();
                assert_eq!(published, vec![64 << 20, 64 << 10]);
            } else {
                assert!(result.is_err(), "operation {fail_at}");
                assert_eq!(operations.len(), fail_at);
            }
            let recovered = Filesystem::mount(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            let published = fail_at == usize::MAX;
            assert_eq!(
                recovered.superblock().generation(),
                generation + u64::from(published)
            );
            assert_eq!(
                recovered
                    .resolve(b"/")
                    .await
                    .unwrap()
                    .metadata()
                    .permissions,
                if published { 0o700 } else { 0o755 }
            );
            // A torn primary selects the newest valid backup. If its commit
            // completed before failure, all of that generation must be readable.
            image
                .write_all_at(&[0; 4096], crate::superblock::MIRROR_OFFSETS[0])
                .unwrap();
            let recovered = Filesystem::mount(Disk(image)).await.unwrap();
            let backup_published = fail_at == usize::MAX || fail_at > first_commit;
            assert_eq!(
                recovered.superblock().generation(),
                generation + u64::from(backup_published)
            );
            assert_eq!(
                recovered
                    .resolve(b"/")
                    .await
                    .unwrap()
                    .metadata()
                    .permissions,
                if backup_published { 0o700 } else { 0o755 }
            );
        }
    });
}

#[cfg(feature = "write")]
#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn extent_cuts_pass_btrfs_check() {
    use btrfs_disk::items::CompressionType;
    for compression in [
        None,
        Some(CompressionType::Zlib),
        Some(CompressionType::Lzo),
        Some(CompressionType::Zstd),
    ] {
        block_on(exercise_extent_cuts(compression, false, assert_btrfs_check));
    }
    block_on(exercise_extent_cuts(None, true, assert_btrfs_check));
}

#[cfg(feature = "write")]
pub(super) fn assert_btrfs_check(path: &std::path::Path) {
    let output = std::process::Command::new("btrfs")
        .args(["check", "--readonly"])
        .arg(path)
        .output()
        .expect("btrfs-progs must be installed");
    assert!(
        output.status.success(),
        "btrfs check\n{}\n{}",
        std::string::String::from_utf8_lossy(&output.stdout),
        std::string::String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "write")]
#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn committed_transaction_passes_btrfs_check() {
    use std::io::{Seek, SeekFrom};

    block_on(async {
        for (separate, free_tree) in [(true, true), (false, true), (false, false)] {
            let (_, mut image) = filesystem_image_with_features(separate, free_tree);
            let mut named = tempfile::NamedTempFile::new().unwrap();
            image.seek(SeekFrom::Start(0)).unwrap();
            std::io::copy(&mut image, named.as_file_mut()).unwrap();
            for commit in [false, true] {
                if commit {
                    let fs = Filesystem::mount(Disk(named.reopen().unwrap()))
                        .await
                        .unwrap();
                    change_root_mode(&fs).await.unwrap();
                }
                assert_btrfs_check(named.path());
            }
        }
    });
}

#[cfg(feature = "write")]
#[test]
fn old_inodes_read_their_own_extent_and_checksum_generation() {
    use crate::accounting::DataChange;
    use crate::allocation::data::DataReference;
    use crate::ondisk::{RawFileExtentBody, RawFileExtentHead, item_type, view};
    use crate::retirement::Retirements;
    use crate::transaction::Transaction;
    use zerocopy::IntoBytes;

    block_on(async {
        let before = vec![0x53; 32768];
        let after = vec![0xa7; before.len()];
        let timestamp = Timestamp::from_secs(1_700_000_000);
        let tree = TreeBuilder::new()
            .root(Metadata::new(0o755, timestamp))
            .file("/data.bin", before.clone(), Metadata::new(0o644, timestamp));
        let (fs, image) = filesystem_image_tree(true, true, tree);
        let old_dir = fs.root().await.unwrap();
        let old_file = fs.lookup(&old_dir, b"data.bin").await.unwrap().unwrap();
        let mut retirements = Retirements::default();
        let mut transaction = Transaction::new(
            fs.volume.view(&fs.snapshot.mapping),
            fs.snapshot.clone(),
            &mut retirements,
            &allocation_of(&fs).await,
        )
        .await
        .unwrap();
        let root = old_file.root;
        let key = Key::new(old_file.number(), item_type::EXTENT_DATA, 0);
        let mut bytes = transaction
            .trees
            .get(&fs.volume, root, key)
            .await
            .unwrap()
            .unwrap();
        let mut head = *view::<RawFileExtentHead>(&bytes).unwrap();
        let head_len = size_of::<RawFileExtentHead>();
        let mut body = *view::<RawFileExtentBody>(&bytes[head_len..]).unwrap();
        assert_eq!(head.extent_type, 1);
        assert_eq!(head.compression, 0);
        assert_eq!(body.offset.get(), 0);
        assert_eq!(body.num_bytes.get(), before.len() as u64);
        let old_data = Key::new(
            body.disk_bytenr.get(),
            item_type::EXTENT_ITEM,
            body.disk_num_bytes.get(),
        );
        let reference = DataReference {
            root: root.tree,
            inode: old_file.number(),
            offset: 0,
        };
        let concurrency = std::thread::available_parallelism().unwrap();
        let allocation = transaction
            .accounting
            .stage_data(
                &fs.volume,
                &mut transaction.trees,
                &mut transaction.space,
                crate::accounting::DataWrite {
                    bytes: &after,
                    reference,
                    checksummed: true,
                },
                concurrency,
            )
            .await
            .unwrap();
        head.generation.set(transaction.trees.generation());
        body.disk_bytenr.set(allocation.objectid);
        body.disk_num_bytes.set(allocation.offset);
        bytes[..head_len].copy_from_slice(head.as_bytes());
        bytes[head_len..].copy_from_slice(body.as_bytes());
        let root = transaction
            .trees
            .apply(&fs.volume, root, key, Some(&bytes), &mut transaction.space)
            .await
            .unwrap();
        transaction.roots.set(root).unwrap();
        transaction
            .accounting
            .update_data(
                &fs.volume,
                &mut transaction.trees,
                &mut transaction.space,
                DataChange {
                    key: old_data,
                    reference,
                    delta: -1,
                },
            )
            .await
            .unwrap();
        let mut preview = vec![0; after.len()];
        assert_eq!(
            transaction
                .read_file(old_file.tree(), old_file.number(), 0, &mut preview)
                .await
                .unwrap(),
            after.len()
        );
        assert_eq!(preview, after);
        fs.read(&old_file, 0, &mut preview).await.unwrap();
        assert_eq!(preview, before);
        transaction
            .commit(
                &mut retirements,
                || panic!("valid fixture UUID"),
                concurrency,
            )
            .await
            .unwrap();
        drop(fs);

        let current = Filesystem::mount(Disk(image)).await.unwrap();
        let new_file = current.resolve(b"/data.bin").await.unwrap();
        let mut output = vec![0; after.len()];
        assert_eq!(
            current.read(&new_file, 0, &mut output).await.unwrap(),
            after.len()
        );
        assert_eq!(output, after);
        // The new checksum tree has removed the old allocation's checksums.
        assert!(matches!(
            current
                .lookup_checksums(&current.snapshot, old_data.objectid, before.len())
                .await,
            Err(crate::Error::MissingDataChecksum { .. })
        ));
        assert_eq!(
            current.read(&old_file, 0, &mut output).await.unwrap(),
            before.len()
        );
        assert_eq!(output, before);
        // Descendants opened through an old directory must retain that view too.
        let from_old_dir = current
            .lookup(&old_dir, b"data.bin")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from_old_dir, old_file);
        assert_eq!(
            current.read(&from_old_dir, 0, &mut output).await.unwrap(),
            before.len()
        );
        assert_eq!(output, before);
    });
}

#[cfg(feature = "write")]
#[test]
fn old_inode_clones_pin_retired_blocks_across_multiple_commits() {
    use crate::retirement::Retirements;
    use crate::space::Space;
    use crate::transaction::Transaction;

    fn allocates(space: &mut Space, wanted: u64, nodesize: u64) -> bool {
        let count = space.available_metadata() / nodesize;
        let mut found = false;
        for _ in 0..count {
            let address = space
                .allocate(crate::ondisk::objectid::FS_TREE, 0, Key::new(0, 0, 0))
                .unwrap();
            found |= address == wanted;
        }
        found
    }

    block_on(async {
        let (first, image) = filesystem_image();
        let inode = first.root().await.unwrap();
        let clone = inode.clone();
        let old_block = inode.root.bytenr;
        let mut retirements = Retirements::default();
        change_root_mode_with_retirements(&first, &mut retirements)
            .await
            .unwrap();
        drop(first);
        drop(inode);
        let second = Filesystem::mount(Disk(image.try_clone().unwrap()))
            .await
            .unwrap();
        change_root_mode_with_retirements(&second, &mut retirements)
            .await
            .unwrap();
        drop(second);
        let current = Filesystem::mount(Disk(image)).await.unwrap();
        let raw_space = Space::load(&current.volume, current.snapshot.root_tree)
            .await
            .unwrap();
        let raw_free = raw_space
            .committed_free(&rpds::RedBlackTreeMapSync::new_sync())
            .unwrap();
        {
            let mut transaction = Transaction::new(
                current.volume.view(&current.snapshot.mapping),
                current.snapshot.clone(),
                &mut retirements,
                &allocation_of(&current).await,
            )
            .await
            .unwrap();
            // Excluding reader pins must not change on-disk free-space accounting.
            let pinned_free = transaction
                .space
                .committed_free(&rpds::RedBlackTreeMapSync::new_sync())
                .unwrap();
            assert_eq!(raw_free.len(), pinned_free.len());
            for (raw, pinned) in raw_free.iter().zip(&pinned_free) {
                assert_eq!(raw.range, pinned.range);
                assert_eq!(raw.free, pinned.free);
            }
            assert!(!allocates(
                &mut transaction.space,
                old_block,
                u64::from(current.superblock().nodesize())
            ));
        }
        drop(clone);
        let mut transaction = Transaction::new(
            current.volume.view(&current.snapshot.mapping),
            current.snapshot.clone(),
            &mut retirements,
            &allocation_of(&current).await,
        )
        .await
        .unwrap();
        assert!(allocates(
            &mut transaction.space,
            old_block,
            u64::from(current.superblock().nodesize())
        ));
    });
}

/// Deterministic, incompressible file content.
#[cfg(feature = "write")]
fn noise(length: usize, seed: u8) -> Vec<u8> {
    let mut state = u32::from(seed) + 1;
    (0..length)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

#[cfg(feature = "write")]
#[test]
fn carried_allocation_state_equals_a_reload_after_every_commit() {
    use super::writer::{Attributes, CloneRange, Name, Writer};
    use crate::compress::write::{Plain, Zstd};
    use core::num::NonZeroUsize;

    // Extent-tree groups and a separate block-group tree, with and without
    // the free-space tree.
    for (separate, free_tree) in [(true, true), (false, true), (false, false)] {
        block_on(async {
            let (_, image) = filesystem_image_with_features(separate, free_tree);
            let mut writer = mount_writer(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            let concurrency = NonZeroUsize::MIN;
            let when = crate::Timestamp {
                seconds: 1_700_000_100,
                nanoseconds: 0,
            };
            let attributes = Attributes {
                permissions: 0o644,
                uid: 0,
                gid: 0,
                umask: 0,
            };
            let check = |writer: &Writer<Disk, FixtureUuids>, step: &'static str| {
                let image = image.try_clone().unwrap();
                let carried = writer.committed_allocation().clone();
                async move {
                    let fresh = Filesystem::mount(Disk(image)).await.unwrap();
                    assert!(
                        carried == allocation_of(&fresh).await,
                        "carried allocation state diverged from the disk image after {step} \
                         (separate block-group tree {separate}, free-space tree {free_tree})"
                    );
                }
            };
            check(&writer, "mount").await;

            // Directory names cover Chinese, Japanese and Korean.
            let directory = "目录-ディレクトリ-디렉터리".as_bytes();
            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            let directory = tx
                .create_directory(
                    Name {
                        parent: &root,
                        bytes: directory,
                    },
                    attributes,
                    when,
                )
                .await
                .unwrap();
            let name = |bytes| Name {
                parent: &directory,
                bytes,
            };
            let first = tx
                .create_file(name(b"a.bin"), attributes, when)
                .await
                .unwrap();
            tx.write_file(
                &first,
                0,
                &noise(300_000, 1),
                WriteOptions::plain(when, concurrency),
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            check(&writer, "create").await;

            // A reader of this generation keeps the next commit's retired
            // blocks pinned; the committed form must not depend on that.
            let reader = writer.snapshot().unwrap();
            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            let directory = tx
                .lookup(&root, "目录-ディレクトリ-디렉터리".as_bytes())
                .await
                .unwrap()
                .unwrap();
            let first = tx.lookup(&directory, b"a.bin").await.unwrap().unwrap();
            tx.write_file(
                &first,
                100_000,
                &noise(80_000, 2),
                WriteOptions::plain(when, concurrency),
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            check(&writer, "overwrite").await;

            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            let directory = tx
                .lookup(&root, "目录-ディレクトリ-디렉터리".as_bytes())
                .await
                .unwrap()
                .unwrap();
            let compressed = tx
                .create_file(
                    Name {
                        parent: &directory,
                        bytes: "圧縮-压缩-압축".as_bytes(),
                    },
                    attributes,
                    when,
                )
                .await
                .unwrap();
            let text = "btrfs 写入 書き込み 쓰기 ".repeat(10_000).into_bytes();
            tx.write_file(
                &compressed,
                0,
                &text,
                WriteOptions {
                    when,
                    concurrency,
                    encoder: Zstd::new(3).unwrap(),
                    max_inline: DEFAULT_MAX_INLINE,
                },
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            check(&writer, "compressed write").await;

            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            let directory = tx
                .lookup(&root, "目录-ディレクトリ-디렉터리".as_bytes())
                .await
                .unwrap()
                .unwrap();
            let first = tx.lookup(&directory, b"a.bin").await.unwrap().unwrap();
            let clone = tx
                .create_file(
                    Name {
                        parent: &directory,
                        bytes: b"clone.bin",
                    },
                    attributes,
                    when,
                )
                .await
                .unwrap();
            tx.clone_range(
                CloneRange {
                    source: &first,
                    destination: &clone,
                    source_offset: 0,
                    destination_offset: 0,
                    length: 131_072,
                },
                WriteOptions::plain(when, concurrency),
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            check(&writer, "reflink").await;

            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            let directory = tx
                .lookup(&root, "目录-ディレクトリ-디렉터리".as_bytes())
                .await
                .unwrap()
                .unwrap();
            tx.unlink_name(
                Name {
                    parent: &directory,
                    bytes: b"a.bin",
                },
                when,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            check(&writer, "unlink of a cloned file").await;

            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            let directory = tx
                .lookup(&root, "目录-ディレクトリ-디렉터리".as_bytes())
                .await
                .unwrap()
                .unwrap();
            let clone = tx.lookup(&directory, b"clone.bin").await.unwrap().unwrap();
            tx.truncate_file(&clone, 4096, WriteOptions::plain(when, concurrency))
                .await
                .unwrap();
            tx.unlink_name(
                Name {
                    parent: &directory,
                    bytes: "圧縮-压缩-압축".as_bytes(),
                },
                when,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            check(&writer, "truncate and unlink").await;

            // Changes the running transaction holds leave the carried form,
            // which is the committed generation's, in place.
            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            let doomed = tx
                .create_file(
                    Name {
                        parent: &root,
                        bytes: b"doomed",
                    },
                    attributes,
                    when,
                )
                .await
                .unwrap();
            tx.write_file(
                &doomed,
                0,
                &noise(50_000, 3),
                WriteOptions {
                    when,
                    concurrency,
                    encoder: Plain,
                    max_inline: DEFAULT_MAX_INLINE,
                },
            )
            .await
            .unwrap();
            check(&writer, "uncommitted").await;
            drop(reader);
        });
    }
}

/// What a device layout looks like on disk, read back through a fresh mount.
#[cfg(feature = "write")]
struct Layout {
    /// Each chunk's logical start, length and type, from the chunk tree.
    chunks: Vec<(u64, u64, u64)>,
    /// Each chunk's physical copies.
    copies: BTreeMap<u64, Vec<u64>>,
    /// Bytes in the bootstrap chunk array.
    sys_chunk_array: usize,
    /// Logical starts of the chunks the bootstrap array carries.
    bootstrap: Vec<u64>,
}

/// Asserts that the chunk tree, the dev tree, both device items, the
/// bootstrap chunk array and the block-group items describe one consistent
/// device, as a fresh mount reads them, and returns what they describe.
#[cfg(feature = "write")]
async fn assert_device_layout(image: &File) -> Layout {
    use crate::ondisk::{
        RawChunk, RawDevItem, RawKey, RawStripe, RawSuperblock, SYS_CHUNK_ARRAY_OFFSET, item_type,
        objectid, view,
    };
    use crate::tree::Cursor;

    let fs = Filesystem::mount(Disk(image.try_clone().unwrap()))
        .await
        .unwrap();
    let device_bytes = fs.superblock().device_bytes();
    let devid = fs.superblock().devid();
    let selected = crate::superblock::select(&Disk(image.try_clone().unwrap()))
        .await
        .unwrap();
    let raw: RawSuperblock = *view(&selected.bytes[..]).unwrap();

    let chunk_pointer = fs.superblock().chunk_tree();
    let chunk_root = TreeRoot {
        tree: objectid::CHUNK_TREE,
        bytenr: chunk_pointer.bytenr,
        level: chunk_pointer.level,
        generation: chunk_pointer.generation,
    };
    let dev_key = Key::new(objectid::DEV_ITEMS, item_type::DEV_ITEM, devid);
    let cursor = Cursor::ceiling(&fs.volume, chunk_root, dev_key)
        .await
        .unwrap();
    assert_eq!(cursor.key(), Some(dev_key));
    let dev: RawDevItem = *view(cursor.item().unwrap()).unwrap();

    let mut chunks = vec![];
    let mut copies = BTreeMap::new();
    let mut stripes = vec![];
    let mut cursor = Cursor::ceiling(
        &fs.volume,
        chunk_root,
        Key::new(objectid::FIRST_CHUNK_TREE, item_type::CHUNK_ITEM, 0),
    )
    .await
    .unwrap();
    while let (Some(key), Some(item)) = (cursor.key(), cursor.item()) {
        if !key.is(objectid::FIRST_CHUNK_TREE, item_type::CHUNK_ITEM) {
            break;
        }
        let chunk: &RawChunk = view(item).unwrap();
        chunks.push((key.offset, chunk.length.get(), chunk.chunk_type.get()));
        let mut physical = vec![];
        for stripe in item[size_of::<RawChunk>()..].chunks(size_of::<RawStripe>()) {
            let stripe: &RawStripe = view(stripe).unwrap();
            assert_eq!(stripe.devid.get(), devid);
            assert_eq!(stripe.dev_uuid, dev.uuid, "stripe names the device's UUID");
            physical.push(stripe.offset.get());
            stripes.push((stripe.offset.get(), chunk.length.get(), key.offset));
        }
        assert_eq!(usize::from(chunk.num_stripes.get()), physical.len());
        copies.insert(key.offset, physical);
        cursor.advance(&fs.volume).await.unwrap();
    }

    let dev_root = super::find_root(&fs.volume, fs.snapshot.root_tree, objectid::DEV_TREE)
        .await
        .unwrap()
        .root;
    let mut extents = vec![];
    let mut cursor = Cursor::ceiling(
        &fs.volume,
        dev_root,
        Key::new(devid, item_type::DEV_EXTENT, 0),
    )
    .await
    .unwrap();
    while let (Some(key), Some(item)) = (cursor.key(), cursor.item()) {
        if !key.is(devid, item_type::DEV_EXTENT) {
            break;
        }
        let word = |at: usize| u64::from_le_bytes(item[at..at + 8].try_into().unwrap());
        assert_eq!(word(0), objectid::CHUNK_TREE);
        assert_eq!(word(8), objectid::FIRST_CHUNK_TREE);
        extents.push((key.offset, word(24), word(16)));
        cursor.advance(&fs.volume).await.unwrap();
    }
    // Every copy of every chunk has its device extent, and nothing else does.
    stripes.sort_unstable();
    assert_eq!(extents, stripes);
    let mut end = 1 << 20;
    for &(start, length, _) in &extents {
        assert!(
            start >= end,
            "device extents overlap or sit in the first MiB"
        );
        end = start + length;
    }
    assert!(end <= device_bytes);
    let allocated: u64 = extents.iter().map(|&(_, length, _)| length).sum();
    assert_eq!(dev.bytes_used.get(), allocated, "chunk-tree device item");
    assert_eq!(
        raw.dev_item.bytes_used.get(),
        allocated,
        "superblock device item"
    );

    let array_length = raw.sys_chunk_array_size.get() as usize;
    let array = &selected.bytes[SYS_CHUNK_ARRAY_OFFSET..][..array_length];
    let mut bootstrap = vec![];
    let mut at = 0;
    while at < array.len() {
        let key: &RawKey = view(&array[at..]).unwrap();
        assert_eq!(key.objectid.get(), objectid::FIRST_CHUNK_TREE);
        assert_eq!(key.item_type, item_type::CHUNK_ITEM);
        let chunk: &RawChunk = view(&array[at + size_of::<RawKey>()..]).unwrap();
        bootstrap.push(key.offset.get());
        at += size_of::<RawKey>()
            + size_of::<RawChunk>()
            + usize::from(chunk.num_stripes.get()) * size_of::<RawStripe>();
    }
    assert_eq!(at, array.len());
    let system: Vec<u64> = chunks
        .iter()
        .filter(|&&(_, _, flags)| flags & 2 != 0)
        .map(|&(logical, ..)| logical)
        .collect();
    assert_eq!(
        bootstrap, system,
        "bootstrap array carries every SYSTEM chunk"
    );

    assert_eq!(
        allocation_of(&fs).await.space.group_summary(),
        chunks,
        "block-group items match the chunk tree"
    );
    Layout {
        chunks,
        copies,
        sys_chunk_array: array_length,
        bootstrap,
    }
}

#[cfg(feature = "write")]
fn count_kind(layout: &Layout, kind: u64) -> usize {
    layout
        .chunks
        .iter()
        .filter(|&&(_, _, flags)| flags & kind != 0)
        .count()
}

#[cfg(feature = "write")]
fn profile_image(
    metadata: ferrosys::btrfs::Profile,
    data: ferrosys::btrfs::Profile,
) -> (Filesystem<Disk>, File) {
    let timestamp = Timestamp::from_secs(1_700_000_000);
    let mut options = FormatOptions::new([0x53; 16], timestamp);
    options.chunk_tree_uuid = [0x71; 16];
    options.device_uuid = [0x29; 16];
    options.plan.metadata_profile = metadata;
    options.plan.data_profile = data;
    filesystem_image_options(
        TreeBuilder::new().root(Metadata::new(0o755, timestamp)),
        tempfile::tempfile().unwrap(),
        options,
    )
}

#[cfg(feature = "write")]
const DIRECTORY: &str = "大容量-大容量-대용량";

#[cfg(feature = "write")]
#[test]
fn data_chunks_are_allocated_after_the_mkfs_group_fills() {
    use super::writer::{Attributes, Name};
    use ferrosys::btrfs::Profile;

    // Single data, and DUP data, whose chunks keep two identical copies.
    for data in [Profile::Single, Profile::Dup] {
        block_on(async {
            let (_, image) = profile_image(Profile::Dup, data);
            let before = assert_device_layout(&image).await;
            let mut writer = mount_writer(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            let concurrency = NonZeroUsize::MIN;
            let when = crate::Timestamp {
                seconds: 1_700_000_100,
                nanoseconds: 0,
            };
            let attributes = Attributes {
                permissions: 0o644,
                uid: 0,
                gid: 0,
                umask: 0,
            };
            // Several times the one data group a fresh image has.
            const MIB: usize = 1 << 20;
            let data_bytes: u64 = before
                .chunks
                .iter()
                .filter(|chunk| chunk.2 & 1 != 0)
                .map(|chunk| chunk.1)
                .sum();
            let pieces = (data_bytes as usize / MIB) + 8;
            let content: Vec<u8> = (0..pieces)
                .flat_map(|seed| noise(MIB, seed as u8))
                .collect();
            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            let file = tx
                .create_file(
                    Name {
                        parent: &root,
                        bytes: DIRECTORY.as_bytes(),
                    },
                    attributes,
                    when,
                )
                .await
                .unwrap();
            for (index, piece) in content.chunks(MIB).enumerate() {
                let written = tx
                    .write_file(
                        &file,
                        (index * MIB) as u64,
                        piece,
                        WriteOptions::plain(when, concurrency),
                    )
                    .await
                    .unwrap();
                assert_eq!(written, MIB);
            }
            tx.commit().await.unwrap();

            let after = assert_device_layout(&image).await;
            assert!(
                count_kind(&after, 1) > count_kind(&before, 1),
                "a data chunk was allocated"
            );
            assert_eq!(count_kind(&after, 4), count_kind(&before, 4));
            // The carried state still equals a reload.
            let fresh = Filesystem::mount(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            assert!(*writer.committed_allocation() == allocation_of(&fresh).await);

            // The data reads back through the committed view and a fresh mount.
            let committed = writer.snapshot().unwrap();
            let inode = committed
                .resolve(alloc::format!("/{DIRECTORY}").as_bytes())
                .await
                .unwrap();
            let mut output = vec![0; content.len()];
            assert_eq!(
                committed.read(&inode, 0, &mut output).await.unwrap(),
                content.len()
            );
            assert!(
                output == content,
                "committed view survives chunk allocation"
            );
            let inode = fresh
                .resolve(alloc::format!("/{DIRECTORY}").as_bytes())
                .await
                .unwrap();
            let mut output = vec![0; content.len()];
            assert_eq!(
                fresh.read(&inode, 0, &mut output).await.unwrap(),
                content.len()
            );
            assert!(output == content, "fresh mount survives chunk allocation");

            // DUP chunks hold two identical copies of every byte.
            let fs = Filesystem::mount(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            for (&logical, physical) in &after.copies {
                let &(_, length, flags) = after
                    .chunks
                    .iter()
                    .find(|&&(start, ..)| start == logical)
                    .unwrap();
                assert_eq!(physical.len(), if flags & (1 << 5) != 0 { 2 } else { 1 });
                assert_eq!(
                    fs.volume.chunks().map(logical, length).unwrap().as_slice(),
                    physical
                );
                if let [first, second] = physical[..] {
                    let (mut a, mut b) = (vec![0; length as usize], vec![0; length as usize]);
                    image.read_exact_at(&mut a, first).unwrap();
                    image.read_exact_at(&mut b, second).unwrap();
                    // The superblock mirror at 64 MiB is each device's own
                    // block, never a chunk copy, wherever a chunk spans it.
                    let mirror = 64_u64 << 20;
                    let at = a.iter().zip(&b).enumerate().position(|(index, (x, y))| {
                        let spans = |start: u64| {
                            (mirror..mirror + (64 << 10)).contains(&(start + index as u64))
                        };
                        x != y && !spans(first) && !spans(second)
                    });
                    assert!(
                        at.is_none(),
                        "chunk {logical:#x} (flags {flags:#x}, length {length:#x}, copies {first:#x} {second:#x}) differs at {at:?}"
                    );
                }
            }
        });
    }
}

#[cfg(feature = "write")]
#[test]
fn a_system_chunk_lands_in_the_bootstrap_array_and_survives_remount() {
    use super::writer::{Attributes, Name};
    use crate::space::SYSTEM;
    use ferrosys::btrfs::Profile;

    for metadata in [Profile::Dup, Profile::Single] {
        block_on(async {
            let (_, image) = profile_image(metadata, Profile::Single);
            let before = assert_device_layout(&image).await;
            let mut writer = mount_writer(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            let mut tx = writer.join().await.unwrap();
            tx.edits().allocate_chunk(SYSTEM).await.unwrap();
            tx.commit().await.unwrap();

            let after = assert_device_layout(&image).await;
            assert_eq!(count_kind(&after, 2), count_kind(&before, 2) + 1);
            assert_eq!(after.bootstrap.len(), before.bootstrap.len() + 1);
            let &(logical, _, flags) = after.chunks.last().unwrap();
            // A key, a chunk item and one stripe per copy.
            let copies = if flags & (1 << 5) != 0 { 2 } else { 1 };
            assert_eq!(copies, after.copies[&logical].len());
            assert_eq!(
                after.sys_chunk_array,
                before.sys_chunk_array + 17 + 48 + 32 * copies
            );
            assert_eq!(flags & 7, SYSTEM);
            // The new chunk keeps the profile the system group already had.
            let &(_, _, old) = before
                .chunks
                .iter()
                .rfind(|chunk| chunk.2 & 2 != 0)
                .unwrap();
            assert_eq!(flags, old);

            // A fresh mount reads the grown array, and the new chunk takes
            // chunk-tree blocks and carries on writing.
            let mut writer = mount_writer(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            let when = crate::Timestamp {
                seconds: 1_700_000_200,
                nanoseconds: 0,
            };
            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            tx.create_file(
                Name {
                    parent: &root,
                    bytes: "書き込み-写入-쓰기".as_bytes(),
                },
                Attributes {
                    permissions: 0o600,
                    uid: 0,
                    gid: 0,
                    umask: 0,
                },
                when,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            assert_device_layout(&image).await;
        });
    }
}

#[cfg(feature = "write")]
#[test]
fn metadata_chunks_keep_their_profile_and_arrive_before_the_group_runs_short() {
    use crate::space::METADATA;
    use ferrosys::btrfs::Profile;

    for profile in [Profile::Dup, Profile::Single] {
        block_on(async {
            let (_, image) = profile_image(profile, Profile::Single);
            let before = assert_device_layout(&image).await;
            let mut writer = mount_writer(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            let mut tx = writer.join().await.unwrap();
            let transaction = tx.edits();
            transaction.prepare_mutation().await.unwrap();
            assert_eq!(
                transaction.space.group_summary().len(),
                before.chunks.len(),
                "a nearly empty metadata group needs no new chunk"
            );
            // Reserve tree blocks until the 80% rule asks for another chunk.
            let (total, _) = transaction.space.totals(METADATA);
            let mut key = 0;
            while {
                let (total, free) = transaction.space.totals(METADATA);
                (total - free) + (2 << 20) < total / 100 * 80
            } {
                key += 1;
                transaction
                    .space
                    .allocate(crate::ondisk::objectid::FS_TREE, 0, Key::new(key, 1, 0))
                    .unwrap();
            }
            transaction.prepare_mutation().await.unwrap();
            let groups = transaction.space.group_summary();
            assert_eq!(groups.len(), before.chunks.len() + 1);
            let &(_, length, flags) = groups.last().unwrap();
            assert_eq!(flags & 7, METADATA);
            assert_eq!(flags & (1 << 5) != 0, profile == Profile::Dup);
            assert!(length <= total.max(256 << 20));
            // The grown group serves the next reservations.
            assert!(transaction.space.totals(METADATA).1 > 2 << 20);
        });
    }
}

#[cfg(feature = "write")]
#[test]
fn a_full_device_shrinks_data_windows_and_reports_enospc_only_without_a_free_sector() {
    use super::writer::{Attributes, Name};
    use crate::space::{DATA, SpaceError};
    use ferrosys::btrfs::Profile;

    block_on(async {
        let (_, image) = profile_image(Profile::Dup, Profile::Single);
        let mut writer = mount_writer(Disk(image.try_clone().unwrap()))
            .await
            .unwrap();
        let concurrency = NonZeroUsize::MIN;
        let when = crate::Timestamp {
            seconds: 1_700_000_100,
            nanoseconds: 0,
        };
        let attributes = Attributes {
            permissions: 0o644,
            uid: 0,
            gid: 0,
            umask: 0,
        };
        const PIECE: usize = 64 << 10;
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let kept = tx
            .create_file(
                Name {
                    parent: &root,
                    bytes: "保存-保存-보존".as_bytes(),
                },
                attributes,
                when,
            )
            .await
            .unwrap();
        let freed = tx
            .create_file(
                Name {
                    parent: &root,
                    bytes: "解放-解放-해제".as_bytes(),
                },
                attributes,
                when,
            )
            .await
            .unwrap();
        // Alternate 64 KiB pieces of two files until the device has nothing
        // left for either: the data groups fill, chunks follow, and the last
        // chunk shrinks to the hole that is left.
        let mut offset = 0;
        let mut kept_content = vec![];
        'fill: for index in 0_u32.. {
            for file in [&kept, &freed] {
                let piece = noise(PIECE, index as u8);
                match tx
                    .write_file(file, offset, &piece, WriteOptions::plain(when, concurrency))
                    .await
                {
                    Ok(PIECE) => {
                        if std::ptr::eq(file, &kept) {
                            kept_content.extend_from_slice(&piece);
                        }
                    }
                    Ok(_) | Err(WriteError::Space(SpaceError::Exhausted { .. })) => break 'fill,
                    Err(error) => panic!("unexpected write failure: {error}"),
                }
            }
            offset += PIECE as u64;
        }
        assert!(tx.edits().space.largest_free(DATA) < PIECE as u64);
        // Free every second piece: the free ranges are now 64 KiB holes and
        // the device has no room for another chunk.
        tx.truncate_file(&freed, 0, WriteOptions::plain(when, concurrency))
            .await
            .unwrap();
        assert_eq!(tx.edits().space.largest_free(DATA), PIECE as u64);
        let holes = tx.edits().space.totals(DATA).1 / PIECE as u64;
        assert!(holes > 100, "the test needs many holes, has {holes}");

        // A 128 KiB write exceeds every range, so each window shrinks to one
        // hole and the write still succeeds.
        let large = tx
            .create_file(
                Name {
                    parent: &root,
                    bytes: "断片-断片-단편".as_bytes(),
                },
                attributes,
                when,
            )
            .await
            .unwrap();
        let mut large_content = vec![];
        let mut stored = 0;
        let remainder = loop {
            let piece = noise(2 * PIECE, (large_content.len() / PIECE / 2) as u8);
            let written = tx
                .write_file(
                    &large,
                    stored,
                    &piece,
                    WriteOptions::plain(when, concurrency),
                )
                .await
                .unwrap_or(0);
            large_content.extend_from_slice(&piece[..written]);
            stored += written as u64;
            if written < piece.len() {
                break written;
            }
        };
        // The device ended it with a short write, a prefix of a piece, or with
        // none left; both leave no free sector to report.
        assert!(remainder < 2 * PIECE);
        assert!(large_content.len() >= 2 * PIECE * 10);
        assert!(tx.edits().space.largest_free(DATA) < 4096);
        let refused = tx
            .write_file(
                &large,
                stored,
                &[0x5a; 4096],
                WriteOptions::plain(when, concurrency),
            )
            .await;
        assert!(
            matches!(
                refused,
                Err(WriteError::Space(SpaceError::Exhausted { flags: DATA, .. }))
            ),
            "{refused:?}"
        );
        tx.commit().await.unwrap();

        assert_device_layout(&image).await;
        let fresh = Filesystem::mount(Disk(image.try_clone().unwrap()))
            .await
            .unwrap();
        assert!(*writer.committed_allocation() == allocation_of(&fresh).await);
        for (name, expected) in [
            ("保存-保存-보존", &kept_content),
            ("断片-断片-단편", &large_content),
        ] {
            let inode = fresh
                .resolve(alloc::format!("/{name}").as_bytes())
                .await
                .unwrap();
            let mut output = vec![0; expected.len()];
            assert_eq!(
                fresh.read(&inode, 0, &mut output).await.unwrap(),
                expected.len()
            );
            assert!(&output == expected, "{name}");
        }
    });
}

/// One commit that changes a little metadata, so the writer publishes.
#[cfg(feature = "write")]
async fn touch(writer: &mut super::writer::Writer<Disk, FixtureUuids>, name: &str) {
    use super::writer::{Attributes, Name};
    let when = crate::Timestamp {
        seconds: 1_700_000_300,
        nanoseconds: 0,
    };
    let mut tx = writer.join().await.unwrap();
    let root = tx.root().await.unwrap();
    tx.create_file(
        Name {
            parent: &root,
            bytes: name.as_bytes(),
        },
        Attributes {
            permissions: 0o600,
            uid: 0,
            gid: 0,
            umask: 0,
        },
        when,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[cfg(feature = "write")]
#[test]
fn unused_block_groups_leave_after_their_extents_are_free_and_no_reader_pins_them() {
    use super::writer::{Attributes, Name};
    use ferrosys::btrfs::Profile;

    block_on(async {
        let (_, image) = profile_image(Profile::Dup, Profile::Single);
        let before = assert_device_layout(&image).await;
        let mut writer = mount_writer(Disk(image.try_clone().unwrap()))
            .await
            .unwrap();
        let concurrency = NonZeroUsize::MIN;
        let when = crate::Timestamp {
            seconds: 1_700_000_100,
            nanoseconds: 0,
        };
        const MIB: usize = 1 << 20;
        let data_bytes: u64 = before
            .chunks
            .iter()
            .filter(|chunk| chunk.2 & 1 != 0)
            .map(|chunk| chunk.1)
            .sum();
        let content: Vec<u8> = (0..(data_bytes as usize / MIB) + 8)
            .flat_map(|seed| noise(MIB, seed as u8))
            .collect();
        let name = "巨大-巨大-거대";
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let file = tx
            .create_file(
                Name {
                    parent: &root,
                    bytes: name.as_bytes(),
                },
                Attributes {
                    permissions: 0o644,
                    uid: 0,
                    gid: 0,
                    umask: 0,
                },
                when,
            )
            .await
            .unwrap();
        for (index, piece) in content.chunks(MIB).enumerate() {
            tx.write_file(
                &file,
                (index * MIB) as u64,
                piece,
                WriteOptions::plain(when, concurrency),
            )
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        let full = assert_device_layout(&image).await;
        assert!(count_kind(&full, 1) > count_kind(&before, 1));

        // A reader of the generation that holds the file pins its extents.
        let reader = writer.snapshot().unwrap();
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        tx.unlink_name(
            Name {
                parent: &root,
                bytes: name.as_bytes(),
            },
            when,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        // The unlink's own commit still counts the extents as used, and the
        // next one finds them pinned by the reader.
        touch(&mut writer, "一-1-일").await;
        touch(&mut writer, "二-2-이").await;
        assert_eq!(
            count_kind(&assert_device_layout(&image).await, 1),
            count_kind(&full, 1),
            "a reader pinning the old generation keeps the group"
        );

        drop(reader);
        touch(&mut writer, "三-3-삼").await;
        let after = assert_device_layout(&image).await;
        assert_eq!(count_kind(&after, 1), count_kind(&before, 1));
        let fresh = Filesystem::mount(Disk(image.try_clone().unwrap()))
            .await
            .unwrap();
        assert!(*writer.committed_allocation() == allocation_of(&fresh).await);
        // The freed device range is available again.
        touch(&mut writer, "四-4-사").await;
        assert_device_layout(&image).await;
    });
}

#[cfg(feature = "write")]
#[test]
fn removal_keeps_the_last_group_of_a_type_and_a_group_made_in_the_same_transaction() {
    use crate::space::{METADATA, SYSTEM};
    use ferrosys::btrfs::Profile;

    for metadata in [Profile::Dup, Profile::Single] {
        block_on(async {
            let (_, image) = profile_image(metadata, Profile::Single);
            let before = assert_device_layout(&image).await;
            let mut writer = mount_writer(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            // Nothing is removable on a fresh image: the empty data group is
            // the last of its type.
            let mut tx = writer.join().await.unwrap();
            assert_eq!(tx.edits().remove_unused_groups().await.unwrap(), 0);
            // A group this transaction allocated stays until it publishes.
            tx.edits().allocate_chunk(SYSTEM).await.unwrap();
            tx.edits().allocate_chunk(METADATA).await.unwrap();
            assert_eq!(tx.edits().remove_unused_groups().await.unwrap(), 0);
            tx.commit().await.unwrap();
            let grown = assert_device_layout(&image).await;
            assert_eq!(count_kind(&grown, 2), count_kind(&before, 2) + 1);
            assert_eq!(count_kind(&grown, 4), count_kind(&before, 4) + 1);

            // The next commit finds the new system and metadata groups
            // published and unused, and removes both: the bootstrap array
            // gives the entry back and the device item the bytes.
            touch(&mut writer, "整理-整理-정리").await;
            let after = assert_device_layout(&image).await;
            assert_eq!(count_kind(&after, 2), count_kind(&before, 2));
            assert_eq!(count_kind(&after, 4), count_kind(&before, 4));
            assert_eq!(after.sys_chunk_array, before.sys_chunk_array);
            assert_eq!(after.bootstrap, before.bootstrap);
            let fresh = Filesystem::mount(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            assert!(*writer.committed_allocation() == allocation_of(&fresh).await);
            touch(&mut writer, "続行-继续-계속").await;
            assert_device_layout(&image).await;
        });
    }
}

#[cfg(feature = "write")]
#[test]
fn empty_transaction_does_not_publish() {
    block_on(async {
        let (_, image) = filesystem_image();
        let operations = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let fs = Filesystem::mount(FaultDisk {
            disk: Disk(image),
            fail_at: usize::MAX,
            operations: operations.clone(),
            poisoned: std::sync::atomic::AtomicBool::new(false),
        })
        .await
        .unwrap();
        let mut retirements = crate::retirement::Retirements::default();
        crate::transaction::Transaction::new(
            fs.volume.view(&fs.snapshot.mapping),
            fs.snapshot.clone(),
            &mut retirements,
            &allocation_of(&fs).await,
        )
        .await
        .unwrap()
        .commit(
            &mut retirements,
            || panic!("no new UUID required"),
            core::num::NonZeroUsize::MIN,
        )
        .await
        .unwrap();
        assert!(operations.lock().unwrap().is_empty());
    });
}

impl BlockSource for Disk {
    fn size_bytes(&self) -> u64 {
        self.0.metadata().unwrap().len()
    }
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> IoResult<()> {
        self.0
            .read_exact_at(buf, offset)
            .map_err(|_| IoError::DeviceFault)
    }
}

#[cfg(feature = "write")]
impl crate::source::BlockSink for Disk {
    async fn stage(&self, block: u64, bytes: &[u8; 4096]) -> IoResult<()> {
        self.0
            .write_all_at(bytes, block * crate::source::READ_ALIGNMENT)
            .map_err(|_| IoError::DeviceFault)
    }
    async fn commit(&self, block: u64, bytes: &[u8; 4096]) -> IoResult<()> {
        self.0.sync_all().map_err(|_| IoError::DeviceFault)?;
        self.stage(block, bytes).await?;
        self.0.sync_all().map_err(|_| IoError::DeviceFault)
    }
}

fn filesystem() -> Filesystem<Disk> {
    filesystem_image().0
}

fn filesystem_image() -> (Filesystem<Disk>, File) {
    filesystem_image_with_features(true, true)
}

fn filesystem_image_with_features(separate: bool, free_tree: bool) -> (Filesystem<Disk>, File) {
    let timestamp = Timestamp::from_secs(1_700_000_000);
    let tree = TreeBuilder::new().root(Metadata::new(0o755, timestamp));
    filesystem_image_tree(separate, free_tree, tree)
}

fn filesystem_image_tree(
    separate: bool,
    free_tree: bool,
    tree: TreeBuilder,
) -> (Filesystem<Disk>, File) {
    filesystem_image_tree_file(separate, free_tree, tree, tempfile::tempfile().unwrap())
}

pub(super) fn filesystem_image_tree_file(
    separate: bool,
    free_tree: bool,
    tree: TreeBuilder,
    file: File,
) -> (Filesystem<Disk>, File) {
    let timestamp = Timestamp::from_secs(1_700_000_000);
    let mut options = FormatOptions::new([0x53; 16], timestamp);
    if !separate {
        options.plan.compat_ro_flags = options
            .plan
            .compat_ro_flags
            .without(ferrosys::btrfs::ondisk::CompatRoFlags::BLOCK_GROUP_TREE);
    }
    if !free_tree {
        options.plan.compat_ro_flags = options
            .plan
            .compat_ro_flags
            .without(ferrosys::btrfs::ondisk::CompatRoFlags::FREE_SPACE_TREE)
            .without(ferrosys::btrfs::ondisk::CompatRoFlags::FREE_SPACE_TREE_VALID);
    }
    filesystem_image_options(tree, file, options)
}

fn filesystem_image_options(
    tree: TreeBuilder,
    mut file: File,
    options: FormatOptions,
) -> (Filesystem<Disk>, File) {
    // ferrosys' default single-device geometry needs at least 109 MiB.
    // DUP data, which mkfs lays out twice, needs 240123904 bytes.
    let size = if options.plan.data_profile == ferrosys::btrfs::Profile::Dup {
        512 << 20
    } else {
        128 << 20
    };
    file.set_len(size).unwrap();
    format_to(&mut file, tree, size, options).unwrap();
    let reference = file.try_clone().unwrap();
    (block_on(Filesystem::mount(Disk(file))).unwrap(), reference)
}

#[cfg(feature = "write")]
#[test]
fn transaction_publishes_roots_and_preserves_previous_generation() {
    use crate::transaction::Transaction;
    use btrfs_disk::superblock::{
        read_superblock_at, read_superblock_bytes_at, superblock_is_valid,
    };

    block_on(async {
        for (separate, free_tree) in [(true, true), (false, true), (false, false)] {
            let (fs, mut image) = filesystem_image_with_features(separate, free_tree);
            let generation = fs.superblock().generation();
            let (root, _) = change_root_mode(&fs).await.unwrap();

            let mounted = Filesystem::mount(Disk(image.try_clone().unwrap()))
                .await
                .unwrap();
            assert_eq!(mounted.superblock().generation(), generation + 1);
            assert_eq!(
                mounted.resolve(b"/").await.unwrap().metadata().permissions,
                0o700
            );
            assert_eq!(
                fs.resolve(b"/").await.unwrap().metadata().permissions,
                0o755
            );
            assert!(matches!(
                Transaction::new(
                    fs.volume.view(&fs.snapshot.mapping),
                    fs.snapshot.clone(),
                    &mut crate::retirement::Retirements::default(),
                    &allocation_of(&fs).await,
                )
                .await,
                Err(WriteError::StaleSnapshot)
            ));
            for offset in crate::superblock::MIRROR_OFFSETS {
                if offset + 4096 > image.metadata().unwrap().len() {
                    continue;
                }
                let bytes = read_superblock_bytes_at(&mut image, offset).unwrap();
                assert!(superblock_is_valid(&bytes));
                let disk = read_superblock_at(&mut image, offset).unwrap();
                assert_eq!(disk.bytenr, offset);
                assert_eq!(disk.generation, generation + 1);
                assert_eq!(disk.root, mounted.snapshot.root_tree.bytenr);
                let backup = disk
                    .backup_roots
                    .iter()
                    .find(|backup| backup.tree_root_gen == generation + 1)
                    .unwrap();
                assert_eq!(backup.tree_root, disk.root);
                assert_eq!(backup.fs_root, root.bytenr);
                assert_eq!(backup.fs_root_gen, root.generation);
                assert_eq!(backup.csum_root, mounted.snapshot.csum_tree.bytenr);
                assert_eq!(backup.bytes_used, disk.bytes_used);
                assert_eq!(backup.total_bytes, disk.total_bytes);
            }
        }
    });
}

#[test]
fn space_load_validates_formatted_file_data_references() {
    block_on(async {
        use crate::accounting::{Accounting, DataChange};
        use crate::allocation::data::{
            DataBackref, DataExtent, DataReference, SharedDataReference,
        };
        use crate::ondisk::{item_type, objectid};

        let timestamp = Timestamp::from_secs(1_700_000_000);
        let tree = TreeBuilder::new()
            .root(Metadata::new(0o755, timestamp))
            .file(
                "/data.bin",
                vec![0x53; 32768],
                Metadata::new(0o644, timestamp),
            );
        let (fs, _) = filesystem_image_tree(true, true, tree);
        let inode = fs.resolve(b"/data.bin").await.unwrap();
        let mut space = crate::space::Space::load(&fs.volume, fs.snapshot.root_tree)
            .await
            .unwrap();
        let root = super::find_root(&fs.volume, fs.snapshot.root_tree, objectid::EXTENT_TREE)
            .await
            .unwrap()
            .root;
        let mut cursor = crate::tree::Cursor::ceiling(&fs.volume, root, Key::new(0, 0, 0))
            .await
            .unwrap();
        let mut allocations = Vec::new();
        while let (Some(key), Some(bytes)) = (cursor.key(), cursor.item()) {
            if key.item_type == item_type::EXTENT_ITEM
                && u64::from_le_bytes(bytes[16..24].try_into().unwrap()) == 1
            {
                let extent = DataExtent::parse(key, bytes, fs.superblock().sectorsize()).unwrap();
                assert_eq!(extent.refs, 1);
                assert_eq!(extent.roots.len(), 1);
                let (reference, references) = extent.roots.first_key_value().unwrap();
                assert_eq!(reference.root, objectid::FS_TREE);
                assert_eq!(reference.inode, inode.number());
                assert_eq!(*references, 1);
                allocations.push((key, *reference));
            }
            cursor.advance(&fs.volume).await.unwrap();
        }
        assert!(!allocations.is_empty());
        let (key, original) = allocations[0];
        assert!(key.offset > u64::from(fs.superblock().nodesize()));
        let mut accounting = Accounting::new(&fs.volume, root, &space, fs.snapshot.csum_tree)
            .await
            .unwrap();
        let mut trees = Trees::new(
            fs.superblock().generation() + 1,
            fs.superblock().csum_type(),
        );
        // Fixed CRC32C collision with sector-aligned file offsets. Check the
        // identities against the independent disk-format implementation too.
        let other = DataReference {
            root: 256,
            inode: 218670021594820,
            offset: 249879179472896,
        };
        let collision = DataReference {
            root: 256,
            inode: 125603394931414,
            offset: 70811665399808,
        };
        assert_eq!(other.hash(), collision.hash());
        assert_eq!(
            other.hash(),
            btrfs_disk::items::extent_data_ref_hash(
                collision.root,
                collision.inode,
                collision.offset
            )
        );
        let shared = SharedDataReference(fs.snapshot.default_subvolume.root.bytenr);
        assert!(
            !accounting
                .update_data(
                    &fs.volume,
                    &mut trees,
                    &mut space,
                    DataChange {
                        key,
                        reference: other,
                        delta: 1
                    }
                )
                .await
                .unwrap()
        );
        assert!(
            !accounting
                .update_data(
                    &fs.volume,
                    &mut trees,
                    &mut space,
                    DataChange {
                        key,
                        reference: collision,
                        delta: 1
                    }
                )
                .await
                .unwrap()
        );
        let displaced = Key::new(
            key.objectid,
            item_type::EXTENT_DATA_REF,
            collision.hash() + 1,
        );
        assert!(
            trees
                .get(&fs.volume, accounting.root, displaced)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            !accounting
                .update_data(
                    &fs.volume,
                    &mut trees,
                    &mut space,
                    DataChange {
                        key,
                        reference: other,
                        delta: -1
                    }
                )
                .await
                .unwrap()
        );
        assert!(
            trees
                .get(&fs.volume, accounting.root, other.key(key.objectid))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !accounting
                .update_data(
                    &fs.volume,
                    &mut trees,
                    &mut space,
                    DataChange {
                        key,
                        reference: shared,
                        delta: 2
                    }
                )
                .await
                .unwrap()
        );
        assert!(
            !accounting
                .update_data(
                    &fs.volume,
                    &mut trees,
                    &mut space,
                    DataChange {
                        key,
                        reference: original,
                        delta: -1
                    }
                )
                .await
                .unwrap()
        );
        assert!(
            !accounting
                .update_data(
                    &fs.volume,
                    &mut trees,
                    &mut space,
                    DataChange {
                        key,
                        reference: shared,
                        delta: -2
                    }
                )
                .await
                .unwrap()
        );
        assert!(
            accounting
                .update_data(
                    &fs.volume,
                    &mut trees,
                    &mut space,
                    DataChange {
                        key,
                        reference: collision,
                        delta: -1
                    }
                )
                .await
                .unwrap()
        );
        assert!(
            trees
                .get(&fs.volume, accounting.root, key)
                .await
                .unwrap()
                .is_none()
        );
        let checksum_key = crate::checksums::key(key.objectid);
        assert!(
            trees
                .get(&fs.volume, fs.snapshot.csum_tree, checksum_key)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            trees
                .get(&fs.volume, accounting.checksum_root(), checksum_key)
                .await
                .unwrap()
                .is_none()
        );
        accounting
            .sync_allocations(&fs.volume, &mut trees, &mut space)
            .await
            .unwrap();
        verify_group_usage(&fs, &trees, &accounting, space.group_root()).await;
        // Retired bytes appear in the next disk free-space tree, but remain
        // unavailable to this transaction's allocator.
        for group in space
            .committed_free(&rpds::RedBlackTreeMapSync::new_sync())
            .unwrap()
        {
            assert!(
                group
                    .free
                    .iter()
                    .all(|range| range.end <= key.objectid
                        || range.start >= key.objectid + key.offset)
            );
        }
        assert!(trees.get(&fs.volume, root, key).await.unwrap().is_some());
    });
}

fn root_node(fs: &Filesystem<Disk>) -> Node {
    let root = fs.snapshot.default_subvolume.root;
    block_on(Node::read(
        &fs.volume,
        root.bytenr,
        Expected {
            level: root.level,
            generation: root.generation,
            first_key: None,
        },
    ))
    .unwrap()
}

#[cfg(feature = "write")]
#[test]
fn data_staging_writes_all_copies_without_publishing_roots() {
    block_on(async {
        use crate::accounting::Accounting;
        use crate::allocation::data::DataReference;
        use crate::ondisk::objectid;

        let (fs, image) = filesystem_image();
        let mut space = crate::space::Space::load(&fs.volume, fs.snapshot.root_tree)
            .await
            .unwrap();
        let extent_root =
            super::find_root(&fs.volume, fs.snapshot.root_tree, objectid::EXTENT_TREE)
                .await
                .unwrap()
                .root;
        let mut accounting =
            Accounting::new(&fs.volume, extent_root, &space, fs.snapshot.csum_tree)
                .await
                .unwrap();
        let mut trees = Trees::new(
            fs.superblock().generation() + 1,
            fs.superblock().csum_type(),
        );
        let concurrency = std::thread::available_parallelism().unwrap();
        let bytes: Vec<_> = (0..32768).map(|index| (index % 251) as u8).collect();
        let reference = DataReference {
            root: 5,
            inode: 256,
            offset: 0,
        };
        let key = accounting
            .stage_data(
                &fs.volume,
                &mut trees,
                &mut space,
                crate::accounting::DataWrite {
                    bytes: &bytes,
                    reference,
                    checksummed: true,
                },
                concurrency,
            )
            .await
            .unwrap();
        let mut actual = vec![0; bytes.len()];
        for physical in fs.volume.chunks().map(key.objectid, key.offset).unwrap() {
            image.read_exact_at(&mut actual, physical).unwrap();
            assert_eq!(actual, bytes);
        }
        let sums = trees
            .get(
                &fs.volume,
                accounting.checksum_root(),
                crate::checksums::key(key.objectid),
            )
            .await
            .unwrap()
            .unwrap();
        let algorithm = fs.superblock().csum_type();
        for (sector, digest) in bytes
            .chunks_exact(fs.superblock().sectorsize() as usize)
            .zip(sums.chunks_exact(algorithm.size()))
        {
            assert!(algorithm.verifies(sector, digest));
        }
        accounting
            .sync_allocations(&fs.volume, &mut trees, &mut space)
            .await
            .unwrap();
        let mut copies_seen = 0;
        for (logical, bytes) in trees.blocks() {
            fs.volume.stage(logical, bytes, concurrency).await.unwrap();
            let copies = fs.volume.chunks().map(logical, bytes.len() as u64).unwrap();
            copies_seen = copies_seen.max(copies.len());
            let mut actual = vec![0; bytes.len()];
            for physical in copies {
                image.read_exact_at(&mut actual, physical).unwrap();
                assert_eq!(actual, bytes);
            }
        }
        assert_eq!(copies_seen, 2);
        let reopened = Filesystem::mount(Disk(image)).await.unwrap();
        assert_eq!(
            reopened.superblock().generation(),
            fs.superblock().generation()
        );
        assert_eq!(
            reopened.snapshot.default_subvolume.root.bytenr,
            fs.snapshot.default_subvolume.root.bytenr
        );
        assert!(
            trees
                .get(&fs.volume, extent_root, key)
                .await
                .unwrap()
                .is_none()
        );
    });
}

#[test]
fn private_data_allocations_register_cancel_and_reuse_their_full_ranges() {
    block_on(async {
        use crate::accounting::{Accounting, DataChange};
        use crate::allocation::data::{DataExtent, DataReference};
        use crate::ondisk::objectid;
        use crate::space::SpaceError;

        for (separate, free_tree) in [(true, true), (false, true), (false, false)] {
            let (fs, _) = filesystem_image_with_features(separate, free_tree);
            let mut space = crate::space::Space::load(&fs.volume, fs.snapshot.root_tree)
                .await
                .unwrap();
            let root = super::find_root(&fs.volume, fs.snapshot.root_tree, objectid::EXTENT_TREE)
                .await
                .unwrap()
                .root;
            let mut accounting = Accounting::new(&fs.volume, root, &space, fs.snapshot.csum_tree)
                .await
                .unwrap();
            let mut trees = Trees::new(
                fs.superblock().generation() + 1,
                fs.superblock().csum_type(),
            );
            let sector = u64::from(fs.superblock().sectorsize());
            assert!(matches!(
                space.reserve_data(0),
                Err(SpaceError::DataLength { .. })
            ));
            assert!(matches!(
                space.reserve_data(sector + 1),
                Err(SpaceError::DataLength { .. })
            ));
            let reference = DataReference {
                root: 5,
                inode: 256,
                offset: 0,
            };
            let first = accounting
                .allocate_data(&fs.volume, &mut trees, &mut space, 3 * sector, reference)
                .await
                .unwrap();
            let second = accounting
                .allocate_data(&fs.volume, &mut trees, &mut space, 5 * sector, reference)
                .await
                .unwrap();
            assert_eq!(second.objectid, first.objectid + first.offset);
            assert!(matches!(
                space.reserve_data(u64::MAX - (sector - 1)),
                Err(SpaceError::Exhausted { .. })
            ));
            accounting
                .sync_allocations(&fs.volume, &mut trees, &mut space)
                .await
                .unwrap();
            verify_group_usage(&fs, &trees, &accounting, space.group_root()).await;
            for key in [first, second] {
                let bytes = trees
                    .get(&fs.volume, accounting.root, key)
                    .await
                    .unwrap()
                    .unwrap();
                let extent = DataExtent::parse(key, &bytes, fs.superblock().sectorsize()).unwrap();
                assert_eq!(extent.refs, 1);
                assert_eq!(extent.generation, trees.generation());
                assert_eq!(extent.roots[&reference], 1);
                let independent = btrfs_disk::items::ExtentItem::parse(
                    &bytes,
                    &btrfs_disk::tree::DiskKey {
                        objectid: key.objectid,
                        key_type: btrfs_disk::tree::KeyType::ExtentItem,
                        offset: key.offset,
                    },
                )
                .unwrap();
                assert!(independent.is_data());
                assert_eq!(independent.refs, 1);
                assert!(trees.get(&fs.volume, root, key).await.unwrap().is_none());
                assert!(
                    accounting
                        .update_data(
                            &fs.volume,
                            &mut trees,
                            &mut space,
                            DataChange {
                                key,
                                reference,
                                delta: -1
                            }
                        )
                        .await
                        .unwrap()
                );
            }
            accounting
                .sync_allocations(&fs.volume, &mut trees, &mut space)
                .await
                .unwrap();
            verify_group_usage(&fs, &trees, &accounting, space.group_root()).await;
            let combined = accounting
                .allocate_data(&fs.volume, &mut trees, &mut space, 8 * sector, reference)
                .await
                .unwrap();
            assert_eq!(combined.objectid, first.objectid);
            accounting
                .sync_allocations(&fs.volume, &mut trees, &mut space)
                .await
                .unwrap();
            verify_group_usage(&fs, &trees, &accounting, space.group_root()).await;
            assert!(!space.cancel_data(fs.snapshot.default_subvolume.root.bytenr));
        }
    });
}

#[test]
fn shared_tree_cow_converts_data_references_and_retires_only_the_last_owner() {
    block_on(async {
        use crate::accounting::{Accounting, MetadataBlock};
        use crate::allocation::MetadataReference;
        use crate::allocation::data::{DataExtent, DataReference};
        use crate::items::{ExtentData, FileExtent};
        use crate::ondisk::{item_type, objectid};

        for siblings in [0, 512] {
            let timestamp = Timestamp::from_secs(1_700_000_000);
            let mut tree = TreeBuilder::new()
                .root(Metadata::new(0o755, timestamp))
                .file(
                    "/data.bin",
                    vec![0x53; 32768],
                    Metadata::new(0o644, timestamp),
                );
            for index in 0..siblings {
                tree = tree.file(
                    alloc::format!("/sibling-{index}"),
                    b"inline data",
                    Metadata::new(0o644, timestamp),
                );
            }
            let (fs, _) = filesystem_image_tree(true, true, tree);
            let inode = fs.resolve(b"/data.bin").await.unwrap();
            let mut trees = Trees::new(
                fs.superblock().generation() + 1,
                fs.superblock().csum_type(),
            );
            let original = fs.snapshot.default_subvolume.root;
            assert_eq!(original.level == 0, siblings == 0);
            let file_key = Key::new(inode.number(), item_type::EXTENT_DATA, 0);
            let bytes = trees
                .get(&fs.volume, original, file_key)
                .await
                .unwrap()
                .unwrap();
            let original_file = bytes.clone();
            let file = FileExtent::parse(file_key, &bytes, fs.superblock().sectorsize()).unwrap();
            let ExtentData::Regular {
                disk_bytenr,
                disk_num_bytes,
                ..
            } = file.data
            else {
                panic!("regular fixture extent")
            };
            let data_key = Key::new(disk_bytenr, item_type::EXTENT_ITEM, disk_num_bytes);
            let mut space = crate::space::Space::load(&fs.volume, fs.snapshot.root_tree)
                .await
                .unwrap();
            let extent_root =
                super::find_root(&fs.volume, fs.snapshot.root_tree, objectid::EXTENT_TREE)
                    .await
                    .unwrap()
                    .root;
            let mut accounting =
                Accounting::new(&fs.volume, extent_root, &space, fs.snapshot.csum_tree)
                    .await
                    .unwrap();
            let block = MetadataBlock {
                address: original.bytenr,
                level: original.level,
            };
            // Model two roots sharing this committed tree. Their extent pointers
            // initially have only the original owner's implicit data references.
            accounting
                .add_metadata(
                    &fs.volume,
                    &mut trees,
                    &mut space,
                    block,
                    MetadataReference::root(256),
                )
                .await
                .unwrap();
            let first = trees
                .apply(
                    &fs.volume,
                    original,
                    file_key,
                    Some(&original_file),
                    &mut space,
                )
                .await
                .unwrap();
            accounting
                .sync_allocations(&fs.volume, &mut trees, &mut space)
                .await
                .unwrap();
            let (_, old) = accounting
                .metadata(&fs.volume, &trees, block)
                .await
                .unwrap();
            assert_eq!(old.references(), 1);
            assert!(old.full_backrefs());
            let bytes = trees
                .get(&fs.volume, accounting.root, data_key)
                .await
                .unwrap()
                .unwrap();
            let data = DataExtent::parse(data_key, &bytes, fs.superblock().sectorsize()).unwrap();
            assert_eq!(data.refs, 2);
            // The old leaf's parent reference is external; the owner's reference
            // remains inline and now belongs to the new copy.
            assert_eq!(
                data.roots[&DataReference {
                    root: 5,
                    inode: inode.number(),
                    offset: 0
                }],
                1
            );
            let (parent_key, _) = trees
                .ceiling(
                    &fs.volume,
                    accounting.root,
                    Key::new(disk_bytenr, item_type::SHARED_DATA_REF, 0),
                )
                .await
                .unwrap()
                .unwrap();
            assert!(parent_key.is(disk_bytenr, item_type::SHARED_DATA_REF));

            let second = trees
                .apply(
                    &fs.volume,
                    TreeRoot {
                        tree: 256,
                        ..original
                    },
                    file_key,
                    Some(&original_file),
                    &mut space,
                )
                .await
                .unwrap();
            accounting
                .sync_allocations(&fs.volume, &mut trees, &mut space)
                .await
                .unwrap();
            assert!(matches!(
                accounting.metadata(&fs.volume, &trees, block).await,
                Err(WriteError::MissingExtent { .. })
            ));
            let bytes = trees
                .get(&fs.volume, accounting.root, data_key)
                .await
                .unwrap()
                .unwrap();
            let data = DataExtent::parse(data_key, &bytes, fs.superblock().sectorsize()).unwrap();
            assert_eq!(data.refs, 2);
            assert!(
                trees
                    .get(&fs.volume, accounting.root, parent_key)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                trees
                    .get(&fs.volume, first, file_key)
                    .await
                    .unwrap()
                    .is_some()
            );
            assert!(
                trees
                    .get(&fs.volume, second, file_key)
                    .await
                    .unwrap()
                    .is_some()
            );
            assert!(
                trees
                    .get(
                        &fs.volume,
                        fs.snapshot.csum_tree,
                        crate::checksums::key(disk_bytenr)
                    )
                    .await
                    .unwrap()
                    .is_some()
            );
            verify_group_usage(&fs, &trees, &accounting, space.group_root()).await;
        }
    });
}

#[test]
fn checksum_deletion_splits_items_preserves_neighbors_and_crosses_holes() {
    block_on(async {
        use crate::checksums::{Checksums, key};
        let fs = filesystem();
        let mut space = crate::space::Space::load(&fs.volume, fs.snapshot.root_tree)
            .await
            .unwrap();
        let mut trees = Trees::new(
            fs.superblock().generation() + 1,
            fs.superblock().csum_type(),
        );
        let mut checksums = Checksums {
            root: fs.snapshot.csum_tree,
        };
        let sector = u64::from(fs.superblock().sectorsize());
        let start = 16 << 20;
        let digests: Vec<_> = (0..20)
            .map(|index| {
                fs.superblock()
                    .csum_type()
                    .digest(&vec![index; sector as usize])
                    .as_bytes()
                    .to_vec()
            })
            .collect();
        for range in [0..8, 12..16, 16..20] {
            let bytes = digests[range.clone()].concat();
            checksums.root = trees
                .apply(
                    &fs.volume,
                    checksums.root,
                    key(start + range.start as u64 * sector),
                    Some(&bytes),
                    &mut space,
                )
                .await
                .unwrap();
        }
        checksums
            .remove(
                &fs.volume,
                &mut trees,
                &mut space,
                start + 2 * sector..start + 5 * sector,
            )
            .await
            .unwrap();
        assert_eq!(
            trees
                .get(&fs.volume, checksums.root, key(start))
                .await
                .unwrap()
                .unwrap(),
            digests[0..2].concat()
        );
        assert_eq!(
            trees
                .get(&fs.volume, checksums.root, key(start + 5 * sector))
                .await
                .unwrap()
                .unwrap(),
            digests[5..8].concat()
        );
        checksums
            .remove(
                &fs.volume,
                &mut trees,
                &mut space,
                start + 7 * sector..start + 17 * sector,
            )
            .await
            .unwrap();
        assert_eq!(
            contents(&trees, checksums.root),
            BTreeMap::from([
                (key(start), digests[0..2].concat()),
                (key(start + 5 * sector), digests[5..7].concat()),
                (key(start + 17 * sector), digests[17..20].concat()),
            ])
        );
        // Removing a missing checksum range is valid and does not allocate.
        let address = checksums.root.bytenr;
        checksums
            .remove(
                &fs.volume,
                &mut trees,
                &mut space,
                start + 8 * sector..start + 12 * sector,
            )
            .await
            .unwrap();
        assert_eq!(checksums.root.bytenr, address);
        let replacement = vec![0x91; 17 * sector as usize];
        checksums
            .set(
                &fs.volume,
                &mut trees,
                &mut space,
                start + sector,
                &replacement,
            )
            .await
            .unwrap();
        let digest = fs
            .superblock()
            .csum_type()
            .digest(&replacement[..sector as usize]);
        assert_eq!(
            contents(&trees, checksums.root),
            BTreeMap::from([
                (key(start), digests[0].clone()),
                (key(start + sector), digest.as_bytes().repeat(17)),
                (key(start + 18 * sector), digests[18..20].concat()),
            ])
        );
        checksums
            .remove(
                &fs.volume,
                &mut trees,
                &mut space,
                start..start + 20 * sector,
            )
            .await
            .unwrap();
        assert!(contents(&trees, checksums.root).is_empty());
        assert!(
            trees
                .ceiling(&fs.volume, fs.snapshot.csum_tree, key(0))
                .await
                .unwrap()
                .is_none()
        );
    });
}

#[test]
fn checksum_deletion_rejects_overlapping_or_partial_digest_items() {
    block_on(async {
        use crate::checksums::{Checksums, key};
        for overlap in [false, true] {
            let fs = filesystem();
            let mut space = crate::space::Space::load(&fs.volume, fs.snapshot.root_tree)
                .await
                .unwrap();
            let mut trees = Trees::new(
                fs.superblock().generation() + 1,
                fs.superblock().csum_type(),
            );
            let mut checksums = Checksums {
                root: fs.snapshot.csum_tree,
            };
            let sector = u64::from(fs.superblock().sectorsize());
            let start = 16 << 20;
            let width = fs.superblock().csum_type().size();
            let bytes = vec![0; if overlap { width * 4 } else { width + 1 }];
            checksums.root = trees
                .apply(
                    &fs.volume,
                    checksums.root,
                    key(start),
                    Some(&bytes),
                    &mut space,
                )
                .await
                .unwrap();
            if overlap {
                checksums.root = trees
                    .apply(
                        &fs.volume,
                        checksums.root,
                        key(start + sector),
                        Some(&vec![0; width]),
                        &mut space,
                    )
                    .await
                    .unwrap();
            }
            assert!(matches!(
                checksums
                    .remove(
                        &fs.volume,
                        &mut trees,
                        &mut space,
                        start..start + 4 * sector
                    )
                    .await,
                Err(WriteError::Checksum { .. })
            ));
        }
    });
}

#[test]
fn checksum_writes_span_full_leaf_items() {
    block_on(async {
        use crate::checksums::Checksums;
        let fs = filesystem();
        let mut space = crate::space::Space::load(&fs.volume, fs.snapshot.root_tree)
            .await
            .unwrap();
        let mut trees = Trees::new(
            fs.superblock().generation() + 1,
            fs.superblock().csum_type(),
        );
        let mut checksums = Checksums {
            root: fs.snapshot.csum_tree,
        };
        let sector = fs.superblock().sectorsize() as usize;
        let algorithm = fs.superblock().csum_type();
        // Enough digests to fill a complete node before accounting for headers.
        let sectors = fs.superblock().nodesize() as usize / algorithm.size();
        let bytes = vec![0x37; sector * sectors];
        let start = 16 << 20;
        checksums
            .set(&fs.volume, &mut trees, &mut space, start, &bytes)
            .await
            .unwrap();
        assert!(checksums.root.level > 0);
        let mut expected = start;
        let digest = algorithm.digest(&bytes[..sector]);
        for (key, payload) in contents(&trees, checksums.root) {
            assert_eq!(key.offset, expected);
            assert!(
                payload
                    .chunks_exact(algorithm.size())
                    .all(|stored| stored == digest.as_bytes())
            );
            expected += (payload.len() / algorithm.size() * sector) as u64;
        }
        assert_eq!(expected, start + bytes.len() as u64);
    });
}

#[test]
fn new_uuid_tree_supports_collisions_replacement_and_deletion() {
    block_on(async {
        use crate::ondisk::{item_type, objectid};
        use crate::uuid_tree::{UuidTree, key};

        let fs = filesystem();
        let mut space = crate::space::Space::load(&fs.volume, fs.snapshot.root_tree)
            .await
            .unwrap();
        let mut trees = Trees::new(fs.snapshot.root_tree.generation + 1, ChecksumType::Crc32c);
        let root = trees
            .create(
                &fs.volume,
                fs.snapshot.root_tree,
                objectid::UUID_TREE,
                &mut space,
            )
            .await
            .unwrap();
        assert!(contents(&trees, root).is_empty());
        let mut index = UuidTree::empty(root);
        let shared = key([1; 16], item_type::UUID_SUBVOL);
        let received = key([2; 16], item_type::UUID_RECEIVED);
        let both: Vec<_> = [5_u64, 256]
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect();
        let desired = BTreeMap::from([(shared, both), (received, 5_u64.to_le_bytes().to_vec())]);
        assert!(
            index
                .sync(&fs.volume, &mut trees, &mut space, desired.clone())
                .await
                .unwrap()
        );
        assert_eq!(contents(&trees, index.root), desired);
        assert!(
            !index
                .sync(&fs.volume, &mut trees, &mut space, desired)
                .await
                .unwrap()
        );
        let desired = BTreeMap::from([(shared, 256_u64.to_le_bytes().to_vec())]);
        assert!(
            index
                .sync(&fs.volume, &mut trees, &mut space, desired.clone())
                .await
                .unwrap()
        );
        assert_eq!(contents(&trees, index.root), desired);
        assert_eq!(index.root.bytenr, root.bytenr);
        assert!(trees.retired().is_empty());
    });
}

struct Allocator {
    next: u64,
    size: u64,
    remaining: usize,
}

impl MetadataAllocator for Allocator {
    fn revise(&mut self, address: u64, _: Key) {
        assert!(address < self.next);
    }

    fn cancel(&mut self, _: u64) {
        self.remaining += 1;
    }

    fn allocate(&mut self, tree: u64, _: u8, _: Key) -> Result<u64, WriteError> {
        if self.remaining == 0 {
            return Err(WriteError::NoSpace { tree });
        }
        self.remaining -= 1;
        let address = self.next;
        self.next += self.size;
        Ok(address)
    }
}

fn contents(trees: &Trees, root: TreeRoot) -> BTreeMap<Key, Vec<u8>> {
    let blocks: BTreeMap<_, _> = trees.blocks().collect();
    let mut pending = vec![(root.bytenr, root.level, root.generation)];
    let mut result = BTreeMap::new();
    while let Some((address, level, generation)) = pending.pop() {
        let block = TreeBlock::parse(blocks[&address]);
        assert_eq!(block.header().bytenr, address);
        assert_eq!(block.header().level, level);
        assert_eq!(block.header().generation, generation);
        assert_eq!(block.header().owner, root.tree);
        assert!(ChecksumType::Crc32c.verifies(&blocks[&address][32..], &block.header().csum));
        match &block {
            TreeBlock::Leaf { items, .. } => {
                let mut previous = None;
                for (slot, item) in items.iter().enumerate() {
                    let key = Key::new(
                        item.key.objectid,
                        item.key.key_type.to_raw(),
                        item.key.offset,
                    );
                    assert!(previous.is_none_or(|previous| previous < key));
                    assert!(
                        result
                            .insert(key, block.item_data(slot).unwrap().to_vec())
                            .is_none()
                    );
                    previous = Some(key);
                }
            }
            TreeBlock::Node { ptrs, .. } => {
                for ptr in ptrs {
                    pending.push((ptr.blockptr, level - 1, ptr.generation));
                }
            }
        }
    }
    result
}

#[test]
fn large_middle_item_can_require_three_leaves_and_preserves_payloads() {
    let fs = filesystem();
    let original = root_node(&fs);
    let mut leaf = Leaf::empty(Header::for_level(&original, 0));
    let capacity = fs.superblock().nodesize() as usize - 101;
    let small = vec![0x31; capacity / 2 - 25];
    let large = vec![0x72; capacity - 25];
    let left = Key::new(1000, 24, 0);
    let middle = Key::new(1001, 24, 0);
    let right = Key::new(1002, 24, 0);
    leaf.set(left, &small).unwrap();
    leaf.set(right, &small).unwrap();
    assert_eq!(leaf.partitions().len(), 1);
    leaf.set(middle, &large).unwrap();
    let ranges = leaf.partitions();
    assert_eq!(ranges.len(), 3);
    for (range, expected) in
        ranges
            .into_iter()
            .zip([small.as_slice(), large.as_slice(), small.as_slice()])
    {
        let bytes = leaf.encode(range, 32 << 20, 2, 5, ChecksumType::Crc32c);
        let parsed = TreeBlock::parse(&bytes);
        assert_eq!(parsed.item_data(0), Some(expected));
    }
    let oversized = vec![0; capacity];
    assert!(leaf.set(middle, &oversized).is_err());
    let bytes = leaf.encode(1..2, 32 << 20, 2, 5, ChecksumType::Crc32c);
    assert_eq!(
        TreeBlock::parse(&bytes).item_data(0),
        Some(large.as_slice())
    );
}

#[test]
fn cow_insert_replace_delete_preserve_the_original_tree_and_collapse_the_root() {
    block_on(async {
        let fs = filesystem();
        let original = root_node(&fs);
        let mut expected: BTreeMap<_, _> = (0..original.nritems())
            .map(|slot| (original.key(slot), original.item(slot).to_vec()))
            .collect();
        let mut root = fs.snapshot.default_subvolume.root;
        let mut trees = Trees::new(
            fs.superblock().generation() + 1,
            fs.superblock().csum_type(),
        );
        let mut allocator = Allocator {
            next: 32 << 20,
            size: u64::from(fs.superblock().nodesize()),
            remaining: 10_000,
        };
        // Reverse insertion exercises left-edge separator replacement as well as splits.
        for index in (0..300).rev() {
            let key = Key::new(1000 + index, 24, 0);
            let bytes = vec![index as u8; 257 + index as usize % 17 * 199];
            root = trees
                .apply(&fs.volume, root, key, Some(&bytes), &mut allocator)
                .await
                .unwrap();
            expected.insert(key, bytes);
        }
        assert!(root.level > 0);
        assert_eq!(contents(&trees, root), expected);
        for (&query, _) in expected.iter().step_by(31) {
            for key in [
                query,
                Key::new(query.objectid, query.item_type, query.offset + 1),
            ] {
                let wanted = expected
                    .range(key..)
                    .next()
                    .map(|(&key, bytes)| (key, bytes.clone()));
                assert_eq!(trees.ceiling(&fs.volume, root, key).await.unwrap(), wanted);
            }
        }
        assert!(
            trees
                .ceiling(&fs.volume, root, Key::new(u64::MAX, u8::MAX, u64::MAX))
                .await
                .unwrap()
                .is_none()
        );

        for index in (0..300).step_by(3) {
            let key = Key::new(1000 + index, 24, 0);
            let bytes = vec![0xa9; 7000];
            root = trees
                .apply(&fs.volume, root, key, Some(&bytes), &mut allocator)
                .await
                .unwrap();
            expected.insert(key, bytes);
        }
        assert_eq!(contents(&trees, root), expected);
        for key in expected.keys().copied().collect::<Vec<_>>() {
            root = trees
                .apply(&fs.volume, root, key, None, &mut allocator)
                .await
                .unwrap();
        }
        assert_eq!(root.level, 0);
        assert!(contents(&trees, root).is_empty());
        assert!(!trees.retired().is_empty());
        let reread = root_node(&fs);
        assert_eq!(original.nritems(), reread.nritems());
        for slot in 0..original.nritems() {
            assert_eq!(original.key(slot), reread.key(slot));
            assert_eq!(original.item(slot), reread.item(slot));
        }
    });
}

#[test]
fn splits_propagate_through_internal_nodes_and_deletion_shrinks_multiple_levels() {
    block_on(async {
        let fs = filesystem();
        let mut root = fs.snapshot.default_subvolume.root;
        let mut trees = Trees::new(
            fs.superblock().generation() + 1,
            fs.superblock().csum_type(),
        );
        let nodesize = fs.superblock().nodesize() as usize;
        let mut allocator = Allocator {
            next: 32 << 20,
            size: nodesize as u64,
            remaining: 10_000,
        };
        // Each payload occupies its own leaf. More leaves than a branch's
        // key-pointer capacity forces a split above the leaf level too.
        let leaves = (nodesize - 101) / 33 + 3;
        let payload = vec![0x75; nodesize - 101 - 25];
        for index in 0..leaves {
            root = trees
                .apply(
                    &fs.volume,
                    root,
                    Key::new(1000 + index as u64, 24, 0),
                    Some(&payload),
                    &mut allocator,
                )
                .await
                .unwrap();
        }
        assert_eq!(root.level, 2);
        let records = contents(&trees, root);
        for index in 0..leaves {
            assert_eq!(records[&Key::new(1000 + index as u64, 24, 0)], payload);
        }
        for index in 0..leaves {
            root = trees
                .apply(
                    &fs.volume,
                    root,
                    Key::new(1000 + index as u64, 24, 0),
                    None,
                    &mut allocator,
                )
                .await
                .unwrap();
        }
        assert_eq!(root.level, 0);
        let original = root_node(&fs);
        let expected: BTreeMap<_, _> = (0..original.nritems())
            .map(|slot| (original.key(slot), original.item(slot).to_vec()))
            .collect();
        assert_eq!(contents(&trees, root), expected);
    });
}

#[test]
fn space_reservations_exclude_existing_extents_and_return_private_allocations() {
    use crate::space::{Space, SpaceError};
    use btrfs_disk::reader::{Traversal, filesystem_open, tree_walk};
    use btrfs_disk::tree::KeyType;

    block_on(async {
        let (fs, reference) = filesystem_image();
        let mut independent = filesystem_open(reference).unwrap();
        let mut occupied = Vec::new();
        tree_walk(
            &mut independent.reader,
            independent.tree_roots[&2].0,
            Traversal::Dfs,
            &mut |block| {
                if let TreeBlock::Leaf { items, .. } = block {
                    for item in items {
                        let length = match item.key.key_type {
                            KeyType::ExtentItem => item.key.offset,
                            KeyType::MetadataItem => u64::from(independent.superblock.nodesize),
                            _ => continue,
                        };
                        occupied.push(item.key.objectid..item.key.objectid + length);
                    }
                }
            },
        )
        .unwrap();
        let mut space = Space::load(&fs.volume, fs.snapshot.root_tree)
            .await
            .unwrap();
        let free_root = super::find_root(
            &fs.volume,
            fs.snapshot.root_tree,
            crate::ondisk::objectid::FREE_SPACE_TREE,
        )
        .await
        .unwrap()
        .root;
        let mut records = BTreeMap::new();
        let mut cursor = crate::tree::Cursor::ceiling(&fs.volume, free_root, Key::new(0, 0, 0))
            .await
            .unwrap();
        while let Some(key) = cursor.key() {
            records.insert(key, cursor.item().unwrap().to_vec());
            cursor.advance(&fs.volume).await.unwrap();
        }
        for group in space
            .committed_free(&rpds::RedBlackTreeMapSync::new_sync())
            .unwrap()
        {
            let key = Key::new(
                group.range.start,
                crate::ondisk::item_type::FREE_SPACE_INFO,
                group.range.end - group.range.start,
            );
            let decoded = crate::free_space::FreeGroup::decode(
                key,
                &records[&key],
                &records,
                u64::from(fs.superblock().sectorsize()),
            )
            .unwrap();
            assert_eq!(decoded.free, group.free);
        }
        let original_available = space.available_metadata();
        let size = u64::from(fs.superblock().nodesize());
        let mut allocated = Vec::new();
        loop {
            match space.allocate(5, 0, Key::new(256, 1, 0)) {
                Ok(address) => {
                    assert!(
                        occupied
                            .iter()
                            .all(|range| address + size <= range.start || range.end <= address)
                    );
                    assert!(
                        fs.volume
                            .chunks()
                            .superblock_stripes()
                            .iter()
                            .all(|range| address + size <= range.start || range.end <= address)
                    );
                    assert!(fs.volume.chunks().map(address, size).is_ok());
                    allocated.push(address);
                }
                Err(WriteError::Space(SpaceError::Exhausted { .. })) => break,
                Err(error) => panic!("unexpected allocation failure: {error}"),
            }
        }
        assert!(!allocated.is_empty());
        let mut unique = allocated.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), allocated.len());
        assert_eq!(space.metadata().count(), allocated.len());
        assert!(!space.cancel_metadata(fs.snapshot.default_subvolume.root.bytenr));
        // Alternating holes exercise coalescing on both sides of each free.
        for &address in allocated.iter().step_by(2) {
            assert!(space.cancel_metadata(address));
            assert!(!space.cancel_metadata(address));
        }
        for &address in allocated.iter().skip(1).step_by(2) {
            assert!(space.cancel_metadata(address));
        }
        assert_eq!(space.available_metadata(), original_available);
        assert_eq!(space.metadata().count(), 0);
        let address = space.allocate(5, 0, Key::new(256, 1, 0)).unwrap();
        assert_eq!(address, allocated[0]);
    });
}

#[test]
fn repeated_cow_edits_recycle_private_space_and_pin_committed_blocks() {
    let fs = filesystem();
    let mut space = block_on(crate::space::Space::load(&fs.volume, fs.snapshot.root_tree)).unwrap();
    let available = space.available_metadata();
    let original = fs.snapshot.default_subvolume.root;
    let mut root = original;
    let mut trees = Trees::new(original.generation + 1, ChecksumType::Crc32c);
    let original_node = root_node(&fs);
    let original_key = original_node.key(0);
    assert_eq!(
        block_on(trees.get(&fs.volume, root, original_key)).unwrap(),
        Some(original_node.item(0).to_vec())
    );
    assert_eq!(
        block_on(trees.get(&fs.volume, root, Key::new(u64::MAX, 0, 0))).unwrap(),
        None
    );
    let key = Key::new(900, 1, 0);
    let mut private_address = None;
    for revision in 0_u64..2048 {
        let checkpoint = trees.clone();
        let checkpoint_space = space.clone();
        let checkpoint_available = space.available_metadata();
        let checkpoint_reservations = space.metadata().collect::<Vec<_>>();
        let checkpoint_root = root;
        root = block_on(trees.apply(
            &fs.volume,
            root,
            key,
            Some(&revision.to_le_bytes()),
            &mut space,
        ))
        .unwrap();
        assert_eq!(
            block_on(trees.get(&fs.volume, root, key)).unwrap(),
            Some(revision.to_le_bytes().to_vec())
        );
        assert_eq!(checkpoint_space.available_metadata(), checkpoint_available);
        assert_eq!(
            checkpoint_space.metadata().collect::<Vec<_>>(),
            checkpoint_reservations
        );
        // The live overlay reuses its private address, while the retained
        // version must still resolve that address to its earlier contents.
        assert_eq!(
            block_on(checkpoint.get(&fs.volume, checkpoint_root, key)).unwrap(),
            revision
                .checked_sub(1)
                .map(|value| value.to_le_bytes().to_vec())
        );
        assert_eq!(*private_address.get_or_insert(root.bytenr), root.bytenr);
        assert_eq!(trees.blocks().count(), 1);
        assert_eq!(space.metadata().count(), 1);
        assert_eq!(
            space.available_metadata(),
            available - u64::from(fs.volume.superblock().nodesize())
        );
        assert_eq!(
            trees.retired().iter().copied().collect::<Vec<_>>(),
            &[crate::tree::write::RetiredBlock {
                tree: original.tree,
                address: original.bytenr,
                level: original.level,
                generation: original.generation
            }]
        );
        assert!(!space.cancel_metadata(original.bytenr));
        let (address, reservation) = space.metadata().next().unwrap();
        assert_eq!(address, root.bytenr);
        assert_eq!(reservation.tree, root.tree);
        assert_eq!(reservation.level, root.level);
        let block = trees.blocks().next().unwrap().1;
        let node = TreeBlock::parse(block);
        let TreeBlock::Leaf { items, .. } = node else {
            panic!("expected leaf")
        };
        assert_eq!(reservation.first.objectid, items[0].key.objectid);
    }
    assert_eq!(contents(&trees, root)[&key], 2047_u64.to_le_bytes());
    assert_eq!(root_node(&fs).level(), original.level);
    loop {
        match space.allocate(5, 0, Key::new(256, 1, 0)) {
            Ok(_) => {}
            Err(WriteError::Space(crate::space::SpaceError::Exhausted { .. })) => break,
            Err(error) => panic!("unexpected allocation failure: {error}"),
        }
    }
    assert_eq!(space.available_metadata(), 0);
    let previous = root.bytenr;
    root = block_on(trees.apply(
        &fs.volume,
        root,
        key,
        Some(&2048_u64.to_le_bytes()),
        &mut space,
    ))
    .unwrap();
    assert_eq!(root.bytenr, previous);
    assert_eq!(contents(&trees, root)[&key], 2048_u64.to_le_bytes());
}

#[test]
fn metadata_accounting_removes_inline_and_external_refs_without_releasing_committed_space() {
    use crate::accounting::{Accounting, MetadataBlock};
    use crate::allocation::MetadataReference;
    use crate::ondisk::{item_type, objectid};
    use crate::space::Space;

    block_on(async {
        let fs = filesystem();
        let original_root =
            super::find_root(&fs.volume, fs.snapshot.root_tree, objectid::EXTENT_TREE)
                .await
                .unwrap()
                .root;
        let block = MetadataBlock {
            address: fs.snapshot.default_subvolume.root.bytenr,
            level: fs.snapshot.default_subvolume.root.level,
        };
        let size = u64::from(fs.superblock().nodesize());
        let mut trees = Trees::new(fs.superblock().generation() + 1, ChecksumType::Crc32c);
        let mut space = Space::load(&fs.volume, fs.snapshot.root_tree)
            .await
            .unwrap();
        let mut accounting =
            Accounting::new(&fs.volume, original_root, &space, fs.snapshot.csum_tree)
                .await
                .unwrap();
        let mut key = Key::new(
            block.address,
            item_type::METADATA_ITEM,
            u64::from(block.level),
        );
        let original = match trees.get(&fs.volume, original_root, key).await.unwrap() {
            Some(bytes) => bytes,
            None => {
                key = Key::new(block.address, item_type::EXTENT_ITEM, size);
                trees
                    .get(&fs.volume, original_root, key)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        let parent = MetadataReference::parent(fs.snapshot.root_tree.bytenr);
        let parent_key = parent.key(block.address);
        accounting
            .add_metadata(&fs.volume, &mut trees, &mut space, block, parent)
            .await
            .unwrap();
        let (_, extent) = accounting
            .metadata(&fs.volume, &trees, block)
            .await
            .unwrap();
        assert_eq!(extent.references(), 2);
        assert!(!extent.full_backrefs());
        accounting
            .set_full_backrefs(&fs.volume, &mut trees, &mut space, block)
            .await
            .unwrap();
        let (_, extent) = accounting
            .metadata(&fs.volume, &trees, block)
            .await
            .unwrap();
        assert!(extent.full_backrefs());
        assert_eq!(extent.references(), 2);
        assert!(
            !accounting
                .remove_metadata(
                    &fs.volume,
                    &mut trees,
                    &mut space,
                    block,
                    MetadataReference::root(objectid::FS_TREE)
                )
                .await
                .unwrap()
        );
        let bytes = trees
            .get(&fs.volume, accounting.root, key)
            .await
            .unwrap()
            .unwrap();
        let disk_key = btrfs_disk::tree::DiskKey {
            objectid: key.objectid,
            key_type: if key.item_type == item_type::METADATA_ITEM {
                btrfs_disk::tree::KeyType::MetadataItem
            } else {
                btrfs_disk::tree::KeyType::ExtentItem
            },
            offset: key.offset,
        };
        let independent = btrfs_disk::items::ExtentItem::parse(&bytes, &disk_key).unwrap();
        assert_eq!(independent.refs, 1);
        assert!(independent.inline_refs.is_empty());
        assert_eq!(
            trees
                .get(&fs.volume, accounting.root, parent_key)
                .await
                .unwrap(),
            Some(Vec::new())
        );
        assert!(
            accounting
                .remove_metadata(&fs.volume, &mut trees, &mut space, block, parent)
                .await
                .unwrap()
        );
        assert_eq!(
            trees.get(&fs.volume, accounting.root, key).await.unwrap(),
            None
        );
        assert_eq!(
            trees
                .get(&fs.volume, accounting.root, parent_key)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            trees.get(&fs.volume, original_root, key).await.unwrap(),
            Some(original)
        );
        assert!(!space.cancel_metadata(block.address));
        accounting
            .sync_allocations(&fs.volume, &mut trees, &mut space)
            .await
            .unwrap();
        verify_group_usage(&fs, &trees, &accounting, space.group_root()).await;
    });
}

#[test]
fn metadata_registration_converges_across_extent_tree_growth_and_cancellation() {
    use crate::accounting::Accounting;
    use crate::ondisk::{item_type, objectid};
    use crate::space::Space;

    for (separate, free_tree) in [(false, true), (true, true), (false, false)] {
        block_on(async {
            let fs = filesystem_image_with_features(separate, free_tree).0;
            let extent_root =
                super::find_root(&fs.volume, fs.snapshot.root_tree, objectid::EXTENT_TREE)
                    .await
                    .unwrap()
                    .root;
            let mut space = Space::load(&fs.volume, fs.snapshot.root_tree)
                .await
                .unwrap();
            let mut trees = Trees::new(
                fs.superblock().generation() + 1,
                fs.superblock().csum_type(),
            );
            let size = u64::from(fs.superblock().nodesize());
            let mut accounting =
                Accounting::new(&fs.volume, extent_root, &space, fs.snapshot.csum_tree)
                    .await
                    .unwrap();
            assert_eq!(accounting.free_root().is_some(), free_tree);
            let mut root = fs.snapshot.default_subvolume.root;
            // Full leaves force enough allocations to split the extent tree as its
            // own allocation records are registered.
            let count = (size as usize - 101) / 33 + 3;
            let payload = vec![0x56; size as usize - 101 - 25];
            for index in 0..count {
                root = trees
                    .apply(
                        &fs.volume,
                        root,
                        Key::new(1000 + index as u64, 24, 0),
                        Some(&payload),
                        &mut space,
                    )
                    .await
                    .unwrap();
            }
            let mut previously_registered: Vec<Key> = Vec::new();
            for shrink in [false, true] {
                if shrink {
                    for index in 0..count {
                        root = trees
                            .apply(
                                &fs.volume,
                                root,
                                Key::new(1000 + index as u64, 24, 0),
                                None,
                                &mut space,
                            )
                            .await
                            .unwrap();
                    }
                }
                accounting
                    .sync_allocations(&fs.volume, &mut trees, &mut space)
                    .await
                    .unwrap();
                verify_group_usage(&fs, &trees, &accounting, space.group_root()).await;
                let current: BTreeMap<_, _> = space.metadata().collect();
                assert_eq!(current.len(), trees.blocks().count());
                if !shrink {
                    assert!(accounting.root.level > 0);
                }
                for (&address, record) in &current {
                    let skinny = fs.superblock().incompat_flags() & (1 << 8) != 0;
                    let key = if skinny {
                        Key::new(address, item_type::METADATA_ITEM, u64::from(record.level))
                    } else {
                        Key::new(address, item_type::EXTENT_ITEM, size)
                    };
                    let bytes = trees
                        .get(&fs.volume, accounting.root, key)
                        .await
                        .unwrap()
                        .unwrap();
                    let disk_key = btrfs_disk::tree::DiskKey {
                        objectid: address,
                        key_type: if skinny {
                            btrfs_disk::tree::KeyType::MetadataItem
                        } else {
                            btrfs_disk::tree::KeyType::ExtentItem
                        },
                        offset: key.offset,
                    };
                    let independent =
                        btrfs_disk::items::ExtentItem::parse(&bytes, &disk_key).unwrap();
                    assert_eq!(independent.refs, 1);
                    assert_eq!(independent.generation, trees.generation());
                    assert_eq!(independent.inline_refs.len(), 1);
                    assert!(
                        matches!(independent.inline_refs[0], btrfs_disk::items::InlineRef::TreeBlockBackref { root, .. } if root == record.tree)
                    );
                    if !skinny {
                        let first = independent.tree_block_key.unwrap();
                        assert_eq!(first.objectid, record.first.objectid);
                        assert_eq!(first.key_type.to_raw(), record.first.item_type);
                        assert_eq!(first.offset, record.first.offset);
                    }
                }
                for key in previously_registered {
                    if !current.contains_key(&key.objectid) {
                        assert_eq!(
                            trees.get(&fs.volume, accounting.root, key).await.unwrap(),
                            None
                        );
                    }
                }
                previously_registered = current
                    .iter()
                    .map(|(&address, record)| {
                        if fs.superblock().incompat_flags() & (1 << 8) != 0 {
                            Key::new(address, item_type::METADATA_ITEM, u64::from(record.level))
                        } else {
                            Key::new(address, item_type::EXTENT_ITEM, size)
                        }
                    })
                    .collect();
                let stable: Vec<_> = trees
                    .blocks()
                    .map(|(address, bytes)| (address, bytes.to_vec()))
                    .collect();
                let available = space.available_metadata();
                accounting
                    .sync_allocations(&fs.volume, &mut trees, &mut space)
                    .await
                    .unwrap();
                assert_eq!(
                    stable,
                    trees
                        .blocks()
                        .map(|(address, bytes)| (address, bytes.to_vec()))
                        .collect::<Vec<_>>()
                );
                assert_eq!(space.available_metadata(), available);
            }
            assert!(!space.cancel_metadata(fs.snapshot.default_subvolume.root.bytenr));
        });
    }
}

#[test]
fn cow_blocks_take_the_writing_tree_owner_from_a_shared_original() {
    block_on(async {
        let fs = filesystem();
        let original = fs.snapshot.default_subvolume.root;
        let mut root = TreeRoot {
            tree: 256,
            ..original
        };
        let mut trees = Trees::new(original.generation + 1, fs.superblock().csum_type());
        let mut space = crate::space::Space::load(&fs.volume, fs.snapshot.root_tree)
            .await
            .unwrap();
        let payload = vec![0x74; fs.superblock().nodesize() as usize - 101 - 25];
        for index in 0..3 {
            root = trees
                .apply(
                    &fs.volume,
                    root,
                    Key::new(1000 + index, 24, 0),
                    Some(&payload),
                    &mut space,
                )
                .await
                .unwrap();
        }
        assert!(root.level > 0);
        contents(&trees, root);
        for (address, reservation) in space.metadata() {
            let bytes = trees
                .blocks()
                .find(|(candidate, _)| *candidate == address)
                .unwrap()
                .1;
            assert_eq!(TreeBlock::parse(bytes).header().owner, reservation.tree);
            assert_eq!(reservation.tree, 256);
        }
        let original_node = root_node(&fs);
        assert_eq!(original_node.key(0), Key::new(256, 1, 0));
        assert!(
            trees
                .blocks()
                .all(|(address, _)| address != original.bytenr)
        );
    });
}

async fn verify_group_usage(
    fs: &Filesystem<Disk>,
    trees: &Trees,
    accounting: &crate::accounting::Accounting,
    original_groups: TreeRoot,
) {
    use crate::ondisk::item_type;
    let extents = contents(trees, accounting.root);
    let free_records = accounting.free_root().map(|root| contents(trees, root));
    let mut cursor = crate::tree::Cursor::ceiling(&fs.volume, original_groups, Key::new(0, 0, 0))
        .await
        .unwrap();
    let mut groups = 0;
    while let Some(key) = cursor.key() {
        if key.item_type == item_type::BLOCK_GROUP_ITEM {
            let original =
                btrfs_disk::items::BlockGroupItem::parse(cursor.item().unwrap()).unwrap();
            let bytes = trees
                .get(&fs.volume, accounting.group_root(), key)
                .await
                .unwrap()
                .unwrap();
            let group = btrfs_disk::items::BlockGroupItem::parse(&bytes).unwrap();
            let expected: u64 = extents
                .keys()
                .filter(|extent| {
                    key.objectid <= extent.objectid && extent.objectid < key.objectid + key.offset
                })
                .map(|extent| match extent.item_type {
                    item_type::METADATA_ITEM => u64::from(fs.superblock().nodesize()),
                    item_type::EXTENT_ITEM => extent.offset,
                    _ => 0,
                })
                .sum();
            assert_eq!(group.used, expected, "group {key}");
            assert_eq!(group.flags, original.flags);
            assert_eq!(group.chunk_objectid, original.chunk_objectid);
            if let Some(records) = &free_records {
                let info_key = Key::new(key.objectid, item_type::FREE_SPACE_INFO, key.offset);
                let info = btrfs_disk::items::FreeSpaceInfo::parse(&records[&info_key]).unwrap();
                let decoded = crate::free_space::FreeGroup::decode(
                    info_key,
                    &records[&info_key],
                    records,
                    u64::from(fs.superblock().sectorsize()),
                )
                .unwrap();
                let mut expected_free = Vec::new();
                let mut address = key.objectid;
                for extent in extents.keys().filter(|extent| {
                    key.objectid <= extent.objectid
                        && extent.objectid < key.objectid + key.offset
                        && matches!(
                            extent.item_type,
                            item_type::EXTENT_ITEM | item_type::METADATA_ITEM
                        )
                }) {
                    if address < extent.objectid {
                        expected_free.push(address..extent.objectid);
                    }
                    address = extent.objectid
                        + if extent.item_type == item_type::METADATA_ITEM {
                            u64::from(fs.superblock().nodesize())
                        } else {
                            extent.offset
                        };
                }
                if address < key.objectid + key.offset {
                    expected_free.push(address..key.objectid + key.offset);
                }
                assert_eq!(decoded.free.iter().collect::<Vec<_>>(), expected_free);
                assert_eq!(info.extent_count as usize, expected_free.len());
                assert_eq!(
                    expected_free
                        .iter()
                        .map(|range| range.end - range.start)
                        .sum::<u64>()
                        + group.used,
                    key.offset
                );
            }
            groups += 1;
        }
        cursor.advance(&fs.volume).await.unwrap();
    }
    assert!(groups > 0);
}

#[test]
fn root_publication_preparation_converges_with_allocation_accounting() {
    use crate::accounting::Accounting;
    use crate::ondisk::objectid;
    use crate::roots::Roots;
    use crate::space::Space;

    block_on(async {
        let fs = filesystem();
        let mut roots = Roots::load(&fs.volume, fs.snapshot.root_tree)
            .await
            .unwrap();
        let mut space = Space::load(&fs.volume, fs.snapshot.root_tree)
            .await
            .unwrap();
        let mut accounting = Accounting::new(
            &fs.volume,
            roots.get(objectid::EXTENT_TREE).unwrap(),
            &space,
            fs.snapshot.csum_tree,
        )
        .await
        .unwrap();
        let mut trees = Trees::new(
            fs.superblock().generation() + 1,
            fs.superblock().csum_type(),
        );
        let size = u64::from(fs.superblock().nodesize());
        let mut root = roots.get(objectid::FS_TREE).unwrap();
        let payload = vec![0x37; size as usize - 101 - 25];
        for shrink in [false, true] {
            for index in 0..4 {
                root = trees
                    .apply(
                        &fs.volume,
                        root,
                        Key::new(1000 + index, 24, 0),
                        (!shrink).then_some(payload.as_slice()),
                        &mut space,
                    )
                    .await
                    .unwrap();
            }
            roots.set(root).unwrap();
            loop {
                accounting
                    .sync_allocations(&fs.volume, &mut trees, &mut space)
                    .await
                    .unwrap();
                roots.set(accounting.root).unwrap();
                roots.set(accounting.checksum_root()).unwrap();
                roots.set(accounting.group_root()).unwrap();
                if let Some(root) = accounting.free_root() {
                    roots.set(root).unwrap();
                }
                if !roots
                    .sync(&fs.volume, &mut trees, &mut space, &mut || {
                        panic!("fixture roots have valid extensions")
                    })
                    .await
                    .unwrap()
                {
                    break;
                }
            }
            for tree in [
                objectid::FS_TREE,
                objectid::EXTENT_TREE,
                objectid::BLOCK_GROUP_TREE,
                objectid::FREE_SPACE_TREE,
            ] {
                let cursor = crate::tree::Cursor::floor(
                    &fs.volume,
                    fs.snapshot.root_tree,
                    Key::new(tree, crate::ondisk::item_type::ROOT_ITEM, u64::MAX),
                )
                .await
                .unwrap();
                let key = cursor.key().unwrap();
                let original = cursor.item().unwrap().to_vec();
                let bytes = trees
                    .get(&fs.volume, roots.root, key)
                    .await
                    .unwrap()
                    .unwrap();
                let parsed = btrfs_disk::items::RootItem::parse(&bytes).unwrap();
                let root = roots.get(tree).unwrap();
                assert_eq!(parsed.bytenr, root.bytenr);
                assert_eq!(parsed.level, root.level);
                assert_eq!(parsed.generation, root.generation);
                assert_eq!(parsed.generation_v2, root.generation);
                let blocks = trees
                    .blocks()
                    .filter(|(_, bytes)| TreeBlock::parse(bytes).header().owner == tree)
                    .count();
                assert_eq!(parsed.bytes_used, blocks as u64 * size);
                assert_eq!(
                    parsed.uuid,
                    btrfs_disk::items::RootItem::parse(&original).unwrap().uuid
                );
                assert_eq!(
                    trees
                        .get(&fs.volume, fs.snapshot.root_tree, key)
                        .await
                        .unwrap(),
                    Some(original)
                );
            }
            verify_group_usage(&fs, &trees, &accounting, space.group_root()).await;
            assert_eq!(root.level, u8::from(!shrink));
        }
    });
}

#[cfg(feature = "write")]
fn running_attributes() -> super::writer::Attributes {
    super::writer::Attributes {
        permissions: 0o644,
        uid: 1000,
        gid: 1000,
        umask: 0,
    }
}

#[cfg(feature = "write")]
const RUNNING_WHEN: crate::Timestamp = crate::Timestamp {
    seconds: 1_700_000_200,
    nanoseconds: 0,
};

/// Creates `name` in the root directory holding `contents`, and leaves the
/// change in the running transaction.
#[cfg(feature = "write")]
async fn write_running(
    writer: &mut super::writer::Writer<Disk, FixtureUuids>,
    name: &[u8],
    contents: &[u8],
) {
    use super::writer::Name;
    let concurrency = std::thread::available_parallelism().unwrap();
    let mut tx = writer.join().await.unwrap();
    let root = tx.root().await.unwrap();
    let file = tx
        .create_file(
            Name {
                parent: &root,
                bytes: name,
            },
            running_attributes(),
            RUNNING_WHEN,
        )
        .await
        .unwrap();
    assert_eq!(
        tx.write_file(
            &file,
            0,
            contents,
            WriteOptions::plain(RUNNING_WHEN, concurrency)
        )
        .await
        .unwrap(),
        contents.len()
    );
}

#[cfg(feature = "write")]
async fn read_whole<S: BlockSource>(view: &Filesystem<S>, name: &[u8]) -> Vec<u8> {
    let inode = view.resolve(name).await.unwrap();
    let mut output = vec![0; inode.metadata().size as usize];
    assert_eq!(
        view.read(&inode, 0, &mut output).await.unwrap(),
        output.len()
    );
    output
}

/// A mutation is visible to every reader once it is accepted, while the
/// device keeps the previous generation; the commit makes it durable, and a
/// view taken before the commit keeps reading what it saw.
#[cfg(feature = "write")]
#[test]
fn mutations_are_visible_at_once_and_durable_at_commit() {
    block_on(async {
        let (_, image) = filesystem_image();
        let mut writer = mount_writer(Disk(image.try_clone().unwrap()))
            .await
            .unwrap();
        let committed = writer.snapshot().unwrap();
        let generation = committed.superblock().generation();
        assert!(writer.dirty().is_clean());
        let name = "可见-見える-보이는".as_bytes();
        let contents = "立即可见 すぐ見える 바로 보인다\n".repeat(400).into_bytes();
        write_running(&mut writer, name, &contents).await;
        assert!(!writer.dirty().is_clean());

        let view = writer.snapshot().unwrap();
        assert_eq!(read_whole(&view, name).await, contents);
        let committed_root = committed.root().await.unwrap();
        assert!(
            committed
                .lookup(&committed_root, name)
                .await
                .unwrap()
                .is_none()
        );
        let device = Filesystem::mount(Disk(image.try_clone().unwrap()))
            .await
            .unwrap();
        assert_eq!(device.superblock().generation(), generation);
        let device_root = device.root().await.unwrap();
        assert!(device.lookup(&device_root, name).await.unwrap().is_none());

        writer.commit().await.unwrap();
        assert!(writer.dirty().is_clean());
        let device = Filesystem::mount(Disk(image)).await.unwrap();
        assert_eq!(device.superblock().generation(), generation + 1);
        assert_eq!(read_whole(&device, name).await, contents);
        assert_eq!(read_whole(&view, name).await, contents);
        assert_eq!(
            read_whole(&writer.snapshot().unwrap(), name).await,
            contents
        );
    });
}

/// Private data the running transaction releases stays out of the allocator
/// while a view that reads it lives, within the transaction and across its
/// commit, and returns once the view is gone.
#[cfg(feature = "write")]
#[test]
fn a_view_keeps_released_private_data_from_reuse_until_it_is_dropped() {
    use super::writer::{Name, Writer};
    block_on(async {
        let (_, image) = filesystem_image();
        let mut writer = mount_writer(Disk(image)).await.unwrap();
        let first = noise(64 << 10, 1);
        write_running(&mut writer, "第一-最初-첫째".as_bytes(), &first).await;
        let view = writer.snapshot().unwrap();
        async fn truncate(writer: &mut Writer<Disk, FixtureUuids>, name: &[u8]) -> u64 {
            let concurrency = std::thread::available_parallelism().unwrap();
            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            let file = tx.lookup(&root, name).await.unwrap().unwrap();
            tx.truncate_file(&file, 0, WriteOptions::plain(RUNNING_WHEN, concurrency))
                .await
                .unwrap();
            tx.edits().space.held_data_bytes()
        }
        // Truncating frees the extent this transaction wrote; first-fit would
        // hand its range to the next write of the same size.
        assert_eq!(
            truncate(&mut writer, "第一-最初-첫째".as_bytes()).await,
            first.len() as u64
        );
        let second = noise(64 << 10, 2);
        write_running(&mut writer, "第二-二番目-둘째".as_bytes(), &second).await;
        assert_eq!(read_whole(&view, "第一-最初-첫째".as_bytes()).await, first);
        drop(view);
        {
            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            tx.set_times(&root, None, Some(RUNNING_WHEN), RUNNING_WHEN)
                .await
                .unwrap();
            assert_eq!(tx.edits().space.held_data_bytes(), 0);
        }

        // A view still alive at the commit keeps the range through the reader
        // pin of the generation its transaction started from.
        let third = noise(64 << 10, 3);
        write_running(&mut writer, "第三-三番目-셋째".as_bytes(), &third).await;
        let view = writer.snapshot().unwrap();
        truncate(&mut writer, "第三-三番目-셋째".as_bytes()).await;
        writer.commit().await.unwrap();
        for seed in 4..8 {
            let name = alloc::format!("后来-後で-나중-{seed}");
            write_running(&mut writer, name.as_bytes(), &noise(64 << 10, seed)).await;
        }
        assert_eq!(
            read_whole(&view, "第三-三番目-셋째".as_bytes()).await,
            third
        );
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        tx.unlink_name(
            Name {
                parent: &root,
                bytes: "第二-二番目-둘째".as_bytes(),
            },
            RUNNING_WHEN,
        )
        .await
        .unwrap();
        assert_eq!(
            read_whole(&view, "第三-三番目-셋째".as_bytes()).await,
            third
        );
    });
}

/// A commit that fails poisons the writer: the changes it carried were
/// already visible and cannot be withdrawn. The device keeps the previous
/// complete generation.
#[cfg(feature = "write")]
#[test]
fn a_failed_commit_poisons_the_writer_and_leaves_the_previous_tree() {
    use super::writer::Name;
    block_on(async {
        let (_, image) = filesystem_image();
        let reference = image.try_clone().unwrap();
        let generation = Filesystem::mount(Disk(image.try_clone().unwrap()))
            .await
            .unwrap()
            .superblock()
            .generation();
        // A directory stages nothing before its commit: the first block the
        // device is asked for is the commit's.
        let mut writer = mount_writer(FaultDisk {
            disk: Disk(image),
            fail_at: 1,
            operations: Default::default(),
            poisoned: std::sync::atomic::AtomicBool::new(false),
        })
        .await
        .unwrap();
        let name = "目录-ディレクトリ-디렉터리".as_bytes();
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        tx.create_directory(
            Name {
                parent: &root,
                bytes: name,
            },
            running_attributes(),
            RUNNING_WHEN,
        )
        .await
        .unwrap();
        let view = writer.snapshot().unwrap();
        assert!(view.resolve(name).await.is_ok());
        assert!(writer.commit().await.is_err());
        assert!(matches!(writer.snapshot(), Err(WriteError::WriterPoisoned)));
        assert!(matches!(
            writer.join().await,
            Err(WriteError::WriterPoisoned)
        ));
        assert!(matches!(
            writer.commit().await,
            Err(WriteError::WriterPoisoned)
        ));
        let device = Filesystem::mount(Disk(reference)).await.unwrap();
        assert_eq!(device.superblock().generation(), generation);
        let device_root = device.root().await.unwrap();
        assert!(device.lookup(&device_root, name).await.unwrap().is_none());
    });
}

/// On a device whose data fills it, metadata runs out at the mutation: each
/// one that cannot reserve is refused with no space before it changes
/// anything, the writer stays healthy, and the commits that follow write
/// every accepted change.
#[cfg(feature = "write")]
#[test]
fn a_full_device_refuses_mutations_and_the_writer_keeps_committing() {
    use super::writer::Name;
    use crate::space::{DATA, METADATA, SpaceError};
    use ferrosys::btrfs::Profile;

    block_on(async {
        let (_, image) = profile_image(Profile::Dup, Profile::Single);
        let mut writer = mount_writer(Disk(image.try_clone().unwrap()))
            .await
            .unwrap();
        let concurrency = NonZeroUsize::MIN;
        let options = || WriteOptions::plain(RUNNING_WHEN, concurrency);
        const MIB: usize = 1 << 20;

        // Data takes every chunk the device can still make.
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let ballast = tx
            .create_file(
                Name {
                    parent: &root,
                    bytes: "压舱-バラスト-밸러스트".as_bytes(),
                },
                running_attributes(),
                RUNNING_WHEN,
            )
            .await
            .unwrap();
        let mut offset = 0;
        loop {
            match tx
                .write_file(&ballast, offset, &noise(MIB, offset as u8), options())
                .await
            {
                Ok(MIB) => offset += MIB as u64,
                Ok(_) | Err(WriteError::Space(SpaceError::Exhausted { flags: DATA, .. })) => {
                    break;
                }
                Err(error) => panic!("unexpected ballast failure: {error}"),
            }
        }
        tx.commit().await.unwrap();

        // Small files go inline, into metadata, until it has no room left:
        // 62 copies of 33 bytes stay within Linux's default `max_inline`.
        let contents = |index: usize| {
            alloc::format!("{index:05} 文件 ファイル 파일 ")
                .repeat(62)
                .into_bytes()
        };
        let name = |index: usize| alloc::format!("小-小さい-작은-{index:05}");
        let mut created = 0;
        let refusal = loop {
            assert!(created < 100_000, "metadata never ran out");
            let mut tx = writer.join().await.unwrap();
            let root = tx.root().await.unwrap();
            let file = match tx
                .create_file(
                    Name {
                        parent: &root,
                        bytes: name(created).as_bytes(),
                    },
                    running_attributes(),
                    RUNNING_WHEN,
                )
                .await
            {
                Ok(file) => file,
                Err(error) => {
                    // Refused before it changed anything: the name is free.
                    assert!(
                        tx.lookup(&root, name(created).as_bytes())
                            .await
                            .unwrap()
                            .is_none()
                    );
                    break error;
                }
            };
            match tx.write_file(&file, 0, &contents(created), options()).await {
                Ok(written) => assert_eq!(written, contents(created).len()),
                Err(error) => {
                    assert_eq!(tx.metadata(&file).await.unwrap().size, 0);
                    // The empty file the creation left is unlinked again, so
                    // every remaining name holds its contents.
                    tx.unlink_name(
                        Name {
                            parent: &root,
                            bytes: name(created).as_bytes(),
                        },
                        RUNNING_WHEN,
                    )
                    .await
                    .unwrap();
                    break error;
                }
            }
            created += 1;
        };
        assert!(
            matches!(
                refusal,
                WriteError::Space(SpaceError::Exhausted {
                    flags: METADATA,
                    ..
                })
            ),
            "{refusal:?}"
        );
        assert!(created > 10, "only {created} files fit");
        // The writer is healthy: what it accepted reads back and commits.
        let view = writer.snapshot().unwrap();
        assert_eq!(
            read_whole(&view, name(created - 1).as_bytes()).await,
            contents(created - 1)
        );
        writer.commit().await.unwrap();
        let device = Filesystem::mount(Disk(image.try_clone().unwrap()))
            .await
            .unwrap();
        for index in [0, created / 2, created - 1] {
            assert_eq!(
                read_whole(&device, name(index).as_bytes()).await,
                contents(index)
            );
        }
        let device_root = device.root().await.unwrap();
        assert!(
            device
                .lookup(&device_root, name(created).as_bytes())
                .await
                .unwrap()
                .is_none()
        );
        // Deleting still works on the full device, and commits.
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        for index in 0..created.min(8) {
            tx.unlink_name(
                Name {
                    parent: &root,
                    bytes: name(index).as_bytes(),
                },
                RUNNING_WHEN,
            )
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        let device = Filesystem::mount(Disk(image)).await.unwrap();
        let device_root = device.root().await.unwrap();
        assert!(
            device
                .lookup(&device_root, name(0).as_bytes())
                .await
                .unwrap()
                .is_none()
        );
        assert!(*writer.committed_allocation() == allocation_of(&device).await);
    });
}

/// The usage `statfs` reports follows the running transaction: a write is
/// counted once it is visible. The tree blocks it replaced stay counted
/// until the commit frees them, as Linux's pinned bytes do.
#[cfg(feature = "write")]
#[test]
fn usage_counts_the_running_transaction() {
    use super::writer::Name;

    block_on(async {
        let (_, image) = filesystem_image();
        let mut writer = mount_writer(Disk(image.try_clone().unwrap()))
            .await
            .unwrap();
        let before = writer.usage().unwrap();
        assert_eq!(
            before.used_bytes,
            writer.snapshot().unwrap().superblock().bytes_used()
        );
        const LENGTH: usize = 1 << 20;
        let mut tx = writer.join().await.unwrap();
        let root = tx.root().await.unwrap();
        let file = tx
            .create_file(
                Name {
                    parent: &root,
                    bytes: "用量-使用量-사용량".as_bytes(),
                },
                running_attributes(),
                RUNNING_WHEN,
            )
            .await
            .unwrap();
        tx.write_file(
            &file,
            0,
            &noise(LENGTH, 7),
            WriteOptions::plain(RUNNING_WHEN, NonZeroUsize::MIN),
        )
        .await
        .unwrap();
        let running = writer.usage().unwrap();
        assert_eq!(running.total_bytes, before.total_bytes);
        assert!(
            running.used_bytes >= before.used_bytes + LENGTH as u64,
            "{before:?} then {running:?}"
        );
        writer.commit().await.unwrap();
        let committed = writer.usage().unwrap();
        assert!(committed.used_bytes <= running.used_bytes);
        assert!(committed.used_bytes >= before.used_bytes + LENGTH as u64);
        let device = Filesystem::mount(Disk(image)).await.unwrap();
        assert_eq!(device.superblock().bytes_used(), committed.used_bytes);
    });
}

/// Names in ASCII, Chinese, Japanese and Korean.
#[cfg(feature = "write")]
const CJK_NAMES: [&str; 4] = ["plain", "中文文件", "日本語のファイル", "한국어파일"];

#[cfg(feature = "write")]
#[test]
fn chmod_and_chown_update_the_inode_and_refuse_set_id_on_files() {
    block_on(exercise_mode_and_owner(|_| {}));
}

#[cfg(feature = "write")]
#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn mode_and_owner_changes_pass_btrfs_check() {
    block_on(exercise_mode_and_owner(assert_btrfs_check));
}

#[cfg(feature = "write")]
async fn exercise_mode_and_owner(mut verify: impl FnMut(&std::path::Path)) {
    use super::writer::{Attributes, Name};
    use alloc::format;
    use zerocopy::IntoBytes;
    let stamp = Timestamp::from_secs(1_700_000_000);
    // Owned by root, as `Metadata::new` makes it.
    let image = |mode: u16| Metadata::new(mode, stamp);
    // Set-id bits as a Linux-made image carries them: the writer cannot
    // create these, so ferrosys lays them out.
    let mut tree = TreeBuilder::new().root(Metadata::new(0o755, stamp));
    for name in CJK_NAMES {
        tree = tree
            .file(format!("/suid-{name}"), "data", image(0o4755))
            .file(format!("/sgid-{name}"), "data", image(0o6755))
            .file(format!("/lock-{name}"), "data", image(0o2644))
            .directory(format!("/dir-{name}"), image(0o2775));
    }
    let named = tempfile::NamedTempFile::new().unwrap();
    drop(filesystem_image_tree_file(
        true,
        true,
        tree,
        named.reopen().unwrap(),
    ));
    verify(named.path());
    let mut writer = mount_writer(Disk(named.reopen().unwrap())).await.unwrap();
    let base = writer.snapshot().unwrap();
    let when = crate::Timestamp {
        seconds: 1_700_000_100,
        nanoseconds: 456,
    };
    let mut tx = writer.join().await.unwrap();
    let root = tx.root().await.unwrap();
    let tree = root.tree();
    macro_rules! at {
        ($name:expr) => {
            Name {
                parent: &root,
                bytes: $name.as_bytes(),
            }
        };
    }
    for name in CJK_NAMES {
        // An owner change removes the set-id bits of a file, whichever id
        // changes, and leaves the other id alone.
        let suid = tx.lookup(&root, format!("suid-{name}").as_bytes()).await;
        let suid = suid.unwrap().unwrap();
        let before = tx.edits().inode_item(tree, suid.number()).await.unwrap().0;
        tx.set_owner(&suid, Some(1001), None, when).await.unwrap();
        let after = tx.edits().inode_item(tree, suid.number()).await.unwrap().0;
        let metadata = tx.metadata(&suid).await.unwrap();
        assert_eq!(metadata.permissions, 0o755, "{name}");
        assert_eq!((metadata.uid, metadata.gid), (1001, 0), "{name}");
        assert_eq!(metadata.ctime, when, "{name}");
        assert_eq!(metadata.mtime, stamp_of(1_700_000_000), "{name}");
        assert_eq!(after.sequence.get(), before.sequence.get() + 1, "{name}");
        assert_eq!(after.transid.get(), tx.edits().trees.generation(), "{name}");
        assert_eq!(after.mode.get() & 0o170_000, 0o100_000, "{name}");

        let sgid = tx.lookup(&root, format!("sgid-{name}").as_bytes()).await;
        let sgid = sgid.unwrap().unwrap();
        tx.set_owner(&sgid, None, Some(2002), when).await.unwrap();
        let metadata = tx.metadata(&sgid).await.unwrap();
        assert_eq!(metadata.permissions, 0o755, "{name}");
        assert_eq!((metadata.uid, metadata.gid), (0, 2002), "{name}");

        // Without group execute S_ISGID marks mandatory locking on Linux;
        // chown still drops it for an unprivileged caller.
        let lock = tx.lookup(&root, format!("lock-{name}").as_bytes()).await;
        let lock = lock.unwrap().unwrap();
        tx.set_owner(&lock, Some(7), Some(8), when).await.unwrap();
        let metadata = tx.metadata(&lock).await.unwrap();
        assert_eq!(metadata.permissions, 0o644, "{name}");
        assert_eq!((metadata.uid, metadata.gid), (7, 8), "{name}");

        // S_ISGID on a directory is legitimate and survives a chown.
        let dir = tx.lookup(&root, format!("dir-{name}").as_bytes()).await;
        let dir = dir.unwrap().unwrap();
        tx.set_owner(&dir, Some(5), Some(6), when).await.unwrap();
        let metadata = tx.metadata(&dir).await.unwrap();
        assert_eq!(metadata.permissions, 0o2775, "{name}");
        assert_eq!((metadata.uid, metadata.gid), (5, 6), "{name}");
        tx.set_permissions(&dir, 0o3770, when).await.unwrap();
        assert_eq!(tx.metadata(&dir).await.unwrap().permissions, 0o3770);

        // chmod replaces the permission bits and keeps the file type; the
        // sticky bit is not a set-id bit.
        let file = tx
            .create_file(
                at!(name),
                Attributes {
                    permissions: 0o644,
                    uid: 1000,
                    gid: 1000,
                    umask: 0,
                },
                stamp_of(1_700_000_050),
            )
            .await
            .unwrap();
        let before = tx.edits().inode_item(tree, file.number()).await.unwrap().0;
        tx.set_permissions(&file, 0o1750, when).await.unwrap();
        let after = tx.edits().inode_item(tree, file.number()).await.unwrap().0;
        let metadata = tx.metadata(&file).await.unwrap();
        assert_eq!(metadata.kind, crate::FileKind::Regular, "{name}");
        assert_eq!(metadata.permissions, 0o1750, "{name}");
        assert_eq!((metadata.uid, metadata.gid), (1000, 1000), "{name}");
        assert_eq!(metadata.ctime, when, "{name}");
        assert_eq!(after.sequence.get(), before.sequence.get() + 1, "{name}");
        assert_eq!(after.transid.get(), tx.edits().trees.generation(), "{name}");

        // Both ids absent still changes the inode, as `fchownat(-1, -1)`.
        tx.set_permissions(&file, 0o600, stamp_of(1_700_000_060))
            .await
            .unwrap();
        tx.set_owner(&file, None, None, when).await.unwrap();
        let metadata = tx.metadata(&file).await.unwrap();
        assert_eq!(metadata.permissions, 0o600, "{name}");
        assert_eq!((metadata.uid, metadata.gid), (1000, 1000), "{name}");
        assert_eq!(metadata.ctime, when, "{name}");

        // The refusals leave the inode and the transaction as they were.
        let settled = tx.edits().inode_item(tree, file.number()).await.unwrap().0;
        for bits in [0o4755, 0o2755, 0o6755, 0o4000, 0o2000] {
            assert!(
                matches!(
                    tx.set_permissions(&file, bits, when).await,
                    Err(WriteError::SetId { permissions }) if permissions == bits
                ),
                "{name} {bits:#o}"
            );
        }
        assert!(matches!(
            tx.set_permissions(&file, 0o100_644, when).await,
            Err(WriteError::InvalidPermissions { .. })
        ));
        let unchanged = tx.edits().inode_item(tree, file.number()).await.unwrap().0;
        assert_eq!(settled.as_bytes(), unchanged.as_bytes(), "{name}");
        let setuid = |permissions| Attributes {
            permissions,
            uid: 0,
            gid: 0,
            umask: 0,
        };
        assert!(matches!(
            tx.create_file(at!(format!("new-{name}")), setuid(0o4755), when)
                .await,
            Err(WriteError::SetId { .. })
        ));
        assert!(matches!(
            tx.create_file(at!(format!("new-{name}")), setuid(0o2755), when)
                .await,
            Err(WriteError::SetId { .. })
        ));
        assert!(matches!(
            tx.symlink(at!(format!("new-{name}")), b"target", setuid(0o2777), when)
                .await,
            Err(WriteError::SetId { .. })
        ));
        assert!(
            tx.lookup(&root, format!("new-{name}").as_bytes())
                .await
                .unwrap()
                .is_none()
        );
        // A directory takes S_ISGID, and its S_ISUID means nothing.
        let directory = tx
            .create_directory(at!(format!("new-{name}")), setuid(0o2755), when)
            .await
            .unwrap();
        assert_eq!(tx.metadata(&directory).await.unwrap().permissions, 0o2755);
        tx.set_permissions(&directory, 0o4755, when).await.unwrap();
        assert_eq!(tx.metadata(&directory).await.unwrap().permissions, 0o4755);
    }
    tx.commit().await.unwrap();

    // The reader sees what the transaction stored.
    let current = writer.snapshot().unwrap();
    for name in CJK_NAMES {
        let read = async |path: String| *current.resolve(path.as_bytes()).await.unwrap().metadata();
        let suid = read(format!("/suid-{name}")).await;
        assert_eq!((suid.permissions, suid.uid, suid.gid), (0o755, 1001, 0));
        assert_eq!(suid.ctime, when);
        let sgid = read(format!("/sgid-{name}")).await;
        assert_eq!((sgid.permissions, sgid.uid, sgid.gid), (0o755, 0, 2002));
        let lock = read(format!("/lock-{name}")).await;
        assert_eq!((lock.permissions, lock.uid, lock.gid), (0o644, 7, 8));
        let dir = read(format!("/dir-{name}")).await;
        assert_eq!((dir.permissions, dir.uid, dir.gid), (0o3770, 5, 6));
        let file = read(format!("/{name}")).await;
        assert_eq!((file.permissions, file.uid, file.gid), (0o600, 1000, 1000));
        assert_eq!(file.kind, crate::FileKind::Regular);
    }
    // The base view still shows the image as it was made.
    let original = *base
        .resolve("/suid-plain".as_bytes())
        .await
        .unwrap()
        .metadata();
    assert_eq!((original.permissions, original.uid), (0o4755, 0));
    drop((base, current, writer));
    verify(named.path());
}

#[cfg(feature = "write")]
fn stamp_of(seconds: i64) -> crate::Timestamp {
    crate::Timestamp {
        seconds,
        nanoseconds: 0,
    }
}
