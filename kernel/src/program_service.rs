extern crate alloc;

use alloc::string::String;

use thiserror::Error;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ProgramExecErrorKind {
    #[error("the wasm binary is invalid")]
    InvalidBinary,
    #[error("the wasm binary exports no supported entry point")]
    MissingEntry,
    #[error("the wasm binary imports unsupported host functions")]
    UnsupportedImport,
    #[error("the wasm artifact signature is invalid or untrusted")]
    InvalidSignature,
    #[error("the requested program path is invalid")]
    InvalidPath,
    #[error("the requested AOT hint is invalid for this input")]
    InvalidHint,
    #[error("the program exhausted its memory budget")]
    OutOfMemory,
    #[error("program exec is unavailable on this platform")]
    Unavailable,
    #[error("the kernel rejected the program for an internal reason")]
    Internal,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error(
    "program memory request of {requested_bytes} bytes exceeds its memory budget: available={available_bytes} reserved={reserved_bytes}"
)]
pub struct ProgramOutOfMemory {
    pub requested_bytes: usize,
    pub available_bytes: usize,
    pub reserved_bytes: usize,
}

#[derive(Debug, Error)]
#[error("{kind}: {detail}")]
pub struct ProgramExecError {
    pub kind: ProgramExecErrorKind,
    pub detail: String,
}
