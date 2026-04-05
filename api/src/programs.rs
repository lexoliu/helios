//! User-program launch syscalls.

use crate::bindings::helios::system::programs as raw;
pub use crate::bindings::helios::system::programs::{
    InstanceId, LaunchError, LaunchErrorKind, LaunchRequest,
};

/// Launches a pure wasm program with the kernel's default minimal rights.
pub fn launch(request: &LaunchRequest) -> Result<InstanceId, LaunchError> {
    raw::launch(request)
}
