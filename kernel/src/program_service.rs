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
    #[error("program exec is unavailable on this platform")]
    Unavailable,
    #[error("the kernel rejected the program for an internal reason")]
    Internal,
}

#[derive(Debug, Error)]
#[error("{kind}: {detail}")]
pub struct ProgramExecError {
    pub kind: ProgramExecErrorKind,
    pub detail: String,
}
