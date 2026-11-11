//! The device, the superblock chosen for it, and the chunk map: everything
//! needed to turn a logical address into verified bytes.

use crate::chunk::ChunkMap;
use crate::error::Error;
use crate::source::BlockSource;
use crate::superblock::Superblock;

/// A mounted device's address space.
///
/// Immutable once built: every method takes `&self`, holds no lock and may
/// run on any number of processors at once.
pub(crate) struct Volume<S> {
    source: S,
    superblock: Superblock,
    chunks: ChunkMap,
}

impl<S: BlockSource> Volume<S> {
    pub(crate) fn new(source: S, superblock: Superblock, chunks: ChunkMap) -> Self {
        Self {
            source,
            superblock,
            chunks,
        }
    }

    pub(crate) fn superblock(&self) -> &Superblock {
        &self.superblock
    }

    pub(crate) fn chunks(&self) -> &ChunkMap {
        &self.chunks
    }

    /// Swaps in the chunk map read from the chunk tree, returning the
    /// bootstrap map it was read through.
    pub(crate) fn replace_chunks(&mut self, chunks: ChunkMap) -> ChunkMap {
        core::mem::replace(&mut self.chunks, chunks)
    }

    /// Reads `buf.len()` bytes at `logical` into `buf` and runs `check`
    /// over them, trying each copy of the chunk in turn until one passes.
    ///
    /// Fails with the first copy's error when no copy passes: the primary
    /// copy's defect is the one a reader of the format expects to hear
    /// about.
    pub(crate) async fn read_verified<T>(
        &self,
        logical: u64,
        buf: &mut [u8],
        check: impl Fn(&[u8]) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let copies = self.chunks.map(logical, buf.len() as u64)?;
        let mut first_error = None;
        for physical in copies {
            let outcome = match self.source.read_at(physical, buf).await {
                Ok(()) => check(buf),
                Err(source) => Err(Error::Io {
                    offset: physical,
                    len: buf.len(),
                    source,
                }),
            };
            match outcome {
                Ok(value) => return Ok(value),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        Err(first_error.unwrap_or(Error::Unmapped {
            logical,
            len: buf.len() as u64,
        }))
    }
}
