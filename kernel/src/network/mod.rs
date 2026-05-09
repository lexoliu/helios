//! In-kernel network service.
//!
//! `service` hosts the `NetworkService` that wraps `helios-netstack`
//! for component-host TCP/UDP/DNS access. `control` exposes the
//! capability-checked admin API used by privileged components.
//! `socket_stack` provides per-task socket lifecycle bookkeeping.

mod control;
mod service;
mod socket_stack;

pub use control::{
    Ipv4Cidr, Ipv4Route, MacAddress, NetworkAdminBackend, NetworkBridgeRequest,
    NetworkBridgeSecurity, NetworkControl, NetworkControlError, NetworkPortId,
};
pub use service::{NetworkService, TcpListenerId, TcpStreamId, UdpSocketId};
pub use socket_stack::SocketStack;
