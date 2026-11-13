//! Kernel runtime state and the type aliases shared with the
//! component host.
//!
//! `state` carries the per-instance runtime state used by the
//! component runtime backend. `types` declares the kernel-facing
//! aliases for component-host service interfaces (network, host
//! filesystem, etc.) that backends and tests share.

mod state;
mod types;

pub use state::RuntimeState;
pub use types::{
    AuthorityDomain, ComponentHostNetwork, ComponentNetworkService, ComponentNetworkState,
    DnsError, DnsErrorKind, ExecOutput, ExecResult, HostDirEntry, HostDirLimit, HostFileSystem,
    HostFilledWrite, HostFsError, HostFsErrorKind, HostMetadata, HostShareRestore, HostWrite,
    Ipv4Address, NetworkErrorDetail, NetworkHandle, NetworkIpAddress, NetworkPacketPump,
    ObjectIdentity, PingError, PingErrorKind, PingReply, RegisteredTcpReadBuffer, SocketReadiness,
    TcpAccepted, TcpError, TcpErrorKind, TcpListener, TcpReadInto, TcpReadProgress,
    TcpWriteProgress, UdpBinding, UdpDatagram, UdpError, UdpErrorKind,
};
