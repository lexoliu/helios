//! The firmware's own device tree, found through the UEFI configuration
//! table.
//!
//! The tree Limine hands over is a copy of the firmware's with every
//! `/memory` node deleted, because the executable is meant to take its
//! memory extents from the memory map (`common/protos/limine.c`). That
//! copy cannot describe locality: memory nodes are what `numa-node-id`
//! places memory with. The firmware's original stays where the firmware
//! put it, and the configuration table's `EFI_DTB_TABLE_GUID` entry
//! names its physical address.
//!
//! Every step is read through Limine's direct map, at the base revision
//! this kernel requests:
//!
//! - the system table: Limine returns its direct-map address from base
//!   revision 5 on;
//! - the configuration table array: from base revision 5 on, Limine
//!   guarantees it lies in a `RESERVED_MAPPED` entry, a type the direct
//!   map covers from base revision 4 on;
//! - the tree: Limine guarantees nothing about where the firmware put
//!   it, so the memory-map entry that holds it is checked before a byte
//!   of it is read. EDK2 allocates it in PEI (`ArmVirtPkg`'s
//!   `PlatformPeiLib`) as `EfiBootServicesData`, which Limine reports as
//!   bootloader-reclaimable.
//!
//! Concurrency contract: read on the bootstrap processor before any
//! secondary starts, and never written; nothing here takes a lock.

use core::ptr::NonNull;

use fdt::{Fdt, FdtError};
use limine::memmap::{
    Entry, MEMMAP_ACPI_NVS, MEMMAP_ACPI_RECLAIMABLE, MEMMAP_BOOTLOADER_RECLAIMABLE,
    MEMMAP_RESERVED_MAPPED,
};
use r_efi::efi::{ConfigurationTable, DTB_TABLE_GUID, SYSTEM_TABLE_SIGNATURE, SystemTable};
use thiserror::Error;

/// The memory-map types the firmware's tree may lie in: firmware memory
/// the direct map covers at base revision 4 and later, none of which this
/// kernel ever primes an allocator from (`convert_memory_region` turns
/// only `MEMMAP_USABLE` into usable memory). A tree in usable memory
/// would be one the bootstrap allocator is primed from while the tree is
/// still being read; one in any other type is either outside the direct
/// map or not firmware memory at all.
const FIRMWARE_TREE_MEMORY_TYPES: [u64; 4] = [
    MEMMAP_BOOTLOADER_RECLAIMABLE,
    MEMMAP_RESERVED_MAPPED,
    MEMMAP_ACPI_RECLAIMABLE,
    MEMMAP_ACPI_NVS,
];

/// Why the firmware's own device tree could not be read.
#[derive(Debug, Error)]
pub(crate) enum FirmwareTreeError {
    #[error(
        "Limine gave no EFI system table on a UEFI boot, so the firmware's own device tree \
         cannot be found"
    )]
    NoSystemTable,
    #[error("the EFI system table's signature is {found:#018x}, not `IBI SYST`")]
    SystemTableSignature { found: u64 },
    #[error("the firmware's device tree at {physical:#x} lies in no memory-map entry")]
    OutsideMemoryMap { physical: usize },
    #[error(
        "the firmware's device tree at {physical:#x} lies in Limine memory-map type \
         {memory_type}, which is not firmware memory the direct map covers"
    )]
    MemoryType { physical: usize, memory_type: u64 },
    #[error(
        "the firmware's device tree at {physical:#x} is not a valid blob within its memory-map \
         entry: {reason}"
    )]
    Malformed { physical: usize, reason: FdtError },
}

/// Where the firmware's own device tree is looked up.
pub(super) struct FirmwareTreeLocator {
    /// Limine's EFI system table response, at its direct-map address, or
    /// `None` when Limine gave none.
    system_table: Option<NonNull<SystemTable>>,
    memory_map: &'static [&'static Entry],
    physical_memory_offset: usize,
}

impl FirmwareTreeLocator {
    pub(super) fn new(
        system_table: Option<NonNull<SystemTable>>,
        memory_map: &'static [&'static Entry],
        physical_memory_offset: usize,
    ) -> Self {
        Self {
            system_table,
            memory_map,
            physical_memory_offset,
        }
    }

    /// The firmware's own tree, or `None` when the configuration table
    /// holds no device tree.
    pub(super) fn open(&self) -> Result<Option<Fdt<'static>>, FirmwareTreeError> {
        let system_table = self.system_table.ok_or(FirmwareTreeError::NoSystemTable)?;
        // SAFETY: Limine's response is the direct-map address of the
        // firmware's system table, which base revision 5 and later keep
        // mapped as `RESERVED_MAPPED` for the life of the kernel, and
        // nothing writes it.
        let system_table = unsafe { system_table.as_ref() };
        if system_table.hdr.signature != SYSTEM_TABLE_SIGNATURE {
            return Err(FirmwareTreeError::SystemTableSignature {
                found: system_table.hdr.signature,
            });
        }
        let Some(entry) = self
            .configuration_table(system_table)
            .iter()
            .find(|entry| entry.vendor_guid == DTB_TABLE_GUID)
        else {
            return Ok(None);
        };
        self.tree_at(entry.vendor_table as usize).map(Some)
    }

    /// The configuration table array the system table names.
    fn configuration_table(&self, system_table: &SystemTable) -> &'static [ConfigurationTable] {
        // The system table holds the array's physical address: Limine
        // never calls `SetVirtualAddressMap`, so every address the
        // firmware's tables hold is still the physical one.
        let array = system_table.configuration_table as usize + self.physical_memory_offset;
        // SAFETY: base revision 5 and later map the configuration table
        // array as `RESERVED_MAPPED`, and the system table counts its
        // entries (UEFI 2.10, section 4.3); nothing writes it.
        unsafe {
            core::slice::from_raw_parts(
                array as *const ConfigurationTable,
                system_table.number_of_table_entries,
            )
        }
    }

    /// The tree at `physical`, once its memory-map entry shows the direct
    /// map covers it and no allocator will be handed it.
    fn tree_at(&self, physical: usize) -> Result<Fdt<'static>, FirmwareTreeError> {
        let address = physical as u64;
        let entry = self
            .memory_map
            .iter()
            .find(|entry| (entry.base..entry.base + entry.length).contains(&address))
            .ok_or(FirmwareTreeError::OutsideMemoryMap { physical })?;
        if !FIRMWARE_TREE_MEMORY_TYPES.contains(&entry.type_) {
            return Err(FirmwareTreeError::MemoryType {
                physical,
                memory_type: entry.type_,
            });
        }
        let available = (entry.base + entry.length - address) as usize;
        // SAFETY: the bytes from `physical` to the end of its memory-map
        // entry are firmware memory the direct map covers and nothing
        // writes for the life of the kernel, by the type just checked.
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (physical + self.physical_memory_offset) as *const u8,
                available,
            )
        };
        let malformed = |reason: FdtError| FirmwareTreeError::Malformed { physical, reason };
        // `Fdt::new` refuses a blob whose header claims more bytes than
        // the slice holds, so a tree that runs past its entry is refused
        // here rather than read past the direct map. The second parse
        // holds the borrow to the tree's own bytes.
        let size = Fdt::new(bytes).map_err(malformed)?.total_size();
        Fdt::new(&bytes[..size]).map_err(malformed)
    }
}
