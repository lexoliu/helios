//! Async-first userland SDK for Helios wasm programs.
//!
//! Each capability area lives behind a Cargo feature so programs only
//! link the WIT interfaces they use: `fs`, `io`, `net`, `programs`,
//! `serial`, `stats`, `sync`, `task`, `tracing`, `profiling`,
//! `instances`, and `channel`. `bindings` exposes the raw generated WIT
//! bindings for anything the typed helpers do not cover, and
//! [`main`](macro@main) wraps a program's async entry point.

pub mod bindings;
#[cfg(feature = "channel")]
pub mod channel;
#[cfg(any(feature = "fs", feature = "io"))]
mod error;
#[cfg(feature = "fs")]
pub mod fs;
#[cfg(feature = "instances")]
pub mod instances;
#[cfg(feature = "io")]
pub mod io;
#[cfg(feature = "net")]
pub mod net;
pub mod prelude;
#[cfg(feature = "profiling")]
pub mod profiling;
#[cfg(feature = "programs")]
pub mod programs;
#[cfg(feature = "serial")]
pub mod serial;
#[cfg(feature = "stats")]
pub mod stats;
#[cfg(feature = "sync")]
pub mod sync;
#[cfg(feature = "task")]
pub mod task;
#[cfg(feature = "tracing")]
pub mod tracing;

#[cfg(any(feature = "fs", feature = "io"))]
pub use error::Result;
#[cfg(feature = "io")]
pub use futures_io::{AsyncRead, AsyncWrite};
pub use helios_api_macro::main;
#[cfg(feature = "io")]
pub use io::{ReadExt, WriteExt};
#[cfg(feature = "io")]
pub use std::io::Error;
use std::io::Write as _;
pub use wit_bindgen;

/// Return values accepted from a `#[helios_api::main]` entry point.
///
/// `()` always succeeds; `Result<(), E>` prints the error to stderr and
/// exits with failure. Implemented here rather than via `Termination`
/// because the wasm component entry point reports success as a plain
/// `Result<(), ()>` to the host.
#[allow(clippy::result_unit_err)]
pub trait MainOutput {
    fn into_run_result(self) -> core::result::Result<(), ()>;
}

impl MainOutput for () {
    fn into_run_result(self) -> core::result::Result<(), ()> {
        Ok(())
    }
}

impl<E> MainOutput for core::result::Result<(), E>
where
    E: core::fmt::Display,
{
    fn into_run_result(self) -> core::result::Result<(), ()> {
        match self {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = writeln!(std::io::stderr().lock(), "{error}");
                Err(())
            }
        }
    }
}
