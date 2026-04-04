use alloc::vec::Vec;

use crate::io::IoResult;

/// Abstract duplex serial transport exposed by the platform.
pub trait SerialPort: Send + Sync {
    fn read(&self, max_bytes: usize) -> impl Future<Output = IoResult<Vec<u8>>> + Send;

    fn write(&self, bytes: &[u8]) -> impl Future<Output = IoResult<()>> + Send;

    fn flush(&self) -> impl Future<Output = IoResult<()>> + Send;
}
