extern crate alloc;

use alloc::sync::Arc;

use acpi::platform::PciConfigRegions;
use acpi::{AcpiTables, Handler};
use pci_types::{ConfigRegionAccess, PciAddress};

pub(crate) type X86Watchdog = i6300esb::I6300EsbWatchdog<EnhancedPciConfigAccess>;

const PCI_ECAM_BUS_SHIFT: u64 = 20;
const PCI_ECAM_DEVICE_SHIFT: u64 = 15;
const PCI_ECAM_FUNCTION_SHIFT: u64 = 12;
const PCI_CONFIG_SPACE_BYTES: u16 = 4096;
const I6300ESB_MMIO_BYTES: usize = 0x10;

#[derive(Clone)]
pub(crate) struct EnhancedPciConfigAccess {
    physical_memory_offset: usize,
    regions: Arc<[PciConfigRegion]>,
}

#[derive(Clone, Copy)]
struct PciConfigRegion {
    base_address: u64,
    segment: u16,
    bus_start: u8,
    bus_end: u8,
}

pub(crate) fn discover<H>(tables: &AcpiTables<H>, physical_memory_offset: usize) -> X86Watchdog
where
    H: Handler,
{
    let Ok(regions) = PciConfigRegions::new(tables) else {
        return i6300esb::disabled();
    };
    let regions = regions
        .regions
        .into_iter()
        .map(|entry| PciConfigRegion {
            base_address: entry.base_address,
            segment: entry.pci_segment_group,
            bus_start: entry.bus_number_start,
            bus_end: entry.bus_number_end,
        })
        .collect::<alloc::vec::Vec<_>>();
    if regions.is_empty() {
        return i6300esb::disabled();
    }

    let access = EnhancedPciConfigAccess {
        physical_memory_offset,
        regions: Arc::<[PciConfigRegion]>::from(regions),
    };
    i6300esb::discover_watchdog(access, 0..=u8::MAX, |physical_base| {
        crate::smp::map_mmio_window(physical_memory_offset, physical_base, I6300ESB_MMIO_BYTES)
    })
}

impl ConfigRegionAccess for EnhancedPciConfigAccess {
    unsafe fn read(&self, address: PciAddress, offset: u16) -> u32 {
        let Some(config_address) = self.config_address(address, offset) else {
            return u32::MAX;
        };
        unsafe { (config_address as *const u32).read_volatile() }
    }

    unsafe fn write(&self, address: PciAddress, offset: u16, value: u32) {
        let config_address = self
            .config_address(address, offset)
            .unwrap_or_else(|| panic!("PCI ECAM region was missing for {address}"));
        unsafe { (config_address as *mut u32).write_volatile(value) };
    }
}

impl i6300esb::PciConfigIo for EnhancedPciConfigAccess {
    fn read_u8(&self, address: PciAddress, offset: u16) -> u8 {
        let config_address = self
            .byte_address(address, offset)
            .unwrap_or_else(|| panic!("PCI ECAM region was missing for {address}"));
        unsafe { (config_address as *const u8).read_volatile() }
    }

    fn read_u16(&self, address: PciAddress, offset: u16) -> u16 {
        assert!(
            offset & 0b1 == 0,
            "PCI ECAM u16 access requires aligned offsets, got {offset:#x}"
        );
        let config_address = self
            .byte_address(address, offset)
            .unwrap_or_else(|| panic!("PCI ECAM region was missing for {address}"));
        unsafe { (config_address as *const u16).read_volatile() }
    }

    fn write_u8(&self, address: PciAddress, offset: u16, value: u8) {
        let config_address = self
            .byte_address(address, offset)
            .unwrap_or_else(|| panic!("PCI ECAM region was missing for {address}"));
        unsafe { (config_address as *mut u8).write_volatile(value) };
    }

    fn write_u16(&self, address: PciAddress, offset: u16, value: u16) {
        assert!(
            offset & 0b1 == 0,
            "PCI ECAM u16 access requires aligned offsets, got {offset:#x}"
        );
        let config_address = self
            .byte_address(address, offset)
            .unwrap_or_else(|| panic!("PCI ECAM region was missing for {address}"));
        unsafe { (config_address as *mut u16).write_volatile(value) };
    }
}

impl EnhancedPciConfigAccess {
    fn config_address(&self, address: PciAddress, offset: u16) -> Option<usize> {
        assert!(
            offset & 0b11 == 0,
            "PCI ECAM access requires aligned offsets, got {offset:#x}"
        );
        assert!(
            offset < PCI_CONFIG_SPACE_BYTES,
            "PCI ECAM offset {offset:#x} exceeds config space"
        );

        let region = self.regions.iter().find(|region| {
            region.segment == address.segment()
                && (region.bus_start..=region.bus_end).contains(&address.bus())
        })?;
        let bus_index = u64::from(address.bus() - region.bus_start);
        let physical_address = region.base_address
            + (bus_index << PCI_ECAM_BUS_SHIFT)
            + (u64::from(address.device()) << PCI_ECAM_DEVICE_SHIFT)
            + (u64::from(address.function()) << PCI_ECAM_FUNCTION_SHIFT)
            + u64::from(offset);
        let physical_address = usize::try_from(physical_address).ok()?;
        self.mapped_virtual_address(physical_address, core::mem::size_of::<u32>())
    }

    fn byte_address(&self, address: PciAddress, offset: u16) -> Option<usize> {
        assert!(
            offset < PCI_CONFIG_SPACE_BYTES,
            "PCI ECAM offset {offset:#x} exceeds config space"
        );

        let region = self.regions.iter().find(|region| {
            region.segment == address.segment()
                && (region.bus_start..=region.bus_end).contains(&address.bus())
        })?;
        let bus_index = u64::from(address.bus() - region.bus_start);
        let physical_address = region.base_address
            + (bus_index << PCI_ECAM_BUS_SHIFT)
            + (u64::from(address.device()) << PCI_ECAM_DEVICE_SHIFT)
            + (u64::from(address.function()) << PCI_ECAM_FUNCTION_SHIFT)
            + u64::from(offset);
        let physical_address = usize::try_from(physical_address).ok()?;
        self.mapped_virtual_address(physical_address, 1)
    }

    fn mapped_virtual_address(&self, physical_address: usize, bytes: usize) -> Option<usize> {
        // ECAM is MMIO: map it uncacheable rather than as plain HHDM RAM.
        Some(crate::smp::map_mmio_window(
            self.physical_memory_offset,
            physical_address,
            bytes,
        ))
    }
}
