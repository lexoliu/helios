//! The data half of log replay: file extents with their extent-tree
//! references, block-group usage and checksums, after `replay_one_extent` in
//! `fs/btrfs/tree-log.c` and `btrfs_drop_extents` in `fs/btrfs/file.c`
//! (v7.3).

use alloc::vec::Vec;
use core::ops::Range;

use super::{LogReplayError, Walk, log_defect};
use crate::accounting::DataChange;
use crate::allocation::data::DataReference;
use crate::checksums;
use crate::items::{ExtentData, FileExtent, INODE_NODATASUM};
use crate::ondisk::{RawFileExtentBody, RawFileExtentHead, item_type, objectid, view};
use crate::source::BlockSink;
use crate::space::SpaceError;
use crate::transaction::Transaction;
use crate::transaction::file::aligned_end;
use crate::tree::Cursor;
use crate::tree::write::WriteError;
use crate::{Error, ItemDefect, Key};

const HEAD_BYTES: usize = size_of::<RawFileExtentHead>();
/// `BTRFS_FEATURE_INCOMPAT_NO_HOLES`, from `btrfs.h`.
const INCOMPAT_NO_HOLES: u64 = 1 << 9;
/// `BTRFS_FILE_EXTENT_INLINE` and `BTRFS_FILE_EXTENT_REG`.
const EXTENT_INLINE: u8 = 0;
const EXTENT_REGULAR: u8 = 1;

impl<S: BlockSink> Transaction<S> {
    /// Replays one file extent item of the log: `replay_one_extent`. The
    /// extents it overlaps are dropped; a regular or preallocated extent
    /// takes a reference on its data extent, which an extent item the
    /// extent tree lacks is created for at its logged address, and its
    /// checksums are copied from the log; the inode's byte count follows.
    pub(super) async fn replay_extent(
        &mut self,
        walk: &Walk,
        key: Key,
        item: &[u8],
    ) -> Result<(), WriteError> {
        let tree = walk.tree;
        let inode = key.objectid;
        let sector = u64::from(self.volume.superblock().sectorsize());
        let parsed = FileExtent::parse(key, item, sector as u32).map_err(log_defect(key))?;
        match parsed.data {
            ExtentData::Inline {
                compression: Some(algorithm),
                ..
            }
            | ExtentData::Regular {
                compression: Some(algorithm),
                ..
            } => self.image.enable_compression(algorithm),
            _ => {}
        }
        let head = *view::<RawFileExtentHead>(item).expect("validated extent head");
        let start = key.offset;
        let inline = head.extent_type == EXTENT_INLINE;
        let body = (!inline)
            .then(|| *view::<RawFileExtentBody>(&item[HEAD_BYTES..]).expect("validated body"));
        let (end, nbytes) = match &body {
            Some(body) => (
                start
                    .checked_add(body.num_bytes.get())
                    .ok_or(WriteError::FileOverflow)?,
                if body.disk_bytenr.get() == 0 {
                    0
                } else {
                    body.num_bytes.get()
                },
            ),
            None => {
                let ram = head.ram_bytes.get();
                (aligned_end(start + ram, sector)?, ram)
            }
        };
        let (_, metadata) = self.inode_item(tree, inode).await?;
        let root = self.roots.get(tree)?;
        if !inline
            && self
                .trees
                .get(&self.volume, root, key)
                .await?
                .is_some_and(|existing| existing.as_slice() == item)
        {
            // The subvolume already maps this exact extent here.
            return Ok(());
        }
        let found = self.drop_extents(walk, inode, start..end).await?;
        if let Some(body) = body {
            let disk = body.disk_bytenr.get();
            let holes = self.volume.superblock().incompat_flags() & INCOMPAT_NO_HOLES != 0;
            if disk != 0 || !holes {
                self.put_item(tree, key, Some(&item.to_vec())).await?;
            }
            if disk != 0 {
                self.reference_logged_extent(walk, key, body).await?;
                if head.extent_type == EXTENT_REGULAR && metadata.flags & INODE_NODATASUM == 0 {
                    let range = if head.compression != 0 {
                        disk..disk + body.disk_num_bytes.get()
                    } else {
                        let first = disk + body.offset.get();
                        first..first + body.num_bytes.get()
                    };
                    for (logical, digests) in self.logged_checksums(walk, range).await? {
                        self.accounting
                            .store_checksums(
                                &self.volume,
                                &mut self.trees,
                                &mut self.space,
                                logical,
                                &digests,
                            )
                            .await?;
                    }
                }
            }
        } else {
            self.overwrite_item(walk, key, item).await?;
        }
        let (mut raw, _) = self.inode_item(tree, inode).await?;
        raw.nbytes.set(
            raw.nbytes
                .get()
                .checked_sub(found)
                .and_then(|bytes| bytes.checked_add(nbytes))
                .ok_or(WriteError::FileOverflow)?,
        );
        self.update_inode(tree, inode, raw).await
    }

    /// Takes the logged extent's reference on its data extent: an added
    /// reference when the extent tree holds it (`btrfs_inc_extent_ref`), the
    /// extent itself otherwise (`btrfs_alloc_logged_file_extent`).
    async fn reference_logged_extent(
        &mut self,
        walk: &Walk,
        key: Key,
        body: RawFileExtentBody,
    ) -> Result<(), WriteError> {
        let extent = Key::new(
            body.disk_bytenr.get(),
            item_type::EXTENT_ITEM,
            body.disk_num_bytes.get(),
        );
        let reference = DataReference {
            root: walk.tree,
            inode: key.objectid,
            offset: key.offset.wrapping_sub(body.offset.get()),
        };
        // The item's insertion may have copied a shared leaf; its references
        // convert before this one changes.
        self.accounting
            .sync_references(&self.volume, &mut self.trees, &mut self.space)
            .await?;
        if self
            .trees
            .get(&self.volume, self.accounting.root, extent)
            .await?
            .is_some()
        {
            self.accounting
                .update_data(
                    &self.volume,
                    &mut self.trees,
                    &mut self.space,
                    DataChange {
                        key: extent,
                        reference,
                        delta: 1,
                    },
                )
                .await?;
            return Ok(());
        }
        // Linux never allocates data over a superblock copy, so a log that
        // claims one is damaged.
        let range = extent.objectid..extent.objectid.saturating_add(extent.offset);
        if self
            .volume
            .chunks()
            .superblock_stripes()
            .iter()
            .any(|stripe| stripe.start < range.end && range.start < stripe.end)
        {
            return Err(SpaceError::NotFree {
                address: extent.objectid,
                bytes: extent.offset,
            }
            .into());
        }
        self.accounting
            .adopt_data(
                &self.volume,
                &mut self.trees,
                &mut self.space,
                extent,
                reference,
            )
            .await
    }

    /// The checksums the log holds for the sectors of `range`, as runs of
    /// consecutive sectors: `btrfs_lookup_csums_list` on the log.
    async fn logged_checksums(
        &self,
        walk: &Walk,
        range: Range<u64>,
    ) -> Result<Vec<(u64, Vec<u8>)>, WriteError> {
        if range.is_empty() {
            return Ok(Vec::new());
        }
        let sector = u64::from(self.volume.superblock().sectorsize());
        let digest = self.volume.superblock().csum_type().size();
        let first = checksums::key(range.start);
        let nodes = self.log_nodes(walk.newest);
        let mut cursor = Cursor::floor(&nodes, walk.log, first).await?;
        let mut runs = Vec::new();
        while let (Some(key), Some(bytes)) = (cursor.key(), cursor.item()) {
            if !key.is(objectid::EXTENT_CSUM, item_type::EXTENT_CSUM) {
                if key < first {
                    cursor.advance(&nodes).await?;
                    continue;
                }
                break;
            }
            if key.offset >= range.end {
                break;
            }
            if bytes.is_empty() || !bytes.len().is_multiple_of(digest) {
                return Err(log_defect(key)(ItemDefect::ChecksumItem(bytes.len())).into());
            }
            if !key.offset.is_multiple_of(sector) {
                return Err(log_defect(key)(ItemDefect::Unaligned {
                    field: "checksum offset",
                    value: key.offset,
                })
                .into());
            }
            let item_end = ((bytes.len() / digest) as u64)
                .checked_mul(sector)
                .and_then(|length| key.offset.checked_add(length))
                .ok_or_else(|| log_defect(key)(ItemDefect::ChecksumItem(bytes.len())))?;
            if item_end > range.start {
                let start = range.start.max(key.offset);
                let stop = range.end.min(item_end);
                let from = ((start - key.offset) / sector) as usize * digest;
                let to = ((stop - key.offset) / sector) as usize * digest;
                runs.push((start, bytes[from..to].to_vec()));
            }
            cursor.advance(&nodes).await?;
        }
        Ok(runs)
    }

    /// Drops the mappings of `inode` in `range`, splitting the extents it
    /// cuts, and returns the bytes of data extents they referenced:
    /// `btrfs_drop_extents` with `update_refs` set, as for any subvolume. An
    /// inline extent is dropped only whole.
    pub(super) async fn drop_extents(
        &mut self,
        walk: &Walk,
        inode: u64,
        range: Range<u64>,
    ) -> Result<u64, WriteError> {
        let tree = walk.tree;
        let sector = self.volume.superblock().sectorsize();
        let mut found = 0_u64;
        let mut next = range.start;
        while next < range.end {
            let root = self.roots.get(tree)?;
            let at = Key::new(inode, item_type::EXTENT_DATA, next);
            let floor = self.trees.floor(&self.volume, root, at).await?;
            let candidate = match floor {
                Some((key, bytes))
                    if key.is(inode, item_type::EXTENT_DATA)
                        && extent_end(tree, key, &bytes, sector)? > next =>
                {
                    Some((key, bytes))
                }
                _ => self.trees.ceiling(&self.volume, root, at).await?,
            };
            let Some((key, bytes)) = candidate else {
                break;
            };
            if !key.is(inode, item_type::EXTENT_DATA) || key.offset >= range.end {
                break;
            }
            let end = extent_end(tree, key, &bytes, sector)?;
            let head = view::<RawFileExtentHead>(&bytes).expect("validated extent head");
            if head.extent_type == EXTENT_INLINE {
                if range.start > key.offset || range.end < end {
                    return Err(LogReplayError::InlineOverlap {
                        subvolume: tree,
                        inode,
                        start: range.start,
                        end: range.end,
                    }
                    .into());
                }
                found = found
                    .checked_add(head.ram_bytes.get())
                    .ok_or(WriteError::FileOverflow)?;
                self.put_item(tree, key, None).await?;
            } else if end == key.offset {
                // A zero-length extent an old kernel left on ENOSPC goes
                // whole, with its reference.
                self.drop_empty_extent(tree, key, &bytes).await?;
            } else {
                found = found
                    .checked_add(self.cut_regular_extent(tree, key, range.clone()).await?)
                    .ok_or(WriteError::FileOverflow)?;
            }
            next = end.max(key.offset.saturating_add(1));
        }
        Ok(found)
    }

    /// Deletes a regular extent item mapping no bytes, dropping its
    /// reference.
    async fn drop_empty_extent(
        &mut self,
        tree: u64,
        key: Key,
        bytes: &[u8],
    ) -> Result<(), WriteError> {
        let body = *view::<RawFileExtentBody>(&bytes[HEAD_BYTES..]).expect("validated body");
        self.put_item(tree, key, None).await?;
        self.accounting
            .sync_references(&self.volume, &mut self.trees, &mut self.space)
            .await?;
        if body.disk_bytenr.get() == 0 {
            return Ok(());
        }
        self.accounting
            .update_data(
                &self.volume,
                &mut self.trees,
                &mut self.space,
                DataChange {
                    key: Key::new(
                        body.disk_bytenr.get(),
                        item_type::EXTENT_ITEM,
                        body.disk_num_bytes.get(),
                    ),
                    reference: DataReference {
                        root: tree,
                        inode: key.objectid,
                        offset: key.offset.wrapping_sub(body.offset.get()),
                    },
                    delta: -1,
                },
            )
            .await?;
        Ok(())
    }
}

/// Where the subvolume extent item `key` ends: past its mapped bytes, or an
/// inline extent past its data, unaligned, as `btrfs_drop_extents` compares
/// it.
fn extent_end(tree: u64, key: Key, bytes: &[u8], sector: u32) -> Result<u64, Error> {
    let extent = FileExtent::parse(key, bytes, sector).map_err(|defect| Error::Item {
        tree,
        key,
        defect,
    })?;
    Ok(match extent.data {
        ExtentData::Inline { .. } => key.offset + extent.end,
        _ => extent.end,
    })
}
