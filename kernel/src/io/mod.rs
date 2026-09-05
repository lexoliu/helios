//! Kernel-side IO primitives shared between the runtime and the
//! component host: byte channels for child stdio, the serial-port
//! transport used by the inspector debug RPC, the recording console
//! that captures host stderr, and the poll registry that wakes async
//! tasks on external events.

mod block;
mod child;
mod console;
mod debug_serial;
mod interrupts;
mod iommu;
mod panic_console;
mod poll_registry;
mod serial;

pub use block::{
    BlockInstallError, BlockSelfCheckError, BlockService, BlockStats, SCRATCH_DISK_SERIAL,
    install_block_devices,
};
pub use child::{
    ByteReadWait, ByteReader, ByteWriteWait, ByteWriter, ClosedPeer, TryRead, TryWrite,
    byte_channel,
};
pub use console::RecordingConsole;
pub use debug_serial::{DebugConsole, DebugSerialAccess, DebugSerialWriter, read_debug_serial};
pub use interrupts::{
    ExternalInterruptHandler, ExternalInterruptRoutes, MAX_BLOCK_DEVICES, MAX_NETWORK_INTERRUPTS,
    wake_queue_owners,
};
pub use iommu::{IommuDomains, IommuEndpointStats, IommuReport, IommuStats, MAX_IOMMU_ENDPOINTS};
pub use panic_console::{PanicSerial, emit_panic_report};
pub use poll_registry::{
    PollKey, PollRegistration, PollRegistry, PollRegistryError, PollSourceKind,
};
pub use serial::{SerialReader, read_serial, try_read_serial};
