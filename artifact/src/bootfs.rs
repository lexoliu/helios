//! The `helios-bootfs` payload image.
//!
//! The user payload is an artifact of its own, separate from the kernel
//! image: `helios-cli kernel-prebuild` writes one file, and the boot
//! protocol hands it to the kernel — a Limine module on the bare-metal
//! targets, an initrd on riscv64, a file on hosted. This module is the
//! single definition of that file's wire format, shared by the writer
//! (`helios-cli`) and the reader (the kernel).
//!
//! Layout, all integers little-endian, every section 16-byte aligned:
//!
//! ```text
//! +--------------------+
//! | header (48 bytes)  |
//! +--------------------+
//! | entry table        |  entry_count entries of 48 bytes each
//! +--------------------+
//! | string table       |  every referenced string at a 16-aligned offset
//! +--------------------+
//! | data               |  every referenced blob at a 16-aligned offset
//! +--------------------+
//! ```
//!
//! An entry names one object: a bootfs directory, a bootfs file, the init
//! component, or the init component's `argv0`. Path and data offsets are
//! relative to the string table and data sections respectively.

use alloc::vec::Vec;
use core::ops::Range;

use thiserror::Error;

/// Magic every `helios-bootfs` image starts with.
pub const MAGIC: [u8; 8] = *b"HLBOOTFS";

/// The only image version this reader and writer speak.
pub const VERSION: u32 = 1;

/// The alignment every offset in the image carries.
pub const SECTION_ALIGN: u64 = 16;

const HEADER_BYTES: usize = 48;
const ENTRY_BYTES: usize = 48;

// Header field offsets.
const VERSION_OFFSET: usize = 8;
const ENTRY_COUNT_OFFSET: usize = 12;
const STRING_TABLE_OFFSET_OFFSET: usize = 16;
const STRING_TABLE_LEN_OFFSET: usize = 24;
const DATA_OFFSET_OFFSET: usize = 32;
const DATA_LEN_OFFSET: usize = 40;

// Entry field offsets.
const ENTRY_KIND_OFFSET: usize = 0;
const ENTRY_RESERVED_OFFSET: usize = 4;
const ENTRY_PATH_OFFSET_OFFSET: usize = 8;
const ENTRY_PATH_LEN_OFFSET: usize = 16;
const ENTRY_DATA_OFFSET_OFFSET: usize = 24;
const ENTRY_DATA_LEN_OFFSET: usize = 32;
const ENTRY_MODIFIED_NANOS_OFFSET: usize = 40;

const KIND_DIRECTORY: u32 = 0;
const KIND_FILE: u32 = 1;
const KIND_INIT_COMPONENT: u32 = 2;
const KIND_INIT_ARGV0: u32 = 3;

/// What one image entry names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A directory: `path` names it, `data` is empty.
    Directory,
    /// A file: `path` names it, `data` carries its bytes.
    File,
    /// The init component: `path` names it, `data` carries the signed
    /// artifact. Exactly one per image.
    InitComponent,
    /// The init component's `argv0`: `path` carries the string,
    /// `data` is empty. Exactly one per image.
    InitArgv0,
}

impl EntryKind {
    fn from_wire(kind: u32) -> Option<Self> {
        match kind {
            KIND_DIRECTORY => Some(Self::Directory),
            KIND_FILE => Some(Self::File),
            KIND_INIT_COMPONENT => Some(Self::InitComponent),
            KIND_INIT_ARGV0 => Some(Self::InitArgv0),
            _ => None,
        }
    }

    fn to_wire(self) -> u32 {
        match self {
            Self::Directory => KIND_DIRECTORY,
            Self::File => KIND_FILE,
            Self::InitComponent => KIND_INIT_COMPONENT,
            Self::InitArgv0 => KIND_INIT_ARGV0,
        }
    }
}

/// One entry of a parsed [`Image`], its `path` and `data` borrowing the
/// image's bytes.
#[derive(Clone, Copy, Debug)]
pub struct ImageEntry {
    kind: EntryKind,
    path: &'static str,
    data: &'static [u8],
    modified_nanos: u64,
}

impl ImageEntry {
    pub fn kind(&self) -> EntryKind {
        self.kind
    }

    pub fn path(&self) -> &'static str {
        self.path
    }

    pub fn data(&self) -> &'static [u8] {
        self.data
    }

    pub fn modified_nanos(&self) -> u64 {
        self.modified_nanos
    }
}

/// A parsed `helios-bootfs` image: a validated view over `bytes`.
///
/// `parse` checks every offset the image records against the slice it
/// was handed, so accessors cannot fail and the returned views never
/// read outside the image.
#[derive(Clone)]
pub struct Image {
    bytes: &'static [u8],
    entry_count: usize,
    string_table: Range<usize>,
    data: Range<usize>,
}

impl Image {
    /// Parses and validates a `helios-bootfs` image.
    ///
    /// Every failure names the field that was wrong; a parsed image's
    /// entries borrow `bytes` and never read outside it.
    pub fn parse(bytes: &'static [u8]) -> Result<Self, BootfsError> {
        if bytes.len() < HEADER_BYTES {
            return Err(BootfsError::TruncatedHeader { len: bytes.len() });
        }
        let magic: [u8; 8] = bytes[..8].try_into().expect("magic field is 8 bytes");
        if magic != MAGIC {
            return Err(BootfsError::BadMagic { found: magic });
        }
        let version = read_u32(bytes, VERSION_OFFSET);
        if version != VERSION {
            return Err(BootfsError::BadVersion { found: version });
        }
        let entry_count = read_u32(bytes, ENTRY_COUNT_OFFSET) as usize;
        let string_table_offset = read_u64(bytes, STRING_TABLE_OFFSET_OFFSET);
        let string_table_len = read_u64(bytes, STRING_TABLE_LEN_OFFSET);
        let data_offset = read_u64(bytes, DATA_OFFSET_OFFSET);
        let data_len = read_u64(bytes, DATA_LEN_OFFSET);

        let entries_end = HEADER_BYTES
            .checked_add(
                entry_count
                    .checked_mul(ENTRY_BYTES)
                    .ok_or(BootfsError::EntryTableOverflow { entry_count })?,
            )
            .ok_or(BootfsError::EntryTableOverflow { entry_count })?;
        if entries_end > bytes.len() {
            return Err(BootfsError::EntryTableOverrun {
                entry_count,
                len: bytes.len(),
            });
        }
        let string_table = checked_section(
            string_table_offset,
            string_table_len,
            bytes.len(),
            Section::StringTable,
        )?;
        let data = checked_section(data_offset, data_len, bytes.len(), Section::Data)?;
        if entries_end > string_table.start {
            return Err(BootfsError::SectionOrder {
                section: Section::EntryTable,
                end: entries_end as u64,
                next_start: string_table.start as u64,
            });
        }
        if string_table.end > data.start {
            return Err(BootfsError::SectionOrder {
                section: Section::StringTable,
                end: string_table.end as u64,
                next_start: data.start as u64,
            });
        }

        let image = Self {
            bytes,
            entry_count,
            string_table,
            data,
        };
        let mut init_components = 0_u32;
        let mut init_argv0s = 0_u32;
        for index in 0..entry_count {
            let entry = image.entry(index)?;
            match entry.kind {
                EntryKind::InitComponent => init_components += 1,
                EntryKind::InitArgv0 => init_argv0s += 1,
                EntryKind::Directory | EntryKind::File => {}
            }
        }
        if init_components != 1 {
            return Err(BootfsError::InitComponentCount {
                found: init_components,
            });
        }
        if init_argv0s != 1 {
            return Err(BootfsError::InitArgv0Count { found: init_argv0s });
        }
        Ok(image)
    }

    /// The image's entries in table order.
    ///
    /// [`Image::parse`] validated every offset they describe, so the
    /// iteration is infallible.
    pub fn entries(&self) -> impl Iterator<Item = ImageEntry> {
        (0..self.entry_count).map(|index| {
            self.entry(index)
                .unwrap_or_else(|error| unreachable!("parsed image entry {index}: {error}"))
        })
    }

    /// The one entry of `kind`, or `None` when the image carries none.
    ///
    /// `parse` already rejects an image carrying two, so the first is
    /// the only one.
    pub fn entry_of_kind(&self, kind: EntryKind) -> Option<ImageEntry> {
        self.entries().find(|entry| entry.kind() == kind)
    }

    /// Reads and validates the raw entry record at `index`.
    fn entry(&self, index: usize) -> Result<ImageEntry, BootfsError> {
        let base = HEADER_BYTES + index * ENTRY_BYTES;
        let kind = read_u32(self.bytes, base + ENTRY_KIND_OFFSET);
        let kind = EntryKind::from_wire(kind).ok_or(BootfsError::EntryKind { index, kind })?;
        let reserved = read_u32(self.bytes, base + ENTRY_RESERVED_OFFSET);
        if reserved != 0 {
            return Err(BootfsError::EntryReserved { index, reserved });
        }
        let path_offset = read_u64(self.bytes, base + ENTRY_PATH_OFFSET_OFFSET);
        let path_len = read_u64(self.bytes, base + ENTRY_PATH_LEN_OFFSET);
        let data_offset = read_u64(self.bytes, base + ENTRY_DATA_OFFSET_OFFSET);
        let data_len = read_u64(self.bytes, base + ENTRY_DATA_LEN_OFFSET);
        let modified_nanos = read_u64(self.bytes, base + ENTRY_MODIFIED_NANOS_OFFSET);
        if !is_aligned(path_offset) {
            return Err(BootfsError::EntryPathMisaligned { index, path_offset });
        }
        if !is_aligned(data_offset) {
            return Err(BootfsError::EntryDataMisaligned { index, data_offset });
        }
        let path = self.slice_in(&self.string_table, path_offset, path_len, index, false)?;
        let path =
            core::str::from_utf8(path).map_err(|_| BootfsError::EntryPathNotUtf8 { index })?;
        let data = self.slice_in(&self.data, data_offset, data_len, index, true)?;
        Ok(ImageEntry {
            kind,
            path,
            data,
            modified_nanos,
        })
    }

    /// `section[offset..offset+len]` as a borrow of the image's bytes,
    /// the offset being relative to the section's start.
    fn slice_in(
        &self,
        section: &Range<usize>,
        offset: u64,
        len: u64,
        index: usize,
        is_data: bool,
    ) -> Result<&'static [u8], BootfsError> {
        let end = offset.checked_add(len).ok_or(if is_data {
            BootfsError::EntryDataOverrun {
                index,
                data_offset: offset,
                data_len: len,
            }
        } else {
            BootfsError::EntryPathOverrun {
                index,
                path_offset: offset,
                path_len: len,
            }
        })?;
        if end > section.len() as u64 {
            return Err(if is_data {
                BootfsError::EntryDataOverrun {
                    index,
                    data_offset: offset,
                    data_len: len,
                }
            } else {
                BootfsError::EntryPathOverrun {
                    index,
                    path_offset: offset,
                    path_len: len,
                }
            });
        }
        let start = section.start + offset as usize;
        Ok(&self.bytes[start..start + len as usize])
    }
}

/// One entry handed to [`write_image`].
#[derive(Clone, Copy)]
pub struct WriteEntry<'a> {
    pub kind: EntryKind,
    pub path: &'a str,
    pub data: &'a [u8],
    pub modified_nanos: u64,
}

/// Serialises `entries` into a `helios-bootfs` image.
///
/// The layout is the one [`Image::parse`] reads: fixed header, entry
/// table, string table, file data, every referenced offset 16-byte
/// aligned.
pub fn write_image(entries: &[WriteEntry<'_>]) -> Vec<u8> {
    let entry_table_len = entries.len() * ENTRY_BYTES;
    let mut string_table = Vec::new();
    let mut path_offsets = Vec::with_capacity(entries.len());
    for entry in entries {
        path_offsets.push(string_table.len() as u64);
        string_table.extend_from_slice(entry.path.as_bytes());
        pad_to(&mut string_table);
    }
    let mut data = Vec::new();
    let mut data_offsets = Vec::with_capacity(entries.len());
    for entry in entries {
        data_offsets.push(data.len() as u64);
        data.extend_from_slice(entry.data);
        pad_to(&mut data);
    }

    let string_table_offset = align(HEADER_BYTES as u64 + entry_table_len as u64);
    let data_offset = align(string_table_offset + string_table.len() as u64);
    let image_len = data_offset + data.len() as u64;
    let mut image = Vec::with_capacity(image_len as usize);

    image.extend_from_slice(&MAGIC);
    image.extend_from_slice(&VERSION.to_le_bytes());
    image.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    image.extend_from_slice(&string_table_offset.to_le_bytes());
    image.extend_from_slice(&(string_table.len() as u64).to_le_bytes());
    image.extend_from_slice(&data_offset.to_le_bytes());
    image.extend_from_slice(&(data.len() as u64).to_le_bytes());
    debug_assert_eq!(image.len(), HEADER_BYTES);

    for (index, entry) in entries.iter().enumerate() {
        image.extend_from_slice(&entry.kind.to_wire().to_le_bytes());
        image.extend_from_slice(&0_u32.to_le_bytes());
        image.extend_from_slice(&path_offsets[index].to_le_bytes());
        image.extend_from_slice(&(entry.path.len() as u64).to_le_bytes());
        image.extend_from_slice(&data_offsets[index].to_le_bytes());
        image.extend_from_slice(&(entry.data.len() as u64).to_le_bytes());
        image.extend_from_slice(&entry.modified_nanos.to_le_bytes());
    }
    pad_to(&mut image);
    image.extend_from_slice(&string_table);
    debug_assert_eq!(image.len() as u64, data_offset);
    image.extend_from_slice(&data);
    debug_assert_eq!(image.len() as u64, image_len);
    image
}

fn is_aligned(offset: u64) -> bool {
    offset.is_multiple_of(SECTION_ALIGN)
}

fn align(offset: u64) -> u64 {
    offset.next_multiple_of(SECTION_ALIGN)
}

fn pad_to(bytes: &mut Vec<u8>) {
    bytes.resize(align(bytes.len() as u64) as usize, 0);
}

fn checked_section(
    offset: u64,
    len: u64,
    image_len: usize,
    section: Section,
) -> Result<Range<usize>, BootfsError> {
    if !is_aligned(offset) {
        return Err(BootfsError::SectionMisaligned { section, offset });
    }
    let end = offset.checked_add(len).ok_or(BootfsError::SectionOverrun {
        section,
        offset,
        len,
    })?;
    if end > image_len as u64 {
        return Err(BootfsError::SectionOverrun {
            section,
            offset,
            len,
        });
    }
    Ok(offset as usize..end as usize)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("u32 field is 4 bytes"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("u64 field is 8 bytes"),
    )
}

/// Which section a header field error names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    EntryTable,
    StringTable,
    Data,
}

impl core::fmt::Display for Section {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EntryTable => f.write_str("entry table"),
            Self::StringTable => f.write_str("string table"),
            Self::Data => f.write_str("data"),
        }
    }
}

/// Why a `helios-bootfs` image did not parse.
///
/// Every variant names the field that was wrong, so a bad image reports
/// what to fix rather than just that it is bad.
#[derive(Debug, Error)]
pub enum BootfsError {
    #[error("bootfs image is {len} bytes, smaller than the {HEADER_BYTES}-byte header")]
    TruncatedHeader { len: usize },
    #[error("bootfs image magic {found:02x?} is not {MAGIC:02x?}")]
    BadMagic { found: [u8; 8] },
    #[error("bootfs image version {found} is not supported, expected {VERSION}")]
    BadVersion { found: u32 },
    #[error("entry_count {entry_count} overflows the image's entry-table size")]
    EntryTableOverflow { entry_count: usize },
    #[error("entry_count {entry_count} entries overruns the {len}-byte image")]
    EntryTableOverrun { entry_count: usize, len: usize },
    #[error("{section} offset {offset:#x} is not {SECTION_ALIGN}-byte aligned")]
    SectionMisaligned { section: Section, offset: u64 },
    #[error("{section} {offset:#x}+{len:#x} overruns the image")]
    SectionOverrun {
        section: Section,
        offset: u64,
        len: u64,
    },
    #[error("{section} ends at {end:#x}, past the next section's start {next_start:#x}")]
    SectionOrder {
        section: Section,
        end: u64,
        next_start: u64,
    },
    #[error("entry {index} has unknown kind {kind}")]
    EntryKind { index: usize, kind: u32 },
    #[error("entry {index} has reserved field {reserved:#x}, expected zero")]
    EntryReserved { index: usize, reserved: u32 },
    #[error("entry {index} path offset {path_offset:#x} is not {SECTION_ALIGN}-byte aligned")]
    EntryPathMisaligned { index: usize, path_offset: u64 },
    #[error("entry {index} data offset {data_offset:#x} is not {SECTION_ALIGN}-byte aligned")]
    EntryDataMisaligned { index: usize, data_offset: u64 },
    #[error("entry {index} path {path_offset:#x}+{path_len:#x} overruns the string table")]
    EntryPathOverrun {
        index: usize,
        path_offset: u64,
        path_len: u64,
    },
    #[error("entry {index} data {data_offset:#x}+{data_len:#x} overruns the data section")]
    EntryDataOverrun {
        index: usize,
        data_offset: u64,
        data_len: u64,
    },
    #[error("entry {index} path is not UTF-8")]
    EntryPathNotUtf8 { index: usize },
    #[error("bootfs image carries {found} init-component entries, expected exactly one")]
    InitComponentCount { found: u32 },
    #[error("bootfs image carries {found} init-argv0 entries, expected exactly one")]
    InitArgv0Count { found: u32 },
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::boxed::Box;

    use super::*;

    fn image(entries: &[WriteEntry<'_>]) -> &'static [u8] {
        Box::leak(write_image(entries).into_boxed_slice())
    }

    fn fixture() -> &'static [u8] {
        image(&[
            WriteEntry {
                kind: EntryKind::Directory,
                path: "bin/empty",
                data: b"",
                modified_nanos: 11,
            },
            WriteEntry {
                kind: EntryKind::File,
                path: "bin/tool",
                data: b"tool-bytes",
                modified_nanos: 22,
            },
            WriteEntry {
                kind: EntryKind::InitComponent,
                path: "init_component.cwasm",
                data: b"init-component-bytes",
                modified_nanos: 33,
            },
            WriteEntry {
                kind: EntryKind::InitArgv0,
                path: "/init.wasm",
                data: b"",
                modified_nanos: 0,
            },
        ])
    }

    /// `fixture` with the `len`-byte field at `offset` replaced.
    ///
    /// Returns a leaked copy so each corruption is one test's own.
    fn corrupt(offset: usize, patch: &[u8]) -> &'static [u8] {
        let mut bytes = fixture().to_vec();
        bytes[offset..offset + patch.len()].copy_from_slice(patch);
        Box::leak(bytes.into_boxed_slice())
    }

    fn header() -> Vec<u8> {
        write_image(&[
            WriteEntry {
                kind: EntryKind::InitComponent,
                path: "init",
                data: b"i",
                modified_nanos: 0,
            },
            WriteEntry {
                kind: EntryKind::InitArgv0,
                path: "/init.wasm",
                data: b"",
                modified_nanos: 0,
            },
        ])
    }

    #[test]
    fn round_trip() {
        let image = Image::parse(fixture()).expect("fixture must parse");
        let entries: Vec<_> = image.entries().collect();
        assert_eq!(entries.len(), 4);

        assert_eq!(entries[0].kind(), EntryKind::Directory);
        assert_eq!(entries[0].path(), "bin/empty");
        assert_eq!(entries[0].data(), b"");
        assert_eq!(entries[0].modified_nanos(), 11);

        assert_eq!(entries[1].kind(), EntryKind::File);
        assert_eq!(entries[1].path(), "bin/tool");
        assert_eq!(entries[1].data(), b"tool-bytes");
        assert_eq!(entries[1].modified_nanos(), 22);

        assert_eq!(entries[2].kind(), EntryKind::InitComponent);
        assert_eq!(entries[2].path(), "init_component.cwasm");
        assert_eq!(entries[2].data(), b"init-component-bytes");
        assert_eq!(entries[2].modified_nanos(), 33);

        assert_eq!(entries[3].kind(), EntryKind::InitArgv0);
        assert_eq!(entries[3].path(), "/init.wasm");

        assert_eq!(
            image
                .entry_of_kind(EntryKind::InitComponent)
                .map(|entry| entry.path()),
            Some("init_component.cwasm")
        );
        assert_eq!(
            image
                .entry_of_kind(EntryKind::InitArgv0)
                .map(|entry| entry.path()),
            Some("/init.wasm")
        );
    }

    #[test]
    fn truncated_header_fails() {
        assert!(matches!(
            Image::parse(&[0_u8; HEADER_BYTES - 1]),
            Err(BootfsError::TruncatedHeader { len }) if len == HEADER_BYTES - 1
        ));
    }

    #[test]
    fn bad_magic_fails() {
        assert!(matches!(
            Image::parse(corrupt(0, b"NOTMAGIC")),
            Err(BootfsError::BadMagic { .. })
        ));
    }

    #[test]
    fn bad_version_fails() {
        assert!(matches!(
            Image::parse(corrupt(VERSION_OFFSET, &2_u32.to_le_bytes())),
            Err(BootfsError::BadVersion { found: 2 })
        ));
    }

    #[test]
    fn entry_table_overrun_fails() {
        assert!(matches!(
            Image::parse(corrupt(ENTRY_COUNT_OFFSET, &u32::MAX.to_le_bytes())),
            Err(BootfsError::EntryTableOverflow { .. })
                | Err(BootfsError::EntryTableOverrun { .. })
        ));
    }

    #[test]
    fn misaligned_string_table_fails() {
        assert!(matches!(
            Image::parse(corrupt(STRING_TABLE_OFFSET_OFFSET, &33_u64.to_le_bytes())),
            Err(BootfsError::SectionMisaligned {
                section: Section::StringTable,
                offset: 33,
            })
        ));
    }

    #[test]
    fn overrun_string_table_fails() {
        assert!(matches!(
            Image::parse(corrupt(STRING_TABLE_LEN_OFFSET, &u64::MAX.to_le_bytes())),
            Err(BootfsError::SectionOverrun {
                section: Section::StringTable,
                ..
            })
        ));
    }

    #[test]
    fn misaligned_data_offset_fails() {
        assert!(matches!(
            Image::parse(corrupt(DATA_OFFSET_OFFSET, &17_u64.to_le_bytes())),
            Err(BootfsError::SectionMisaligned {
                section: Section::Data,
                offset: 17,
            })
        ));
    }

    #[test]
    fn overrun_data_section_fails() {
        assert!(matches!(
            Image::parse(corrupt(DATA_LEN_OFFSET, &u64::MAX.to_le_bytes())),
            Err(BootfsError::SectionOverrun {
                section: Section::Data,
                ..
            })
        ));
    }

    #[test]
    fn overlapping_sections_fail() {
        // Move the string table over the entry table's tail.
        let mut bytes = header();
        bytes[STRING_TABLE_OFFSET_OFFSET..STRING_TABLE_OFFSET_OFFSET + 8]
            .copy_from_slice(&48_u64.to_le_bytes());
        let bytes = Box::leak(bytes.into_boxed_slice());
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::SectionOrder {
                section: Section::EntryTable,
                ..
            })
        ));
    }

    #[test]
    fn unknown_entry_kind_fails() {
        let bytes = corrupt(HEADER_BYTES + ENTRY_KIND_OFFSET, &9_u32.to_le_bytes());
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::EntryKind { index: 0, kind: 9 })
        ));
    }

    #[test]
    fn nonzero_reserved_fails() {
        let bytes = corrupt(HEADER_BYTES + ENTRY_RESERVED_OFFSET, &1_u32.to_le_bytes());
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::EntryReserved { index: 0, .. })
        ));
    }

    #[test]
    fn misaligned_path_offset_fails() {
        let bytes = corrupt(
            HEADER_BYTES + ENTRY_PATH_OFFSET_OFFSET,
            &4_u64.to_le_bytes(),
        );
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::EntryPathMisaligned { index: 0, .. })
        ));
    }

    #[test]
    fn overrun_path_fails() {
        let bytes = corrupt(
            HEADER_BYTES + ENTRY_PATH_LEN_OFFSET,
            &u64::MAX.to_le_bytes(),
        );
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::EntryPathOverrun { index: 0, .. })
        ));
    }

    #[test]
    fn misaligned_data_offset_entry_fails() {
        let bytes = corrupt(
            HEADER_BYTES + ENTRY_DATA_OFFSET_OFFSET,
            &4_u64.to_le_bytes(),
        );
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::EntryDataMisaligned { index: 0, .. })
        ));
    }

    #[test]
    fn overrun_entry_data_fails() {
        let bytes = corrupt(
            HEADER_BYTES + ENTRY_DATA_LEN_OFFSET,
            &u64::MAX.to_le_bytes(),
        );
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::EntryDataOverrun { index: 0, .. })
        ));
    }

    #[test]
    fn non_utf8_path_fails() {
        // Entry 0's path is "bin/empty" at string-table offset 0; flip its
        // first byte to an invalid UTF-8 start.
        let image = fixture();
        let string_table_offset = u64::from_le_bytes(
            image[STRING_TABLE_OFFSET_OFFSET..STRING_TABLE_OFFSET_OFFSET + 8]
                .try_into()
                .unwrap(),
        ) as usize;
        let bytes = corrupt(string_table_offset, &[0xff]);
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::EntryPathNotUtf8 { index: 0 })
        ));
    }

    #[test]
    fn missing_init_entries_fail() {
        let bytes = image(&[WriteEntry {
            kind: EntryKind::File,
            path: "bin/tool",
            data: b"x",
            modified_nanos: 0,
        }]);
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::InitComponentCount { found: 0 })
        ));
    }

    #[test]
    fn duplicate_init_component_fails() {
        let entry = WriteEntry {
            kind: EntryKind::InitComponent,
            path: "init",
            data: b"i",
            modified_nanos: 0,
        };
        let argv0 = WriteEntry {
            kind: EntryKind::InitArgv0,
            path: "/init.wasm",
            data: b"",
            modified_nanos: 0,
        };
        let bytes = image(&[entry, argv0, entry]);
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::InitComponentCount { found: 2 })
        ));
    }

    #[test]
    fn duplicate_init_argv0_fails() {
        let component = WriteEntry {
            kind: EntryKind::InitComponent,
            path: "init",
            data: b"i",
            modified_nanos: 0,
        };
        let argv0 = WriteEntry {
            kind: EntryKind::InitArgv0,
            path: "/init.wasm",
            data: b"",
            modified_nanos: 0,
        };
        let bytes = image(&[component, argv0, argv0]);
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::InitArgv0Count { found: 2 })
        ));
    }
}
