//! EROFS images mounted over the block cache, and corrupted copies of
//! them.
//!
//! Every image is built from the same tree ([`populate`]). The builder in
//! `erofs-rs` writes the images that run on every host. `mkfs.erofs`
//! from erofs-utils writes the ones in [`mkfs`], which the Linux CI
//! runner installs and the macOS development host does not have, so they
//! are ignored by default and the CI lane runs them with
//! `--run-ignored`; they fail, never skip, when the tool is missing.

extern crate std;

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use erofs_rs::build::{Compression, Options};
use futures_lite::future::block_on;
use helios_hal::fs::{
    BlockSerial, DeviceId, DirCookie, FileSystem, FsError, FsPath, IoError, NodeKind, OpenFlags,
    WritePosition,
};

use super::{ErofsError, ErofsFs};
use crate::io::{MemoryDisk, cache_over};
use crate::vfs::{AttachedStorage, BOOT_DEVICE, MountSource, MountTable, Namespace, Storage};
use crate::{
    BlockCacheError, BootFs, EmbeddedBootDirectory, EmbeddedBootFs, MountRequestError,
    ProcessAuthority, UnsupportedHostFileSystem, request_mount,
};

const SERIAL: &str = "erofs-test";
const HELLO: &[u8] = b"hello from erofs\n";
/// Names and contents in Chinese, Japanese and Korean.
const CJK_FILES: [(&str, &str); 3] = [
    ("中文.txt", "你好，世界\n"),
    ("日本語.txt", "こんにちは世界\n"),
    ("한국어.txt", "안녕하세요 세계\n"),
];
/// Large enough to span many 4 KiB blocks and several compressed
/// clusters.
const BIG_LEN: usize = 300_000;
/// Enough entries that the directory spans several blocks, so a listing
/// pages across block boundaries.
const MANY_FILES: usize = 600;
/// A listing page short enough that the directory takes many of them.
const PAGE: usize = 64;
/// Where the superblock sits, and the bytes after it the tests touch
/// (`struct erofs_super_block` in Linux's `fs/erofs/erofs_fs.h`).
const SUPERBLOCK: usize = 1024;
const FEATURE_COMPAT: usize = SUPERBLOCK + 8;
const VOLUME_NAME: usize = SUPERBLOCK + 64;
/// `EROFS_FEATURE_COMPAT_SB_CHKSUM`.
const SB_CHECKSUM: u32 = 1;

fn big() -> Vec<u8> {
    (0..BIG_LEN)
        .map(|index| (index % 251) as u8 ^ (index / 4096) as u8)
        .collect()
}

fn many_name(index: usize) -> String {
    format!("entry-{index:04}")
}

/// What a `many/` file holds: nothing that spells an entry's name, so a
/// search of the image for a name finds the directory.
fn many_content(index: usize) -> String {
    format!("file number {index}\n")
}

/// Fills `root` with the tree every image here is built from.
fn populate(root: &Path) {
    fs::write(root.join("hello.txt"), HELLO).unwrap();
    fs::write(root.join("big.bin"), big()).unwrap();
    fs::create_dir(root.join("docs")).unwrap();
    for (name, content) in CJK_FILES {
        fs::write(root.join("docs").join(name), content).unwrap();
    }
    symlink("../hello.txt", root.join("docs/hello")).unwrap();
    fs::create_dir(root.join("many")).unwrap();
    for index in 0..MANY_FILES {
        fs::write(
            root.join("many").join(many_name(index)),
            many_content(index),
        )
        .unwrap();
    }
}

/// An image of the tree from the `erofs-rs` builder.
fn built(compression: Compression) -> Vec<u8> {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("root");
    fs::create_dir(&root).unwrap();
    populate(&root);
    let image = scratch.path().join("image.erofs");
    Options::default()
        .compression(compression)
        .build_from_directory(&root, &image)
        .unwrap();
    fs::read(image).unwrap()
}

fn mount_on(disk: &MemoryDisk) -> Result<ErofsFs<MemoryDisk>, ErofsError> {
    let cache = cache_over(disk, 256, 1);
    block_on(ErofsFs::mount(cache.disk(0), DeviceId::new(0x5e)))
}

fn mount(image: &[u8]) -> Result<ErofsFs<MemoryDisk>, ErofsError> {
    mount_on(&MemoryDisk::holding(SERIAL, image, false))
}

fn path(path: &str) -> FsPath<'_> {
    FsPath::new(path).expect("a valid filesystem path")
}

fn read_all(fs: &ErofsFs<MemoryDisk>, file: &str) -> FsResultBytes {
    let mut contents = Vec::new();
    loop {
        let chunk = block_on(fs.read_at(path(file), contents.len() as u64, 65_536))?;
        if chunk.is_empty() {
            return Ok(contents);
        }
        contents.extend_from_slice(&chunk);
    }
}

type FsResultBytes = Result<Vec<u8>, FsError>;

/// Every entry of the directory at `dir`, a page of [`PAGE`] at a time,
/// each page resuming at the cookie the last entry of the one before it
/// returned.
fn list(fs: &ErofsFs<MemoryDisk>, dir: &str) -> Result<Vec<(String, NodeKind)>, FsError> {
    let mut entries = Vec::new();
    let mut cookie = DirCookie::START;
    loop {
        let page = block_on(fs.read_dir(path(dir), cookie, PAGE))?;
        let complete = page.len() < PAGE;
        for entry in &page {
            assert_ne!(entry.inode, 0, "{} carries its inode", entry.name);
            entries.push((entry.name.clone(), entry.kind));
        }
        match page.last() {
            Some(last) if !complete => cookie = last.next,
            _ => return Ok(entries),
        }
    }
}

/// What every image of the tree answers, whoever wrote it.
fn assert_tree(fs: &ErofsFs<MemoryDisk>) {
    let root = block_on(fs.stat(FsPath::ROOT)).unwrap();
    assert_eq!(root.kind, NodeKind::Directory);
    assert_eq!(root.id.device, DeviceId::new(0x5e));

    assert_eq!(read_all(fs, "hello.txt").unwrap(), HELLO);
    let hello = block_on(fs.stat(path("hello.txt"))).unwrap();
    assert_eq!(
        (hello.kind, hello.size),
        (NodeKind::File, HELLO.len() as u64)
    );

    let expected = big();
    assert_eq!(read_all(fs, "big.bin").unwrap(), expected);
    let tail = block_on(fs.read_at(path("big.bin"), BIG_LEN as u64 - 10, 4096)).unwrap();
    assert_eq!(&tail[..], &expected[BIG_LEN - 10..]);
    assert!(
        block_on(fs.read_at(path("big.bin"), BIG_LEN as u64, 4096))
            .unwrap()
            .is_empty()
    );

    let mut docs = list(fs, "docs").unwrap();
    docs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected_docs: Vec<(String, NodeKind)> = CJK_FILES
        .iter()
        .map(|(name, _)| (String::from(*name), NodeKind::File))
        .chain([(String::from("hello"), NodeKind::Symlink)])
        .collect();
    expected_docs.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(docs, expected_docs);
    for (name, content) in CJK_FILES {
        assert_eq!(
            read_all(fs, &format!("docs/{name}")).unwrap(),
            content.as_bytes()
        );
    }
    assert_eq!(
        block_on(fs.read_link(path("docs/hello"))).unwrap(),
        "../hello.txt"
    );
    assert_eq!(
        block_on(fs.stat(path("docs/hello"))).unwrap().kind,
        NodeKind::Symlink
    );

    let many = list(fs, "many").unwrap();
    assert_eq!(many.len(), MANY_FILES, "every entry is listed exactly once");
    let names: BTreeSet<String> = many.iter().map(|(name, _)| name.clone()).collect();
    let expected_names: BTreeSet<String> = (0..MANY_FILES).map(many_name).collect();
    assert_eq!(names, expected_names);
    assert!(many.iter().all(|(_, kind)| *kind == NodeKind::File));
    assert_eq!(
        read_all(fs, &format!("many/{}", many_name(417))).unwrap(),
        many_content(417).as_bytes()
    );

    let root_names: BTreeSet<String> = list(fs, "").unwrap().into_iter().map(|e| e.0).collect();
    assert_eq!(
        root_names,
        ["big.bin", "docs", "hello.txt", "many"]
            .into_iter()
            .map(String::from)
            .collect()
    );
}

/// What a read-only image refuses, and what a missing or misused path
/// answers.
fn assert_refusals(fs: &ErofsFs<MemoryDisk>) {
    assert_eq!(block_on(fs.stat(path("absent"))), Err(FsError::NoEntry));
    assert_eq!(
        block_on(fs.stat(path("hello.txt/below"))),
        Err(FsError::NotDirectory)
    );
    assert_eq!(
        block_on(fs.read_at(path("docs"), 0, 16)),
        Err(FsError::IsDirectory)
    );
    assert_eq!(
        block_on(fs.read_dir(path("hello.txt"), DirCookie::START, 8)),
        Err(FsError::NotDirectory)
    );
    assert_eq!(
        block_on(fs.open(path("new.txt"), OpenFlags::CREATE, 0)),
        Err(FsError::ReadOnly)
    );
    assert_eq!(
        block_on(fs.open(path("hello.txt"), OpenFlags::TRUNCATE, 0)),
        Err(FsError::ReadOnly)
    );
    assert_eq!(
        block_on(fs.open(path("hello.txt"), OpenFlags::DIRECTORY, 0)),
        Err(FsError::NotDirectory)
    );
    assert_eq!(
        block_on(crate::vfs::write_bytes(
            fs,
            path("hello.txt"),
            WritePosition::End,
            b"more",
            0
        )),
        Err(FsError::ReadOnly)
    );
    assert_eq!(
        block_on(fs.unlink(path("hello.txt"))),
        Err(FsError::ReadOnly)
    );
    // `docs/` holds six entries, `.` and `..` among them, in one block:
    // index 7 of that block is past its last entry, so no listing of the
    // directory handed that cookie out.
    assert_eq!(
        block_on(fs.read_dir(path("docs"), DirCookie::new(7), 8)),
        Err(FsError::Invalid)
    );
}

/// The byte offset of the first `needle` at or after `from`.
fn find(image: &[u8], needle: &[u8], from: usize) -> usize {
    image[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| from + at)
        .unwrap_or_else(|| panic!("the image holds {:?}", String::from_utf8_lossy(needle)))
}

fn has_superblock_checksum(image: &[u8]) -> bool {
    let compat = u32::from_le_bytes(
        image[FEATURE_COMPAT..FEATURE_COMPAT + 4]
            .try_into()
            .unwrap(),
    );
    compat & SB_CHECKSUM != 0
}

/// What every corrupted copy of `image` fails with.
fn assert_corruptions_fail(image: &[u8]) {
    assert!(
        has_superblock_checksum(image),
        "the image carries a superblock checksum"
    );

    // A byte the checksum covers but no structure reads.
    let mut renamed = image.to_vec();
    renamed[VOLUME_NAME] ^= 0x5a;
    match mount(&renamed) {
        Err(ErofsError::Checksum { stored, computed }) => assert_ne!(stored, computed),
        Err(other) => panic!("a changed volume name fails the checksum, not {other}"),
        Ok(_) => panic!("a changed volume name fails the checksum"),
    }

    let mut unmagic = image.to_vec();
    unmagic[SUPERBLOCK..SUPERBLOCK + 4].fill(0);
    assert!(matches!(mount(&unmagic), Err(ErofsError::Format(_))));

    // One name in the middle of `many/`, past the block the checksum
    // covers, made to sort after its neighbours: the mount succeeds and
    // every read that reaches that directory block fails.
    let mut unsorted = image.to_vec();
    let name = many_name(MANY_FILES / 2);
    let at = find(&unsorted, name.as_bytes(), 4096);
    unsorted[at..at + name.len()].copy_from_slice(b"entry-9999");
    let fs = mount(&unsorted).expect("the superblock is intact");
    assert_eq!(list(&fs, "many"), Err(FsError::Io));
    assert_eq!(
        block_on(fs.stat(path(&format!("many/{name}")))),
        Err(FsError::Io)
    );
    assert_eq!(read_all(&fs, "hello.txt").unwrap(), HELLO);

    // An image cut short of its data: the superblock is whole, and the
    // first read past the end fails.
    let truncated = &image[..8192];
    let fs = mount(truncated).expect("the superblock is intact");
    assert_eq!(read_all(&fs, "big.bin"), Err(FsError::Io));

    match mount_on(&MemoryDisk::holding(SERIAL, image, true)) {
        Err(ErofsError::Device(BlockCacheError::Device(IoError::DeviceFault))) => {}
        Err(other) => panic!("a disk that cannot be read fails with its own error, not {other}"),
        Ok(_) => panic!("a disk that cannot be read fails the mount"),
    }
}

#[test]
fn builder_images_mount_and_read_their_tree() {
    for compression in [Compression::None, Compression::Lz4, Compression::Deflate] {
        let fs = mount(&built(compression)).expect("the image mounts");
        assert_tree(&fs);
        assert_refusals(&fs);
    }
}

#[test]
fn corrupted_builder_images_fail_with_typed_errors() {
    assert_corruptions_fail(&built(Compression::None));
}

/// The storage of a machine whose one disk is a [`MemoryDisk`].
#[derive(Clone, Copy)]
struct MemoryStorage;

impl Storage for MemoryStorage {
    type HostShare = UnsupportedHostFileSystem;
    type Disk = MemoryDisk;
}

const BOOT_IMAGE: EmbeddedBootFs =
    EmbeddedBootFs::new(&[EmbeddedBootDirectory::new("mnt", 0)], &[]);

fn request(
    namespace: &Namespace<MemoryStorage>,
    disk: &MemoryDisk,
    serial: &str,
) -> Result<(), MountRequestError> {
    block_on(request_mount(
        &ProcessAuthority::root(),
        namespace,
        AttachedStorage {
            host_share: None,
            disks: Some(cache_over(disk, 256, 1)),
        },
        MountSource::Disk {
            serial: BlockSerial::new(serial.as_bytes()).unwrap(),
        },
        "/mnt",
        false,
    ))
}

#[test]
fn a_disk_mount_request_attaches_the_image_read_only() {
    let image = built(Compression::Lz4);
    let namespace = Namespace::new(MountTable::new(BootFs::new(BOOT_IMAGE, BOOT_DEVICE), 1));
    let disk = MemoryDisk::holding(SERIAL, &image, false);

    assert!(matches!(
        request(&namespace, &disk, "other-disk"),
        Err(MountRequestError::DiskUnavailable { .. })
    ));

    request(&namespace, &disk, SERIAL).expect("the image mounts");
    let resolved = namespace.resolve("/mnt/hello.txt").expect("resolves");
    assert_eq!(resolved.mount_point(), "/mnt");
    let bytes = block_on(resolved.fs().read_at(resolved.path(), 0, 4096)).unwrap();
    assert_eq!(&bytes[..], HELLO);
    assert_eq!(resolved.writable().map(drop), Err(FsError::ReadOnly));
    let listing = block_on(namespace.list("/mnt/docs")).unwrap();
    assert_eq!(listing.len(), CJK_FILES.len() + 1);
}

#[test]
fn a_corrupted_disk_fails_the_mount_request_with_the_image_error() {
    let mut image = built(Compression::None);
    image[VOLUME_NAME] ^= 0x5a;
    let namespace = Namespace::new(MountTable::new(BootFs::new(BOOT_IMAGE, BOOT_DEVICE), 1));
    let disk = MemoryDisk::holding(SERIAL, &image, false);
    match request(&namespace, &disk, SERIAL) {
        Err(MountRequestError::Erofs {
            serial,
            error: ErofsError::Checksum { .. },
        }) => assert_eq!(serial.as_bytes(), SERIAL.as_bytes()),
        other => panic!("a corrupted image fails with its checksum error, not {other:?}"),
    }
    // Nothing was mounted: the point still shows the boot image.
    assert_eq!(
        namespace.resolve("/mnt/hello.txt").unwrap().mount_point(),
        "/"
    );
}

/// Images `mkfs.erofs` writes: uncompressed and with each codec the
/// kernel reads.
mod mkfs {
    use std::process::Command;

    use super::*;

    /// Runs `mkfs.erofs` over the tree with `args` and loads the image.
    fn mkfs(args: &[&str]) -> Vec<u8> {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().join("root");
        fs::create_dir(&root).unwrap();
        populate(&root);
        let image = scratch.path().join("image.erofs");
        let output = Command::new("mkfs.erofs")
            .args(args)
            .arg(&image)
            .arg(&root)
            .output()
            .unwrap_or_else(|error| panic!("running mkfs.erofs from erofs-utils: {error}"));
        assert!(
            output.status.success(),
            "mkfs.erofs {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::read(image).unwrap()
    }

    #[test]
    #[ignore = "needs mkfs.erofs from erofs-utils; CI runs it with --run-ignored"]
    fn mkfs_images_mount_and_read_their_tree() {
        for args in [&[][..], &["-zlz4"], &["-zlz4hc"], &["-zdeflate"]] {
            let fs = mount(&mkfs(args))
                .unwrap_or_else(|error| panic!("mkfs.erofs {args:?} image mounts: {error}"));
            assert_tree(&fs);
            assert_refusals(&fs);
        }
    }

    #[test]
    #[ignore = "needs mkfs.erofs from erofs-utils; CI runs it with --run-ignored"]
    fn corrupted_mkfs_images_fail_with_typed_errors() {
        assert_corruptions_fail(&mkfs(&[]));
    }
}
