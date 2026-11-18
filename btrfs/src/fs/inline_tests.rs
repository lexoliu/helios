//! Inline file extents against the bounds `btrfs check` and Linux hold them
//! to (lexoliu/helios#1086): every path that stores a file inline (write,
//! append, truncate and reflink), uncompressed and compressed, at and across
//! each limit. The `#[ignore]`d test runs the same scenarios under
//! `btrfs check`, which needs btrfs-progs.

extern crate std;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use ferrosys::{Metadata, Timestamp, TreeBuilder};
use futures_lite::future::block_on;

use super::write_tests::{
    Disk, FixtureUuids, assert_btrfs_check, filesystem_image_tree_file, mount_writer,
};
use super::writer::{Attributes, CloneRange, InodeHandle, Name, WriteTransaction, Writer};
use crate::Compression;
use crate::compress::write::{Encoder, Plain, Zstd};
use crate::fs::{ExtentInfo, ExtentKind};
use crate::ondisk::{RawFileExtentHead, RawHeader, RawItem};
use crate::transaction::file::{DEFAULT_MAX_INLINE, WriteOptions};

const WHEN: crate::Timestamp = crate::Timestamp {
    seconds: 1_700_000_100,
    nanoseconds: 0,
};
const SECTOR: u64 = 4096;

/// How a file's data is expected to be stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// One inline item holding the whole file, compressed or not.
    Inline(Option<Compression>),
    /// No inline item at all.
    Regular,
}

/// What every file the scenarios made must hold.
#[derive(Default)]
struct Expected(BTreeMap<String, (Vec<u8>, Layout)>);

impl Expected {
    fn set(&mut self, name: &str, content: Vec<u8>, layout: Layout) {
        self.0.insert(name.into(), (content, layout));
    }
}

fn attributes() -> Attributes {
    Attributes {
        permissions: 0o644,
        uid: 1000,
        gid: 1000,
        umask: 0,
    }
}

fn concurrency() -> core::num::NonZeroUsize {
    std::thread::available_parallelism().unwrap()
}

fn plain(max_inline: u64) -> WriteOptions<Plain> {
    WriteOptions {
        max_inline,
        ..WriteOptions::plain(WHEN, concurrency())
    }
}

fn zstd(encoder: &mut Zstd, max_inline: u64) -> WriteOptions<&mut Zstd> {
    WriteOptions {
        when: WHEN,
        concurrency: concurrency(),
        encoder,
        max_inline,
    }
}

/// Text that compresses well: Chinese, Japanese and Korean.
fn text(length: usize) -> Vec<u8> {
    let mut bytes = "内联数据 インラインデータ 인라인 데이터 "
        .repeat(length)
        .into_bytes();
    bytes.truncate(length);
    bytes
}

/// Bytes no encoder shrinks.
fn noise(length: usize, seed: u32) -> Vec<u8> {
    let mut state = seed;
    (0..length)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

/// The length zstd at level 3 stores `content` in.
fn zstd_length(content: &[u8]) -> u64 {
    Zstd::new(3)
        .unwrap()
        .encode(content, SECTOR as usize)
        .unwrap()
        .expect("the content compresses")
        .bytes
        .len() as u64
}

async fn create<S: crate::source::BlockSink>(
    tx: &mut WriteTransaction<'_, S, FixtureUuids>,
    name: &str,
) -> InodeHandle {
    let root = tx.root().await.unwrap();
    tx.create_file(
        Name {
            parent: &root,
            bytes: name.as_bytes(),
        },
        attributes(),
        WHEN,
    )
    .await
    .unwrap()
}

async fn open<S: crate::source::BlockSink>(
    tx: &WriteTransaction<'_, S, FixtureUuids>,
    name: &str,
) -> InodeHandle {
    let root = tx.root().await.unwrap();
    tx.lookup(&root, name.as_bytes()).await.unwrap().unwrap()
}

/// btrfs-progs' bound on an inline item: `process_file_extent` in
/// `check/main.c` (`I_ERR_FILE_EXTENT_TOO_LARGE`, "inline file extent too
/// large") and `check_file_extent_inline` in `check/mode-lowmem.c`.
fn assert_progs_bound(name: &str, extent: &ExtentInfo, nodesize: u64) {
    let max_inline_data = nodesize
        - (size_of::<RawHeader>() + size_of::<RawItem>() + size_of::<RawFileExtentHead>()) as u64;
    let bound = (SECTOR - 1).min(max_inline_data);
    let within = match extent.compression {
        None => extent.len <= bound && extent.stored == extent.len,
        Some(_) => extent.stored <= bound && extent.len <= SECTOR,
    };
    assert!(within, "{name}: {extent:?} exceeds btrfs check's bound");
}

/// Reads every expected file back through a fresh view and checks its bytes
/// and its layout.
async fn verify(writer: &Writer<Disk, FixtureUuids>, expected: &Expected) {
    let view = writer.snapshot().unwrap();
    let nodesize = u64::from(view.superblock().nodesize());
    for (name, (content, layout)) in &expected.0 {
        let inode = view
            .resolve(alloc::format!("/{name}").as_bytes())
            .await
            .unwrap();
        assert_eq!(inode.metadata().size, content.len() as u64, "{name}");
        let mut read = vec![0; content.len()];
        assert_eq!(
            view.read(&inode, 0, &mut read).await.unwrap(),
            content.len(),
            "{name}"
        );
        assert!(read == *content, "{name}: contents differ");
        let extents = view.file_extents(&inode).await.unwrap();
        for extent in extents.iter().filter(|e| e.kind == ExtentKind::Inline) {
            assert_progs_bound(name, extent, nodesize);
        }
        match layout {
            Layout::Inline(compression) => {
                assert_eq!(extents.len(), 1, "{name}: {extents:?}");
                let extent = extents[0];
                assert_eq!(extent.kind, ExtentKind::Inline, "{name}: {extent:?}");
                assert_eq!(extent.len, content.len() as u64, "{name}: {extent:?}");
                assert_eq!(extent.compression, *compression, "{name}: {extent:?}");
            }
            Layout::Regular => assert!(
                extents.iter().all(|e| e.kind != ExtentKind::Inline),
                "{name}: {extents:?}"
            ),
        }
    }
}

/// Commits, checks every file, and hands the image to `check`.
async fn commit(
    writer: &mut Writer<Disk, FixtureUuids>,
    expected: &Expected,
    path: &std::path::Path,
    check: &mut impl FnMut(&std::path::Path),
) {
    writer.commit().await.unwrap();
    verify(writer, expected).await;
    check(path);
}

async fn exercise(mut check: impl FnMut(&std::path::Path)) {
    let named = tempfile::NamedTempFile::new().unwrap();
    let builder =
        TreeBuilder::new().root(Metadata::new(0o755, Timestamp::from_secs(1_700_000_000)));
    drop(filesystem_image_tree_file(
        true,
        true,
        builder,
        named.reopen().unwrap(),
    ));
    let mut writer = mount_writer(Disk(named.reopen().unwrap())).await.unwrap();
    let mut encoder = Zstd::new(3).unwrap();
    let mut expected = Expected::default();
    let compressed = Layout::Inline(Some(Compression::Zstd));

    // Writes. `max_inline` is Linux's default unless the name says otherwise.
    let mut tx = writer.join().await.unwrap();
    for (name, content, options, layout) in [
        // At and across `max_inline`.
        (
            "上限-上限-상한",
            noise(2048, 1),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "超过-超過-초과",
            noise(2049, 2),
            plain(2048),
            Layout::Regular,
        ),
        // A full sector never stays inline uncompressed: the bad item.
        (
            "一扇区-一セクタ-한섹터",
            noise(4096, 3),
            plain(2048),
            Layout::Regular,
        ),
        // An operator's larger `max_inline` still stops below a sector.
        (
            "扇区减一-セクタ未満-섹터미만",
            noise(4095, 4),
            plain(4096),
            Layout::Inline(None),
        ),
        (
            "整扇区-全セクタ-전섹터",
            noise(4096, 5),
            plain(4096),
            Layout::Regular,
        ),
        // `max_inline=0` stores nothing inline.
        ("禁止-禁止-금지", noise(10, 6), plain(0), Layout::Regular),
        // Written past its start, a small file is still one inline item.
        (
            "偏移-オフセット-오프셋",
            noise(100, 7),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "追加-追記-추가",
            noise(2000, 8),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "追加上限-追記上限-추가상한",
            noise(2000, 17),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "追加越界-追記越界-추가초과",
            noise(2000, 18),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "追加后方-追記後方-추가후방",
            noise(100, 19),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "缩小-縮小-축소",
            noise(2000, 9),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "伸长-伸長-신장",
            noise(100, 10),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "伸长超过-伸長超過-신장초과",
            noise(100, 11),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "伸长整扇-伸長全扇-신장전섹터",
            noise(100, 20),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "大伸长-大伸長-대신장",
            noise(100, 12),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "小的-小さい-작은",
            noise(100, 13),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "小的后方-小さい後方-작은후방",
            noise(100, 21),
            plain(2048),
            Layout::Inline(None),
        ),
        (
            "覆盖小-上書き小-덮어쓰기작은",
            noise(100, 22),
            plain(2048),
            Layout::Inline(None),
        ),
    ] {
        let file = create(&mut tx, name).await;
        let offset = if name.starts_with("偏移") { 40 } else { 0 };
        let data = &content[offset..];
        tx.write_file(&file, offset as u64, data, options)
            .await
            .unwrap();
        let mut stored = content;
        stored[..offset].fill(0);
        expected.set(name, stored, layout);
    }
    // Compressed: a compressible sector stays inline, its ram size at the
    // sector bound; data compression cannot shrink falls back to its plain
    // size, here past `max_inline`.
    for (name, content, layout) in [
        ("压缩-圧縮-압축", text(4096), compressed),
        (
            "压缩超扇-圧縮超扇-압축초과섹터",
            text(4097),
            Layout::Regular,
        ),
        ("压缩缩小-圧縮縮小-압축축소", text(4096), compressed),
        ("压缩重压-圧縮再圧縮-압축재압축", text(4096), compressed),
        ("压缩伸长-圧縮伸長-압축신장", text(2000), compressed),
        (
            "压缩伸长上限-圧縮伸長上限-압축신장상한",
            text(2000),
            compressed,
        ),
        (
            "压缩伸长超过-圧縮伸長超過-압축신장초과",
            text(2000),
            compressed,
        ),
        (
            "压缩伸长超扇-圧縮伸長超扇-압축신장초과섹터",
            text(2000),
            compressed,
        ),
        ("压缩追加-圧縮追記-압축추가", text(2000), compressed),
        (
            "压缩追加超过-圧縮追記超過-압축추가초과",
            text(2000),
            compressed,
        ),
        (
            "压缩追加乱码-圧縮追記乱数-압축추가난수",
            text(2000),
            compressed,
        ),
        (
            "压缩追加超扇-圧縮追記超扇-압축추가초과섹터",
            text(4096),
            compressed,
        ),
        ("乱码-乱数-난수", noise(3000, 14), Layout::Regular),
    ] {
        let file = create(&mut tx, name).await;
        tx.write_file(&file, 0, &content, zstd(&mut encoder, DEFAULT_MAX_INLINE))
            .await
            .unwrap();
        expected.set(name, content, layout);
    }
    // At and across `max_inline` by compressed length.
    let content = text(3000);
    let length = zstd_length(&content);
    for (name, max_inline, layout) in [
        ("压缩上限-圧縮上限-압축상한", length, compressed),
        ("压缩超过-圧縮超過-압축초과", length - 1, Layout::Regular),
    ] {
        let file = create(&mut tx, name).await;
        tx.write_file(&file, 0, &content, zstd(&mut encoder, max_inline))
            .await
            .unwrap();
        expected.set(name, content.clone(), layout);
    }
    // A file filling its sector is regular, whatever stores the next one.
    let file = create(&mut tx, "大的-大きい-큰").await;
    tx.write_file(&file, 0, &noise(3000, 15), plain(2048))
        .await
        .unwrap();
    expected.set("大的-大きい-큰", noise(3000, 15), Layout::Regular);
    commit(&mut writer, &expected, named.path(), &mut check).await;

    // Appends and truncation.
    let mut tx = writer.join().await.unwrap();
    let file = open(&tx, "追加-追記-추가").await;
    tx.write_file(&file, 2000, &noise(100, 16), plain(2048))
        .await
        .unwrap();
    let mut content = noise(2000, 8);
    content.extend_from_slice(&noise(100, 16));
    expected.set("追加-追記-추가", content, Layout::Regular);

    // Uncompressed append at `max_inline` stays inline; one more byte converts.
    let file = open(&tx, "追加上限-追記上限-추가상한").await;
    tx.write_file(&file, 2000, &noise(48, 23), plain(2048))
        .await
        .unwrap();
    let mut content = noise(2000, 17);
    content.extend_from_slice(&noise(48, 23));
    expected.set("追加上限-追記上限-추가상한", content, Layout::Inline(None));

    let file = open(&tx, "追加越界-追記越界-추가초과").await;
    tx.write_file(&file, 2000, &noise(48, 24), plain(2048))
        .await
        .unwrap();
    tx.write_file(&file, 2048, &noise(1, 25), plain(2048))
        .await
        .unwrap();
    let mut content = noise(2000, 18);
    content.extend_from_slice(&noise(48, 24));
    content.extend_from_slice(&noise(1, 25));
    expected.set("追加越界-追記越界-추가초과", content, Layout::Regular);

    // Writing past the first sector promotes an existing inline item.
    let file = open(&tx, "追加后方-追記後方-추가후방").await;
    tx.write_file(&file, SECTOR, &noise(100, 26), plain(2048))
        .await
        .unwrap();
    let mut content = noise(100, 19);
    content.resize(SECTOR as usize, 0);
    content.extend_from_slice(&noise(100, 26));
    expected.set("追加后方-追記後方-추가후방", content, Layout::Regular);

    // Compressed appends: within limit, across `max_inline`, incompressible, and past sector.
    let file = open(&tx, "压缩追加-圧縮追記-압축추가").await;
    let tail = text(2096);
    tx.write_file(&file, 2000, &tail, zstd(&mut encoder, DEFAULT_MAX_INLINE))
        .await
        .unwrap();
    let mut joined = text(2000);
    joined.extend_from_slice(&tail);
    expected.set("压缩追加-圧縮追記-압축추가", joined.clone(), compressed);

    let joined_len = zstd_length(&joined);
    let file = open(&tx, "压缩追加超过-圧縮追記超過-압축추가초과").await;
    tx.write_file(&file, 2000, &tail, zstd(&mut encoder, joined_len - 1))
        .await
        .unwrap();
    expected.set(
        "压缩追加超过-圧縮追記超過-압축추가초과",
        joined,
        Layout::Regular,
    );

    let file = open(&tx, "压缩追加乱码-圧縮追記乱数-압축추가난수").await;
    let random_tail = noise(2096, 27);
    tx.write_file(
        &file,
        2000,
        &random_tail,
        zstd(&mut encoder, DEFAULT_MAX_INLINE),
    )
    .await
    .unwrap();
    let mut content = text(2000);
    content.extend_from_slice(&random_tail);
    expected.set(
        "压缩追加乱码-圧縮追記乱数-압축추가난수",
        content,
        Layout::Regular,
    );

    let file = open(&tx, "压缩追加超扇-圧縮追記超扇-압축추가초과섹터").await;
    let one = text(1);
    tx.write_file(&file, SECTOR, &one, zstd(&mut encoder, DEFAULT_MAX_INLINE))
        .await
        .unwrap();
    let mut content = text(4096);
    content.extend_from_slice(&one);
    expected.set(
        "压缩追加超扇-圧縮追記超扇-압축추가초과섹터",
        content,
        Layout::Regular,
    );

    // Uncompressed inline shrinks in place.
    let file = open(&tx, "缩小-縮小-축소").await;
    tx.truncate_file(&file, 1000, plain(2048)).await.unwrap();
    expected.set("缩小-縮小-축소", noise(1000, 9), Layout::Inline(None));

    // Growth stores the zero-extended file afresh: at and across the limit,
    // at the sector boundary, and past the sector.
    for (name, seed, size, max_inline, layout) in [
        ("伸长-伸長-신장", 10, 2048, 2048, Layout::Inline(None)),
        (
            "伸长超过-伸長超過-신장초과",
            11,
            2049,
            2048,
            Layout::Regular,
        ),
        (
            "伸长整扇-伸長全扇-신장전섹터",
            20,
            SECTOR,
            SECTOR,
            Layout::Regular,
        ),
        ("大伸长-大伸長-대신장", 12, 8192, 2048, Layout::Regular),
    ] {
        let file = open(&tx, name).await;
        tx.truncate_file(&file, size, plain(max_inline))
            .await
            .unwrap();
        let mut content = noise(100, seed);
        content.resize(size as usize, 0);
        expected.set(name, content, layout);
    }

    // A compressed item cannot be cut: its first `size` bytes are stored
    // again, as plain bytes past the limit or compressed inline.
    let file = open(&tx, "压缩缩小-圧縮縮小-압축축소").await;
    tx.truncate_file(&file, 3000, plain(2048)).await.unwrap();
    expected.set("压缩缩小-圧縮縮小-압축축소", text(3000), Layout::Regular);
    let file = open(&tx, "压缩重压-圧縮再圧縮-압축재압축").await;
    tx.truncate_file(&file, 3000, zstd(&mut encoder, DEFAULT_MAX_INLINE))
        .await
        .unwrap();
    expected.set("压缩重压-圧縮再圧縮-압축재압축", text(3000), compressed);

    let mut padded_4096 = text(2000);
    padded_4096.resize(SECTOR as usize, 0);
    let padded_len = zstd_length(&padded_4096);
    for (name, size, max_inline, layout) in [
        (
            "压缩伸长-圧縮伸長-압축신장",
            SECTOR,
            DEFAULT_MAX_INLINE,
            compressed,
        ),
        (
            "压缩伸长上限-圧縮伸長上限-압축신장상한",
            SECTOR,
            padded_len,
            compressed,
        ),
        (
            "压缩伸长超过-圧縮伸長超過-압축신장초과",
            SECTOR,
            padded_len - 1,
            Layout::Regular,
        ),
        (
            "压缩伸长超扇-圧縮伸長超扇-압축신장초과섹터",
            SECTOR + 1,
            DEFAULT_MAX_INLINE,
            Layout::Regular,
        ),
    ] {
        let file = open(&tx, name).await;
        tx.truncate_file(&file, size, zstd(&mut encoder, max_inline))
            .await
            .unwrap();
        let mut content = text(2000);
        content.resize(size as usize, 0);
        expected.set(name, content, layout);
    }
    commit(&mut writer, &expected, named.path(), &mut check).await;

    // Reflink. An inline source lands as its own item at offset 0 of an
    // empty or smaller inline destination when `max_inline` allows it, and
    // otherwise its data is written through the normal path.
    let mut tx = writer.join().await.unwrap();
    let compressed_sector_len = zstd_length(&text(4096));
    for (source, destination, offset, max_inline, layout) in [
        (
            "上限-上限-상한",
            "复制-複製-복제",
            0,
            2048,
            Layout::Inline(None),
        ),
        (
            "上限-上限-상한",
            "复制超过-複製超過-복제초과",
            0,
            2047,
            Layout::Regular,
        ),
        (
            "压缩-圧縮-압축",
            "压缩复制-圧縮複製-압축복제",
            0,
            2048,
            compressed,
        ),
        (
            "压缩-圧縮-압축",
            "压缩复制上限-圧縮複製上限-압축복제상한",
            0,
            compressed_sector_len,
            compressed,
        ),
        (
            "压缩-圧縮-압축",
            "压缩复制超过-圧縮複製超過-압축복제초과",
            0,
            compressed_sector_len - 1,
            Layout::Regular,
        ),
        (
            "上限-上限-상한",
            "后方复制-後方複製-후방복제",
            SECTOR,
            2048,
            Layout::Regular,
        ),
    ] {
        let from = open(&tx, source).await;
        let to = create(&mut tx, destination).await;
        let length = expected.0[source].0.len() as u64;
        tx.clone_range(
            CloneRange {
                source: &from,
                destination: &to,
                source_offset: 0,
                destination_offset: offset,
                length,
            },
            plain(max_inline),
        )
        .await
        .unwrap();
        let mut content = vec![0; offset as usize];
        content.extend_from_slice(&expected.0[source].0.clone());
        expected.set(destination, content, layout);
    }
    // Inline source replacing a smaller inline destination stays inline.
    let from = open(&tx, "上限-上限-상한").await;
    let to = open(&tx, "覆盖小-上書き小-덮어쓰기작은").await;
    tx.clone_range(
        CloneRange {
            source: &from,
            destination: &to,
            source_offset: 0,
            destination_offset: 0,
            length: 2048,
        },
        plain(2048),
    )
    .await
    .unwrap();
    expected.set(
        "覆盖小-上書き小-덮어쓰기작은",
        expected.0["上限-上限-상한"].0.clone(),
        Layout::Inline(None),
    );

    // Shared extents replace a destination's inline data, which the clone
    // keeps as a regular extent where they do not cover it.
    let from = open(&tx, "大的-大きい-큰").await;
    let to = open(&tx, "小的-小さい-작은").await;
    tx.clone_range(
        CloneRange {
            source: &from,
            destination: &to,
            source_offset: 0,
            destination_offset: 0,
            length: 3000,
        },
        plain(2048),
    )
    .await
    .unwrap();
    expected.set("小的-小さい-작은", noise(3000, 15), Layout::Regular);

    let to = open(&tx, "小的后方-小さい後方-작은후방").await;
    tx.clone_range(
        CloneRange {
            source: &from,
            destination: &to,
            source_offset: 0,
            destination_offset: SECTOR,
            length: 3000,
        },
        plain(2048),
    )
    .await
    .unwrap();
    let mut content = noise(100, 21);
    content.resize(SECTOR as usize, 0);
    content.extend_from_slice(&noise(3000, 15));
    expected.set("小的后方-小さい後方-작은후방", content, Layout::Regular);
    commit(&mut writer, &expected, named.path(), &mut check).await;
}

#[test]
fn inline_extents_follow_linux_limits() {
    block_on(exercise(|_| {}));
}

#[test]
#[ignore = "needs btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn inline_extents_pass_btrfs_check() {
    block_on(exercise(assert_btrfs_check));
}
