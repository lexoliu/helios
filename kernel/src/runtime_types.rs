extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use core::fmt::Display;
use core::future::Future;
use core::pin::Pin;

use crate::InstanceId;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PingErrorKind {
    UnresolvedHost,
    Timeout,
    Unavailable,
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

#[derive(Clone, Debug)]
pub struct PingError {
    pub kind: PingErrorKind,
    pub detail: String,
}

impl PingError {
    pub fn from_io(error: IoError, context: impl Display) -> Self {
        Self {
            kind: PingErrorKind::from_io(error),
            detail: alloc::format!("{context}: {error}"),
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
}

#[derive(Clone, Debug)]
pub struct PingReply {
    pub address: Ipv4Address,
    pub round_trip_nanos: u64,
    pub payload_bytes: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpErrorKind {
    UnresolvedHost,
    Timeout,
    Unavailable,
    Internal,
}

impl TcpErrorKind {
    pub const fn from_io(error: IoError) -> Self {
        match error {
            IoError::Unsupported | IoError::PermissionDenied | IoError::ReadOnly => {
                TcpErrorKind::Unavailable
            }
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

#[derive(Clone, Debug)]
pub struct TcpError {
    pub kind: TcpErrorKind,
    pub detail: String,
}

impl TcpError {
    pub fn from_io(error: IoError, context: impl Display) -> Self {
        Self {
            kind: TcpErrorKind::from_io(error),
            detail: alloc::format!("{context}: {error}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpErrorKind {
    UnresolvedHost,
    Timeout,
    Unavailable,
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

#[derive(Clone, Debug)]
pub struct UdpError {
    pub kind: UdpErrorKind,
    pub detail: String,
}

impl UdpError {
    pub fn from_io(error: IoError, context: impl Display) -> Self {
        Self {
            kind: UdpErrorKind::from_io(error),
            detail: alloc::format!("{context}: {error}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct UdpDatagram {
    pub address: Ipv4Address,
    pub port: u16,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct UdpBinding<Socket> {
    pub socket: Socket,
    pub local_port: u16,
}

pub trait TcpStreamToken: Copy + Send + 'static {
    fn into_raw(self) -> u64;

    fn from_raw(raw: u64) -> Self;
}

impl TcpStreamToken for u64 {
    fn into_raw(self) -> u64 {
        self
    }

    fn from_raw(raw: u64) -> Self {
        raw
    }
}

pub trait UdpSocketToken: Copy + Send + 'static {
    fn into_raw(self) -> u64;

    fn from_raw(raw: u64) -> Self;
}

impl UdpSocketToken for u64 {
    fn into_raw(self) -> u64 {
        self
    }

    fn from_raw(raw: u64) -> Self {
        raw
    }
}

pub trait DynComponentNetworkService: Send + Sync + 'static {
    fn ping<'a>(
        &'a self,
        host: String,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<PingReply, PingError>> + Send + 'a>>;

    fn tcp_connect<'a>(
        &'a self,
        host: String,
        port: u16,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, TcpError>> + Send + 'a>>;

    fn tcp_write_all<'a>(
        &'a self,
        stream: u64,
        bytes: Vec<u8>,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), TcpError>> + Send + 'a>>;

    fn tcp_read<'a>(
        &'a self,
        stream: u64,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, TcpError>> + Send + 'a>>;

    fn tcp_close<'a>(&'a self, stream: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    fn udp_bind<'a>(
        &'a self,
        local_port: u16,
    ) -> Pin<Box<dyn Future<Output = Result<UdpBinding<u64>, UdpError>> + Send + 'a>>;

    fn udp_send<'a>(
        &'a self,
        socket: u64,
        host: String,
        port: u16,
        bytes: Vec<u8>,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, UdpError>> + Send + 'a>>;

    fn udp_receive<'a>(
        &'a self,
        socket: u64,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<UdpDatagram>, UdpError>> + Send + 'a>>;

    fn udp_close<'a>(&'a self, socket: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

#[derive(Clone)]
pub struct DynamicNetworkService {
    inner: Arc<dyn DynComponentNetworkService>,
}

impl DynamicNetworkService {
    pub fn from_service<Service>(service: Service) -> Self
    where
        Service: ComponentNetworkService + Sync,
        Service::TcpStream: TcpStreamToken,
        Service::UdpSocket: UdpSocketToken,
        for<'a> Service::PingFuture<'a>: Send,
        for<'a> Service::TcpConnectFuture<'a>: Send,
        for<'a> Service::TcpWriteFuture<'a>: Send,
        for<'a> Service::TcpReadFuture<'a>: Send,
        for<'a> Service::TcpCloseFuture<'a>: Send,
        for<'a> Service::UdpBindFuture<'a>: Send,
        for<'a> Service::UdpSendFuture<'a>: Send,
        for<'a> Service::UdpReceiveFuture<'a>: Send,
        for<'a> Service::UdpCloseFuture<'a>: Send,
    {
        Self {
            inner: Arc::new(TypedNetworkService { service }),
        }
    }
}

impl ComponentNetworkService for DynamicNetworkService {
    type TcpStream = u64;
    type UdpSocket = u64;

    type PingFuture<'a>
        = Pin<Box<dyn Future<Output = Result<PingReply, PingError>> + Send + 'a>>
    where
        Self: 'a;

    type TcpConnectFuture<'a>
        = Pin<Box<dyn Future<Output = Result<Self::TcpStream, TcpError>> + Send + 'a>>
    where
        Self: 'a;

    type TcpWriteFuture<'a>
        = Pin<Box<dyn Future<Output = Result<(), TcpError>> + Send + 'a>>
    where
        Self: 'a;

    type TcpReadFuture<'a>
        = Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, TcpError>> + Send + 'a>>
    where
        Self: 'a;

    type TcpCloseFuture<'a>
        = Pin<Box<dyn Future<Output = ()> + Send + 'a>>
    where
        Self: 'a;

    type UdpBindFuture<'a>
        = Pin<Box<dyn Future<Output = Result<UdpBinding<Self::UdpSocket>, UdpError>> + Send + 'a>>
    where
        Self: 'a;

    type UdpSendFuture<'a>
        = Pin<Box<dyn Future<Output = Result<u64, UdpError>> + Send + 'a>>
    where
        Self: 'a;

    type UdpReceiveFuture<'a>
        = Pin<Box<dyn Future<Output = Result<Option<UdpDatagram>, UdpError>> + Send + 'a>>
    where
        Self: 'a;

    type UdpCloseFuture<'a>
        = Pin<Box<dyn Future<Output = ()> + Send + 'a>>
    where
        Self: 'a;

    fn ping(&self, host: &str, timeout_nanos: u64) -> Self::PingFuture<'_> {
        self.inner.ping(String::from(host), timeout_nanos)
    }

    fn tcp_connect(&self, host: &str, port: u16, timeout_nanos: u64) -> Self::TcpConnectFuture<'_> {
        self.inner
            .tcp_connect(String::from(host), port, timeout_nanos)
    }

    fn tcp_write_all(
        &self,
        stream: Self::TcpStream,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Self::TcpWriteFuture<'_> {
        self.inner
            .tcp_write_all(stream, bytes.to_vec(), timeout_nanos)
    }

    fn tcp_read(
        &self,
        stream: Self::TcpStream,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Self::TcpReadFuture<'_> {
        self.inner.tcp_read(stream, max_bytes, timeout_nanos)
    }

    fn tcp_close(&self, stream: Self::TcpStream) -> Self::TcpCloseFuture<'_> {
        self.inner.tcp_close(stream)
    }

    fn udp_bind(&self, local_port: u16) -> Self::UdpBindFuture<'_> {
        self.inner.udp_bind(local_port)
    }

    fn udp_send(
        &self,
        socket: Self::UdpSocket,
        host: &str,
        port: u16,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Self::UdpSendFuture<'_> {
        self.inner.udp_send(
            socket,
            String::from(host),
            port,
            bytes.to_vec(),
            timeout_nanos,
        )
    }

    fn udp_receive(
        &self,
        socket: Self::UdpSocket,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Self::UdpReceiveFuture<'_> {
        self.inner.udp_receive(socket, max_bytes, timeout_nanos)
    }

    fn udp_close(&self, socket: Self::UdpSocket) -> Self::UdpCloseFuture<'_> {
        self.inner.udp_close(socket)
    }
}

struct TypedNetworkService<Service> {
    service: Service,
}

impl<Service> DynComponentNetworkService for TypedNetworkService<Service>
where
    Service: ComponentNetworkService + Sync,
    Service::TcpStream: TcpStreamToken,
    Service::UdpSocket: UdpSocketToken,
    for<'a> Service::PingFuture<'a>: Send,
    for<'a> Service::TcpConnectFuture<'a>: Send,
    for<'a> Service::TcpWriteFuture<'a>: Send,
    for<'a> Service::TcpReadFuture<'a>: Send,
    for<'a> Service::TcpCloseFuture<'a>: Send,
    for<'a> Service::UdpBindFuture<'a>: Send,
    for<'a> Service::UdpSendFuture<'a>: Send,
    for<'a> Service::UdpReceiveFuture<'a>: Send,
    for<'a> Service::UdpCloseFuture<'a>: Send,
{
    fn ping<'a>(
        &'a self,
        host: String,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<PingReply, PingError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move { service.ping(&host, timeout_nanos).await })
    }

    fn tcp_connect<'a>(
        &'a self,
        host: String,
        port: u16,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, TcpError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            let stream = service.tcp_connect(&host, port, timeout_nanos).await?;
            Ok(stream.into_raw())
        })
    }

    fn tcp_write_all<'a>(
        &'a self,
        stream: u64,
        bytes: Vec<u8>,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), TcpError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            service
                .tcp_write_all(
                    <Service::TcpStream as TcpStreamToken>::from_raw(stream),
                    &bytes,
                    timeout_nanos,
                )
                .await
        })
    }

    fn tcp_read<'a>(
        &'a self,
        stream: u64,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, TcpError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            service
                .tcp_read(
                    <Service::TcpStream as TcpStreamToken>::from_raw(stream),
                    max_bytes,
                    timeout_nanos,
                )
                .await
        })
    }

    fn tcp_close<'a>(&'a self, stream: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            service
                .tcp_close(<Service::TcpStream as TcpStreamToken>::from_raw(stream))
                .await
        })
    }

    fn udp_bind<'a>(
        &'a self,
        local_port: u16,
    ) -> Pin<Box<dyn Future<Output = Result<UdpBinding<u64>, UdpError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            let binding = service.udp_bind(local_port).await?;
            Ok(UdpBinding {
                socket: binding.socket.into_raw(),
                local_port: binding.local_port,
            })
        })
    }

    fn udp_send<'a>(
        &'a self,
        socket: u64,
        host: String,
        port: u16,
        bytes: Vec<u8>,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, UdpError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            service
                .udp_send(
                    <Service::UdpSocket as UdpSocketToken>::from_raw(socket),
                    &host,
                    port,
                    &bytes,
                    timeout_nanos,
                )
                .await
        })
    }

    fn udp_receive<'a>(
        &'a self,
        socket: u64,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<UdpDatagram>, UdpError>> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            service
                .udp_receive(
                    <Service::UdpSocket as UdpSocketToken>::from_raw(socket),
                    max_bytes,
                    timeout_nanos,
                )
                .await
        })
    }

    fn udp_close<'a>(&'a self, socket: u64) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let service = self.service.clone();
        Box::pin(async move {
            service
                .udp_close(<Service::UdpSocket as UdpSocketToken>::from_raw(socket))
                .await
        })
    }
}

pub trait ComponentNetworkService: Clone + Send + 'static {
    type TcpStream: Copy + Send + 'static;
    type UdpSocket: Copy + Send + 'static;

    type PingFuture<'a>: Future<Output = Result<PingReply, PingError>> + 'a
    where
        Self: 'a;

    type TcpConnectFuture<'a>: Future<Output = Result<Self::TcpStream, TcpError>> + 'a
    where
        Self: 'a;

    type TcpWriteFuture<'a>: Future<Output = Result<(), TcpError>> + 'a
    where
        Self: 'a;

    type TcpReadFuture<'a>: Future<Output = Result<Option<Vec<u8>>, TcpError>> + 'a
    where
        Self: 'a;

    type TcpCloseFuture<'a>: Future<Output = ()> + 'a
    where
        Self: 'a;

    type UdpBindFuture<'a>: Future<Output = Result<UdpBinding<Self::UdpSocket>, UdpError>> + 'a
    where
        Self: 'a;

    type UdpSendFuture<'a>: Future<Output = Result<u64, UdpError>> + 'a
    where
        Self: 'a;

    type UdpReceiveFuture<'a>: Future<Output = Result<Option<UdpDatagram>, UdpError>> + 'a
    where
        Self: 'a;

    type UdpCloseFuture<'a>: Future<Output = ()> + 'a
    where
        Self: 'a;

    fn ping(&self, host: &str, timeout_nanos: u64) -> Self::PingFuture<'_>;

    fn tcp_connect(&self, host: &str, port: u16, timeout_nanos: u64) -> Self::TcpConnectFuture<'_>;

    fn tcp_write_all(
        &self,
        stream: Self::TcpStream,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Self::TcpWriteFuture<'_>;

    fn tcp_read(
        &self,
        stream: Self::TcpStream,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Self::TcpReadFuture<'_>;

    fn tcp_close(&self, stream: Self::TcpStream) -> Self::TcpCloseFuture<'_>;

    fn udp_bind(&self, local_port: u16) -> Self::UdpBindFuture<'_>;

    fn udp_send(
        &self,
        socket: Self::UdpSocket,
        host: &str,
        port: u16,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Self::UdpSendFuture<'_>;

    fn udp_receive(
        &self,
        socket: Self::UdpSocket,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Self::UdpReceiveFuture<'_>;

    fn udp_close(&self, socket: Self::UdpSocket) -> Self::UdpCloseFuture<'_>;
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

#[derive(Clone, Debug)]
pub struct HostMetadata {
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
    type StatFuture<'a>: Future<Output = Result<HostMetadata, HostFsError>> + Send + 'a
    where
        Self: 'a;

    type ReadDirFuture<'a>: Future<Output = Result<Vec<HostDirEntry>, HostFsError>> + Send + 'a
    where
        Self: 'a;

    type ReadFileFuture<'a>: Future<Output = Result<Vec<u8>, HostFsError>> + Send + 'a
    where
        Self: 'a;

    type ReadFileRangeFuture<'a>: Future<Output = Result<Vec<u8>, HostFsError>> + Send + 'a
    where
        Self: 'a;

    type WriteFileFuture<'a>: Future<Output = Result<(), HostFsError>> + Send + 'a
    where
        Self: 'a;

    type TruncateFileFuture<'a>: Future<Output = Result<(), HostFsError>> + Send + 'a
    where
        Self: 'a;

    type CreateFileFuture<'a>: Future<Output = Result<(), HostFsError>> + Send + 'a
    where
        Self: 'a;

    type CreateDirectoryFuture<'a>: Future<Output = Result<(), HostFsError>> + Send + 'a
    where
        Self: 'a;

    type RemoveFuture<'a>: Future<Output = Result<(), HostFsError>> + Send + 'a
    where
        Self: 'a;

    type RenameFuture<'a>: Future<Output = Result<(), HostFsError>> + Send + 'a
    where
        Self: 'a;

    fn stat_path(&self, path: &str) -> Self::StatFuture<'_>;
    fn read_dir(&self, path: &str) -> Self::ReadDirFuture<'_>;
    fn read_file(&self, path: &str) -> Self::ReadFileFuture<'_>;
    fn read_file_range(
        &self,
        path: &str,
        offset: u64,
        max_bytes: u32,
    ) -> Self::ReadFileRangeFuture<'_>;
    fn write_file(&self, path: &str, offset: u64, bytes: &[u8]) -> Self::WriteFileFuture<'_>;
    fn truncate_file(&self, path: &str) -> Self::TruncateFileFuture<'_>;
    fn create_file(&self, path: &str) -> Self::CreateFileFuture<'_>;
    fn create_directory(&self, path: &str) -> Self::CreateDirectoryFuture<'_>;
    fn remove(&self, path: &str, directory: bool) -> Self::RemoveFuture<'_>;
    fn rename(&self, source: &str, destination: &str) -> Self::RenameFuture<'_>;
}

pub trait ComponentHostFilesystemState<Service>: Clone + Send + 'static
where
    Service: HostFileSystem,
{
    fn host_filesystem_service(&self) -> Option<Service>;
}
