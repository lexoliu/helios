//! System statistics syscalls.

use crate::bindings::helios::system::stats as raw;
pub use crate::bindings::helios::system::stats::subscribe;
pub use crate::bindings::helios::system::stats::{
    BlockDevice, HostShareCache, Iommu, IommuEndpoint, Memory, MemoryBalloon, MemoryPressure,
    MonoNanos, Network, NetworkQueue, Permille, Processor, Processors, Sample, Swap,
};

/// Returns the latest coherent system statistics snapshot.
pub fn snapshot() -> Sample {
    raw::snapshot()
}
