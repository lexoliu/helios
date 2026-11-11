use arrayvec::ArrayString;
use core::fmt;
use thiserror::Error;

/// Longest node name an error carries; a longer one is cut at a character
/// boundary. Devicetree names are at most 31 characters (DTSpec 2.2.1), so
/// the cap is generous.
const NODE_NAME_CAPACITY: usize = 64;

/// A devicetree node name copied out of the blob, so an error outlives it
/// without an allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeName(ArrayString<NODE_NAME_CAPACITY>);

impl NodeName {
    pub(crate) fn new(name: &str) -> Self {
        let mut end = name.len().min(NODE_NAME_CAPACITY);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        Self(ArrayString::from(&name[..end]).expect("cut to the capacity"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A firmware NUMA description the readers refuse. Every variant names the
/// entry it refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum FirmwareNumaError {
    #[error("the backend reports no processors")]
    NoProcessors,
    #[error("node `{node}`: `numa-node-id` is not a single 32-bit cell")]
    NodeIdMalformed { node: NodeName },
    #[error("`numa-node-id` {id} is one distinct node id too many; at most {limit} are supported")]
    TooManyNodes { id: u32, limit: usize },
    #[error("cpu node `{node}` has no `numa-node-id` while other nodes do")]
    CpuWithoutNode { node: NodeName },
    #[error("memory node `{node}` has no `numa-node-id` while other nodes do")]
    MemoryWithoutNode { node: NodeName },
    #[error("node `{node}`: `reg` is missing or does not hold whole address/size entries")]
    RegMalformed { node: NodeName },
    #[error(
        "memory node `{node}`: range at {base:#x} of {size:#x} bytes overflows the address space"
    )]
    MemoryRangeOverflow {
        node: NodeName,
        base: u64,
        size: u64,
    },
    #[error(
        "processor {processor} has hardware id {hardware_id:#x} but no cpu node has that `reg`"
    )]
    ProcessorWithoutCpuNode { processor: u16, hardware_id: u64 },
    #[error("`/distance-map` is not a `numa-distance-map-v1` whole of u32 triplets")]
    DistanceMapMalformed,
    #[error("`/distance-map` names node id {node_id}, which no cpu or memory node carries")]
    DistanceUnknownNode { node_id: u32 },
    #[error("`/distance-map` gives node {node_id} distance {distance} to itself; it must be 10")]
    DistanceDiagonal { node_id: u32, distance: u32 },
    #[error(
        "`/distance-map` gives nodes {from} to {to} distance {distance}; remote must exceed 10"
    )]
    DistanceNotRemote { from: u32, to: u32, distance: u32 },
    #[error("`/distance-map` gives nodes {from} to {to} distance {distance}, above 255")]
    DistanceOutOfRange { from: u32, to: u32, distance: u32 },
    #[error("`/distance-map` gives nodes {from} and {to} both {first} and {second}")]
    DistanceConflict {
        from: u32,
        to: u32,
        first: u32,
        second: u32,
    },
}
