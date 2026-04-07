//! Async-first userland helpers for Helios components.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunError;

pub trait MainOutput {
    fn into_run_result(self) -> core::result::Result<(), RunError>;
}

impl MainOutput for () {
    fn into_run_result(self) -> core::result::Result<(), RunError> {
        Ok(())
    }
}

impl<E> MainOutput for core::result::Result<(), E>
where
    E: core::fmt::Display,
{
    fn into_run_result(self) -> core::result::Result<(), RunError> {
        match self {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = writeln!(std::io::stderr().lock(), "{error}");
                Err(RunError)
            }
        }
    }
}
