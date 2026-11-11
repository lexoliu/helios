//! Item payloads, turned into validated values.
//!
//! Each parser takes an item's raw bytes, checks every length and field the
//! read path relies on, and returns either a value whose fields can be used
//! as they are or the [`ItemDefect`] that names the failed check.

use crate::error::{Compression, FileKind, ItemDefect};
use crate::key::Key;
use crate::ondisk::{
    MAX_COMPRESSED_EXTENT, MAX_LEVEL, NAME_MAX, RawDirItem, RawFileExtentBody, RawFileExtentHead,
    RawInodeItem, RawRootItem, RawTimespec, item_type, view,
};
use crate::tree::TreeRoot;

/// A point in time as the format stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp {
    /// Seconds since the Unix epoch; negative before it.
    pub seconds: i64,
    /// Nanoseconds within the second, below one billion.
    pub nanoseconds: u32,
}

impl Timestamp {
    fn parse(raw: &RawTimespec) -> Result<Self, ItemDefect> {
        let nanoseconds = raw.nsec.get();
        if nanoseconds >= 1_000_000_000 {
            return Err(ItemDefect::Nanoseconds(nanoseconds));
        }
        Ok(Self {
            seconds: raw.sec.get() as i64,
            nanoseconds,
        })
    }
}

/// The inode flag marking a file whose data carries no checksums.
pub(crate) const INODE_NODATASUM: u64 = 1 << 0;

/// What an inode item records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub kind: FileKind,
    /// Permission and set-id bits, without the file type.
    pub permissions: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    /// Logical size in bytes.
    pub size: u64,
    /// Bytes of storage charged to the inode.
    pub nbytes: u64,
    pub rdev: u64,
    /// The `BTRFS_INODE_*` flags.
    pub flags: u64,
    pub generation: u64,
    pub atime: Timestamp,
    pub ctime: Timestamp,
    pub mtime: Timestamp,
    pub otime: Timestamp,
}

impl Metadata {
    pub(crate) fn parse(item: &[u8]) -> Result<Self, ItemDefect> {
        let raw: &RawInodeItem = view(item).ok_or(ItemDefect::Size {
            needed: size_of::<RawInodeItem>(),
            found: item.len(),
        })?;
        let mode = raw.mode.get();
        let kind = match mode & 0o170_000 {
            0o100_000 => FileKind::Regular,
            0o040_000 => FileKind::Directory,
            0o120_000 => FileKind::Symlink,
            0o020_000 => FileKind::CharDevice,
            0o060_000 => FileKind::BlockDevice,
            0o010_000 => FileKind::Fifo,
            0o140_000 => FileKind::Socket,
            _ => return Err(ItemDefect::Mode(mode)),
        };
        Ok(Self {
            kind,
            permissions: mode & 0o7777,
            uid: raw.uid.get(),
            gid: raw.gid.get(),
            nlink: raw.nlink.get(),
            size: raw.size.get(),
            nbytes: raw.nbytes.get(),
            rdev: raw.rdev.get(),
            flags: raw.flags.get(),
            generation: raw.generation.get(),
            atime: Timestamp::parse(&raw.atime)?,
            ctime: Timestamp::parse(&raw.ctime)?,
            mtime: Timestamp::parse(&raw.mtime)?,
            otime: Timestamp::parse(&raw.otime)?,
        })
    }
}

/// Where a directory entry leads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryTarget {
    /// An inode in the directory's own tree.
    Inode(u64),
    /// The root directory of another subvolume, by tree id.
    Subvolume(u64),
}

/// One record of a directory item, directory index or xattr item.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DirRecord<'a> {
    pub(crate) location: Key,
    pub(crate) entry_type: u8,
    pub(crate) name: &'a [u8],
    pub(crate) data: &'a [u8],
}

const ENTRY_TYPE_XATTR: u8 = 8;

impl DirRecord<'_> {
    pub(crate) fn kind(&self) -> FileKind {
        // `dir_records` accepted only the seven file types.
        match self.entry_type {
            1 => FileKind::Regular,
            2 => FileKind::Directory,
            3 => FileKind::CharDevice,
            4 => FileKind::BlockDevice,
            5 => FileKind::Fifo,
            6 => FileKind::Socket,
            _ => FileKind::Symlink,
        }
    }

    pub(crate) fn target(&self) -> EntryTarget {
        // `dir_records` accepted only these two location types.
        if self.location.item_type == item_type::ROOT_ITEM {
            EntryTarget::Subvolume(self.location.objectid)
        } else {
            EntryTarget::Inode(self.location.objectid)
        }
    }
}

/// Which kind of item a run of directory records came from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// A directory item or index: a named link with no data.
    Entry,
    /// An extended attribute: a name with a value.
    Xattr,
}

/// The records packed into one item, each validated as it is reached.
pub(crate) fn dir_records(
    mut payload: &[u8],
    kind: RecordKind,
) -> impl Iterator<Item = Result<DirRecord<'_>, ItemDefect>> {
    core::iter::from_fn(move || {
        if payload.is_empty() {
            return None;
        }
        let record = parse_record(payload, kind);
        match record {
            Ok((record, consumed)) => {
                payload = &payload[consumed..];
                Some(Ok(record))
            }
            Err(defect) => {
                payload = &[];
                Some(Err(defect))
            }
        }
    })
}

fn parse_record(payload: &[u8], kind: RecordKind) -> Result<(DirRecord<'_>, usize), ItemDefect> {
    let header_bytes = size_of::<RawDirItem>();
    let raw: &RawDirItem = view(payload).ok_or(ItemDefect::Size {
        needed: header_bytes,
        found: payload.len(),
    })?;
    let name_len = usize::from(raw.name_len.get());
    let data_len = usize::from(raw.data_len.get());
    if name_len == 0 || name_len > NAME_MAX {
        return Err(ItemDefect::NameLength(name_len));
    }
    let consumed = header_bytes + name_len + data_len;
    if payload.len() < consumed {
        return Err(ItemDefect::Size {
            needed: consumed,
            found: payload.len(),
        });
    }
    let location = Key::from_raw(&raw.location);
    match kind {
        RecordKind::Entry => {
            if !(1..=7).contains(&raw.entry_type) {
                return Err(ItemDefect::EntryType(raw.entry_type));
            }
            if data_len != 0 {
                return Err(ItemDefect::UnexpectedData(data_len));
            }
            if location.item_type != item_type::INODE_ITEM
                && location.item_type != item_type::ROOT_ITEM
            {
                return Err(ItemDefect::LocationType(location.item_type));
            }
        }
        RecordKind::Xattr => {
            if raw.entry_type != ENTRY_TYPE_XATTR {
                return Err(ItemDefect::EntryType(raw.entry_type));
            }
        }
    }
    let name = &payload[header_bytes..header_bytes + name_len];
    let data = &payload[header_bytes + name_len..consumed];
    Ok((
        DirRecord {
            location,
            entry_type: raw.entry_type,
            name,
            data,
        },
        consumed,
    ))
}

/// The one record a directory index item holds.
pub(crate) fn dir_index_record(payload: &[u8]) -> Result<DirRecord<'_>, ItemDefect> {
    let (record, consumed) = parse_record(payload, RecordKind::Entry)?;
    if consumed != payload.len() {
        return Err(ItemDefect::IndexEntries);
    }
    Ok(record)
}

/// A root item: where a tree starts and which inode is its top directory.
pub(crate) struct RootRecord {
    pub(crate) root: TreeRoot,
    pub(crate) root_dirid: u64,
}

impl RootRecord {
    pub(crate) fn parse(key: Key, item: &[u8], sectorsize: u32) -> Result<Self, ItemDefect> {
        let raw: &RawRootItem = view(item).ok_or(ItemDefect::Size {
            needed: size_of::<RawRootItem>(),
            found: item.len(),
        })?;
        if raw.level >= MAX_LEVEL {
            return Err(ItemDefect::RootLevel(raw.level));
        }
        let bytenr = raw.bytenr.get();
        if !bytenr.is_multiple_of(u64::from(sectorsize)) {
            return Err(ItemDefect::RootBytenr(bytenr));
        }
        Ok(Self {
            root: TreeRoot {
                tree: key.objectid,
                bytenr,
                level: raw.level,
                generation: raw.generation.get(),
            },
            root_dirid: raw.root_dirid.get(),
        })
    }
}

/// What a file extent item stores for its range.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ExtentData<'a> {
    /// The bytes are in the item, possibly compressed.
    Inline {
        data: &'a [u8],
        compression: Option<Compression>,
    },
    /// The bytes are in a data extent on disk.
    Regular {
        disk_bytenr: u64,
        disk_num_bytes: u64,
        /// Offset into the decompressed extent where this range starts.
        offset: u64,
        ram_bytes: u64,
        compression: Option<Compression>,
    },
    /// The range reads as zeroes: an explicit hole or a preallocated,
    /// never-written extent.
    Zero,
}

/// A file extent item: the file range `[start, end)` and where it lives.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FileExtent<'a> {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) data: ExtentData<'a>,
}

const EXTENT_INLINE: u8 = 0;
const EXTENT_REGULAR: u8 = 1;
const EXTENT_PREALLOC: u8 = 2;

impl<'a> FileExtent<'a> {
    pub(crate) fn parse(key: Key, item: &'a [u8], sectorsize: u32) -> Result<Self, ItemDefect> {
        let head_bytes = size_of::<RawFileExtentHead>();
        let head: &RawFileExtentHead = view(item).ok_or(ItemDefect::Size {
            needed: head_bytes,
            found: item.len(),
        })?;
        if head.encryption != 0 || head.other_encoding.get() != 0 {
            return Err(ItemDefect::Encoding {
                encryption: head.encryption,
                other_encoding: head.other_encoding.get(),
            });
        }
        let compression = match head.compression {
            0 => None,
            1 => Some(Compression::Zlib),
            2 => Some(Compression::Lzo),
            3 => Some(Compression::Zstd),
            other => return Err(ItemDefect::Compression(other)),
        };
        let sector = u64::from(sectorsize);
        let start = key.offset;
        let ram_bytes = head.ram_bytes.get();

        if head.extent_type == EXTENT_INLINE {
            if start != 0 {
                return Err(ItemDefect::InlineOffset(start));
            }
            let data = &item[head_bytes..];
            let consistent = match compression {
                None => ram_bytes == data.len() as u64,
                Some(_) => ram_bytes <= sector && !data.is_empty(),
            };
            if !consistent {
                return Err(ItemDefect::InlineSize {
                    ram_bytes,
                    stored: data.len(),
                });
            }
            return Ok(Self {
                start,
                end: ram_bytes,
                data: ExtentData::Inline { data, compression },
            });
        }
        if head.extent_type != EXTENT_REGULAR && head.extent_type != EXTENT_PREALLOC {
            return Err(ItemDefect::ExtentType(head.extent_type));
        }

        let body_bytes = head_bytes + size_of::<RawFileExtentBody>();
        if item.len() != body_bytes {
            return Err(ItemDefect::Size {
                needed: body_bytes,
                found: item.len(),
            });
        }
        let body: &RawFileExtentBody =
            view(&item[head_bytes..]).expect("item length checked above");
        let disk_bytenr = body.disk_bytenr.get();
        let disk_num_bytes = body.disk_num_bytes.get();
        let offset = body.offset.get();
        let num_bytes = body.num_bytes.get();
        for (field, value) in [
            ("file offset", start),
            ("disk_bytenr", disk_bytenr),
            ("disk_num_bytes", disk_num_bytes),
            ("offset", offset),
            ("num_bytes", num_bytes),
        ] {
            if !value.is_multiple_of(sector) {
                return Err(ItemDefect::Unaligned { field, value });
            }
        }
        let end = start
            .checked_add(num_bytes)
            .ok_or(ItemDefect::ExtentOverflow)?;
        let referenced_end = offset
            .checked_add(num_bytes)
            .ok_or(ItemDefect::ExtentOverflow)?;

        if head.extent_type == EXTENT_PREALLOC {
            if compression.is_some() {
                return Err(ItemDefect::Compression(head.compression));
            }
            return Ok(Self {
                start,
                end,
                data: ExtentData::Zero,
            });
        }
        if disk_bytenr == 0 {
            return Ok(Self {
                start,
                end,
                data: ExtentData::Zero,
            });
        }
        disk_bytenr
            .checked_add(disk_num_bytes)
            .ok_or(ItemDefect::ExtentOverflow)?;
        let limit = match compression {
            None => disk_num_bytes,
            Some(_) => {
                for size in [disk_num_bytes, ram_bytes] {
                    if size > MAX_COMPRESSED_EXTENT {
                        return Err(ItemDefect::CompressedSize(size));
                    }
                }
                ram_bytes
            }
        };
        if referenced_end > limit {
            return Err(ItemDefect::ExtentRange {
                offset,
                len: num_bytes,
                limit,
            });
        }
        Ok(Self {
            start,
            end,
            data: ExtentData::Regular {
                disk_bytenr,
                disk_num_bytes,
                offset,
                ram_bytes,
                compression,
            },
        })
    }
}
