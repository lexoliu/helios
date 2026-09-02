//! System statistics syscalls.

use crate::bindings::helios::system::stats as raw;
pub use crate::bindings::helios::system::stats::subscribe;
pub use crate::bindings::helios::system::stats::{
    BlockDevice, Iommu, IommuEndpoint, Memory, MemoryBalloon, MemoryPressure, MonoNanos, Permille,
    Processor, Processors, Sample,
};

/// Returns the latest coherent system statistics snapshot.
pub fn snapshot() -> Sample {
    raw::snapshot()
}
