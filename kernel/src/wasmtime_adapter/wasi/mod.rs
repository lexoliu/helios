mod fs;
pub(crate) use fs::*;
mod net;
pub(crate) use net::*;
mod stream;
pub(crate) use stream::*;
mod host_env;
pub(crate) use host_env::*;

// The bindgen `with:` mappings re-export these resource types with `pub use`,
// which requires a fully `pub` path; the globs above are only crate-visible.
pub use fs::FsDescriptor;
pub use host_env::{TerminalInput, TerminalOutput};
pub use net::{
    P2IncomingDatagramStream, P2Network, P2OutgoingDatagramStream, P2ResolveAddressStream,
    TcpSocket, UdpSocket,
};

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::{
    AuthorityDomain, ComponentNetworkService, ComponentOutputMode, ComponentOutputRoute,
    ComponentOutputStreamKind, EmbeddedBootFs, HostFsErrorKind, ObjectIdentity,
};
use bytes::{Bytes, BytesMut};
use futures::channel::oneshot;
use hashbrown::HashMap;
use helios_hal::cpu::Cpu;
use helios_netstack::Ipv6Address;
use spin::Mutex;
use thiserror::Error;
use triomphe::Arc;
use wasmtime::component::{
    Access, Accessor, Component, Destination, FutureReader, HasSelf, Resource, Source,
    StreamConsumer, StreamProducer, StreamReader, StreamResult, WriteBuffer,
};
use wasmtime::{self, Result, StoreContextMut};

use crate::wasmtime_adapter::component_host::{
    ComponentHostNetworkService, HostRuntimeState, OutputStreamKind, StoreData,
};

pub(crate) type FsNodeKind = crate::ComponentFsNodeKind;
const FILE_READ_CHUNK_BYTES: usize = 1024 * 1024;
const DEFAULT_WASI_UDP_BUFFER_BYTES: u64 = 64 * 1024;
/// Receive window the netstack actually reserves per TCP socket.
///
/// `wasi:sockets` lets an implementation clamp a buffer-size hint and expects
/// the getter to report the effective value, so this is the ceiling for
/// `set-receive-buffer-size` rather than an arbitrary echo of the request.
const WASI_TCP_RECEIVE_BUFFER_BYTES: u64 = helios_netstack::TCP_RECEIVE_WINDOW_BYTES as u64;
/// Transmit buffer the netstack actually reserves per TCP socket.
const WASI_TCP_SEND_BUFFER_BYTES: u64 = helios_netstack::TCP_TRANSMIT_BUFFER_BYTES as u64;
const DEFAULT_WASI_UDP_HOP_LIMIT: u8 = helios_netstack::DEFAULT_HOP_LIMIT;
const DEFAULT_WASI_TCP_HOP_LIMIT: u8 = helios_netstack::DEFAULT_HOP_LIMIT;
const DEFAULT_WASI_TCP_LISTEN_BACKLOG: u16 = 128;
const DEFAULT_WASI_TCP_KEEP_ALIVE_IDLE_NANOS: u64 = 7_200_000_000_000;
const DEFAULT_WASI_TCP_KEEP_ALIVE_INTERVAL_NANOS: u64 = 75_000_000_000;
const DEFAULT_WASI_TCP_KEEP_ALIVE_COUNT: u32 = 9;
const MAX_WASI_UDP_DATAGRAM_BYTES: usize = u16::MAX as usize;
const MAX_SYMLINK_DEPTH: usize = 16;
/// 9p `QTDIR`: the qid type bit that marks a host-share object as a directory.
pub(crate) const P9_QID_TYPE_DIRECTORY: u8 = 0x80;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("guest requested wasi preview2 exit code {code}")]
pub(crate) struct Preview2GuestExit {
    code: u32,
}

impl Preview2GuestExit {
    pub(crate) const fn new(code: u32) -> Self {
        Self { code }
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("guest requested wasi preview3 exit code {code}")]
struct Preview3GuestExit {
    code: u32,
}

impl Preview3GuestExit {
    const fn new(code: u32) -> Self {
        Self { code }
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub(crate) enum WasiAdapterTrap {
    #[error("random byte length exceeds usize")]
    RandomLengthOverflow,
    #[error("file read offset overflowed u64")]
    FileReadOffsetOverflow,
    #[error("file write offset does not fit into usize")]
    FileWriteOffsetOverflow,
    #[error("udp send exceeded the permit returned by check-send")]
    UdpSendPermitExceeded,
}

pub(crate) struct WasiImportSet {
    names: Vec<String>,
}

impl WasiImportSet {
    pub(crate) fn from_component(engine: &wasmtime::Engine, component: &Component) -> Self {
        Self {
            names: component
                .component_type()
                .imports(engine)
                .map(|(name, _)| name.to_owned())
                .collect(),
        }
    }

    pub(crate) fn has(&self, interface: &str, version_prefix: &str) -> bool {
        self.names.iter().any(|name| {
            name.strip_prefix(interface)
                .and_then(|suffix| suffix.strip_prefix('@'))
                .is_some_and(|version| version.starts_with(version_prefix))
        })
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }
}

/// Whether the component's standard input is attached to the interactive
/// serial console.
///
/// The serial console is the only terminal Helios attaches a component to;
/// every other stdin route is a byte channel fed by a parent process, which is
/// a pipe rather than a tty. `wasi:cli/terminal-stdin` must only produce a
/// `terminal-input` resource for the console so guests do not misdetect a tty
/// on a captured stream.
///
/// Console-attached components read keystrokes through `helios:system/serial`
/// rather than the wasi stdin stream, which stays empty; the terminal
/// resource describes the attachment, not that stream.
pub(crate) fn stdin_is_terminal(mode: &ComponentOutputMode) -> bool {
    match mode {
        ComponentOutputMode::Serial => true,
        ComponentOutputMode::Trace
        | ComponentOutputMode::Child { .. }
        | ComponentOutputMode::RoutedChild { .. } => false,
    }
}

/// Whether the requested output stream is attached to the interactive serial
/// console.
///
/// `Trace` and `Discard` routes are capture sinks, and `Child` routes are
/// pipes to a parent process; only the serial console is a terminal.
pub(crate) fn output_is_terminal(
    mode: &ComponentOutputMode,
    kind: ComponentOutputStreamKind,
) -> bool {
    match mode {
        ComponentOutputMode::Serial => true,
        ComponentOutputMode::Trace | ComponentOutputMode::Child { .. } => false,
        ComponentOutputMode::RoutedChild { stdout, stderr, .. } => {
            let route = match kind {
                ComponentOutputStreamKind::Stdout => stdout,
                ComponentOutputStreamKind::Stderr => stderr,
            };
            match route {
                ComponentOutputRoute::Serial => true,
                ComponentOutputRoute::Trace
                | ComponentOutputRoute::Child(_)
                | ComponentOutputRoute::Discard => false,
            }
        }
    }
}

pub(crate) mod preview1;
pub(crate) mod preview2;
pub(crate) mod preview3;

pub(crate) mod bindings;

use bindings as wasi;
use wasi::cli::types as cli_types;
use wasi::filesystem::types as fs_types;
use wasi::sockets::ip_name_lookup;
use wasi::sockets::types as socket_types;

type TcpReadResult = core::result::Result<Option<Bytes>, socket_types::ErrorCode>;

type TcpWriteResult = core::result::Result<(), socket_types::ErrorCode>;

#[repr(transparent)]
pub struct TrappableError<T> {
    err: wasmtime::Error,
    _marker: PhantomData<T>,
}

pub type FsError = TrappableError<fs_types::ErrorCode>;

impl<T> TrappableError<T> {
    fn trap(err: impl Into<wasmtime::Error>) -> Self {
        Self {
            err: err.into(),
            _marker: PhantomData,
        }
    }

    fn downcast(self) -> Result<T>
    where
        T: core::error::Error + Send + Sync + 'static,
    {
        self.err.downcast()
    }
}

impl<T> From<T> for TrappableError<T>
where
    T: core::error::Error + Send + Sync + 'static,
{
    fn from(error: T) -> Self {
        Self::trap(error)
    }
}

impl<T> core::fmt::Debug for TrappableError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.err.fmt(f)
    }
}

impl<T> core::fmt::Display for TrappableError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.err.fmt(f)
    }
}

impl<T> core::error::Error for TrappableError<T> {}

#[cfg(test)]
mod tests {
    use crate::SocketReadiness;
    use alloc::boxed::Box;
    use alloc::collections::BTreeSet;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::{
        AuthorityDomain, ComponentHostFilesystemState, ComponentNetworkService,
        EmbeddedBootDirectory, EmbeddedBootFile, EmbeddedBootFs, ObjectIdentity,
        UnsupportedHostFileSystem,
    };
    use bytes::Bytes;
    use futures_lite::future::block_on;

    use super::{
        ComponentHostNetworkService, DEFAULT_WASI_TCP_HOP_LIMIT, DEFAULT_WASI_TCP_KEEP_ALIVE_COUNT,
        DEFAULT_WASI_TCP_KEEP_ALIVE_IDLE_NANOS, DEFAULT_WASI_TCP_KEEP_ALIVE_INTERVAL_NANOS,
        DEFAULT_WASI_TCP_LISTEN_BACKLOG, DebugFileSystem, FsDescriptor, FsNodeKind,
        P2ResolveAddressStream, TcpSocket, UdpSocket, WasiTcpIpAddress, WasiTcpSocketAddress,
        WasiTcpSocketFamily, WasiUdpSocketAddress, WasiUdpSocketError, WasiUdpSocketFamily,
        format_p3_tcp_socket_address, format_p3_udp_socket_address, fs_types,
        has_wasi_network_rights, ip_name_lookup, map_p3_dns_error, map_p3_tcp_error,
        map_p3_udp_family, map_p3_udp_socket_error, parse_p3_tcp_socket_address,
        parse_p3_udp_socket_address, preview3, socket_types, stdin_is_terminal,
        wasi_tcp_bind_rights, wasi_udp_bind_rights,
    };

    const fn tcp4(octets: [u8; 4], port: u16) -> WasiTcpSocketAddress {
        WasiTcpSocketAddress {
            address: WasiTcpIpAddress::Ipv4(crate::Ipv4Address::new(octets)),
            port,
        }
    }

    const fn udp4(octets: [u8; 4], port: u16) -> WasiUdpSocketAddress {
        WasiUdpSocketAddress {
            address: crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new(octets)),
            port,
        }
    }

    #[derive(Clone)]
    struct TestRuntimeState;

    impl ComponentHostFilesystemState<UnsupportedHostFileSystem> for TestRuntimeState {
        fn host_filesystem_service(&self) -> Option<UnsupportedHostFileSystem> {
            None
        }

        fn bootfs(&self) -> Option<EmbeddedBootFs> {
            None
        }
    }

    #[derive(Clone)]
    struct TestNetworkService;

    impl ComponentNetworkService for TestNetworkService {
        type TcpStream = u64;
        type TcpListener = u64;
        type UdpSocket = u64;

        // The in-memory doubles model an always-ready loopback peer.
        fn tcp_readiness(
            &self,
            _: Self::TcpStream,
        ) -> impl Future<Output = Result<SocketReadiness, crate::TcpError>> + Send + '_ {
            core::future::ready(Ok(SocketReadiness {
                readable: true,
                writable: true,
                hangup: false,
            }))
        }

        fn tcp_listener_readiness(
            &self,
            _: Self::TcpListener,
        ) -> impl Future<Output = Result<SocketReadiness, crate::TcpError>> + Send + '_ {
            core::future::ready(Ok(SocketReadiness {
                readable: true,
                writable: false,
                hangup: false,
            }))
        }

        fn udp_readiness(
            &self,
            _: Self::UdpSocket,
        ) -> impl Future<Output = Result<SocketReadiness, crate::UdpError>> + Send + '_ {
            core::future::ready(Ok(SocketReadiness {
                readable: true,
                writable: true,
                hangup: false,
            }))
        }

        fn hardware_address(&self) -> [u8; 6] {
            [2, 0, 0, 0, 0, 1]
        }

        fn ipv4_cidr(
            &self,
        ) -> impl core::future::Future<Output = Option<crate::Ipv4Cidr>> + Send + '_ {
            core::future::ready(Some(crate::Ipv4Cidr::new(
                crate::Ipv4Address::new([127, 0, 0, 1]),
                8,
            )))
        }

        fn ping(
            &self,
            _: &str,
            _: u64,
        ) -> impl core::future::Future<Output = Result<crate::PingReply, crate::PingError>> + Send + '_
        {
            core::future::ready(Ok(crate::PingReply {
                address: crate::Ipv4Address::new([127, 0, 0, 1]),
                round_trip_nanos: 1,
                payload_bytes: 1,
            }))
        }

        fn dns_resolve(
            &self,
            _: &str,
            _: u64,
        ) -> impl core::future::Future<
            Output = Result<Vec<crate::NetworkIpAddress>, crate::DnsError>,
        > + Send
        + '_ {
            core::future::ready(Ok(vec![crate::NetworkIpAddress::Ipv4(
                crate::Ipv4Address::new([127, 0, 0, 1]),
            )]))
        }

        fn tcp_connect(
            &self,
            _: &str,
            _: u16,
            _: u64,
        ) -> impl core::future::Future<Output = Result<Self::TcpStream, crate::TcpError>> + Send + '_
        {
            core::future::ready(Ok(7))
        }

        fn tcp_connect_from(
            &self,
            _: &str,
            _: u16,
            local_port: u16,
            _: u8,
            _: u64,
        ) -> impl core::future::Future<Output = Result<Self::TcpStream, crate::TcpError>> + Send + '_
        {
            core::future::ready(Ok(u64::from(local_port)))
        }

        fn tcp_connect_address(
            &self,
            _: crate::NetworkIpAddress,
            _: u16,
            local_port: u16,
            _: u8,
            _: u64,
        ) -> impl core::future::Future<Output = Result<Self::TcpStream, crate::TcpError>> + Send + '_
        {
            core::future::ready(Ok(if local_port == 0 {
                7
            } else {
                u64::from(local_port)
            }))
        }

        fn tcp_listen(
            &self,
            _: crate::NetworkIpAddress,
            local_port: u16,
            _: u16,
            _: u8,
        ) -> impl core::future::Future<
            Output = Result<crate::TcpListener<Self::TcpListener>, crate::TcpError>,
        > + Send
        + '_ {
            core::future::ready(Ok(crate::TcpListener {
                listener: 8,
                local_port,
            }))
        }

        fn tcp_set_hop_limit(&self, _: Self::TcpStream, _: u8) -> Result<(), crate::TcpError> {
            Ok(())
        }

        fn tcp_listener_set_hop_limit(
            &self,
            _: Self::TcpListener,
            _: u8,
        ) -> Result<(), crate::TcpError> {
            Ok(())
        }

        fn tcp_accept(
            &self,
            listener: Self::TcpListener,
            _: u64,
        ) -> impl core::future::Future<
            Output = Result<crate::TcpAccepted<Self::TcpStream>, crate::TcpError>,
        > + Send
        + '_ {
            core::future::ready(Ok(crate::TcpAccepted {
                stream: listener + 1,
                address: crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([127, 0, 0, 1])),
                port: 4040,
            }))
        }

        fn tcp_write_all(
            &self,
            _: Self::TcpStream,
            _: &[u8],
            _: u64,
        ) -> impl core::future::Future<Output = Result<(), crate::TcpError>> + Send + '_ {
            core::future::ready(Ok(()))
        }

        fn tcp_read(
            &self,
            _: Self::TcpStream,
            _: u32,
            _: u64,
        ) -> impl core::future::Future<Output = Result<Option<Bytes>, crate::TcpError>> + Send + '_
        {
            core::future::ready(Ok(Some(Bytes::from_static(&[4, 2]))))
        }

        fn tcp_shutdown_send(
            &self,
            _: Self::TcpStream,
        ) -> impl core::future::Future<Output = Result<(), crate::TcpError>> + Send + '_ {
            core::future::ready(Ok(()))
        }

        fn tcp_close(
            &self,
            _: Self::TcpStream,
        ) -> impl core::future::Future<Output = ()> + Send + '_ {
            core::future::ready(())
        }

        fn udp_bind(
            &self,
            local_port: u16,
        ) -> impl core::future::Future<
            Output = Result<crate::UdpBinding<Self::UdpSocket>, crate::UdpError>,
        > + Send
        + '_ {
            core::future::ready(Ok(crate::UdpBinding {
                socket: 9,
                local_port,
            }))
        }

        fn udp_connect(
            &self,
            _: Self::UdpSocket,
            _: crate::NetworkIpAddress,
            _: u16,
        ) -> Result<(), crate::UdpError> {
            Ok(())
        }

        fn udp_disconnect(&self, _: Self::UdpSocket) -> Result<(), crate::UdpError> {
            Ok(())
        }

        fn udp_set_hop_limit(&self, _: Self::UdpSocket, _: u8) -> Result<(), crate::UdpError> {
            Ok(())
        }

        fn udp_send(
            &self,
            _: Self::UdpSocket,
            _: &str,
            _: u16,
            bytes: &[u8],
            _: u64,
        ) -> impl core::future::Future<Output = Result<u64, crate::UdpError>> + Send + '_ {
            let _ = bytes;
            async { panic!("WASI UDP send must use typed address path") }
        }

        fn udp_send_address(
            &self,
            _: Self::UdpSocket,
            _: crate::NetworkIpAddress,
            _: u16,
            bytes: &[u8],
            _: u64,
        ) -> impl core::future::Future<Output = Result<u64, crate::UdpError>> + Send + '_ {
            core::future::ready(Ok(bytes.len() as u64))
        }

        fn udp_receive(
            &self,
            _: Self::UdpSocket,
            _: u32,
            _: u64,
        ) -> impl core::future::Future<
            Output = Result<Option<crate::UdpDatagram>, crate::UdpError>,
        > + Send
        + '_ {
            core::future::ready(Ok(None))
        }

        fn udp_join_multicast_v4(
            &self,
            _: crate::Ipv4Address,
            _: crate::Ipv4Address,
        ) -> impl core::future::Future<Output = Result<(), crate::UdpError>> + Send + '_ {
            core::future::ready(Ok(()))
        }

        fn udp_leave_multicast_v4(
            &self,
            _: crate::Ipv4Address,
            _: crate::Ipv4Address,
        ) -> impl core::future::Future<Output = Result<(), crate::UdpError>> + Send + '_ {
            core::future::ready(Ok(()))
        }

        fn udp_close(
            &self,
            _: Self::UdpSocket,
        ) -> impl core::future::Future<Output = ()> + Send + '_ {
            core::future::ready(())
        }
    }

    impl crate::NetworkAdminBackend for TestNetworkService {
        fn bridge_port(
            &self,
            _: crate::NetworkPortId,
            _: crate::NetworkBridgeRequest,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Err(crate::NetworkControlError::BridgeUnavailable))
        }

        fn unbridge_port(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Err(crate::NetworkControlError::BridgeUnavailable))
        }

        fn acquire_dhcp(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<Output = Result<crate::Ipv4Cidr, crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(crate::Ipv4Cidr::new(
                crate::Ipv4Address::new([127, 0, 0, 1]),
                8,
            )))
        }

        fn add_address(
            &self,
            _: crate::NetworkPortId,
            _: crate::Ipv4Cidr,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn remove_address(
            &self,
            _: crate::NetworkPortId,
            _: crate::Ipv4Cidr,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn clear_addresses(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn list_addresses(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<
            Output = Result<Vec<crate::Ipv4Cidr>, crate::NetworkControlError>,
        > + Send {
            core::future::ready(Ok(vec![crate::Ipv4Cidr::new(
                crate::Ipv4Address::new([127, 0, 0, 1]),
                8,
            )]))
        }

        fn mac_address(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<
            Output = Result<crate::MacAddress, crate::NetworkControlError>,
        > + Send {
            core::future::ready(Ok(crate::MacAddress::new([2, 0, 0, 0, 0, 1])))
        }

        fn set_gateway(
            &self,
            _: crate::NetworkPortId,
            _: crate::Ipv4Address,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn add_route(
            &self,
            _: crate::NetworkPortId,
            _: crate::Ipv4Route,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn remove_route(
            &self,
            _: crate::NetworkPortId,
            _: crate::Ipv4Route,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn clear_routes(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<Output = Result<(), crate::NetworkControlError>> + Send
        {
            core::future::ready(Ok(()))
        }

        fn list_routes(
            &self,
            _: crate::NetworkPortId,
        ) -> impl core::future::Future<
            Output = Result<Vec<crate::Ipv4Route>, crate::NetworkControlError>,
        > + Send {
            core::future::ready(Ok(vec![crate::Ipv4Route::new(
                crate::Ipv4Cidr::new(crate::Ipv4Address::new([0, 0, 0, 0]), 0),
                crate::Ipv4Address::new([127, 0, 0, 1]),
            )]))
        }
    }

    type TestFileSystem = DebugFileSystem<TestRuntimeState, UnsupportedHostFileSystem>;

    fn test_filesystem() -> TestFileSystem {
        DebugFileSystem::new(TestRuntimeState)
    }

    fn test_bootfs() -> EmbeddedBootFs {
        let directories = Box::leak(Box::new([EmbeddedBootDirectory::new("bin/empty", 43)]));
        let files = Box::leak(Box::new([EmbeddedBootFile::new("bin/tool", b"tool", 42)]));
        EmbeddedBootFs::new(directories, files)
    }

    fn readonly_root_descriptor() -> FsDescriptor {
        FsDescriptor {
            path: "/".into(),
            kind: FsNodeKind::Directory,
            flags: fs_types::DescriptorFlags::READ,
            identity: Some(ObjectIdentity::new(AuthorityDomain::GUEST_BOOTFS, 1)),
        }
    }

    #[test]
    fn preview3_linked_interfaces_exist_in_checked_in_wit() {
        let wit_interfaces = wit_interface_names(preview3::WIT_PACKAGES);
        for interface in preview3::LINKED_INTERFACES {
            assert!(
                wit_interfaces.contains(interface),
                "Preview3 adapter maps {interface}, but checked-in WIT does not declare it"
            );
        }
    }

    #[test]
    fn preview3_linked_interfaces_cover_required_subsystems() {
        assert_interface_set_eq(
            preview3::LINKED_INTERFACES,
            &[
                "wasi:clocks/monotonic-clock",
                "wasi:clocks/system-clock",
                "wasi:clocks/timezone",
                "wasi:cli/environment",
                "wasi:cli/exit",
                "wasi:cli/stdin",
                "wasi:cli/stdout",
                "wasi:cli/stderr",
                "wasi:cli/terminal-input",
                "wasi:cli/terminal-output",
                "wasi:cli/terminal-stdin",
                "wasi:cli/terminal-stdout",
                "wasi:cli/terminal-stderr",
                "wasi:random/random",
                "wasi:random/insecure",
                "wasi:random/insecure-seed",
                "wasi:filesystem/types",
                "wasi:filesystem/preopens",
                "wasi:sockets/types",
                "wasi:sockets/ip-name-lookup",
            ],
            "Preview3",
        );
    }

    #[test]
    fn only_serial_console_stdio_is_reported_as_a_terminal() {
        use super::{ComponentOutputMode, ComponentOutputRoute, output_is_terminal};
        use crate::ComponentOutputStreamKind::{Stderr, Stdout};

        assert!(stdin_is_terminal(&ComponentOutputMode::Serial));
        assert!(output_is_terminal(&ComponentOutputMode::Serial, Stdout));
        assert!(output_is_terminal(&ComponentOutputMode::Serial, Stderr));

        assert!(!stdin_is_terminal(&ComponentOutputMode::Trace));
        assert!(!output_is_terminal(&ComponentOutputMode::Trace, Stdout));
        assert!(!output_is_terminal(&ComponentOutputMode::Trace, Stderr));

        let (stdout_tx, _stdout_rx) = crate::byte_channel();
        let (stderr_tx, _stderr_rx) = crate::byte_channel();
        let (_stdin_tx, stdin_rx) = crate::byte_channel();
        let captured = ComponentOutputMode::Child {
            stdin_rx: stdin_rx.clone(),
            stdout_tx,
            stderr_tx,
        };
        assert!(!stdin_is_terminal(&captured));
        assert!(!output_is_terminal(&captured, Stdout));
        assert!(!output_is_terminal(&captured, Stderr));

        let (routed_stdout_tx, _routed_stdout_rx) = crate::byte_channel();
        let routed = ComponentOutputMode::RoutedChild {
            stdin_rx,
            stdout: ComponentOutputRoute::Child(routed_stdout_tx),
            stderr: ComponentOutputRoute::Serial,
        };
        assert!(!stdin_is_terminal(&routed));
        assert!(!output_is_terminal(&routed, Stdout));
        assert!(output_is_terminal(&routed, Stderr));
    }

    #[test]
    fn preview3_linked_functions_cover_required_subsystems() {
        assert_function_set_eq(
            &wit_function_names(preview3::WIT_PACKAGES, preview3::LINKED_INTERFACES),
            &[
                "wasi:cli/environment.get-arguments",
                "wasi:cli/environment.get-environment",
                "wasi:cli/environment.get-initial-cwd",
                "wasi:cli/exit.exit",
                "wasi:cli/exit.exit-with-code",
                "wasi:cli/stderr.write-via-stream",
                "wasi:cli/stdin.read-via-stream",
                "wasi:cli/stdout.write-via-stream",
                "wasi:cli/terminal-stderr.get-terminal-stderr",
                "wasi:cli/terminal-stdin.get-terminal-stdin",
                "wasi:cli/terminal-stdout.get-terminal-stdout",
                "wasi:clocks/monotonic-clock.get-resolution",
                "wasi:clocks/monotonic-clock.now",
                "wasi:clocks/monotonic-clock.wait-for",
                "wasi:clocks/monotonic-clock.wait-until",
                "wasi:clocks/system-clock.get-resolution",
                "wasi:clocks/system-clock.now",
                "wasi:clocks/timezone.iana-id",
                "wasi:clocks/timezone.to-debug-string",
                "wasi:clocks/timezone.utc-offset",
                "wasi:filesystem/preopens.get-directories",
                "wasi:filesystem/types.descriptor.advise",
                "wasi:filesystem/types.descriptor.append-via-stream",
                "wasi:filesystem/types.descriptor.create-directory-at",
                "wasi:filesystem/types.descriptor.get-flags",
                "wasi:filesystem/types.descriptor.get-type",
                "wasi:filesystem/types.descriptor.is-same-object",
                "wasi:filesystem/types.descriptor.link-at",
                "wasi:filesystem/types.descriptor.metadata-hash",
                "wasi:filesystem/types.descriptor.metadata-hash-at",
                "wasi:filesystem/types.descriptor.open-at",
                "wasi:filesystem/types.descriptor.read-directory",
                "wasi:filesystem/types.descriptor.read-via-stream",
                "wasi:filesystem/types.descriptor.readlink-at",
                "wasi:filesystem/types.descriptor.remove-directory-at",
                "wasi:filesystem/types.descriptor.rename-at",
                "wasi:filesystem/types.descriptor.set-size",
                "wasi:filesystem/types.descriptor.set-times",
                "wasi:filesystem/types.descriptor.set-times-at",
                "wasi:filesystem/types.descriptor.stat",
                "wasi:filesystem/types.descriptor.stat-at",
                "wasi:filesystem/types.descriptor.symlink-at",
                "wasi:filesystem/types.descriptor.sync",
                "wasi:filesystem/types.descriptor.sync-data",
                "wasi:filesystem/types.descriptor.unlink-file-at",
                "wasi:filesystem/types.descriptor.write-via-stream",
                "wasi:random/insecure-seed.get-insecure-seed",
                "wasi:random/insecure.get-insecure-random-bytes",
                "wasi:random/insecure.get-insecure-random-u64",
                "wasi:random/random.get-random-bytes",
                "wasi:random/random.get-random-u64",
                "wasi:sockets/ip-name-lookup.resolve-addresses",
                "wasi:sockets/types.tcp-socket.bind",
                "wasi:sockets/types.tcp-socket.connect",
                "wasi:sockets/types.tcp-socket.create",
                "wasi:sockets/types.tcp-socket.get-address-family",
                "wasi:sockets/types.tcp-socket.get-hop-limit",
                "wasi:sockets/types.tcp-socket.get-is-listening",
                "wasi:sockets/types.tcp-socket.get-keep-alive-count",
                "wasi:sockets/types.tcp-socket.get-keep-alive-enabled",
                "wasi:sockets/types.tcp-socket.get-keep-alive-idle-time",
                "wasi:sockets/types.tcp-socket.get-keep-alive-interval",
                "wasi:sockets/types.tcp-socket.get-local-address",
                "wasi:sockets/types.tcp-socket.get-receive-buffer-size",
                "wasi:sockets/types.tcp-socket.get-remote-address",
                "wasi:sockets/types.tcp-socket.get-send-buffer-size",
                "wasi:sockets/types.tcp-socket.listen",
                "wasi:sockets/types.tcp-socket.receive",
                "wasi:sockets/types.tcp-socket.send",
                "wasi:sockets/types.tcp-socket.set-hop-limit",
                "wasi:sockets/types.tcp-socket.set-keep-alive-count",
                "wasi:sockets/types.tcp-socket.set-keep-alive-enabled",
                "wasi:sockets/types.tcp-socket.set-keep-alive-idle-time",
                "wasi:sockets/types.tcp-socket.set-keep-alive-interval",
                "wasi:sockets/types.tcp-socket.set-listen-backlog-size",
                "wasi:sockets/types.tcp-socket.set-receive-buffer-size",
                "wasi:sockets/types.tcp-socket.set-send-buffer-size",
                "wasi:sockets/types.udp-socket.bind",
                "wasi:sockets/types.udp-socket.connect",
                "wasi:sockets/types.udp-socket.create",
                "wasi:sockets/types.udp-socket.disconnect",
                "wasi:sockets/types.udp-socket.get-address-family",
                "wasi:sockets/types.udp-socket.get-local-address",
                "wasi:sockets/types.udp-socket.get-receive-buffer-size",
                "wasi:sockets/types.udp-socket.get-remote-address",
                "wasi:sockets/types.udp-socket.get-send-buffer-size",
                "wasi:sockets/types.udp-socket.get-unicast-hop-limit",
                "wasi:sockets/types.udp-socket.receive",
                "wasi:sockets/types.udp-socket.send",
                "wasi:sockets/types.udp-socket.set-receive-buffer-size",
                "wasi:sockets/types.udp-socket.set-send-buffer-size",
                "wasi:sockets/types.udp-socket.set-unicast-hop-limit",
            ],
            "Preview3",
        );
    }

    #[test]
    fn preview2_linked_interfaces_exist_in_wasmtime_wit() {
        let wit_interfaces =
            wit_interface_names(crate::wasmtime_adapter::wasi::preview2::PREVIEW2_WIT_PACKAGES);
        for interface in crate::wasmtime_adapter::wasi::preview2::PREVIEW2_LINKED_INTERFACES {
            assert!(
                wit_interfaces.contains(interface),
                "Preview2 adapter maps {interface}, but Wasmtime WIT does not declare it"
            );
        }
    }

    #[test]
    fn preview2_linked_interfaces_cover_required_subsystems() {
        assert_interface_set_eq(
            crate::wasmtime_adapter::wasi::preview2::PREVIEW2_LINKED_INTERFACES,
            &[
                "wasi:cli/environment",
                "wasi:cli/exit",
                "wasi:cli/stdin",
                "wasi:cli/stdout",
                "wasi:cli/stderr",
                "wasi:cli/terminal-input",
                "wasi:cli/terminal-output",
                "wasi:cli/terminal-stdin",
                "wasi:cli/terminal-stdout",
                "wasi:cli/terminal-stderr",
                "wasi:clocks/monotonic-clock",
                "wasi:clocks/system-clock",
                "wasi:filesystem/preopens",
                "wasi:filesystem/types",
                "wasi:random/random",
                "wasi:random/insecure",
                "wasi:random/insecure-seed",
                "wasi:io/error",
                "wasi:io/poll",
                "wasi:io/streams",
                "wasi:sockets/network",
                "wasi:sockets/instance-network",
                "wasi:sockets/udp",
                "wasi:sockets/udp-create-socket",
                "wasi:sockets/tcp",
                "wasi:sockets/tcp-create-socket",
                "wasi:sockets/ip-name-lookup",
            ],
            "Preview2",
        );
    }

    #[test]
    fn preview2_linked_functions_cover_required_subsystems() {
        assert_function_set_eq(
            &wit_function_names(
                crate::wasmtime_adapter::wasi::preview2::PREVIEW2_WIT_PACKAGES,
                crate::wasmtime_adapter::wasi::preview2::PREVIEW2_LINKED_INTERFACES,
            ),
            &[
                "wasi:cli/environment.get-arguments",
                "wasi:cli/environment.get-environment",
                "wasi:cli/environment.initial-cwd",
                "wasi:cli/exit.exit",
                "wasi:cli/exit.exit-with-code",
                "wasi:cli/stderr.get-stderr",
                "wasi:cli/stdin.get-stdin",
                "wasi:cli/stdout.get-stdout",
                "wasi:cli/terminal-stderr.get-terminal-stderr",
                "wasi:cli/terminal-stdin.get-terminal-stdin",
                "wasi:cli/terminal-stdout.get-terminal-stdout",
                "wasi:clocks/monotonic-clock.now",
                "wasi:clocks/monotonic-clock.resolution",
                "wasi:clocks/monotonic-clock.subscribe-duration",
                "wasi:clocks/monotonic-clock.subscribe-instant",
                "wasi:clocks/system-clock.now",
                "wasi:clocks/system-clock.resolution",
                "wasi:filesystem/preopens.get-directories",
                "wasi:filesystem/types.descriptor.advise",
                "wasi:filesystem/types.descriptor.append-via-stream",
                "wasi:filesystem/types.descriptor.create-directory-at",
                "wasi:filesystem/types.descriptor.get-flags",
                "wasi:filesystem/types.descriptor.get-type",
                "wasi:filesystem/types.descriptor.is-same-object",
                "wasi:filesystem/types.descriptor.link-at",
                "wasi:filesystem/types.descriptor.metadata-hash",
                "wasi:filesystem/types.descriptor.metadata-hash-at",
                "wasi:filesystem/types.descriptor.open-at",
                "wasi:filesystem/types.descriptor.read",
                "wasi:filesystem/types.descriptor.read-directory",
                "wasi:filesystem/types.descriptor.read-via-stream",
                "wasi:filesystem/types.descriptor.readlink-at",
                "wasi:filesystem/types.descriptor.remove-directory-at",
                "wasi:filesystem/types.descriptor.rename-at",
                "wasi:filesystem/types.descriptor.set-size",
                "wasi:filesystem/types.descriptor.set-times",
                "wasi:filesystem/types.descriptor.set-times-at",
                "wasi:filesystem/types.descriptor.stat",
                "wasi:filesystem/types.descriptor.stat-at",
                "wasi:filesystem/types.descriptor.symlink-at",
                "wasi:filesystem/types.descriptor.sync",
                "wasi:filesystem/types.descriptor.sync-data",
                "wasi:filesystem/types.descriptor.unlink-file-at",
                "wasi:filesystem/types.descriptor.write",
                "wasi:filesystem/types.descriptor.write-via-stream",
                "wasi:filesystem/types.directory-entry-stream.read-directory-entry",
                "wasi:filesystem/types.filesystem-error-code",
                "wasi:io/error.error.to-debug-string",
                "wasi:io/poll.poll",
                "wasi:io/poll.pollable.block",
                "wasi:io/poll.pollable.ready",
                "wasi:io/streams.input-stream.blocking-read",
                "wasi:io/streams.input-stream.blocking-skip",
                "wasi:io/streams.input-stream.read",
                "wasi:io/streams.input-stream.skip",
                "wasi:io/streams.input-stream.subscribe",
                "wasi:io/streams.output-stream.blocking-flush",
                "wasi:io/streams.output-stream.blocking-splice",
                "wasi:io/streams.output-stream.blocking-write-and-flush",
                "wasi:io/streams.output-stream.blocking-write-zeroes-and-flush",
                "wasi:io/streams.output-stream.check-write",
                "wasi:io/streams.output-stream.flush",
                "wasi:io/streams.output-stream.splice",
                "wasi:io/streams.output-stream.subscribe",
                "wasi:io/streams.output-stream.write",
                "wasi:io/streams.output-stream.write-zeroes",
                "wasi:random/insecure-seed.insecure-seed",
                "wasi:random/insecure.get-insecure-random-bytes",
                "wasi:random/insecure.get-insecure-random-u64",
                "wasi:random/random.get-random-bytes",
                "wasi:random/random.get-random-u64",
                "wasi:sockets/instance-network.instance-network",
                "wasi:sockets/ip-name-lookup.resolve-address-stream.resolve-next-address",
                "wasi:sockets/ip-name-lookup.resolve-address-stream.subscribe",
                "wasi:sockets/ip-name-lookup.resolve-addresses",
                "wasi:sockets/network.network-error-code",
                "wasi:sockets/tcp-create-socket.create-tcp-socket",
                "wasi:sockets/tcp.tcp-socket.accept",
                "wasi:sockets/tcp.tcp-socket.address-family",
                "wasi:sockets/tcp.tcp-socket.finish-bind",
                "wasi:sockets/tcp.tcp-socket.finish-connect",
                "wasi:sockets/tcp.tcp-socket.finish-listen",
                "wasi:sockets/tcp.tcp-socket.hop-limit",
                "wasi:sockets/tcp.tcp-socket.is-listening",
                "wasi:sockets/tcp.tcp-socket.keep-alive-count",
                "wasi:sockets/tcp.tcp-socket.keep-alive-enabled",
                "wasi:sockets/tcp.tcp-socket.keep-alive-idle-time",
                "wasi:sockets/tcp.tcp-socket.keep-alive-interval",
                "wasi:sockets/tcp.tcp-socket.local-address",
                "wasi:sockets/tcp.tcp-socket.receive-buffer-size",
                "wasi:sockets/tcp.tcp-socket.remote-address",
                "wasi:sockets/tcp.tcp-socket.send-buffer-size",
                "wasi:sockets/tcp.tcp-socket.set-hop-limit",
                "wasi:sockets/tcp.tcp-socket.set-keep-alive-count",
                "wasi:sockets/tcp.tcp-socket.set-keep-alive-enabled",
                "wasi:sockets/tcp.tcp-socket.set-keep-alive-idle-time",
                "wasi:sockets/tcp.tcp-socket.set-keep-alive-interval",
                "wasi:sockets/tcp.tcp-socket.set-listen-backlog-size",
                "wasi:sockets/tcp.tcp-socket.set-receive-buffer-size",
                "wasi:sockets/tcp.tcp-socket.set-send-buffer-size",
                "wasi:sockets/tcp.tcp-socket.shutdown",
                "wasi:sockets/tcp.tcp-socket.start-bind",
                "wasi:sockets/tcp.tcp-socket.start-connect",
                "wasi:sockets/tcp.tcp-socket.start-listen",
                "wasi:sockets/tcp.tcp-socket.subscribe",
                "wasi:sockets/udp-create-socket.create-udp-socket",
                "wasi:sockets/udp.incoming-datagram-stream.receive",
                "wasi:sockets/udp.incoming-datagram-stream.subscribe",
                "wasi:sockets/udp.outgoing-datagram-stream.check-send",
                "wasi:sockets/udp.outgoing-datagram-stream.send",
                "wasi:sockets/udp.outgoing-datagram-stream.subscribe",
                "wasi:sockets/udp.udp-socket.address-family",
                "wasi:sockets/udp.udp-socket.finish-bind",
                "wasi:sockets/udp.udp-socket.local-address",
                "wasi:sockets/udp.udp-socket.receive-buffer-size",
                "wasi:sockets/udp.udp-socket.remote-address",
                "wasi:sockets/udp.udp-socket.send-buffer-size",
                "wasi:sockets/udp.udp-socket.set-receive-buffer-size",
                "wasi:sockets/udp.udp-socket.set-send-buffer-size",
                "wasi:sockets/udp.udp-socket.set-unicast-hop-limit",
                "wasi:sockets/udp.udp-socket.start-bind",
                "wasi:sockets/udp.udp-socket.stream",
                "wasi:sockets/udp.udp-socket.subscribe",
                "wasi:sockets/udp.udp-socket.unicast-hop-limit",
            ],
            "Preview2",
        );
    }

    fn assert_interface_set_eq(actual: &[&'static str], expected: &[&'static str], label: &str) {
        let actual = actual.iter().copied().collect::<BTreeSet<_>>();
        let expected = expected.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(
            actual, expected,
            "{label} linked interface coverage changed"
        );
    }

    fn assert_function_set_eq(actual: &BTreeSet<String>, expected: &[&'static str], label: &str) {
        let expected = expected
            .iter()
            .map(|function| String::from(*function))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            actual, &expected,
            "{label} linked function coverage changed"
        );
    }

    fn wit_interface_names(packages: &[(&'static str, &'static str)]) -> BTreeSet<&'static str> {
        packages
            .iter()
            .flat_map(|(package, wit)| {
                wit.lines().filter_map(move |line| {
                    let trimmed = line.trim_start();
                    let rest = trimmed.strip_prefix("interface ")?;
                    let name = rest
                        .split(|byte: char| byte.is_whitespace() || byte == '{')
                        .next()?;
                    wit_interface_name(package, name)
                })
            })
            .collect()
    }

    fn wit_function_names(
        packages: &[(&'static str, &'static str)],
        linked_interfaces: &[&'static str],
    ) -> BTreeSet<String> {
        let linked_interfaces = linked_interfaces.iter().copied().collect::<BTreeSet<_>>();
        let mut functions = BTreeSet::new();
        for (package, wit) in packages {
            let mut interface = None;
            let mut interface_depth = 0_i32;
            let mut resource = None;
            let mut resource_depth = 0_i32;

            for line in wit.lines() {
                let trimmed = line.trim_start();
                if interface.is_none() {
                    if let Some(rest) = trimmed.strip_prefix("interface ") {
                        let Some(name) = rest
                            .split(|byte: char| byte.is_whitespace() || byte == '{')
                            .next()
                        else {
                            continue;
                        };
                        let Some(mapped) = wit_interface_name(package, name) else {
                            continue;
                        };
                        if !linked_interfaces.contains(mapped) {
                            continue;
                        }
                        interface = Some(mapped);
                        interface_depth = wit_brace_delta(line);
                    }
                    continue;
                }

                if resource_depth > 0 {
                    if let Some(method) = wit_func_name(trimmed) {
                        let interface = interface.expect("resource methods require an interface");
                        let resource = resource.expect("resource method requires resource context");
                        functions.insert(wit_scoped_resource_function(interface, resource, method));
                    }
                    resource_depth += wit_brace_delta(line);
                    if resource_depth <= 0 {
                        resource = None;
                        resource_depth = 0;
                    }
                } else if let Some(rest) = trimmed.strip_prefix("resource ") {
                    let Some(name) = rest
                        .split(|byte: char| byte.is_whitespace() || byte == '{' || byte == ';')
                        .next()
                    else {
                        continue;
                    };
                    resource = Some(name);
                    resource_depth = wit_brace_delta(line);
                    if resource_depth <= 0 {
                        resource = None;
                        resource_depth = 0;
                    }
                } else if let Some(function) = wit_func_name(trimmed) {
                    let interface = interface.expect("functions require an interface");
                    functions.insert(wit_scoped_function(interface, function));
                }

                interface_depth += wit_brace_delta(line);
                if interface_depth <= 0 {
                    interface = None;
                    interface_depth = 0;
                    resource = None;
                    resource_depth = 0;
                }
            }
        }
        functions
    }

    fn wit_interface_name(package: &str, name: &str) -> Option<&'static str> {
        Some(match package {
            "wasi:cli" => match name {
                "environment" => "wasi:cli/environment",
                "exit" => "wasi:cli/exit",
                "run" => "wasi:cli/run",
                "types" => "wasi:cli/types",
                "stdin" => "wasi:cli/stdin",
                "stdout" => "wasi:cli/stdout",
                "stderr" => "wasi:cli/stderr",
                "terminal-input" => "wasi:cli/terminal-input",
                "terminal-output" => "wasi:cli/terminal-output",
                "terminal-stdin" => "wasi:cli/terminal-stdin",
                "terminal-stdout" => "wasi:cli/terminal-stdout",
                "terminal-stderr" => "wasi:cli/terminal-stderr",
                _ => return None,
            },
            "wasi:clocks" => match name {
                "types" => "wasi:clocks/types",
                "monotonic-clock" => "wasi:clocks/monotonic-clock",
                "system-clock" | "wall-clock" => "wasi:clocks/system-clock",
                "timezone" => "wasi:clocks/timezone",
                _ => return None,
            },
            "wasi:io" => match name {
                "error" => "wasi:io/error",
                "poll" => "wasi:io/poll",
                "streams" => "wasi:io/streams",
                _ => return None,
            },
            "wasi:filesystem" => match name {
                "types" => "wasi:filesystem/types",
                "preopens" => "wasi:filesystem/preopens",
                _ => return None,
            },
            "wasi:random" => match name {
                "random" => "wasi:random/random",
                "insecure" => "wasi:random/insecure",
                "insecure-seed" => "wasi:random/insecure-seed",
                _ => return None,
            },
            "wasi:sockets" => match name {
                "types" => "wasi:sockets/types",
                "network" => "wasi:sockets/network",
                "instance-network" => "wasi:sockets/instance-network",
                "udp" => "wasi:sockets/udp",
                "udp-create-socket" => "wasi:sockets/udp-create-socket",
                "tcp" => "wasi:sockets/tcp",
                "tcp-create-socket" => "wasi:sockets/tcp-create-socket",
                "ip-name-lookup" => "wasi:sockets/ip-name-lookup",
                _ => return None,
            },
            _ => return None,
        })
    }

    fn wit_func_name(line: &str) -> Option<&str> {
        if !line.contains("func(") && !line.contains("func()") {
            return None;
        }
        let (name, signature) = line.split_once(':')?;
        if !signature.contains("func") {
            return None;
        }
        name.strip_prefix('%').or(Some(name))
    }

    fn wit_scoped_function(interface: &str, function: &str) -> String {
        let mut scoped = String::from(interface);
        scoped.push('.');
        scoped.push_str(function);
        scoped
    }

    fn wit_scoped_resource_function(interface: &str, resource: &str, function: &str) -> String {
        let mut scoped = String::from(interface);
        scoped.push('.');
        scoped.push_str(resource);
        scoped.push('.');
        scoped.push_str(function);
        scoped
    }

    fn wit_brace_delta(line: &str) -> i32 {
        if line.trim_start().starts_with("///") {
            return 0;
        }
        let opens = line.bytes().filter(|byte| *byte == b'{').count() as i32;
        let closes = line.bytes().filter(|byte| *byte == b'}').count() as i32;
        opens - closes
    }

    #[test]
    fn create_directory_adds_node() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();

        filesystem
            .create_directory_at(&root, "tmp", 7)
            .expect("directory creation must succeed");

        let node = filesystem
            .get_node("/tmp")
            .expect("directory node must exist after creation");
        assert_eq!(node.kind, FsNodeKind::Directory);
        assert_eq!(node.modified_nanos, 7);
    }

    #[test]
    fn create_directory_rejects_existing_path() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();

        filesystem
            .create_directory_at(&root, "tmp", 1)
            .expect("initial directory creation must succeed");

        let error = filesystem
            .create_directory_at(&root, "tmp", 2)
            .expect_err("creating the same directory twice must fail");
        assert!(matches!(error, fs_types::ErrorCode::Exist));
    }

    #[test]
    fn bootfs_seed_adds_readonly_programs() {
        let mut filesystem = test_filesystem();
        filesystem.seed_bootfs(test_bootfs());

        let program = filesystem
            .get_node("/bin/tool")
            .expect("bootfs program must be present");
        assert_eq!(program.kind, FsNodeKind::File);
        assert!(program.readonly);
        assert_eq!(program.modified_nanos, 42);

        let directory = filesystem
            .get_node("/bin")
            .expect("bootfs program directory must be present");
        assert_eq!(directory.kind, FsNodeKind::Directory);
        assert!(directory.readonly);

        let empty = filesystem
            .get_node("/bin/empty")
            .expect("bootfs empty directory must be present");
        assert_eq!(empty.kind, FsNodeKind::Directory);
        assert!(empty.readonly);
        assert_eq!(empty.modified_nanos, 43);
    }

    #[test]
    fn readonly_bootfs_file_rejects_writes() {
        let mut filesystem = test_filesystem();
        filesystem.seed_bootfs(test_bootfs());
        let root = filesystem.root_descriptor();

        let descriptor = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "bin/tool",
                fs_types::OpenFlags::empty(),
                fs_types::DescriptorFlags::WRITE,
                0,
            )
            .expect("opening readonly bootfs file must succeed for lookup");

        let error = filesystem
            .write_at(&descriptor, 0, b"x", 1)
            .expect_err("readonly bootfs file must reject writes");
        assert!(matches!(error, fs_types::ErrorCode::ReadOnly));
    }

    #[test]
    fn open_at_rejects_descriptor_right_widening() {
        let mut filesystem = test_filesystem();
        filesystem.seed_bootfs(test_bootfs());
        let root = readonly_root_descriptor();

        let error = match filesystem.open_at(
            &root,
            fs_types::PathFlags::empty(),
            "bin/tool",
            fs_types::OpenFlags::empty(),
            fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::WRITE,
            0,
        ) {
            Ok(_) => panic!("open_at must not grant write through a read-only base descriptor"),
            Err(error) => error,
        };
        assert!(matches!(error, fs_types::ErrorCode::ReadOnly));
    }

    #[test]
    fn opening_existing_directory_without_directory_flag_fails() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();

        filesystem
            .create_directory_at(&root, "tmp", 1)
            .expect("directory creation must succeed");

        let error = match filesystem.open_at(
            &root,
            fs_types::PathFlags::empty(),
            "tmp",
            fs_types::OpenFlags::CREATE,
            fs_types::DescriptorFlags::WRITE,
            2,
        ) {
            Ok(_) => panic!("opening an existing directory as a file must fail"),
            Err(error) => error,
        };
        assert!(matches!(error, fs_types::ErrorCode::IsDirectory));
    }

    fn host_metadata(identity: ObjectIdentity, qid_type: u8) -> crate::HostMetadata {
        crate::HostMetadata {
            identity,
            qid_path: identity.local(),
            qid_type,
            mode: 0o100_644,
            size: 17,
            link_count: 2,
            access_nanos: 3_000_000_004,
            modified_nanos: 5_000_000_006,
            status_nanos: 7_000_000_008,
        }
    }

    /// Host-share stat must carry the host's real timestamps and link count.
    ///
    /// They used to be reported as absent and `1`, which makes `make`, `rsync`,
    /// and every other mtime-driven tool treat a shared file as ageless.
    #[test]
    fn host_stat_reports_host_timestamps_and_link_count() {
        let identity = ObjectIdentity::new(AuthorityDomain::HOST_SHARE_9P, 4242);

        let stat = super::descriptor_stat_from_host_metadata(&host_metadata(identity, 0));

        assert!(matches!(stat.type_, fs_types::DescriptorType::RegularFile));
        assert_eq!(stat.link_count, 2);
        assert_eq!(stat.size, 17);
        let access = stat
            .data_access_timestamp
            .expect("host stat must report an access timestamp");
        let modified = stat
            .data_modification_timestamp
            .expect("host stat must report a modification timestamp");
        let status = stat
            .status_change_timestamp
            .expect("host stat must report a status timestamp");
        assert_eq!((access.seconds, access.nanoseconds), (3, 4));
        assert_eq!((modified.seconds, modified.nanoseconds), (5, 6));
        assert_eq!((status.seconds, status.nanoseconds), (7, 8));
    }

    #[test]
    fn host_directory_metadata_maps_to_a_directory_descriptor_type() {
        let identity = ObjectIdentity::new(AuthorityDomain::HOST_SHARE_9P, 9);

        let stat = super::descriptor_stat_from_host_metadata(&host_metadata(
            identity,
            super::P9_QID_TYPE_DIRECTORY,
        ));

        assert!(matches!(stat.type_, fs_types::DescriptorType::Directory));
    }

    /// `st_dev`/`st_ino` are derived from object identity, so neither half may
    /// be zero for a host-share file: programs de-duplicate by that pair.
    #[test]
    fn host_file_identity_yields_nonzero_device_and_inode() {
        let identity = ObjectIdentity::new(AuthorityDomain::HOST_SHARE_9P, 4242);

        assert_ne!(identity.domain().raw(), 0);
        assert_ne!(identity.local(), 0);
        assert_eq!(identity.local(), 4242);
        assert_ne!(
            identity.domain().raw(),
            AuthorityDomain::GUEST_BOOTFS.raw(),
            "the host share must not share a device id with bootfs"
        );
    }

    /// Embedded nodes need the same guarantee: a distinct, nonzero inode per
    /// object within one device.
    #[test]
    fn embedded_nodes_report_nonzero_distinct_inodes() {
        let mut filesystem = test_filesystem();
        filesystem.seed_bootfs(test_bootfs());

        let root = filesystem
            .identity_at_path("/")
            .expect("root identity must resolve");
        let program = filesystem
            .identity_at_path("/bin/tool")
            .expect("bootfs program identity must resolve");

        assert_eq!(root.domain(), AuthorityDomain::GUEST_BOOTFS);
        assert_eq!(program.domain(), root.domain());
        assert_ne!(root.local(), 0);
        assert_ne!(program.local(), 0);
        assert_ne!(root.local(), program.local());
    }

    #[test]
    fn descriptors_carry_stable_object_identity() {
        let mut filesystem = test_filesystem();
        filesystem.seed_bootfs(test_bootfs());
        let root = filesystem.root_descriptor();

        let first = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "bin/tool",
                fs_types::OpenFlags::empty(),
                fs_types::DescriptorFlags::READ,
                0,
            )
            .expect("first open must succeed");
        let second = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "bin/tool",
                fs_types::OpenFlags::empty(),
                fs_types::DescriptorFlags::READ,
                0,
            )
            .expect("second open must succeed");

        assert_eq!(first.identity, second.identity);
    }

    #[test]
    fn rename_preserves_object_identity_local_part() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();
        let descriptor = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "tmp",
                fs_types::OpenFlags::CREATE,
                fs_types::DescriptorFlags::WRITE,
                1,
            )
            .expect("file creation must succeed");
        filesystem
            .write_at(&descriptor, 0, b"data", 1)
            .expect("file write must succeed");
        let before = filesystem
            .metadata_hash("/tmp")
            .expect("metadata hash before rename must succeed");

        filesystem
            .rename_at(&root, "tmp", &root, "renamed", 2)
            .expect("rename must succeed");
        let after = filesystem
            .metadata_hash("/renamed")
            .expect("metadata hash after rename must succeed");

        assert_eq!(before.lower, after.lower);
    }

    #[test]
    fn rename_rejects_cross_authority_domain() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();
        let other_domain = FsDescriptor {
            path: "/".into(),
            kind: FsNodeKind::Directory,
            flags: fs_types::DescriptorFlags::READ
                | fs_types::DescriptorFlags::WRITE
                | fs_types::DescriptorFlags::MUTATE_DIRECTORY,
            identity: Some(ObjectIdentity::new(AuthorityDomain::HOST_SHARE_9P, 1)),
        };
        let descriptor = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "tmp",
                fs_types::OpenFlags::CREATE,
                fs_types::DescriptorFlags::WRITE,
                1,
            )
            .expect("file creation must succeed");
        filesystem
            .write_at(&descriptor, 0, b"data", 1)
            .expect("file write must succeed");

        let error = filesystem
            .rename_at(&root, "tmp", &other_domain, "tmp", 2)
            .expect_err("rename across authority domains must fail");

        assert!(matches!(error, fs_types::ErrorCode::NotPermitted));
    }

    #[test]
    fn rename_rejects_unresolvable_authority_domain() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();
        let stale = FsDescriptor {
            path: "/stale".into(),
            kind: FsNodeKind::Directory,
            flags: fs_types::DescriptorFlags::READ
                | fs_types::DescriptorFlags::WRITE
                | fs_types::DescriptorFlags::MUTATE_DIRECTORY,
            identity: None,
        };
        let descriptor = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "tmp",
                fs_types::OpenFlags::CREATE,
                fs_types::DescriptorFlags::WRITE,
                1,
            )
            .expect("file creation must succeed");
        filesystem
            .write_at(&descriptor, 0, b"data", 1)
            .expect("file write must succeed");

        let error = filesystem
            .rename_at(&root, "tmp", &stale, "tmp", 2)
            .expect_err("rename through an unresolvable domain must fail");

        assert!(matches!(error, fs_types::ErrorCode::NoEntry));
    }

    #[test]
    fn hardlink_preserves_identity_and_shared_contents() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();
        let original = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "tmp",
                fs_types::OpenFlags::CREATE,
                fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::WRITE,
                1,
            )
            .expect("file creation must succeed");
        filesystem
            .write_at(&original, 0, b"data", 1)
            .expect("write must succeed");
        filesystem
            .link_at(&root, "tmp", &root, "alias", 2)
            .expect("hardlink must succeed");
        let alias = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "alias",
                fs_types::OpenFlags::empty(),
                fs_types::DescriptorFlags::READ,
                2,
            )
            .expect("alias open must succeed");

        assert_eq!(original.identity, alias.identity);
        assert_eq!(
            filesystem
                .stat("/tmp")
                .expect("stat must succeed")
                .link_count,
            2
        );

        filesystem
            .write_at(&original, 0, b"bolt", 3)
            .expect("linked write must succeed");
        assert_eq!(
            filesystem
                .read_file_chunk(&alias, 0, 4)
                .expect("alias read must succeed")
                .as_ref(),
            b"bolt"
        );
    }

    #[test]
    fn hardlink_resolves_preopen_identity_without_widening_rights() {
        let mut filesystem = test_filesystem();
        let mut root = filesystem.root_descriptor();
        root.identity = None;
        let original = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "tmp",
                fs_types::OpenFlags::CREATE,
                fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::WRITE,
                1,
            )
            .expect("file creation must succeed");
        filesystem
            .write_at(&original, 0, b"data", 1)
            .expect("write must succeed");

        filesystem
            .link_at(&root, "tmp", &root, "alias", 2)
            .expect("preopen-like root descriptor must resolve to its stable identity");
        let alias = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "alias",
                fs_types::OpenFlags::empty(),
                fs_types::DescriptorFlags::READ,
                2,
            )
            .expect("alias open must succeed");

        assert_eq!(original.identity, alias.identity);
        assert!(!alias.flags.contains(fs_types::DescriptorFlags::WRITE));
    }

    #[test]
    fn set_size_resizes_all_links_without_widening_descriptor_rights() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();
        let original = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "tmp",
                fs_types::OpenFlags::CREATE,
                fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::WRITE,
                1,
            )
            .expect("file creation must succeed");
        filesystem
            .write_at(&original, 0, b"data", 2)
            .expect("write must succeed");
        filesystem
            .link_at(&root, "tmp", &root, "alias", 3)
            .expect("hardlink must succeed");
        filesystem
            .set_size(&original, 6, 4)
            .expect("set-size must grow the file");

        let alias = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "alias",
                fs_types::OpenFlags::empty(),
                fs_types::DescriptorFlags::READ,
                4,
            )
            .expect("alias open must succeed");
        assert_eq!(
            filesystem
                .read_file_chunk(&alias, 0, 6)
                .expect("alias read must succeed")
                .as_ref(),
            b"data\0\0"
        );
        assert_eq!(filesystem.stat("/tmp").expect("stat must succeed").size, 6);

        let read_only = FsDescriptor {
            path: "/tmp".into(),
            kind: FsNodeKind::File,
            flags: fs_types::DescriptorFlags::READ,
            identity: original.identity,
        };
        let error = filesystem
            .set_size(&read_only, 1, 5)
            .expect_err("set-size through a read-only descriptor must fail");
        assert!(matches!(error, fs_types::ErrorCode::ReadOnly));
    }

    #[test]
    fn set_times_updates_stat_and_metadata_hash() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();
        let descriptor = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "tmp",
                fs_types::OpenFlags::CREATE,
                fs_types::DescriptorFlags::READ | fs_types::DescriptorFlags::WRITE,
                1,
            )
            .expect("file creation must succeed");
        let before = filesystem
            .metadata_hash("/tmp")
            .expect("metadata hash before set-times must succeed");

        filesystem
            .set_times(&descriptor, Some(11), Some(17), 19)
            .expect("set-times must succeed");
        let stat = filesystem.stat("/tmp").expect("stat must succeed");
        assert_eq!(
            stat.data_access_timestamp
                .expect("access timestamp must exist")
                .nanoseconds,
            11
        );
        assert_eq!(
            stat.data_modification_timestamp
                .expect("modified timestamp must exist")
                .nanoseconds,
            17
        );
        assert_eq!(
            stat.status_change_timestamp
                .expect("status timestamp must exist")
                .nanoseconds,
            19
        );
        let after = filesystem
            .metadata_hash("/tmp")
            .expect("metadata hash after set-times must succeed");
        assert_ne!(before.upper, after.upper);
    }

    #[test]
    fn hardlink_rejects_directory_and_cross_domain() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();
        filesystem
            .create_directory_at(&root, "dir", 1)
            .expect("directory creation must succeed");
        filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "tmp",
                fs_types::OpenFlags::CREATE,
                fs_types::DescriptorFlags::READ,
                1,
            )
            .expect("file creation must succeed");
        let other_domain = FsDescriptor {
            path: "/".into(),
            kind: FsNodeKind::Directory,
            flags: fs_types::DescriptorFlags::READ
                | fs_types::DescriptorFlags::WRITE
                | fs_types::DescriptorFlags::MUTATE_DIRECTORY,
            identity: Some(ObjectIdentity::new(AuthorityDomain::HOST_SHARE_9P, 1)),
        };

        let directory_error = filesystem
            .link_at(&root, "dir", &root, "dir-link", 1)
            .expect_err("hardlinking directories must fail");
        assert!(matches!(directory_error, fs_types::ErrorCode::NotPermitted));

        let file_error = filesystem
            .link_at(&root, "tmp", &other_domain, "tool", 1)
            .expect_err("hardlink across authority domains must fail");
        assert!(matches!(file_error, fs_types::ErrorCode::NotPermitted));
    }

    #[test]
    fn hardlink_rejects_unresolvable_authority_domain() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();
        let stale = FsDescriptor {
            path: "/stale".into(),
            kind: FsNodeKind::Directory,
            flags: fs_types::DescriptorFlags::READ
                | fs_types::DescriptorFlags::WRITE
                | fs_types::DescriptorFlags::MUTATE_DIRECTORY,
            identity: None,
        };
        filesystem
            .open_at(
                &root,
                fs_types::PathFlags::empty(),
                "tmp",
                fs_types::OpenFlags::CREATE,
                fs_types::DescriptorFlags::READ,
                1,
            )
            .expect("file creation must succeed");

        let error = filesystem
            .link_at(&root, "tmp", &stale, "alias", 1)
            .expect_err("hardlink through an unresolvable domain must fail");

        assert!(matches!(error, fs_types::ErrorCode::NoEntry));
    }

    #[test]
    fn symlink_payload_is_read_without_normalization() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();
        filesystem
            .symlink_at(&root, "tool-link", "bin/tool", 1)
            .expect("symlink creation must succeed");

        assert_eq!(
            filesystem
                .readlink_at(&root, "tool-link")
                .expect("readlink must succeed"),
            "bin/tool"
        );
        match filesystem
            .stat("/tool-link")
            .expect("symlink stat must succeed")
            .type_
        {
            fs_types::DescriptorType::SymbolicLink => {}
            _ => panic!("symlink stat must report symbolic-link type"),
        }
    }

    #[test]
    fn readlink_payload_validation_rejects_host_absolute_and_parent_escape() {
        assert!(matches!(
            super::resolve_symlink_payload("/host/link", "/Users/lexoliu/private"),
            Err(fs_types::ErrorCode::NotPermitted)
        ));
        assert!(matches!(
            super::resolve_symlink_payload("/host/link", "share/../../private"),
            Err(fs_types::ErrorCode::NotPermitted)
        ));
        assert_eq!(
            super::resolve_symlink_payload("/host/link", "share/target")
                .expect("confined relative payload should resolve"),
            "/host/share/target"
        );
    }

    #[test]
    fn symlink_rejects_rooted_and_parent_escape_payloads() {
        let mut filesystem = test_filesystem();
        let root = filesystem.root_descriptor();

        let absolute = filesystem
            .symlink_at(&root, "absolute", "/bin/tool", 1)
            .expect_err("absolute symlink payload must be rejected");
        assert!(matches!(absolute, fs_types::ErrorCode::NotPermitted));

        let parent = filesystem
            .symlink_at(&root, "escape", "../bin/tool", 1)
            .expect_err("parent escape symlink payload must be rejected");
        assert!(matches!(parent, fs_types::ErrorCode::NotPermitted));
    }

    #[test]
    fn symlink_follow_is_confined_and_loop_limited() {
        let mut filesystem = test_filesystem();
        filesystem.seed_bootfs(test_bootfs());
        let root = filesystem.root_descriptor();
        filesystem
            .symlink_at(&root, "tool-link", "bin/tool", 1)
            .expect("symlink creation must succeed");
        let opened = filesystem
            .open_at(
                &root,
                fs_types::PathFlags::SYMLINK_FOLLOW,
                "tool-link",
                fs_types::OpenFlags::empty(),
                fs_types::DescriptorFlags::READ,
                1,
            )
            .expect("symlink follow must open target");
        assert_eq!(
            filesystem
                .read_file_chunk(&opened, 0, 4)
                .expect("target read must succeed")
                .as_ref(),
            b"tool"
        );

        filesystem
            .symlink_at(&root, "loop-a", "loop-b", 1)
            .expect("loop symlink a must be created");
        filesystem
            .symlink_at(&root, "loop-b", "loop-a", 1)
            .expect("loop symlink b must be created");
        let error = match filesystem.open_at(
            &root,
            fs_types::PathFlags::SYMLINK_FOLLOW,
            "loop-a",
            fs_types::OpenFlags::empty(),
            fs_types::DescriptorFlags::READ,
            1,
        ) {
            Ok(_) => panic!("symlink loop must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, fs_types::ErrorCode::Loop));
    }

    #[test]
    fn wasi_udp_bind_rights_require_privileged_bind_for_low_ports() {
        assert_eq!(wasi_udp_bind_rights(0), crate::NetworkAuthorityRights::UDP);
        assert_eq!(
            wasi_udp_bind_rights(1024),
            crate::NetworkAuthorityRights::UDP
        );
        assert_eq!(
            wasi_udp_bind_rights(53),
            crate::NetworkAuthorityRights::UDP | crate::NetworkAuthorityRights::PRIVILEGED_BIND
        );
    }

    #[test]
    fn wasi_tcp_bind_rights_require_privileged_bind_for_low_ports() {
        assert_eq!(wasi_tcp_bind_rights(0), crate::NetworkAuthorityRights::TCP);
        assert_eq!(
            wasi_tcp_bind_rights(1024),
            crate::NetworkAuthorityRights::TCP
        );
        assert_eq!(
            wasi_tcp_bind_rights(443),
            crate::NetworkAuthorityRights::TCP | crate::NetworkAuthorityRights::PRIVILEGED_BIND
        );
    }

    #[test]
    fn wasi_network_right_check_never_widens_authority() {
        let mut authority = crate::ProcessAuthority::empty();
        authority.grant_network_rights(crate::NetworkAuthorityRights::UDP);

        assert!(has_wasi_network_rights(
            &authority,
            crate::NetworkAuthorityRights::UDP
        ));
        assert!(!has_wasi_network_rights(
            &authority,
            crate::NetworkAuthorityRights::UDP | crate::NetworkAuthorityRights::PRIVILEGED_BIND
        ));
    }

    #[test]
    fn p3_network_error_mapping_does_not_allocate_detail_strings() {
        assert!(matches!(
            map_p3_tcp_error(crate::TcpError {
                kind: crate::TcpErrorKind::Internal,
                detail: crate::NetworkErrorDetail::InternalInvariant,
            }),
            socket_types::ErrorCode::Other(None)
        ));
        assert!(matches!(
            map_p3_udp_socket_error(WasiUdpSocketError::Backend(crate::UdpError {
                kind: crate::UdpErrorKind::Internal,
                detail: crate::NetworkErrorDetail::InternalInvariant,
            })),
            socket_types::ErrorCode::Other(None)
        ));
        assert!(matches!(
            map_p3_dns_error(crate::DnsError {
                kind: crate::DnsErrorKind::Internal,
                detail: crate::NetworkErrorDetail::InternalInvariant,
            }),
            ip_name_lookup::ErrorCode::Other(None)
        ));
    }

    #[test]
    fn p3_tcp_ipv6_addresses_roundtrip_while_family_mismatches_fail() {
        assert_eq!(
            map_p3_udp_family(socket_types::IpAddressFamily::Ipv6)
                .expect("IPv6 UDP family must be supported"),
            WasiUdpSocketFamily::Ipv6
        );
        let ipv6_socket_address =
            socket_types::IpSocketAddress::Ipv6(socket_types::Ipv6SocketAddress {
                port: 80,
                flow_info: 0,
                address: (0x2001, 0x0db8, 0, 0, 0, 0, 0, 1),
                scope_id: 0,
            });
        assert!(matches!(
            parse_p3_tcp_socket_address(ipv6_socket_address, WasiTcpSocketFamily::Ipv4,),
            Err(socket_types::ErrorCode::NotSupported)
        ));
        let parsed = parse_p3_tcp_socket_address(ipv6_socket_address, WasiTcpSocketFamily::Ipv6)
            .expect("IPv6 TCP socket address must parse for IPv6 sockets");
        match format_p3_tcp_socket_address(parsed) {
            socket_types::IpSocketAddress::Ipv6(address) => {
                assert_eq!(address.port, 80);
                assert_eq!(address.address, (0x2001, 0x0db8, 0, 0, 0, 0, 0, 1));
            }
            socket_types::IpSocketAddress::Ipv4(_) => panic!("IPv6 TCP address formatted as IPv4"),
        }
        assert!(matches!(
            parse_p3_tcp_socket_address(
                socket_types::IpSocketAddress::Ipv4(socket_types::Ipv4SocketAddress {
                    port: 80,
                    address: (127, 0, 0, 1),
                }),
                WasiTcpSocketFamily::Ipv6,
            ),
            Err(socket_types::ErrorCode::NotSupported)
        ));
        let udp = parse_p3_udp_socket_address(ipv6_socket_address, WasiUdpSocketFamily::Ipv6)
            .expect("IPv6 UDP socket address must parse for IPv6 sockets");
        match format_p3_udp_socket_address(udp) {
            socket_types::IpSocketAddress::Ipv6(address) => {
                assert_eq!(address.port, 80);
                assert_eq!(address.address, (0x2001, 0x0db8, 0, 0, 0, 0, 0, 1));
            }
            socket_types::IpSocketAddress::Ipv4(_) => panic!("IPv6 UDP address formatted as IPv4"),
        }
        assert!(matches!(
            parse_p3_udp_socket_address(
                socket_types::IpSocketAddress::Ipv6(socket_types::Ipv6SocketAddress {
                    port: 53,
                    flow_info: 0,
                    address: (0, 0, 0, 0, 0, 0, 0, 1),
                    scope_id: 0,
                }),
                WasiUdpSocketFamily::Ipv4,
            ),
            Err(WasiUdpSocketError::NotSupported)
        ));
    }

    #[test]
    fn p2_resolve_stream_yields_both_address_families_in_resolver_order() {
        let mut stream = P2ResolveAddressStream::pending();
        let v4 = crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([93, 184, 215, 14]));
        let v6 = crate::NetworkIpAddress::Ipv6(helios_netstack::Ipv6Address::new([
            0x26, 0x06, 0x28, 0x00, 0x02, 0x1f, 0xcb, 0x07, 0x68, 0x20, 0x80, 0xda, 0x00, 0xaf,
            0x6b, 0x08,
        ]));
        stream.complete(Ok(vec![v4, v6]));

        assert_eq!(stream.next_address().unwrap(), Some(v4));
        assert_eq!(stream.next_address().unwrap(), Some(v6));
        assert_eq!(stream.next_address().unwrap(), None);
    }

    #[test]
    fn p3_ip_address_formatting_preserves_the_address_family() {
        use crate::wasmtime_adapter::wasi::net::format_p3_ip_address;

        assert!(matches!(
            format_p3_ip_address(crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([
                192, 0, 2, 1
            ]))),
            socket_types::IpAddress::Ipv4((192, 0, 2, 1))
        ));
        // Each 16-bit group is big-endian, per `wasi:sockets` ipv6-address.
        assert!(matches!(
            format_p3_ip_address(crate::NetworkIpAddress::Ipv6(
                helios_netstack::Ipv6Address::new([
                    0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
                ])
            )),
            socket_types::IpAddress::Ipv6((0x2001, 0x0db8, 0, 0, 0, 0, 0, 1))
        ));
    }

    #[test]
    fn p2_resolve_stream_yields_addresses_after_background_completion() {
        let mut stream = P2ResolveAddressStream::pending();
        assert!(!stream.is_ready());
        assert!(matches!(
            stream
                .next_address()
                .expect_err("pending stream has no address"),
            crate::DnsError {
                kind: crate::DnsErrorKind::Unavailable,
                ..
            }
        ));

        let address = crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([192, 0, 2, 1]));
        stream.complete(Ok(vec![address]));
        assert!(stream.is_ready());
        assert_eq!(stream.next_address().unwrap(), Some(address));
        assert_eq!(stream.next_address().unwrap(), None);
    }

    #[test]
    fn udp_socket_send_uses_typed_address_path() {
        let service = ComponentHostNetworkService::from_service(TestNetworkService);
        let socket = UdpSocket::new(service, WasiUdpSocketFamily::Ipv4);

        block_on(socket.send_datagram(b"hello", Some(udp4([192, 0, 2, 4], 53)), 0))
            .expect("UDP send should use typed backend address");
    }

    #[test]
    fn p3_tcp_socket_hop_limit_is_descriptor_local_state() {
        let service = ComponentHostNetworkService::from_service(TestNetworkService);
        let socket = TcpSocket::new(service, WasiTcpSocketFamily::Ipv4);

        assert_eq!(socket.hop_limit().unwrap(), DEFAULT_WASI_TCP_HOP_LIMIT);
        socket
            .set_hop_limit(127)
            .expect("nonzero hop limit must be accepted");
        assert_eq!(socket.hop_limit().unwrap(), 127);
        assert!(matches!(
            socket.set_hop_limit(0),
            Err(socket_types::ErrorCode::InvalidArgument)
        ));
        assert_eq!(socket.hop_limit().unwrap(), 127);
    }

    /// Enabling keepalive must fail rather than be recorded.
    ///
    /// The netstack has no keepalive timer, so a socket that accepted
    /// `set-keep-alive-enabled(true)` would leave a guest believing dead peers
    /// get detected. The timing knobs stay settable — `wasi:sockets` allows
    /// them to be configured while keepalive is off — and reading them back
    /// still reports what was stored.
    #[test]
    fn tcp_socket_rejects_enabling_unsupported_keep_alive() {
        let service = ComponentHostNetworkService::from_service(TestNetworkService);
        let socket = TcpSocket::new(service, WasiTcpSocketFamily::Ipv4);

        assert!(!socket.keep_alive_enabled().unwrap());
        assert_eq!(
            socket.keep_alive_idle_time().unwrap(),
            DEFAULT_WASI_TCP_KEEP_ALIVE_IDLE_NANOS
        );
        assert_eq!(
            socket.keep_alive_interval().unwrap(),
            DEFAULT_WASI_TCP_KEEP_ALIVE_INTERVAL_NANOS
        );
        assert_eq!(
            socket.keep_alive_count().unwrap(),
            DEFAULT_WASI_TCP_KEEP_ALIVE_COUNT
        );

        assert!(matches!(
            socket.set_keep_alive_enabled(true),
            Err(socket_types::ErrorCode::NotSupported)
        ));
        assert!(!socket.keep_alive_enabled().unwrap());
        socket
            .set_keep_alive_enabled(false)
            .expect("disabling keepalive matches what the stack already does");
        socket
            .set_keep_alive_idle_time(11)
            .expect("nonzero idle time must be accepted");
        socket
            .set_keep_alive_interval(13)
            .expect("nonzero interval must be accepted");
        socket
            .set_keep_alive_count(3)
            .expect("nonzero count must be accepted");

        assert!(!socket.keep_alive_enabled().unwrap());
        assert_eq!(socket.keep_alive_idle_time().unwrap(), 11);
        assert_eq!(socket.keep_alive_interval().unwrap(), 13);
        assert_eq!(socket.keep_alive_count().unwrap(), 3);
        assert!(matches!(
            socket.set_keep_alive_idle_time(0),
            Err(socket_types::ErrorCode::InvalidArgument)
        ));
        assert!(matches!(
            socket.set_keep_alive_interval(0),
            Err(socket_types::ErrorCode::InvalidArgument)
        ));
        assert!(matches!(
            socket.set_keep_alive_count(0),
            Err(socket_types::ErrorCode::InvalidArgument)
        ));
        assert_eq!(socket.keep_alive_idle_time().unwrap(), 11);
        assert_eq!(socket.keep_alive_interval().unwrap(), 13);
        assert_eq!(socket.keep_alive_count().unwrap(), 3);
    }

    #[test]
    fn tcp_socket_shutdown_directions_are_idempotent_local_state() {
        let service = ComponentHostNetworkService::from_service(TestNetworkService);
        let socket = TcpSocket::accepted(
            service,
            WasiTcpSocketFamily::Ipv4,
            7,
            tcp4([127, 0, 0, 1], 8080),
            tcp4([127, 0, 0, 1], 4040),
        );

        socket
            .shutdown_receive()
            .expect("connected receive shutdown must be accepted");
        socket
            .shutdown_receive()
            .expect("receive shutdown must be idempotent");
        assert_eq!(
            block_on(socket.read(8)).expect("receive shutdown read must succeed"),
            None
        );

        let (_, stream) = socket
            .shutdown_send_state()
            .expect("connected send shutdown must be accepted");
        assert_eq!(stream, 7);
        let (_, stream) = socket
            .shutdown_send_state()
            .expect("send shutdown must be idempotent");
        assert_eq!(stream, 7);
        assert!(matches!(
            block_on(socket.write_all_bytes(Bytes::from_static(b"x"))),
            Err(socket_types::ErrorCode::InvalidState)
        ));
    }

    #[test]
    fn tcp_socket_bind_local_tracks_authorized_local_address() {
        let service = ComponentHostNetworkService::from_service(TestNetworkService);
        let socket = TcpSocket::new(service, WasiTcpSocketFamily::Ipv4);
        let local = tcp4([127, 0, 0, 1], 8080);

        socket
            .bind_local(local)
            .expect("unconnected socket must accept a local bind");
        assert_eq!(socket.local_address().unwrap(), local);
        assert!(!socket.is_listening());
    }

    #[test]
    fn tcp_socket_listen_backlog_is_descriptor_local_state() {
        let service = ComponentHostNetworkService::from_service(TestNetworkService);
        let socket = TcpSocket::new(service, WasiTcpSocketFamily::Ipv4);

        assert_eq!(socket.listen_backlog(), DEFAULT_WASI_TCP_LISTEN_BACKLOG);
        socket
            .set_listen_backlog_size(64)
            .expect("positive u16 backlog must be accepted");
        assert_eq!(socket.listen_backlog(), 64);
        assert!(matches!(
            socket.set_listen_backlog_size(0),
            Err(socket_types::ErrorCode::InvalidArgument)
        ));
        assert!(matches!(
            socket.set_listen_backlog_size(u64::from(u16::MAX) + 1),
            Err(socket_types::ErrorCode::InvalidArgument)
        ));
        assert_eq!(socket.listen_backlog(), 64);
    }

    #[test]
    fn p3_tcp_socket_uses_network_backend_without_widening_rights() {
        let service = ComponentHostNetworkService::from_service(TestNetworkService);
        let socket = TcpSocket::new(service, WasiTcpSocketFamily::Ipv4);
        let remote = tcp4([203, 0, 113, 10], 443);

        block_on(socket.connect(remote)).expect("test TCP backend should connect");
        assert_eq!(socket.remote_address().unwrap(), remote);
        block_on(socket.write_all_bytes(Bytes::from_static(b"hello")))
            .expect("test TCP backend should write");
        let bytes = block_on(socket.read(16))
            .unwrap()
            .expect("test TCP backend should read bytes");
        assert_eq!(bytes.as_ref(), [4, 2]);
    }
}
