use arrayvec::ArrayVec;
use core::cell::Cell;
use core::ops::RangeInclusive;

use fdt::Fdt;
use helios_hal::align_up;
use pci_types::{Bar, ConfigRegionAccess, EndpointHeader, MAX_BARS, PciAddress, PciHeader};

const PCI_ECAM_BUS_SHIFT: usize = 20;
const PCI_ECAM_DEVICE_SHIFT: usize = 15;
const PCI_ECAM_FUNCTION_SHIFT: usize = 12;
const PCI_CONFIG_SPACE_BYTES: u16 = 4096;
const PCI_RANGE_IO_SPACE: u8 = 0x01;
const PCI_RANGE_MEMORY32_SPACE: u8 = 0x02;
const PCI_RANGE_MEMORY64_SPACE: u8 = 0x03;
const MAX_PCI_MEMORY_WINDOWS: usize = 4;

#[derive(Clone, Copy)]
pub(crate) struct EcamPciConfigAccess {
    segment: u16,
    bus_start: u8,
    bus_end: u8,
    ecam_base: usize,
}

pub(crate) struct PciHostController {
    access: EcamPciConfigAccess,
    bus_range: RangeInclusive<u8>,
    memory_windows: ArrayVec<PciMemoryWindow, MAX_PCI_MEMORY_WINDOWS>,
}

struct PciMemoryWindow {
    pci_base: u64,
    cpu_base: u64,
    size: u64,
    next_free: Cell<u64>,
}

impl PciHostController {
    pub(crate) fn discover(fdt: &Fdt<'_>) -> Option<Self> {
        let node = fdt.find_compatible(&["pci-host-ecam-generic"])?;
        let ecam_base = node
            .reg()
            .and_then(|mut reg| reg.next())
            .map(|region| region.starting_address as usize)
            .unwrap_or_else(|| panic!("PCI ECAM host node is missing a CPU-addressable reg"));
        let domain = parse_optional_u32_property(node.property("linux,pci-domain"))
            .map(|domain| {
                u16::try_from(domain)
                    .unwrap_or_else(|_| panic!("PCI domain {domain} does not fit in u16"))
            })
            .unwrap_or(0);
        let bus_range = parse_bus_range(node);
        let parent_address_cells = infer_parent_address_cells(node);
        let memory_windows = parse_memory_windows(node, parent_address_cells);

        Some(Self {
            access: EcamPciConfigAccess {
                segment: domain,
                bus_start: *bus_range.start(),
                bus_end: *bus_range.end(),
                ecam_base,
            },
            bus_range,
            memory_windows,
        })
    }

    pub(crate) fn config_access(&self) -> EcamPciConfigAccess {
        self.access
    }

    pub(crate) fn bus_range(&self) -> RangeInclusive<u8> {
        self.bus_range.clone()
    }

    pub(crate) fn translate_mmio_address(&self, bus_address: usize) -> usize {
        let bus_address = u64::try_from(bus_address)
            .unwrap_or_else(|_| panic!("PCI MMIO address {bus_address:#x} does not fit in u64"));
        let window = self
            .memory_windows
            .iter()
            .find(|window| window.contains(bus_address))
            .unwrap_or_else(|| {
                panic!("PCI MMIO address {bus_address:#x} did not match any CPU memory window")
            });
        window.translate(bus_address)
    }

    pub(crate) fn assign_unassigned_memory_bars(&self, address: PciAddress) {
        let header = PciHeader::new(address);
        let mut endpoint = EndpointHeader::from_header(header, self.access)
            .unwrap_or_else(|| panic!("PCI function {address} was not an endpoint"));
        let mut slot = 0_u8;

        while usize::from(slot) < MAX_BARS {
            let Some(bar) = endpoint.bar(slot, self.access) else {
                slot += 1;
                continue;
            };

            match bar {
                Bar::Memory32 {
                    address: 0, size, ..
                } => {
                    let assigned = self.allocate_memory_bar(u64::from(size), false);
                    unsafe {
                        endpoint
                            .write_bar(slot, self.access, assigned as usize)
                            .unwrap_or_else(|error| {
                                panic!("failed to assign PCI BAR{slot} for {address}: {error:?}")
                            });
                    }
                }
                Bar::Memory64 {
                    address: 0, size, ..
                } => {
                    let assigned = self.allocate_memory_bar(size, true);
                    unsafe {
                        endpoint
                            .write_bar(slot, self.access, assigned as usize)
                            .unwrap_or_else(|error| {
                                panic!("failed to assign PCI BAR{slot} for {address}: {error:?}")
                            });
                    }
                    slot += 1;
                }
                Bar::Memory64 { .. } => slot += 1,
                Bar::Memory32 { .. } | Bar::Io { .. } => {}
            }

            slot += 1;
        }
    }

    fn allocate_memory_bar(&self, size: u64, allow_above_4g: bool) -> u64 {
        assert!(
            size.is_power_of_two(),
            "PCI BAR size {size:#x} was not a power of two"
        );
        self.memory_windows
            .iter()
            .find_map(|window| window.allocate(size, allow_above_4g))
            .unwrap_or_else(|| {
                panic!(
                    "failed to allocate a PCI memory BAR of size {size:#x} on riscv (64-bit allowed: {allow_above_4g})"
                )
            })
    }
}

impl ConfigRegionAccess for EcamPciConfigAccess {
    unsafe fn read(&self, address: PciAddress, offset: u16) -> u32 {
        let config_address = self.config_address(address, offset);
        unsafe { (config_address as *const u32).read_volatile() }
    }

    unsafe fn write(&self, address: PciAddress, offset: u16, value: u32) {
        let config_address = self.config_address(address, offset);
        unsafe { (config_address as *mut u32).write_volatile(value) };
    }
}

impl i6300esb::PciConfigIo for EcamPciConfigAccess {
    fn read_u8(&self, address: PciAddress, offset: u16) -> u8 {
        let config_address = self.byte_address(address, offset);
        unsafe { (config_address as *const u8).read_volatile() }
    }

    fn read_u16(&self, address: PciAddress, offset: u16) -> u16 {
        assert!(
            offset & 0b1 == 0,
            "PCI ECAM u16 access requires aligned offsets, got {offset:#x}"
        );
        let config_address = self.byte_address(address, offset);
        unsafe { (config_address as *const u16).read_volatile() }
    }

    fn write_u8(&self, address: PciAddress, offset: u16, value: u8) {
        let config_address = self.byte_address(address, offset);
        unsafe { (config_address as *mut u8).write_volatile(value) };
    }

    fn write_u16(&self, address: PciAddress, offset: u16, value: u16) {
        assert!(
            offset & 0b1 == 0,
            "PCI ECAM u16 access requires aligned offsets, got {offset:#x}"
        );
        let config_address = self.byte_address(address, offset);
        unsafe { (config_address as *mut u16).write_volatile(value) };
    }
}

impl EcamPciConfigAccess {
    fn config_address(&self, address: PciAddress, offset: u16) -> usize {
        assert!(
            address.segment() == self.segment,
            "PCI segment mismatch: expected {}, got {}",
            self.segment,
            address.segment()
        );
        assert!(
            (self.bus_start..=self.bus_end).contains(&address.bus()),
            "PCI bus {} is outside ECAM bus range {}..={}",
            address.bus(),
            self.bus_start,
            self.bus_end
        );
        assert!(
            offset & 0b11 == 0,
            "PCI ECAM access requires aligned offsets, got {offset:#x}"
        );
        assert!(
            offset < PCI_CONFIG_SPACE_BYTES,
            "PCI ECAM offset {offset:#x} exceeds config space"
        );

        let bus_index = usize::from(address.bus() - self.bus_start);
        let bus_offset = bus_index
            .checked_shl(PCI_ECAM_BUS_SHIFT as u32)
            .unwrap_or_else(|| panic!("PCI bus index {bus_index} overflowed ECAM addressing"));
        let device_offset = usize::from(address.device()) << PCI_ECAM_DEVICE_SHIFT;
        let function_offset = usize::from(address.function()) << PCI_ECAM_FUNCTION_SHIFT;

        self.ecam_base
            .checked_add(bus_offset)
            .and_then(|base| base.checked_add(device_offset))
            .and_then(|base| base.checked_add(function_offset))
            .and_then(|base| base.checked_add(usize::from(offset)))
            .unwrap_or_else(|| {
                panic!("PCI ECAM address overflowed for {address} offset {offset:#x}")
            })
    }

    fn byte_address(&self, address: PciAddress, offset: u16) -> usize {
        assert!(
            address.segment() == self.segment,
            "PCI segment mismatch: expected {}, got {}",
            self.segment,
            address.segment()
        );
        assert!(
            (self.bus_start..=self.bus_end).contains(&address.bus()),
            "PCI bus {} is outside ECAM bus range {}..={}",
            address.bus(),
            self.bus_start,
            self.bus_end
        );
        assert!(
            offset < PCI_CONFIG_SPACE_BYTES,
            "PCI ECAM offset {offset:#x} exceeds config space"
        );

        let bus_index = usize::from(address.bus() - self.bus_start);
        let bus_offset = bus_index
            .checked_shl(PCI_ECAM_BUS_SHIFT as u32)
            .unwrap_or_else(|| panic!("PCI bus index {bus_index} overflowed ECAM addressing"));
        let device_offset = usize::from(address.device()) << PCI_ECAM_DEVICE_SHIFT;
        let function_offset = usize::from(address.function()) << PCI_ECAM_FUNCTION_SHIFT;

        self.ecam_base
            .checked_add(bus_offset)
            .and_then(|base| base.checked_add(device_offset))
            .and_then(|base| base.checked_add(function_offset))
            .and_then(|base| base.checked_add(usize::from(offset)))
            .unwrap_or_else(|| {
                panic!("PCI ECAM address overflowed for {address} offset {offset:#x}")
            })
    }
}

impl PciMemoryWindow {
    fn contains(&self, bus_address: u64) -> bool {
        let end = self
            .pci_base
            .checked_add(self.size)
            .unwrap_or_else(|| panic!("PCI MMIO window overflowed while checking membership"));
        self.pci_base <= bus_address && bus_address < end
    }

    fn translate(&self, bus_address: u64) -> usize {
        let offset = bus_address
            .checked_sub(self.pci_base)
            .unwrap_or_else(|| panic!("PCI MMIO translation underflowed"));
        let cpu_address = self
            .cpu_base
            .checked_add(offset)
            .unwrap_or_else(|| panic!("PCI MMIO CPU address overflowed"));
        usize::try_from(cpu_address).unwrap_or_else(|_| {
            panic!("PCI MMIO CPU address {cpu_address:#x} does not fit in usize")
        })
    }

    fn allocate(&self, size: u64, allow_above_4g: bool) -> Option<u64> {
        let current = self.next_free.get();
        let aligned = align_up(
            usize::try_from(current).unwrap_or_else(|_| {
                panic!("PCI BAR allocator cursor {current:#x} does not fit in usize")
            }),
            usize::try_from(size)
                .unwrap_or_else(|_| panic!("PCI BAR size {size:#x} does not fit in usize")),
        ) as u64;
        let end = aligned.checked_add(size)?;
        let window_end = self.pci_base.checked_add(self.size)?;
        if end > window_end {
            return None;
        }
        if !allow_above_4g && end > (u64::from(u32::MAX) + 1) {
            return None;
        }
        self.next_free.set(end);
        Some(aligned)
    }
}

fn parse_bus_range(node: fdt::node::FdtNode<'_, '_>) -> RangeInclusive<u8> {
    let property = node
        .property("bus-range")
        .unwrap_or_else(|| panic!("PCI host node is missing bus-range"));
    let start = read_be_u32(
        property
            .value
            .get(..4)
            .unwrap_or_else(|| panic!("PCI bus-range is missing the start cell")),
    );
    let end = read_be_u32(
        property
            .value
            .get(4..8)
            .unwrap_or_else(|| panic!("PCI bus-range is missing the end cell")),
    );
    let start = u8::try_from(start)
        .unwrap_or_else(|_| panic!("PCI bus-range start {start} does not fit in u8"));
    let end =
        u8::try_from(end).unwrap_or_else(|_| panic!("PCI bus-range end {end} does not fit in u8"));
    assert!(
        start <= end,
        "PCI bus-range start {start} exceeds end {end}"
    );
    start..=end
}

fn infer_parent_address_cells(node: fdt::node::FdtNode<'_, '_>) -> usize {
    let property = node
        .property("ranges")
        .unwrap_or_else(|| panic!("PCI host node is missing ranges"));
    assert!(
        property.value.len().is_multiple_of(4),
        "PCI ranges byte length {} is not a multiple of 4",
        property.value.len()
    );

    let child_sizes = node.cell_sizes();
    assert!(
        child_sizes.address_cells == 3,
        "PCI host #address-cells must be 3, got {}",
        child_sizes.address_cells
    );
    let total_cells = property.value.len() / 4;
    let entry_tail_cells = child_sizes.address_cells + child_sizes.size_cells;
    let mut parent_cells = None;

    for candidate in [1_usize, 2] {
        let entry_cells = entry_tail_cells + candidate;
        if total_cells.is_multiple_of(entry_cells) {
            assert!(
                parent_cells.is_none(),
                "PCI ranges matched multiple parent #address-cells candidates"
            );
            parent_cells = Some(candidate);
        }
    }

    parent_cells.unwrap_or_else(|| {
        panic!(
            "failed to infer PCI parent #address-cells from {} total range cells",
            total_cells
        )
    })
}

fn parse_memory_windows(
    node: fdt::node::FdtNode<'_, '_>,
    parent_address_cells: usize,
) -> ArrayVec<PciMemoryWindow, MAX_PCI_MEMORY_WINDOWS> {
    let property = node
        .property("ranges")
        .unwrap_or_else(|| panic!("PCI host node is missing ranges"));
    let child_sizes = node.cell_sizes();
    let entry_cells = child_sizes.address_cells + parent_address_cells + child_sizes.size_cells;
    let entry_bytes = entry_cells * 4;
    assert!(
        property.value.len().is_multiple_of(entry_bytes),
        "PCI ranges byte length {} is not divisible by entry size {}",
        property.value.len(),
        entry_bytes
    );

    let mut windows = ArrayVec::new();
    for entry in property.value.chunks_exact(entry_bytes) {
        let child_hi = read_be_u32(&entry[..4]);
        let space = (child_hi >> 24) as u8;
        if space == PCI_RANGE_IO_SPACE {
            continue;
        }
        let extra_address_bits = child_hi & 0x00ff_ffff;
        assert!(
            space == PCI_RANGE_MEMORY32_SPACE || space == PCI_RANGE_MEMORY64_SPACE,
            "unsupported PCI ranges space code {space:#x}"
        );
        assert!(
            extra_address_bits == 0,
            "unsupported PCI range encoded high address bits {extra_address_bits:#x}"
        );
        let child_base =
            (u64::from(read_be_u32(&entry[4..8])) << 32) | u64::from(read_be_u32(&entry[8..12]));
        let parent_offset = child_sizes.address_cells * 4;
        let parent_base =
            parse_cells_as_u64(&entry[parent_offset..parent_offset + parent_address_cells * 4]);
        let size_offset = parent_offset + parent_address_cells * 4;
        let size =
            parse_cells_as_u64(&entry[size_offset..size_offset + child_sizes.size_cells * 4]);
        assert!(size != 0, "PCI memory window size must be non-zero");

        windows
            .try_push(PciMemoryWindow {
                pci_base: child_base,
                cpu_base: parent_base,
                size,
                next_free: Cell::new(child_base),
            })
            .unwrap_or_else(|_| panic!("PCI host exposed too many memory windows"));
    }

    assert!(
        !windows.is_empty(),
        "PCI host did not expose any memory-mapped ranges"
    );
    windows
}

fn parse_optional_u32_property(property: Option<fdt::node::NodeProperty<'_>>) -> Option<u32> {
    property.map(|property| read_be_u32(property.value))
}

fn parse_cells_as_u64(bytes: &[u8]) -> u64 {
    assert!(
        bytes.len().is_multiple_of(4),
        "device-tree cell slice length {} is not a multiple of 4",
        bytes.len()
    );
    match bytes.len() / 4 {
        1 => u64::from(read_be_u32(bytes)),
        2 => (u64::from(read_be_u32(&bytes[..4])) << 32) | u64::from(read_be_u32(&bytes[4..8])),
        cells => panic!("unsupported device-tree cell count {cells}"),
    }
}

fn read_be_u32(bytes: &[u8]) -> u32 {
    let bytes: [u8; 4] = bytes
        .get(..4)
        .unwrap_or_else(|| panic!("expected a 4-byte device-tree cell"))
        .try_into()
        .unwrap_or_else(|_| panic!("expected a 4-byte device-tree cell"));
    u32::from_be_bytes(bytes)
}
