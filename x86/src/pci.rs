use pci_types::{ConfigRegionAccess, EndpointHeader, PciAddress, PciHeader};
use x86_64::instructions::port::{PortReadOnly, PortWriteOnly};

const PCI_CONFIG_ADDRESS_PORT: u16 = 0x0cf8;
const PCI_CONFIG_DATA_PORT: u16 = 0x0cfc;
const PCI_MAX_DEVICE: u8 = 32;
const PCI_MAX_FUNCTION: u8 = 8;

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

    pub(crate) fn find_endpoint(&self, vendor_id: u16, device_id: u16) -> Option<EndpointHeader> {
        for bus in 0..=u8::MAX {
            for device in 0..PCI_MAX_DEVICE {
                let slot = PciAddress::new(0, bus, device, 0);
                let header = PciHeader::new(slot);
                let (vendor, _) = header.id(self);
                if vendor == u16::MAX {
                    continue;
                }

                let function_count = if header.has_multiple_functions(self) {
                    PCI_MAX_FUNCTION
                } else {
                    1
                };
                for function in 0..function_count {
                    let address = PciAddress::new(0, bus, device, function);
                    let header = PciHeader::new(address);
                    let (vendor, device) = header.id(self);
                    if vendor == u16::MAX {
                        continue;
                    }
                    if vendor != vendor_id || device != device_id {
                        continue;
                    }
                    return Some(
                        EndpointHeader::from_header(header, self).unwrap_or_else(|| {
                            panic!(
                                "PCI function {address} matched watchdog id but was not an endpoint"
                            )
                        }),
                    );
                }
            }
        }

        None
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
