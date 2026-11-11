//! Read-only btrfs for Helios.
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
//! The read path is shaped for the copy-on-write transactions that follow
//! it. Every lookup starts from a tree root captured once — the
//! superblock's roots at mount, an [`Inode`]'s subvolume root after — so a
//! commit that publishes new roots and flips the superblock leaves readers
//! of the previous commit consistent, as long as the blocks those roots
//! reach are not reused while they are read.

#![no_std]

extern crate alloc;

mod chunk;
mod compress;
mod csum;
mod error;
mod fs;
mod items;
mod key;
mod ondisk;
mod source;
mod superblock;
mod tree;
mod volume;

pub use csum::{ChecksumType, Digest};
pub use error::{
    ChunkDefect, Compression, DecompressDefect, Error, FileKind, ItemDefect, NodeDefect,
    SuperblockDefect,
};
pub use fs::{DirEntry, DirPosition, Filesystem, Inode, MAX_SYMLINK_FOLLOWS};
pub use items::{EntryTarget, Metadata, Timestamp};
pub use key::Key;
pub use source::{BlockSource, READ_ALIGNMENT};
pub use superblock::{MIRROR_OFFSETS, RootPointer, Superblock};
