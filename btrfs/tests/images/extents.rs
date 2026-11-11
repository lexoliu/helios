//! Compressed, sparse and preallocated files, written by a copy-on-write
//! transaction onto a formatted image.

use helios_btrfs::Error;

use crate::fixture::{
    CJK_FILES, LZO_INODE, MANY_FILES, PREALLOC_INODE, SPARSE_INODE, SPARSE_SECOND_RUN, ZLIB_INODE,
    ZSTD_INLINE_INODE, ZSTD_INODE, added_content, block_on, extents_image, mount, read_all,
};
use crate::walk::walk;

const FILES: [(&[u8], u64); 6] = [
    (b"/zlib.txt", ZLIB_INODE),
    (b"/lzo.txt", LZO_INODE),
    (b"/zstd.txt", ZSTD_INODE),
    (b"/zstd-inline.txt", ZSTD_INLINE_INODE),
    (b"/sparse.bin", SPARSE_INODE),
    (b"/prealloc.bin", PREALLOC_INODE),
];

#[test]
fn reads_every_extent_kind() {
    let fs = mount(extents_image()).expect("mounts");
    assert!(
        fs.superblock().generation() > 1,
        "a second transaction committed"
    );
    for (path, inode) in FILES {
        let file = block_on(fs.resolve(path)).expect("resolves");
        assert_eq!(file.number(), inode);
        let expected = added_content(inode);
        for chunk in [4096, 3000, 1 << 20] {
            assert_eq!(
                read_all(&fs, &file, chunk).expect("reads"),
                expected,
                "{} in chunks of {chunk}",
                String::from_utf8_lossy(path)
            );
        }
    }
}

#[test]
fn reads_inside_compressed_extents_at_any_offset() {
    let fs = mount(extents_image()).expect("mounts");
    for inode in [ZLIB_INODE, LZO_INODE, ZSTD_INODE] {
        let path = FILES.iter().find(|(_, number)| *number == inode).unwrap().0;
        let file = block_on(fs.resolve(path)).expect("resolves");
        let expected = added_content(inode);
        for (offset, len) in [
            (1, 7),
            (131_000, 2_000),
            (200_001, 50_000),
            (307_000, 4_000),
        ] {
            let mut buf = vec![0; len];
            let count = block_on(fs.read(&file, offset, &mut buf)).expect("reads");
            let start = offset as usize;
            let end = (start + len).min(expected.len());
            assert_eq!(&buf[..count], &expected[start..end]);
        }
    }
}

#[test]
fn holes_read_as_zeroes() {
    let fs = mount(extents_image()).expect("mounts");
    let sparse = block_on(fs.resolve(b"/sparse.bin")).expect("resolves");
    let mut buf = vec![0xff; 4096];
    block_on(fs.read(&sparse, SPARSE_SECOND_RUN - 4096, &mut buf)).expect("reads");
    assert!(buf.iter().all(|byte| *byte == 0));
}

#[test]
fn the_formatted_files_survive_the_transaction() {
    let fs = mount(extents_image()).expect("mounts");
    let census = walk(&fs).expect("walks");
    assert_eq!(
        census.files,
        6 + CJK_FILES.len() + MANY_FILES + 1 + FILES.len()
    );
}

#[test]
fn a_damaged_compressed_extent_fails_its_checksum() {
    let mut image = extents_image();
    // Every zstd frame starts with its magic number, and btrfs starts each
    // compressed extent on a sector boundary.
    let block = image
        .find_block_starting_with(&[0x28, 0xb5, 0x2f, 0xfd])
        .expect("a zstd extent is on the device");
    image.flip_bit(block + 40, 2);
    let fs = mount(image).expect("metadata is intact");
    let zstd = block_on(fs.resolve(b"/zstd.txt")).expect("resolves");
    assert!(matches!(
        read_all(&fs, &zstd, 1 << 20),
        Err(Error::DataChecksum { .. })
    ));
}
