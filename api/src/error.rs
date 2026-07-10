//! `std::io::Error` constructors shared by the `fs` and `io` helpers.
//!
//! The SDK reports every fault as `std::io::Error` so programs interact
//! with one error vocabulary; these constructors attach the failing
//! path or stream and the WASI error code as the message.

use std::io;

/// Result alias used across the SDK's I/O surfaces.
pub type Result<T> = io::Result<T>;

pub fn missing_root_directory() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "helios root directory '/' is not mounted",
    )
}

pub fn parent_traversal(path: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("path {path:?} contains unsupported parent traversal"),
    )
}

pub fn non_utf8_path(path: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("path {path:?} contains a non-utf8 segment"),
    )
}

pub fn filesystem(
    path: &str,
    code: crate::bindings::wasi::filesystem::types::ErrorCode,
) -> io::Error {
    io::Error::other(format!("filesystem operation on {path:?} failed: {code:?}"))
}

#[cfg(feature = "io")]
pub fn stdout(code: crate::bindings::wasi::cli::types::ErrorCode) -> io::Error {
    io::Error::other(format!("stdout write failed: {code:?}"))
}

#[cfg(feature = "io")]
pub fn stderr(code: crate::bindings::wasi::cli::types::ErrorCode) -> io::Error {
    io::Error::other(format!("stderr write failed: {code:?}"))
}

#[cfg(feature = "io")]
pub fn stdin(code: crate::bindings::wasi::cli::types::ErrorCode) -> io::Error {
    io::Error::other(format!("stdin read failed: {code:?}"))
}

pub fn invalid_utf8(path: &str, error: std::string::FromUtf8Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("file {path:?} is not valid utf-8: {error}"),
    )
}

pub fn file_too_large(path: &str, size: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("file {path:?} size {size} does not fit into usize"),
    )
}
