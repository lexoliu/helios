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
    AuthorityDomain, ComponentHostFilesystemState, ComponentNetworkService, ComponentNetworkState,
    DnsError, DnsErrorKind, ExecOutput, ExecResult, HostDirEntry, HostFileSystem, HostFsError,
    HostFsErrorKind, HostMetadata, Ipv4Address, NetworkErrorDetail, NetworkIpAddress,
    ObjectIdentity, PingError, PingErrorKind, PingReply, RegisteredTcpReadBuffer, TcpAccepted,
    TcpError, TcpErrorKind, TcpListener, UdpBinding, UdpDatagram, UdpError, UdpErrorKind,
};
