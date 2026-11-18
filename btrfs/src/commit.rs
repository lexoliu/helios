//! Superblock publication image. Preserve fields the transaction does not
//! change, while rotating the typed backup roots and checksumming each mirror.

use alloc::sync::Arc;
use zerocopy::{FromZeros, IntoBytes};

use crate::ondisk::{
    RawChunk, RawKey, RawRootBackup, RawStripe, RawSuperblock, SUPERBLOCK_BYTES,
    SYS_CHUNK_ARRAY_BYTES, objectid, view,
};
use crate::roots::Roots;
use crate::transaction::chunks::ChunkError;
use crate::tree::TreeRoot;
use crate::tree::write::WriteError;
use crate::{ChecksumType, Compression, Key};

#[derive(Clone)]
pub(crate) struct CommitImage {
    pub(crate) bytes: Arc<[u8; SUPERBLOCK_BYTES]>,
}

impl CommitImage {
    pub(crate) fn superblock(
        &mut self,
        algorithm: ChecksumType,
    ) -> Result<crate::Superblock, WriteError> {
        let primary = crate::superblock::MIRROR_OFFSETS[0];
        Ok(crate::Superblock::parse(
            self.at(primary, algorithm),
            primary,
        )?)
    }

    pub(crate) fn enable_compression(&mut self, algorithm: Compression) {
        let flag = match algorithm {
            Compression::Zlib => 0,
            Compression::Lzo => 1 << 3, // BTRFS_FEATURE_INCOMPAT_COMPRESS_LZO.
            Compression::Zstd => 1 << 4, // BTRFS_FEATURE_INCOMPAT_COMPRESS_ZSTD.
        };
        let mut raw = *view::<RawSuperblock>(&self.bytes[..]).expect("validated superblock image");
        raw.incompat_flags.set(raw.incompat_flags.get() | flag);
        Arc::make_mut(&mut self.bytes)[..size_of::<RawSuperblock>()]
            .copy_from_slice(raw.as_bytes());
    }

    /// Records `bytes` more allocated on the device in the superblock's
    /// embedded device item, which the chunk tree's device item mirrors.
    pub(crate) fn add_device_used(&mut self, bytes: u64) {
        let mut raw = *view::<RawSuperblock>(&self.bytes[..]).expect("validated superblock image");
        raw.dev_item
            .bytes_used
            .set(raw.dev_item.bytes_used.get() + bytes);
        Arc::make_mut(&mut self.bytes)[..size_of::<RawSuperblock>()]
            .copy_from_slice(raw.as_bytes());
    }

    /// Records `bytes` fewer allocated on the device (`add_device_used`'s
    /// counterpart, for a removed chunk).
    pub(crate) fn sub_device_used(&mut self, bytes: u64) {
        let mut raw = *view::<RawSuperblock>(&self.bytes[..]).expect("validated superblock image");
        raw.dev_item.bytes_used.set(
            raw.dev_item
                .bytes_used
                .get()
                .checked_sub(bytes)
                .expect("a removed chunk was counted as allocated"),
        );
        Arc::make_mut(&mut self.bytes)[..size_of::<RawSuperblock>()]
            .copy_from_slice(raw.as_bytes());
    }

    /// Removes the SYSTEM chunk starting at `logical` from the bootstrap
    /// array, as Linux's `remove_sys_chunk_from_array` does: the following
    /// entries move down over it.
    pub(crate) fn remove_system_chunk(&mut self, logical: u64) -> Result<(), ChunkError> {
        let mut raw = *view::<RawSuperblock>(&self.bytes[..]).expect("validated superblock image");
        let used = raw.sys_chunk_array_size.get() as usize;
        let mut at = 0;
        while at < used {
            let key: &RawKey = view(&raw.sys_chunk_array[at..used])
                .ok_or(ChunkError::Truncated { len: used - at })?;
            let chunk: &RawChunk = view(&raw.sys_chunk_array[at + size_of::<RawKey>()..used])
                .ok_or(ChunkError::Truncated { len: used - at })?;
            let entry = size_of::<RawKey>()
                + size_of::<RawChunk>()
                + usize::from(chunk.num_stripes.get()) * size_of::<RawStripe>();
            if key.offset.get() == logical {
                raw.sys_chunk_array.copy_within(at + entry..used, at);
                raw.sys_chunk_array[used - entry..used].fill(0);
                raw.sys_chunk_array_size.set((used - entry) as u32);
                Arc::make_mut(&mut self.bytes)[..size_of::<RawSuperblock>()]
                    .copy_from_slice(raw.as_bytes());
                return Ok(());
            }
            at += entry;
        }
        Err(ChunkError::NotInSystemArray { logical })
    }

    /// Appends a SYSTEM chunk to the bootstrap array the superblock carries,
    /// as Linux's `btrfs_add_system_chunk`: the key, then the chunk item.
    pub(crate) fn append_system_chunk(&mut self, key: Key, chunk: &[u8]) -> Result<(), ChunkError> {
        let mut raw = *view::<RawSuperblock>(&self.bytes[..]).expect("validated superblock image");
        let used = raw.sys_chunk_array_size.get() as usize;
        let needed = size_of::<RawKey>() + chunk.len();
        if used + needed > SYS_CHUNK_ARRAY_BYTES {
            return Err(ChunkError::SystemArrayFull {
                used,
                needed,
                capacity: SYS_CHUNK_ARRAY_BYTES,
            });
        }
        let mut raw_key = RawKey::new_zeroed();
        raw_key.objectid.set(key.objectid);
        raw_key.item_type = key.item_type;
        raw_key.offset.set(key.offset);
        raw.sys_chunk_array[used..used + size_of::<RawKey>()].copy_from_slice(raw_key.as_bytes());
        raw.sys_chunk_array[used + size_of::<RawKey>()..used + needed].copy_from_slice(chunk);
        raw.sys_chunk_array_size.set((used + needed) as u32);
        Arc::make_mut(&mut self.bytes)[..size_of::<RawSuperblock>()]
            .copy_from_slice(raw.as_bytes());
        Ok(())
    }

    pub(crate) fn prepare(
        &mut self,
        roots: &Roots,
        chunk_root: TreeRoot,
        used: u64,
    ) -> Result<(), WriteError> {
        let mut raw = *view::<RawSuperblock>(&self.bytes[..]).expect("validated superblock image");
        raw.chunk_root.set(chunk_root.bytenr);
        raw.chunk_root_level = chunk_root.level;
        raw.chunk_root_generation.set(chunk_root.generation);
        let extent = roots.get(objectid::EXTENT_TREE)?;
        let fs = roots.get(objectid::FS_TREE)?;
        let device = roots.get(objectid::DEV_TREE)?;
        let checksum = roots.get(objectid::CSUM_TREE)?;
        let generation = roots.root.generation;
        raw.generation.set(generation);
        raw.root.set(roots.root.bytenr);
        raw.root_level = roots.root.level;
        raw.bytes_used.set(used);
        // Every commit holds what any log tree held, so it points at none,
        // as `btrfs_commit_transaction` clears them.
        raw.log_root.set(0);
        raw.log_root_level = 0;
        raw.flags.set(raw.flags.get() | 1); // BTRFS_SUPER_FLAG_WRITTEN.
        // Existing v1 free-space cache inodes are stale after our CoW edits.
        raw.cache_generation.set(0);
        raw.uuid_tree_generation.set(generation);
        if raw.compat_ro_flags.get() & 1 != 0 {
            raw.compat_ro_flags.set(raw.compat_ro_flags.get() | 2); // FREE_SPACE_TREE_VALID.
        }
        let offset = size_of::<RawSuperblock>();
        let old = view::<[RawRootBackup; 4]>(&self.bytes[offset..])
            .expect("four backup roots fit superblock");
        let latest = old
            .iter()
            .enumerate()
            .max_by_key(|(_, root)| root.tree_generation.get())
            .map(|(index, _)| index)
            .expect("four roots");
        let slot = if old[latest].tree_generation.get() == generation {
            latest
        } else {
            (latest + 1) % old.len()
        };
        let mut backup = RawRootBackup::new_zeroed();
        backup.tree = raw.root;
        backup.tree_generation = raw.generation;
        backup.chunk = raw.chunk_root;
        backup.chunk_generation = raw.chunk_root_generation;
        backup.extent.set(extent.bytenr);
        backup.extent_generation.set(extent.generation);
        backup.fs.set(fs.bytenr);
        backup.fs_generation.set(fs.generation);
        backup.device.set(device.bytenr);
        backup.device_generation.set(device.generation);
        backup.checksum.set(checksum.bytenr);
        backup.checksum_generation.set(checksum.generation);
        backup.total_bytes = raw.total_bytes;
        backup.bytes_used = raw.bytes_used;
        backup.devices = raw.num_devices;
        backup.tree_level = raw.root_level;
        backup.chunk_level = raw.chunk_root_level;
        backup.extent_level = extent.level;
        backup.fs_level = fs.level;
        backup.device_level = device.level;
        backup.checksum_level = checksum.level;
        let start = offset + slot * size_of::<RawRootBackup>();
        Arc::make_mut(&mut self.bytes)[start..start + size_of::<RawRootBackup>()]
            .copy_from_slice(backup.as_bytes());
        Arc::make_mut(&mut self.bytes)[..offset].copy_from_slice(raw.as_bytes());
        Ok(())
    }

    pub(crate) fn at(&mut self, offset: u64, algorithm: ChecksumType) -> &[u8; SUPERBLOCK_BYTES] {
        let mut raw = *view::<RawSuperblock>(&self.bytes[..]).expect("validated superblock image");
        raw.bytenr.set(offset);
        raw.csum.fill(0);
        Arc::make_mut(&mut self.bytes)[..size_of::<RawSuperblock>()]
            .copy_from_slice(raw.as_bytes());
        let digest = algorithm.digest(&self.bytes[32..]);
        Arc::make_mut(&mut self.bytes)[..digest.as_bytes().len()]
            .copy_from_slice(digest.as_bytes());
        &self.bytes
    }
}
