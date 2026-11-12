//! The memory types a user mapping of a device region takes.
//!
//! A 4 KiB leaf entry selects one of eight `IA32_PAT` entries with
//! `index = PAT << 2 | PCD << 1 | PWT` (Intel SDM Vol. 3A, Table 13-11
//! "Selection of PAT Entries with PAT, PCD, and PWT Flags"). This backend
//! never writes `IA32_PAT`, so the table is the power-up value of SDM
//! Vol. 3A Table 13-12 "Memory Type Setting of PAT Entries Following a
//! Power-up or Reset": entry 0 is write-back and entry 3 (`PWT | PCD`) is
//! uncacheable, the encoding the kernel's own register-file mappings in
//! `smp.rs` already use. The two kinds of [`MemoryKind`] need exactly those
//! two entries, and the `PAT` bit is never set.

use helios_hal::device::{DeviceRegionAttributes, MemoryKind};
use x86_64::structures::paging::PageTableFlags;

/// The bits of a leaf entry that select the memory type: `PWT` and `PCD`.
pub(crate) const MEMORY_TYPE_BITS: PageTableFlags =
    PageTableFlags::WRITE_THROUGH.union(PageTableFlags::NO_CACHE);

/// The `PWT`/`PCD` bits a region's kind calls for: none (write-back) for
/// memory the processor may treat as RAM, both (uncacheable) for a
/// register file.
const fn memory_type_flags(kind: MemoryKind) -> PageTableFlags {
    match kind {
        MemoryKind::Normal => PageTableFlags::empty(),
        MemoryKind::Device => MEMORY_TYPE_BITS,
    }
}

/// The leaf flags that show a device region: present, never executable,
/// writable only when the region is, and the region's memory type.
pub(crate) fn device_leaf_flags(attributes: DeviceRegionAttributes) -> PageTableFlags {
    let mut flags =
        PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE | memory_type_flags(attributes.kind);
    if attributes.writable {
        flags |= PageTableFlags::WRITABLE;
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_register_file_is_uncacheable_and_not_writable() {
        assert_eq!(
            device_leaf_flags(DeviceRegionAttributes::READ_ONLY_REGISTERS),
            PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE | MEMORY_TYPE_BITS
        );
    }

    #[test]
    fn writable_register_file_is_uncacheable_and_writable() {
        assert_eq!(
            device_leaf_flags(DeviceRegionAttributes::REGISTERS),
            PageTableFlags::PRESENT
                | PageTableFlags::NO_EXECUTE
                | PageTableFlags::WRITABLE
                | MEMORY_TYPE_BITS
        );
    }

    #[test]
    fn prefetchable_memory_is_write_back() {
        assert_eq!(
            device_leaf_flags(DeviceRegionAttributes::PREFETCHABLE_MEMORY),
            PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE | PageTableFlags::WRITABLE
        );
    }
}
