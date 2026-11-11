//! What the filesystem reads from.

use core::future::Future;

use helios_hal::io::IoResult;

/// Granularity of every read the filesystem issues: each starts on a
/// multiple of this many bytes and is a multiple of it long.
///
/// The format's smallest sector is 4 KiB, superblock copies sit at 4 KiB
/// multiples, and every chunk, tree block and data extent is aligned to the
/// filesystem's sector size, so no read ever needs a partial block.
pub const READ_ALIGNMENT: u64 = 4096;

/// A device or a cache in front of one, read by byte offset.
///
/// This is the whole of what the read path needs: positional, async reads
/// of whole aligned blocks, and the device's length. It is deliberately not
/// [`helios_hal::fs::BlockDevice`]: the filesystem reads through the
/// kernel's block cache rather than the raw device, addresses bytes rather
/// than device blocks of a size it does not choose, and has no use for the
/// device's flush, discard and identification surface. The kernel's cache
/// implements this trait for a cached device; tests implement it over an
/// image in memory.
///
/// # Concurrency
///
/// `read_at` takes `&self` and may be called from any processor, any
/// number of times concurrently; the filesystem issues independent reads
/// from independent tasks and never serialises them. An implementation
/// that needs exclusion provides it itself.
pub trait BlockSource: Send + Sync {
    /// Bytes the source holds.
    fn size_bytes(&self) -> u64;

    /// Fills `buf` with the bytes starting at `offset`.
    ///
    /// `offset` and `buf.len()` are multiples of [`READ_ALIGNMENT`]. A
    /// range reaching past [`size_bytes`](Self::size_bytes) fails with
    /// [`helios_hal::io::IoError::OutOfBounds`].
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> impl Future<Output = IoResult<()>> + Send;
}
