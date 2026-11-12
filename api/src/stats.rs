//! System statistics syscalls.

use crate::bindings::helios::system::stats as raw;
pub use crate::bindings::helios::system::stats::subscribe;
pub use crate::bindings::helios::system::stats::{
    AudioStream, BlockDevice, GrantedDevice, HostShareCache, InputDevice, Iommu, IommuEndpoint,
    Memory, MemoryBalloon, MemoryPressure, MonoNanos, Network, NetworkQueue, Permille, Processor,
    Processors, Sample, Swap, SwapDisk, SwapFaultSource, SwapFaults, SwapPolicy, SwapStore,
    SwapZero,
};

/// Returns the latest coherent system statistics snapshot.
pub fn snapshot() -> Sample {
    raw::snapshot()
}
