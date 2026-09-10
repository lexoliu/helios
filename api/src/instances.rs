//! Privileged live program instance inspection calls.

use crate::bindings::helios::system::instances as raw;
pub use crate::bindings::helios::system::instances::{
    Instance, InstanceId, KillError, MonoNanos, Permille,
};

/// Returns the current live program instances visible to the privileged caller.
pub fn snapshot() -> Vec<Instance> {
    raw::snapshot()
}

/// Asks a live instance to stop.
///
/// The instance is flagged and unwound at its next yield point, so this
/// returns as soon as the flag is set rather than when the instance is
/// gone. A supervised kernel plugin is rebuilt by its supervisor
/// afterwards; an ordinary program is not.
pub fn kill(id: InstanceId) -> Result<(), KillError> {
    raw::kill(id)
}
