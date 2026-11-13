//! Memory locality as the platform describes it.
//!
//! Concurrency contract: everything here is an immutable value or a
//! read-only view over firmware tables the backend opened on the
//! bootstrap processor. The kernel reads a description exactly once, on
//! the bootstrap processor inside `prime_bootstrap_allocator`, before any
//! secondary starts, and keeps its own copy; nothing in hal is read after
//! that, so no implementation needs a lock or an atomic.

use core::iter::{Once, once};
use core::ops::Range;

use crate::cpu::ProcessorId;

/// The most nodes a description may name. Linux's x86-64 default
/// (`NODES_SHIFT` = 6). A firmware naming more is refused by name.
pub const MAX_NODES: usize = 64;

/// A node: dense, `0..NumaTopology::node_count()`. Firmware numbering
/// (ACPI proximity domains, devicetree `numa-node-id`) is mapped to it in
/// ascending order of the firmware number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(u8);

impl NodeId {
    pub const FIRST: Self = Self(0);

    /// Panics when `index >= MAX_NODES`.
    pub const fn new(index: usize) -> Self {
        assert!(index < MAX_NODES, "node index exceeds MAX_NODES");
        Self(index as u8)
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Relative memory latency between two nodes in ACPI SLIT units:
/// `LOCAL` is 10, a remote node is above it, 255 is unreachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeDistance(u8);

impl NodeDistance {
    pub const LOCAL: Self = Self(10);
    /// What a platform that publishes no distance table means by "remote":
    /// Linux's `REMOTE_DISTANCE`, the convention both firmware paths use.
    pub const UNIFORM_REMOTE: Self = Self(20);

    pub const fn new(raw: u8) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

/// One physical range and the node it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryAffinity {
    pub physical: Range<u64>,
    pub node: NodeId,
}

/// The platform's locality description.
///
/// Contract an implementation states and the kernel checks once, failing
/// with the offending entry named:
/// - `node_count()` is in `1..=MAX_NODES`;
/// - `processor_node(p) < node_count()` for every `p < processor_count()`,
///   and `processor_count()` equals `Cpu::processor_count()`;
/// - `distance(n, n) == LOCAL`, `distance(a, b) > LOCAL` for `a != b`;
/// - `memory_affinity()` ranges are disjoint, and every usable byte the
///   backend hands the kernel lies in exactly one of them.
pub trait NumaTopology {
    type MemoryAffinities<'a>: Iterator<Item = MemoryAffinity> + 'a
    where
        Self: 'a;

    fn node_count(&self) -> usize;
    fn processor_count(&self) -> usize;
    fn processor_node(&self, processor: ProcessorId) -> NodeId;
    fn distance(&self, from: NodeId, to: NodeId) -> NodeDistance;
    fn memory_affinity(&self) -> Self::MemoryAffinities<'_>;
}

/// A platform with no NUMA description: one node holding every processor
/// and all of physical memory.
#[derive(Clone, Copy, Debug)]
pub struct SingleNode {
    processors: usize,
}

impl SingleNode {
    /// Panics on zero processors.
    pub fn new(processors: usize) -> Self {
        assert!(processors != 0, "a machine has at least one processor");
        Self { processors }
    }
}

impl NumaTopology for SingleNode {
    type MemoryAffinities<'a> = Once<MemoryAffinity>;

    fn node_count(&self) -> usize {
        1
    }

    fn processor_count(&self) -> usize {
        self.processors
    }

    fn processor_node(&self, processor: ProcessorId) -> NodeId {
        debug_assert!(
            usize::from(processor.id()) < self.processors,
            "processor {} is beyond the {} the platform has",
            processor.id(),
            self.processors
        );
        NodeId::FIRST
    }

    fn distance(&self, from: NodeId, to: NodeId) -> NodeDistance {
        debug_assert!(
            from == NodeId::FIRST && to == NodeId::FIRST,
            "a single-node platform has no node {} or {}",
            from.index(),
            to.index()
        );
        NodeDistance::LOCAL
    }

    fn memory_affinity(&self) -> Self::MemoryAffinities<'_> {
        once(MemoryAffinity {
            physical: 0..u64::MAX,
            node: NodeId::FIRST,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_round_trips_its_index() {
        assert_eq!(NodeId::new(0), NodeId::FIRST);
        assert_eq!(NodeId::new(MAX_NODES - 1).index(), MAX_NODES - 1);
    }

    #[test]
    #[should_panic(expected = "node index exceeds MAX_NODES")]
    fn node_id_refuses_an_index_past_the_cap() {
        NodeId::new(MAX_NODES);
    }

    #[test]
    fn distances_order_local_below_remote() {
        assert!(NodeDistance::LOCAL < NodeDistance::UNIFORM_REMOTE);
        assert_eq!(NodeDistance::new(31).get(), 31);
    }

    #[test]
    fn single_node_places_everything_on_the_first_node() {
        let numa = SingleNode::new(4);
        assert_eq!(numa.node_count(), 1);
        assert_eq!(numa.processor_count(), 4);
        for processor in 0..4 {
            assert_eq!(
                numa.processor_node(ProcessorId::new(processor)),
                NodeId::FIRST
            );
        }
        assert_eq!(
            numa.distance(NodeId::FIRST, NodeId::FIRST),
            NodeDistance::LOCAL
        );
        let mut affinity = numa.memory_affinity();
        assert_eq!(
            affinity.next(),
            Some(MemoryAffinity {
                physical: 0..u64::MAX,
                node: NodeId::FIRST
            })
        );
        assert_eq!(affinity.next(), None);
    }

    #[test]
    #[should_panic(expected = "at least one processor")]
    fn single_node_refuses_zero_processors() {
        SingleNode::new(0);
    }
}
