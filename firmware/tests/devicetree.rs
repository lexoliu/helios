//! `DeviceTreeNuma` over flattened devicetrees assembled from the DTSpec
//! layout (section 5) inside the test, so no binary fixture is committed.

use fdt::Fdt;
use helios_firmware::{DeviceTreeNuma, FirmwareNumaError, ProcessorIdentities};
use helios_hal::cpu::ProcessorId;
use helios_hal::numa::{MAX_NODES, MemoryAffinity, NodeDistance, NodeId, NumaTopology, SingleNode};

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;

struct Node {
    name: String,
    props: Vec<(String, Vec<u8>)>,
    children: Vec<Node>,
}

fn node(name: &str) -> Node {
    Node {
        name: name.to_owned(),
        props: Vec::new(),
        children: Vec::new(),
    }
}

impl Node {
    fn prop(mut self, name: &str, value: Vec<u8>) -> Self {
        self.props.push((name.to_owned(), value));
        self
    }

    fn cells(self, name: &str, cells: &[u32]) -> Self {
        let bytes = cells.iter().flat_map(|cell| cell.to_be_bytes()).collect();
        self.prop(name, bytes)
    }

    fn string(self, name: &str, value: &str) -> Self {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        self.prop(name, bytes)
    }

    fn child(mut self, child: Node) -> Self {
        self.children.push(child);
        self
    }
}

fn pad4(bytes: &mut Vec<u8>) {
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn emit(node: &Node, structure: &mut Vec<u8>, strings: &mut Vec<u8>) {
    push_u32(structure, FDT_BEGIN_NODE);
    structure.extend_from_slice(node.name.as_bytes());
    structure.push(0);
    pad4(structure);
    for (name, value) in &node.props {
        // Strings are deduplicated by whole-name match.
        let needle = [name.as_bytes(), &[0]].concat();
        let offset = strings
            .windows(needle.len())
            .position(|window| window == needle)
            .filter(|&at| at == 0 || strings[at - 1] == 0)
            .unwrap_or_else(|| {
                let at = strings.len();
                strings.extend_from_slice(&needle);
                at
            });
        push_u32(structure, FDT_PROP);
        push_u32(structure, value.len() as u32);
        push_u32(structure, offset as u32);
        structure.extend_from_slice(value);
        pad4(structure);
    }
    for child in &node.children {
        emit(child, structure, strings);
    }
    push_u32(structure, FDT_END_NODE);
}

/// Header, one empty memory reservation block, structure block, strings.
fn blob(root: &Node) -> Vec<u8> {
    const HEADER: u32 = 40;
    const RESERVATIONS: u32 = 16;
    let mut structure = Vec::new();
    let mut strings = Vec::new();
    emit(root, &mut structure, &mut strings);
    push_u32(&mut structure, FDT_END);
    let off_struct = HEADER + RESERVATIONS;
    let off_strings = off_struct + structure.len() as u32;
    let total = off_strings + strings.len() as u32;
    let mut out = Vec::new();
    for word in [
        0xd00d_feed,
        total,
        off_struct,
        off_strings,
        HEADER,
        17,
        16,
        0,
        strings.len() as u32,
        structure.len() as u32,
    ] {
        push_u32(&mut out, word);
    }
    out.extend_from_slice(&[0; RESERVATIONS as usize]);
    out.extend_from_slice(&structure);
    out.extend_from_slice(&strings);
    out
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

fn cpu(reg: u32, numa_node: Option<u32>) -> Node {
    let cpu = node(&format!("cpu@{reg:x}"))
        .string("device_type", "cpu")
        .cells("reg", &[reg]);
    match numa_node {
        Some(id) => cpu.cells("numa-node-id", &[id]),
        None => cpu,
    }
}

fn cpus(list: Vec<Node>) -> Node {
    let mut cpus = node("cpus")
        .cells("#address-cells", &[1])
        .cells("#size-cells", &[0]);
    for entry in list {
        cpus = cpus.child(entry);
    }
    cpus
}

/// A 1/1-cell memory node.
fn memory32(base: u32, size: u32, id: Option<u32>) -> Node {
    let memory = node(&format!("memory@{base:x}"))
        .string("device_type", "memory")
        .cells("reg", &[base, size]);
    match id {
        Some(id) => memory.cells("numa-node-id", &[id]),
        None => memory,
    }
}

fn distance_map(triplets: &[[u32; 3]]) -> Node {
    node("distance-map")
        .string("compatible", "numa-distance-map-v1")
        .cells("distance-matrix", &triplets.concat())
}

fn root(cells: u32, children: Vec<Node>) -> Node {
    let mut root = node("")
        .cells("#address-cells", &[cells])
        .cells("#size-cells", &[cells]);
    for child in children {
        root = root.child(child);
    }
    root
}

fn memory_of(numa: &impl NumaTopology) -> Vec<MemoryAffinity> {
    numa.memory_affinity().collect()
}

fn two_node_tree() -> Node {
    root(
        1,
        vec![
            cpus(vec![cpu(0, Some(0)), cpu(1, Some(1))]),
            memory32(0x4000_0000, 0x1000_0000, Some(0)),
            memory32(0x8000_0000, 0x2000_0000, Some(1)),
            distance_map(&[[0, 0, 10], [0, 1, 20], [1, 0, 20], [1, 1, 10]]),
        ],
    )
}

#[test]
fn two_nodes_with_two_processors_and_memory_on_each() {
    let bytes = blob(&two_node_tree());
    let fdt = Fdt::new(&bytes).unwrap();
    let numa = DeviceTreeNuma::new(&fdt, Ids(vec![0, 1])).unwrap();
    assert_eq!(numa.node_count(), 2);
    assert_eq!(numa.processor_count(), 2);
    assert_eq!(numa.processor_node(ProcessorId::new(0)), NodeId::new(0));
    assert_eq!(numa.processor_node(ProcessorId::new(1)), NodeId::new(1));
    assert_eq!(
        numa.distance(NodeId::new(0), NodeId::new(0)),
        NodeDistance::LOCAL
    );
    assert_eq!(numa.distance(NodeId::new(0), NodeId::new(1)).get(), 20);
    assert_eq!(numa.distance(NodeId::new(1), NodeId::new(0)).get(), 20);
    assert_eq!(
        memory_of(&numa),
        vec![
            MemoryAffinity {
                physical: 0x4000_0000..0x5000_0000,
                node: NodeId::new(0)
            },
            MemoryAffinity {
                physical: 0x8000_0000..0xa000_0000,
                node: NodeId::new(1)
            },
        ]
    );
}

#[test]
fn processor_order_is_the_backends_not_the_trees() {
    let bytes = blob(&two_node_tree());
    let fdt = Fdt::new(&bytes).unwrap();
    let numa = DeviceTreeNuma::new(&fdt, Ids(vec![1, 0])).unwrap();
    assert_eq!(numa.processor_node(ProcessorId::new(0)), NodeId::new(1));
    assert_eq!(numa.processor_node(ProcessorId::new(1)), NodeId::new(0));
}

#[test]
fn sparse_node_ids_map_to_dense_ones_ascending() {
    let tree = root(
        1,
        vec![
            cpus(vec![cpu(0, Some(40)), cpu(1, Some(7))]),
            memory32(0x4000_0000, 0x1000_0000, Some(40)),
            distance_map(&[[7, 40, 30]]),
        ],
    );
    let bytes = blob(&tree);
    let fdt = Fdt::new(&bytes).unwrap();
    let numa = DeviceTreeNuma::new(&fdt, Ids(vec![0, 1])).unwrap();
    assert_eq!(numa.node_count(), 2);
    assert_eq!(numa.processor_node(ProcessorId::new(0)), NodeId::new(1));
    assert_eq!(numa.processor_node(ProcessorId::new(1)), NodeId::new(0));
    assert_eq!(memory_of(&numa)[0].node, NodeId::new(1));
    assert_eq!(numa.distance(NodeId::new(0), NodeId::new(1)).get(), 30);
}

#[test]
fn three_nodes_with_a_matrix_given_in_one_direction_only() {
    // Row 0 reads 10, 30, 20 once both directions are written.
    let tree = root(
        1,
        vec![
            cpus(vec![cpu(0, Some(0)), cpu(1, Some(1)), cpu(2, Some(2))]),
            memory32(0x4000_0000, 0x1000_0000, Some(0)),
            distance_map(&[
                [0, 0, 10],
                [0, 1, 30],
                [0, 2, 20],
                [1, 1, 10],
                [1, 2, 25],
                [2, 2, 10],
            ]),
        ],
    );
    let bytes = blob(&tree);
    let fdt = Fdt::new(&bytes).unwrap();
    let numa = DeviceTreeNuma::new(&fdt, Ids(vec![0, 1, 2])).unwrap();
    assert_eq!(numa.node_count(), 3);
    let d = |a, b| numa.distance(NodeId::new(a), NodeId::new(b)).get();
    assert_eq!([d(0, 0), d(0, 1), d(0, 2)], [10, 30, 20]);
    assert_eq!([d(1, 0), d(1, 1), d(1, 2)], [30, 10, 25]);
    assert_eq!([d(2, 0), d(2, 1), d(2, 2)], [20, 25, 10]);
}

#[test]
fn a_missing_pair_or_map_is_the_uniform_remote_distance() {
    let tree = root(
        1,
        vec![
            cpus(vec![cpu(0, Some(0)), cpu(1, Some(1))]),
            memory32(0x4000_0000, 0x1000_0000, Some(0)),
        ],
    );
    let bytes = blob(&tree);
    let fdt = Fdt::new(&bytes).unwrap();
    let numa = DeviceTreeNuma::new(&fdt, Ids(vec![0, 1])).unwrap();
    assert_eq!(
        numa.distance(NodeId::new(0), NodeId::new(1)),
        NodeDistance::UNIFORM_REMOTE
    );
    assert_eq!(
        numa.distance(NodeId::new(1), NodeId::new(1)),
        NodeDistance::LOCAL
    );
}

#[test]
fn no_numa_properties_is_one_node() {
    let tree = root(
        1,
        vec![
            cpus(vec![cpu(0, None), cpu(1, None), cpu(2, None)]),
            memory32(0x4000_0000, 0x1000_0000, None),
        ],
    );
    let bytes = blob(&tree);
    let fdt = Fdt::new(&bytes).unwrap();
    let numa = DeviceTreeNuma::new(&fdt, Ids(vec![0, 1, 2])).unwrap();
    assert_eq!(numa.node_count(), 1);
    assert_eq!(numa.processor_count(), 3);
    for processor in 0..3 {
        assert_eq!(
            numa.processor_node(ProcessorId::new(processor)),
            NodeId::FIRST
        );
    }
    assert_eq!(
        numa.distance(NodeId::FIRST, NodeId::FIRST),
        NodeDistance::LOCAL
    );
    assert_eq!(
        memory_of(&numa),
        vec![MemoryAffinity {
            physical: 0..u64::MAX,
            node: NodeId::FIRST
        }]
    );
}

#[test]
fn a_tree_without_numa_properties_answers_exactly_as_single_node() {
    let tree = root(
        1,
        vec![
            cpus(vec![cpu(0, None), cpu(1, None)]),
            memory32(0x4000_0000, 0x1000_0000, None),
            memory32(0x8000_0000, 0x1000_0000, None),
        ],
    );
    let bytes = blob(&tree);
    let fdt = Fdt::new(&bytes).unwrap();
    let numa = DeviceTreeNuma::new(&fdt, Ids(vec![0, 1])).unwrap();
    let single = SingleNode::new(2);
    assert_eq!(numa.node_count(), single.node_count());
    assert_eq!(numa.processor_count(), single.processor_count());
    for processor in 0..2 {
        let processor = ProcessorId::new(processor);
        assert_eq!(
            numa.processor_node(processor),
            single.processor_node(processor)
        );
    }
    assert_eq!(
        numa.distance(NodeId::FIRST, NodeId::FIRST),
        single.distance(NodeId::FIRST, NodeId::FIRST)
    );
    assert_eq!(memory_of(&numa), memory_of(&single));
}

/// The shape QEMU's `virt` board emits under `-numa`: two address cells
/// for an MPIDR `reg`, a `cpu-map` beside the cpu nodes, and a backend
/// that lists the bootstrap processor first.
#[test]
fn mpidr_keyed_cpus_beside_a_cpu_map_map_in_the_backends_order() {
    let mpidr_cpu = |mpidr: u32, id: u32| {
        node(&format!("cpu@{mpidr:x}"))
            .string("device_type", "cpu")
            .cells("reg", &[0, mpidr])
            .cells("numa-node-id", &[id])
    };
    let cpu_map = node("cpu-map").child(node("socket0").child(node("cluster0")));
    let cpus = node("cpus")
        .cells("#address-cells", &[2])
        .cells("#size-cells", &[0])
        .child(mpidr_cpu(0x0, 0))
        .child(mpidr_cpu(0x1, 0))
        .child(mpidr_cpu(0x100, 1))
        .child(mpidr_cpu(0x101, 1))
        .child(cpu_map);
    let tree = root(
        1,
        vec![
            cpus,
            memory32(0x4000_0000, 0x4000_0000, Some(0)),
            memory32(0x8000_0000, 0x4000_0000, Some(1)),
            distance_map(&[[0, 1, 20]]),
        ],
    );
    let bytes = blob(&tree);
    let fdt = Fdt::new(&bytes).unwrap();
    let numa = DeviceTreeNuma::new(&fdt, Ids(vec![0x100, 0x0, 0x1, 0x101])).unwrap();
    assert_eq!(numa.node_count(), 2);
    let nodes: Vec<_> = (0..4)
        .map(|processor| numa.processor_node(ProcessorId::new(processor)).index())
        .collect();
    assert_eq!(nodes, vec![1, 0, 0, 1]);
    assert_eq!(numa.distance(NodeId::new(1), NodeId::new(0)).get(), 20);
}

fn refusal(tree: Node, ids: Vec<u64>) -> FirmwareNumaError {
    let bytes = blob(&tree);
    let fdt = Fdt::new(&bytes).unwrap();
    match DeviceTreeNuma::new(&fdt, Ids(ids)) {
        Ok(_) => panic!("the description should have been refused"),
        Err(error) => error,
    }
}

fn with_map(triplets: &[[u32; 3]]) -> Node {
    root(
        1,
        vec![
            cpus(vec![cpu(0, Some(0)), cpu(1, Some(1))]),
            memory32(0x4000_0000, 0x1000_0000, Some(0)),
            distance_map(triplets),
        ],
    )
}

#[test]
fn a_diagonal_other_than_ten_is_refused() {
    assert_eq!(
        refusal(with_map(&[[1, 1, 11]]), vec![0, 1]),
        FirmwareNumaError::DistanceDiagonal {
            node_id: 1,
            distance: 11
        }
    );
}

#[test]
fn an_off_diagonal_at_or_below_ten_is_refused() {
    assert_eq!(
        refusal(with_map(&[[0, 1, 10]]), vec![0, 1]),
        FirmwareNumaError::DistanceNotRemote {
            from: 0,
            to: 1,
            distance: 10
        }
    );
}

#[test]
fn a_node_id_outside_the_node_list_is_refused() {
    assert_eq!(
        refusal(with_map(&[[0, 5, 20]]), vec![0, 1]),
        FirmwareNumaError::DistanceUnknownNode { node_id: 5 }
    );
}

#[test]
fn contradicting_entries_for_one_pair_are_refused() {
    assert_eq!(
        refusal(with_map(&[[0, 1, 20], [1, 0, 30]]), vec![0, 1]),
        FirmwareNumaError::DistanceConflict {
            from: 1,
            to: 0,
            first: 20,
            second: 30
        }
    );
}

#[test]
fn more_distinct_node_ids_than_the_cap_is_refused() {
    let list = (0..=MAX_NODES as u32)
        .map(|id| cpu(id, Some(id)))
        .collect::<Vec<_>>();
    let tree = root(1, vec![cpus(list), memory32(0x4000_0000, 0x1000, Some(0))]);
    assert_eq!(
        refusal(tree, vec![0]),
        FirmwareNumaError::TooManyNodes {
            id: MAX_NODES as u32,
            limit: MAX_NODES
        }
    );
}

#[test]
fn the_cap_itself_is_accepted() {
    let list = (0..MAX_NODES as u32)
        .map(|id| cpu(id, Some(id)))
        .collect::<Vec<_>>();
    let tree = root(1, vec![cpus(list), memory32(0x4000_0000, 0x1000, Some(0))]);
    let bytes = blob(&tree);
    let fdt = Fdt::new(&bytes).unwrap();
    let numa = DeviceTreeNuma::new(&fdt, Ids(vec![0])).unwrap();
    assert_eq!(numa.node_count(), MAX_NODES);
}

#[test]
fn a_cpu_without_a_node_id_among_ones_that_have_it_is_refused() {
    let tree = root(
        1,
        vec![
            cpus(vec![cpu(0, Some(0)), cpu(1, None)]),
            memory32(0x4000_0000, 0x1000_0000, Some(0)),
        ],
    );
    match refusal(tree, vec![0, 1]) {
        FirmwareNumaError::CpuWithoutNode { node } => assert_eq!(node.as_str(), "cpu@1"),
        other => panic!("unexpected refusal: {other}"),
    }
}

#[test]
fn a_memory_node_without_a_node_id_among_ones_that_have_it_is_refused() {
    let tree = root(
        1,
        vec![
            cpus(vec![cpu(0, Some(0))]),
            memory32(0x4000_0000, 0x1000_0000, None),
        ],
    );
    match refusal(tree, vec![0]) {
        FirmwareNumaError::MemoryWithoutNode { node } => {
            assert_eq!(node.as_str(), "memory@40000000")
        }
        other => panic!("unexpected refusal: {other}"),
    }
}

#[test]
fn a_hardware_id_with_no_cpu_node_is_refused() {
    assert_eq!(
        refusal(two_node_tree(), vec![0, 9]),
        FirmwareNumaError::ProcessorWithoutCpuNode {
            processor: 1,
            hardware_id: 9
        }
    );
}

#[test]
fn a_memory_node_with_several_ranges_yields_each() {
    let memory = node("memory@40000000")
        .string("device_type", "memory")
        .cells(
            "reg",
            &[
                0x4000_0000,
                0x1000_0000,
                0x8000_0000,
                0x1000_0000,
                0xc000_0000,
                0x4000,
            ],
        )
        .cells("numa-node-id", &[0]);
    let tree = root(1, vec![cpus(vec![cpu(0, Some(0))]), memory]);
    let bytes = blob(&tree);
    let fdt = Fdt::new(&bytes).unwrap();
    let numa = DeviceTreeNuma::new(&fdt, Ids(vec![0])).unwrap();
    let ranges: Vec<_> = memory_of(&numa).into_iter().map(|m| m.physical).collect();
    assert_eq!(
        ranges,
        vec![
            0x4000_0000..0x5000_0000,
            0x8000_0000..0x9000_0000,
            0xc000_0000..0xc000_4000
        ]
    );
}

#[test]
fn a_ragged_reg_is_refused() {
    let memory = node("memory@40000000")
        .string("device_type", "memory")
        .cells("reg", &[0x4000_0000, 0x1000_0000, 0x8000_0000])
        .cells("numa-node-id", &[0]);
    let tree = root(1, vec![cpus(vec![cpu(0, Some(0))]), memory]);
    match refusal(tree, vec![0]) {
        FirmwareNumaError::RegMalformed { node } => assert_eq!(node.as_str(), "memory@40000000"),
        other => panic!("unexpected refusal: {other}"),
    }
}

#[test]
fn a_range_past_the_address_space_is_refused() {
    let memory = node("memory@ffffffffffff0000")
        .string("device_type", "memory")
        .cells("reg", &[u32::MAX, 0xffff_0000, 0, 0x2_0000])
        .cells("numa-node-id", &[0]);
    let tree = root(2, vec![memory, cpus(vec![cpu(0, Some(0))])]);
    match refusal(tree, vec![0]) {
        FirmwareNumaError::MemoryRangeOverflow { node, base, size } => {
            assert_eq!(node.as_str(), "memory@ffffffffffff0000");
            assert_eq!((base, size), (0xffff_ffff_ffff_0000, 0x2_0000));
        }
        other => panic!("unexpected refusal: {other}"),
    }
}

#[test]
fn two_address_and_size_cells_carry_ranges_above_4_gib() {
    let memory = |base_hi: u32, size_hi: u32, id: u32| {
        node(&format!("memory@{base_hi:x}00000000"))
            .string("device_type", "memory")
            .cells("reg", &[base_hi, 0, size_hi, 0x4000_0000])
            .cells("numa-node-id", &[id])
    };
    let tree = root(
        2,
        vec![
            memory(0x1, 0x1, 0),
            memory(0x3, 0x2, 1),
            cpus(vec![cpu(0, Some(0)), cpu(1, Some(1))]),
            distance_map(&[[0, 1, 40]]),
        ],
    );
    let bytes = blob(&tree);
    let fdt = Fdt::new(&bytes).unwrap();
    let numa = DeviceTreeNuma::new(&fdt, Ids(vec![0, 1])).unwrap();
    assert_eq!(
        memory_of(&numa),
        vec![
            MemoryAffinity {
                physical: 0x1_0000_0000..0x2_4000_0000,
                node: NodeId::new(0)
            },
            MemoryAffinity {
                physical: 0x3_0000_0000..0x5_4000_0000,
                node: NodeId::new(1)
            },
        ]
    );
    assert_eq!(numa.distance(NodeId::new(1), NodeId::new(0)).get(), 40);
}

#[test]
fn a_backend_with_no_processors_is_refused() {
    assert_eq!(
        refusal(two_node_tree(), Vec::new()),
        FirmwareNumaError::NoProcessors
    );
}
