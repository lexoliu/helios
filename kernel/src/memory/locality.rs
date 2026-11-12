//! The machine's memory locality, copied once out of the platform's
//! [`NumaTopology`] and read-only from then on.
//!
//! # Concurrency contract
//!
//! Built on the bootstrap processor inside `prime_bootstrap_allocator`,
//! before any secondary starts, and immutable after that. Every method
//! reads without synchronization from any processor, in fault and
//! interrupt context included.
//!
//! # Orders
//!
//! Both orders list a node's candidates nearest first. Candidates at the
//! same distance are ordered by `(candidate - node - 1) mod node_count`,
//! so equal-distance fallback spreads over the peers the way Linux's
//! `find_next_best_node` staggers it, without keeping any load state.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;

use arrayvec::ArrayVec;
use helios_hal::cpu::ProcessorId;
use helios_hal::numa::{MAX_NODES, NodeDistance, NodeId, NumaTopology};

use crate::MAX_BOOT_MEMORY_REGIONS;

/// A run of user-pool memory on one node: `start..end` in the kernel's
/// direct map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeRun {
    pub start: usize,
    pub end: usize,
    pub node: NodeId,
}

pub struct Locality {
    /// Index: [`ProcessorId`].
    processor_nodes: Box<[NodeId]>,
    /// Index: [`NodeId`].
    node_processors: Box<[Box<[ProcessorId]>]>,
    /// `node_count²` entries, row = from.
    distances: Box<[NodeDistance]>,
    /// Per node: every node, itself first, then by distance.
    scheduling_order: Box<[Box<[NodeId]>]>,
    /// Per node: the nodes that have user memory, by distance. A node
    /// without memory of its own is served by its nearest node with some,
    /// which is Linux's `local_memory_node`.
    memory_order: Box<[Box<[NodeId]>]>,
    /// User-pool bytes per node.
    memory_bytes: Box<[usize]>,
    /// The user-pool runs sorted by address, adjacent same-node regions
    /// merged.
    frame_nodes: ArrayVec<NodeRun, MAX_BOOT_MEMORY_REGIONS>,
}

impl Locality {
    /// Copies `numa` and checks it, panicking with the offending entry
    /// named.
    ///
    /// `pool_regions` is the user pool's memory in the kernel's direct
    /// map, each region on its node. That every usable byte lies in
    /// exactly one affinity range is checked where the physical addresses
    /// are still known, by `usable_node_regions`: the kernel sees the
    /// direct map only.
    pub(crate) fn new<Numa: NumaTopology>(numa: &Numa, pool_regions: &[NodeRun]) -> Self {
        let node_count = numa.node_count();
        assert!(
            (1..=MAX_NODES).contains(&node_count),
            "NUMA description names {node_count} nodes; the kernel supports 1..={MAX_NODES}"
        );
        let processor_count = numa.processor_count();
        assert!(processor_count != 0, "NUMA description names no processors");

        let mut processor_nodes = Vec::with_capacity(processor_count);
        let mut node_processors = alloc::vec![Vec::new(); node_count];
        for index in 0..processor_count {
            let processor = ProcessorId::new(index as u16);
            let node = numa.processor_node(processor);
            assert!(
                node.index() < node_count,
                "NUMA description places processor {index} on node {} of {node_count}",
                node.index()
            );
            processor_nodes.push(node);
            node_processors[node.index()].push(processor);
        }

        let mut distances = Vec::with_capacity(node_count * node_count);
        for from in 0..node_count {
            for to in 0..node_count {
                let distance = numa.distance(NodeId::new(from), NodeId::new(to));
                if from == to {
                    assert!(
                        distance == NodeDistance::LOCAL,
                        "NUMA distance from node {from} to itself is {}, not {}",
                        distance.get(),
                        NodeDistance::LOCAL.get()
                    );
                } else {
                    assert!(
                        distance > NodeDistance::LOCAL,
                        "NUMA distance from node {from} to node {to} is {}, not above {}",
                        distance.get(),
                        NodeDistance::LOCAL.get()
                    );
                }
                distances.push(distance);
            }
        }

        let mut affinity: Vec<_> = numa.memory_affinity().collect();
        affinity.sort_unstable_by_key(|entry| entry.physical.start);
        for entry in &affinity {
            assert!(
                entry.node.index() < node_count,
                "NUMA memory affinity {:#x?} names node {} of {node_count}",
                entry.physical,
                entry.node.index()
            );
        }
        for pair in affinity.windows(2) {
            assert!(
                pair[0].physical.end <= pair[1].physical.start,
                "NUMA memory affinity ranges {:#x?} and {:#x?} overlap",
                pair[0].physical,
                pair[1].physical
            );
        }

        let mut memory_bytes = alloc::vec![0usize; node_count];
        for run in pool_regions {
            assert!(
                run.node.index() < node_count,
                "user memory {:#x}..{:#x} is on node {} of {node_count}",
                run.start,
                run.end,
                run.node.index()
            );
            memory_bytes[run.node.index()] += run.end - run.start;
        }
        let frame_nodes = merged_runs(pool_regions);

        let scheduling_order = (0..node_count)
            .map(|from| nearest_first(&distances, from, |_| true))
            .collect();
        let memory_order = (0..node_count)
            .map(|from| nearest_first(&distances, from, |node| memory_bytes[node] != 0))
            .collect();

        Self {
            processor_nodes: processor_nodes.into_boxed_slice(),
            node_processors: node_processors
                .into_iter()
                .map(Vec::into_boxed_slice)
                .collect(),
            distances: distances.into_boxed_slice(),
            scheduling_order,
            memory_order,
            memory_bytes: memory_bytes.into_boxed_slice(),
            frame_nodes,
        }
    }

    pub fn node_count(&self) -> usize {
        self.node_processors.len()
    }

    pub fn processor_count(&self) -> usize {
        self.processor_nodes.len()
    }

    /// The node `processor` is on.
    pub fn node_of(&self, processor: ProcessorId) -> NodeId {
        self.processor_nodes[usize::from(processor.id())]
    }

    /// The processors on `node`, in ascending order.
    pub fn processors_of(&self, node: NodeId) -> &[ProcessorId] {
        &self.node_processors[node.index()]
    }

    pub fn distance(&self, from: NodeId, to: NodeId) -> NodeDistance {
        self.distances[from.index() * self.node_count() + to.index()]
    }

    /// Every node, `node` first, then the rest nearest first. A
    /// scheduler drains queues in this order.
    pub fn scheduling_order(&self, node: NodeId) -> &[NodeId] {
        &self.scheduling_order[node.index()]
    }

    /// The nodes that have user memory, nearest to `node` first. An
    /// allocation on a processor of `node` tries them in this order.
    pub fn memory_order(&self, node: NodeId) -> &[NodeId] {
        &self.memory_order[node.index()]
    }

    /// Bytes of user memory the pool was seeded with on `node`.
    pub fn memory_bytes(&self, node: NodeId) -> usize {
        self.memory_bytes[node.index()]
    }

    /// The node the user-pool address `address` lies on. Panics on an
    /// address outside the pool: a frame the pool never owned has no node
    /// to return to.
    pub fn node_of_address(&self, address: usize) -> NodeId {
        let after = self.frame_nodes.partition_point(|run| run.start <= address);
        after
            .checked_sub(1)
            .map(|index| self.frame_nodes[index])
            .filter(|run| address < run.end)
            .unwrap_or_else(|| panic!("address {address:#x} is outside the user memory pool"))
            .node
    }

    /// The distance matrix as the boot line prints it: rows separated by
    /// `;`, entries by `,`.
    pub fn distances_display(&self) -> impl fmt::Display + '_ {
        DistanceMatrix(self)
    }

    /// `node`'s processors as a cpulist (`0-1,4`).
    pub fn processors_display(&self, node: NodeId) -> impl fmt::Display + '_ {
        CpuList(self.processors_of(node))
    }
}

/// The nodes `include` accepts, in the order from `from`'s row of the
/// `distances` matrix: nearest first, ties rotated (see the module docs).
fn nearest_first(
    distances: &[NodeDistance],
    from: usize,
    include: impl Fn(usize) -> bool,
) -> Box<[NodeId]> {
    let node_count = distances.len().isqrt();
    let mut order: Vec<usize> = (0..node_count).filter(|&node| include(node)).collect();
    order.sort_by_key(|&node| {
        (
            distances[from * node_count + node],
            (node + node_count - from - 1) % node_count,
        )
    });
    order.into_iter().map(NodeId::new).collect()
}

/// Address-sorted runs with touching same-node regions merged.
fn merged_runs(regions: &[NodeRun]) -> ArrayVec<NodeRun, MAX_BOOT_MEMORY_REGIONS> {
    let mut sorted: ArrayVec<NodeRun, MAX_BOOT_MEMORY_REGIONS> = ArrayVec::new();
    for run in regions {
        sorted.try_push(*run).unwrap_or_else(|_| {
            panic!("user memory is in more than {MAX_BOOT_MEMORY_REGIONS} runs")
        });
    }
    sorted.sort_unstable_by_key(|run| run.start);
    let mut merged: ArrayVec<NodeRun, MAX_BOOT_MEMORY_REGIONS> = ArrayVec::new();
    for run in sorted {
        if let Some(last) = merged.last_mut() {
            assert!(
                last.end <= run.start,
                "user memory {:#x}..{:#x} overlaps {:#x}..{:#x}",
                last.start,
                last.end,
                run.start,
                run.end
            );
            if last.end == run.start && last.node == run.node {
                last.end = run.end;
                continue;
            }
        }
        merged.push(run);
    }
    merged
}

struct DistanceMatrix<'a>(&'a Locality);

impl fmt::Display for DistanceMatrix<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.0.node_count();
        for from in 0..count {
            if from != 0 {
                formatter.write_str(";")?;
            }
            for to in 0..count {
                if to != 0 {
                    formatter.write_str(",")?;
                }
                write!(
                    formatter,
                    "{}",
                    self.0.distance(NodeId::new(from), NodeId::new(to)).get()
                )?;
            }
        }
        Ok(())
    }
}

/// Processors as the Linux cpulist format: ascending runs as `first-last`,
/// separated by commas.
struct CpuList<'a>(&'a [ProcessorId]);

impl fmt::Display for CpuList<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut processors = self.0.iter().map(|processor| processor.id()).peekable();
        let mut first = true;
        while let Some(start) = processors.next() {
            let mut end = start;
            while processors.next_if(|&next| next == end + 1).is_some() {
                end += 1;
            }
            if !first {
                formatter.write_str(",")?;
            }
            first = false;
            if start == end {
                write!(formatter, "{start}")?;
            } else {
                write!(formatter, "{start}-{end}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use core::ops::Range;
    use helios_hal::numa::MemoryAffinity;

    use super::*;

    /// A description a test spells out, node by node.
    struct Described {
        processors: Vec<usize>,
        /// Row-major, `node_count²`.
        distances: Vec<u8>,
        affinity: Vec<(Range<u64>, usize)>,
    }

    impl NumaTopology for Described {
        type MemoryAffinities<'a> = alloc::vec::IntoIter<MemoryAffinity>;

        fn node_count(&self) -> usize {
            self.distances.len().isqrt()
        }

        fn processor_count(&self) -> usize {
            self.processors.len()
        }

        fn processor_node(&self, processor: ProcessorId) -> NodeId {
            NodeId::new(self.processors[usize::from(processor.id())])
        }

        fn distance(&self, from: NodeId, to: NodeId) -> NodeDistance {
            NodeDistance::new(self.distances[from.index() * self.node_count() + to.index()])
        }

        fn memory_affinity(&self) -> Self::MemoryAffinities<'_> {
            self.affinity
                .iter()
                .map(|(physical, node)| MemoryAffinity {
                    physical: physical.clone(),
                    node: NodeId::new(*node),
                })
                .collect::<Vec<_>>()
                .into_iter()
        }
    }

    fn run(range: Range<usize>, node: usize) -> NodeRun {
        NodeRun {
            start: range.start,
            end: range.end,
            node: NodeId::new(node),
        }
    }

    fn order(nodes: &[NodeId]) -> Vec<usize> {
        nodes.iter().map(|node| node.index()).collect()
    }

    fn four_nodes_all_remote_equal() -> Described {
        Described {
            processors: alloc::vec![0, 1, 2, 3],
            #[rustfmt::skip]
            distances: alloc::vec![
                10, 20, 20, 20,
                20, 10, 20, 20,
                20, 20, 10, 20,
                20, 20, 20, 10,
            ],
            affinity: alloc::vec![],
        }
    }

    #[test]
    fn equal_distances_rotate_by_node_so_peers_do_not_all_fall_back_to_the_lowest_id() {
        let numa = four_nodes_all_remote_equal();
        let pool: Vec<_> = (0..4)
            .map(|n| run(n * 0x1000..(n + 1) * 0x1000, n))
            .collect();
        let locality = Locality::new(&numa, &pool);
        assert_eq!(order(locality.memory_order(NodeId::new(0))), [0, 1, 2, 3]);
        assert_eq!(order(locality.memory_order(NodeId::new(1))), [1, 2, 3, 0]);
        assert_eq!(order(locality.memory_order(NodeId::new(2))), [2, 3, 0, 1]);
        assert_eq!(order(locality.memory_order(NodeId::new(3))), [3, 0, 1, 2]);
        assert_eq!(
            order(locality.scheduling_order(NodeId::new(2))),
            [2, 3, 0, 1]
        );
    }

    #[test]
    fn an_asymmetric_matrix_orders_each_row_by_its_own_distances() {
        let numa = Described {
            processors: alloc::vec![0, 1, 2],
            #[rustfmt::skip]
            distances: alloc::vec![
                10, 30, 20,
                30, 10, 20,
                20, 20, 10,
            ],
            affinity: alloc::vec![],
        };
        let pool: Vec<_> = (0..3)
            .map(|n| run(n * 0x1000..(n + 1) * 0x1000, n))
            .collect();
        let locality = Locality::new(&numa, &pool);
        assert_eq!(order(locality.memory_order(NodeId::new(0))), [0, 2, 1]);
        assert_eq!(order(locality.scheduling_order(NodeId::new(0))), [0, 2, 1]);
        assert_eq!(order(locality.memory_order(NodeId::new(1))), [1, 2, 0]);
    }

    #[test]
    fn the_memory_order_skips_a_node_without_memory() {
        let numa = Described {
            processors: alloc::vec![0, 1, 2],
            #[rustfmt::skip]
            distances: alloc::vec![
                10, 20, 40,
                20, 10, 30,
                40, 30, 10,
            ],
            affinity: alloc::vec![],
        };
        // Node 1 has processors and no memory.
        let pool = [run(0..0x1000, 0), run(0x1000..0x2000, 2)];
        let locality = Locality::new(&numa, &pool);
        assert_eq!(order(locality.memory_order(NodeId::new(1))), [0, 2]);
        assert_eq!(order(locality.scheduling_order(NodeId::new(1))), [1, 0, 2]);
        assert_eq!(locality.memory_bytes(NodeId::new(1)), 0);
        assert_eq!(locality.memory_bytes(NodeId::new(2)), 0x1000);
    }

    #[test]
    fn processors_and_addresses_resolve_to_their_nodes() {
        let numa = Described {
            processors: alloc::vec![0, 0, 1, 1, 0],
            #[rustfmt::skip]
            distances: alloc::vec![
                10, 20,
                20, 10,
            ],
            affinity: alloc::vec![],
        };
        let pool = [
            run(0x3000..0x4000, 1),
            run(0x1000..0x2000, 0),
            run(0x2000..0x3000, 0),
        ];
        let locality = Locality::new(&numa, &pool);
        assert_eq!(locality.node_of(ProcessorId::new(3)), NodeId::new(1));
        assert_eq!(
            locality.processors_display(NodeId::new(0)).to_string(),
            "0-1,4"
        );
        assert_eq!(
            locality.processors_display(NodeId::new(1)).to_string(),
            "2-3"
        );
        assert_eq!(locality.distances_display().to_string(), "10,20;20,10");
        assert_eq!(locality.node_of_address(0x1000), NodeId::new(0));
        assert_eq!(locality.node_of_address(0x2fff), NodeId::new(0));
        assert_eq!(locality.node_of_address(0x3000), NodeId::new(1));
        assert_eq!(
            locality.frame_nodes.len(),
            2,
            "touching same-node regions merge"
        );
    }

    #[test]
    #[should_panic(expected = "outside the user memory pool")]
    fn an_address_in_a_gap_has_no_node() {
        let numa = Described {
            processors: alloc::vec![0],
            distances: alloc::vec![10],
            affinity: alloc::vec![],
        };
        let locality = Locality::new(&numa, &[run(0x1000..0x2000, 0), run(0x3000..0x4000, 0)]);
        locality.node_of_address(0x2000);
    }

    #[test]
    #[should_panic(expected = "from node 1 to itself is 11")]
    fn a_diagonal_other_than_local_is_refused() {
        let numa = Described {
            processors: alloc::vec![0, 1],
            distances: alloc::vec![10, 20, 20, 11],
            affinity: alloc::vec![],
        };
        Locality::new(&numa, &[run(0..0x1000, 0)]);
    }

    #[test]
    #[should_panic(expected = "from node 0 to node 1 is 10, not above 10")]
    fn an_off_diagonal_at_or_below_local_is_refused() {
        let numa = Described {
            processors: alloc::vec![0, 1],
            distances: alloc::vec![10, 10, 20, 10],
            affinity: alloc::vec![],
        };
        Locality::new(&numa, &[run(0..0x1000, 0)]);
    }

    #[test]
    #[should_panic(expected = "places processor 1 on node 2 of 2")]
    fn a_processor_beyond_the_node_count_is_refused() {
        let numa = Described {
            processors: alloc::vec![0, 2],
            distances: alloc::vec![10, 20, 20, 10],
            affinity: alloc::vec![],
        };
        Locality::new(&numa, &[run(0..0x1000, 0)]);
    }

    #[test]
    #[should_panic(expected = "overlap")]
    fn overlapping_affinity_ranges_are_refused() {
        let numa = Described {
            processors: alloc::vec![0, 1],
            distances: alloc::vec![10, 20, 20, 10],
            affinity: alloc::vec![(0x0..0x2000, 0), (0x1000..0x3000, 1)],
        };
        Locality::new(&numa, &[run(0..0x1000, 0)]);
    }

    #[test]
    #[should_panic(expected = "names 0 nodes")]
    fn a_description_without_nodes_is_refused() {
        let numa = Described {
            processors: alloc::vec![],
            distances: alloc::vec![],
            affinity: alloc::vec![],
        };
        Locality::new(&numa, &[]);
    }
}
