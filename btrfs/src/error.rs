//! Typed failures.
//!
//! Every failure names what was checked and where: the device offset, the
//! logical address, the tree and key. A malformed image is never a panic;
//! it is one of the defects below, and the caller can tell a damaged
//! filesystem ([`Error::is_corruption`]) from a device that failed to
//! answer and from a filesystem this crate deliberately refuses.

use core::fmt;

use helios_hal::io::IoError;
use thiserror::Error;

use crate::key::Key;

/// What went wrong reading a btrfs filesystem.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum Error {
    /// The block source failed a read.
    #[error("reading {len} bytes at device offset {offset:#x}: {source}")]
    Io {
        offset: u64,
        len: usize,
        source: IoError,
    },
    /// No superblock copy on the device verified. `primary` says why the
    /// primary copy was rejected; `backups_checked` counts the backup
    /// copies that fit on the device, none of which verified either.
    #[error(
        "no valid superblock: the primary copy {primary}, and none of the {backups_checked} backup copies verified"
    )]
    NoValidSuperblock {
        primary: SuperblockDefect,
        backups_checked: u8,
    },
    /// The filesystem uses an incompat feature this crate does not
    /// implement; mounting it anyway would misread it.
    #[error("incompat feature {name} ({bit:#x}) is not implemented")]
    UnsupportedIncompat { name: &'static str, bit: u64 },
    /// The filesystem spans several devices; only single-device
    /// filesystems are implemented.
    #[error("the filesystem spans {devices} devices; only single-device filesystems are supported")]
    MultiDevice { devices: u64 },
    /// A chunk uses a block-group profile that needs more than one device.
    #[error(
        "chunk at logical {logical:#x} uses the {profile} profile, which needs several devices"
    )]
    UnsupportedProfile { logical: u64, profile: &'static str },
    /// The filesystem holds a log tree that must be replayed before its
    /// trees describe the fsynced state; replay is not implemented.
    #[error("the filesystem has an unreplayed log tree at logical {log_root:#x}")]
    LogReplayRequired { log_root: u64 },
    /// The bootstrap chunk array in the superblock is malformed.
    #[error("superblock sys_chunk_array at byte {offset}: {defect}")]
    SysChunkArray { offset: usize, defect: ChunkDefect },
    /// A chunk item is malformed.
    #[error("chunk at logical {logical:#x}: {defect}")]
    Chunk { logical: u64, defect: ChunkDefect },
    /// A logical range is not inside any chunk.
    #[error("logical range {logical:#x}+{len:#x} is not inside one chunk")]
    Unmapped { logical: u64, len: u64 },
    /// A tree block failed validation on every copy.
    #[error("tree block at logical {logical:#x}: {defect}")]
    TreeBlock { logical: u64, defect: NodeDefect },
    /// Two consecutive items of a tree walk are not in ascending key order,
    /// so the tree is malformed or links back on itself.
    #[error("tree {tree}: key {next} follows {previous}, breaking ascending order")]
    TreeOrder { tree: u64, previous: Key, next: Key },
    /// An item's payload is malformed.
    #[error("tree {tree}, item {key}: {defect}")]
    Item {
        tree: u64,
        key: Key,
        defect: ItemDefect,
    },
    /// A tree the filesystem needs has no root item.
    #[error("tree {tree} has no root item")]
    MissingRoot { tree: u64 },
    /// A directory entry or a root item names an inode with no inode item.
    #[error("tree {tree} has no inode item for inode {inode}")]
    MissingInode { tree: u64, inode: u64 },
    /// A data block's checksum does not match on any copy.
    #[error("data checksum mismatch for the sector at logical {logical:#x}")]
    DataChecksum { logical: u64 },
    /// A data block of a file that is checksummed has no checksum.
    #[error("no data checksum for the sector at logical {logical:#x}")]
    MissingDataChecksum { logical: u64 },
    /// A compressed extent does not decompress.
    #[error(
        "{algorithm} extent at file offset {file_offset:#x} of inode {inode} in tree {tree}: {defect}"
    )]
    Decompress {
        algorithm: Compression,
        tree: u64,
        inode: u64,
        file_offset: u64,
        defect: DecompressDefect,
    },
    /// The inode is not of the kind the operation needs.
    #[error("inode {inode} in tree {tree} is a {found}, not a {expected}")]
    WrongKind {
        tree: u64,
        inode: u64,
        expected: FileKind,
        found: FileKind,
    },
    /// A path component does not exist. `component` counts the components
    /// walked before it, symlink targets included, from zero.
    #[error("path component {component} does not exist")]
    NotFound { component: usize },
    /// Path resolution followed more symlinks than the limit.
    #[error("path resolution followed more than {limit} symlinks")]
    SymlinkLimit { limit: usize },
}

impl Error {
    /// Whether the error reports damage to the filesystem image, as
    /// opposed to a device failure, a refused feature or a missing path.
    pub fn is_corruption(&self) -> bool {
        matches!(
            self,
            Self::NoValidSuperblock { .. }
                | Self::SysChunkArray { .. }
                | Self::Chunk { .. }
                | Self::Unmapped { .. }
                | Self::TreeBlock { .. }
                | Self::TreeOrder { .. }
                | Self::Item { .. }
                | Self::MissingRoot { .. }
                | Self::MissingInode { .. }
                | Self::DataChecksum { .. }
                | Self::MissingDataChecksum { .. }
                | Self::Decompress { .. }
        )
    }
}

/// Why a superblock copy was rejected.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SuperblockDefect {
    #[error("does not fit on the device")]
    BeyondDevice,
    #[error("could not be read: {0}")]
    Unreadable(IoError),
    #[error("has no btrfs magic")]
    Magic,
    #[error("records its own location as {recorded:#x}")]
    Bytenr { recorded: u64 },
    #[error("names checksum type {0}, which the format does not define")]
    ChecksumType(u16),
    #[error("fails its checksum")]
    Checksum,
    #[error("has sector size {0}, not a power of two from 4 KiB to 64 KiB")]
    SectorSize(u32),
    #[error("has node size {nodesize} for sector size {sectorsize}")]
    NodeSize { nodesize: u32, sectorsize: u32 },
    #[error("has a {field} of {value:#x}, not aligned to the sector size")]
    Unaligned { field: &'static str, value: u64 },
    #[error("has a {field} of {level}, beyond the deepest tree level")]
    Level { field: &'static str, level: u8 },
    #[error("has a sys_chunk_array_size of {0}, larger than the array")]
    SysChunkArraySize(u32),
    #[error("has a device item whose fsid is not the filesystem's")]
    DeviceFsid,
    #[error("records a device of {0} bytes")]
    DeviceSize(u64),
}

/// Why a chunk, from the chunk tree or the bootstrap array, was rejected.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ChunkDefect {
    #[error("the record is truncated")]
    Truncated,
    #[error("the key is not a chunk item key")]
    Key,
    #[error("its length {0:#x} is zero or not sector aligned")]
    Length(u64),
    #[error("its logical start is not sector aligned")]
    LogicalAlignment,
    #[error("its stripe length is {0:#x}, not 64 KiB")]
    StripeLength(u64),
    #[error("its type {0:#x} names no block-group kind")]
    Kind(u64),
    #[error("its type {chunk_type:#x} holds {stripes} stripes")]
    StripeCount { chunk_type: u64, stripes: u16 },
    #[error("a stripe is on device {0}, not this filesystem's device")]
    Device(u64),
    #[error("a stripe at physical {0:#x} is not sector aligned or runs past the device")]
    Stripe(u64),
    #[error("it overlaps the chunk before it")]
    Overlap,
    #[error("it differs from the chunk tree's record of it")]
    Mismatch,
    #[error("its logical range overflows")]
    Overflow,
}

/// Why a tree block was rejected.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum NodeDefect {
    #[error("fails its checksum")]
    Checksum,
    #[error("records its own address as {recorded:#x}")]
    Bytenr { recorded: u64 },
    #[error("belongs to another filesystem")]
    Fsid,
    #[error("is at level {found}, expected {expected}")]
    Level { expected: u8, found: u8 },
    #[error("has generation {found}, its parent records {expected}")]
    Generation { expected: u64, found: u64 },
    #[error("has generation {found}, newer than the superblock's {superblock}")]
    FutureGeneration { found: u64, superblock: u64 },
    #[error("claims {nritems} items, more than fit in a block")]
    ItemCount { nritems: u32 },
    #[error("is empty below the root")]
    Empty,
    #[error("slot {slot} has data outside the block or out of order")]
    ItemData { slot: u32 },
    #[error("slot {slot} has a key not above the previous slot's")]
    KeyOrder { slot: u32 },
    #[error("starts with key {found}, its parent points at it with {expected}")]
    FirstKey { expected: Key, found: Key },
}

/// Why an item's payload was rejected.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ItemDefect {
    #[error("holds {found} bytes, needs at least {needed}")]
    Size { needed: usize, found: usize },
    #[error("has a name of {0} bytes")]
    NameLength(usize),
    #[error("carries {0} data bytes where none belong")]
    UnexpectedData(usize),
    #[error("has directory entry type {0}")]
    EntryType(u8),
    #[error("points at a location of item type {0}")]
    LocationType(u8),
    #[error("is a directory index holding more than one entry")]
    IndexEntries,
    #[error("has inode mode {0:#o}, which names no file type")]
    Mode(u32),
    #[error("has a timestamp with {0} nanoseconds")]
    Nanoseconds(u32),
    #[error("has extent type {0}")]
    ExtentType(u8),
    #[error("has compression type {0}")]
    Compression(u8),
    #[error("has encryption {encryption} or other encoding {other_encoding}")]
    Encoding { encryption: u8, other_encoding: u16 },
    #[error("has its {field} of {value:#x} not sector aligned")]
    Unaligned { field: &'static str, value: u64 },
    #[error("references bytes {offset:#x}+{len:#x} beyond its {limit:#x}-byte extent")]
    ExtentRange { offset: u64, len: u64, limit: u64 },
    #[error("records {ram_bytes} bytes but holds {stored}")]
    InlineSize { ram_bytes: u64, stored: usize },
    #[error("is an inline extent at file offset {0:#x}, not 0")]
    InlineOffset(u64),
    #[error("is a compressed extent of {0} bytes, more than 128 KiB")]
    CompressedSize(u64),
    #[error("starts before the previous extent ends at {0:#x}")]
    ExtentOverlap(u64),
    #[error("is a file extent whose range overflows")]
    ExtentOverflow,
    #[error("is a checksum item of {0} bytes, not whole checksums")]
    ChecksumItem(usize),
    #[error("is a symlink of {0} bytes")]
    SymlinkSize(u64),
    #[error("is a root item at level {0}")]
    RootLevel(u8),
    #[error("is a root item with bytenr {0:#x} not sector aligned")]
    RootBytenr(u64),
}

/// Why a compressed extent failed to decompress.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DecompressDefect {
    #[error("the stream is malformed")]
    Malformed,
    #[error("the stream ends before its end marker")]
    Truncated,
    #[error("the stream decompresses to more than its {0} bytes")]
    Overrun(u64),
    #[error("the stream declares a {0}-byte window, beyond 128 KiB")]
    Window(u64),
}

/// The compression an extent is stored with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    Zlib,
    Lzo,
    Zstd,
}

impl fmt::Display for Compression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Zlib => "zlib",
            Self::Lzo => "lzo",
            Self::Zstd => "zstd",
        })
    }
}

/// The kind of file an inode is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
}

impl fmt::Display for FileKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Regular => "regular file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::CharDevice => "character device",
            Self::BlockDevice => "block device",
            Self::Fifo => "fifo",
            Self::Socket => "socket",
        })
    }
}
