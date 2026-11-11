//! The mounted filesystem: inodes, directories, file data, symlinks and
//! extended attributes.

use alloc::vec;
use alloc::vec::Vec;
use core::ops::ControlFlow;

use crate::chunk::ChunkMap;
use crate::compress::decompress;
use crate::csum::name_hash;
use crate::error::{Error, FileKind, ItemDefect};
use crate::items::{
    DirRecord, EntryTarget, ExtentData, FileExtent, INODE_NODATASUM, Metadata, RecordKind,
    RootRecord, dir_index_record, dir_records,
};
use crate::key::Key;
use crate::ondisk::{MAX_COMPRESSED_EXTENT, NAME_MAX, SYMLINK_MAX, item_type, objectid};
use crate::source::BlockSource;
use crate::superblock::{self, Superblock};
use crate::tree::{Cursor, TreeRoot};
use crate::volume::Volume;

/// Most symlinks one path resolution follows, as Linux's `MAXSYMLINKS`.
pub const MAX_SYMLINK_FOLLOWS: usize = 40;

/// Largest window of file data read and checksummed in one request.
const DATA_WINDOW: u64 = MAX_COMPRESSED_EXTENT;

/// A btrfs filesystem mounted read-only from one device.
///
/// # Concurrency
///
/// Mounting builds everything the filesystem needs to find data — the
/// verified superblock, the chunk map, the roots of the root, checksum and
/// default-subvolume trees — and none of it changes afterwards. Every
/// operation takes `&self`, holds no lock, keeps its walk state in its own
/// future, and may run on any processor concurrently with any other; the
/// only shared resource is the [`BlockSource`], whose reads are themselves
/// independent. Each walk allocates the tree blocks on its path (at most
/// eight, each one node size) and a bounded data window, and frees them
/// when it finishes.
///
/// An [`Inode`] records the tree root of the commit it was read from, so
/// operations on it see that commit's state whatever happens afterwards.
pub struct Filesystem<S> {
    volume: Volume<S>,
    root_tree: TreeRoot,
    csum_tree: TreeRoot,
    default_subvolume: RootRecord,
}

/// An inode, as of the commit it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inode {
    root: TreeRoot,
    number: u64,
    metadata: Metadata,
}

impl Inode {
    /// The id of the subvolume tree the inode lives in.
    pub fn tree(&self) -> u64 {
        self.root.tree
    }

    /// The inode number within its subvolume.
    pub fn number(&self) -> u64 {
        self.number
    }

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    pub fn kind(&self) -> FileKind {
        self.metadata.kind
    }

    fn expect_kind(&self, expected: FileKind) -> Result<(), Error> {
        if self.metadata.kind != expected {
            return Err(Error::WrongKind {
                tree: self.root.tree,
                inode: self.number,
                expected,
                found: self.metadata.kind,
            });
        }
        Ok(())
    }

    fn is_checksummed(&self) -> bool {
        self.metadata.flags & INODE_NODATASUM == 0
    }
}

/// A place in a directory listing to resume from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DirPosition(pub u64);

impl DirPosition {
    /// The start of a listing.
    pub const START: Self = Self(0);
}

/// One entry of a directory listing. `.` and `..` are not stored by the
/// format and are not listed.
#[derive(Clone, Copy, Debug)]
pub struct DirEntry<'a> {
    pub name: &'a [u8],
    pub kind: FileKind,
    pub target: EntryTarget,
    /// Where this entry sits in the listing.
    pub position: DirPosition,
}

impl<S: BlockSource> Filesystem<S> {
    /// Mounts the filesystem on `source`.
    ///
    /// Chooses and verifies the superblock, refuses features this crate
    /// does not implement, reads the chunk tree through the bootstrap
    /// chunks the superblock carries and checks the two agree, and finds
    /// the checksum tree and the default subvolume.
    pub async fn mount(source: S) -> Result<Self, Error> {
        let selected = superblock::select(&source).await?;
        let superblock = selected.superblock;
        superblock.check_supported()?;
        let bootstrap = ChunkMap::from_sys_chunk_array(&superblock, &selected.bytes)?;
        let chunk_root = superblock.chunk_tree();
        let root_tree = TreeRoot {
            tree: objectid::ROOT_TREE,
            bytenr: superblock.root_tree().bytenr,
            level: superblock.root_tree().level,
            generation: superblock.generation(),
        };
        let mut volume = Volume::new(source, superblock, bootstrap);

        let chunks = read_chunk_tree(
            &volume,
            TreeRoot {
                tree: objectid::CHUNK_TREE,
                bytenr: chunk_root.bytenr,
                level: chunk_root.level,
                generation: chunk_root.generation,
            },
        )
        .await?;
        let bootstrap = volume.replace_chunks(chunks);
        volume.chunks().check_contains(&bootstrap)?;

        let csum_tree = find_root(&volume, root_tree, objectid::CSUM_TREE)
            .await?
            .root;
        let default_tree = default_subvolume(&volume, root_tree).await?;
        let default_subvolume = find_root(&volume, root_tree, default_tree).await?;
        Ok(Self {
            volume,
            root_tree,
            csum_tree,
            default_subvolume,
        })
    }

    pub fn superblock(&self) -> &Superblock {
        self.volume.superblock()
    }

    /// The id of the subvolume a mount lands in.
    pub fn default_subvolume(&self) -> u64 {
        self.default_subvolume.root.tree
    }

    /// The root directory of the default subvolume.
    pub async fn root(&self) -> Result<Inode, Error> {
        self.load_inode(
            self.default_subvolume.root,
            self.default_subvolume.root_dirid,
        )
        .await
    }

    /// The root directory of subvolume `tree`.
    pub async fn subvolume_root(&self, tree: u64) -> Result<Inode, Error> {
        let record = find_root(&self.volume, self.root_tree, tree).await?;
        self.load_inode(record.root, record.root_dirid).await
    }

    async fn load_inode(&self, root: TreeRoot, number: u64) -> Result<Inode, Error> {
        let key = Key::new(number, item_type::INODE_ITEM, 0);
        let cursor = Cursor::ceiling(&self.volume, root, key).await?;
        let (Some(found), Some(item)) = (cursor.key(), cursor.item()) else {
            return Err(Error::MissingInode {
                tree: root.tree,
                inode: number,
            });
        };
        if found != key {
            return Err(Error::MissingInode {
                tree: root.tree,
                inode: number,
            });
        }
        let metadata = Metadata::parse(item).map_err(|defect| Error::Item {
            tree: root.tree,
            key,
            defect,
        })?;
        Ok(Inode {
            root,
            number,
            metadata,
        })
    }

    async fn open_target(&self, dir: &Inode, target: EntryTarget) -> Result<Inode, Error> {
        match target {
            EntryTarget::Inode(number) => self.load_inode(dir.root, number).await,
            EntryTarget::Subvolume(tree) => self.subvolume_root(tree).await,
        }
    }

    /// Opens what directory entry `entry` of `dir` names.
    pub async fn open_entry(&self, dir: &Inode, entry: &DirEntry<'_>) -> Result<Inode, Error> {
        self.open_target(dir, entry.target).await
    }

    /// Looks `name` up in directory `dir`. `None` when no entry has that
    /// name; `.` and `..` are not stored and are never found.
    pub async fn lookup(&self, dir: &Inode, name: &[u8]) -> Result<Option<Inode>, Error> {
        dir.expect_kind(FileKind::Directory)?;
        if name.is_empty() || name.len() > NAME_MAX {
            return Ok(None);
        }
        let key = Key::new(dir.number, item_type::DIR_ITEM, name_hash(name));
        let target = {
            let cursor = Cursor::ceiling(&self.volume, dir.root, key).await?;
            find_record(&cursor, key, name, RecordKind::Entry)?.map(|record| record.target())
        };
        match target {
            Some(target) => self.open_target(dir, target).await.map(Some),
            None => Ok(None),
        }
    }

    /// Lists directory `dir` from `from`, handing each entry to `visit`.
    ///
    /// `visit` returns [`ControlFlow::Continue`] to take the entry and go
    /// on, or [`ControlFlow::Break`] to stop without taking it. Returns the
    /// position to resume from when `visit` stopped the listing, `None`
    /// once every entry has been visited.
    pub async fn read_dir<F>(
        &self,
        dir: &Inode,
        from: DirPosition,
        mut visit: F,
    ) -> Result<Option<DirPosition>, Error>
    where
        F: FnMut(DirEntry<'_>) -> ControlFlow<()>,
    {
        dir.expect_kind(FileKind::Directory)?;
        let start = Key::new(dir.number, item_type::DIR_INDEX, from.0);
        let mut cursor = Cursor::ceiling(&self.volume, dir.root, start).await?;
        while let (Some(key), Some(item)) = (cursor.key(), cursor.item()) {
            if !key.is(dir.number, item_type::DIR_INDEX) {
                break;
            }
            let record = dir_index_record(item).map_err(|defect| Error::Item {
                tree: dir.root.tree,
                key,
                defect,
            })?;
            let entry = DirEntry {
                name: record.name,
                kind: record.kind(),
                target: record.target(),
                position: DirPosition(key.offset),
            };
            if visit(entry).is_break() {
                return Ok(Some(DirPosition(key.offset)));
            }
            cursor.advance(&self.volume).await?;
        }
        Ok(None)
    }

    /// Reads file data at `offset` into `buf`. Returns the bytes read,
    /// fewer than `buf.len()` only at the end of the file.
    pub async fn read(&self, file: &Inode, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        file.expect_kind(FileKind::Regular)?;
        self.read_range(file, offset, buf).await
    }

    /// Reads the target of symlink `link` into `buf`. Returns the target's
    /// length; the target is complete when that is at most `buf.len()`.
    pub async fn read_link(&self, link: &Inode, buf: &mut [u8]) -> Result<usize, Error> {
        link.expect_kind(FileKind::Symlink)?;
        let size = link.metadata.size;
        if size == 0 || size > SYMLINK_MAX {
            return Err(Error::Item {
                tree: link.root.tree,
                key: Key::new(link.number, item_type::INODE_ITEM, 0),
                defect: ItemDefect::SymlinkSize(size),
            });
        }
        let len = size as usize;
        let take = len.min(buf.len());
        self.read_range(link, 0, &mut buf[..take]).await?;
        Ok(len)
    }

    /// Reads extended attribute `name` of `inode` into `buf`. Returns the
    /// value's length, `None` when the inode has no such attribute; the
    /// value is complete when its length is at most `buf.len()`.
    pub async fn xattr(
        &self,
        inode: &Inode,
        name: &[u8],
        buf: &mut [u8],
    ) -> Result<Option<usize>, Error> {
        if name.is_empty() || name.len() > NAME_MAX {
            return Ok(None);
        }
        let key = Key::new(inode.number, item_type::XATTR_ITEM, name_hash(name));
        let cursor = Cursor::ceiling(&self.volume, inode.root, key).await?;
        let Some(record) = find_record(&cursor, key, name, RecordKind::Xattr)? else {
            return Ok(None);
        };
        let take = record.data.len().min(buf.len());
        buf[..take].copy_from_slice(&record.data[..take]);
        Ok(Some(record.data.len()))
    }

    /// Hands the name of every extended attribute of `inode` to `visit`,
    /// until it returns [`ControlFlow::Break`].
    pub async fn list_xattrs<F>(&self, inode: &Inode, mut visit: F) -> Result<(), Error>
    where
        F: FnMut(&[u8]) -> ControlFlow<()>,
    {
        let start = Key::new(inode.number, item_type::XATTR_ITEM, 0);
        let mut cursor = Cursor::ceiling(&self.volume, inode.root, start).await?;
        while let (Some(key), Some(item)) = (cursor.key(), cursor.item()) {
            if !key.is(inode.number, item_type::XATTR_ITEM) {
                break;
            }
            for record in dir_records(item, RecordKind::Xattr) {
                let record = record.map_err(|defect| Error::Item {
                    tree: inode.root.tree,
                    key,
                    defect,
                })?;
                if visit(record.name).is_break() {
                    return Ok(());
                }
            }
            cursor.advance(&self.volume).await?;
        }
        Ok(())
    }

    /// Resolves `path` from the default subvolume's root directory,
    /// following symlinks, the last component's included, up to
    /// [`MAX_SYMLINK_FOLLOWS`] of them. An absolute symlink target restarts
    /// from that root; `..` never climbs above it.
    pub async fn resolve(&self, path: &[u8]) -> Result<Inode, Error> {
        let root = self.root().await?;
        let mut parents: Vec<Inode> = Vec::new();
        let mut current = root.clone();
        let mut remaining: Vec<u8> = path.to_vec();
        let mut cursor = 0;
        let mut component_index = 0;
        let mut follows = 0;
        while cursor < remaining.len() {
            let rest = &remaining[cursor..];
            let length = rest
                .iter()
                .position(|byte| *byte == b'/')
                .unwrap_or(rest.len());
            let component = &rest[..length];
            let next_cursor = cursor + length + 1;
            match component {
                b"" | b"." => {
                    cursor = next_cursor;
                    continue;
                }
                b".." => {
                    if let Some(parent) = parents.pop() {
                        current = parent;
                    }
                    cursor = next_cursor;
                    component_index += 1;
                    continue;
                }
                _ => {}
            }
            let found = self
                .lookup(&current, component)
                .await?
                .ok_or(Error::NotFound {
                    component: component_index,
                })?;
            component_index += 1;
            if found.kind() == FileKind::Symlink {
                follows += 1;
                if follows > MAX_SYMLINK_FOLLOWS {
                    return Err(Error::SymlinkLimit {
                        limit: MAX_SYMLINK_FOLLOWS,
                    });
                }
                let mut target = vec![0_u8; SYMLINK_MAX as usize];
                let len = self.read_link(&found, &mut target).await?;
                target.truncate(len);
                if target.first() == Some(&b'/') {
                    parents.clear();
                    current = root.clone();
                }
                target.push(b'/');
                if next_cursor < remaining.len() {
                    target.extend_from_slice(&remaining[next_cursor..]);
                }
                remaining = target;
                cursor = 0;
                continue;
            }
            parents.push(core::mem::replace(&mut current, found));
            cursor = next_cursor;
        }
        Ok(current)
    }

    /// The file bytes at `offset`, through the inode's extents.
    async fn read_range(&self, inode: &Inode, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let size = inode.metadata.size;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let len = (size - offset).min(buf.len() as u64) as usize;
        let buf = &mut buf[..len];
        let end = offset + len as u64;
        let first_extent = Key::new(inode.number, item_type::EXTENT_DATA, 0);
        let mut cursor = Cursor::floor(
            &self.volume,
            inode.root,
            Key::new(inode.number, item_type::EXTENT_DATA, offset),
        )
        .await?;
        let mut position = offset;
        let mut previous_end = 0;
        while position < end {
            let (Some(key), Some(item)) = (cursor.key(), cursor.item()) else {
                break;
            };
            if key < first_extent {
                cursor.advance(&self.volume).await?;
                continue;
            }
            if !key.is(inode.number, item_type::EXTENT_DATA) {
                break;
            }
            let item_error = |defect| Error::Item {
                tree: inode.root.tree,
                key,
                defect,
            };
            let extent =
                FileExtent::parse(key, item, self.superblock().sectorsize()).map_err(item_error)?;
            if extent.start < previous_end {
                return Err(item_error(ItemDefect::ExtentOverlap(previous_end)));
            }
            previous_end = extent.end;
            if extent.end > position {
                if extent.start > position {
                    // A hole the format leaves implicit.
                    let gap_end = extent.start.min(end);
                    buf[(position - offset) as usize..(gap_end - offset) as usize].fill(0);
                    position = gap_end;
                }
                if position < end {
                    let copy_end = extent.end.min(end);
                    let out = &mut buf[(position - offset) as usize..(copy_end - offset) as usize];
                    self.copy_extent(inode, &extent, position - extent.start, out)
                        .await?;
                    position = copy_end;
                }
            }
            cursor.advance(&self.volume).await?;
        }
        // Past the last extent the file reads as zeroes up to its size.
        buf[(position - offset) as usize..].fill(0);
        Ok(len)
    }

    /// Copies the bytes of `extent` from `from`, relative to its start,
    /// into `out`.
    async fn copy_extent(
        &self,
        inode: &Inode,
        extent: &FileExtent<'_>,
        from: u64,
        out: &mut [u8],
    ) -> Result<(), Error> {
        let sectorsize = self.superblock().sectorsize() as usize;
        let decompress_error = |algorithm, defect| Error::Decompress {
            algorithm,
            tree: inode.root.tree,
            inode: inode.number,
            file_offset: extent.start,
            defect,
        };
        match extent.data {
            ExtentData::Zero => out.fill(0),
            ExtentData::Inline {
                data,
                compression: None,
            } => {
                let from = from as usize;
                out.copy_from_slice(&data[from..from + out.len()]);
            }
            ExtentData::Inline {
                data,
                compression: Some(algorithm),
            } => {
                // `FileExtent::parse` bounded this by the sector size.
                let mut plain = vec![0_u8; extent.end as usize];
                decompress(algorithm, data, &mut plain, sectorsize)
                    .map_err(|defect| decompress_error(algorithm, defect))?;
                let from = from as usize;
                out.copy_from_slice(&plain[from..from + out.len()]);
            }
            ExtentData::Regular {
                disk_bytenr,
                offset,
                compression: None,
                ..
            } => {
                self.read_data(disk_bytenr + offset + from, out, inode.is_checksummed())
                    .await?;
            }
            ExtentData::Regular {
                disk_bytenr,
                disk_num_bytes,
                offset,
                ram_bytes,
                compression: Some(algorithm),
            } => {
                // Both sizes were bounded by 128 KiB in `FileExtent::parse`.
                let mut stored = vec![0_u8; disk_num_bytes as usize];
                self.read_sectors(disk_bytenr, &mut stored, inode.is_checksummed())
                    .await?;
                let mut plain = vec![0_u8; ram_bytes as usize];
                decompress(algorithm, &stored, &mut plain, sectorsize)
                    .map_err(|defect| decompress_error(algorithm, defect))?;
                let from = (offset + from) as usize;
                out.copy_from_slice(&plain[from..from + out.len()]);
            }
        }
        Ok(())
    }

    /// Reads data bytes at `logical`, which need not be sector aligned, in
    /// windows of at most [`DATA_WINDOW`] bytes.
    async fn read_data(
        &self,
        logical: u64,
        out: &mut [u8],
        checksummed: bool,
    ) -> Result<(), Error> {
        let sector = u64::from(self.superblock().sectorsize());
        let mut window = Vec::new();
        let mut done = 0;
        while done < out.len() {
            let at = logical + done as u64;
            let window_start = at - at % sector;
            let wanted_end = at + (out.len() - done) as u64;
            let window_end = wanted_end
                .next_multiple_of(sector)
                .min(window_start + DATA_WINDOW);
            window.resize((window_end - window_start) as usize, 0);
            self.read_sectors(window_start, &mut window, checksummed)
                .await?;
            let skip = (at - window_start) as usize;
            let count = (window.len() - skip).min(out.len() - done);
            out[done..done + count].copy_from_slice(&window[skip..skip + count]);
            done += count;
        }
        Ok(())
    }

    /// Reads whole sectors at `logical` and, for checksummed data, verifies
    /// each against the checksum tree, falling back to the other copy of a
    /// `dup` chunk when one fails.
    async fn read_sectors(
        &self,
        logical: u64,
        buf: &mut [u8],
        checksummed: bool,
    ) -> Result<(), Error> {
        let sector = self.superblock().sectorsize() as usize;
        let csum_type = self.superblock().csum_type();
        let expected = if checksummed {
            Some(self.lookup_checksums(logical, buf.len()).await?)
        } else {
            None
        };
        self.volume
            .read_verified(logical, buf, |data| {
                let Some(expected) = &expected else {
                    return Ok(());
                };
                let size = csum_type.size();
                for (index, (sector_data, stored)) in data
                    .chunks_exact(sector)
                    .zip(expected.chunks_exact(size))
                    .enumerate()
                {
                    if !csum_type.verifies(sector_data, stored) {
                        return Err(Error::DataChecksum {
                            logical: logical + (index * sector) as u64,
                        });
                    }
                }
                Ok(())
            })
            .await
    }

    /// The stored checksum of every sector of `[logical, logical + len)`,
    /// concatenated.
    async fn lookup_checksums(&self, logical: u64, len: usize) -> Result<Vec<u8>, Error> {
        let sector = u64::from(self.superblock().sectorsize());
        let size = self.superblock().csum_type().size();
        let sectors = len / sector as usize;
        let mut sums = vec![0_u8; sectors * size];
        let mut filled = 0;
        let first = Key::new(objectid::EXTENT_CSUM, item_type::EXTENT_CSUM, 0);
        let mut cursor = Cursor::floor(
            &self.volume,
            self.csum_tree,
            Key::new(objectid::EXTENT_CSUM, item_type::EXTENT_CSUM, logical),
        )
        .await?;
        while filled < sectors {
            let (Some(key), Some(item)) = (cursor.key(), cursor.item()) else {
                break;
            };
            if key < first {
                cursor.advance(&self.volume).await?;
                continue;
            }
            if !key.is(objectid::EXTENT_CSUM, item_type::EXTENT_CSUM) {
                break;
            }
            let item_error = |defect| Error::Item {
                tree: cursor.tree(),
                key,
                defect,
            };
            if item.len() % size != 0 {
                return Err(item_error(ItemDefect::ChecksumItem(item.len())));
            }
            if !key.offset.is_multiple_of(sector) {
                return Err(item_error(ItemDefect::Unaligned {
                    field: "checksum start",
                    value: key.offset,
                }));
            }
            let item_sectors = (item.len() / size) as u64;
            let item_end = key
                .offset
                .checked_add(item_sectors * sector)
                .ok_or(item_error(ItemDefect::ExtentOverflow))?;
            let wanted = logical + filled as u64 * sector;
            if item_end <= wanted {
                cursor.advance(&self.volume).await?;
                continue;
            }
            if key.offset > wanted {
                break;
            }
            let skip = ((wanted - key.offset) / sector) as usize;
            let count = (item_sectors as usize - skip).min(sectors - filled);
            sums[filled * size..(filled + count) * size]
                .copy_from_slice(&item[skip * size..(skip + count) * size]);
            filled += count;
            cursor.advance(&self.volume).await?;
        }
        if filled < sectors {
            return Err(Error::MissingDataChecksum {
                logical: logical + filled as u64 * sector,
            });
        }
        Ok(sums)
    }
}

/// The record named `name` in the directory or xattr item at `key`, which
/// the cursor sits on or past.
fn find_record<'a>(
    cursor: &'a Cursor,
    key: Key,
    name: &[u8],
    kind: RecordKind,
) -> Result<Option<DirRecord<'a>>, Error> {
    let (Some(found), Some(item)) = (cursor.key(), cursor.item()) else {
        return Ok(None);
    };
    if found != key {
        return Ok(None);
    }
    for record in dir_records(item, kind) {
        let record = record.map_err(|defect| Error::Item {
            tree: cursor.tree(),
            key,
            defect,
        })?;
        if record.name == name {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

/// Every chunk item of the chunk tree, read through the bootstrap map.
async fn read_chunk_tree<S: BlockSource>(
    volume: &Volume<S>,
    root: TreeRoot,
) -> Result<ChunkMap, Error> {
    let mut chunks = ChunkMap::default();
    let start = Key::new(objectid::FIRST_CHUNK_TREE, item_type::CHUNK_ITEM, 0);
    let mut cursor = Cursor::ceiling(volume, root, start).await?;
    while let (Some(key), Some(item)) = (cursor.key(), cursor.item()) {
        if !key.is(objectid::FIRST_CHUNK_TREE, item_type::CHUNK_ITEM) {
            break;
        }
        chunks.insert_item(key, item, volume.superblock())?;
        cursor.advance(volume).await?;
    }
    Ok(chunks)
}

/// The newest root item of tree `tree`.
async fn find_root<S: BlockSource>(
    volume: &Volume<S>,
    root_tree: TreeRoot,
    tree: u64,
) -> Result<RootRecord, Error> {
    let cursor = Cursor::floor(
        volume,
        root_tree,
        Key::new(tree, item_type::ROOT_ITEM, u64::MAX),
    )
    .await?;
    let (Some(key), Some(item)) = (cursor.key(), cursor.item()) else {
        return Err(Error::MissingRoot { tree });
    };
    if !key.is(tree, item_type::ROOT_ITEM) {
        return Err(Error::MissingRoot { tree });
    }
    RootRecord::parse(key, item, volume.superblock().sectorsize()).map_err(|defect| Error::Item {
        tree: objectid::ROOT_TREE,
        key,
        defect,
    })
}

/// The subvolume the root tree's `default` directory entry names, or the
/// top-level one when the filesystem records none.
async fn default_subvolume<S: BlockSource>(
    volume: &Volume<S>,
    root_tree: TreeRoot,
) -> Result<u64, Error> {
    const NAME: &[u8] = b"default";
    let key = Key::new(
        objectid::ROOT_TREE_DIR,
        item_type::DIR_ITEM,
        name_hash(NAME),
    );
    let cursor = Cursor::ceiling(volume, root_tree, key).await?;
    let tree = match find_record(&cursor, key, NAME, RecordKind::Entry)? {
        Some(record) => record.location.objectid,
        None => objectid::FS_TREE,
    };
    let is_subvolume =
        tree == objectid::FS_TREE || (objectid::FIRST_FREE..=objectid::LAST_FREE).contains(&tree);
    if !is_subvolume {
        return Err(Error::Item {
            tree: objectid::ROOT_TREE,
            key,
            defect: ItemDefect::LocationType(item_type::ROOT_ITEM),
        });
    }
    Ok(tree)
}
