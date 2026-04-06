//! User-program exec syscalls.

use crate::bindings::helios::system::programs as raw;
pub use crate::bindings::helios::system::programs::{
    ExecError, ExecErrorKind, ExecRequest, ExecResult,
};

/// Launches a pure wasm program with the kernel's default minimal rights.
pub fn exec(request: &ExecRequest) -> Result<ExecResult, ExecError> {
    raw::exec(request)
}
