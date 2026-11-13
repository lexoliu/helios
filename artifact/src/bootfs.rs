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
//! | signature footer   |  128 bytes, see [`SIGNATURE_FOOTER_BYTES`]
//! +--------------------+
//! ```
//!
//! Everything above the footer is the *body*, and the data section ends
//! exactly where the body does. The footer is the image's signature:
//!
//! ```text
//! +------+--------------------------------------------------+
//! |    0 | magic `HLBFSSIG`                                 |
//! |    8 | footer version (u32)                             |
//! |   12 | algorithm (u32, 1 = Ed25519)                     |
//! |   16 | signer public key (32 bytes)                     |
//! |   48 | signature (64 bytes)                             |
//! |  112 | reserved, zero (16 bytes)                        |
//! +------+--------------------------------------------------+
//! ```
//!
//! The signed message is [`SIGNATURE_DOMAIN`] followed by the SHA-256 of
//! the body, so the header is covered. [`Image::parse_signed`] verifies
//! the signature against a set of trusted public keys before it reads a
//! single field of the body.
//!
//! An entry names one object: a bootfs directory, a bootfs file, the init
//! component, or the init component's `argv0`. Path and data offsets are
//! relative to the string table and data sections respectively.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Magic every `helios-bootfs` image starts with.
pub const MAGIC: [u8; 8] = *b"HLBOOTFS";

/// The only image version this reader and writer speak.
///
/// Version 2 is version 1's body followed by a signature footer, with
/// the data section required to end where the body does.
pub const VERSION: u32 = 2;

/// Magic the signature footer starts with.
pub const SIGNATURE_MAGIC: [u8; 8] = *b"HLBFSSIG";

/// The only footer version this reader and writer speak.
pub const SIGNATURE_VERSION: u32 = 1;

/// The algorithm field of an Ed25519 footer.
pub const ALGORITHM_ED25519: u32 = 1;

/// Size of the signature footer every image ends with.
pub const SIGNATURE_FOOTER_BYTES: usize = 128;

/// The domain-separation prefix of the signed message.
pub const SIGNATURE_DOMAIN: &[u8] = b"helios-bootfs-signature-v1";

/// Length of the signed message: the domain and a SHA-256 digest.
pub const SIGNING_MESSAGE_BYTES: usize = SIGNATURE_DOMAIN.len() + 32;

/// A SHA-256 digest of an image body.
pub type Digest256 = [u8; 32];

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

// Footer field offsets.
const FOOTER_VERSION_OFFSET: usize = 8;
const FOOTER_ALGORITHM_OFFSET: usize = 12;
const FOOTER_SIGNER_OFFSET: usize = 16;
const FOOTER_SIGNATURE_OFFSET: usize = 48;
const FOOTER_RESERVED_OFFSET: usize = 112;

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

impl core::fmt::Display for EntryKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Directory => f.write_str("directory"),
            Self::File => f.write_str("file"),
            Self::InitComponent => f.write_str("init-component"),
            Self::InitArgv0 => f.write_str("init-argv0"),
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

/// A short, stable name for a public key in diagnostics: the first eight
/// bytes of its SHA-256, shown as hex.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint([u8; 8]);

impl Fingerprint {
    pub fn of(public_key: &[u8; 32]) -> Self {
        let digest = Sha256::digest(public_key);
        Self(
            digest[..8]
                .try_into()
                .expect("a SHA-256 digest is 32 bytes"),
        )
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

/// The fingerprints of the trusted keys, rendered as a list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintList(pub Vec<Fingerprint>);

impl fmt::Display for FingerprintList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[")?;
        for (index, fingerprint) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{fingerprint}")?;
        }
        f.write_str("]")
    }
}

/// A verified, parsed image and the digest its signature covers.
#[derive(Clone)]
pub struct SignedImage {
    /// The parsed body.
    pub image: Image,
    /// SHA-256 of the body, the value the signature commits to. A later
    /// stage extends a measurement register with it.
    pub digest: Digest256,
    /// The trusted key that signed the image.
    pub signer: [u8; 32],
}

/// A parsed `helios-bootfs` image: a validated view over its body.
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
    /// Verifies a signed `helios-bootfs` image and parses it.
    ///
    /// The body is hashed once and its signature checked against
    /// `trusted_public_keys` before any body field is read, so a refused
    /// image never reaches the shape parser.
    pub fn parse_signed(
        bytes: &'static [u8],
        trusted_public_keys: &[[u8; 32]],
    ) -> Result<SignedImage, BootfsError> {
        let (body, signer, digest) = verify_signature(bytes, trusted_public_keys)?;
        let image = Self::parse(body)?;
        Ok(SignedImage {
            image,
            digest,
            signer,
        })
    }

    /// Parses and validates an image body, the signature footer already
    /// stripped and verified.
    ///
    /// Every failure names the field that was wrong; a parsed image's
    /// entries borrow `bytes` and never read outside it.
    fn parse(bytes: &'static [u8]) -> Result<Self, BootfsError> {
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
        if data.end != bytes.len() {
            return Err(BootfsError::TrailingBytes {
                data_end: data.end as u64,
                body_len: bytes.len() as u64,
            });
        }
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
        // One bring-up allocation bounded by the entry count: the writer
        // sorts directories and files separately, so the paths are not
        // globally ordered and an adjacent-entry comparison cannot catch
        // a duplicate.
        let mut bootfs_paths = BTreeSet::new();
        for index in 0..entry_count {
            let entry = image.entry(index)?;
            match entry.kind {
                EntryKind::InitComponent => init_components += 1,
                EntryKind::InitArgv0 => init_argv0s += 1,
                // The bootfs lookup views are first-match-wins, so a
                // second entry naming a path the table already carries
                // makes the image ambiguous rather than shadowed.
                EntryKind::Directory | EntryKind::File if !bootfs_paths.insert(entry.path) => {
                    return Err(BootfsError::DuplicatePath {
                        index,
                        path: entry.path,
                    });
                }
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
        if !data.is_empty() && matches!(kind, EntryKind::Directory | EntryKind::InitArgv0) {
            return Err(BootfsError::EntryDataUnexpected {
                index,
                kind,
                data_len,
            });
        }
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

/// Serialises `entries` into an unsigned image body.
///
/// The layout is the one [`Image::parse_signed`] reads once a footer is
/// attached: fixed header, entry table, string table, file data, every
/// referenced offset 16-byte aligned. [`sign_image`] or
/// [`attach_signature`] completes it.
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

/// The message a signer signs for `body`.
pub fn signing_message(body: &[u8]) -> [u8; SIGNING_MESSAGE_BYTES] {
    let mut message = [0_u8; SIGNING_MESSAGE_BYTES];
    message[..SIGNATURE_DOMAIN.len()].copy_from_slice(SIGNATURE_DOMAIN);
    message[SIGNATURE_DOMAIN.len()..].copy_from_slice(&Sha256::digest(body));
    message
}

/// Appends the signature footer to `body`, after checking that
/// `signature` is `signer`'s signature over [`signing_message`].
///
/// This is the writer half for a signer that holds no key file: the
/// message goes to the signer, the signature comes back here.
pub fn attach_signature(
    mut body: Vec<u8>,
    signer: &[u8; 32],
    signature: &[u8; 64],
) -> Result<Vec<u8>, BootfsError> {
    let key = VerifyingKey::from_bytes(signer).map_err(|_| BootfsError::InvalidSignerKey)?;
    key.verify_strict(&signing_message(&body), &Signature::from_bytes(signature))
        .map_err(|_| BootfsError::SignatureMismatch)?;
    body.reserve(SIGNATURE_FOOTER_BYTES);
    body.extend_from_slice(&SIGNATURE_MAGIC);
    body.extend_from_slice(&SIGNATURE_VERSION.to_le_bytes());
    body.extend_from_slice(&ALGORITHM_ED25519.to_le_bytes());
    body.extend_from_slice(signer);
    body.extend_from_slice(signature);
    body.extend_from_slice(&[0_u8; SIGNATURE_FOOTER_BYTES - FOOTER_RESERVED_OFFSET]);
    Ok(body)
}

/// Signs `body` with `key` and appends the footer.
pub fn sign_image(body: Vec<u8>, key: &SigningKey) -> Vec<u8> {
    let signature = key.sign(&signing_message(&body)).to_bytes();
    attach_signature(body, &VerifyingKey::from(key).to_bytes(), &signature)
        .expect("a signature just made with the key verifies under its public key")
}

/// [`sign_image`] for a key given as its 32-byte secret seed.
pub fn sign_image_with_secret_key_bytes(body: Vec<u8>, secret_key: &[u8; 32]) -> Vec<u8> {
    sign_image(body, &SigningKey::from_bytes(secret_key))
}

/// Splits `bytes` into body and footer and verifies the footer's
/// signature, returning the body, the signer and the body's digest.
fn verify_signature(
    bytes: &'static [u8],
    trusted_public_keys: &[[u8; 32]],
) -> Result<(&'static [u8], [u8; 32], Digest256), BootfsError> {
    let Some(body_len) = bytes.len().checked_sub(SIGNATURE_FOOTER_BYTES) else {
        return Err(BootfsError::MissingSignature);
    };
    let (body, footer) = bytes.split_at(body_len);
    if footer[..8] != SIGNATURE_MAGIC {
        return Err(BootfsError::MissingSignature);
    }
    let version = read_u32(footer, FOOTER_VERSION_OFFSET);
    let algorithm = read_u32(footer, FOOTER_ALGORITHM_OFFSET);
    if version != SIGNATURE_VERSION || algorithm != ALGORITHM_ED25519 {
        return Err(BootfsError::UnsupportedSignature { version, algorithm });
    }
    if footer[FOOTER_RESERVED_OFFSET..]
        .iter()
        .any(|&byte| byte != 0)
    {
        return Err(BootfsError::FooterReserved);
    }
    let signer: [u8; 32] = footer[FOOTER_SIGNER_OFFSET..FOOTER_SIGNATURE_OFFSET]
        .try_into()
        .expect("signer field is 32 bytes");
    if !trusted_public_keys.contains(&signer) {
        return Err(BootfsError::UntrustedSigner {
            signer: Fingerprint::of(&signer),
            trusted: FingerprintList(trusted_public_keys.iter().map(Fingerprint::of).collect()),
        });
    }
    let signature: [u8; 64] = footer[FOOTER_SIGNATURE_OFFSET..FOOTER_RESERVED_OFFSET]
        .try_into()
        .expect("signature field is 64 bytes");
    let key = VerifyingKey::from_bytes(&signer).map_err(|_| BootfsError::InvalidSignerKey)?;
    let digest: Digest256 = Sha256::digest(body).into();
    let mut message = [0_u8; SIGNING_MESSAGE_BYTES];
    message[..SIGNATURE_DOMAIN.len()].copy_from_slice(SIGNATURE_DOMAIN);
    message[SIGNATURE_DOMAIN.len()..].copy_from_slice(&digest);
    key.verify_strict(&message, &Signature::from_bytes(&signature))
        .map_err(|_| BootfsError::SignatureMismatch)?;
    Ok((body, signer, digest))
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
    #[error("bootfs image carries no signature footer")]
    MissingSignature,
    #[error(
        "bootfs signature footer version {version} algorithm {algorithm} is not supported, \
         expected version {SIGNATURE_VERSION} algorithm {ALGORITHM_ED25519}"
    )]
    UnsupportedSignature { version: u32, algorithm: u32 },
    #[error("bootfs signature footer reserved bytes are not zero")]
    FooterReserved,
    #[error("bootfs signer public key is not a valid Ed25519 point")]
    InvalidSignerKey,
    #[error("bootfs signer {signer} is not among the trusted keys {trusted}")]
    UntrustedSigner {
        signer: Fingerprint,
        trusted: FingerprintList,
    },
    #[error("bootfs signature does not match the image body")]
    SignatureMismatch,
    #[error("data section ends at {data_end:#x} but the body is {body_len:#x} bytes")]
    TrailingBytes { data_end: u64, body_len: u64 },
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
    #[error("{kind} entry {index} carries {data_len} data bytes; its kind carries none")]
    EntryDataUnexpected {
        index: usize,
        kind: EntryKind,
        data_len: u64,
    },
    #[error("bootfs image entry {index} repeats the path {path:?}")]
    DuplicatePath { index: usize, path: &'static str },
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
            Image::parse(corrupt(VERSION_OFFSET, &1_u32.to_le_bytes())),
            Err(BootfsError::BadVersion { found: 1 })
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
    fn directory_with_data_fails() {
        let bytes = image(&[
            WriteEntry {
                kind: EntryKind::Directory,
                path: "bin/empty",
                data: b"stowaway",
                modified_nanos: 0,
            },
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
        ]);
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::EntryDataUnexpected {
                index: 0,
                kind: EntryKind::Directory,
                data_len: 8,
            })
        ));
    }

    #[test]
    fn init_argv0_with_data_fails() {
        let bytes = image(&[
            WriteEntry {
                kind: EntryKind::InitComponent,
                path: "init",
                data: b"i",
                modified_nanos: 0,
            },
            WriteEntry {
                kind: EntryKind::InitArgv0,
                path: "/init.wasm",
                data: b"stowaway",
                modified_nanos: 0,
            },
        ]);
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::EntryDataUnexpected {
                index: 1,
                kind: EntryKind::InitArgv0,
                data_len: 8,
            })
        ));
    }

    #[test]
    fn duplicate_paths_fail() {
        let bytes = image(&[
            WriteEntry {
                kind: EntryKind::File,
                path: "bin/tool",
                data: b"first",
                modified_nanos: 0,
            },
            WriteEntry {
                kind: EntryKind::File,
                path: "bin/tool",
                data: b"second",
                modified_nanos: 0,
            },
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
        ]);
        assert!(matches!(
            Image::parse(bytes),
            Err(BootfsError::DuplicatePath {
                index: 1,
                path: "bin/tool",
            })
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

    // ---- signature footer ----

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[0x42; 32])
    }

    fn other_key() -> SigningKey {
        SigningKey::from_bytes(&[0x43; 32])
    }

    fn trusted() -> [[u8; 32]; 1] {
        [VerifyingKey::from(&signing_key()).to_bytes()]
    }

    fn signed() -> Vec<u8> {
        sign_image(fixture().to_vec(), &signing_key())
    }

    fn leak(bytes: Vec<u8>) -> &'static [u8] {
        Box::leak(bytes.into_boxed_slice())
    }

    fn flipped(bytes: &[u8], offset: usize) -> &'static [u8] {
        let mut bytes = bytes.to_vec();
        bytes[offset] ^= 0x01;
        leak(bytes)
    }

    fn u64_at(bytes: &[u8], offset: usize) -> usize {
        read_u64(bytes, offset) as usize
    }

    #[test]
    fn signed_image_verifies() {
        let bytes = leak(signed());
        let verified = Image::parse_signed(bytes, &trusted()).expect("signed image verifies");
        assert_eq!(verified.image.entries().count(), 4);
        assert_eq!(verified.digest, <[u8; 32]>::from(Sha256::digest(fixture())));
        assert_eq!(verified.signer, trusted()[0]);
    }

    #[test]
    fn signing_message_is_domain_then_digest() {
        let message = signing_message(fixture());
        assert!(message.starts_with(b"helios-bootfs-signature-v1"));
        assert_eq!(
            &message[SIGNATURE_DOMAIN.len()..],
            &Sha256::digest(fixture())[..]
        );
    }

    #[test]
    fn attached_signature_matches_signed_image() {
        let key = signing_key();
        let signature = key.sign(&signing_message(fixture())).to_bytes();
        let attached = attach_signature(
            fixture().to_vec(),
            &VerifyingKey::from(&key).to_bytes(),
            &signature,
        )
        .expect("a valid signature attaches");
        assert_eq!(attached, signed());
        assert!(matches!(
            attach_signature(
                fixture().to_vec(),
                &VerifyingKey::from(&key).to_bytes(),
                &[0; 64]
            ),
            Err(BootfsError::SignatureMismatch)
        ));
    }

    #[test]
    fn flipped_byte_in_each_body_section_is_refused() {
        let signed = signed();
        let body = fixture();
        let entry_table = HEADER_BYTES;
        let string_table = u64_at(body, STRING_TABLE_OFFSET_OFFSET);
        let data = u64_at(body, DATA_OFFSET_OFFSET);
        for (section, offset) in [
            ("header", VERSION_OFFSET),
            ("entry table", entry_table),
            ("string table", string_table),
            ("data", data),
        ] {
            assert!(
                matches!(
                    Image::parse_signed(flipped(&signed, offset), &trusted()),
                    Err(BootfsError::SignatureMismatch)
                ),
                "flipped {section} byte"
            );
        }
    }

    #[test]
    fn flipped_byte_in_each_footer_field_is_refused() {
        let signed = signed();
        let footer = signed.len() - SIGNATURE_FOOTER_BYTES;
        let at = |field: usize| flipped(&signed, footer + field);
        assert!(matches!(
            Image::parse_signed(at(0), &trusted()),
            Err(BootfsError::MissingSignature)
        ));
        assert!(matches!(
            Image::parse_signed(at(FOOTER_VERSION_OFFSET), &trusted()),
            Err(BootfsError::UnsupportedSignature { version: 0, .. })
        ));
        assert!(matches!(
            Image::parse_signed(at(FOOTER_ALGORITHM_OFFSET), &trusted()),
            Err(BootfsError::UnsupportedSignature { algorithm: 0, .. })
        ));
        assert!(matches!(
            Image::parse_signed(at(FOOTER_SIGNER_OFFSET), &trusted()),
            Err(BootfsError::UntrustedSigner { .. })
        ));
        assert!(matches!(
            Image::parse_signed(at(FOOTER_SIGNATURE_OFFSET), &trusted()),
            Err(BootfsError::SignatureMismatch)
        ));
        assert!(matches!(
            Image::parse_signed(at(FOOTER_RESERVED_OFFSET), &trusted()),
            Err(BootfsError::FooterReserved)
        ));
    }

    #[test]
    fn untrusted_signer_is_refused_with_fingerprints() {
        let bytes = leak(sign_image(fixture().to_vec(), &other_key()));
        let Err(BootfsError::UntrustedSigner {
            signer,
            trusted: roots,
        }) = Image::parse_signed(bytes, &trusted())
        else {
            panic!("a foreign signer must be refused");
        };
        assert_eq!(
            signer,
            Fingerprint::of(&VerifyingKey::from(&other_key()).to_bytes())
        );
        assert_eq!(roots.0, [Fingerprint::of(&trusted()[0])]);
    }

    #[test]
    fn trailing_bytes_after_the_data_section_are_refused() {
        // Signed correctly, so only the shape check can refuse it.
        let mut body = fixture().to_vec();
        body.extend_from_slice(&[0; 16]);
        let bytes = leak(sign_image(body, &signing_key()));
        assert!(matches!(
            Image::parse_signed(bytes, &trusted()),
            Err(BootfsError::TrailingBytes { .. })
        ));
    }

    #[test]
    fn version_one_image_is_refused() {
        // A version-1 image is a body with no footer.
        let mut v1 = fixture().to_vec();
        v1[VERSION_OFFSET..VERSION_OFFSET + 4].copy_from_slice(&1_u32.to_le_bytes());
        assert!(matches!(
            Image::parse_signed(leak(v1.clone()), &trusted()),
            Err(BootfsError::MissingSignature)
        ));
        // Signed with a v1 header it still fails on the version.
        assert!(matches!(
            Image::parse_signed(leak(sign_image(v1, &signing_key())), &trusted()),
            Err(BootfsError::BadVersion { found: 1 })
        ));
    }

    #[test]
    fn short_input_has_no_signature() {
        assert!(matches!(
            Image::parse_signed(&[0; 16], &trusted()),
            Err(BootfsError::MissingSignature)
        ));
    }
}
