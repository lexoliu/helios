//! The four checksum algorithms the format defines, and the CRC32C name
//! hash directory and xattr keys are stored under.

use blake2::Blake2b256;
use crc::{CRC_32_ISCSI, Crc, Table};
use sha2::{Digest as _, Sha256};
use zrip_core::xxhash::xxh64;

/// CRC32C (Castagnoli). Sixteen-way slicing: every tree block and every
/// data sector the filesystem reads passes through it.
const CASTAGNOLI: Crc<u32, Table<16>> = Crc::<u32, Table<16>>::new(&CRC_32_ISCSI);

/// Bytes of the longest digest the format stores.
pub const MAX_DIGEST_BYTES: usize = 32;

/// The checksum algorithm the superblock's `csum_type` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksumType {
    Crc32c,
    XxHash64,
    Sha256,
    Blake2b,
}

impl ChecksumType {
    /// The algorithm `csum_type` names, or `None` for a value the format
    /// does not define.
    pub const fn from_raw(raw: u16) -> Option<Self> {
        match raw {
            0 => Some(Self::Crc32c),
            1 => Some(Self::XxHash64),
            2 => Some(Self::Sha256),
            3 => Some(Self::Blake2b),
            _ => None,
        }
    }

    /// Bytes of one stored digest.
    pub const fn size(self) -> usize {
        match self {
            Self::Crc32c => 4,
            Self::XxHash64 => 8,
            Self::Sha256 | Self::Blake2b => 32,
        }
    }

    /// The digest of `data`, as the format stores it.
    pub fn digest(self, data: &[u8]) -> Digest {
        let mut bytes = [0_u8; MAX_DIGEST_BYTES];
        match self {
            Self::Crc32c => bytes[..4].copy_from_slice(&CASTAGNOLI.checksum(data).to_le_bytes()),
            Self::XxHash64 => bytes[..8].copy_from_slice(&xxh64(data, 0).to_le_bytes()),
            Self::Sha256 => bytes.copy_from_slice(&Sha256::digest(data)),
            Self::Blake2b => bytes.copy_from_slice(&Blake2b256::digest(data)),
        }
        Digest {
            bytes,
            len: self.size(),
        }
    }

    /// Whether `stored`, a checksum field or a checksum-tree entry, holds
    /// the digest of `data`.
    pub(crate) fn verifies(self, data: &[u8], stored: &[u8]) -> bool {
        let digest = self.digest(data);
        stored.get(..digest.len) == Some(digest.as_bytes())
    }
}

/// A computed checksum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Digest {
    bytes: [u8; MAX_DIGEST_BYTES],
    len: usize,
}

impl Digest {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// The hash directory items and xattr items are keyed by: the kernel's
/// `crc32c(~1, name)`, a CRC32C register seeded with `0xfffffffe` and
/// returned without the final inversion.
pub(crate) fn name_hash(name: &[u8]) -> u64 {
    // `crc` takes the seed in the algorithm's unreflected form and inverts
    // on finalisation; undo both to get the kernel's raw register.
    let register = CASTAGNOLI.digest_with_initial((!1_u32).reverse_bits());
    let mut digest = register;
    digest.update(name);
    u64::from(!digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_matches_the_catalogue_check_value() {
        assert_eq!(CASTAGNOLI.checksum(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn name_hash_matches_the_kernel_seeding() {
        // `crc32c(~1, "")` is the seed itself.
        assert_eq!(name_hash(b""), 0xFFFF_FFFE);
        // The hash ferrosys, an independent writer, stores "default" under.
        assert_eq!(
            name_hash(b"default"),
            ferrosys::btrfs::ondisk::name_hash(b"default")
        );
        assert_eq!(
            name_hash(b"hello"),
            ferrosys::btrfs::ondisk::name_hash(b"hello")
        );
    }

    #[test]
    fn digests_have_the_stored_widths() {
        for (kind, len) in [
            (ChecksumType::Crc32c, 4),
            (ChecksumType::XxHash64, 8),
            (ChecksumType::Sha256, 32),
            (ChecksumType::Blake2b, 32),
        ] {
            assert_eq!(kind.digest(b"abc").as_bytes().len(), len);
        }
    }

    #[test]
    fn digests_match_published_vectors() {
        // XXH64("abc", seed 0) = 0x44bc2cf5ad770999.
        assert_eq!(
            ChecksumType::XxHash64.digest(b"abc").as_bytes(),
            0x44bc_2cf5_ad77_0999_u64.to_le_bytes()
        );
        // SHA-256("abc").
        assert_eq!(
            ChecksumType::Sha256.digest(b"abc").as_bytes()[..4],
            [0xba, 0x78, 0x16, 0xbf]
        );
        // BLAKE2b-256("abc").
        assert_eq!(
            ChecksumType::Blake2b.digest(b"abc").as_bytes()[..4],
            [0xbd, 0xdd, 0x81, 0x3c]
        );
    }
}
