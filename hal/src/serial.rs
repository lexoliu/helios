use alloc::vec::Vec;

use crate::io::IoResult;

/// Low-level byte-oriented serial transport.
///
/// This is the HAL contract for machine UART-style devices that the kernel may
/// use during bring-up, debugger transport, or other byte-stream control paths.
/// It intentionally stays synchronous and minimal because the implementation is
/// usually a direct MMIO/PIO hardware access path.
pub trait ByteSerial: Send + Sync {
    fn try_read_byte(&self) -> Option<u8>;

    fn write_bytes(&self, bytes: &[u8]);
}

/// Abstract duplex serial transport exposed by the platform.
pub trait SerialPort: Send + Sync {
    fn read(&self, max_bytes: usize) -> impl Future<Output = IoResult<Vec<u8>>> + Send;

    fn write(&self, bytes: &[u8]) -> impl Future<Output = IoResult<()>> + Send;

    fn flush(&self) -> impl Future<Output = IoResult<()>> + Send;
}
