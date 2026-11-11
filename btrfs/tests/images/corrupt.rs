//! Damaged copies of a generated image fail with typed errors.
//!
//! Damage is placed by reading the image's own bytes at the offsets the
//! format defines, not through this crate, so a test never depends on the
//! reader it is checking to find what to break.

use std::collections::BTreeMap;

use helios_btrfs::{ChecksumType, Error, ItemDefect, NodeDefect, SuperblockDefect};
use helios_hal::io::IoError;

use crate::fixture::{BIG_LEN, SparseImage, base_image, block_on, mount, pattern, read_all};
use crate::walk::walk;

const PRIMARY: u64 = 64 << 10;
const BACKUP: u64 = 64 << 20;
const SUPERBLOCK_BYTES: usize = 4096;

const HEADER_BYTES: u64 = 101;
const ITEM_BYTES: u64 = 25;
const FS_TREE: u64 = 5;

const INODE_ITEM: u8 = 1;
const DIR_INDEX: u8 = 96;
const EXTENT_DATA: u8 = 108;

fn read_u64(image: &SparseImage, offset: u64) -> u64 {
    let mut bytes = [0; 8];
    image.read_bytes(offset, &mut bytes);
    u64::from_le_bytes(bytes)
}

fn read_u32(image: &SparseImage, offset: u64) -> u32 {
    let mut bytes = [0; 4];
    image.read_bytes(offset, &mut bytes);
    u32::from_le_bytes(bytes)
}

fn nodesize(image: &SparseImage) -> u64 {
    u64::from(read_u32(image, PRIMARY + 148))
}

/// Recomputes the CRC32C checksum of the `len` bytes at `offset`, the way
/// the format seals a superblock or a tree block.
fn reseal(image: &mut SparseImage, offset: u64, len: usize) {
    let mut bytes = vec![0; len];
    image.read_bytes(offset, &mut bytes);
    let digest = ChecksumType::Crc32c.digest(&bytes[32..]);
    image.write_bytes(offset, digest.as_bytes());
}

/// Applies `edit` to both superblock copies and reseals them.
fn edit_superblocks(image: &mut SparseImage, edit: impl Fn(&mut SparseImage, u64)) {
    for copy in [PRIMARY, BACKUP] {
        edit(image, copy);
        reseal(image, copy, SUPERBLOCK_BYTES);
    }
}

/// A tree block's header, read at a physical offset.
#[derive(Clone, Copy, Debug)]
struct Header {
    bytenr: u64,
    owner: u64,
    nritems: u32,
    level: u8,
}

/// Every tree block on the device, by logical address, with the physical
/// offset of each copy: a block is any written sector starting with a
/// header stamped with this filesystem's fsid.
fn tree_blocks(image: &SparseImage) -> BTreeMap<u64, (Header, Vec<u64>)> {
    let mut fsid = [0; 16];
    image.read_bytes(PRIMARY + 32, &mut fsid);
    let mut blocks: BTreeMap<u64, (Header, Vec<u64>)> = BTreeMap::new();
    for physical in image.written_blocks().collect::<Vec<_>>() {
        if (PRIMARY..PRIMARY + 4096).contains(&physical) || physical == BACKUP {
            continue;
        }
        let mut stamp = [0; 16];
        image.read_bytes(physical + 32, &mut stamp);
        if stamp != fsid {
            continue;
        }
        let header = Header {
            bytenr: read_u64(image, physical + 48),
            owner: read_u64(image, physical + 88),
            nritems: read_u32(image, physical + 96),
            level: {
                let mut level = [0];
                image.read_bytes(physical + 100, &mut level);
                level[0]
            },
        };
        blocks
            .entry(header.bytenr)
            .or_insert((header, Vec::new()))
            .1
            .push(physical);
    }
    blocks
}

/// The leaves of the top-level subvolume's tree.
fn fs_leaves(image: &SparseImage) -> Vec<(Header, Vec<u64>)> {
    tree_blocks(image)
        .into_values()
        .filter(|(header, _)| header.owner == FS_TREE && header.level == 0)
        .collect()
}

/// Applies `edit` to every copy of the block at `copies` and reseals it.
fn edit_block(image: &mut SparseImage, copies: &[u64], edit: impl Fn(&mut SparseImage, u64)) {
    let len = nodesize(image) as usize;
    for physical in copies {
        edit(image, *physical);
        reseal(image, *physical, len);
    }
}

fn walk_image(image: SparseImage) -> Result<(), Error> {
    let fs = mount(image)?;
    walk(&fs).map(|_| ())
}

/// The first leaf slot whose key has `item_type`, and that leaf.
fn find_item(image: &SparseImage, item_type: u8) -> ((Header, Vec<u64>), u64) {
    for leaf in fs_leaves(image) {
        let physical = leaf.1[0];
        for slot in 0..u64::from(leaf.0.nritems) {
            let mut kind = [0];
            image.read_bytes(physical + HEADER_BYTES + slot * ITEM_BYTES + 8, &mut kind);
            if kind[0] == item_type {
                return (leaf, slot);
            }
        }
    }
    panic!("no leaf of the top-level tree holds an item of type {item_type}");
}

/// The physical offset of item `slot`'s data in the leaf at `physical`.
fn item_data(image: &SparseImage, physical: u64, slot: u64) -> u64 {
    let offset = read_u32(image, physical + HEADER_BYTES + slot * ITEM_BYTES + 17);
    physical + HEADER_BYTES + u64::from(offset)
}

#[test]
fn a_damaged_primary_superblock_falls_back_to_the_backup() {
    let mut image = base_image();
    image.flip_bit(PRIMARY + 300, 3);
    let fs = mount(image).expect("mounts from the backup");
    assert_eq!(fs.superblock().mirror(), 1);
    walk(&fs).expect("the filesystem reads through the backup superblock");
}

#[test]
fn every_superblock_damaged_refuses_the_mount() {
    let mut image = base_image();
    image.flip_bit(PRIMARY + 300, 3);
    image.flip_bit(BACKUP + 300, 3);
    assert_eq!(
        mount(image).err(),
        Some(Error::NoValidSuperblock {
            primary: SuperblockDefect::Checksum,
            backups_checked: 1,
        })
    );
}

#[test]
fn superblock_fields_are_validated_past_the_checksum() {
    let mut image = base_image();
    edit_superblocks(&mut image, |image, copy| {
        image.write_bytes(copy + 144, &1000_u32.to_le_bytes());
    });
    assert_eq!(
        mount(image).err(),
        Some(Error::NoValidSuperblock {
            primary: SuperblockDefect::SectorSize(1000),
            backups_checked: 1,
        })
    );

    let mut image = base_image();
    for copy in [PRIMARY, BACKUP] {
        image.write_bytes(copy + 196, &9_u16.to_le_bytes());
    }
    assert_eq!(
        mount(image).err(),
        Some(Error::NoValidSuperblock {
            primary: SuperblockDefect::ChecksumType(9),
            backups_checked: 1,
        })
    );
}

#[test]
fn unimplemented_features_refuse_the_mount_by_name() {
    for (bit, name) in [
        (1_u64 << 7, "raid56"),
        (1 << 12, "zoned"),
        (1 << 15, "undefined"),
    ] {
        let mut image = base_image();
        edit_superblocks(&mut image, |image, copy| {
            let flags = read_u64(image, copy + 188) | bit;
            image.write_bytes(copy + 188, &flags.to_le_bytes());
        });
        assert_eq!(
            mount(image).err(),
            Some(Error::UnsupportedIncompat { name, bit })
        );
    }

    let mut image = base_image();
    edit_superblocks(&mut image, |image, copy| {
        image.write_bytes(copy + 136, &2_u64.to_le_bytes());
    });
    assert_eq!(mount(image).err(), Some(Error::MultiDevice { devices: 2 }));

    let mut image = base_image();
    edit_superblocks(&mut image, |image, copy| {
        image.write_bytes(copy + 96, &(1_u64 << 20).to_le_bytes());
    });
    assert_eq!(
        mount(image).err(),
        Some(Error::LogReplayRequired { log_root: 1 << 20 })
    );
}

#[test]
fn a_truncated_image_fails_with_typed_errors() {
    let image = base_image();
    assert_eq!(
        mount(image.truncated(16 << 10)).err(),
        Some(Error::NoValidSuperblock {
            primary: SuperblockDefect::BeyondDevice,
            backups_checked: 0,
        })
    );
    let mut failed = 0;
    for len in [1 << 20, 8 << 20, 32 << 20, 96 << 20] {
        match walk_image(image.truncated(len)) {
            Ok(()) => {}
            Err(Error::Io {
                source: IoError::OutOfBounds,
                ..
            }) => failed += 1,
            Err(other) => panic!("a {len}-byte cut failed with {other}"),
        }
    }
    assert!(failed > 0, "some cut must lose blocks the walk needs");
}

#[test]
fn one_damaged_copy_of_a_dup_block_reads_from_the_other() {
    let mut image = base_image();
    let leaves = fs_leaves(&image);
    assert!(leaves.len() >= 10, "the top-level tree spans many leaves");
    let (_, copies) = leaves.into_iter().next().expect("a leaf");
    assert_eq!(copies.len(), 2, "metadata is dup");
    image.flip_bit(copies[0] + 2000, 5);
    walk_image(image).expect("the other copy verifies");
}

#[test]
fn a_damaged_tree_block_fails_its_checksum() {
    let mut image = base_image();
    let (header, copies) = fs_leaves(&image)
        .into_iter()
        .nth(1)
        .expect("several leaves");
    for physical in &copies {
        image.flip_bit(physical + 2000, 5);
    }
    assert_eq!(
        walk_image(image),
        Err(Error::TreeBlock {
            logical: header.bytenr,
            defect: NodeDefect::Checksum,
        })
    );
}

#[test]
fn damaged_root_and_chunk_trees_refuse_the_mount() {
    let image = base_image();
    let blocks = tree_blocks(&image);
    for field in [80, 88] {
        let logical = read_u64(&image, PRIMARY + field);
        let mut damaged = image.clone();
        for physical in &blocks[&logical].1 {
            damaged.flip_bit(physical + 200, 1);
        }
        assert_eq!(
            mount(damaged).err(),
            Some(Error::TreeBlock {
                logical,
                defect: NodeDefect::Checksum,
            })
        );
    }
}

#[test]
fn tree_blocks_are_validated_past_the_checksum() {
    let image = base_image();
    let (header, copies) = fs_leaves(&image)
        .into_iter()
        .nth(1)
        .expect("several leaves");
    let logical = header.bytenr;
    type Edit = Box<dyn Fn(&mut SparseImage, u64)>;
    let cases: [(&str, Edit, NodeDefect); 5] = [
        (
            "item count",
            Box::new(|image, at| image.write_bytes(at + 96, &0x7fff_u32.to_le_bytes())),
            NodeDefect::ItemCount { nritems: 0x7fff },
        ),
        (
            "level",
            Box::new(|image, at| image.write_bytes(at + 100, &[1])),
            NodeDefect::Level {
                expected: 0,
                found: 1,
            },
        ),
        (
            "bytenr",
            Box::new(move |image, at| image.write_bytes(at + 48, &(logical + 4096).to_le_bytes())),
            NodeDefect::Bytenr {
                recorded: logical + 4096,
            },
        ),
        (
            "key order",
            Box::new(|image, at| {
                let mut first = [0; 17];
                let mut second = [0; 17];
                image.read_bytes(at + HEADER_BYTES, &mut first);
                image.read_bytes(at + HEADER_BYTES + ITEM_BYTES, &mut second);
                image.write_bytes(at + HEADER_BYTES, &second);
                image.write_bytes(at + HEADER_BYTES + ITEM_BYTES, &first);
            }),
            NodeDefect::KeyOrder { slot: 1 },
        ),
        (
            "item data",
            Box::new(|image, at| {
                let field = at + HEADER_BYTES + 2 * ITEM_BYTES + 17;
                let offset = read_u32(image, field) - 1;
                image.write_bytes(field, &offset.to_le_bytes());
            }),
            NodeDefect::ItemData { slot: 2 },
        ),
    ];
    for (what, edit, defect) in cases {
        let mut damaged = image.clone();
        edit_block(&mut damaged, &copies, edit);
        assert_eq!(
            walk_image(damaged),
            Err(Error::TreeBlock { logical, defect }),
            "{what}"
        );
    }
}

#[test]
fn malformed_items_fail_with_the_check_they_broke() {
    let image = base_image();

    // A directory index entry with an empty name.
    let ((_, copies), slot) = find_item(&image, DIR_INDEX);
    let mut damaged = image.clone();
    edit_block(&mut damaged, &copies, |image, at| {
        image.write_bytes(item_data(image, at, slot) + 27, &0_u16.to_le_bytes());
    });
    assert!(matches!(
        walk_image(damaged),
        Err(Error::Item {
            defect: ItemDefect::NameLength(0),
            ..
        })
    ));

    // An inode whose mode names no file type.
    let ((_, copies), slot) = find_item(&image, INODE_ITEM);
    let mut damaged = image.clone();
    edit_block(&mut damaged, &copies, |image, at| {
        image.write_bytes(item_data(image, at, slot) + 52, &0o644_u32.to_le_bytes());
    });
    assert!(matches!(
        walk_image(damaged),
        Err(Error::Item {
            defect: ItemDefect::Mode(0o644),
            ..
        })
    ));

    // A file extent compressed with an algorithm the format does not define.
    let ((_, copies), slot) = find_item(&image, EXTENT_DATA);
    let mut damaged = image.clone();
    edit_block(&mut damaged, &copies, |image, at| {
        image.write_bytes(item_data(image, at, slot) + 16, &[9]);
    });
    assert!(matches!(
        walk_image(damaged),
        Err(Error::Item {
            defect: ItemDefect::Compression(9),
            ..
        })
    ));
}

#[test]
fn damaged_file_data_fails_its_checksum() {
    let mut image = base_image();
    let expected = pattern(BIG_LEN, 7);
    let block = image
        .find_block_starting_with(&expected[..64])
        .expect("the file's first sector is on the device");
    image.flip_bit(block + 100, 0);
    let fs = mount(image).expect("metadata is intact");
    let big = block_on(fs.resolve(b"/big.bin")).expect("resolves");
    assert!(matches!(
        read_all(&fs, &big, 1 << 20),
        Err(Error::DataChecksum { .. })
    ));
    // Bytes outside the damaged sector still read.
    let mut tail = vec![0; 4096];
    block_on(fs.read(&big, 1 << 19, &mut tail)).expect("an intact window reads");
    assert_eq!(tail, expected[1 << 19..(1 << 19) + 4096]);
}
