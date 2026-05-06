extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use core::future::Future;

use crate::InstanceId;
use bytes::Bytes;
use helios_hal::io::IoError;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct ExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct ExecResult {
    pub instance_id: InstanceId,
    pub exit_code: u32,
    pub output: ExecOutput,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum PingErrorKind {
    #[error("host could not be resolved")]
    UnresolvedHost,
    #[error("operation timed out")]
    Timeout,
    #[error("network service is unavailable")]
    Unavailable,
    #[error("internal network error")]
    Internal,
}

impl PingErrorKind {
    pub const fn from_io(error: IoError) -> Self {
        match error {
            IoError::Unsupported | IoError::PermissionDenied | IoError::ReadOnly => {
                PingErrorKind::Unavailable
            }
            IoError::NotFound
            | IoError::AlreadyExists
            | IoError::NotDirectory
            | IoError::IsDirectory
            | IoError::DirectoryNotEmpty
            | IoError::InvalidBufferLength { .. }
            | IoError::InvalidDeviceConfig(_)
            | IoError::OutOfBounds
            | IoError::DeviceFault => PingErrorKind::Internal,
        }
    }
}

#[derive(Clone, Debug, Error)]
#[error("{kind}: {detail}")]
pub struct PingError {
    pub kind: PingErrorKind,
    pub detail: NetworkErrorDetail,
}

impl PingError {
    pub const fn from_io(error: IoError, detail: NetworkErrorDetail) -> Self {
        Self {
            kind: PingErrorKind::from_io(error),
            detail,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Address {
    octets: [u8; 4],
}

impl Ipv4Address {
    pub const fn new(octets: [u8; 4]) -> Self {
        Self { octets }
    }

    pub const fn octets(self) -> [u8; 4] {
        self.octets
    }

    pub fn write_dotted_decimal<'a>(self, buffer: &'a mut [u8; 15]) -> &'a str {
        let mut cursor = 0;
        for (index, octet) in self.octets.into_iter().enumerate() {
            if index != 0 {
                buffer[cursor] = b'.';
                cursor += 1;
            }
            cursor += write_decimal_u8(octet, &mut buffer[cursor..]);
        }
        // SAFETY: all bytes written by `write_decimal_u8` and the separators
        // above are ASCII digits or dots.
        unsafe { core::str::from_utf8_unchecked(&buffer[..cursor]) }
    }
}

fn write_decimal_u8(value: u8, buffer: &mut [u8]) -> usize {
    if value >= 100 {
        buffer[0] = b'0' + (value / 100);
        buffer[1] = b'0' + ((value / 10) % 10);
        buffer[2] = b'0' + (value % 10);
        3
    } else if value >= 10 {
        buffer[0] = b'0' + (value / 10);
        buffer[1] = b'0' + (value % 10);
        2
    } else {
        buffer[0] = b'0' + value;
        1
    }
}

#[derive(Clone, Debug)]
pub struct PingReply {
    pub address: Ipv4Address,
    pub round_trip_nanos: u64,
    pub payload_bytes: u16,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DnsErrorKind {
    #[error("host could not be resolved")]
    UnresolvedHost,
    #[error("operation timed out")]
    Timeout,
    #[error("DNS service is unavailable")]
    Unavailable,
    #[error("internal DNS error")]
    Internal,
}

impl DnsErrorKind {
    pub const fn from_io(error: IoError) -> Self {
        match error {
            IoError::Unsupported | IoError::PermissionDenied | IoError::ReadOnly => {
                DnsErrorKind::Unavailable
            }
            IoError::NotFound
            | IoError::AlreadyExists
            | IoError::NotDirectory
            | IoError::IsDirectory
            | IoError::DirectoryNotEmpty
            | IoError::InvalidBufferLength { .. }
            | IoError::InvalidDeviceConfig(_)
            | IoError::OutOfBounds
            | IoError::DeviceFault => DnsErrorKind::Internal,
        }
    }
}

#[derive(Clone, Debug, Error)]
#[error("{kind}: {detail}")]
pub struct DnsError {
    pub kind: DnsErrorKind,
    pub detail: NetworkErrorDetail,
}

impl DnsError {
    pub const fn from_io(error: IoError, detail: NetworkErrorDetail) -> Self {
        Self {
            kind: DnsErrorKind::from_io(error),
            detail,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum TcpErrorKind {
    #[error("host could not be resolved")]
    UnresolvedHost,
    #[error("operation timed out")]
    Timeout,
    #[error("permission denied")]
    PermissionDenied,
    #[error("TCP service is unavailable")]
    Unavailable,
    #[error("internal TCP error")]
    Internal,
}

impl TcpErrorKind {
    pub const fn from_io(error: IoError) -> Self {
        match error {
            IoError::PermissionDenied | IoError::ReadOnly => TcpErrorKind::PermissionDenied,
            IoError::Unsupported => TcpErrorKind::Unavailable,
            IoError::NotFound
            | IoError::AlreadyExists
            | IoError::NotDirectory
            | IoError::IsDirectory
            | IoError::DirectoryNotEmpty
            | IoError::InvalidBufferLength { .. }
            | IoError::InvalidDeviceConfig(_)
            | IoError::OutOfBounds
            | IoError::DeviceFault => TcpErrorKind::Internal,
        }
    }
}

#[derive(Clone, Debug, Error)]
#[error("{kind}: {detail}")]
pub struct TcpError {
    pub kind: TcpErrorKind,
    pub detail: NetworkErrorDetail,
}

impl TcpError {
    pub const fn from_io(error: IoError, detail: NetworkErrorDetail) -> Self {
        Self {
            kind: TcpErrorKind::from_io(error),
            detail,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum UdpErrorKind {
    #[error("host could not be resolved")]
    UnresolvedHost,
    #[error("operation timed out")]
    Timeout,
    #[error("permission denied")]
    PermissionDenied,
    #[error("UDP service is unavailable")]
    Unavailable,
    #[error("internal UDP error")]
    Internal,
}

impl UdpErrorKind {
    pub const fn from_io(error: IoError) -> Self {
        match error {
            IoError::Unsupported | IoError::PermissionDenied | IoError::ReadOnly => {
                UdpErrorKind::Unavailable
            }
            IoError::NotFound
            | IoError::AlreadyExists
            | IoError::NotDirectory
            | IoError::IsDirectory
            | IoError::DirectoryNotEmpty
            | IoError::InvalidBufferLength { .. }
            | IoError::InvalidDeviceConfig(_)
            | IoError::OutOfBounds
            | IoError::DeviceFault => UdpErrorKind::Internal,
        }
    }
}

#[derive(Clone, Debug, Error)]
#[error("{kind}: {detail}")]
pub struct UdpError {
    pub kind: UdpErrorKind,
    pub detail: NetworkErrorDetail,
}

impl UdpError {
    pub const fn from_io(error: IoError, detail: NetworkErrorDetail) -> Self {
        Self {
            kind: UdpErrorKind::from_io(error),
            detail,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum NetworkErrorDetail {
    #[error("network configuration timed out")]
    NetworkConfigurationTimeout,
    #[error("network service is unavailable")]
    NetworkServiceUnavailable,
    #[error("network authority is missing required rights")]
    NetworkAuthorityMissing,
    #[error("privileged bind authority is required for this port")]
    PrivilegedBindDenied,
    #[error("virtio network device advance failed")]
    VirtioAdvanceFailed,
    #[error("DNS servers are not configured")]
    DnsServersUnavailable,
    #[error("DNS query could not be started")]
    DnsQueryStartFailed,
    #[error("DNS lookup timed out")]
    DnsLookupTimeout,
    #[error("DNS lookup failed")]
    DnsLookupFailed,
    #[error("DNS lookup returned no IPv4 address")]
    DnsNoIpv4Address,
    #[error("DNS result is not ready")]
    DnsResultNotReady,
    #[error("ICMP echo request timed out")]
    IcmpEchoTimeout,
    #[error("ICMP echo request could not be queued")]
    IcmpQueueFailed,
    #[error("TCP connect timed out")]
    TcpConnectTimeout,
    #[error("TCP connect could not be started")]
    TcpConnectStartFailed,
    #[error("TCP stream closed during connect")]
    TcpClosedDuringConnect,
    #[error("TCP listen could not be started")]
    TcpListenStartFailed,
    #[error("TCP accept timed out")]
    TcpAcceptTimeout,
    #[error("TCP listener closed unexpectedly")]
    TcpListenerClosedUnexpectedly,
    #[error("TCP write timed out")]
    TcpWriteTimeout,
    #[error("TCP write could not be queued")]
    TcpWriteQueueFailed,
    #[error("TCP stream is no longer writable")]
    TcpNoLongerWritable,
    #[error("TCP read timed out")]
    TcpReadTimeout,
    #[error("TCP receive failed")]
    TcpReceiveFailed,
    #[error("TCP stream closed unexpectedly")]
    TcpClosedUnexpectedly,
    #[error("no ephemeral TCP ports are available")]
    TcpNoEphemeralPorts,
    #[error("UDP send timed out")]
    UdpSendTimeout,
    #[error("UDP receive timed out")]
    UdpReceiveTimeout,
    #[error("UDP local port is already in use")]
    UdpPortInUse,
    #[error("UDP bind failed")]
    UdpBindFailed,
    #[error("UDP datagram exceeds transmit capacity")]
    UdpDatagramTooLarge,
    #[error("UDP datagram could not be queued")]
    UdpQueueFailed,
    #[error("UDP receive failed")]
    UdpReceiveFailed,
    #[error("no ephemeral UDP ports are available")]
    UdpNoEphemeralPorts,
    #[error("UDP multicast interface is unavailable")]
    UdpMulticastInterfaceUnavailable,
    #[error("UDP multicast group could not be joined")]
    UdpMulticastJoinFailed,
    #[error("UDP multicast group could not be left")]
    UdpMulticastLeaveFailed,
    #[error("unknown TCP stream")]
    UnknownTcpStream,
    #[error("unknown UDP socket")]
    UnknownUdpSocket,
    #[error("internal network invariant failed")]
    InternalInvariant,
}

impl NetworkErrorDetail {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NetworkConfigurationTimeout => "network configuration timed out",
            Self::NetworkServiceUnavailable => "network service is unavailable",
            Self::NetworkAuthorityMissing => "network authority is missing required rights",
            Self::PrivilegedBindDenied => "privileged bind authority is required for this port",
            Self::VirtioAdvanceFailed => "virtio network device advance failed",
            Self::DnsServersUnavailable => "DNS servers are not configured",
            Self::DnsQueryStartFailed => "DNS query could not be started",
            Self::DnsLookupTimeout => "DNS lookup timed out",
            Self::DnsLookupFailed => "DNS lookup failed",
            Self::DnsNoIpv4Address => "DNS lookup returned no IPv4 address",
            Self::DnsResultNotReady => "DNS result is not ready",
            Self::IcmpEchoTimeout => "ICMP echo request timed out",
            Self::IcmpQueueFailed => "ICMP echo request could not be queued",
            Self::TcpConnectTimeout => "TCP connect timed out",
            Self::TcpConnectStartFailed => "TCP connect could not be started",
            Self::TcpClosedDuringConnect => "TCP stream closed during connect",
            Self::TcpListenStartFailed => "TCP listen could not be started",
            Self::TcpAcceptTimeout => "TCP accept timed out",
            Self::TcpListenerClosedUnexpectedly => "TCP listener closed unexpectedly",
            Self::TcpWriteTimeout => "TCP write timed out",
            Self::TcpWriteQueueFailed => "TCP write could not be queued",
            Self::TcpNoLongerWritable => "TCP stream is no longer writable",
            Self::TcpReadTimeout => "TCP read timed out",
            Self::TcpReceiveFailed => "TCP receive failed",
            Self::TcpClosedUnexpectedly => "TCP stream closed unexpectedly",
            Self::TcpNoEphemeralPorts => "no ephemeral TCP ports are available",
            Self::UdpSendTimeout => "UDP send timed out",
            Self::UdpReceiveTimeout => "UDP receive timed out",
            Self::UdpPortInUse => "UDP local port is already in use",
            Self::UdpBindFailed => "UDP bind failed",
            Self::UdpDatagramTooLarge => "UDP datagram exceeds transmit capacity",
            Self::UdpQueueFailed => "UDP datagram could not be queued",
            Self::UdpReceiveFailed => "UDP receive failed",
            Self::UdpNoEphemeralPorts => "no ephemeral UDP ports are available",
            Self::UdpMulticastInterfaceUnavailable => "UDP multicast interface is unavailable",
            Self::UdpMulticastJoinFailed => "UDP multicast group could not be joined",
            Self::UdpMulticastLeaveFailed => "UDP multicast group could not be left",
            Self::UnknownTcpStream => "unknown TCP stream",
            Self::UnknownUdpSocket => "unknown UDP socket",
            Self::InternalInvariant => "internal network invariant failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct UdpDatagram {
    pub address: Ipv4Address,
    pub port: u16,
    pub bytes: Bytes,
}

#[derive(Clone, Debug)]
pub struct UdpBinding<Socket> {
    pub socket: Socket,
    pub local_port: u16,
}

#[derive(Clone, Debug)]
pub struct TcpListener<Listener> {
    pub listener: Listener,
    pub local_port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkIpAddress {
    Ipv4(Ipv4Address),
    Ipv6(helios_netstack::Ipv6Address),
}

#[derive(Clone, Debug)]
pub struct TcpAccepted<Stream> {
    pub stream: Stream,
    pub address: NetworkIpAddress,
    pub port: u16,
}

pub trait ComponentNetworkService: Clone + Send + Sync + 'static {
    type TcpStream: Copy + Send + 'static;
    type TcpListener: Copy + Send + 'static;
    type UdpSocket: Copy + Send + 'static;

    fn hardware_address(&self) -> [u8; 6];

    fn ipv4_cidr(&self) -> impl Future<Output = Option<crate::Ipv4Cidr>> + Send + '_;

    fn ping<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<PingReply, PingError>> + Send + 'a;

    fn dns_resolve<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Vec<Ipv4Address>, DnsError>> + Send + 'a;

    fn tcp_connect<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Self::TcpStream, TcpError>> + Send + 'a;

    fn tcp_connect_from<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Self::TcpStream, TcpError>> + Send + 'a;

    fn tcp_listen(
        &self,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: u16,
    ) -> impl Future<Output = Result<TcpListener<Self::TcpListener>, TcpError>> + Send + '_;

    fn tcp_accept(
        &self,
        listener: Self::TcpListener,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<TcpAccepted<Self::TcpStream>, TcpError>> + Send + '_;

    fn tcp_write_all<'a>(
        &'a self,
        stream: Self::TcpStream,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<(), TcpError>> + Send + 'a;

    fn tcp_write_all_bytes<'a>(
        &'a self,
        stream: Self::TcpStream,
        bytes: Bytes,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<(), TcpError>> + Send + 'a {
        async move { self.tcp_write_all(stream, &bytes, timeout_nanos).await }
    }

    fn tcp_read<'a>(
        &'a self,
        stream: Self::TcpStream,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Option<Bytes>, TcpError>> + Send + 'a;

    fn tcp_shutdown_send(
        &self,
        stream: Self::TcpStream,
    ) -> impl Future<Output = Result<(), TcpError>> + Send + '_;

    fn tcp_close(&self, stream: Self::TcpStream) -> impl Future<Output = ()> + Send + '_;

    fn udp_bind(
        &self,
        local_port: u16,
    ) -> impl Future<Output = Result<UdpBinding<Self::UdpSocket>, UdpError>> + Send + '_;

    fn udp_send<'a>(
        &'a self,
        socket: Self::UdpSocket,
        host: &'a str,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<u64, UdpError>> + Send + 'a;

    fn udp_receive<'a>(
        &'a self,
        socket: Self::UdpSocket,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl Future<Output = Result<Option<UdpDatagram>, UdpError>> + Send + 'a;

    fn udp_join_multicast_v4(
        &self,
        group: Ipv4Address,
        interface: Ipv4Address,
    ) -> impl Future<Output = Result<(), UdpError>> + Send + '_;

    fn udp_leave_multicast_v4(
        &self,
        group: Ipv4Address,
        interface: Ipv4Address,
    ) -> impl Future<Output = Result<(), UdpError>> + Send + '_;

    fn udp_close(&self, socket: Self::UdpSocket) -> impl Future<Output = ()> + Send + '_;
}

pub trait ComponentNetworkState<Service>: Clone + Send + 'static
where
    Service: ComponentNetworkService,
{
    fn network_service(&self) -> Option<Service>;
}

#[derive(Clone, Debug)]
pub struct HostDirEntry {
    pub name: String,
    pub is_directory: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AuthorityDomain(u64);

impl AuthorityDomain {
    pub const GUEST_BOOTFS: Self = Self(1);
    pub const HOST_SHARE_9P: Self = Self(2);
    const HOST_DEVICE_NAMESPACE: u64 = 1 << 63;

    pub const fn new(raw: u64) -> Self {
        assert!(raw != 0, "authority domain id zero is reserved");
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn host_device(device: u64) -> Self {
        Self(Self::HOST_DEVICE_NAMESPACE | device)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ObjectIdentity {
    domain: AuthorityDomain,
    local: u64,
}

impl ObjectIdentity {
    pub const fn new(domain: AuthorityDomain, local: u64) -> Self {
        Self { domain, local }
    }

    pub const fn domain(self) -> AuthorityDomain {
        self.domain
    }

    pub const fn local(self) -> u64 {
        self.local
    }
}

#[derive(Clone, Debug)]
pub struct HostMetadata {
    pub identity: ObjectIdentity,
    pub qid_path: u64,
    pub qid_type: u8,
    pub mode: u32,
    pub size: u64,
}

#[derive(Clone, Debug, Error)]
pub enum HostFsError {
    #[error("transport error: {0}")]
    Transport(IoError),
    #[error("protocol error: {0}")]
    Protocol(&'static str),
    #[error("server error code {0}")]
    Server(u32),
    #[error("invalid utf-8")]
    Utf8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostFsErrorKind {
    NoEntry,
    Exist,
    NotDirectory,
    IsDirectory,
    NotEmpty,
    ReadOnly,
    Unsupported,
    Io,
}

impl HostFsError {
    pub const fn kind(&self) -> HostFsErrorKind {
        match self {
            HostFsError::Transport(IoError::NotFound) => HostFsErrorKind::NoEntry,
            HostFsError::Transport(IoError::AlreadyExists) => HostFsErrorKind::Exist,
            HostFsError::Transport(IoError::NotDirectory) => HostFsErrorKind::NotDirectory,
            HostFsError::Transport(IoError::IsDirectory) => HostFsErrorKind::IsDirectory,
            HostFsError::Transport(IoError::DirectoryNotEmpty) => HostFsErrorKind::NotEmpty,
            HostFsError::Transport(IoError::PermissionDenied)
            | HostFsError::Transport(IoError::ReadOnly) => HostFsErrorKind::ReadOnly,
            HostFsError::Transport(IoError::Unsupported) => HostFsErrorKind::Unsupported,
            HostFsError::Transport(IoError::InvalidBufferLength { .. })
            | HostFsError::Transport(IoError::InvalidDeviceConfig(_))
            | HostFsError::Transport(IoError::OutOfBounds)
            | HostFsError::Transport(IoError::DeviceFault)
            | HostFsError::Protocol(_)
            | HostFsError::Utf8 => HostFsErrorKind::Io,
            HostFsError::Server(code) => match code {
                2 => HostFsErrorKind::NoEntry,
                17 => HostFsErrorKind::Exist,
                20 => HostFsErrorKind::NotDirectory,
                21 => HostFsErrorKind::IsDirectory,
                39 => HostFsErrorKind::NotEmpty,
                _ => HostFsErrorKind::Io,
            },
        }
    }
}

pub trait HostFileSystem: Clone + Send + 'static {
    fn stat_path<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<HostMetadata, HostFsError>> + Send + 'a;
    fn read_dir<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<Vec<HostDirEntry>, HostFsError>> + Send + 'a;
    fn read_file<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<Vec<u8>, HostFsError>> + Send + 'a;
    fn read_file_range<'a>(
        &'a self,
        path: &'a str,
        offset: u64,
        max_bytes: u32,
    ) -> impl Future<Output = Result<Vec<u8>, HostFsError>> + Send + 'a;
    fn write_file<'a>(
        &'a self,
        path: &'a str,
        offset: u64,
        bytes: &'a [u8],
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a;
    fn truncate_file<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a;
    fn set_file_size<'a>(
        &'a self,
        path: &'a str,
        size: u64,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a;
    fn set_times<'a>(
        &'a self,
        path: &'a str,
        access_nanos: Option<u64>,
        modified_nanos: Option<u64>,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a;
    fn create_file<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a;
    fn create_directory<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a;
    fn remove<'a>(
        &'a self,
        path: &'a str,
        directory: bool,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a;
    fn rename<'a>(
        &'a self,
        source: &'a str,
        destination: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a;
    fn hard_link<'a>(
        &'a self,
        source: &'a str,
        destination: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a;
    fn symlink<'a>(
        &'a self,
        target: &'a str,
        link_path: &'a str,
    ) -> impl Future<Output = Result<(), HostFsError>> + Send + 'a;
    fn read_link<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<String, HostFsError>> + Send + 'a;
}

pub trait ComponentHostFilesystemState<Service>: Clone + Send + 'static
where
    Service: HostFileSystem,
{
    fn host_filesystem_service(&self) -> Option<Service>;
    fn bootfs(&self) -> Option<crate::EmbeddedBootFs>;
}

#[cfg(test)]
mod tests {
    use super::Ipv4Address;

    #[test]
    fn ipv4_dotted_decimal_uses_caller_buffer() {
        let mut buffer = [0; 15];
        assert_eq!(
            Ipv4Address::new([192, 168, 0, 1]).write_dotted_decimal(&mut buffer),
            "192.168.0.1"
        );

        let mut buffer = [0; 15];
        assert_eq!(
            Ipv4Address::new([255, 255, 255, 255]).write_dotted_decimal(&mut buffer),
            "255.255.255.255"
        );

        let mut buffer = [0; 15];
        assert_eq!(
            Ipv4Address::new([0, 0, 0, 0]).write_dotted_decimal(&mut buffer),
            "0.0.0.0"
        );
    }
}
