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

    /// Lets the device raise its receive interrupt again.
    ///
    /// A consumer calls this as it arms its wait, before the drain that
    /// decides whether to park: a byte that is already waiting, or that
    /// lands afterwards, raises the line. Idempotent.
    fn enable_receive_interrupt(&self);

    /// Stops the device raising its receive interrupt.
    ///
    /// The interrupt handler calls this before it returns, so a
    /// level-triggered line whose bytes are drained later by a task does
    /// not re-raise on every return from the handler. The bytes stay in
    /// the device until [`Self::try_read_byte`] takes them; only the
    /// line is quiet. Idempotent.
    fn disable_receive_interrupt(&self);
}

/// Abstract duplex serial transport exposed by the platform.
pub trait SerialPort: Send + Sync {
    fn read(&self, max_bytes: usize) -> impl Future<Output = IoResult<Vec<u8>>> + Send;

    fn write(&self, bytes: &[u8]) -> impl Future<Output = IoResult<()>> + Send;

    fn flush(&self) -> impl Future<Output = IoResult<()>> + Send;
}
