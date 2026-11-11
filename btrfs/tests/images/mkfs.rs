//! Images `mkfs.btrfs --rootdir` writes, one per checksum algorithm.
//!
//! These need btrfs-progs, which the Linux CI runner installs and the macOS
//! development host does not have, so they are ignored by default and the
//! CI lane runs them with `--run-ignored`. They fail, never skip, when the
//! tool is missing.

use std::fs;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

use helios_btrfs::ChecksumType;

use crate::fixture::{
    BIG_LEN, CJK_FILES, HELLO, SparseImage, block_on, many_content, many_name, mount, pattern,
    read_all,
};
use crate::walk::walk;

const IMAGE_BYTES: u64 = 256 << 20;
const MKFS_FILES: usize = 1200;

/// Fills `root` with the tree every image here is built from.
fn populate(root: &Path) {
    fs::write(root.join("hello.txt"), HELLO).unwrap();
    fs::write(root.join("big.bin"), pattern(BIG_LEN, 7)).unwrap();
    fs::create_dir_all(root.join("docs/nested")).unwrap();
    for (name, content) in CJK_FILES {
        fs::write(root.join("docs").join(name), content).unwrap();
    }
    symlink("../hello.txt", root.join("docs/hello")).unwrap();
    symlink("/big.bin", root.join("docs/nested/big")).unwrap();
    fs::create_dir(root.join("many")).unwrap();
    for index in 0..MKFS_FILES {
        fs::write(
            root.join("many").join(many_name(index)),
            many_content(index),
        )
        .unwrap();
    }
}

/// Runs `mkfs.btrfs` over `root` and loads the image it wrote.
fn mkfs(root: &Path, args: &[&str]) -> SparseImage {
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("image");
    fs::File::create(&path)
        .unwrap()
        .set_len(IMAGE_BYTES)
        .unwrap();
    let output = Command::new("mkfs.btrfs")
        .args(["-q", "-f", "--rootdir"])
        .arg(root)
        .args(args)
        .arg(&path)
        .output()
        .unwrap_or_else(|error| panic!("running mkfs.btrfs from btrfs-progs: {error}"));
    assert!(
        output.status.success(),
        "mkfs.btrfs {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut image = SparseImage::new(IMAGE_BYTES);
    let mut file = fs::File::open(&path).unwrap();
    let mut block = vec![0; 1 << 16];
    let mut offset = 0;
    loop {
        let count = file.read(&mut block).unwrap();
        if count == 0 {
            break;
        }
        if block[..count].iter().any(|byte| *byte != 0) {
            image.write_bytes(offset, &block[..count]);
        }
        offset += count as u64;
    }
    image
}

fn check(image: SparseImage, csum: ChecksumType) {
    let fs = mount(image).expect("mounts");
    assert_eq!(fs.superblock().csum_type(), csum);
    let big = block_on(fs.resolve(b"/docs/nested/big")).expect("resolves");
    assert_eq!(
        read_all(&fs, &big, 100_000).expect("reads"),
        pattern(BIG_LEN, 7)
    );
    let hello = block_on(fs.resolve(b"/docs/hello")).expect("resolves");
    assert_eq!(read_all(&fs, &hello, 4096).expect("reads"), HELLO);
    for (name, content) in CJK_FILES {
        let file = block_on(fs.resolve(format!("/docs/{name}").as_bytes())).expect("resolves");
        assert_eq!(
            read_all(&fs, &file, 4096).expect("reads"),
            content.as_bytes()
        );
    }
    let census = walk(&fs).expect("walks");
    assert_eq!(census.files, 2 + CJK_FILES.len() + MKFS_FILES);
}

#[test]
#[ignore = "needs mkfs.btrfs from btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn mkfs_btrfs_images_for_every_checksum() {
    let root = tempfile::tempdir().unwrap();
    populate(root.path());
    for (name, csum) in [
        ("crc32c", ChecksumType::Crc32c),
        ("xxhash", ChecksumType::XxHash64),
        ("sha256", ChecksumType::Sha256),
        ("blake2", ChecksumType::Blake2b),
    ] {
        check(mkfs(root.path(), &["--csum", name]), csum);
    }
}

#[test]
#[ignore = "needs mkfs.btrfs from btrfs-progs; the Linux CI lane runs it with --run-ignored"]
fn mkfs_btrfs_images_at_every_node_size() {
    let root = tempfile::tempdir().unwrap();
    populate(root.path());
    for nodesize in ["4096", "16384", "65536"] {
        check(
            mkfs(root.path(), &["--nodesize", nodesize]),
            ChecksumType::Crc32c,
        );
    }
}
