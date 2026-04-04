//! Privileged live wasm instance inspection syscalls.

use crate::bindings::helios::system::instances as raw;
pub use crate::bindings::helios::system::instances::{Instance, InstanceId, MonoNanos, Permille};

/// Returns the current live wasm instances visible to the privileged caller.
pub fn snapshot() -> Vec<Instance> {
    raw::snapshot()
}
