//! Btrfs for Helios, with immutable read views and optional CoW transactions.
//!
//! [`Filesystem::mount`] takes any [`BlockSource`] — in the kernel, the
//! block cache in front of a virtio-blk disk — and reads a single-device
//! btrfs: the superblock (falling back to a backup copy when the primary
//! fails verification), the chunk tree through the superblock's bootstrap
//! chunks, the root tree, and the subvolume trees with their inodes,
//! directories, symlinks, extended attributes and file extents, inline,
//! regular, preallocated and holes, zlib, LZO and zstd compressed.
//!
//! # Untrusted input
//!
//! The device is not trusted. Every superblock, tree block and data sector
//! is checked against its checksum (CRC32C, xxHash64, SHA-256 or BLAKE2b,
//! as the superblock names) before anything in it is used, and every
//! on-disk record becomes a validated type first. A malformed image is a
//! typed [`Error`] naming the failed check, never a panic. Nothing is
//! allocated in proportion to an on-disk value that was not first bounded
//! by a format limit: tree blocks are one node size, data is read in
//! windows of at most 128 KiB, and compressed extents are refused above
//! the format's 128 KiB. Walks are bounded: a descent by the format's eight
//! levels, a walk across leaves by the ascending order of the keys it
//! yields, which a cycle or a shared subtree breaks. An incompat feature
//! this crate does not implement refuses the mount with the feature named.
//!
//! # Concurrency
//!
//! A mounted [`Filesystem`] is immutable: every operation takes `&self`,
//! takes no lock, and may run concurrently on any processor. The only
//! shared resource is the [`BlockSource`], whose contract is the same.
//!
//! # Writing
//!
//! The `write` feature exposes `write::Writer` and its running transaction,
//! with per-operation rollback. As Linux's btrfs, a change is visible at once
//! and durable at commit: every accepted mutation publishes a new immutable
//! view of the transaction's private CoW blocks, and a commit writes them and
//! then the superblocks. Existing readers retain the complete mapping and
//! checksum roots of their view; blocks it reaches cannot be reused while any
//! reader still holds it.

#![no_std]

extern crate alloc;

#[cfg(feature = "write")]
mod accounting;
#[cfg(feature = "write")]
mod allocation;
#[cfg(feature = "write")]
mod checksums;
mod chunk;
#[cfg(feature = "write")]
mod commit;
mod compress;
mod csum;
mod data;
mod error;
#[cfg(feature = "write")]
mod free_space;
mod fs;
mod items;
mod key;
mod ondisk;
#[cfg(feature = "write")]
mod retirement;
#[cfg(feature = "write")]
mod roots;
mod source;
#[cfg(feature = "write")]
mod space;
mod superblock;
#[cfg(feature = "write")]
mod transaction;
mod tree;
#[cfg(feature = "write")]
mod uuid_tree;
mod volume;
#[cfg(feature = "write")]
pub mod write;

pub use csum::{ChecksumType, Digest};
pub use error::{
    ChunkDefect, Compression, DecompressDefect, Error, FileKind, ItemDefect, NodeDefect,
    SuperblockDefect,
};
pub use fs::{DirEntry, DirPosition, Filesystem, Inode, MAX_SYMLINK_FOLLOWS};
#[cfg(feature = "test-support")]
pub use fs::{ExtentInfo, ExtentKind};
pub use items::{DeviceNumber, EntryTarget, Metadata, Timestamp};
pub use key::Key;
pub use source::{BlockSink, BlockSource, READ_ALIGNMENT};
pub use superblock::{MIRROR_OFFSETS, RootPointer, Superblock};

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod testing;
