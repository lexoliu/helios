//! The superblock: which copy to trust, and what it says.

use alloc::vec;

use tracing::warn;

use crate::csum::ChecksumType;
use crate::error::{Error, SuperblockDefect};
use crate::ondisk::{
    CSUM_FIELD_BYTES, MAX_LEVEL, RawSuperblock, SUPERBLOCK_BYTES, SUPERBLOCK_MAGIC,
    SYS_CHUNK_ARRAY_BYTES, view,
};
use crate::source::BlockSource;

/// Device offsets of the superblock copies: the primary, then the backups
/// at 64 MiB and 256 GiB where the device is long enough to hold them.
pub const MIRROR_OFFSETS: [u64; 3] = [64 << 10, 64 << 20, 256 << 30];

/// Every incompat bit the format defines, its name, and whether this crate
/// reads filesystems that set it.
///
/// A bit that is defined but not implemented refuses the mount, and so does
/// a bit the format does not define yet: either way the filesystem stores
/// something this reader would misread.
const INCOMPAT_FEATURES: [(u64, &str, bool); 17] = [
    (1 << 0, "mixed-backref", true),
    (1 << 1, "default-subvol", true),
    (1 << 2, "mixed-bg", true),
    (1 << 3, "compress-lzo", true),
    (1 << 4, "compress-zstd", true),
    (1 << 5, "big-metadata", true),
    (1 << 6, "extref", true),
    (1 << 7, "raid56", false),
    (1 << 8, "skinny-metadata", true),
    (1 << 9, "no-holes", true),
    (1 << 10, "metadata-uuid", true),
    (1 << 11, "raid1c34", false),
    (1 << 12, "zoned", false),
    (1 << 13, "extent-tree-v2", false),
    (1 << 14, "raid-stripe-tree", false),
    (1 << 16, "squota", false),
    (1 << 17, "remap-tree", false),
];

const INCOMPAT_METADATA_UUID: u64 = 1 << 10;

/// A tree root the superblock points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootPointer {
    pub bytenr: u64,
    pub level: u8,
    pub generation: u64,
}

/// A verified superblock.
///
/// Every field has been checked against the format's constraints: the
/// sizes are powers of two in the format's range, the tree roots are
/// sector aligned and within the deepest level, and the device item
/// belongs to this filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Superblock {
    fsid: [u8; 16],
    metadata_fsid: [u8; 16],
    generation: u64,
    root: RootPointer,
    chunk_root: RootPointer,
    log_root: u64,
    total_bytes: u64,
    num_devices: u64,
    sectorsize: u32,
    nodesize: u32,
    csum_type: ChecksumType,
    incompat_flags: u64,
    compat_ro_flags: u64,
    label: [u8; 256],
    devid: u64,
    device_bytes: u64,
    sys_chunk_array_len: usize,
    mirror: usize,
}

impl Superblock {
    /// Validates one superblock copy read from `offset`.
    pub(crate) fn parse(bytes: &[u8], offset: u64) -> Result<Self, SuperblockDefect> {
        let raw: &RawSuperblock = view(bytes).ok_or(SuperblockDefect::Magic)?;
        if raw.magic.get() != SUPERBLOCK_MAGIC {
            return Err(SuperblockDefect::Magic);
        }
        if raw.bytenr.get() != offset {
            return Err(SuperblockDefect::Bytenr {
                recorded: raw.bytenr.get(),
            });
        }
        let csum_type = ChecksumType::from_raw(raw.csum_type.get())
            .ok_or(SuperblockDefect::ChecksumType(raw.csum_type.get()))?;
        if !csum_type.verifies(&bytes[CSUM_FIELD_BYTES..SUPERBLOCK_BYTES], &raw.csum) {
            return Err(SuperblockDefect::Checksum);
        }

        let sectorsize = raw.sectorsize.get();
        if !sectorsize.is_power_of_two() || !(4096..=65536).contains(&sectorsize) {
            return Err(SuperblockDefect::SectorSize(sectorsize));
        }
        let nodesize = raw.nodesize.get();
        if !nodesize.is_power_of_two() || nodesize < sectorsize || nodesize > 65536 {
            return Err(SuperblockDefect::NodeSize {
                nodesize,
                sectorsize,
            });
        }
        let sector = u64::from(sectorsize);
        for (field, value) in [
            ("root", raw.root.get()),
            ("chunk_root", raw.chunk_root.get()),
            ("log_root", raw.log_root.get()),
            ("total_bytes", raw.total_bytes.get()),
        ] {
            if !value.is_multiple_of(sector) {
                return Err(SuperblockDefect::Unaligned { field, value });
            }
        }
        for (field, level) in [
            ("root_level", raw.root_level),
            ("chunk_root_level", raw.chunk_root_level),
        ] {
            if level >= MAX_LEVEL {
                return Err(SuperblockDefect::Level { field, level });
            }
        }
        let sys_chunk_array_size = raw.sys_chunk_array_size.get();
        let sys_chunk_array_len = usize::try_from(sys_chunk_array_size)
            .ok()
            .filter(|len| *len <= SYS_CHUNK_ARRAY_BYTES)
            .ok_or(SuperblockDefect::SysChunkArraySize(sys_chunk_array_size))?;

        let incompat_flags = raw.incompat_flags.get();
        let metadata_fsid = if incompat_flags & INCOMPAT_METADATA_UUID != 0 {
            raw.metadata_uuid
        } else {
            raw.fsid
        };
        if raw.dev_item.fsid != metadata_fsid {
            return Err(SuperblockDefect::DeviceFsid);
        }
        let device_bytes = raw.dev_item.total_bytes.get();
        if device_bytes == 0 || !device_bytes.is_multiple_of(sector) {
            return Err(SuperblockDefect::DeviceSize(device_bytes));
        }

        let mirror = MIRROR_OFFSETS
            .iter()
            .position(|mirror| *mirror == offset)
            .unwrap_or(0);
        Ok(Self {
            fsid: raw.fsid,
            metadata_fsid,
            generation: raw.generation.get(),
            root: RootPointer {
                bytenr: raw.root.get(),
                level: raw.root_level,
                generation: raw.generation.get(),
            },
            chunk_root: RootPointer {
                bytenr: raw.chunk_root.get(),
                level: raw.chunk_root_level,
                generation: raw.chunk_root_generation.get(),
            },
            log_root: raw.log_root.get(),
            total_bytes: raw.total_bytes.get(),
            num_devices: raw.num_devices.get(),
            sectorsize,
            nodesize,
            csum_type,
            incompat_flags,
            compat_ro_flags: raw.compat_ro_flags.get(),
            label: raw.label,
            devid: raw.dev_item.devid.get(),
            device_bytes,
            sys_chunk_array_len,
            mirror,
        })
    }

    /// Refuses a valid superblock describing a filesystem this crate does
    /// not read.
    pub(crate) fn check_supported(&self) -> Result<(), Error> {
        let mut unknown = self.incompat_flags;
        for (bit, name, supported) in INCOMPAT_FEATURES {
            if self.incompat_flags & bit != 0 && !supported {
                return Err(Error::UnsupportedIncompat { name, bit });
            }
            unknown &= !bit;
        }
        if unknown != 0 {
            let bit = 1_u64 << unknown.trailing_zeros();
            return Err(Error::UnsupportedIncompat {
                name: "undefined",
                bit,
            });
        }
        if self.num_devices != 1 {
            return Err(Error::MultiDevice {
                devices: self.num_devices,
            });
        }
        if self.log_root != 0 {
            return Err(Error::LogReplayRequired {
                log_root: self.log_root,
            });
        }
        Ok(())
    }

    /// The filesystem's UUID, the one `blkid` reports.
    pub fn fsid(&self) -> [u8; 16] {
        self.fsid
    }

    /// The UUID every tree block is stamped with: the `metadata_uuid` when
    /// the filesystem carries one, otherwise the fsid.
    pub fn metadata_fsid(&self) -> [u8; 16] {
        self.metadata_fsid
    }

    /// The transaction this superblock committed.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn root_tree(&self) -> RootPointer {
        self.root
    }

    pub fn chunk_tree(&self) -> RootPointer {
        self.chunk_root
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn sectorsize(&self) -> u32 {
        self.sectorsize
    }

    pub fn nodesize(&self) -> u32 {
        self.nodesize
    }

    pub fn csum_type(&self) -> ChecksumType {
        self.csum_type
    }

    pub fn incompat_flags(&self) -> u64 {
        self.incompat_flags
    }

    pub fn compat_ro_flags(&self) -> u64 {
        self.compat_ro_flags
    }

    /// The label, without its NUL padding.
    pub fn label(&self) -> &[u8] {
        let end = self
            .label
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.label.len());
        &self.label[..end]
    }

    /// The id the filesystem's one device is known by in chunk stripes.
    pub fn devid(&self) -> u64 {
        self.devid
    }

    /// Bytes of the device the filesystem may use.
    pub fn device_bytes(&self) -> u64 {
        self.device_bytes
    }

    /// Which copy this is: 0 for the primary, 1 and 2 for the backups.
    pub fn mirror(&self) -> usize {
        self.mirror
    }

    pub(crate) fn sys_chunk_array_len(&self) -> usize {
        self.sys_chunk_array_len
    }
}

/// The chosen superblock and the raw bytes it was parsed from, which still
/// hold the bootstrap chunk array.
pub(crate) struct SelectedSuperblock {
    pub(crate) superblock: Superblock,
    pub(crate) bytes: alloc::vec::Vec<u8>,
}

/// Reads the superblock copies and chooses the one to mount from.
///
/// The primary is authoritative whenever it verifies, which is what the
/// kernel mounts from. When it does not, the newest backup copy that
/// verifies takes its place: a torn or damaged primary is exactly what the
/// backups exist for.
pub(crate) async fn select<S: BlockSource>(source: &S) -> Result<SelectedSuperblock, Error> {
    let mut bytes = vec![0_u8; SUPERBLOCK_BYTES];
    let primary = read_copy(source, MIRROR_OFFSETS[0], &mut bytes).await;
    let primary_defect = match primary {
        Ok(superblock) => return Ok(SelectedSuperblock { superblock, bytes }),
        Err(defect) => defect,
    };

    let mut best: Option<SelectedSuperblock> = None;
    let mut backups_checked = 0_u8;
    for offset in &MIRROR_OFFSETS[1..] {
        let mut candidate = vec![0_u8; SUPERBLOCK_BYTES];
        match read_copy(source, *offset, &mut candidate).await {
            Err(SuperblockDefect::BeyondDevice) => continue,
            Err(_) => backups_checked += 1,
            Ok(superblock) => {
                backups_checked += 1;
                let newer = best
                    .as_ref()
                    .is_none_or(|chosen| superblock.generation > chosen.superblock.generation);
                if newer {
                    best = Some(SelectedSuperblock {
                        superblock,
                        bytes: candidate,
                    });
                }
            }
        }
    }
    match best {
        Some(selected) => {
            warn!(
                mirror = selected.superblock.mirror,
                generation = selected.superblock.generation,
                primary = %primary_defect,
                "btrfs: primary superblock rejected, mounting from a backup copy"
            );
            Ok(selected)
        }
        None => Err(Error::NoValidSuperblock {
            primary: primary_defect,
            backups_checked,
        }),
    }
}

async fn read_copy<S: BlockSource>(
    source: &S,
    offset: u64,
    bytes: &mut [u8],
) -> Result<Superblock, SuperblockDefect> {
    let fits = offset
        .checked_add(SUPERBLOCK_BYTES as u64)
        .is_some_and(|end| end <= source.size_bytes());
    if !fits {
        return Err(SuperblockDefect::BeyondDevice);
    }
    source
        .read_at(offset, bytes)
        .await
        .map_err(SuperblockDefect::Unreadable)?;
    Superblock::parse(bytes, offset)
}
