//! `AcpiNuma` over SRAT, SLIT and MADT tables assembled from the ACPI 6.5
//! layouts (§5.2.6 header, §5.2.12 MADT, §5.2.16 SRAT, §5.2.17 SLIT)
//! inside the test, so no binary fixture is committed. The tables sit in
//! host memory and a handler maps a "physical" address to the same host
//! address.

use std::ptr::NonNull;

use acpi::aml::AmlError;
use acpi::{AcpiTables, Handle, Handler, PciAddress, RawPhysicalMapping};
use helios_firmware::{
    AcpiNuma, FirmwareNumaError, GiccUidKey, LocalApicKey, ProcessorIdentities, SratProcessorKey,
};
use helios_hal::cpu::ProcessorId;
use helios_hal::numa::{MAX_NODES, MemoryAffinity, NodeDistance, NodeId, NumaTopology, SingleNode};

/// Every table here lives in the test's own memory.
#[derive(Clone)]
struct HostMemory;

impl Handler for HostMemory {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> RawPhysicalMapping<T> {
        let start = NonNull::new(physical_address as *mut T).expect("a table is at a real address");
        // SAFETY: the address is a live table buffer the test owns, and
        // the mapping is the identity, so nothing is created or undone.
        unsafe { RawPhysicalMapping::new(physical_address, start, size, size) }
    }

    unsafe fn unmap_physical_region<T>(&self, _region: RawPhysicalMapping<T>) {}

    fn read_u8(&self, _address: usize) -> u8 {
        unreachable!("the NUMA reader touches no register")
    }

    fn read_u16(&self, _address: usize) -> u16 {
        unreachable!("the NUMA reader touches no register")
    }

    fn read_u32(&self, _address: usize) -> u32 {
        unreachable!("the NUMA reader touches no register")
    }

    fn read_u64(&self, _address: usize) -> u64 {
        unreachable!("the NUMA reader touches no register")
    }

    fn write_u8(&self, _address: usize, _value: u8) {
        unreachable!("the NUMA reader touches no register")
    }

    fn write_u16(&self, _address: usize, _value: u16) {
        unreachable!("the NUMA reader touches no register")
    }

    fn write_u32(&self, _address: usize, _value: u32) {
        unreachable!("the NUMA reader touches no register")
    }

    fn write_u64(&self, _address: usize, _value: u64) {
        unreachable!("the NUMA reader touches no register")
    }

    fn read_io_u8(&self, _port: u16) -> u8 {
        unreachable!("the NUMA reader touches no port")
    }

    fn read_io_u16(&self, _port: u16) -> u16 {
        unreachable!("the NUMA reader touches no port")
    }

    fn read_io_u32(&self, _port: u16) -> u32 {
        unreachable!("the NUMA reader touches no port")
    }

    fn write_io_u8(&self, _port: u16, _value: u8) {
        unreachable!("the NUMA reader touches no port")
    }

    fn write_io_u16(&self, _port: u16, _value: u16) {
        unreachable!("the NUMA reader touches no port")
    }

    fn write_io_u32(&self, _port: u16, _value: u32) {
        unreachable!("the NUMA reader touches no port")
    }

    fn read_pci_u8(&self, _address: PciAddress, _offset: u16) -> u8 {
        unreachable!("the NUMA reader touches no PCI configuration space")
    }

    fn read_pci_u16(&self, _address: PciAddress, _offset: u16) -> u16 {
        unreachable!("the NUMA reader touches no PCI configuration space")
    }

    fn read_pci_u32(&self, _address: PciAddress, _offset: u16) -> u32 {
        unreachable!("the NUMA reader touches no PCI configuration space")
    }

    fn write_pci_u8(&self, _address: PciAddress, _offset: u16, _value: u8) {
        unreachable!("the NUMA reader touches no PCI configuration space")
    }

    fn write_pci_u16(&self, _address: PciAddress, _offset: u16, _value: u16) {
        unreachable!("the NUMA reader touches no PCI configuration space")
    }

    fn write_pci_u32(&self, _address: PciAddress, _offset: u16, _value: u32) {
        unreachable!("the NUMA reader touches no PCI configuration space")
    }

    fn nanos_since_boot(&self) -> u64 {
        unreachable!("the NUMA reader keeps no time")
    }

    fn stall(&self, _microseconds: u64) {
        unreachable!("the NUMA reader keeps no time")
    }

    fn sleep(&self, _milliseconds: u64) {
        unreachable!("the NUMA reader keeps no time")
    }

    fn create_mutex(&self) -> Handle {
        unreachable!("the NUMA reader runs no AML")
    }

    fn acquire(&self, _mutex: Handle, _timeout: u16) -> Result<(), AmlError> {
        unreachable!("the NUMA reader runs no AML")
    }

    fn release(&self, _mutex: Handle) {
        unreachable!("the NUMA reader runs no AML")
    }
}

/// System Description Table Header (§5.2.6), checksummed, then `body`.
fn sdt(signature: &[u8; 4], revision: u8, body: &[u8]) -> Vec<u8> {
    const HEADER: usize = 36;
    let mut table = Vec::with_capacity(HEADER + body.len());
    table.extend_from_slice(signature);
    table.extend_from_slice(&((HEADER + body.len()) as u32).to_le_bytes());
    table.push(revision);
    table.push(0);
    table.extend_from_slice(b"HELIOS");
    table.extend_from_slice(b"NUMATEST");
    table.extend_from_slice(&1_u32.to_le_bytes());
    table.extend_from_slice(b"HLOS");
    table.extend_from_slice(&1_u32.to_le_bytes());
    table.extend_from_slice(body);
    let sum = table.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
    table[9] = 0_u8.wrapping_sub(sum);
    table
}

/// Flags bit 0 of every SRAT affinity structure and of a MADT GICC entry.
const ENABLED: u32 = 1;
const DISABLED: u32 = 0;
/// Memory Affinity flags bit 1, Hot Pluggable.
const HOT_PLUGGABLE: u32 = 1 << 1;

/// Processor Local APIC/SAPIC Affinity Structure, 16 bytes.
fn lapic(apic_id: u8, domain: u32, flags: u32) -> Vec<u8> {
    let domain = domain.to_le_bytes();
    let mut entry = vec![0, 16, domain[0], apic_id];
    entry.extend_from_slice(&flags.to_le_bytes());
    entry.push(0); // Local SAPIC EID
    entry.extend_from_slice(&domain[1..]);
    entry.extend_from_slice(&0_u32.to_le_bytes()); // Clock domain
    entry
}

/// Processor Local x2APIC Affinity Structure, 24 bytes.
fn x2apic(x2apic_id: u32, domain: u32, flags: u32) -> Vec<u8> {
    let mut entry = vec![2, 24, 0, 0];
    entry.extend_from_slice(&domain.to_le_bytes());
    entry.extend_from_slice(&x2apic_id.to_le_bytes());
    entry.extend_from_slice(&flags.to_le_bytes());
    entry.extend_from_slice(&0_u32.to_le_bytes()); // Clock domain
    entry.extend_from_slice(&0_u32.to_le_bytes());
    entry
}

/// GICC Affinity Structure, 18 bytes.
fn gicc_affinity(processor_uid: u32, domain: u32, flags: u32) -> Vec<u8> {
    let mut entry = vec![3, 18];
    entry.extend_from_slice(&domain.to_le_bytes());
    entry.extend_from_slice(&processor_uid.to_le_bytes());
    entry.extend_from_slice(&flags.to_le_bytes());
    entry.extend_from_slice(&0_u32.to_le_bytes()); // Clock domain
    entry
}

/// Memory Affinity Structure, 40 bytes, base and length each
/// split into low and high words.
fn memory(base: u64, length: u64, domain: u32, flags: u32) -> Vec<u8> {
    let mut entry = vec![1, 40];
    entry.extend_from_slice(&domain.to_le_bytes());
    entry.extend_from_slice(&0_u16.to_le_bytes());
    entry.extend_from_slice(&(base as u32).to_le_bytes());
    entry.extend_from_slice(&((base >> 32) as u32).to_le_bytes());
    entry.extend_from_slice(&(length as u32).to_le_bytes());
    entry.extend_from_slice(&((length >> 32) as u32).to_le_bytes());
    entry.extend_from_slice(&0_u32.to_le_bytes());
    entry.extend_from_slice(&flags.to_le_bytes());
    entry.extend_from_slice(&0_u64.to_le_bytes());
    entry
}

/// SRAT: a reserved word that must be 1, eight reserved
/// bytes, then the affinity structures.
fn srat(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut body = 1_u32.to_le_bytes().to_vec();
    body.extend_from_slice(&0_u64.to_le_bytes());
    body.extend(entries.concat());
    sdt(b"SRAT", 3, &body)
}

/// SLIT: the locality count, then the row-major matrix.
fn slit(localities: u64, matrix: &[u8]) -> Vec<u8> {
    let mut body = localities.to_le_bytes().to_vec();
    body.extend_from_slice(matrix);
    sdt(b"SLIT", 1, &body)
}

/// A full SLIT whose diagonal is 10 and whose other entries are `remote`
/// except where `pairs` names them.
fn slit_with(localities: u32, remote: u8, pairs: &[(u32, u32, u8)]) -> Vec<u8> {
    let side = localities as usize;
    let mut matrix = vec![remote; side * side];
    for locality in 0..side {
        matrix[locality * side + locality] = 10;
    }
    for &(from, to, distance) in pairs {
        matrix[from as usize * side + to as usize] = distance;
    }
    slit(u64::from(localities), &matrix)
}

/// GIC CPU Interface (GICC) Structure, 82 bytes: the ACPI
/// processor UID at offset 8, flags at 12, MPIDR at 68.
fn madt_gicc(processor_uid: u32, mpidr: u64) -> Vec<u8> {
    let mut entry = vec![0; 82];
    entry[0] = 0x0b;
    entry[1] = 82;
    entry[8..12].copy_from_slice(&processor_uid.to_le_bytes());
    entry[12..16].copy_from_slice(&ENABLED.to_le_bytes());
    entry[68..76].copy_from_slice(&mpidr.to_le_bytes());
    entry
}

/// MADT: the local interrupt controller address and flags,
/// then the interrupt controller structures.
fn madt(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut body = 0_u32.to_le_bytes().to_vec();
    body.extend_from_slice(&0_u32.to_le_bytes());
    body.extend(entries.concat());
    sdt(b"APIC", 5, &body)
}

/// The tables and the XSDT that lists them, kept alive while open.
struct Firmware {
    _tables: Vec<Vec<u8>>,
    xsdt: Vec<u8>,
}

impl Firmware {
    fn new(tables: Vec<Vec<u8>>) -> Self {
        let addresses: Vec<u8> = tables
            .iter()
            .flat_map(|table| (table.as_ptr() as u64).to_le_bytes())
            .collect();
        Self {
            xsdt: sdt(b"XSDT", 1, &addresses),
            _tables: tables,
        }
    }

    fn open(&self) -> AcpiTables<HostMemory> {
        // SAFETY: the XSDT and every table it lists are owned by `self`
        // and outlive the returned tables' use in each test.
        unsafe { AcpiTables::from_rsdt(HostMemory, self.xsdt.as_ptr() as usize, 8) }
            .expect("the XSDT is well formed")
    }
}

struct Ids(Vec<u64>);

impl ProcessorIdentities for Ids {
    fn processor_count(&self) -> usize {
        self.0.len()
    }

    fn hardware_id(&self, processor: ProcessorId) -> u64 {
        self.0[usize::from(processor.id())]
    }
}

fn x86_numa(firmware: &Firmware, ids: Vec<u64>) -> AcpiNuma<HostMemory, Ids, LocalApicKey> {
    AcpiNuma::new(&firmware.open(), Ids(ids), LocalApicKey).expect("the description is valid")
}

fn processor_nodes(numa: &impl NumaTopology) -> Vec<usize> {
    (0..numa.processor_count())
        .map(|processor| {
            numa.processor_node(ProcessorId::new(processor as u16))
                .index()
        })
        .collect()
}

fn memory_of(numa: &impl NumaTopology) -> Vec<MemoryAffinity> {
    numa.memory_affinity().collect()
}

fn distances(numa: &impl NumaTopology) -> Vec<Vec<u8>> {
    let nodes = numa.node_count();
    (0..nodes)
        .map(|from| {
            (0..nodes)
                .map(|to| numa.distance(NodeId::new(from), NodeId::new(to)).get())
                .collect()
        })
        .collect()
}

fn affinity(physical: std::ops::Range<u64>, node: usize) -> MemoryAffinity {
    MemoryAffinity {
        physical,
        node: NodeId::new(node),
    }
}

/// The shape QEMU's `q35` emits for `-smp 4,sockets=2` and two `-numa
/// node`s: one Local APIC entry per processor, node 0's memory split
/// around the 640 KiB-1 MiB hole, disabled empty entries padding the
/// memory structures, and a SLIT for `-numa dist`.
fn q35_two_nodes() -> Firmware {
    Firmware::new(vec![
        srat(&[
            lapic(0, 0, ENABLED),
            lapic(1, 0, ENABLED),
            lapic(2, 1, ENABLED),
            lapic(3, 1, ENABLED),
            memory(0, 0xa_0000, 0, ENABLED),
            memory(0x10_0000, 0x3ff0_0000, 0, ENABLED),
            memory(0x4000_0000, 0x4000_0000, 1, ENABLED),
            memory(0, 0, 0, DISABLED),
            memory(0, 0, 0, DISABLED),
        ]),
        slit(2, &[10, 20, 20, 10]),
    ])
}

#[test]
fn two_nodes_from_local_apic_affinity() {
    let firmware = q35_two_nodes();
    let numa = x86_numa(&firmware, vec![0, 1, 2, 3]);
    assert_eq!(numa.node_count(), 2);
    assert_eq!(numa.processor_count(), 4);
    assert_eq!(processor_nodes(&numa), vec![0, 0, 1, 1]);
    assert_eq!(distances(&numa), vec![vec![10, 20], vec![20, 10]]);
    assert_eq!(
        memory_of(&numa),
        vec![
            affinity(0..0xa_0000, 0),
            affinity(0x10_0000..0x4000_0000, 0),
            affinity(0x4000_0000..0x8000_0000, 1),
        ]
    );
}

#[test]
fn processor_order_is_the_backends_not_the_tables() {
    let firmware = q35_two_nodes();
    let numa = x86_numa(&firmware, vec![3, 0, 2, 1]);
    assert_eq!(processor_nodes(&numa), vec![1, 0, 1, 0]);
}

#[test]
fn x2apic_affinity_keys_a_processor_past_eight_bits() {
    let firmware = Firmware::new(vec![srat(&[
        lapic(0, 0, ENABLED),
        x2apic(0x1_0000, 1, ENABLED),
        memory(0, 0x4000_0000, 0, ENABLED),
        memory(0x4000_0000, 0x4000_0000, 1, ENABLED),
    ])]);
    let numa = x86_numa(&firmware, vec![0, 0x1_0000]);
    assert_eq!(processor_nodes(&numa), vec![0, 1]);
}

#[test]
fn sparse_proximity_domains_map_to_dense_nodes_ascending() {
    let firmware = Firmware::new(vec![
        srat(&[
            lapic(0, 40, ENABLED),
            lapic(1, 5, ENABLED),
            memory(0, 0x4000_0000, 40, ENABLED),
            memory(0x4000_0000, 0x4000_0000, 5, ENABLED),
        ]),
        slit_with(41, 20, &[(5, 40, 30), (40, 5, 31)]),
    ]);
    let numa = x86_numa(&firmware, vec![0, 1]);
    assert_eq!(numa.node_count(), 2);
    assert_eq!(processor_nodes(&numa), vec![1, 0]);
    assert_eq!(distances(&numa), vec![vec![10, 30], vec![31, 10]]);
    assert_eq!(
        memory_of(&numa),
        vec![
            affinity(0..0x4000_0000, 1),
            affinity(0x4000_0000..0x8000_0000, 0),
        ]
    );
}

#[test]
fn disabled_entries_place_nothing_and_name_no_node() {
    let firmware = Firmware::new(vec![srat(&[
        lapic(0, 7, DISABLED),
        lapic(0, 1, ENABLED),
        lapic(1, 2, ENABLED),
        memory(0, 0x4000_0000, 1, ENABLED),
        memory(0x4000_0000, 0x4000_0000, 2, ENABLED),
        memory(0x8000_0000, 0x4000_0000, 9, DISABLED),
    ])]);
    let numa = x86_numa(&firmware, vec![0, 1]);
    assert_eq!(numa.node_count(), 2);
    assert_eq!(processor_nodes(&numa), vec![0, 1]);
    assert_eq!(
        memory_of(&numa),
        vec![
            affinity(0..0x4000_0000, 0),
            affinity(0x4000_0000..0x8000_0000, 1),
        ]
    );
}

/// A length whose high word differs from the base's high word, so a
/// reader that fills the length's high half from the base shows.
#[test]
fn a_memory_range_above_4_gib_keeps_its_own_length() {
    let firmware = Firmware::new(vec![srat(&[
        lapic(0, 0, ENABLED),
        lapic(1, 1, ENABLED),
        memory(0, 0x8000_0000, 0, ENABLED),
        memory(0x1_0000_0000, 0x2_4000_0000, 1, ENABLED),
    ])]);
    let numa = x86_numa(&firmware, vec![0, 1]);
    assert_eq!(
        memory_of(&numa),
        vec![
            affinity(0..0x8000_0000, 0),
            affinity(0x1_0000_0000..0x3_4000_0000, 1),
        ]
    );
}

#[test]
fn hot_pluggable_memory_keeps_its_affinity() {
    let firmware = Firmware::new(vec![srat(&[
        lapic(0, 0, ENABLED),
        memory(0, 0x4000_0000, 0, ENABLED),
        memory(0x1_0000_0000, 0x4000_0000, 0, ENABLED | HOT_PLUGGABLE),
    ])]);
    let numa = x86_numa(&firmware, vec![0]);
    assert_eq!(
        memory_of(&numa),
        vec![
            affinity(0..0x4000_0000, 0),
            affinity(0x1_0000_0000..0x1_4000_0000, 0),
        ]
    );
}

/// Distances that differ by direction, so a transposed lookup shows.
#[test]
fn a_two_locality_slit_is_read_from_then_to() {
    let firmware = Firmware::new(vec![
        srat(&[
            lapic(0, 0, ENABLED),
            lapic(1, 1, ENABLED),
            memory(0, 0x4000_0000, 0, ENABLED),
            memory(0x4000_0000, 0x4000_0000, 1, ENABLED),
        ]),
        slit(2, &[10, 20, 21, 10]),
    ]);
    let numa = x86_numa(&firmware, vec![0, 1]);
    assert_eq!(distances(&numa), vec![vec![10, 20], vec![21, 10]]);
}

#[test]
fn three_nodes_with_an_asymmetric_matrix() {
    let firmware = Firmware::new(vec![
        srat(&[
            lapic(0, 0, ENABLED),
            lapic(1, 1, ENABLED),
            lapic(2, 2, ENABLED),
            memory(0, 0x4000_0000, 0, ENABLED),
        ]),
        slit(3, &[10, 30, 20, 30, 10, 25, 21, 25, 10]),
    ]);
    let numa = x86_numa(&firmware, vec![0, 1, 2]);
    assert_eq!(
        distances(&numa),
        vec![vec![10, 30, 20], vec![30, 10, 25], vec![21, 25, 10]]
    );
}

#[test]
fn no_slit_is_the_uniform_remote_distance() {
    let firmware = Firmware::new(vec![srat(&[
        lapic(0, 0, ENABLED),
        lapic(1, 1, ENABLED),
        memory(0, 0x4000_0000, 0, ENABLED),
    ])]);
    let numa = x86_numa(&firmware, vec![0, 1]);
    assert_eq!(
        numa.distance(NodeId::new(0), NodeId::new(1)),
        NodeDistance::UNIFORM_REMOTE
    );
    assert_eq!(
        numa.distance(NodeId::new(1), NodeId::new(1)),
        NodeDistance::LOCAL
    );
}

fn assert_single_node(numa: &impl NumaTopology, processors: usize) {
    let single = SingleNode::new(processors);
    assert_eq!(numa.node_count(), single.node_count());
    assert_eq!(numa.processor_count(), single.processor_count());
    assert_eq!(processor_nodes(numa), processor_nodes(&single));
    assert_eq!(
        numa.distance(NodeId::FIRST, NodeId::FIRST),
        single.distance(NodeId::FIRST, NodeId::FIRST)
    );
    assert_eq!(memory_of(numa), memory_of(&single));
}

#[test]
fn no_srat_answers_exactly_as_single_node() {
    let firmware = Firmware::new(vec![madt(&[])]);
    let numa = x86_numa(&firmware, vec![0, 1, 2]);
    assert_single_node(&numa, 3);
}

#[test]
fn an_srat_without_enabled_entries_answers_as_single_node() {
    let firmware = Firmware::new(vec![
        srat(&[lapic(0, 3, DISABLED), memory(0, 0, 0, DISABLED)]),
        slit(1, &[10]),
    ]);
    let numa = x86_numa(&firmware, vec![0, 1]);
    assert_single_node(&numa, 2);
}

/// The shape QEMU's `virt` emits with `-acpi` under `-numa`: the GICC
/// Affinity entries name ACPI processor UIDs, which the MADT's GICC
/// entries pair with MPIDRs, and the backend lists the bootstrap
/// processor first. One MADT MPIDR carries the U bit (bit 30), which
/// keys nothing.
fn virt_two_nodes() -> Firmware {
    Firmware::new(vec![
        madt(&[
            madt_gicc(0, 0x0),
            madt_gicc(1, 0x1),
            madt_gicc(2, 0x100 | 1 << 30),
            madt_gicc(3, 0x101),
        ]),
        srat(&[
            gicc_affinity(0, 0, ENABLED),
            gicc_affinity(1, 0, ENABLED),
            gicc_affinity(2, 1, ENABLED),
            gicc_affinity(3, 1, ENABLED),
            memory(0x4000_0000, 0x4000_0000, 0, ENABLED),
            memory(0x8000_0000, 0x4000_0000, 1, ENABLED),
        ]),
        slit(2, &[10, 20, 20, 10]),
    ])
}

fn arm_numa(
    firmware: &Firmware,
    ids: Vec<u64>,
) -> Result<AcpiNuma<HostMemory, Ids, GiccUidKey<HostMemory>>, FirmwareNumaError> {
    let tables = firmware.open();
    AcpiNuma::new(&tables, Ids(ids), GiccUidKey::new(&tables)?)
}

#[test]
fn gicc_affinity_maps_through_the_madt_in_the_backends_order() {
    let firmware = virt_two_nodes();
    let numa = arm_numa(&firmware, vec![0x100, 0x0, 0x1, 0x101]).expect("valid description");
    assert_eq!(numa.node_count(), 2);
    assert_eq!(processor_nodes(&numa), vec![1, 0, 0, 1]);
    assert_eq!(distances(&numa), vec![vec![10, 20], vec![20, 10]]);
    assert_eq!(
        memory_of(&numa),
        vec![
            affinity(0x4000_0000..0x8000_0000, 0),
            affinity(0x8000_0000..0xc000_0000, 1),
        ]
    );
}

#[test]
fn a_local_apic_entry_keys_nothing_on_a_gicc_machine() {
    // The key types read only their own architecture's entries.
    let entry = lapic(0, 0, ENABLED);
    let table = srat(&[entry]);
    let firmware = Firmware::new(vec![table, madt(&[madt_gicc(0, 0)])]);
    let tables = firmware.open();
    let srat = tables
        .find_table::<acpi::sdt::srat::Srat>()
        .expect("the SRAT is listed");
    let entries: Vec<_> = srat.get().entries().collect();
    assert!(GiccUidKey::<HostMemory>::processor_entry(&entries[0]).is_none());
    assert!(LocalApicKey::processor_entry(&entries[0]).is_some());
}

fn refusal<K: SratProcessorKey>(
    result: Result<AcpiNuma<HostMemory, Ids, K>, FirmwareNumaError>,
) -> FirmwareNumaError {
    match result {
        Ok(_) => panic!("the description should have been refused"),
        Err(error) => error,
    }
}

fn x86_refusal(firmware: &Firmware, ids: Vec<u64>) -> FirmwareNumaError {
    refusal(AcpiNuma::new(&firmware.open(), Ids(ids), LocalApicKey))
}

#[test]
fn a_processor_without_an_enabled_entry_is_refused() {
    let firmware = Firmware::new(vec![srat(&[
        lapic(0, 0, ENABLED),
        lapic(1, 1, DISABLED),
        memory(0, 0x4000_0000, 0, ENABLED),
    ])]);
    assert_eq!(
        x86_refusal(&firmware, vec![0, 1]),
        FirmwareNumaError::ProcessorWithoutAffinity {
            processor: 1,
            hardware_id: 1,
            key: 1
        }
    );
}

#[test]
fn a_processor_in_two_domains_is_refused() {
    let firmware = Firmware::new(vec![srat(&[
        lapic(0, 0, ENABLED),
        x2apic(0, 1, ENABLED),
        memory(0, 0x4000_0000, 0, ENABLED),
    ])]);
    assert_eq!(
        x86_refusal(&firmware, vec![0]),
        FirmwareNumaError::ProcessorAffinityConflict {
            processor: 0,
            key: 0,
            first: 0,
            second: 1
        }
    );
}

#[test]
fn an_mpidr_no_madt_gicc_names_is_refused() {
    let firmware = virt_two_nodes();
    assert_eq!(
        refusal(arm_numa(&firmware, vec![0x0, 0x200])),
        FirmwareNumaError::ProcessorWithoutGicc {
            processor: 1,
            mpidr: 0x200
        }
    );
}

#[test]
fn gicc_keys_without_an_madt_are_refused() {
    let firmware = Firmware::new(vec![srat(&[gicc_affinity(0, 0, ENABLED)])]);
    assert_eq!(
        refusal(arm_numa(&firmware, vec![0])),
        FirmwareNumaError::MadtMissing
    );
}

fn two_nodes_with(slit: Vec<u8>) -> Firmware {
    Firmware::new(vec![
        srat(&[
            lapic(0, 0, ENABLED),
            lapic(1, 1, ENABLED),
            memory(0, 0x4000_0000, 0, ENABLED),
        ]),
        slit,
    ])
}

#[test]
fn a_slit_diagonal_other_than_ten_is_refused() {
    assert_eq!(
        x86_refusal(&two_nodes_with(slit(2, &[10, 20, 20, 11])), vec![0, 1]),
        FirmwareNumaError::SlitDiagonal {
            locality: 1,
            distance: 11
        }
    );
}

#[test]
fn a_slit_off_diagonal_at_or_below_ten_is_refused() {
    assert_eq!(
        x86_refusal(&two_nodes_with(slit(2, &[10, 10, 20, 10])), vec![0, 1]),
        FirmwareNumaError::SlitNotRemote {
            from: 0,
            to: 1,
            distance: 10
        }
    );
}

/// Linux's `slit_valid()` checks every locality, not only the ones the
/// SRAT names, and so does the reader.
#[test]
fn a_bad_entry_between_localities_the_srat_does_not_name_is_refused() {
    let firmware = two_nodes_with(slit_with(3, 20, &[(2, 1, 5)]));
    assert_eq!(
        x86_refusal(&firmware, vec![0, 1]),
        FirmwareNumaError::SlitNotRemote {
            from: 2,
            to: 1,
            distance: 5
        }
    );
}

#[test]
fn a_domain_beyond_the_slit_localities_is_refused() {
    let firmware = Firmware::new(vec![
        srat(&[
            lapic(0, 0, ENABLED),
            lapic(1, 2, ENABLED),
            memory(0, 0x4000_0000, 0, ENABLED),
        ]),
        slit(2, &[10, 20, 20, 10]),
    ]);
    assert_eq!(
        x86_refusal(&firmware, vec![0, 1]),
        FirmwareNumaError::SlitMissingDomain {
            domain: 2,
            localities: 2
        }
    );
}

#[test]
fn a_slit_too_short_for_its_matrix_is_refused() {
    assert_eq!(
        x86_refusal(&two_nodes_with(slit(2, &[10, 20, 20])), vec![0, 1]),
        FirmwareNumaError::SlitTruncated {
            localities: 2,
            length: 36 + 8 + 3
        }
    );
}

#[test]
fn a_locality_count_whose_square_overflows_is_refused() {
    assert_eq!(
        x86_refusal(&two_nodes_with(slit(u64::MAX, &[10])), vec![0, 1]),
        FirmwareNumaError::SlitTruncated {
            localities: u64::MAX,
            length: 36 + 8 + 1
        }
    );
}

#[test]
fn a_memory_range_past_the_address_space_is_refused() {
    let firmware = Firmware::new(vec![srat(&[
        lapic(0, 0, ENABLED),
        memory(0xffff_ffff_ffff_0000, 0x2_0000, 0, ENABLED),
    ])]);
    assert_eq!(
        x86_refusal(&firmware, vec![0]),
        FirmwareNumaError::SratMemoryRangeOverflow {
            domain: 0,
            base: 0xffff_ffff_ffff_0000,
            length: 0x2_0000
        }
    );
}

fn one_range_per_domain(domains: u32) -> Firmware {
    let mut entries = vec![lapic(0, 0, ENABLED)];
    entries.extend((0..domains).map(|domain| {
        memory(
            u64::from(domain) * 0x1000_0000,
            0x1000_0000,
            domain,
            ENABLED,
        )
    }));
    Firmware::new(vec![srat(&entries)])
}

#[test]
fn more_proximity_domains_than_the_cap_is_refused() {
    assert_eq!(
        x86_refusal(&one_range_per_domain(MAX_NODES as u32 + 1), vec![0]),
        FirmwareNumaError::TooManyProximityDomains {
            domain: MAX_NODES as u32,
            limit: MAX_NODES
        }
    );
}

#[test]
fn the_cap_itself_is_accepted() {
    let firmware = one_range_per_domain(MAX_NODES as u32);
    let numa = x86_numa(&firmware, vec![0]);
    assert_eq!(numa.node_count(), MAX_NODES);
    assert_eq!(memory_of(&numa).len(), MAX_NODES);
}

#[test]
fn a_backend_with_no_processors_is_refused() {
    assert_eq!(
        x86_refusal(&q35_two_nodes(), Vec::new()),
        FirmwareNumaError::NoProcessors
    );
}
