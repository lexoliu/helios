//! Privileged live program instance inspection calls.

use crate::bindings::helios::system::instances as raw;
pub use crate::bindings::helios::system::instances::{Instance, InstanceId, MonoNanos, Permille};

/// Returns the current live program instances visible to the privileged caller.
pub fn snapshot() -> Vec<Instance> {
    raw::snapshot()
}
