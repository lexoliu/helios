//! virtio-iommu discovery and device confinement for the x86 backend.
//!
//! q35 describes its translation topology in the ACPI VIOT table: one
//! node names the PCI function the virtio-iommu itself sits on, and PCI
//! range nodes map a span of bus/device/function numbers onto the
//! endpoint identities that unit knows them by. This module reads that
//! table, brings the unit up, and hands the kernel the endpoint of every
//! virtio function it is about to drive.
//!
//! Concurrency contract: discovery, bring-up and domain construction all
//! run on the bootstrap processor with interrupts masked, before any
//! device is programmed. Afterwards the only thing that touches the unit
//! is its interrupt handler, which drains reported faults.

extern crate alloc;

use alloc::sync::Arc;

use acpi::sdt::SdtHeader;
use helios_hal::iommu::{DmaTranslation, EndpointId, PhysicalRange};
use helios_kernel::{ExternalInterruptHandler, IommuDomains, IommuReport, MAX_IOMMU_ENDPOINTS};
use helios_virtio::{
    DeviceType, MAX_RESERVED_REGIONS, OffsetDmaPool, PlatformDmaPool, ReservedRegion,
    VirtioIommuDevice, VirtioPciTransport,
};
use pci_types::PciAddress;

use crate::pci::PciRoot;

/// The DMA pool every virtio-PCI driver on this backend publishes
/// addresses from.
///
/// It is the same type whether or not the platform confines its devices:
/// what changes is the translation inside it, which is the platform's
/// own answer to "what address does a device have to issue for this
/// page".
pub(crate) type X86DmaPool = PlatformDmaPool<OffsetDmaPool>;

/// The `VIOT` signature, which the vendored `acpi` crate has no
/// constant for and cannot be handed one: its `Signature` type has no
/// public constructor, so the table is found by walking the headers.
const VIOT_SIGNATURE: &str = "VIOT";

/// `struct acpi_table_viot` after the common SDT header.
const VIOT_NODE_COUNT: usize = 0x00;
const VIOT_NODE_OFFSET: usize = 0x02;

/// Node types (VIOT specification §3).
const NODE_PCI_RANGE: u8 = 1;
const NODE_VIRTIO_IOMMU_PCI: u8 = 3;

/// `struct acpi_viot_header` field offsets, from the start of a node.
const NODE_TYPE: usize = 0x00;
const NODE_LENGTH: usize = 0x02;

/// `struct acpi_viot_pci_range` field offsets.
const PCI_RANGE_ENDPOINT_START: usize = 0x04;
const PCI_RANGE_SEGMENT_START: usize = 0x08;
const PCI_RANGE_SEGMENT_END: usize = 0x0a;
const PCI_RANGE_BDF_START: usize = 0x0c;
const PCI_RANGE_BDF_END: usize = 0x0e;
const PCI_RANGE_OUTPUT_NODE: usize = 0x10;
const PCI_RANGE_BYTES: usize = 24;

/// `struct acpi_viot_virtio_iommu_pci` field offsets.
const VIRTIO_IOMMU_SEGMENT: usize = 0x04;
const VIRTIO_IOMMU_BDF: usize = 0x06;
const VIRTIO_IOMMU_BYTES: usize = 16;

/// PCI range nodes one VIOT may declare. q35 describes its whole bus
/// with one; four leaves room for a machine that splits it.
const MAX_PCI_RANGES: usize = 4;

/// The translation unit as VIOT describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IommuTopology {
    /// The PCI function the virtio-iommu itself sits on.
    unit: PciAddress,
    ranges: [Option<PciRange>; MAX_PCI_RANGES],
}

/// One span of PCI functions and the endpoint identities they are known
/// by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PciRange {
    endpoint_start: u32,
    segment_start: u16,
    segment_end: u16,
    bdf_start: u16,
    bdf_end: u16,
}

impl PciRange {
    fn endpoint_of(&self, address: PciAddress) -> Option<EndpointId> {
        let segment = address.segment();
        let bdf = bus_device_function(address);
        if segment < self.segment_start
            || segment > self.segment_end
            || bdf < self.bdf_start
            || bdf > self.bdf_end
        {
            return None;
        }
        Some(EndpointId::new(
            self.endpoint_start + u32::from(bdf - self.bdf_start),
        ))
    }
}

impl IommuTopology {
    /// The PCI function the virtio-iommu itself sits on.
    pub(crate) fn unit(&self) -> PciAddress {
        self.unit
    }

    /// The endpoint identity the unit knows `address` by, or `None` when
    /// the topology puts that function outside its reach.
    ///
    /// The unit's own function is deliberately excluded: a translation
    /// unit publishes its request and event rings at physical addresses
    /// and must never be attached to one of its own domains.
    pub(crate) fn endpoint_of(&self, address: PciAddress) -> Option<EndpointId> {
        if address == self.unit {
            return None;
        }
        self.ranges
            .iter()
            .flatten()
            .find_map(|range| range.endpoint_of(address))
    }
}

/// The unit the platform exposes, once it has been brought up.
pub(crate) type X86VirtioIommuDevice = VirtioIommuDevice<VirtioPciTransport<OffsetDmaPool>>;

/// The interrupt handler of the platform's translation unit.
///
/// It drains the faults the unit reported and publishes the running
/// total so `helios-inspector stats` shows it.
#[derive(Clone)]
pub(crate) struct VirtioIommu {
    device: Arc<X86VirtioIommuDevice>,
    report: Arc<IommuReport>,
}

impl VirtioIommu {
    pub(crate) fn new(device: Arc<X86VirtioIommuDevice>, report: Arc<IommuReport>) -> Self {
        Self { device, report }
    }
}

impl ExternalInterruptHandler for VirtioIommu {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
        self.report.record_faults(self.device.fault_count());
    }
}

/// Reads the platform's translation topology out of the ACPI VIOT table.
///
/// A machine with no VIOT has no translation unit its devices can be put
/// behind, which is the ordinary case; the caller then leaves the
/// devices addressing physical memory directly.
pub(crate) fn discover(
    rsdp_address: usize,
    physical_memory_offset: usize,
) -> Option<IommuTopology> {
    let handler = crate::smp::PhysicalOffsetAcpiHandler {
        physical_memory_offset,
        tsc_base: 0,
        tsc_hz: 1,
    };
    let tables = unsafe { acpi::AcpiTables::from_rsdp(handler, rsdp_address) }
        .unwrap_or_else(|error| panic!("failed to parse ACPI tables for the VIOT: {error:?}"));
    let (physical_address, header) = tables
        .table_headers()
        .find(|(_, header)| header.signature.as_str() == VIOT_SIGNATURE)?;
    // `SdtHeader` is a packed structure, so the field is copied out
    // before anything takes a reference to it.
    let declared_length = header.length;
    let length = usize::try_from(declared_length)
        .unwrap_or_else(|_| panic!("ACPI VIOT length {declared_length} does not fit usize"));
    let virtual_address = physical_address
        .checked_add(physical_memory_offset)
        .unwrap_or_else(|| panic!("ACPI VIOT at {physical_address:#x} overflowed the HHDM"));
    // SAFETY: the address and length come from the RSDT entry the
    // firmware published, and the whole table sits inside the direct
    // physical map the bootloader established.
    let table = unsafe { core::slice::from_raw_parts(virtual_address as *const u8, length) };
    Some(parse_viot(table))
}

/// Brings the unit at `address` up on its own MSI-X vector.
pub(crate) fn install(
    pci: &PciRoot,
    address: PciAddress,
    physical_memory_offset: usize,
    vector: u8,
    destination_apic_id: u32,
) -> Arc<X86VirtioIommuDevice> {
    assert_eq!(
        helios_virtio::virtio_pci_device_type(&pci.access(), address),
        Some(DeviceType::Iommu),
        "the ACPI VIOT named {address} as the translation unit but it carries no virtio-iommu"
    );
    let msix_vector = pci.bind_msix_vector(address, vector, destination_apic_id);
    let device = helios_virtio::iommu_from_pci(
        &pci.access(),
        address,
        pci,
        OffsetDmaPool::new(physical_memory_offset),
        Some(msix_vector),
    )
    .unwrap_or_else(|error| {
        panic!("failed to initialize the virtio-iommu function at {address}: {error}")
    });
    tracing::info!(
        function = %address,
        msix_vector = vector,
        global_bypass = device.global_bypass(),
        "virtio-iommu online"
    );
    Arc::new(device)
}

/// The devices one translation unit confines, and what each of them
/// reaches.
pub(crate) struct Confinement {
    device: Arc<X86VirtioIommuDevice>,
    report: Arc<IommuReport>,
    entries: [Option<(PciAddress, DmaTranslation)>; MAX_IOMMU_ENDPOINTS],
}

impl Confinement {
    /// The translation the driver of `address` publishes its addresses
    /// through.
    pub(crate) fn translation_of(&self, address: PciAddress) -> DmaTranslation {
        self.entries
            .iter()
            .flatten()
            .find_map(|(confined, translation)| (*confined == address).then_some(*translation))
            .unwrap_or_else(|| panic!("PCI function {address} was never given an IOMMU domain"))
    }

    /// The handler that drains the unit's fault reports.
    pub(crate) fn interrupt_handler(&self) -> VirtioIommu {
        VirtioIommu::new(self.device.clone(), self.report.clone())
    }

    /// What the kernel publishes about this unit.
    pub(crate) fn report(&self) -> Arc<IommuReport> {
        self.report.clone()
    }
}

/// Brings the platform's translation unit up and gives every function in
/// `functions` a domain of its own.
///
/// `dma_memory` is the physical memory the kernel allocates its DMA
/// buffers from; nothing outside it, and nothing outside the doorbells
/// the unit and the platform name, is reachable by any confined device
/// afterwards.
pub(crate) fn confine_devices(
    pci: &PciRoot,
    topology: IommuTopology,
    physical_memory_offset: usize,
    vector: u8,
    destination_apic_id: u32,
    dma_memory: &[PhysicalRange],
    functions: &[PciAddress],
) -> Confinement {
    let device = install(
        pci,
        topology.unit(),
        physical_memory_offset,
        vector,
        destination_apic_id,
    );

    // The unit is authoritative about the ranges its endpoints have to
    // keep reaching; the platform's own message window is the answer
    // when it names none, which is what q35 does.
    let mut doorbells = [MSI_DOORBELL; MAX_RESERVED_REGIONS + 1];
    let mut doorbell_count = 1;
    let mut probed = [ReservedRegion {
        start: 0,
        bytes: 0,
        doorbell: false,
    }; MAX_RESERVED_REGIONS];
    for address in functions {
        let Some(endpoint) = topology.endpoint_of(*address) else {
            panic!("PCI function {address} is not covered by the ACPI VIOT topology");
        };
        let found = device.probe(endpoint, &mut probed).unwrap_or_else(|error| {
            panic!("virtio-iommu refused to probe endpoint {endpoint}: {error}")
        });
        for region in &probed[..found] {
            assert!(
                region.doorbell,
                "virtio-iommu reserved a range at {:#x} that endpoint {endpoint} may not reach; \
                 helios has no way to keep that endpoint working",
                region.start
            );
            let range = PhysicalRange::new(region.start, region.bytes);
            if doorbells[..doorbell_count].contains(&range) {
                continue;
            }
            doorbells[doorbell_count] = range;
            doorbell_count += 1;
        }
    }

    let mut domains = IommuDomains::new(device.clone(), dma_memory, &doorbells[..doorbell_count])
        .unwrap_or_else(|error| panic!("the IOMMU domain layout is not buildable: {error}"));
    let mut entries = [const { None }; MAX_IOMMU_ENDPOINTS];
    for (slot, address) in functions.iter().enumerate() {
        let endpoint = topology
            .endpoint_of(*address)
            .unwrap_or_else(|| panic!("PCI function {address} left the VIOT topology"));
        let translation = domains
            .confine(endpoint)
            .unwrap_or_else(|error| panic!("failed to confine {address}: {error}"));
        entries[slot] = Some((*address, translation));
    }

    Confinement {
        report: domains.report(device.global_bypass()),
        device,
        entries,
    }
}

/// The pool a virtio-PCI driver publishes addresses from.
///
/// With no translation unit the addresses are physical, which is what
/// every machine without one requires.
pub(crate) fn dma_pool(
    confinement: Option<&Confinement>,
    address: PciAddress,
    physical_memory_offset: usize,
) -> X86DmaPool {
    let translation = match confinement {
        Some(confinement) => confinement.translation_of(address),
        None => DmaTranslation::direct(),
    };
    PlatformDmaPool::new(OffsetDmaPool::new(physical_memory_offset), translation)
}

/// The local-APIC message window every MSI-X capable function writes to.
///
/// A confined device still has to reach it, and the address is fixed by
/// the interrupt controller, so every domain maps it at its own physical
/// address.
pub(crate) const MSI_DOORBELL: PhysicalRange = PhysicalRange::new(0xfee0_0000, 0x10_0000);

fn bus_device_function(address: PciAddress) -> u16 {
    (u16::from(address.bus()) << 8)
        | (u16::from(address.device()) << 3)
        | u16::from(address.function())
}

fn parse_viot(table: &[u8]) -> IommuTopology {
    let header_bytes = core::mem::size_of::<SdtHeader>();
    let body = &table[header_bytes..];
    let node_count = usize::from(read_u16(body, VIOT_NODE_COUNT));
    let node_offset = usize::from(read_u16(body, VIOT_NODE_OFFSET));
    assert!(
        node_offset >= header_bytes,
        "ACPI VIOT node array starts inside the table header"
    );

    let mut unit = None;
    let mut ranges = [const { None }; MAX_PCI_RANGES];
    let mut found_ranges = 0;
    let mut offset = node_offset;
    for _ in 0..node_count {
        assert!(
            offset + NODE_LENGTH + 2 <= table.len(),
            "ACPI VIOT node at {offset:#x} runs past the table"
        );
        let node_type = table[offset + NODE_TYPE];
        let length = usize::from(read_u16(table, offset + NODE_LENGTH));
        assert!(
            length != 0 && offset + length <= table.len(),
            "ACPI VIOT node at {offset:#x} declares an unusable length {length}"
        );
        let node = &table[offset..offset + length];
        match node_type {
            NODE_VIRTIO_IOMMU_PCI => {
                assert!(
                    length >= VIRTIO_IOMMU_BYTES,
                    "ACPI VIOT virtio-pci node is shorter than the layout"
                );
                let segment = read_u16(node, VIRTIO_IOMMU_SEGMENT);
                let bdf = read_u16(node, VIRTIO_IOMMU_BDF);
                assert!(
                    unit.replace(pci_address(segment, bdf)).is_none(),
                    "ACPI VIOT declares more than one virtio-iommu"
                );
            }
            NODE_PCI_RANGE => {
                assert!(
                    length >= PCI_RANGE_BYTES,
                    "ACPI VIOT PCI range node is shorter than the layout"
                );
                assert!(
                    found_ranges < MAX_PCI_RANGES,
                    "ACPI VIOT declares more than {MAX_PCI_RANGES} PCI ranges"
                );
                // The output node is the offset of the unit node this
                // range translates through. helios drives one unit, so a
                // range pointing anywhere else is a topology it cannot
                // honour.
                let output_node = usize::from(read_u16(node, PCI_RANGE_OUTPUT_NODE));
                assert!(
                    output_node >= node_offset && output_node < table.len(),
                    "ACPI VIOT PCI range points at node offset {output_node:#x}, which is not a node"
                );
                assert_eq!(
                    table[output_node + NODE_TYPE],
                    NODE_VIRTIO_IOMMU_PCI,
                    "ACPI VIOT PCI range translates through a unit helios does not drive"
                );
                ranges[found_ranges] = Some(PciRange {
                    endpoint_start: read_u32(node, PCI_RANGE_ENDPOINT_START),
                    segment_start: read_u16(node, PCI_RANGE_SEGMENT_START),
                    segment_end: read_u16(node, PCI_RANGE_SEGMENT_END),
                    bdf_start: read_u16(node, PCI_RANGE_BDF_START),
                    bdf_end: read_u16(node, PCI_RANGE_BDF_END),
                });
                found_ranges += 1;
            }
            _ => {}
        }
        offset += length;
    }

    IommuTopology {
        unit: unit.unwrap_or_else(|| panic!("ACPI VIOT declares no virtio-iommu unit")),
        ranges,
    }
}

fn pci_address(segment: u16, bdf: u16) -> PciAddress {
    PciAddress::new(
        segment,
        (bdf >> 8) as u8,
        ((bdf >> 3) & 0x1f) as u8,
        (bdf & 0x7) as u8,
    )
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}
