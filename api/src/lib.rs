//! Async-first userland helpers for Helios components.

pub mod bindings;
pub mod channel;
mod error;
pub mod fs;
pub mod io;
pub mod prelude;
pub mod serial;
pub mod stats;
pub mod sync;
pub mod task;
pub mod tracing;

pub use error::Result;
pub use futures_io::{AsyncRead, AsyncWrite};
pub use helios_api_macro::main;
pub use io::{ReadExt, WriteExt};
pub use std::io::Error;
pub use wit_bindgen;

pub trait MainOutput {
    fn into_run_result(self) -> core::result::Result<(), ()>;
}

impl MainOutput for () {
    fn into_run_result(self) -> core::result::Result<(), ()> {
        Ok(())
    }
}

impl<E> MainOutput for core::result::Result<(), E> {
    fn into_run_result(self) -> core::result::Result<(), ()> {
        self.map_err(|_| ())
    }
}
