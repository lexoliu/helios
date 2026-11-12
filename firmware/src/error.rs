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
    #[cfg(feature = "devicetree")]
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
    #[error(
        "SRAT proximity domain {domain} is one distinct domain too many; at most {limit} are \
         supported"
    )]
    TooManyProximityDomains { domain: u32, limit: usize },
    #[error(
        "SRAT memory affinity for proximity domain {domain}: range at {base:#x} of {length:#x} \
         bytes overflows the address space"
    )]
    SratMemoryRangeOverflow { domain: u32, base: u64, length: u64 },
    #[error("the SRAT keys processors by ACPI processor UID and there is no MADT to map them")]
    MadtMissing,
    #[error("processor {processor} has MPIDR {mpidr:#x} but no MADT GICC entry names it")]
    ProcessorWithoutGicc { processor: u16, mpidr: u64 },
    #[error(
        "processor {processor} (hardware id {hardware_id:#x}, SRAT key {key:#x}) has no enabled \
         SRAT affinity entry"
    )]
    ProcessorWithoutAffinity {
        processor: u16,
        hardware_id: u64,
        key: u64,
    },
    #[error(
        "processor {processor} (SRAT key {key:#x}) has enabled SRAT affinity entries in both \
         proximity domain {first} and {second}"
    )]
    ProcessorAffinityConflict {
        processor: u16,
        key: u64,
        first: u32,
        second: u32,
    },
    #[error(
        "the SLIT names {localities} localities but its {length} bytes do not hold the \
         {localities}x{localities} matrix"
    )]
    SlitTruncated { localities: u64, length: u32 },
    #[error("the SLIT gives locality {locality} distance {distance} to itself; it must be 10")]
    SlitDiagonal { locality: u32, distance: u8 },
    #[error("the SLIT gives localities {from} to {to} distance {distance}; remote must exceed 10")]
    SlitNotRemote { from: u32, to: u32, distance: u8 },
    #[error(
        "SRAT proximity domain {domain} is beyond the {localities} localities the SLIT describes"
    )]
    SlitMissingDomain { domain: u32, localities: u64 },
}
