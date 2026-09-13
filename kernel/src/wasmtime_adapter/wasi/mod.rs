mod fs;
pub(crate) use fs::*;
pub(in crate::wasmtime_adapter) mod net;
pub(crate) use net::*;
mod stream;
pub(crate) use stream::*;
mod host_env;
pub(crate) use host_env::*;
pub(crate) mod http;
pub(crate) use http::*;

// The bindgen `with:` mappings re-export these resource types with `pub use`,
// which requires a fully `pub` path; the globs above are only crate-visible.
pub use fs::FsDescriptor;
pub use host_env::{TerminalInput, TerminalOutput};
pub use http::{WasiRequest, WasiResponse};
pub use net::{
    P2IncomingDatagramStream, P2Network, P2OutgoingDatagramStream, P2ResolveAddressStream,
    TcpSocket, UdpSocket,
};

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::{
    AuthorityDomain, ComponentOutputMode, ComponentOutputRoute, ComponentOutputStreamKind,
    EmbeddedBootFs, HostFsErrorKind, ObjectIdentity,
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
    Access, Accessor, Component, Destination, FutureConsumer, FutureProducer, FutureReader,
    HasSelf, Resource, Source, StreamConsumer, StreamProducer, StreamReader, StreamResult,
    WriteBuffer,
};
use wasmtime::{self, Result, StoreContextMut};

use crate::wasmtime_adapter::component_host::{HostRuntimeState, OutputStreamKind, StoreData};

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
pub(crate) mod http_bindings;

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
    use crate::test_support::{TestNetworkService, TestSocketRetirement};
    use alloc::boxed::Box;
    use alloc::collections::BTreeSet;
    use alloc::string::String;
    use alloc::vec;

    use crate::{
        AuthorityDomain, ComponentHostFilesystemState, EmbeddedBootDirectory, EmbeddedBootFile,
        EmbeddedBootFs, ObjectIdentity, UnsupportedHostFileSystem,
    };
    use bytes::Bytes;
    use futures_lite::future::block_on;

    use super::{
        DEFAULT_WASI_TCP_HOP_LIMIT, DEFAULT_WASI_TCP_KEEP_ALIVE_COUNT,
        DEFAULT_WASI_TCP_KEEP_ALIVE_IDLE_NANOS, DEFAULT_WASI_TCP_KEEP_ALIVE_INTERVAL_NANOS,
        DEFAULT_WASI_TCP_LISTEN_BACKLOG, DebugFileSystem, FsDescriptor, FsNodeKind,
        P2ResolveAddressStream, TcpSocket, UdpSocket, WasiTcpIpAddress, WasiTcpSocketAddress,
        WasiTcpSocketFamily, WasiUdpSocketAddress, WasiUdpSocketError, WasiUdpSocketFamily,
        format_p3_tcp_socket_address, format_p3_udp_socket_address, fs_types,
        has_wasi_network_rights, http, ip_name_lookup, map_p3_dns_error, map_p3_tcp_error,
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
    fn http_linked_interfaces_exist_in_checked_in_wit() {
        let wit_interfaces = wit_interface_names(preview3::WIT_PACKAGES);
        for interface in http::LINKED_INTERFACES {
            assert!(
                wit_interfaces.contains(interface),
                "http adapter maps {interface}, but checked-in WIT does not declare it"
            );
        }
    }

    #[test]
    fn http_linked_interfaces_cover_the_client_surface() {
        // `handler` is deliberately absent: the kernel calls it as an export
        // on the plugin store, it never serves it as an import.
        assert_interface_set_eq(
            http::LINKED_INTERFACES,
            &["wasi:http/types", "wasi:http/client"],
            "http",
        );
    }

    #[test]
    fn http_linked_functions_cover_the_whole_types_interface() {
        assert_function_set_eq(
            &wit_function_names(preview3::WIT_PACKAGES, http::LINKED_INTERFACES),
            &HTTP_EXPECTED_FUNCTIONS,
            "http",
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

    /// Every function the kernel must implement to serve `wasi:http` to a
    /// guest. Derived from the checked-in WIT, so adding a method upstream
    /// fails this test rather than silently going unimplemented.
    const HTTP_EXPECTED_FUNCTIONS: [&str; 34] = [
        "wasi:http/client.send",
        "wasi:http/types.fields.append",
        "wasi:http/types.fields.clone",
        "wasi:http/types.fields.copy-all",
        "wasi:http/types.fields.delete",
        "wasi:http/types.fields.from-list",
        "wasi:http/types.fields.get",
        "wasi:http/types.fields.get-and-delete",
        "wasi:http/types.fields.has",
        "wasi:http/types.fields.set",
        "wasi:http/types.request.consume-body",
        "wasi:http/types.request.get-authority",
        "wasi:http/types.request.get-headers",
        "wasi:http/types.request.get-method",
        "wasi:http/types.request.get-options",
        "wasi:http/types.request.get-path-with-query",
        "wasi:http/types.request.get-scheme",
        "wasi:http/types.request.new",
        "wasi:http/types.request.set-authority",
        "wasi:http/types.request.set-method",
        "wasi:http/types.request.set-path-with-query",
        "wasi:http/types.request.set-scheme",
        "wasi:http/types.request-options.clone",
        "wasi:http/types.request-options.get-between-bytes-timeout",
        "wasi:http/types.request-options.get-connect-timeout",
        "wasi:http/types.request-options.get-first-byte-timeout",
        "wasi:http/types.request-options.set-between-bytes-timeout",
        "wasi:http/types.request-options.set-connect-timeout",
        "wasi:http/types.request-options.set-first-byte-timeout",
        "wasi:http/types.response.consume-body",
        "wasi:http/types.response.get-headers",
        "wasi:http/types.response.get-status-code",
        "wasi:http/types.response.new",
        "wasi:http/types.response.set-status-code",
    ];

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
            "wasi:http" => match name {
                "types" => "wasi:http/types",
                "client" => "wasi:http/client",
                "handler" => "wasi:http/handler",
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
        let service = TestNetworkService::new();
        let retirement = TestSocketRetirement::new(service.clone());
        let socket = UdpSocket::new(retirement.sender(), WasiUdpSocketFamily::Ipv4);

        block_on(socket.send_datagram(&service, b"hello", Some(udp4([192, 0, 2, 4], 53)), 0))
            .expect("UDP send should use typed backend address");
    }

    /// A `wasi:sockets` socket's kernel stream dies with the socket.
    ///
    /// A component's resource destructors run when the *guest* drops a
    /// handle and never when the store around it is torn down, so a
    /// close that only the destructor performed was lost by every
    /// program that exited with its socket still open. In #184 that
    /// left connections in their shards — eleven of them on one shard,
    /// each still advertising a receive window — after the programs
    /// that opened them had gone. Ownership lives on the socket state
    /// instead, so whatever ends the resource's life ends the stream's.
    ///
    /// The socket resource holds no service (#219), so its drop queues
    /// the stream and the store closes it on its next turn. Both halves
    /// are asserted here: nothing is queued while the socket lives, the
    /// drop queues, and the drain closes.
    #[test]
    fn a_wasi_tcp_socket_retires_its_stream_when_its_resource_is_dropped() {
        let (service, closed) = crate::test_support::recording_network_service();
        let retirement = TestSocketRetirement::new(service.clone());
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        block_on(socket.connect(&service, tcp4([127, 0, 0, 1], 80)))
            .expect("the test service always connects");

        assert!(
            !retirement.queued(),
            "a live socket must not have retired its stream"
        );
        drop(socket);
        assert!(
            retirement.queued(),
            "dropping the socket must queue the stream it owns"
        );
        assert_eq!(
            closed.count(),
            0,
            "nothing is closed until the store takes its turn"
        );
        assert_eq!(retirement.drain(), 1, "the drain retires the queued stream");
        assert_eq!(closed.count(), 1);
        assert_eq!(closed.last(), 7, "the retired stream is the one connected");
    }

    /// A socket that owns nothing queues nothing, so a store whose
    /// guest only ever created sockets never pays a close.
    #[test]
    fn a_wasi_tcp_socket_that_owns_nothing_queues_nothing() {
        let (service, closed) = crate::test_support::recording_network_service();
        let retirement = TestSocketRetirement::new(service.clone());
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);

        drop(socket);
        assert!(
            !retirement.queued(),
            "a socket with no stream and no listener has nothing to retire"
        );
        assert_eq!(retirement.drain(), 0);
        assert_eq!(closed.count(), 0);
    }

    /// A socket handed to a stream producer outlives the resource
    /// table, and the stream is retired only once the last holder is
    /// gone. Retiring on the first drop would close a connection the
    /// read side is still using, and the slab slot it frees is handed
    /// straight to the next connection.
    #[test]
    fn a_wasi_tcp_socket_retires_its_stream_only_once_every_holder_is_gone() {
        let (service, closed) = crate::test_support::recording_network_service();
        let retirement = TestSocketRetirement::new(service.clone());
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        block_on(socket.connect(&service, tcp4([127, 0, 0, 1], 80)))
            .expect("the test service always connects");
        let borrowed = socket.clone();

        drop(socket);
        assert_eq!(
            retirement.drain(),
            0,
            "a stream still held by another clone must stay open"
        );
        drop(borrowed);
        assert_eq!(retirement.drain(), 1, "the last holder retires the stream");
        assert_eq!(closed.count(), 1);
    }

    /// The retirement reaches the wire: the last holder of a
    /// `wasi:sockets` socket puts its FIN on it.
    ///
    /// #184 moved the stream's ownership onto `TcpSocketState`, and the
    /// tests above prove the drop retires it. What retirement meant
    /// inside the stack was dropping the socket, clearing its timers
    /// and freeing its slot with nothing sent, so a component that
    /// exited holding a connection was invisible to its peer, which
    /// kept an established connection until its own timeout (#224).
    #[test]
    fn a_wasi_tcp_socket_puts_its_fin_on_the_wire_when_its_resource_is_dropped() {
        use crate::network::EstablishedTcpFixture;
        use helios_netstack::TcpFlags;

        let fixture = EstablishedTcpFixture::new();
        let retirement = crate::SocketRetirementQueue::new();
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        socket.inner.lock().stream = Some(crate::NetworkHandle::into_raw(fixture.stream()));

        assert!(
            fixture.drive().is_empty(),
            "a live socket owes its peer nothing"
        );

        drop(socket);
        assert_eq!(
            crate::retire_queued_handles(&retirement, &fixture.service()),
            1,
            "the store's drain retires the connection the socket queued"
        );
        let segments = fixture.drive();
        let fin = segments
            .iter()
            .find(|segment| segment.flags.contains(TcpFlags::FIN))
            .expect("the resource takes its connection down with a FIN");
        assert_eq!(
            fin.sequence,
            EstablishedTcpFixture::LOCAL_SEQUENCE.wrapping_add(1),
            "the FIN carries this side's send sequence"
        );
        assert!(
            !fin.flags.contains(TcpFlags::RST),
            "a connection with nothing unread is closed, not aborted"
        );
    }

    /// The reply a connected socket would read back, as the peer's
    /// segment header. `acked_len` is how many bytes this side sent —
    /// the acknowledgement may not outrun it.
    fn peer_data_segment(acked_len: u32) -> helios_netstack::TcpHeader {
        use crate::network::EstablishedTcpFixture;
        use helios_netstack::TcpFlags;
        helios_netstack::TcpHeader {
            source_port: EstablishedTcpFixture::PEER_PORT,
            destination_port: EstablishedTcpFixture::LOCAL_PORT,
            // The handshake consumed one sequence number on each side;
            // the peer's next payload rides on `PEER_SEQUENCE + 1`, and
            // its acknowledgement covers this side's SYN plus what the
            // test sent.
            sequence: EstablishedTcpFixture::PEER_SEQUENCE + 1,
            acknowledgement: EstablishedTcpFixture::LOCAL_SEQUENCE + 1 + acked_len,
            flags: TcpFlags::ACK.union(TcpFlags::PSH),
            window_size: u16::MAX,
        }
    }

    /// The `wasi:io` streams on a connected TCP socket run the whole
    /// exchange on the task that polls them (#354).
    ///
    /// Both halves prove it the same way: nothing between the stream and
    /// the socket can run a task — the types hold no spawner — so the
    /// write's segment reaching the device's transmit ring and the read
    /// answering from the receive queue happen inside this test's own
    /// `block_on`. The bridge they replaced needed two detached tasks for
    /// the same round trip.
    #[test]
    fn p2_tcp_socket_streams_run_the_exchange_on_the_calling_task() {
        use crate::network::EstablishedTcpFixture;
        use wasmtime_wasi_io::poll::Pollable;
        use wasmtime_wasi_io::streams::{InputStream, OutputStream};

        let device = crate::test_support::RecordingNetworkInterface::accepting_transmissions(1);
        let fixture = EstablishedTcpFixture::with_interface(device.clone());
        let retirement = crate::SocketRetirementQueue::new();
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        socket.inner.lock().stream = Some(crate::NetworkHandle::into_raw(fixture.stream()));
        let service = fixture.service();
        let mut input = super::net::TcpSocketInputStream::new(socket.clone(), service.clone());
        let mut output = super::net::TcpSocketOutputStream::new(socket, service);

        // `write` is queue, segment, submit and doorbell on this poll —
        // the frame on the device's ring is the proof.
        block_on(output.ready());
        assert!(
            output
                .check_write()
                .expect("a fresh connection has send room")
                > 0,
            "the permit must cover at least one byte"
        );
        output
            .write(Bytes::from_static(b"ping"))
            .expect("the write queues onto the send path");
        let wire = device.transmitted_frames();
        assert!(
            wire.iter()
                .any(|frame| frame.windows(4).any(|window| window == b"ping")),
            "the write's payload reached the wire inside the write call"
        );

        // `read` drains the socket's queue synchronously: the reply the
        // stack already accepted comes back from the same call.
        fixture.deliver(peer_data_segment(4), b"pong");
        let bytes = input.read(4).expect("the queued reply reads back");
        assert_eq!(&bytes[..], b"pong");
    }

    /// A segment landing between `ready`'s queue probe and its park
    /// still wakes the reader — the arm-before-test rule.
    ///
    /// The first `poll` finds an empty queue and parks on the shard's
    /// wait with the signal already armed; `deliver_rx` then puts the
    /// reply on the device's ring and raises the completion event — the
    /// arrive-between-test-and-park window, reproduced exactly. The next
    /// poll must resolve: sleeping through it is the defect §4 exists to
    /// prevent.
    #[test]
    fn p2_tcp_input_stream_wakes_when_a_segment_arrives_between_probe_and_park() {
        use crate::network::EstablishedTcpFixture;
        use futures_lite::future::poll_once;
        use wasmtime_wasi_io::poll::Pollable;
        use wasmtime_wasi_io::streams::InputStream;

        let fixture = EstablishedTcpFixture::new();
        let retirement = crate::SocketRetirementQueue::new();
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        socket.inner.lock().stream = Some(crate::NetworkHandle::into_raw(fixture.stream()));
        let mut input = super::net::TcpSocketInputStream::new(socket, fixture.service());

        let mut ready = input.ready();
        assert!(
            block_on(poll_once(ready.as_mut())).is_none(),
            "an empty receive queue parks the readiness wait"
        );
        fixture.deliver_rx(peer_data_segment(0), b"pong");
        assert!(
            block_on(poll_once(ready.as_mut())).is_some(),
            "the segment that arrived between probe and park resolves the wait"
        );
        drop(ready);
        let bytes = input.read(4).expect("the parked reader's reply reads back");
        assert_eq!(&bytes[..], b"pong");
    }

    /// `shutdown(Send)` makes the output stream permanently ready, and
    /// the accessors that follow report `closed`.
    ///
    /// The flag and the stack's FIN are both exercised: the readiness
    /// wait resolves on the flag, and the send probe reports the socket
    /// the shutdown left behind — a `FinWait1` connection cannot send
    /// (#358 review: a zero-room answer conflated this with a full
    /// queue, and the wait never resolved).
    #[test]
    fn p2_tcp_output_stream_reports_closed_after_send_shutdown() {
        use crate::network::EstablishedTcpFixture;
        use futures_lite::future::poll_once;
        use wasmtime_wasi_io::poll::Pollable;
        use wasmtime_wasi_io::streams::{OutputStream, StreamError};

        let fixture = EstablishedTcpFixture::new();
        let service = fixture.service();
        let retirement = crate::SocketRetirementQueue::new();
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        socket.inner.lock().stream = Some(crate::NetworkHandle::into_raw(fixture.stream()));
        let mut output = super::net::TcpSocketOutputStream::new(socket.clone(), service.clone());

        socket
            .shutdown_send_state()
            .expect("a connected socket shuts its send side");
        block_on(service.tcp_shutdown_send(fixture.stream())).expect("the shutdown queues the FIN");
        assert!(
            matches!(
                service.tcp_send_room(fixture.stream()),
                Ok(crate::TcpWriteProgress::Closed(_))
            ),
            "the send probe reports the close, not a full queue"
        );

        assert!(
            block_on(poll_once(output.ready().as_mut())).is_some(),
            "a shut-down send side resolves the readiness wait"
        );
        assert!(
            matches!(output.check_write(), Err(StreamError::Closed)),
            "check-write reports the closed send side"
        );
        assert!(
            matches!(
                output.write(Bytes::from_static(b"ping")),
                Err(StreamError::Closed)
            ),
            "write reports the closed send side"
        );
    }

    /// The peer's reset ends the send side the same way: `ready`
    /// resolves and `check_write`/`write` answer `closed`.
    #[test]
    fn p2_tcp_output_stream_reports_closed_after_peer_reset() {
        use crate::network::EstablishedTcpFixture;
        use futures_lite::future::poll_once;
        use helios_netstack::TcpFlags;
        use wasmtime_wasi_io::poll::Pollable;
        use wasmtime_wasi_io::streams::{OutputStream, StreamError};

        let fixture = EstablishedTcpFixture::new();
        let retirement = crate::SocketRetirementQueue::new();
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        socket.inner.lock().stream = Some(crate::NetworkHandle::into_raw(fixture.stream()));
        let mut output = super::net::TcpSocketOutputStream::new(socket, fixture.service());

        fixture.deliver(
            helios_netstack::TcpHeader {
                source_port: EstablishedTcpFixture::PEER_PORT,
                destination_port: EstablishedTcpFixture::LOCAL_PORT,
                sequence: EstablishedTcpFixture::PEER_SEQUENCE + 1,
                acknowledgement: EstablishedTcpFixture::LOCAL_SEQUENCE + 1,
                flags: TcpFlags::RST.union(TcpFlags::ACK),
                window_size: 0,
            },
            &[],
        );

        assert!(
            block_on(poll_once(output.ready().as_mut())).is_some(),
            "a reset send side resolves the readiness wait"
        );
        assert!(
            matches!(output.check_write(), Err(StreamError::Closed)),
            "check-write reports the reset send side as closed"
        );
        assert!(
            matches!(
                output.write(Bytes::from_static(b"ping")),
                Err(StreamError::Closed)
            ),
            "write reports the reset send side as closed"
        );
    }

    /// A `ready` parked behind a full send queue is woken by the peer's
    /// ACK — the send half of the arm-before-test rule.
    ///
    /// The queue is filled through the service directly so `pending`
    /// stays empty and the wait is `tcp_write_ready`'s own loop: probe,
    /// arm, park. The ACK then arrives the way the device delivers one
    /// — ring plus completion event — and the armed wait resolves.
    #[test]
    fn p2_tcp_output_stream_wakes_when_an_ack_arrives_between_probe_and_park() {
        use crate::network::EstablishedTcpFixture;
        use futures_lite::future::poll_once;
        use helios_netstack::TcpFlags;
        use wasmtime_wasi_io::poll::Pollable;
        use wasmtime_wasi_io::streams::OutputStream;

        let fixture = EstablishedTcpFixture::new();
        let service = fixture.service();
        let retirement = crate::SocketRetirementQueue::new();
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        socket.inner.lock().stream = Some(crate::NetworkHandle::into_raw(fixture.stream()));
        let mut output = super::net::TcpSocketOutputStream::new(socket, service.clone());

        // The peer advertised a 64 KiB window and never ACKs, so enough
        // queued bytes fill it and then the send queue behind it.
        let mut fill = Bytes::from(vec![0xAB; 2 * 1024 * 1024]);
        loop {
            service
                .tcp_try_write(fixture.stream(), &mut fill)
                .expect("the fill write queues");
            match service
                .tcp_send_room(fixture.stream())
                .expect("the probe answers")
            {
                crate::TcpWriteProgress::Pending => break,
                crate::TcpWriteProgress::Room(_) => {}
                crate::TcpWriteProgress::Closed(_) => {
                    panic!("a live connection cannot report a closed send side")
                }
            }
            assert!(
                !fill.is_empty(),
                "the send queue must fill before the fill buffer runs out"
            );
        }
        // How far the send sequence actually ran is what the peer may
        // acknowledge — the window, the congestion window and the
        // socket's own segmentation decide it, so read it off the
        // wire: the refused device's ring kept the segments staged,
        // and `drive` reports them.
        let sent = fixture
            .drive()
            .iter()
            .map(|segment| segment.payload_len as u32)
            .fold(
                EstablishedTcpFixture::LOCAL_SEQUENCE.wrapping_add(1),
                u32::wrapping_add,
            );
        assert_ne!(
            sent,
            EstablishedTcpFixture::LOCAL_SEQUENCE.wrapping_add(1),
            "the fill's drive put data on the wire"
        );

        let mut ready = output.ready();
        assert!(
            block_on(poll_once(ready.as_mut())).is_none(),
            "a full send queue parks the readiness wait"
        );
        // A full ACK reopens the window; the drive the woken wait runs
        // then moves queued bytes into flight and frees the queue.
        fixture.deliver_rx(
            helios_netstack::TcpHeader {
                source_port: EstablishedTcpFixture::PEER_PORT,
                destination_port: EstablishedTcpFixture::LOCAL_PORT,
                sequence: EstablishedTcpFixture::PEER_SEQUENCE + 1,
                acknowledgement: sent,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
            &[],
        );
        assert!(
            block_on(poll_once(ready.as_mut())).is_some(),
            "the ACK that arrived after the park resolves the wait"
        );
        drop(ready);
        assert!(
            output
                .check_write()
                .expect("the freed send queue reports room")
                > 0,
            "the drained queue reopens the write permit"
        );
    }

    /// A device fault inside `ready`'s drive is not reproducible by
    /// `read`'s non-parking probe, so the stream keeps it and the next
    /// `read` reports it — `last-operation-failed`, not an empty read
    /// that would spin a `poll` loop forever.
    #[test]
    fn p2_tcp_input_stream_reports_the_failure_ready_saw() {
        use wasmtime_wasi_io::poll::Pollable;
        use wasmtime_wasi_io::streams::{InputStream, StreamError};

        let service = TestNetworkService::new();
        service.fail_drives();
        let retirement = crate::SocketRetirementQueue::new();
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        socket.inner.lock().stream = Some(7);
        let mut input = super::net::TcpSocketInputStream::new(socket, service);

        // `ready` resolves — the pollable must, so the accessor can
        // report what happened — and records the failure it saw.
        block_on(input.ready());
        assert!(
            matches!(input.read(4), Err(StreamError::LastOperationFailed(_))),
            "the read reports the drive failure"
        );
    }

    /// The output half of the same contract: a parked `pending` batch
    /// whose completion fails is kept, and the failure is what
    /// `check_write`/`flush`/`write` then report — not a silently
    /// dropped batch behind a fresh permit.
    #[test]
    fn p2_tcp_output_stream_keeps_the_batch_whose_write_failed() {
        use wasmtime_wasi_io::poll::Pollable;
        use wasmtime_wasi_io::streams::{OutputStream, StreamError};

        let service = TestNetworkService::new();
        let retirement = crate::SocketRetirementQueue::new();
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        socket.inner.lock().stream = Some(7);
        let mut output = super::net::TcpSocketOutputStream::new(socket, service.clone());

        // With the drive broken the send queue reads as full, so the
        // write parks its batch in `pending`.
        service.fail_drives();
        output
            .write(Bytes::from_static(b"ping"))
            .expect("the write parks the batch it cannot queue");
        block_on(output.ready());
        assert!(
            matches!(
                output.check_write(),
                Err(StreamError::LastOperationFailed(_))
            ),
            "check-write reports the failed batch write"
        );
        assert!(
            matches!(output.flush(), Err(StreamError::LastOperationFailed(_))),
            "flush reports the failed batch write"
        );

        // The batch survived: once the drive heals, `ready` retries it
        // and the service takes the bytes it was never allowed to drop.
        service.heal_drives();
        block_on(output.ready());
        assert_eq!(
            service.bytes_written(),
            4,
            "the parked batch is retried, not dropped"
        );
    }

    /// A `wasi:sockets` socket's kernel listener dies with the socket.
    ///
    /// Nothing retired it before: the resource destructor deleted the
    /// table entry, `TcpSocketState`'s `Drop` retired only the stream
    /// beside it, and the network service offered no
    /// `tcp_listener_close` at all — so every listener a component
    /// opened stayed installed on every shard with its local port bound
    /// for the rest of the boot (#191).
    #[test]
    fn a_wasi_tcp_socket_retires_its_listener_when_its_resource_is_dropped() {
        let (service, closed) = crate::test_support::recording_listener_network_service();
        let retirement = TestSocketRetirement::new(service);
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        socket.inner.lock().listener = Some(41);

        assert!(
            !retirement.queued(),
            "a live socket must not have retired its listener"
        );
        drop(socket);
        assert_eq!(
            retirement.drain(),
            1,
            "dropping the socket must retire the listener it owns"
        );
        assert_eq!(closed.count(), 1);
        assert_eq!(closed.last(), 41);
    }

    /// A listener that was opened but never adopted is retired too.
    ///
    /// `tcp_listen` completes on a detached task and parks its answer in
    /// `listen_result`; the listen stream moves it into `listener` the
    /// next time it is polled. A socket dropped in that window holds a
    /// listener in the shards with nothing pointing at it, which is the
    /// same leak by a narrower door.
    #[test]
    fn a_wasi_tcp_socket_retires_a_listener_it_never_adopted() {
        let (service, closed) = crate::test_support::recording_listener_network_service();
        let retirement = TestSocketRetirement::new(service);
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        socket.inner.lock().listen_result = Some(Ok(crate::TcpListener {
            listener: 42,
            local_port: 8080,
        }));

        drop(socket);
        assert_eq!(
            retirement.drain(),
            1,
            "a listener still sitting in the listen result is retired"
        );
        assert_eq!(closed.count(), 1);
        assert_eq!(closed.last(), 42);
    }

    /// The socket a completed accept parks, as `PendingAccept`'s task
    /// builds it.
    fn accepted_connection(retire: crate::SocketRetirementSender, stream: u64) -> TcpSocket {
        TcpSocket::from_accepted(
            retire,
            WasiTcpSocketFamily::Ipv4,
            tcp4([127, 0, 0, 1], 8080),
            crate::TcpAccepted {
                stream,
                address: crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([127, 0, 0, 1])),
                port: 4040,
            },
        )
    }

    /// A connection that was accepted but never adopted is retired too.
    ///
    /// The accept completes on a detached task and parks its answer in
    /// `accept_result`; the accept path turns it into a guest resource
    /// the next time the guest asks. While that answer was a bare
    /// stream id, a socket dropped in the window between the two left a
    /// connected stream in its shard with nothing pointing at it, which
    /// is #184's leak through a narrower door (#225). The parked value
    /// is the accepted socket itself, so it dies with the listening one.
    #[test]
    fn a_wasi_tcp_socket_retires_a_connection_it_never_adopted() {
        let (service, closed) = crate::test_support::recording_network_service();
        let retirement = TestSocketRetirement::new(service);
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        socket.inner.lock().accept_result = Some(Ok(accepted_connection(retirement.sender(), 43)));

        drop(socket);
        assert_eq!(
            retirement.drain(),
            1,
            "a connection still sitting in the accept result is retired"
        );
        assert_eq!(closed.count(), 1);
        assert_eq!(closed.last(), 43);
    }

    /// A connection accepted after the socket died is retired too.
    ///
    /// The detached accept task holds the listening socket's state,
    /// because that is where it parks its answer, so the state outlives
    /// the guest resource whenever the two race. What the task parks
    /// owns its stream, and the state's own drop — the moment the task
    /// lets go of it — retires the connection nobody will ever adopt.
    #[test]
    fn a_wasi_tcp_socket_retires_a_connection_accepted_after_it_died() {
        let (service, closed) = crate::test_support::recording_network_service();
        let retirement = TestSocketRetirement::new(service);
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        let task_state = socket.inner.clone();
        socket.inner.lock().accept_in_progress = true;

        drop(socket);
        assert!(
            !retirement.queued(),
            "the accept task still holds the listening socket's state"
        );

        {
            let mut state = task_state.lock();
            state.accept_in_progress = false;
            state.accept_result = Some(Ok(accepted_connection(retirement.sender(), 43)));
        }
        drop(task_state);
        assert_eq!(
            retirement.drain(),
            1,
            "the state the accept task released retires the connection it accepted"
        );
        assert_eq!(closed.count(), 1);
        assert_eq!(closed.last(), 43);
    }

    /// A `wasi:sockets` datagram socket's kernel socket dies with the
    /// socket.
    ///
    /// Nothing retired it before: the resource destructor deleted the
    /// table entry and `UdpSocketState` had no `Drop`, so every program
    /// that bound a `udp-socket` left a replica on every shard and a
    /// slot in `udp_slots` behind it (#190).
    #[test]
    fn a_wasi_udp_socket_retires_its_socket_when_its_resource_is_dropped() {
        let (service, closed) = crate::test_support::recording_udp_network_service();
        let retirement = TestSocketRetirement::new(service.clone());
        let socket = UdpSocket::new(retirement.sender(), WasiUdpSocketFamily::Ipv4);
        block_on(socket.bind(&service, udp4([0, 0, 0, 0], 5353)))
            .expect("the test service always binds");

        assert!(
            !retirement.queued(),
            "a live socket must not have retired its binding"
        );
        drop(socket);
        assert_eq!(
            retirement.drain(),
            1,
            "dropping the socket must retire the binding it owns"
        );
        assert_eq!(closed.count(), 1);
        assert_eq!(closed.last(), 9, "the retired socket is the one bound");
    }

    /// A bind the guest started and never finished is retired too. The
    /// kernel socket is allocated by `start-bind`, and a program is
    /// free to exit before `finish-bind` promotes it.
    #[test]
    fn a_wasi_udp_socket_retires_a_bind_that_never_finished() {
        let (service, closed) = crate::test_support::recording_udp_network_service();
        let retirement = TestSocketRetirement::new(service.clone());
        let socket = UdpSocket::new(retirement.sender(), WasiUdpSocketFamily::Ipv4);
        block_on(socket.start_bind_p2(&service, udp4([0, 0, 0, 0], 5353)))
            .expect("the test service always binds");

        assert!(!retirement.queued());
        drop(socket);
        assert_eq!(
            retirement.drain(),
            1,
            "a pending bind holds a kernel socket and must retire it"
        );
        assert_eq!(closed.count(), 1);
        assert_eq!(closed.last(), 9);
    }

    /// A socket handed to the p2 datagram streams outlives the resource
    /// table, and the binding is retired only once the last holder is
    /// gone. Retiring on the first drop would unbind a socket the
    /// incoming stream is still reading, and the slab slot it frees is
    /// handed straight to the next bind.
    #[test]
    fn a_wasi_udp_socket_retires_its_socket_only_once_every_holder_is_gone() {
        let (service, closed) = crate::test_support::recording_udp_network_service();
        let retirement = TestSocketRetirement::new(service.clone());
        let socket = UdpSocket::new(retirement.sender(), WasiUdpSocketFamily::Ipv4);
        block_on(socket.bind(&service, udp4([0, 0, 0, 0], 5353)))
            .expect("the test service always binds");
        let borrowed = socket.clone();

        drop(socket);
        assert_eq!(
            retirement.drain(),
            0,
            "a binding still held by another clone must stay open"
        );
        drop(borrowed);
        assert_eq!(retirement.drain(), 1, "the last holder retires the binding");
        assert_eq!(closed.count(), 1);
    }

    #[test]
    fn p3_tcp_socket_hop_limit_is_descriptor_local_state() {
        let service = TestNetworkService::new();
        let retirement = TestSocketRetirement::new(service.clone());
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);

        assert_eq!(socket.hop_limit().unwrap(), DEFAULT_WASI_TCP_HOP_LIMIT);
        socket
            .set_hop_limit(&service, 127)
            .expect("nonzero hop limit must be accepted");
        assert_eq!(socket.hop_limit().unwrap(), 127);
        assert!(matches!(
            socket.set_hop_limit(&service, 0),
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
        let retirement = TestSocketRetirement::new(TestNetworkService::new());
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);

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
        let service = TestNetworkService::new();
        let retirement = TestSocketRetirement::new(service.clone());
        let socket = TcpSocket::accepted(
            retirement.sender(),
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
            block_on(socket.read(&service, 8)).expect("receive shutdown read must succeed"),
            None
        );

        let stream = socket
            .shutdown_send_state()
            .expect("connected send shutdown must be accepted");
        assert_eq!(stream, 7);
        let stream = socket
            .shutdown_send_state()
            .expect("send shutdown must be idempotent");
        assert_eq!(stream, 7);
        assert!(matches!(
            block_on(socket.write_all_bytes(&service, Bytes::from_static(b"x"))),
            Err(socket_types::ErrorCode::InvalidState)
        ));
    }

    #[test]
    fn tcp_socket_bind_local_tracks_authorized_local_address() {
        let retirement = TestSocketRetirement::new(TestNetworkService::new());
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        let local = tcp4([127, 0, 0, 1], 8080);

        socket
            .bind_local(local)
            .expect("unconnected socket must accept a local bind");
        assert_eq!(socket.local_address().unwrap(), local);
        assert!(!socket.is_listening());
    }

    #[test]
    fn tcp_socket_listen_backlog_is_descriptor_local_state() {
        let retirement = TestSocketRetirement::new(TestNetworkService::new());
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);

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
        let service = TestNetworkService::new();
        let retirement = TestSocketRetirement::new(service.clone());
        let socket = TcpSocket::new(retirement.sender(), WasiTcpSocketFamily::Ipv4);
        let remote = tcp4([203, 0, 113, 10], 443);

        block_on(socket.connect(&service, remote)).expect("test TCP backend should connect");
        assert_eq!(socket.remote_address().unwrap(), remote);
        block_on(socket.write_all_bytes(&service, Bytes::from_static(b"hello")))
            .expect("test TCP backend should write");
        let bytes = block_on(socket.read(&service, 16))
            .unwrap()
            .expect("test TCP backend should read bytes");
        assert_eq!(bytes.as_ref(), [4, 2]);
    }
}
