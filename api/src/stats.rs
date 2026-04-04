//! System statistics syscalls.

use crate::bindings::helios::system::stats as raw;
pub use crate::bindings::helios::system::stats::subscribe;
pub use crate::bindings::helios::system::stats::{
    Memory, MemoryPressure, MonoNanos, Permille, Processor, Processors, Sample,
};

/// Returns the latest coherent system statistics snapshot.
pub fn snapshot() -> Sample {
    raw::snapshot()
}
