//! Transaction-owned checksum-tree edits. A checksum item can cover several
//! allocations; deleting one allocation preserves both neighboring ranges.

use alloc::vec::Vec;
use core::ops::Range;

use crate::Key;
use crate::ondisk::{RawHeader, RawItem, item_type, objectid};
use crate::source::BlockSource;
use crate::space::Space;
use crate::tree::TreeRoot;
use crate::tree::write::{Trees, WriteError};
use crate::volume::Volume;

#[derive(Clone)]
pub(crate) struct Checksums {
    pub(crate) root: TreeRoot,
}

impl Checksums {
    /// Store checksums of complete on-disk sectors, including any compressed
    /// extent padding. The caller stages those same bytes before publication.
    pub(crate) async fn set<S: BlockSource>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
        logical: u64,
        data: &[u8],
    ) -> Result<(), WriteError> {
        let sector = volume.superblock().sectorsize() as usize;
        let algorithm = volume.superblock().csum_type();
        let mut digests = Vec::with_capacity(data.len() / sector * algorithm.size());
        for sector in data.chunks_exact(sector) {
            digests.extend_from_slice(algorithm.digest(sector).as_bytes());
        }
        self.store(volume, trees, space, logical, &digests).await
    }

    /// Replaces the checksums of the sectors from `logical` with `digests`,
    /// one digest per sector, splitting them into items that fit a leaf.
    pub(crate) async fn store<S: BlockSource>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
        logical: u64,
        digests: &[u8],
    ) -> Result<(), WriteError> {
        let sector = u64::from(volume.superblock().sectorsize());
        let digest = volume.superblock().csum_type().size();
        let invalid = || WriteError::Checksum { key: key(logical) };
        if digests.is_empty() || !digests.len().is_multiple_of(digest) {
            return Err(invalid());
        }
        let end = ((digests.len() / digest) as u64)
            .checked_mul(sector)
            .and_then(|length| logical.checked_add(length))
            .ok_or_else(invalid)?;
        self.remove(volume, trees, space, logical..end).await?;
        let per_item = (volume.superblock().nodesize() as usize
            - size_of::<RawHeader>()
            - size_of::<RawItem>())
            / digest;
        for (index, chunk) in digests.chunks(per_item * digest).enumerate() {
            let start = logical + (index * per_item) as u64 * sector;
            self.root = trees
                .apply(volume, self.root, key(start), Some(chunk), space)
                .await?;
        }
        Ok(())
    }

    /// Missing checksums are legal for preallocated or NODATASUM extents.
    /// Any failure aborts the transaction, including its earlier tree edits.
    pub(crate) async fn remove<S: BlockSource>(
        &mut self,
        volume: &Volume<S>,
        trees: &mut Trees,
        space: &mut Space,
        range: Range<u64>,
    ) -> Result<(), WriteError> {
        let sector = u64::from(volume.superblock().sectorsize());
        let digest = volume.superblock().csum_type().size();
        let first = key(range.start);
        if range.start >= range.end
            || !range.start.is_multiple_of(sector)
            || !range.end.is_multiple_of(sector)
        {
            return Err(WriteError::Checksum { key: first });
        }
        let mut current = trees
            .floor(volume, self.root, first)
            .await?
            .filter(|(key, _)| key.is(objectid::EXTENT_CSUM, item_type::EXTENT_CSUM));
        if current.is_none() {
            current = trees.ceiling(volume, self.root, first).await?;
        }
        while let Some((item, bytes)) = current {
            if !item.is(objectid::EXTENT_CSUM, item_type::EXTENT_CSUM) || item.offset >= range.end {
                break;
            }
            let error = || WriteError::Checksum { key: item };
            if bytes.is_empty()
                || !bytes.len().is_multiple_of(digest)
                || !item.offset.is_multiple_of(sector)
            {
                return Err(error());
            }
            let length = (bytes.len() / digest) as u64;
            let end = length
                .checked_mul(sector)
                .and_then(|length| item.offset.checked_add(length))
                .ok_or_else(error)?;
            let next = trees
                .ceiling(volume, self.root, key(item.offset + 1))
                .await?;
            if next.as_ref().is_some_and(|(key, _)| {
                key.is(objectid::EXTENT_CSUM, item_type::EXTENT_CSUM) && key.offset < end
            }) {
                return Err(error());
            }
            if end > range.start {
                let start = range.start.max(item.offset);
                let stop = range.end.min(end);
                let prefix = ((start - item.offset) / sector) as usize * digest;
                let suffix = ((stop - item.offset) / sector) as usize * digest;
                self.root = trees
                    .apply(
                        volume,
                        self.root,
                        item,
                        (prefix != 0).then_some(&bytes[..prefix]),
                        space,
                    )
                    .await?;
                if suffix < bytes.len() {
                    self.root = trees
                        .apply(volume, self.root, key(stop), Some(&bytes[suffix..]), space)
                        .await?;
                }
            }
            current = next;
        }
        Ok(())
    }
}

pub(crate) fn key(logical: u64) -> Key {
    Key::new(objectid::EXTENT_CSUM, item_type::EXTENT_CSUM, logical)
}
