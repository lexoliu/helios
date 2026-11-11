//! NUMA locality from a flattened devicetree
//! (`Documentation/devicetree/bindings/numa.txt`).
//!
//! Concurrency contract: construction reads the blob once on the calling
//! processor and keeps a node list and a distance matrix; every method of
//! the view afterwards only reads those and the blob, so it is lock-free
//! and callable from any processor.

use arrayvec::ArrayVec;
use core::ops::Range;
use fdt::Fdt;
use fdt::node::{FdtNode, RawReg};
use helios_hal::cpu::ProcessorId;
use helios_hal::numa::{MAX_NODES, MemoryAffinity, NodeDistance, NodeId, NumaTopology};

use crate::ProcessorIdentities;
use crate::error::{FirmwareNumaError, NodeName};

const NUMA_NODE_ID: &str = "numa-node-id";
const DISTANCE_MAP_COMPATIBLE: &str = "numa-distance-map-v1";
/// Cells in one `distance-matrix` entry: node, node, distance.
const DISTANCE_ENTRY_CELLS: usize = 3;
/// The largest distance a `NodeDistance` holds (255 is unreachable).
const MAX_DISTANCE: u32 = u8::MAX as u32;

/// A devicetree's NUMA description, validated at construction so that no
/// method of the view can fail.
///
/// Without any `numa-node-id` on a cpu or memory node the machine has one
/// node and answers as `SingleNode` does.
pub struct DeviceTreeNuma<'f, P: ProcessorIdentities> {
    fdt: &'f Fdt<'f>,
    identities: P,
    /// The distinct `numa-node-id` values, ascending; index is the `NodeId`.
    /// Empty when the tree carries none.
    node_ids: ArrayVec<u32, MAX_NODES>,
    /// Row-major distances by `NodeId`; zero is an entry the map did not
    /// give.
    distances: [[u8; MAX_NODES]; MAX_NODES],
}

impl<'f, P: ProcessorIdentities> DeviceTreeNuma<'f, P> {
    pub fn new(fdt: &'f Fdt<'f>, identities: P) -> Result<Self, FirmwareNumaError> {
        if identities.processor_count() == 0 {
            return Err(FirmwareNumaError::NoProcessors);
        }
        let mut this = Self {
            fdt,
            identities,
            node_ids: ArrayVec::new(),
            distances: [[0; MAX_NODES]; MAX_NODES],
        };
        this.collect_node_ids()?;
        if this.node_ids.is_empty() {
            return Ok(this);
        }
        this.validate_memory()?;
        this.read_distances()?;
        this.validate_processors()?;
        Ok(this)
    }

    fn cpu_nodes(&self) -> impl Iterator<Item = FdtNode<'f, 'f>> + use<'f, P> {
        let fdt: &'f Fdt<'f> = self.fdt;
        fdt.find_node("/cpus")
            .into_iter()
            .flat_map(|cpus| cpus.children())
            .filter(|node| base_name(node) == "cpu")
    }

    fn memory_nodes(&self) -> impl Iterator<Item = FdtNode<'f, 'f>> + use<'f, P> {
        let fdt: &'f Fdt<'f> = self.fdt;
        fdt.find_node("/")
            .into_iter()
            .flat_map(|root| root.children())
            .filter(|node| base_name(node) == "memory")
    }

    /// Collects the distinct ids and refuses a cpu or memory node that
    /// lacks one when another has it.
    fn collect_node_ids(&mut self) -> Result<(), FirmwareNumaError> {
        let mut cpu_without = None;
        let mut memory_without = None;
        let nodes = self
            .cpu_nodes()
            .map(|node| (node, true))
            .chain(self.memory_nodes().map(|node| (node, false)));
        for (node, is_cpu) in nodes {
            match node_id_of(&node)? {
                Some(id) => match self.node_ids.binary_search(&id) {
                    Ok(_) => {}
                    Err(position) => self.node_ids.try_insert(position, id).map_err(|_| {
                        FirmwareNumaError::TooManyNodes {
                            id,
                            limit: MAX_NODES,
                        }
                    })?,
                },
                None if is_cpu => cpu_without = cpu_without.or(Some(NodeName::new(node.name))),
                None => memory_without = memory_without.or(Some(NodeName::new(node.name))),
            }
        }
        if self.node_ids.is_empty() {
            return Ok(());
        }
        if let Some(node) = cpu_without {
            return Err(FirmwareNumaError::CpuWithoutNode { node });
        }
        if let Some(node) = memory_without {
            return Err(FirmwareNumaError::MemoryWithoutNode { node });
        }
        Ok(())
    }

    fn validate_memory(&self) -> Result<(), FirmwareNumaError> {
        for node in self.memory_nodes() {
            let regs = node.raw_reg().ok_or_else(|| reg_malformed(&node))?;
            let mut entries = 0;
            for reg in regs {
                range_of(&reg).map_err(|overflow| match overflow {
                    RangeError::Malformed => reg_malformed(&node),
                    RangeError::Overflow { base, size } => FirmwareNumaError::MemoryRangeOverflow {
                        node: NodeName::new(node.name),
                        base,
                        size,
                    },
                })?;
                entries += 1;
            }
            let stride = node
                .raw_reg()
                .and_then(|mut regs| regs.next())
                .map_or(0, |reg| reg.address.len() + reg.size.len());
            let length = node.property("reg").map_or(0, |reg| reg.value.len());
            if entries == 0 || entries * stride != length {
                return Err(reg_malformed(&node));
            }
        }
        Ok(())
    }

    fn read_distances(&mut self) -> Result<(), FirmwareNumaError> {
        let fdt: &'f Fdt<'f> = self.fdt;
        let Some(map) = fdt.find_node("/distance-map") else {
            return Ok(());
        };
        let is_v1 = map
            .compatible()
            .is_some_and(|compatible| compatible.all().any(|c| c == DISTANCE_MAP_COMPATIBLE));
        if !is_v1 {
            return Err(FirmwareNumaError::DistanceMapMalformed);
        }
        let Some(matrix) = map.property("distance-matrix") else {
            return Ok(());
        };
        let bytes = matrix.value;
        if bytes.len() % (DISTANCE_ENTRY_CELLS * 4) != 0 {
            return Err(FirmwareNumaError::DistanceMapMalformed);
        }
        for entry in bytes.chunks_exact(DISTANCE_ENTRY_CELLS * 4) {
            let cell = |index: usize| {
                u32::from_be_bytes(entry[index * 4..index * 4 + 4].try_into().unwrap())
            };
            let (from, to, distance) = (cell(0), cell(1), cell(2));
            let index_of = |id: u32| {
                self.node_ids
                    .binary_search(&id)
                    .map_err(|_| FirmwareNumaError::DistanceUnknownNode { node_id: id })
            };
            let (a, b) = (index_of(from)?, index_of(to)?);
            if a == b {
                if distance != u32::from(NodeDistance::LOCAL.get()) {
                    return Err(FirmwareNumaError::DistanceDiagonal {
                        node_id: from,
                        distance,
                    });
                }
            } else if distance <= u32::from(NodeDistance::LOCAL.get()) {
                return Err(FirmwareNumaError::DistanceNotRemote { from, to, distance });
            }
            if distance > MAX_DISTANCE {
                return Err(FirmwareNumaError::DistanceOutOfRange { from, to, distance });
            }
            let raw = distance as u8;
            for (row, column) in [(a, b), (b, a)] {
                let slot = &mut self.distances[row][column];
                if *slot != 0 && *slot != raw {
                    return Err(FirmwareNumaError::DistanceConflict {
                        from,
                        to,
                        first: u32::from(*slot),
                        second: distance,
                    });
                }
                *slot = raw;
            }
        }
        Ok(())
    }

    fn validate_processors(&self) -> Result<(), FirmwareNumaError> {
        for index in 0..self.identities.processor_count() {
            let processor = ProcessorId::new(index as u16);
            let hardware_id = self.identities.hardware_id(processor);
            if self.cpu_node_of(hardware_id).is_none() {
                return Err(FirmwareNumaError::ProcessorWithoutCpuNode {
                    processor: processor.id(),
                    hardware_id,
                });
            }
        }
        Ok(())
    }

    /// The cpu node one of whose `reg` entries is `hardware_id`.
    fn cpu_node_of(&self, hardware_id: u64) -> Option<FdtNode<'f, 'f>> {
        self.cpu_nodes().find(|node| {
            node.raw_reg()
                .is_some_and(|mut regs| regs.any(|reg| be_value(reg.address) == Some(hardware_id)))
        })
    }

    fn node_of(&self, node: &FdtNode<'_, '_>) -> NodeId {
        let id = node_id_of(node)
            .ok()
            .flatten()
            .expect("construction checked every cpu and memory node has a numa-node-id");
        self.index_of(id)
    }

    fn index_of(&self, id: u32) -> NodeId {
        let index = self
            .node_ids
            .binary_search(&id)
            .expect("construction collected every cpu and memory node id");
        NodeId::new(index)
    }
}

impl<P: ProcessorIdentities> NumaTopology for DeviceTreeNuma<'_, P> {
    type MemoryAffinities<'a>
        = DeviceTreeMemory<'a, P>
    where
        Self: 'a;

    fn node_count(&self) -> usize {
        self.node_ids.len().max(1)
    }

    fn processor_count(&self) -> usize {
        self.identities.processor_count()
    }

    fn processor_node(&self, processor: ProcessorId) -> NodeId {
        if self.node_ids.is_empty() {
            return NodeId::FIRST;
        }
        let hardware_id = self.identities.hardware_id(processor);
        let node = self
            .cpu_node_of(hardware_id)
            .expect("construction checked every processor has a cpu node");
        self.node_of(&node)
    }

    fn distance(&self, from: NodeId, to: NodeId) -> NodeDistance {
        debug_assert!(
            from.index() < self.node_count() && to.index() < self.node_count(),
            "nodes {} and {} are beyond the {} the platform has",
            from.index(),
            to.index(),
            self.node_count()
        );
        match self.distances[from.index()][to.index()] {
            0 if from == to => NodeDistance::LOCAL,
            0 => NodeDistance::UNIFORM_REMOTE,
            raw => NodeDistance::new(raw),
        }
    }

    fn memory_affinity(&self) -> Self::MemoryAffinities<'_> {
        DeviceTreeMemory {
            numa: self,
            node: 0,
            range: 0,
        }
    }
}

/// Every `reg` range of every `memory` node with its node. Each step walks
/// to its position again, because the devicetree reader has no iterator type
/// one can store without allocating; a tree holds a handful of nodes and
/// the kernel reads it once.
pub struct DeviceTreeMemory<'a, P: ProcessorIdentities> {
    numa: &'a DeviceTreeNuma<'a, P>,
    node: usize,
    range: usize,
}

impl<P: ProcessorIdentities> Iterator for DeviceTreeMemory<'_, P> {
    type Item = MemoryAffinity;

    fn next(&mut self) -> Option<MemoryAffinity> {
        if self.numa.node_ids.is_empty() {
            // `SingleNode`'s answer: all of physical memory on the one node.
            if self.node != 0 {
                return None;
            }
            self.node = 1;
            return Some(MemoryAffinity {
                physical: 0..u64::MAX,
                node: NodeId::FIRST,
            });
        }
        loop {
            let node = self.numa.memory_nodes().nth(self.node)?;
            match node.raw_reg().and_then(|mut regs| regs.nth(self.range)) {
                Some(reg) => {
                    self.range += 1;
                    let physical = range_of(&reg)
                        .ok()
                        .expect("construction checked every memory range");
                    return Some(MemoryAffinity {
                        physical,
                        node: self.numa.node_of(&node),
                    });
                }
                None => {
                    self.node += 1;
                    self.range = 0;
                }
            }
        }
    }
}

fn base_name<'a>(node: &FdtNode<'_, 'a>) -> &'a str {
    node.name.split('@').next().unwrap_or(node.name)
}

fn node_id_of(node: &FdtNode<'_, '_>) -> Result<Option<u32>, FirmwareNumaError> {
    let Some(property) = node.property(NUMA_NODE_ID) else {
        return Ok(None);
    };
    let bytes: [u8; 4] =
        property
            .value
            .try_into()
            .map_err(|_| FirmwareNumaError::NodeIdMalformed {
                node: NodeName::new(node.name),
            })?;
    Ok(Some(u32::from_be_bytes(bytes)))
}

fn reg_malformed(node: &FdtNode<'_, '_>) -> FirmwareNumaError {
    FirmwareNumaError::RegMalformed {
        node: NodeName::new(node.name),
    }
}

/// A big-endian cell run as an integer; more than two cells do not fit.
fn be_value(cells: &[u8]) -> Option<u64> {
    if cells.is_empty() || cells.len() > 8 {
        return None;
    }
    Some(
        cells
            .iter()
            .fold(0, |value, &byte| (value << 8) | u64::from(byte)),
    )
}

enum RangeError {
    Malformed,
    Overflow { base: u64, size: u64 },
}

fn range_of(reg: &RawReg<'_>) -> Result<Range<u64>, RangeError> {
    let base = be_value(reg.address).ok_or(RangeError::Malformed)?;
    let size = be_value(reg.size).ok_or(RangeError::Malformed)?;
    let end = base
        .checked_add(size)
        .ok_or(RangeError::Overflow { base, size })?;
    Ok(base..end)
}
