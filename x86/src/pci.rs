use pci_types::{ConfigRegionAccess, PciAddress};
use x86_64::instructions::port::{PortReadOnly, PortWriteOnly};

const PCI_CONFIG_ADDRESS_PORT: u16 = 0x0cf8;
const PCI_CONFIG_DATA_PORT: u16 = 0x0cfc;

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
