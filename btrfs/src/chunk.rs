//! The logical address space: chunks, and the device ranges behind them.

use alloc::vec::Vec;

use arrayvec::ArrayVec;

use crate::error::{ChunkDefect, Error};
use crate::key::Key;
use crate::ondisk::{
    RawChunk, RawKey, RawStripe, STRIPE_LEN, SYS_CHUNK_ARRAY_OFFSET, item_type, objectid, view,
};
use crate::superblock::Superblock;

const BLOCK_GROUP_DATA: u64 = 1 << 0;
const BLOCK_GROUP_SYSTEM: u64 = 1 << 1;
const BLOCK_GROUP_METADATA: u64 = 1 << 2;
const BLOCK_GROUP_DUP: u64 = 1 << 5;

/// Profiles that spread a chunk over several devices, or remap it, and
/// their names. `dup`, two copies on one device, is the only profile bit a
/// single-device filesystem reads.
const MULTI_DEVICE_PROFILES: [(u64, &str); 8] = [
    (1 << 3, "raid0"),
    (1 << 4, "raid1"),
    (1 << 6, "raid10"),
    (1 << 7, "raid5"),
    (1 << 8, "raid6"),
    (1 << 9, "raid1c3"),
    (1 << 10, "raid1c4"),
    (1 << 11, "remapped"),
];

/// Copies a chunk keeps on the one device: one for `single`, two for `dup`.
pub(crate) const MAX_COPIES: usize = 2;

/// Why a chunk record was not accepted.
enum ChunkRefusal {
    Defect(ChunkDefect),
    Profile(&'static str),
}

impl From<ChunkDefect> for ChunkRefusal {
    fn from(defect: ChunkDefect) -> Self {
        Self::Defect(defect)
    }
}

/// One chunk: a logical range and the device offsets holding each copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Chunk {
    logical: u64,
    length: u64,
    copies: ArrayVec<u64, MAX_COPIES>,
}

impl Chunk {
    /// Parses and validates the chunk at the head of `bytes`, keyed `key`.
    /// Returns the chunk and the bytes it occupied.
    fn parse(
        key: Key,
        bytes: &[u8],
        superblock: &Superblock,
    ) -> Result<(Self, usize), ChunkRefusal> {
        if !key.is(objectid::FIRST_CHUNK_TREE, item_type::CHUNK_ITEM) {
            return Err(ChunkDefect::Key.into());
        }
        let logical = key.offset;
        let raw: &RawChunk = view(bytes).ok_or(ChunkDefect::Truncated)?;
        let stripes = raw.num_stripes.get();
        let consumed = size_of::<RawChunk>() + usize::from(stripes) * size_of::<RawStripe>();
        if bytes.len() < consumed {
            return Err(ChunkDefect::Truncated.into());
        }

        let sector = u64::from(superblock.sectorsize());
        let length = raw.length.get();
        if length == 0 || !length.is_multiple_of(sector) {
            return Err(ChunkDefect::Length(length).into());
        }
        if !logical.is_multiple_of(sector) {
            return Err(ChunkDefect::LogicalAlignment.into());
        }
        if logical.checked_add(length).is_none() {
            return Err(ChunkDefect::Overflow.into());
        }
        if raw.stripe_len.get() != STRIPE_LEN {
            return Err(ChunkDefect::StripeLength(raw.stripe_len.get()).into());
        }

        let chunk_type = raw.chunk_type.get();
        let kinds = chunk_type & (BLOCK_GROUP_DATA | BLOCK_GROUP_SYSTEM | BLOCK_GROUP_METADATA);
        let mixed = BLOCK_GROUP_DATA | BLOCK_GROUP_METADATA;
        let kind_valid = kinds == BLOCK_GROUP_DATA
            || kinds == BLOCK_GROUP_SYSTEM
            || kinds == BLOCK_GROUP_METADATA
            || kinds == mixed;
        if !kind_valid {
            return Err(ChunkDefect::Kind(chunk_type).into());
        }
        if let Some((_, profile)) = MULTI_DEVICE_PROFILES
            .iter()
            .find(|(bit, _)| chunk_type & bit != 0)
        {
            return Err(ChunkRefusal::Profile(profile));
        }
        let copies_expected = if chunk_type & BLOCK_GROUP_DUP != 0 {
            2
        } else {
            1
        };
        if stripes != copies_expected {
            return Err(ChunkDefect::StripeCount {
                chunk_type,
                stripes,
            }
            .into());
        }

        let mut copies = ArrayVec::new();
        let stripe_bytes = &bytes[size_of::<RawChunk>()..consumed];
        for stripe in stripe_bytes.chunks_exact(size_of::<RawStripe>()) {
            let stripe: &RawStripe = view(stripe).ok_or(ChunkDefect::Truncated)?;
            let devid = stripe.devid.get();
            if devid != superblock.devid() {
                return Err(ChunkDefect::Device(devid).into());
            }
            let physical = stripe.offset.get();
            let in_device = physical
                .checked_add(length)
                .is_some_and(|end| end <= superblock.device_bytes());
            if !physical.is_multiple_of(sector) || !in_device {
                return Err(ChunkDefect::Stripe(physical).into());
            }
            copies.push(physical);
        }
        Ok((
            Self {
                logical,
                length,
                copies,
            },
            consumed,
        ))
    }

    fn end(&self) -> u64 {
        // `parse` refused a range that overflows.
        self.logical + self.length
    }
}

/// The whole logical-to-physical map of a single-device filesystem.
///
/// Built once at mount and never changed afterwards, so lookups take no
/// lock and run on any number of processors at once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChunkMap {
    /// Sorted by logical start, non-overlapping.
    chunks: Vec<Chunk>,
}

impl ChunkMap {
    /// The bootstrap map: the chunks the superblock carries so that the
    /// chunk tree itself can be read.
    pub(crate) fn from_sys_chunk_array(
        superblock: &Superblock,
        superblock_bytes: &[u8],
    ) -> Result<Self, Error> {
        let array = &superblock_bytes[SYS_CHUNK_ARRAY_OFFSET..][..superblock.sys_chunk_array_len()];
        let mut map = Self::default();
        let mut offset = 0;
        while offset < array.len() {
            let rest = &array[offset..];
            let refused = |refusal| match refusal {
                ChunkRefusal::Defect(defect) => Error::SysChunkArray { offset, defect },
                ChunkRefusal::Profile(profile) => Error::UnsupportedProfile {
                    logical: view::<RawKey>(rest).map_or(0, |key| key.offset.get()),
                    profile,
                },
            };
            let raw_key: &RawKey = view(rest).ok_or(refused(ChunkDefect::Truncated.into()))?;
            let key = Key::from_raw(raw_key);
            let (chunk, consumed) =
                Chunk::parse(key, &rest[size_of::<RawKey>()..], superblock).map_err(refused)?;
            map.insert(chunk).map_err(|defect| refused(defect.into()))?;
            offset += size_of::<RawKey>() + consumed;
        }
        Ok(map)
    }

    /// Adds a chunk item from the chunk tree, which presents them in
    /// ascending logical order.
    pub(crate) fn insert_item(
        &mut self,
        key: Key,
        item: &[u8],
        superblock: &Superblock,
    ) -> Result<(), Error> {
        let logical = key.offset;
        let refused = |refusal| match refusal {
            ChunkRefusal::Defect(defect) => Error::Chunk { logical, defect },
            ChunkRefusal::Profile(profile) => Error::UnsupportedProfile { logical, profile },
        };
        let (chunk, _) = Chunk::parse(key, item, superblock).map_err(refused)?;
        self.insert(chunk).map_err(|defect| refused(defect.into()))
    }

    fn insert(&mut self, chunk: Chunk) -> Result<(), ChunkDefect> {
        if let Some(last) = self.chunks.last()
            && chunk.logical < last.end()
        {
            return Err(ChunkDefect::Overlap);
        }
        self.chunks.push(chunk);
        Ok(())
    }

    /// Checks that every bootstrap chunk appears in this map, the one the
    /// chunk tree describes, with the same range and copies.
    pub(crate) fn check_contains(&self, bootstrap: &Self) -> Result<(), Error> {
        for chunk in &bootstrap.chunks {
            if self.find(chunk.logical) != Some(chunk) {
                return Err(Error::Chunk {
                    logical: chunk.logical,
                    defect: ChunkDefect::Mismatch,
                });
            }
        }
        Ok(())
    }

    fn find(&self, logical: u64) -> Option<&Chunk> {
        let index = self.chunks.partition_point(|chunk| chunk.end() <= logical);
        self.chunks
            .get(index)
            .filter(|chunk| chunk.logical <= logical)
    }

    /// The device offset of every copy of `[logical, logical + len)`, which
    /// must lie inside one chunk.
    pub(crate) fn map(&self, logical: u64, len: u64) -> Result<ArrayVec<u64, MAX_COPIES>, Error> {
        let unmapped = Error::Unmapped { logical, len };
        let chunk = self.find(logical).ok_or(unmapped)?;
        let end = logical.checked_add(len).ok_or(unmapped)?;
        if end > chunk.end() {
            return Err(unmapped);
        }
        let delta = logical - chunk.logical;
        Ok(chunk
            .copies
            .iter()
            .map(|physical| physical + delta)
            .collect())
    }
}
