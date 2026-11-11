//! The format's on-disk records, byte for byte.
//!
//! Every structure here is a little-endian, unaligned view over device
//! bytes: [`zerocopy`] proves at compile time that any byte pattern is a
//! valid value and that the layout has no padding, so viewing untrusted
//! bytes through one of these types cannot be undefined behaviour. None of
//! them is trusted: the modules above turn each into a validated type
//! before anything it says is used.
//!
//! Layouts follow the kernel's `include/uapi/linux/btrfs_tree.h`.

use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

/// Bytes of the checksum field at the head of a superblock and a tree block.
pub(crate) const CSUM_FIELD_BYTES: usize = 32;
/// Bytes of a superblock.
pub(crate) const SUPERBLOCK_BYTES: usize = 4096;
/// `_BHRfS_M`, the superblock magic.
pub(crate) const SUPERBLOCK_MAGIC: u64 = 0x4D5F_5366_5248_425F;
/// Bytes of the superblock's bootstrap chunk array.
pub(crate) const SYS_CHUNK_ARRAY_BYTES: usize = 2048;
/// Offset of the bootstrap chunk array inside the superblock.
pub(crate) const SYS_CHUNK_ARRAY_OFFSET: usize = 811;
/// Levels a tree may have: a leaf is level 0, the highest root level 7.
pub(crate) const MAX_LEVEL: u8 = 8;
/// Longest name a directory entry or an xattr name may carry.
pub(crate) const NAME_MAX: usize = 255;
/// Longest symlink target the format stores (`PATH_MAX` less the NUL).
pub(crate) const SYMLINK_MAX: u64 = 4095;
/// Most bytes a compressed extent occupies on disk, and most bytes it
/// decompresses to (`BTRFS_MAX_COMPRESSED`, `BTRFS_MAX_UNCOMPRESSED`).
pub(crate) const MAX_COMPRESSED_EXTENT: u64 = 128 * 1024;
/// The stripe length every chunk records (`BTRFS_STRIPE_LEN`).
pub(crate) const STRIPE_LEN: u64 = 64 * 1024;

/// Object ids with a fixed meaning.
pub(crate) mod objectid {
    pub(crate) const ROOT_TREE: u64 = 1;
    pub(crate) const CHUNK_TREE: u64 = 3;
    pub(crate) const FS_TREE: u64 = 5;
    pub(crate) const ROOT_TREE_DIR: u64 = 6;
    pub(crate) const CSUM_TREE: u64 = 7;
    pub(crate) const FIRST_FREE: u64 = 256;
    pub(crate) const LAST_FREE: u64 = -256_i64 as u64;
    pub(crate) const FIRST_CHUNK_TREE: u64 = 256;
    pub(crate) const EXTENT_CSUM: u64 = -10_i64 as u64;
}

/// Item types the read path consults.
pub(crate) mod item_type {
    pub(crate) const INODE_ITEM: u8 = 1;
    pub(crate) const XATTR_ITEM: u8 = 24;
    pub(crate) const DIR_ITEM: u8 = 84;
    pub(crate) const DIR_INDEX: u8 = 96;
    pub(crate) const EXTENT_DATA: u8 = 108;
    pub(crate) const EXTENT_CSUM: u8 = 128;
    pub(crate) const ROOT_ITEM: u8 = 132;
    pub(crate) const CHUNK_ITEM: u8 = 228;
}

/// `btrfs_disk_key`.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawKey {
    pub(crate) objectid: U64,
    pub(crate) item_type: u8,
    pub(crate) offset: U64,
}

/// `btrfs_dev_item`.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawDevItem {
    pub(crate) devid: U64,
    pub(crate) total_bytes: U64,
    pub(crate) bytes_used: U64,
    pub(crate) io_align: U32,
    pub(crate) io_width: U32,
    pub(crate) sector_size: U32,
    pub(crate) device_type: U64,
    pub(crate) generation: U64,
    pub(crate) start_offset: U64,
    pub(crate) dev_group: U32,
    pub(crate) seek_speed: u8,
    pub(crate) bandwidth: u8,
    pub(crate) uuid: [u8; 16],
    pub(crate) fsid: [u8; 16],
}

/// The head of `btrfs_super_block`, through the bootstrap chunk array.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawSuperblock {
    pub(crate) csum: [u8; CSUM_FIELD_BYTES],
    pub(crate) fsid: [u8; 16],
    pub(crate) bytenr: U64,
    pub(crate) flags: U64,
    pub(crate) magic: U64,
    pub(crate) generation: U64,
    pub(crate) root: U64,
    pub(crate) chunk_root: U64,
    pub(crate) log_root: U64,
    pub(crate) log_root_transid: U64,
    pub(crate) total_bytes: U64,
    pub(crate) bytes_used: U64,
    pub(crate) root_dir_objectid: U64,
    pub(crate) num_devices: U64,
    pub(crate) sectorsize: U32,
    pub(crate) nodesize: U32,
    pub(crate) leafsize: U32,
    pub(crate) stripesize: U32,
    pub(crate) sys_chunk_array_size: U32,
    pub(crate) chunk_root_generation: U64,
    pub(crate) compat_flags: U64,
    pub(crate) compat_ro_flags: U64,
    pub(crate) incompat_flags: U64,
    pub(crate) csum_type: U16,
    pub(crate) root_level: u8,
    pub(crate) chunk_root_level: u8,
    pub(crate) log_root_level: u8,
    pub(crate) dev_item: RawDevItem,
    pub(crate) label: [u8; 256],
    pub(crate) cache_generation: U64,
    pub(crate) uuid_tree_generation: U64,
    pub(crate) metadata_uuid: [u8; 16],
    pub(crate) nr_global_roots: U64,
    pub(crate) reserved: [u8; 216],
    pub(crate) sys_chunk_array: [u8; SYS_CHUNK_ARRAY_BYTES],
}

/// `btrfs_header`, the head of every tree block.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawHeader {
    pub(crate) csum: [u8; CSUM_FIELD_BYTES],
    pub(crate) fsid: [u8; 16],
    pub(crate) bytenr: U64,
    pub(crate) flags: U64,
    pub(crate) chunk_tree_uuid: [u8; 16],
    pub(crate) generation: U64,
    pub(crate) owner: U64,
    pub(crate) nritems: U32,
    pub(crate) level: u8,
}

/// `btrfs_item`: a leaf's slot, pointing at its data.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawItem {
    pub(crate) key: RawKey,
    pub(crate) offset: U32,
    pub(crate) size: U32,
}

/// `btrfs_key_ptr`: an internal node's slot, pointing at a child block.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawKeyPtr {
    pub(crate) key: RawKey,
    pub(crate) blockptr: U64,
    pub(crate) generation: U64,
}

/// `btrfs_chunk`, without its trailing stripes.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawChunk {
    pub(crate) length: U64,
    pub(crate) owner: U64,
    pub(crate) stripe_len: U64,
    pub(crate) chunk_type: U64,
    pub(crate) io_align: U32,
    pub(crate) io_width: U32,
    pub(crate) sector_size: U32,
    pub(crate) num_stripes: U16,
    pub(crate) sub_stripes: U16,
}

/// `btrfs_stripe`.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawStripe {
    pub(crate) devid: U64,
    pub(crate) offset: U64,
    pub(crate) dev_uuid: [u8; 16],
}

/// `btrfs_timespec`.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawTimespec {
    pub(crate) sec: U64,
    pub(crate) nsec: U32,
}

/// `btrfs_inode_item`.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawInodeItem {
    pub(crate) generation: U64,
    pub(crate) transid: U64,
    pub(crate) size: U64,
    pub(crate) nbytes: U64,
    pub(crate) block_group: U64,
    pub(crate) nlink: U32,
    pub(crate) uid: U32,
    pub(crate) gid: U32,
    pub(crate) mode: U32,
    pub(crate) rdev: U64,
    pub(crate) flags: U64,
    pub(crate) sequence: U64,
    pub(crate) reserved: [U64; 4],
    pub(crate) atime: RawTimespec,
    pub(crate) ctime: RawTimespec,
    pub(crate) mtime: RawTimespec,
    pub(crate) otime: RawTimespec,
}

/// The head of `btrfs_root_item` every format revision carries; later
/// revisions append fields the read path does not consult.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawRootItem {
    pub(crate) inode: RawInodeItem,
    pub(crate) generation: U64,
    pub(crate) root_dirid: U64,
    pub(crate) bytenr: U64,
    pub(crate) byte_limit: U64,
    pub(crate) bytes_used: U64,
    pub(crate) last_snapshot: U64,
    pub(crate) flags: U64,
    pub(crate) refs: U32,
    pub(crate) drop_progress: RawKey,
    pub(crate) drop_level: u8,
    pub(crate) level: u8,
}

/// `btrfs_dir_item`, without its trailing name and data.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawDirItem {
    pub(crate) location: RawKey,
    pub(crate) transid: U64,
    pub(crate) data_len: U16,
    pub(crate) name_len: U16,
    pub(crate) entry_type: u8,
}

/// The head of `btrfs_file_extent_item` every extent type carries. An
/// inline extent's data follows it directly.
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawFileExtentHead {
    pub(crate) generation: U64,
    pub(crate) ram_bytes: U64,
    pub(crate) compression: u8,
    pub(crate) encryption: u8,
    pub(crate) other_encoding: U16,
    pub(crate) extent_type: u8,
}

/// The tail of `btrfs_file_extent_item` a regular or preallocated extent
/// carries after [`RawFileExtentHead`].
#[derive(Clone, Copy, FromBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
pub(crate) struct RawFileExtentBody {
    pub(crate) disk_bytenr: U64,
    pub(crate) disk_num_bytes: U64,
    pub(crate) offset: U64,
    pub(crate) num_bytes: U64,
}

/// Views the head of `bytes` as `T`, or `None` when `bytes` is shorter.
pub(crate) fn view<T>(bytes: &[u8]) -> Option<&T>
where
    T: FromBytes + KnownLayout + Immutable + Unaligned,
{
    T::ref_from_prefix(bytes).ok().map(|(value, _)| value)
}

const _: () = {
    assert!(size_of::<RawKey>() == 17);
    assert!(size_of::<RawDevItem>() == 98);
    assert!(size_of::<RawSuperblock>() == SYS_CHUNK_ARRAY_OFFSET + SYS_CHUNK_ARRAY_BYTES);
    assert!(size_of::<RawHeader>() == 101);
    assert!(size_of::<RawItem>() == 25);
    assert!(size_of::<RawKeyPtr>() == 33);
    assert!(size_of::<RawChunk>() == 48);
    assert!(size_of::<RawStripe>() == 32);
    assert!(size_of::<RawInodeItem>() == 160);
    assert!(size_of::<RawRootItem>() == 239);
    assert!(size_of::<RawDirItem>() == 30);
    assert!(size_of::<RawFileExtentHead>() == 21);
    assert!(size_of::<RawFileExtentBody>() == 32);
};
