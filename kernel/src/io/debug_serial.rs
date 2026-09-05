//! The debug UART, as every backend uses it.
//!
//! The component host takes the debugger's byte transport as two plain
//! function pointers: one that drains whatever the port has, one that
//! pushes bytes at it. Every backend used to write that pair itself,
//! around a single line of register access. What differs between
//! machines is only how the port is reached; the shape around it is
//! kernel logic and lives here.

extern crate alloc;

use alloc::vec::Vec;

use helios_hal::serial::ByteSerial;

use super::serial::try_read_serial;

/// A backend's access to the machine's debug UART.
///
/// A backend implements this on the port type itself and supplies
/// nothing but the way to reach it, which is why the trait carries no
/// receiver: the port is a machine-wide device, not per-processor
/// state, and the kernel asks for it from wherever it happens to run.
///
/// Reaching a port that the backend has not brought up yet is a
/// programming error, not a condition to report: the transport that
/// would carry the report *is* the port. An implementation panics.
///
/// Neither operation takes the console gate, and neither may start
/// taking it here. The gate spans one complete record — a tracing
/// event, a `[KDBG …]` marker, a panic report — and these bytes are a
/// fragment of a guest stdout write or of an inspector RPC frame, whose
/// length the guest chooses. Holding a critical section across one is
/// what #103 has to solve, and it needs an owner for the port rather
/// than a wider gate.
pub trait DebugSerialAccess {
    /// The port the backend reaches its debug UART through.
    type Port: ByteSerial;

    /// The machine's debug UART.
    fn port() -> Self::Port;
}

/// Drains what the debug UART has, up to `max_bytes`, into caller-owned
/// storage; leaves `buffer` empty when no byte is ready.
///
/// Monomorphised for one backend, this coerces to the [`SerialReader`]
/// the component host installs.
///
/// [`SerialReader`]: super::serial::SerialReader
pub fn read_debug_serial<Access: DebugSerialAccess>(buffer: &mut Vec<u8>, max_bytes: u32) {
    try_read_serial(&Access::port(), buffer, max_bytes);
}

/// Pushes `bytes` at the debug UART, spinning on the transmit FIFO for
/// as long as the hardware needs.
pub fn write_debug_serial_bytes<Access: DebugSerialAccess>(bytes: &[u8]) {
    Access::port().write_bytes(bytes);
}
