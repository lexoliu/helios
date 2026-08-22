//! x86 PCI adaptation: legacy (0xCF8/0xCFC) configuration access, bus
//! enumeration, BAR mapping and MSI-X vector binding.
//!
//! q35 exposes every device Helios cares about through segment 0, so the
//! legacy configuration mechanism is sufficient and avoids depending on
//! the ACPI MCFG table for device discovery. Extended (>0xff) config
//! space is only needed by the watchdog, which keeps its own ECAM
//! accessor.

use core::ptr::NonNull;

use helios_hal::io::{IoError, IoResult};
use helios_virtio::{DeviceType, PciMmioMapper};
use pci_types::capability::PciCapability;
use pci_types::{Bar, CommandRegister, ConfigRegionAccess, EndpointHeader, PciAddress, PciHeader};
use x86_64::instructions::port::{PortReadOnly, PortWriteOnly};

const PCI_CONFIG_ADDRESS_PORT: u16 = 0x0cf8;
const PCI_CONFIG_DATA_PORT: u16 = 0x0cfc;
const PCI_INVALID_VENDOR_ID: u16 = 0xffff;
const PCI_MAX_DEVICES: u8 = 32;
const PCI_MAX_FUNCTIONS: u8 = 8;

/// Local-APIC message address range used by MSI/MSI-X on x86.
const MSI_ADDRESS_BASE: u64 = 0xfee0_0000;
const MSI_DESTINATION_SHIFT: u32 = 12;

/// MSI-X table entry layout, in bytes.
const MSIX_ENTRY_BYTES: usize = 16;
const MSIX_ENTRY_ADDRESS_LOW: usize = 0x0;
const MSIX_ENTRY_ADDRESS_HIGH: usize = 0x4;
const MSIX_ENTRY_DATA: usize = 0x8;
const MSIX_ENTRY_VECTOR_CONTROL: usize = 0xc;
const MSIX_VECTOR_CONTROL_MASKED: u32 = 1 << 0;

/// The MSI-X table entry Helios binds every virtio structure of a
/// function to. One entry per device keeps the interrupt path a single
/// IDT vector per device, which is all the kernel's route table needs.
pub(crate) const MSIX_VIRTIO_ENTRY: u16 = 0;

#[derive(Clone, Copy, Default)]
pub(crate) struct LegacyPciConfigAccess;

impl LegacyPciConfigAccess {
    pub(crate) const fn new() -> Self {
        Self
    }

    pub(crate) fn read_u8(&self, address: PciAddress, offset: u16) -> u8 {
        let aligned_offset = offset & !0b11;
        let shift = ((offset & 0b11) * 8) as u32;
        ((unsafe { self.read(address, aligned_offset) } >> shift) & 0xff) as u8
    }

    pub(crate) fn read_u16(&self, address: PciAddress, offset: u16) -> u16 {
        let aligned_offset = offset & !0b11;
        let shift = ((offset & 0b10) * 8) as u32;
        ((unsafe { self.read(address, aligned_offset) } >> shift) & 0xffff) as u16
    }

    pub(crate) fn write_u8(&self, address: PciAddress, offset: u16, value: u8) {
        let aligned_offset = offset & !0b11;
        let shift = ((offset & 0b11) * 8) as u32;
        let mut data = unsafe { self.read(address, aligned_offset) };
        data &= !(0xff_u32 << shift);
        data |= u32::from(value) << shift;
        unsafe { self.write(address, aligned_offset, data) };
    }

    pub(crate) fn write_u16(&self, address: PciAddress, offset: u16, value: u16) {
        let aligned_offset = offset & !0b11;
        let shift = ((offset & 0b10) * 8) as u32;
        let mut data = unsafe { self.read(address, aligned_offset) };
        data &= !(0xffff_u32 << shift);
        data |= u32::from(value) << shift;
        unsafe { self.write(address, aligned_offset, data) };
    }

    fn config_address(address: PciAddress, offset: u16) -> u32 {
        assert!(
            address.segment() == 0,
            "legacy PCI config-space access only supports segment 0, got {}",
            address.segment()
        );
        assert!(
            offset & 0b11 == 0,
            "legacy PCI config-space access requires aligned offsets, got {offset:#x}"
        );
        assert!(
            offset < 0x100,
            "legacy PCI config-space access only supports the first 256 bytes, got {offset:#x}"
        );

        (1_u32 << 31)
            | (u32::from(address.bus()) << 16)
            | (u32::from(address.device()) << 11)
            | (u32::from(address.function()) << 8)
            | u32::from(offset)
    }
}

impl ConfigRegionAccess for LegacyPciConfigAccess {
    unsafe fn read(&self, address: PciAddress, offset: u16) -> u32 {
        critical_section::with(|_| unsafe {
            PortWriteOnly::<u32>::new(PCI_CONFIG_ADDRESS_PORT)
                .write(Self::config_address(address, offset));
            PortReadOnly::<u32>::new(PCI_CONFIG_DATA_PORT).read()
        })
    }

    unsafe fn write(&self, address: PciAddress, offset: u16, value: u32) {
        critical_section::with(|_| unsafe {
            PortWriteOnly::<u32>::new(PCI_CONFIG_ADDRESS_PORT)
                .write(Self::config_address(address, offset));
            PortWriteOnly::<u32>::new(PCI_CONFIG_DATA_PORT).write(value);
        });
    }
}

/// The x86 PCI root: configuration access plus the HHDM offset every BAR
/// mapping is expressed against.
#[derive(Clone, Copy)]
pub(crate) struct PciRoot {
    access: LegacyPciConfigAccess,
    physical_memory_offset: usize,
}

impl PciRoot {
    pub(crate) const fn new(physical_memory_offset: usize) -> Self {
        Self {
            access: LegacyPciConfigAccess::new(),
            physical_memory_offset,
        }
    }

    pub(crate) fn access(&self) -> LegacyPciConfigAccess {
        self.access
    }

    /// Every function present in segment 0.
    ///
    /// Buses are walked exhaustively rather than followed through
    /// bridge secondary-bus registers: with a single configuration
    /// segment the bus number alone addresses a function, and the walk
    /// then covers devices behind bridges without the kernel having to
    /// re-derive the firmware's bridge numbering.
    fn functions(&self) -> impl Iterator<Item = PciAddress> + '_ {
        (0..=u8::MAX).flat_map(move |bus| {
            (0..PCI_MAX_DEVICES).flat_map(move |device| {
                (0..PCI_MAX_FUNCTIONS).filter_map(move |function| {
                    let address = PciAddress::new(0, bus, device, function);
                    let header = PciHeader::new(address);
                    let (vendor_id, _) = header.id(self.access);
                    if vendor_id == PCI_INVALID_VENDOR_ID {
                        return None;
                    }
                    if function != 0
                        && !PciHeader::new(PciAddress::new(0, bus, device, 0))
                            .has_multiple_functions(self.access)
                    {
                        return None;
                    }
                    Some(address)
                })
            })
        })
    }

    /// Locates the first modern virtio function of the requested type.
    pub(crate) fn find_virtio_function(&self, device_type: DeviceType) -> Option<PciAddress> {
        self.functions().find(|address| {
            helios_virtio::virtio_pci_device_type(&self.access, *address) == Some(device_type)
        })
    }

    /// Points one MSI-X table entry of `address` at `vector` on the
    /// local APIC of `destination_apic_id` and enables MSI-X on the
    /// function.
    ///
    /// Returns the table index the virtio common configuration should
    /// bind its queues to.
    pub(crate) fn bind_msix_vector(
        &self,
        address: PciAddress,
        vector: u8,
        destination_apic_id: u32,
    ) -> u16 {
        assert!(
            destination_apic_id <= u32::from(u8::MAX),
            "MSI-X without interrupt remapping cannot target APIC id {destination_apic_id}"
        );
        let header = PciHeader::new(address);
        let mut endpoint = EndpointHeader::from_header(header, self.access)
            .unwrap_or_else(|| panic!("PCI function {address} is not an endpoint"));
        let mut capability = endpoint
            .capabilities(self.access)
            .find_map(|capability| match capability {
                PciCapability::MsiX(msix) => Some(msix),
                _ => None,
            })
            .unwrap_or_else(|| panic!("PCI function {address} exposes no MSI-X capability"));
        assert!(
            capability.table_size() > MSIX_VIRTIO_ENTRY,
            "PCI function {address} has an MSI-X table too small for entry {MSIX_VIRTIO_ENTRY}"
        );

        // Sizing a BAR drives all-ones probe values through it, so
        // memory decode has to be off for the duration.
        endpoint.update_command(self.access, |command| {
            command & !CommandRegister::MEMORY_ENABLE
        });
        let table_bar = read_memory_bar(&endpoint, self.access, capability.table_bar());
        endpoint.update_command(self.access, |command| {
            command | CommandRegister::MEMORY_ENABLE
        });

        let entry_offset = usize::try_from(capability.table_offset())
            .unwrap_or_else(|_| panic!("MSI-X table offset for {address} does not fit in usize"))
            + usize::from(MSIX_VIRTIO_ENTRY) * MSIX_ENTRY_BYTES;
        let entry = self
            .map_region(table_bar + entry_offset as u64, MSIX_ENTRY_BYTES)
            .unwrap_or_else(|error| panic!("failed to map the MSI-X table of {address}: {error}"))
            .as_ptr()
            .cast::<u32>();

        let message_address =
            MSI_ADDRESS_BASE | (u64::from(destination_apic_id) << MSI_DESTINATION_SHIFT);
        unsafe {
            entry
                .byte_add(MSIX_ENTRY_VECTOR_CONTROL)
                .write_volatile(MSIX_VECTOR_CONTROL_MASKED);
            entry
                .byte_add(MSIX_ENTRY_ADDRESS_LOW)
                .write_volatile(message_address as u32);
            entry
                .byte_add(MSIX_ENTRY_ADDRESS_HIGH)
                .write_volatile((message_address >> 32) as u32);
            entry
                .byte_add(MSIX_ENTRY_DATA)
                .write_volatile(u32::from(vector));
            entry.byte_add(MSIX_ENTRY_VECTOR_CONTROL).write_volatile(0);
        }

        capability.set_function_mask(false, self.access);
        capability.set_enabled(true, self.access);
        assert!(
            capability.enabled(self.access),
            "PCI function {address} refused to enable MSI-X"
        );
        MSIX_VIRTIO_ENTRY
    }
}

impl PciMmioMapper for PciRoot {
    fn map_region(&self, bar_address: u64, bytes: usize) -> IoResult<NonNull<u8>> {
        let physical_start = usize::try_from(bar_address).map_err(|_| IoError::OutOfBounds)?;
        let virtual_start =
            crate::smp::map_mmio_window(self.physical_memory_offset, physical_start, bytes);
        NonNull::new(virtual_start as *mut u8).ok_or(IoError::DeviceFault)
    }
}

fn read_memory_bar(endpoint: &EndpointHeader, access: LegacyPciConfigAccess, slot: u8) -> u64 {
    match endpoint.bar(slot, access) {
        Some(Bar::Memory32 { address, .. }) => u64::from(address),
        Some(Bar::Memory64 { address, .. }) => address,
        Some(Bar::Io { .. }) | None => {
            panic!("PCI BAR{slot} is not an assigned memory BAR")
        }
    }
}
