//! `RENAME_NOREPLACE` and `RENAME_EXCHANGE` against a real image, read back
//! through the reader. The `#[ignore]`d test runs the same scenarios under
//! `btrfs check`, which needs btrfs-progs.

extern crate std;

use alloc::vec;
use alloc::vec::Vec;

use ferrosys::{Metadata, Timestamp, TreeBuilder};
use futures_lite::future::block_on;
use helios_hal::fs::RenameMode::{Exchange, NoReplace, Replace};

use super::write_tests::{
    Disk, FixtureUuids, assert_btrfs_check, filesystem_image_tree_file, mount_writer,
};
use super::writer::Writer;
use crate::source::BlockSink;
use crate::transaction::Transaction;
use crate::transaction::file::WriteOptions;
use crate::transaction::namespace::{Attributes, Name};
use crate::tree::write::WriteError;
use crate::{FileKind, Timestamp as When};

const WHEN: When = When {
    seconds: 1_700_000_100,
    nanoseconds: 123,
};

/// A freshly formatted image with a writer mounted on it.
struct Image {
    named: tempfile::NamedTempFile,
    writer: Writer<Disk, FixtureUuids>,
    tree: u64,
    root: u64,
}

fn name(tree: u64, parent: u64, bytes: &[u8]) -> Name<'_> {
    Name {
        tree,
        parent,
        bytes,
    }
}

/// Creates a directory, or a regular file holding `content`.
async fn create(
    edits: &mut Transaction<impl BlockSink>,
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
            name(tree, parent, entry.as_bytes()),
            Attributes {
                mode,
                uid: 1000,
                gid: 1000,
                umask: 0,
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

/// Commits the running transaction, then lets `verify` inspect the image at
/// `path`.
async fn commit(
    writer: &mut Writer<Disk, FixtureUuids>,
    path: &std::path::Path,
    verify: &mut impl FnMut(&std::path::Path),
) {
    writer.commit().await.unwrap();
    verify(path);
}

impl Image {
    async fn new() -> Self {
        let timestamp = Timestamp::from_secs(1_700_000_000);
        let named = tempfile::NamedTempFile::new().unwrap();
        let builder = TreeBuilder::new().root(Metadata::new(0o755, timestamp));
        drop(filesystem_image_tree_file(
            true,
            true,
            builder,
            named.reopen().unwrap(),
        ));
        let writer = mount_writer(Disk(named.reopen().unwrap())).await.unwrap();
        let root = writer.snapshot().unwrap().root().await.unwrap();
        Self {
            named,
            writer,
            tree: root.tree(),
            root: root.number(),
        }
    }

    /// The inode `path` names and its metadata, read through the reader.
    async fn stat(&self, path: &str) -> (u64, crate::Metadata) {
        let inode = self
            .writer
            .snapshot()
            .unwrap()
            .resolve(path.as_bytes())
            .await
            .unwrap();
        (inode.number(), *inode.metadata())
    }

    async fn gone(&self, path: &str) -> bool {
        self.writer
            .snapshot()
            .unwrap()
            .resolve(path.as_bytes())
            .await
            .is_err()
    }

    async fn read(&self, path: &str) -> Vec<u8> {
        let snapshot = self.writer.snapshot().unwrap();
        let inode = snapshot.resolve(path.as_bytes()).await.unwrap();
        let mut output = vec![0; inode.metadata().size as usize];
        snapshot.read(&inode, 0, &mut output).await.unwrap();
        output
    }
}

async fn exercise_noreplace(mut verify: impl FnMut(&std::path::Path)) {
    let mut image = Image::new().await;
    let (tree, root) = (image.tree, image.root);
    let mut tx = image.writer.join().await.unwrap();
    let edits = tx.edits();
    let source = create(edits, tree, root, "source", Some(b"moved")).await;
    create(edits, tree, root, "taken", Some(b"kept")).await;
    create(edits, tree, root, "empty", None).await;
    create(edits, tree, root, "other", None).await;
    // Any name at the destination is refused, whatever it is and whatever
    // the source is, and nothing changes.
    for (from, to) in [
        ("source", "taken"),
        ("source", "empty"),
        ("empty", "other"),
        ("empty", "source"),
        ("source", "source"),
    ] {
        let result = edits
            .rename(
                name(tree, root, from.as_bytes()),
                name(tree, root, to.as_bytes()),
                NoReplace,
                WHEN,
            )
            .await;
        assert!(
            matches!(result, Err(WriteError::Exists { .. })),
            "{from} over {to}"
        );
    }
    // A missing source is reported before a taken destination.
    for to in ["free", "taken"] {
        let result = edits
            .rename(
                name(tree, root, b"missing"),
                name(tree, root, to.as_bytes()),
                NoReplace,
                WHEN,
            )
            .await;
        assert!(matches!(result, Err(WriteError::NoEntry { .. })), "{to}");
    }
    edits
        .rename(
            name(tree, root, b"source"),
            name(tree, root, b"free"),
            NoReplace,
            WHEN,
        )
        .await
        .unwrap();
    // The default mode still replaces.
    edits
        .rename(
            name(tree, root, b"free"),
            name(tree, root, b"taken"),
            Replace,
            WHEN,
        )
        .await
        .unwrap();
    commit(&mut image.writer, image.named.path(), &mut verify).await;
    assert!(image.gone("/free").await);
    assert!(image.gone("/source").await);
    let (number, metadata) = image.stat("/taken").await;
    assert_eq!((number, metadata.nlink), (source, 1));
    assert_eq!(image.read("/taken").await, b"moved");
}

#[test]
fn rename_noreplace_refuses_an_existing_destination_with_eexist() {
    block_on(exercise_noreplace(|_| {}));
}

async fn exercise_exchange_file_and_directory(mut verify: impl FnMut(&std::path::Path)) {
    let mut image = Image::new().await;
    let (tree, root) = (image.tree, image.root);
    let mut tx = image.writer.join().await.unwrap();
    let edits = tx.edits();
    let first = create(edits, tree, root, "甲", None).await;
    let second = create(edits, tree, root, "乙", None).await;
    let directory = create(edits, tree, first, "目录", None).await;
    create(edits, tree, directory, "内部", Some(b"inside")).await;
    let file = create(edits, tree, second, "文件名更长", Some(&[0x5a; 9000])).await;
    commit(&mut image.writer, image.named.path(), &mut verify).await;
    let before_first = image.stat("/甲").await.1;
    let before_second = image.stat("/乙").await.1;

    let mut tx = image.writer.join().await.unwrap();
    tx.edits()
        .rename(
            name(tree, first, "目录".as_bytes()),
            name(tree, second, "文件名更长".as_bytes()),
            Exchange,
            WHEN,
        )
        .await
        .unwrap();
    commit(&mut image.writer, image.named.path(), &mut verify).await;

    // Each name now shows the other object, and the objects are intact.
    let (number, metadata) = image.stat("/甲/目录").await;
    assert_eq!(
        (number, metadata.kind, metadata.nlink),
        (file, FileKind::Regular, 1)
    );
    assert_eq!(image.read("/甲/目录").await, [0x5a; 9000]);
    let (number, metadata) = image.stat("/乙/文件名更长").await;
    assert_eq!((number, metadata.kind), (directory, FileKind::Directory));
    assert_eq!(image.read("/乙/文件名更长/内部").await, b"inside");
    // Names stay where they were, so no directory size or link count moves,
    // and both parents and both objects record the change.
    let (_, after_first) = image.stat("/甲").await;
    let (_, after_second) = image.stat("/乙").await;
    assert_eq!(after_first.size, before_first.size);
    assert_eq!(after_second.size, before_second.size);
    for parent in [after_first, after_second] {
        assert_eq!((parent.ctime, parent.mtime), (WHEN, WHEN));
    }
    assert_eq!(image.stat("/甲/目录").await.1.ctime, WHEN);
    assert_eq!(image.stat("/乙/文件名更长").await.1.ctime, WHEN);

    let mut tx = image.writer.join().await.unwrap();
    let edits = tx.edits();
    // The moved directory's parent reference follows it: the directory
    // that now holds it cannot go beneath it.
    let result = edits
        .rename(
            name(tree, root, "乙".as_bytes()),
            name(tree, directory, b"loop"),
            NoReplace,
            WHEN,
        )
        .await;
    assert!(matches!(result, Err(WriteError::DirectoryCycle)));
    // The reverse exchange restores the original layout.
    edits
        .rename(
            name(tree, first, "目录".as_bytes()),
            name(tree, second, "文件名更长".as_bytes()),
            Exchange,
            WHEN,
        )
        .await
        .unwrap();
    commit(&mut image.writer, image.named.path(), &mut verify).await;
    assert_eq!(image.stat("/甲/目录").await.0, directory);
    assert_eq!(image.stat("/乙/文件名更长").await.0, file);
}

#[test]
fn 交换_文件与目录跨父目录互换且读回一致() {
    block_on(exercise_exchange_file_and_directory(|_| {}));
}

async fn exercise_exchange_directories(mut verify: impl FnMut(&std::path::Path)) {
    let mut image = Image::new().await;
    let (tree, root) = (image.tree, image.root);
    let mut tx = image.writer.join().await.unwrap();
    let edits = tx.edits();
    let left = create(edits, tree, root, "左", None).await;
    let right = create(edits, tree, root, "右", None).await;
    let left_directory = create(edits, tree, left, "ディレクトリ", None).await;
    let right_directory = create(edits, tree, right, "디렉터리", None).await;
    create(edits, tree, left_directory, "左の中身", Some(b"left")).await;
    create(edits, tree, right_directory, "오른쪽", Some(b"right")).await;
    // A pair of different name lengths within one parent.
    let short = create(edits, tree, left, "a", None).await;
    let long = create(edits, tree, left, "bbbbbbbb", Some(b"b")).await;
    commit(&mut image.writer, image.named.path(), &mut verify).await;
    let before = image.stat("/左").await.1;

    let mut tx = image.writer.join().await.unwrap();
    let edits = tx.edits();
    edits
        .rename(
            name(tree, left, "ディレクトリ".as_bytes()),
            name(tree, right, "디렉터리".as_bytes()),
            Exchange,
            WHEN,
        )
        .await
        .unwrap();
    edits
        .rename(
            name(tree, left, b"a"),
            name(tree, left, b"bbbbbbbb"),
            Exchange,
            WHEN,
        )
        .await
        .unwrap();
    commit(&mut image.writer, image.named.path(), &mut verify).await;
    assert_eq!(image.stat("/左/ディレクトリ").await.0, right_directory);
    assert_eq!(image.stat("/右/디렉터리").await.0, left_directory);
    assert_eq!(image.read("/左/ディレクトリ/오른쪽").await, b"right");
    assert_eq!(image.read("/右/디렉터리/左の中身").await, b"left");
    assert_eq!(image.stat("/左/a").await.0, long);
    assert_eq!(image.stat("/左/bbbbbbbb").await.0, short);
    assert_eq!(image.stat("/左").await.1.size, before.size);

    // Both moved directories answer to their new parents: each parent now
    // lies above the directory that came from the other.
    let mut tx = image.writer.join().await.unwrap();
    let edits = tx.edits();
    for (moved_into, parent_name, held) in
        [(left_directory, "右", right), (right_directory, "左", left)]
    {
        let result = edits
            .rename(
                name(tree, root, parent_name.as_bytes()),
                name(tree, moved_into, b"loop"),
                NoReplace,
                WHEN,
            )
            .await;
        assert!(
            matches!(result, Err(WriteError::DirectoryCycle)),
            "{parent_name} {held}"
        );
    }
}

#[test]
fn 디렉터리_교환은_서로_다른_부모_사이에서_원자적이다() {
    block_on(exercise_exchange_directories(|_| {}));
}

#[test]
fn 存在しない名前との交換はenoentになりinvalidな交換はeinvalになる() {
    block_on(async {
        let mut image = Image::new().await;
        let (tree, root) = (image.tree, image.root);
        let mut tx = image.writer.join().await.unwrap();
        let edits = tx.edits();
        let outer = create(edits, tree, root, "outer", None).await;
        let inner = create(edits, tree, outer, "inner", None).await;
        create(edits, tree, root, "file", Some(b"x")).await;
        // ENOENT: the destination, the source, and both missing.
        for (from, to) in [
            ("file", "nothing"),
            ("nothing", "file"),
            ("nothing", "never"),
        ] {
            let result = edits
                .rename(
                    name(tree, root, from.as_bytes()),
                    name(tree, root, to.as_bytes()),
                    Exchange,
                    WHEN,
                )
                .await;
            assert!(
                matches!(result, Err(WriteError::NoEntry { .. })),
                "{from} with {to}"
            );
        }
        // EINVAL: a directory exchanged with one of its descendants, in
        // either order.
        let result = edits
            .rename(
                name(tree, root, b"outer"),
                name(tree, outer, b"inner"),
                Exchange,
                WHEN,
            )
            .await;
        assert!(matches!(result, Err(WriteError::DirectoryCycle)));
        let result = edits
            .rename(
                name(tree, outer, b"inner"),
                name(tree, root, b"outer"),
                Exchange,
                WHEN,
            )
            .await;
        assert!(matches!(result, Err(WriteError::DirectoryCycle)));
        // A name exchanged with itself changes nothing.
        edits
            .rename(
                name(tree, root, b"file"),
                name(tree, root, b"file"),
                Exchange,
                WHEN,
            )
            .await
            .unwrap();
        commit(&mut image.writer, image.named.path(), &mut |_| {}).await;
        assert_eq!(image.stat("/outer/inner").await.0, inner);
        assert_eq!(image.read("/file").await, b"x");
    });
}

#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn rename_modes_pass_btrfs_check() {
    block_on(async {
        exercise_noreplace(assert_btrfs_check).await;
        exercise_exchange_file_and_directory(assert_btrfs_check).await;
        exercise_exchange_directories(assert_btrfs_check).await;
    });
}
