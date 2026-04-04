//! Async serial transport helpers for debugger-style component protocols.

use std::io;
use std::vec::Vec;

pub use crate::bindings::helios::system::serial::SerialRights;
use crate::bindings::helios::system::serial::{self, SerialPort};

/// Kernel-provided debugger serial endpoint.
pub struct DebugPort {
    raw: SerialPort,
}

/// Returns the debugger serial endpoint when the current component holds the
/// corresponding capability.
pub fn debug_port() -> Option<DebugPort> {
    serial::debug_port().map(|raw| DebugPort { raw })
}

impl DebugPort {
    pub fn rights(&self) -> SerialRights {
        self.raw.rights()
    }

    pub async fn read(&self, max_bytes: usize) -> io::Result<Vec<u8>> {
        let max_bytes = u32::try_from(max_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "serial read is too large"))?;
        Ok(self.raw.read(max_bytes).await)
    }

    pub async fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        self.raw.write(bytes.to_vec()).await;
        Ok(())
    }

    pub async fn flush(&self) -> io::Result<()> {
        self.raw.flush().await;
        Ok(())
    }
}
