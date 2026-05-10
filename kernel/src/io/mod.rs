//! Kernel-side IO primitives shared between the runtime and the
//! component host: byte channels for child stdio, the serial-port
//! transport used by the inspector debug RPC, the recording console
//! that captures host stderr, and the poll registry that wakes async
//! tasks on external events.

mod child;
mod console;
mod poll_registry;
mod serial;

pub use child::{ByteReadWait, ByteReader, ByteWriter, ClosedPeer, TryRead, byte_channel};
pub use console::RecordingConsole;
pub use poll_registry::{
    PollKey, PollRegistration, PollRegistry, PollRegistryError, PollSourceKind,
};
pub use serial::{
    SerialReader, emit_serial_error_marker, emit_serial_stage_marker, read_serial, try_read_serial,
    write_serial,
};
