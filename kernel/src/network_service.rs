extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use core::future::Future;
use core::num::NonZeroU32;
use core::task::Poll;
use core::time::Duration;

use helios_hal::cpu::Cpu;
use helios_hal::io::IoError;
use smoltcp::iface::{Config as InterfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, dns, icmp, tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    DnsQueryType, EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr,
    IpEndpoint, IpListenEndpoint, Ipv4Address, Ipv4Cidr,
};

use crate::{
    ComponentNetworkService, ComponentRuntimeState, DnsError, DnsErrorKind,
    Ipv4Address as KernelIpv4Address, NetworkErrorDetail, Notify, PingError, PingErrorKind,
    PingReply, TcpError, TcpErrorKind, Timer, UdpBinding, UdpDatagram, UdpError, UdpErrorKind,
};

const DHCP_PARAMETERS: &[u8] = &[1, 3, 6];
const ICMP_IDENTIFIER: u16 = 0x4845;
const ICMP_PAYLOAD: &[u8] = b"helios";
const ICMP_BUFFER_BYTES: usize = 512;
const ICMP_BUFFER_PACKETS: usize = 4;
const TCP_BUFFER_BYTES: usize = 8 * 1024;
const UDP_BUFFER_BYTES: usize = 8 * 1024;
const UDP_BUFFER_PACKETS: usize = 16;
const EPHEMERAL_PORT_START: u16 = 49_152;
const EPHEMERAL_PORT_END: u16 = 65_535;

pub trait NetworkDevice: Clone + Send + Sync + 'static {
    fn mac_address(&self) -> [u8; 6];

    fn max_frame_len(&self) -> usize;

    fn try_receive(&self) -> impl Future<Output = Result<Option<Box<[u8]>>, IoError>> + Send + '_;

    fn transmit<'a>(
        &'a self,
        frame: &'a [u8],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a;

    fn wait_for_event(&self) -> impl Future<Output = ()> + Send + '_;
}

#[derive(Clone)]
pub struct NetworkService<CpuImpl, Runtime, Device>
where
    CpuImpl: Cpu + Clone,
    Runtime: ComponentRuntimeState + Sync,
    Device: NetworkDevice,
{
    inner: Arc<NetworkServiceInner<CpuImpl, Runtime, Device>>,
}

struct NetworkServiceInner<CpuImpl, Runtime, Device>
where
    CpuImpl: Cpu + Clone,
    Runtime: ComponentRuntimeState + Sync,
    Device: NetworkDevice,
{
    cpu: CpuImpl,
    runtime_state: Runtime,
    timer: Timer<CpuImpl>,
    device: Device,
    state: crate::Mutex<NetworkState>,
    requests: ConcurrentQueue<NetworkRequest>,
    request_ready: Notify,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TcpStreamId(NonZeroU32);

#[cfg(feature = "wasmtime-runtime")]
impl crate::ComponentHostTcpStreamToken for TcpStreamId {
    fn into_raw(self) -> u64 {
        u64::from(self.0.get())
    }

    fn from_raw(raw: u64) -> Self {
        let raw = u32::try_from(raw)
            .unwrap_or_else(|_| panic!("tcp stream handle {raw} does not fit in u32"));
        let raw =
            NonZeroU32::new(raw).unwrap_or_else(|| panic!("tcp stream handle must be non-zero"));
        Self(raw)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct UdpSocketId(NonZeroU32);

#[cfg(feature = "wasmtime-runtime")]
impl crate::ComponentHostUdpSocketToken for UdpSocketId {
    fn into_raw(self) -> u64 {
        u64::from(self.0.get())
    }

    fn from_raw(raw: u64) -> Self {
        let raw = u32::try_from(raw)
            .unwrap_or_else(|_| panic!("udp socket handle {raw} does not fit in u32"));
        let raw =
            NonZeroU32::new(raw).unwrap_or_else(|| panic!("udp socket handle must be non-zero"));
        Self(raw)
    }
}

enum NetworkRequest {
    Ping(PingRequest),
    DnsResolve(DnsResolveRequest),
    TcpConnect(TcpConnectRequest),
    TcpWrite(TcpWriteRequest),
    TcpRead(TcpReadRequest),
    TcpClose(TcpCloseRequest),
    UdpBind(UdpBindRequest),
    UdpSend(UdpSendRequest),
    UdpReceive(UdpReceiveRequest),
    UdpClose(UdpCloseRequest),
}

struct PingRequest {
    host: String,
    timeout_nanos: u64,
    response: RequestResponse<Result<PingReply, PingError>>,
}

struct DnsResolveRequest {
    host: String,
    timeout_nanos: u64,
    response: RequestResponse<Result<Vec<KernelIpv4Address>, DnsError>>,
}

struct TcpConnectRequest {
    host: String,
    port: u16,
    timeout_nanos: u64,
    response: RequestResponse<Result<TcpStreamId, TcpError>>,
}

struct TcpWriteRequest {
    stream: TcpStreamId,
    bytes: Vec<u8>,
    timeout_nanos: u64,
    response: RequestResponse<Result<(), TcpError>>,
}

struct TcpReadRequest {
    stream: TcpStreamId,
    max_bytes: u32,
    timeout_nanos: u64,
    response: RequestResponse<Result<Option<Vec<u8>>, TcpError>>,
}

struct TcpCloseRequest {
    stream: TcpStreamId,
    response: RequestResponse<()>,
}

struct UdpBindRequest {
    local_port: u16,
    response: RequestResponse<Result<UdpBinding<UdpSocketId>, UdpError>>,
}

struct UdpSendRequest {
    socket: UdpSocketId,
    host: String,
    port: u16,
    bytes: Vec<u8>,
    timeout_nanos: u64,
    response: RequestResponse<Result<u64, UdpError>>,
}

struct UdpReceiveRequest {
    socket: UdpSocketId,
    max_bytes: u32,
    timeout_nanos: u64,
    response: RequestResponse<Result<Option<UdpDatagram>, UdpError>>,
}

struct UdpCloseRequest {
    socket: UdpSocketId,
    response: RequestResponse<()>,
}

struct RequestResponse<T> {
    inner: Arc<RequestResponseInner<T>>,
}

struct RequestResponseInner<T> {
    result: crate::Mutex<Option<T>>,
    ready: Notify,
}

impl<T> Clone for RequestResponse<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
impl<T> RequestResponse<T> {
    fn new() -> Self {
        Self {
            inner: Arc::new(RequestResponseInner {
                result: crate::Mutex::new(None),
                ready: Notify::new(),
            }),
        }
    }

    async fn complete(&self, result: T) {
        let mut slot = self.inner.result.lock().await;
        assert!(slot.is_none(), "network request completed more than once");
        *slot = Some(result);
        self.inner.ready.notify_all();
    }

    async fn wait(&self) -> T {
        loop {
            if let Some(result) = self.inner.result.lock().await.take() {
                return result;
            }

            self.inner.ready.notified().await;
        }
    }
}

struct NetworkState {
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp_handle: SocketHandle,
    dns_handle: SocketHandle,
    icmp_handle: SocketHandle,
    inbound: VecDeque<Box<[u8]>>,
    outbound: VecDeque<Vec<u8>>,
    ipv4_address: Option<Ipv4Cidr>,
    dns_servers: Vec<IpAddress>,
    next_echo_sequence: u16,
    next_tcp_local_port: u16,
    next_udp_local_port: u16,
    tcp_streams: Vec<Option<TcpStreamState>>,
    udp_sockets: Vec<Option<UdpSocketState>>,
    max_frame_len: usize,
}

struct TcpStreamState {
    handle: SocketHandle,
}

struct UdpSocketState {
    handle: SocketHandle,
}

struct QueueDevice<'a> {
    inbound: &'a mut VecDeque<Box<[u8]>>,
    outbound: &'a mut VecDeque<Vec<u8>>,
    max_frame_len: usize,
}

struct QueueRxToken {
    frame: Box<[u8]>,
}

struct QueueTxToken<'a> {
    outbound: &'a mut VecDeque<Vec<u8>>,
    max_frame_len: usize,
}

enum TcpConnectProgress {
    Pending,
    Connected,
}

enum TcpReadProgress {
    Pending,
    Data(Vec<u8>),
    Eof,
}

enum UdpReceiveProgress {
    Pending,
    Data(UdpDatagram),
}

fn map_ipv4_address(address: Ipv4Address) -> KernelIpv4Address {
    KernelIpv4Address::new(address.octets())
}

impl<CpuImpl, Runtime, DeviceImpl> NetworkService<CpuImpl, Runtime, DeviceImpl>
where
    CpuImpl: Cpu + Clone,
    Runtime: ComponentRuntimeState + Sync,
    DeviceImpl: NetworkDevice,
{
    pub fn new(
        cpu: CpuImpl,
        runtime_state: Runtime,
        timer: Timer<CpuImpl>,
        device: DeviceImpl,
    ) -> Self {
        let random_seed = cpu.now().ticks();
        Self {
            inner: Arc::new(NetworkServiceInner {
                cpu,
                runtime_state,
                timer,
                device: device.clone(),
                state: crate::Mutex::new(NetworkState::new(
                    device.mac_address(),
                    device.max_frame_len(),
                    random_seed,
                )),
                requests: ConcurrentQueue::unbounded(),
                request_ready: Notify::new(),
            }),
        }
    }

    pub async fn ping(&self, host: &str, timeout_nanos: u64) -> Result<PingReply, PingError> {
        let response = RequestResponse::new();
        self.enqueue_request(NetworkRequest::Ping(PingRequest {
            host: host.to_owned(),
            timeout_nanos,
            response: response.clone(),
        }));
        response.wait().await
    }

    pub async fn dns_resolve(
        &self,
        host: &str,
        timeout_nanos: u64,
    ) -> Result<Vec<KernelIpv4Address>, DnsError> {
        let response = RequestResponse::new();
        self.enqueue_request(NetworkRequest::DnsResolve(DnsResolveRequest {
            host: host.to_owned(),
            timeout_nanos,
            response: response.clone(),
        }));
        response.wait().await
    }

    pub async fn tcp_connect(
        &self,
        host: &str,
        port: u16,
        timeout_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        let response = RequestResponse::new();
        self.enqueue_request(NetworkRequest::TcpConnect(TcpConnectRequest {
            host: host.to_owned(),
            port,
            timeout_nanos,
            response: response.clone(),
        }));
        response.wait().await
    }

    pub async fn tcp_write_all(
        &self,
        stream: TcpStreamId,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<(), TcpError> {
        let response = RequestResponse::new();
        self.enqueue_request(NetworkRequest::TcpWrite(TcpWriteRequest {
            stream,
            bytes: bytes.to_vec(),
            timeout_nanos,
            response: response.clone(),
        }));
        response.wait().await
    }

    pub async fn tcp_read(
        &self,
        stream: TcpStreamId,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Result<Option<Vec<u8>>, TcpError> {
        let response = RequestResponse::new();
        self.enqueue_request(NetworkRequest::TcpRead(TcpReadRequest {
            stream,
            max_bytes,
            timeout_nanos,
            response: response.clone(),
        }));
        response.wait().await
    }

    pub async fn tcp_close(&self, stream: TcpStreamId) {
        let response = RequestResponse::new();
        self.enqueue_request(NetworkRequest::TcpClose(TcpCloseRequest {
            stream,
            response: response.clone(),
        }));
        response.wait().await;
    }

    pub async fn udp_bind(&self, local_port: u16) -> Result<UdpBinding<UdpSocketId>, UdpError> {
        let response = RequestResponse::new();
        self.enqueue_request(NetworkRequest::UdpBind(UdpBindRequest {
            local_port,
            response: response.clone(),
        }));
        response.wait().await
    }

    pub async fn udp_send(
        &self,
        socket: UdpSocketId,
        host: &str,
        port: u16,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<u64, UdpError> {
        let response = RequestResponse::new();
        self.enqueue_request(NetworkRequest::UdpSend(UdpSendRequest {
            socket,
            host: host.to_owned(),
            port,
            bytes: bytes.to_vec(),
            timeout_nanos,
            response: response.clone(),
        }));
        response.wait().await
    }

    pub async fn udp_receive(
        &self,
        socket: UdpSocketId,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Result<Option<UdpDatagram>, UdpError> {
        let response = RequestResponse::new();
        self.enqueue_request(NetworkRequest::UdpReceive(UdpReceiveRequest {
            socket,
            max_bytes,
            timeout_nanos,
            response: response.clone(),
        }));
        response.wait().await
    }

    pub async fn udp_close(&self, socket: UdpSocketId) {
        let response = RequestResponse::new();
        self.enqueue_request(NetworkRequest::UdpClose(UdpCloseRequest {
            socket,
            response: response.clone(),
        }));
        response.wait().await;
    }

    fn enqueue_request(&self, request: NetworkRequest) {
        match self.inner.requests.push(request) {
            Ok(()) => self.inner.request_ready.notify_one(),
            Err(PushError::Full(_)) => unreachable!("unbounded network queue reported full"),
            Err(PushError::Closed(_)) => panic!("network request queue was closed unexpectedly"),
        }
    }

    pub async fn run_requests(&self) {
        loop {
            let request = self.next_request().await;
            match request {
                NetworkRequest::Ping(request) => {
                    let result = self
                        .execute_ping(&request.host, request.timeout_nanos)
                        .await;
                    request.response.complete(result).await;
                }
                NetworkRequest::DnsResolve(request) => {
                    let result = self
                        .execute_dns_resolve(&request.host, request.timeout_nanos)
                        .await;
                    request.response.complete(result).await;
                }
                NetworkRequest::TcpConnect(request) => {
                    let result = self
                        .execute_tcp_connect(&request.host, request.port, request.timeout_nanos)
                        .await;
                    request.response.complete(result).await;
                }
                NetworkRequest::TcpWrite(request) => {
                    let result = self
                        .execute_tcp_write_all(
                            request.stream,
                            &request.bytes,
                            request.timeout_nanos,
                        )
                        .await;
                    request.response.complete(result).await;
                }
                NetworkRequest::TcpRead(request) => {
                    let result = self
                        .execute_tcp_read(request.stream, request.max_bytes, request.timeout_nanos)
                        .await;
                    request.response.complete(result).await;
                }
                NetworkRequest::TcpClose(request) => {
                    self.inner
                        .state
                        .lock()
                        .await
                        .remove_tcp_stream(request.stream);
                    request.response.complete(()).await;
                }
                NetworkRequest::UdpBind(request) => {
                    let result = self.execute_udp_bind(request.local_port).await;
                    request.response.complete(result).await;
                }
                NetworkRequest::UdpSend(request) => {
                    let result = self
                        .execute_udp_send(
                            request.socket,
                            &request.host,
                            request.port,
                            &request.bytes,
                            request.timeout_nanos,
                        )
                        .await;
                    request.response.complete(result).await;
                }
                NetworkRequest::UdpReceive(request) => {
                    let result = self
                        .execute_udp_receive(
                            request.socket,
                            request.max_bytes,
                            request.timeout_nanos,
                        )
                        .await;
                    request.response.complete(result).await;
                }
                NetworkRequest::UdpClose(request) => {
                    self.inner
                        .state
                        .lock()
                        .await
                        .remove_udp_socket(request.socket);
                    request.response.complete(()).await;
                }
            }
        }
    }

    async fn next_request(&self) -> NetworkRequest {
        loop {
            match self.inner.requests.pop() {
                Ok(request) => return request,
                Err(PopError::Empty) => self.inner.request_ready.notified().await,
                Err(PopError::Closed) => panic!("network request queue was closed unexpectedly"),
            }
        }
    }

    async fn execute_ping(&self, host: &str, timeout_nanos: u64) -> Result<PingReply, PingError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        self.wait_for_ipv4_ping(deadline_nanos).await?;
        let destination = self.resolve_host_ping(host, deadline_nanos).await?;
        let (sequence, sent_at_nanos) = {
            let mut state = self.inner.state.lock().await;
            let sequence = state.start_ping(destination)?;
            (sequence, self.now_nanos())
        };

        let payload_bytes = self
            .wait_until_ping(
                deadline_nanos,
                PingError {
                    kind: PingErrorKind::Timeout,
                    detail: NetworkErrorDetail::IcmpEchoTimeout,
                },
                move |state| Ok(state.take_ping_reply(destination, sequence)),
            )
            .await?;
        Ok(PingReply {
            address: map_ipv4_address(destination),
            round_trip_nanos: self.now_nanos().saturating_sub(sent_at_nanos),
            payload_bytes,
        })
    }

    async fn execute_dns_resolve(
        &self,
        host: &str,
        timeout_nanos: u64,
    ) -> Result<Vec<KernelIpv4Address>, DnsError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        self.wait_for_ipv4_dns(deadline_nanos).await?;
        self.resolve_host_dns(host, deadline_nanos).await
    }

    async fn execute_tcp_connect(
        &self,
        host: &str,
        port: u16,
        timeout_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        self.wait_for_ipv4_tcp(deadline_nanos).await?;
        let destination = self.resolve_host_tcp(host, deadline_nanos).await?;
        let stream = {
            let mut state = self.inner.state.lock().await;
            state.start_tcp_connect(destination, port)?
        };

        loop {
            self.drive_tcp().await?;
            let now_nanos = self.now_nanos();
            let next_wait = {
                let mut state = self.inner.state.lock().await;
                match state.poll_tcp_connect(stream) {
                    Ok(TcpConnectProgress::Connected) => return Ok(stream),
                    Ok(TcpConnectProgress::Pending) => {
                        if now_nanos >= deadline_nanos {
                            state.remove_tcp_stream(stream);
                            return Err(TcpError {
                                kind: TcpErrorKind::Timeout,
                                detail: NetworkErrorDetail::TcpConnectTimeout,
                            });
                        }
                        state.next_wait_duration(now_nanos, deadline_nanos)
                    }
                    Err(error) => {
                        state.remove_tcp_stream(stream);
                        return Err(error);
                    }
                }
            };
            self.wait_for_progress(next_wait).await;
        }
    }

    async fn execute_tcp_write_all(
        &self,
        stream: TcpStreamId,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<(), TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        let mut offset = 0usize;
        while offset < bytes.len() {
            self.drive_tcp().await?;
            let now_nanos = self.now_nanos();
            let next_wait = {
                let mut state = self.inner.state.lock().await;
                match state.try_write_tcp(stream, &bytes[offset..])? {
                    Some(written) => {
                        assert!(written != 0, "tcp write reported zero-byte progress");
                        offset = offset
                            .checked_add(written)
                            .expect("tcp write offset overflowed usize");
                        continue;
                    }
                    None => {
                        if now_nanos >= deadline_nanos {
                            return Err(TcpError {
                                kind: TcpErrorKind::Timeout,
                                detail: NetworkErrorDetail::TcpWriteTimeout,
                            });
                        }
                        state.next_wait_duration(now_nanos, deadline_nanos)
                    }
                }
            };
            self.wait_for_progress(next_wait).await;
        }
        Ok(())
    }

    async fn execute_tcp_read(
        &self,
        stream: TcpStreamId,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Result<Option<Vec<u8>>, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        loop {
            self.drive_tcp().await?;
            let now_nanos = self.now_nanos();
            let next_wait = {
                let mut state = self.inner.state.lock().await;
                match state.poll_tcp_read(stream, max_bytes as usize)? {
                    TcpReadProgress::Data(bytes) => return Ok(Some(bytes)),
                    TcpReadProgress::Eof => return Ok(None),
                    TcpReadProgress::Pending => {
                        if now_nanos >= deadline_nanos {
                            return Err(TcpError {
                                kind: TcpErrorKind::Timeout,
                                detail: NetworkErrorDetail::TcpReadTimeout,
                            });
                        }
                        state.next_wait_duration(now_nanos, deadline_nanos)
                    }
                }
            };
            self.wait_for_progress(next_wait).await;
        }
    }

    async fn execute_udp_bind(&self, local_port: u16) -> Result<UdpBinding<UdpSocketId>, UdpError> {
        let mut state = self.inner.state.lock().await;
        state.start_udp_bind(local_port)
    }

    async fn execute_udp_send(
        &self,
        socket: UdpSocketId,
        host: &str,
        port: u16,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<u64, UdpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        self.wait_for_ipv4_udp(deadline_nanos).await?;
        let destination = self.resolve_host_udp(host, deadline_nanos).await?;
        loop {
            self.drive_udp().await?;
            let now_nanos = self.now_nanos();
            let next_wait = {
                let mut state = self.inner.state.lock().await;
                match state.try_send_udp(socket, destination, port, bytes)? {
                    Some(written) => {
                        return Ok(u64::try_from(written).unwrap_or_else(|_| {
                            panic!("udp write length {} exceeds u64", written)
                        }));
                    }
                    None => {
                        if now_nanos >= deadline_nanos {
                            return Err(UdpError {
                                kind: UdpErrorKind::Timeout,
                                detail: NetworkErrorDetail::UdpSendTimeout,
                            });
                        }
                        state.next_wait_duration(now_nanos, deadline_nanos)
                    }
                }
            };
            self.wait_for_progress(next_wait).await;
        }
    }

    async fn execute_udp_receive(
        &self,
        socket: UdpSocketId,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Result<Option<UdpDatagram>, UdpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        loop {
            self.drive_udp().await?;
            let now_nanos = self.now_nanos();
            let next_wait = {
                let mut state = self.inner.state.lock().await;
                match state.poll_udp_receive(socket, max_bytes as usize)? {
                    UdpReceiveProgress::Data(datagram) => return Ok(Some(datagram)),
                    UdpReceiveProgress::Pending => {
                        if now_nanos >= deadline_nanos {
                            return Err(UdpError {
                                kind: UdpErrorKind::Timeout,
                                detail: NetworkErrorDetail::UdpReceiveTimeout,
                            });
                        }
                        state.next_wait_duration(now_nanos, deadline_nanos)
                    }
                }
            };
            self.wait_for_progress(next_wait).await;
        }
    }

    async fn wait_for_ipv4_ping(&self, deadline_nanos: u64) -> Result<(), PingError> {
        self.wait_until_ping(
            deadline_nanos,
            PingError {
                kind: PingErrorKind::Timeout,
                detail: NetworkErrorDetail::NetworkConfigurationTimeout,
            },
            |state| Ok(state.is_configured().then_some(())),
        )
        .await
    }

    async fn wait_for_ipv4_dns(&self, deadline_nanos: u64) -> Result<(), DnsError> {
        loop {
            self.drive_dns().await?;
            let now_nanos = self.now_nanos();
            let next_wait = {
                let mut state = self.inner.state.lock().await;
                if state.is_configured() {
                    return Ok(());
                }
                if now_nanos >= deadline_nanos {
                    return Err(DnsError {
                        kind: DnsErrorKind::Timeout,
                        detail: NetworkErrorDetail::NetworkConfigurationTimeout,
                    });
                }
                state.next_wait_duration(now_nanos, deadline_nanos)
            };
            self.wait_for_progress(next_wait).await;
        }
    }

    async fn wait_for_ipv4_tcp(&self, deadline_nanos: u64) -> Result<(), TcpError> {
        loop {
            self.drive_tcp().await?;
            let now_nanos = self.now_nanos();
            let next_wait = {
                let mut state = self.inner.state.lock().await;
                if state.is_configured() {
                    return Ok(());
                }
                if now_nanos >= deadline_nanos {
                    return Err(TcpError {
                        kind: TcpErrorKind::Timeout,
                        detail: NetworkErrorDetail::NetworkConfigurationTimeout,
                    });
                }
                state.next_wait_duration(now_nanos, deadline_nanos)
            };
            self.wait_for_progress(next_wait).await;
        }
    }

    async fn wait_for_ipv4_udp(&self, deadline_nanos: u64) -> Result<(), UdpError> {
        loop {
            self.drive_udp().await?;
            let now_nanos = self.now_nanos();
            let next_wait = {
                let mut state = self.inner.state.lock().await;
                if state.is_configured() {
                    return Ok(());
                }
                if now_nanos >= deadline_nanos {
                    return Err(UdpError {
                        kind: UdpErrorKind::Timeout,
                        detail: NetworkErrorDetail::NetworkConfigurationTimeout,
                    });
                }
                state.next_wait_duration(now_nanos, deadline_nanos)
            };
            self.wait_for_progress(next_wait).await;
        }
    }

    async fn resolve_host_ping(
        &self,
        host: &str,
        deadline_nanos: u64,
    ) -> Result<Ipv4Address, PingError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(address);
        }

        let query = {
            let mut state = self.inner.state.lock().await;
            state.start_dns_query(host)?
        };
        let result = self
            .wait_until_ping(
                deadline_nanos,
                PingError {
                    kind: PingErrorKind::Timeout,
                    detail: NetworkErrorDetail::DnsLookupTimeout,
                },
                move |state| state.take_dns_result(query),
            )
            .await;
        if matches!(
            result,
            Err(PingError {
                kind: PingErrorKind::Timeout,
                ..
            })
        ) {
            let mut state = self.inner.state.lock().await;
            state.cancel_dns_query(query);
        }
        result
    }

    async fn resolve_host_dns(
        &self,
        host: &str,
        deadline_nanos: u64,
    ) -> Result<Vec<KernelIpv4Address>, DnsError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(vec![map_ipv4_address(address)]);
        }

        let query = {
            let mut state = self.inner.state.lock().await;
            state.start_dns_query_dns(host)?
        };
        loop {
            self.drive_dns().await?;
            let now_nanos = self.now_nanos();
            let next_wait = {
                let mut state = self.inner.state.lock().await;
                match state.take_dns_addresses_dns(query)? {
                    Some(addresses) => return Ok(addresses),
                    None => {
                        if now_nanos >= deadline_nanos {
                            state.cancel_dns_query(query);
                            return Err(DnsError {
                                kind: DnsErrorKind::Timeout,
                                detail: NetworkErrorDetail::DnsLookupTimeout,
                            });
                        }
                        state.next_wait_duration(now_nanos, deadline_nanos)
                    }
                }
            };
            self.wait_for_progress(next_wait).await;
        }
    }

    async fn resolve_host_tcp(
        &self,
        host: &str,
        deadline_nanos: u64,
    ) -> Result<Ipv4Address, TcpError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(address);
        }

        let query = {
            let mut state = self.inner.state.lock().await;
            state.start_dns_query_tcp(host)?
        };
        loop {
            self.drive_tcp().await?;
            let now_nanos = self.now_nanos();
            let next_wait = {
                let mut state = self.inner.state.lock().await;
                match state.take_dns_result_tcp(query)? {
                    Some(address) => return Ok(address),
                    None => {
                        if now_nanos >= deadline_nanos {
                            state.cancel_dns_query(query);
                            return Err(TcpError {
                                kind: TcpErrorKind::Timeout,
                                detail: NetworkErrorDetail::DnsLookupTimeout,
                            });
                        }
                        state.next_wait_duration(now_nanos, deadline_nanos)
                    }
                }
            };
            self.wait_for_progress(next_wait).await;
        }
    }

    async fn resolve_host_udp(
        &self,
        host: &str,
        deadline_nanos: u64,
    ) -> Result<Ipv4Address, UdpError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(address);
        }

        let query = {
            let mut state = self.inner.state.lock().await;
            state.start_dns_query_udp(host)?
        };
        loop {
            self.drive_udp().await?;
            let now_nanos = self.now_nanos();
            let next_wait = {
                let mut state = self.inner.state.lock().await;
                match state.take_dns_result_udp(query)? {
                    Some(address) => return Ok(address),
                    None => {
                        if now_nanos >= deadline_nanos {
                            state.cancel_dns_query(query);
                            return Err(UdpError {
                                kind: UdpErrorKind::Timeout,
                                detail: NetworkErrorDetail::DnsLookupTimeout,
                            });
                        }
                        state.next_wait_duration(now_nanos, deadline_nanos)
                    }
                }
            };
            self.wait_for_progress(next_wait).await;
        }
    }

    async fn wait_until_ping<T>(
        &self,
        deadline_nanos: u64,
        timeout_error: PingError,
        mut check: impl FnMut(&mut NetworkState) -> Result<Option<T>, PingError>,
    ) -> Result<T, PingError> {
        loop {
            self.drive_ping().await?;

            let now_nanos = self.now_nanos();
            let next_wait = {
                let mut state = self.inner.state.lock().await;
                if let Some(value) = check(&mut state)? {
                    return Ok(value);
                }
                if now_nanos >= deadline_nanos {
                    return Err(timeout_error);
                }
                state.next_wait_duration(now_nanos, deadline_nanos)
            };

            self.wait_for_progress(next_wait).await;
        }
    }

    async fn drive_ping(&self) -> Result<(), PingError> {
        self.drive_network()
            .await
            .map_err(|error| PingError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))
    }

    async fn drive_dns(&self) -> Result<(), DnsError> {
        self.drive_network()
            .await
            .map_err(|error| DnsError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))
    }

    async fn drive_tcp(&self) -> Result<(), TcpError> {
        self.drive_network()
            .await
            .map_err(|error| TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))
    }

    async fn drive_udp(&self) -> Result<(), UdpError> {
        self.drive_network()
            .await
            .map_err(|error| UdpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))
    }

    async fn drive_network(&self) -> Result<(), IoError> {
        loop {
            let mut received = Vec::new();
            loop {
                match self.inner.device.try_receive().await {
                    Ok(Some(frame)) => received.push(frame),
                    Ok(None) => break,
                    Err(error) => return Err(error),
                }
            }
            let outbound = {
                let mut state = self.inner.state.lock().await;
                for frame in received {
                    state.inbound.push_back(frame);
                }
                state.poll(smol_now(self.now_nanos()));
                state.take_outbound()
            };

            if outbound.is_empty() {
                return Ok(());
            }

            // A TX completion and RX readiness can be observed in the same device event. Loop
            // back after every burst so inbound packets that arrived while transmitting are
            // drained before we sleep again.
            for frame in outbound {
                self.inner.device.transmit(&frame).await?;
            }
        }
    }

    async fn wait_for_progress(&self, duration: Duration) {
        if duration.is_zero() {
            return;
        }

        let event = self.inner.device.wait_for_event();
        let timer = self.inner.timer.sleep_for(duration);
        let mut event = core::pin::pin!(event);
        let mut timer = core::pin::pin!(timer);

        core::future::poll_fn(|cx| {
            if event.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
            if timer.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
            Poll::Pending
        })
        .await;
    }

    fn now_nanos(&self) -> u64 {
        self.inner
            .runtime_state
            .uptime_nanos(self.inner.cpu.now().ticks())
    }

    pub fn hardware_address(&self) -> [u8; 6] {
        self.inner.device.mac_address()
    }

    pub async fn ipv4_cidr(&self) -> Option<crate::Ipv4Cidr> {
        let state = self.inner.state.lock().await;
        state
            .ipv4_address
            .map(|cidr| crate::Ipv4Cidr::new(map_ipv4_address(cidr.address()), cidr.prefix_len()))
    }
}

impl<CpuImpl, Runtime, DeviceImpl> ComponentNetworkService
    for NetworkService<CpuImpl, Runtime, DeviceImpl>
where
    CpuImpl: Cpu + Clone,
    Runtime: ComponentRuntimeState + Sync,
    DeviceImpl: NetworkDevice,
{
    type TcpStream = TcpStreamId;
    type UdpSocket = UdpSocketId;

    fn hardware_address(&self) -> [u8; 6] {
        NetworkService::hardware_address(self)
    }

    fn ipv4_cidr(&self) -> impl core::future::Future<Output = Option<crate::Ipv4Cidr>> + Send + '_ {
        async move { NetworkService::ipv4_cidr(self).await }
    }

    fn ping<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<PingReply, PingError>> + Send + 'a {
        async move { NetworkService::ping(self, host, timeout_nanos).await }
    }

    fn dns_resolve<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Vec<KernelIpv4Address>, DnsError>> + Send + 'a
    {
        async move { NetworkService::dns_resolve(self, host, timeout_nanos).await }
    }

    fn tcp_connect<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Self::TcpStream, TcpError>> + Send + 'a {
        async move { NetworkService::tcp_connect(self, host, port, timeout_nanos).await }
    }

    fn tcp_write_all<'a>(
        &'a self,
        stream: Self::TcpStream,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<(), TcpError>> + Send + 'a {
        async move { NetworkService::tcp_write_all(self, stream, bytes, timeout_nanos).await }
    }

    fn tcp_read<'a>(
        &'a self,
        stream: Self::TcpStream,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Option<Vec<u8>>, TcpError>> + Send + 'a {
        async move { NetworkService::tcp_read(self, stream, max_bytes, timeout_nanos).await }
    }

    fn tcp_close(
        &self,
        stream: Self::TcpStream,
    ) -> impl core::future::Future<Output = ()> + Send + '_ {
        async move { NetworkService::tcp_close(self, stream).await }
    }

    fn udp_bind(
        &self,
        local_port: u16,
    ) -> impl core::future::Future<Output = Result<UdpBinding<Self::UdpSocket>, UdpError>> + Send + '_
    {
        async move { NetworkService::udp_bind(self, local_port).await }
    }

    fn udp_send<'a>(
        &'a self,
        socket: Self::UdpSocket,
        host: &'a str,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<u64, UdpError>> + Send + 'a {
        async move { NetworkService::udp_send(self, socket, host, port, bytes, timeout_nanos).await }
    }

    fn udp_receive<'a>(
        &'a self,
        socket: Self::UdpSocket,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Option<UdpDatagram>, UdpError>> + Send + 'a {
        async move { NetworkService::udp_receive(self, socket, max_bytes, timeout_nanos).await }
    }

    fn udp_close(
        &self,
        socket: Self::UdpSocket,
    ) -> impl core::future::Future<Output = ()> + Send + '_ {
        async move { NetworkService::udp_close(self, socket).await }
    }
}

impl NetworkState {
    fn new(mac: [u8; 6], max_frame_len: usize, random_seed: u64) -> Self {
        let mut queue_device = QueueDevice {
            inbound: &mut VecDeque::new(),
            outbound: &mut VecDeque::new(),
            max_frame_len,
        };
        let mut config = InterfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        config.random_seed = random_seed;
        let iface = Interface::new(config, &mut queue_device, SmolInstant::ZERO);

        let mut sockets = SocketSet::new(vec![]);

        let mut dhcp = dhcpv4::Socket::new();
        dhcp.set_parameter_request_list(DHCP_PARAMETERS);
        let dhcp_handle = sockets.add(dhcp);

        let dns_handle = sockets.add(dns::Socket::new(&[], vec![None]));

        let mut icmp_socket = icmp::Socket::new(
            icmp::PacketBuffer::new(
                vec![icmp::PacketMetadata::EMPTY; ICMP_BUFFER_PACKETS],
                vec![0; ICMP_BUFFER_BYTES],
            ),
            icmp::PacketBuffer::new(
                vec![icmp::PacketMetadata::EMPTY; ICMP_BUFFER_PACKETS],
                vec![0; ICMP_BUFFER_BYTES],
            ),
        );
        icmp_socket
            .bind(icmp::Endpoint::Ident(ICMP_IDENTIFIER))
            .unwrap_or_else(|error| panic!("failed to bind ICMP socket: {error:?}"));
        let icmp_handle = sockets.add(icmp_socket);

        Self {
            iface,
            sockets,
            dhcp_handle,
            dns_handle,
            icmp_handle,
            inbound: VecDeque::new(),
            outbound: VecDeque::new(),
            ipv4_address: None,
            dns_servers: Vec::new(),
            next_echo_sequence: 1,
            next_tcp_local_port: EPHEMERAL_PORT_START,
            next_udp_local_port: EPHEMERAL_PORT_START,
            tcp_streams: Vec::new(),
            udp_sockets: Vec::new(),
            max_frame_len,
        }
    }

    fn poll(&mut self, now: SmolInstant) {
        {
            let mut device = QueueDevice {
                inbound: &mut self.inbound,
                outbound: &mut self.outbound,
                max_frame_len: self.max_frame_len,
            };
            let _ = self.iface.poll(now, &mut device, &mut self.sockets);
        }
        self.apply_dhcp();
    }

    fn apply_dhcp(&mut self) {
        let event = {
            let socket = self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp_handle);
            socket.poll()
        };
        match event {
            Some(dhcpv4::Event::Configured(config)) => {
                self.ipv4_address = Some(config.address);
                self.dns_servers = config
                    .dns_servers
                    .iter()
                    .copied()
                    .map(IpAddress::Ipv4)
                    .collect();
                self.iface.update_ip_addrs(|addresses| {
                    addresses.clear();
                    addresses
                        .push(IpCidr::Ipv4(config.address))
                        .unwrap_or_else(|_| panic!("interface address table overflowed"));
                });
                let routes = self.iface.routes_mut();
                routes.remove_default_ipv4_route();
                if let Some(router) = config.router {
                    routes
                        .add_default_ipv4_route(router)
                        .unwrap_or_else(|error| {
                            panic!("failed to install default IPv4 route: {error:?}")
                        });
                }
                self.sockets
                    .get_mut::<dns::Socket>(self.dns_handle)
                    .update_servers(&self.dns_servers);
            }
            Some(dhcpv4::Event::Deconfigured) => {
                self.ipv4_address = None;
                self.dns_servers.clear();
                self.iface.update_ip_addrs(|addresses| addresses.clear());
                self.iface.routes_mut().remove_default_ipv4_route();
                self.sockets
                    .get_mut::<dns::Socket>(self.dns_handle)
                    .update_servers(&[]);
            }
            None => {}
        }
    }

    fn is_configured(&self) -> bool {
        self.ipv4_address.is_some()
    }

    fn start_dns_query(&mut self, host: &str) -> Result<dns::QueryHandle, PingError> {
        if self.dns_servers.is_empty() {
            return Err(PingError {
                kind: PingErrorKind::Unavailable,
                detail: NetworkErrorDetail::DnsServersUnavailable,
            });
        }

        self.sockets
            .get_mut::<dns::Socket>(self.dns_handle)
            .start_query(self.iface.context(), host, DnsQueryType::A)
            .map_err(|error| PingError {
                kind: PingErrorKind::UnresolvedHost,
                detail: {
                    tracing::error!(host, ?error, "failed to start DNS query");
                    NetworkErrorDetail::DnsQueryStartFailed
                },
            })
    }

    fn start_dns_query_dns(&mut self, host: &str) -> Result<dns::QueryHandle, DnsError> {
        if self.dns_servers.is_empty() {
            return Err(DnsError {
                kind: DnsErrorKind::Unavailable,
                detail: NetworkErrorDetail::DnsServersUnavailable,
            });
        }

        self.sockets
            .get_mut::<dns::Socket>(self.dns_handle)
            .start_query(self.iface.context(), host, DnsQueryType::A)
            .map_err(|error| DnsError {
                kind: DnsErrorKind::UnresolvedHost,
                detail: {
                    tracing::error!(host, ?error, "failed to start DNS query");
                    NetworkErrorDetail::DnsQueryStartFailed
                },
            })
    }

    fn start_dns_query_tcp(&mut self, host: &str) -> Result<dns::QueryHandle, TcpError> {
        if self.dns_servers.is_empty() {
            return Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::DnsServersUnavailable,
            });
        }

        self.sockets
            .get_mut::<dns::Socket>(self.dns_handle)
            .start_query(self.iface.context(), host, DnsQueryType::A)
            .map_err(|error| TcpError {
                kind: TcpErrorKind::UnresolvedHost,
                detail: {
                    tracing::error!(host, ?error, "failed to start DNS query");
                    NetworkErrorDetail::DnsQueryStartFailed
                },
            })
    }

    fn start_dns_query_udp(&mut self, host: &str) -> Result<dns::QueryHandle, UdpError> {
        if self.dns_servers.is_empty() {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::DnsServersUnavailable,
            });
        }

        self.sockets
            .get_mut::<dns::Socket>(self.dns_handle)
            .start_query(self.iface.context(), host, DnsQueryType::A)
            .map_err(|error| UdpError {
                kind: UdpErrorKind::UnresolvedHost,
                detail: {
                    tracing::error!(host, ?error, "failed to start DNS query");
                    NetworkErrorDetail::DnsQueryStartFailed
                },
            })
    }

    fn take_dns_result(
        &mut self,
        query: dns::QueryHandle,
    ) -> Result<Option<Ipv4Address>, PingError> {
        match self
            .sockets
            .get_mut::<dns::Socket>(self.dns_handle)
            .get_query_result(query)
        {
            Ok(addresses) => Ok(addresses.into_iter().next().map(|address| match address {
                IpAddress::Ipv4(address) => address,
            })),
            Err(dns::GetQueryResultError::Pending) => Ok(None),
            Err(dns::GetQueryResultError::Failed) => Err(PingError {
                kind: PingErrorKind::UnresolvedHost,
                detail: NetworkErrorDetail::DnsLookupFailed,
            }),
        }
    }

    fn take_dns_addresses_dns(
        &mut self,
        query: dns::QueryHandle,
    ) -> Result<Option<Vec<KernelIpv4Address>>, DnsError> {
        match self
            .sockets
            .get_mut::<dns::Socket>(self.dns_handle)
            .get_query_result(query)
        {
            Ok(addresses) => {
                let resolved = addresses
                    .into_iter()
                    .map(|address| match address {
                        IpAddress::Ipv4(address) => map_ipv4_address(address),
                    })
                    .collect::<Vec<_>>();
                if resolved.is_empty() {
                    return Err(DnsError {
                        kind: DnsErrorKind::UnresolvedHost,
                        detail: NetworkErrorDetail::DnsNoIpv4Address,
                    });
                }
                Ok(Some(resolved))
            }
            Err(dns::GetQueryResultError::Pending) => Ok(None),
            Err(dns::GetQueryResultError::Failed) => Err(DnsError {
                kind: DnsErrorKind::UnresolvedHost,
                detail: NetworkErrorDetail::DnsLookupFailed,
            }),
        }
    }

    fn take_dns_result_tcp(
        &mut self,
        query: dns::QueryHandle,
    ) -> Result<Option<Ipv4Address>, TcpError> {
        match self
            .sockets
            .get_mut::<dns::Socket>(self.dns_handle)
            .get_query_result(query)
        {
            Ok(addresses) => Ok(addresses.into_iter().next().map(|address| match address {
                IpAddress::Ipv4(address) => address,
            })),
            Err(dns::GetQueryResultError::Pending) => Ok(None),
            Err(dns::GetQueryResultError::Failed) => Err(TcpError {
                kind: TcpErrorKind::UnresolvedHost,
                detail: NetworkErrorDetail::DnsLookupFailed,
            }),
        }
    }

    fn take_dns_result_udp(
        &mut self,
        query: dns::QueryHandle,
    ) -> Result<Option<Ipv4Address>, UdpError> {
        match self
            .sockets
            .get_mut::<dns::Socket>(self.dns_handle)
            .get_query_result(query)
        {
            Ok(addresses) => Ok(addresses.into_iter().next().map(|address| match address {
                IpAddress::Ipv4(address) => address,
            })),
            Err(dns::GetQueryResultError::Pending) => Ok(None),
            Err(dns::GetQueryResultError::Failed) => Err(UdpError {
                kind: UdpErrorKind::UnresolvedHost,
                detail: NetworkErrorDetail::DnsLookupFailed,
            }),
        }
    }

    fn cancel_dns_query(&mut self, query: dns::QueryHandle) {
        self.sockets
            .get_mut::<dns::Socket>(self.dns_handle)
            .cancel_query(query);
    }

    fn start_ping(&mut self, destination: Ipv4Address) -> Result<u16, PingError> {
        let sequence = self.next_echo_sequence;
        self.next_echo_sequence = self.next_echo_sequence.wrapping_add(1);
        if self.next_echo_sequence == 0 {
            self.next_echo_sequence = 1;
        }

        let socket = self.sockets.get_mut::<icmp::Socket>(self.icmp_handle);
        while socket.can_recv() {
            let _ = socket.recv();
        }

        let repr = Icmpv4Repr::EchoRequest {
            ident: ICMP_IDENTIFIER,
            seq_no: sequence,
            data: ICMP_PAYLOAD,
        };
        let mut packet = vec![0; repr.buffer_len()];
        repr.emit(
            &mut Icmpv4Packet::new_unchecked(&mut packet),
            &ChecksumCapabilities::default(),
        );
        socket
            .send_slice(&packet, IpAddress::Ipv4(destination))
            .map_err(|error| PingError {
                kind: PingErrorKind::Unavailable,
                detail: {
                    tracing::error!(?error, "failed to queue ICMP echo request");
                    NetworkErrorDetail::IcmpQueueFailed
                },
            })?;
        Ok(sequence)
    }

    fn take_ping_reply(&mut self, destination: Ipv4Address, sequence: u16) -> Option<u16> {
        let socket = self.sockets.get_mut::<icmp::Socket>(self.icmp_handle);
        while socket.can_recv() {
            let (packet, remote) = socket.recv().unwrap_or_else(|error| {
                panic!("ICMP socket reported readable state but recv failed: {error:?}")
            });
            let IpAddress::Ipv4(remote) = remote;
            if remote != destination {
                continue;
            }

            let packet = Icmpv4Packet::new_checked(packet).ok()?;
            let repr = Icmpv4Repr::parse(&packet, &ChecksumCapabilities::ignored()).ok()?;
            if let Icmpv4Repr::EchoReply {
                ident,
                seq_no,
                data,
            } = repr
            {
                if ident != ICMP_IDENTIFIER || seq_no != sequence {
                    continue;
                }

                return Some(
                    u16::try_from(data.len()).unwrap_or_else(|_| {
                        panic!("ICMP payload length {} exceeds u16", data.len())
                    }),
                );
            }
        }
        None
    }

    fn start_tcp_connect(
        &mut self,
        destination: Ipv4Address,
        port: u16,
    ) -> Result<TcpStreamId, TcpError> {
        let local_port = self.allocate_tcp_local_port()?;
        let socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; TCP_BUFFER_BYTES]),
            tcp::SocketBuffer::new(vec![0; TCP_BUFFER_BYTES]),
        );
        let handle = self.sockets.add(socket);
        let connect_result = self.sockets.get_mut::<tcp::Socket>(handle).connect(
            self.iface.context(),
            (IpAddress::Ipv4(destination), port),
            local_port,
        );
        if let Err(error) = connect_result {
            let _ = self.sockets.remove(handle);
            tracing::error!(
                destination = ?destination.octets(),
                port,
                ?error,
                "failed to start TCP connect"
            );
            return Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpConnectStartFailed,
            });
        }

        Ok(self.insert_tcp_stream(handle))
    }

    fn poll_tcp_connect(&mut self, stream: TcpStreamId) -> Result<TcpConnectProgress, TcpError> {
        let socket = self.tcp_socket_mut(stream)?;
        if socket.may_send() || socket.can_recv() {
            return Ok(TcpConnectProgress::Connected);
        }

        match socket.state() {
            tcp::State::Closed | tcp::State::TimeWait => Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpClosedDuringConnect,
            }),
            _ => Ok(TcpConnectProgress::Pending),
        }
    }

    fn try_write_tcp(
        &mut self,
        stream: TcpStreamId,
        bytes: &[u8],
    ) -> Result<Option<usize>, TcpError> {
        let socket = self.tcp_socket_mut(stream)?;
        if socket.can_send() {
            let written = socket.send_slice(bytes).map_err(|error| {
                tracing::error!(stream = stream.0.get(), ?error, "failed to queue TCP write");
                TcpError {
                    kind: TcpErrorKind::Unavailable,
                    detail: NetworkErrorDetail::TcpWriteQueueFailed,
                }
            })?;
            return Ok(Some(written));
        }
        if !socket.may_send() {
            return Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpNoLongerWritable,
            });
        }
        Ok(None)
    }

    fn poll_tcp_read(
        &mut self,
        stream: TcpStreamId,
        max_bytes: usize,
    ) -> Result<TcpReadProgress, TcpError> {
        let socket = self.tcp_socket_mut(stream)?;
        if socket.can_recv() {
            let mut bytes = vec![0; max_bytes.max(1)];
            let read = socket.recv_slice(&mut bytes).map_err(|error| {
                tracing::error!(
                    stream = stream.0.get(),
                    ?error,
                    "failed to receive TCP data"
                );
                TcpError {
                    kind: TcpErrorKind::Unavailable,
                    detail: NetworkErrorDetail::TcpReceiveFailed,
                }
            })?;
            bytes.truncate(read);
            return Ok(TcpReadProgress::Data(bytes));
        }
        if socket.may_recv() {
            return Ok(TcpReadProgress::Pending);
        }
        match socket.state() {
            tcp::State::Closed | tcp::State::TimeWait => Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpClosedUnexpectedly,
            }),
            _ => Ok(TcpReadProgress::Eof),
        }
    }

    fn start_udp_bind(&mut self, local_port: u16) -> Result<UdpBinding<UdpSocketId>, UdpError> {
        let local_port = if local_port == 0 {
            self.allocate_udp_local_port()?
        } else if self.is_udp_local_port_free(local_port) {
            local_port
        } else {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpPortInUse,
            });
        };

        let socket = udp::Socket::new(
            udp::PacketBuffer::new(
                vec![udp::PacketMetadata::EMPTY; UDP_BUFFER_PACKETS],
                vec![0; UDP_BUFFER_BYTES],
            ),
            udp::PacketBuffer::new(
                vec![udp::PacketMetadata::EMPTY; UDP_BUFFER_PACKETS],
                vec![0; UDP_BUFFER_BYTES],
            ),
        );
        let handle = self.sockets.add(socket);
        let bind_result = self
            .sockets
            .get_mut::<udp::Socket>(handle)
            .bind(IpListenEndpoint {
                addr: None,
                port: local_port,
            });
        if let Err(error) = bind_result {
            let _ = self.sockets.remove(handle);
            tracing::error!(local_port, ?error, "failed to bind UDP socket");
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpBindFailed,
            });
        }

        Ok(UdpBinding {
            socket: self.insert_udp_socket(handle),
            local_port,
        })
    }

    fn try_send_udp(
        &mut self,
        socket: UdpSocketId,
        destination: Ipv4Address,
        port: u16,
        bytes: &[u8],
    ) -> Result<Option<usize>, UdpError> {
        let socket = self.udp_socket_mut(socket)?;
        if socket.can_send() {
            if bytes.len() > socket.payload_send_capacity() {
                tracing::error!(
                    datagram_len = bytes.len(),
                    transmit_capacity = socket.payload_send_capacity(),
                    "UDP datagram exceeds transmit capacity"
                );
                return Err(UdpError {
                    kind: UdpErrorKind::Unavailable,
                    detail: NetworkErrorDetail::UdpDatagramTooLarge,
                });
            }
            socket
                .send_slice(
                    bytes,
                    IpEndpoint {
                        addr: IpAddress::Ipv4(destination),
                        port,
                    },
                )
                .map_err(|error| {
                    tracing::error!(?error, "failed to queue UDP datagram");
                    UdpError {
                        kind: UdpErrorKind::Unavailable,
                        detail: NetworkErrorDetail::UdpQueueFailed,
                    }
                })?;
            return Ok(Some(bytes.len()));
        }
        Ok(None)
    }

    fn poll_udp_receive(
        &mut self,
        socket: UdpSocketId,
        max_bytes: usize,
    ) -> Result<UdpReceiveProgress, UdpError> {
        let socket = self.udp_socket_mut(socket)?;
        if socket.can_recv() {
            let mut bytes = vec![0; max_bytes.max(1)];
            let (read, metadata) = socket.recv_slice(&mut bytes).map_err(|error| {
                tracing::error!(?error, "failed to receive UDP datagram");
                UdpError {
                    kind: UdpErrorKind::Unavailable,
                    detail: NetworkErrorDetail::UdpReceiveFailed,
                }
            })?;
            bytes.truncate(read);
            let IpAddress::Ipv4(address) = metadata.endpoint.addr;
            return Ok(UdpReceiveProgress::Data(UdpDatagram {
                address: map_ipv4_address(address),
                port: metadata.endpoint.port,
                bytes,
            }));
        }
        Ok(UdpReceiveProgress::Pending)
    }

    fn remove_tcp_stream(&mut self, stream: TcpStreamId) {
        let index = stream_index(stream);
        let Some(slot) = self.tcp_streams.get_mut(index) else {
            return;
        };
        let Some(state) = slot.take() else {
            return;
        };
        let _ = self.sockets.remove(state.handle);
    }

    fn remove_udp_socket(&mut self, socket: UdpSocketId) {
        let index = socket_index(socket);
        let Some(slot) = self.udp_sockets.get_mut(index) else {
            return;
        };
        let Some(state) = slot.take() else {
            return;
        };
        let _ = self.sockets.remove(state.handle);
    }

    fn insert_tcp_stream(&mut self, handle: SocketHandle) -> TcpStreamId {
        if let Some((index, slot)) = self
            .tcp_streams
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.is_none())
        {
            *slot = Some(TcpStreamState { handle });
            return tcp_stream_id(index);
        }

        self.tcp_streams.push(Some(TcpStreamState { handle }));
        tcp_stream_id(self.tcp_streams.len() - 1)
    }

    fn insert_udp_socket(&mut self, handle: SocketHandle) -> UdpSocketId {
        if let Some((index, slot)) = self
            .udp_sockets
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.is_none())
        {
            *slot = Some(UdpSocketState { handle });
            return udp_socket_id(index);
        }

        self.udp_sockets.push(Some(UdpSocketState { handle }));
        udp_socket_id(self.udp_sockets.len() - 1)
    }

    fn tcp_socket_mut(
        &mut self,
        stream: TcpStreamId,
    ) -> Result<&mut tcp::Socket<'static>, TcpError> {
        let handle = self
            .tcp_streams
            .get(stream_index(stream))
            .and_then(|slot| slot.as_ref())
            .map(|state| state.handle)
            .ok_or_else(|| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UnknownTcpStream,
            })?;
        Ok(self.sockets.get_mut::<tcp::Socket>(handle))
    }

    fn udp_socket_mut(
        &mut self,
        socket: UdpSocketId,
    ) -> Result<&mut udp::Socket<'static>, UdpError> {
        let handle = self
            .udp_sockets
            .get(socket_index(socket))
            .and_then(|slot| slot.as_ref())
            .map(|state| state.handle)
            .ok_or_else(|| UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UnknownUdpSocket,
            })?;
        Ok(self.sockets.get_mut::<udp::Socket>(handle))
    }

    fn allocate_tcp_local_port(&mut self) -> Result<u16, TcpError> {
        let attempts = usize::from(EPHEMERAL_PORT_END - EPHEMERAL_PORT_START) + 1;
        for _ in 0..attempts {
            let candidate = self.next_tcp_local_port;
            self.next_tcp_local_port = if self.next_tcp_local_port == EPHEMERAL_PORT_END {
                EPHEMERAL_PORT_START
            } else {
                self.next_tcp_local_port + 1
            };
            if self.is_tcp_local_port_free(candidate) {
                return Ok(candidate);
            }
        }

        Err(TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpNoEphemeralPorts,
        })
    }

    fn allocate_udp_local_port(&mut self) -> Result<u16, UdpError> {
        let attempts = usize::from(EPHEMERAL_PORT_END - EPHEMERAL_PORT_START) + 1;
        for _ in 0..attempts {
            let candidate = self.next_udp_local_port;
            self.next_udp_local_port = if self.next_udp_local_port == EPHEMERAL_PORT_END {
                EPHEMERAL_PORT_START
            } else {
                self.next_udp_local_port + 1
            };
            if self.is_udp_local_port_free(candidate) {
                return Ok(candidate);
            }
        }

        Err(UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpNoEphemeralPorts,
        })
    }

    fn is_tcp_local_port_free(&self, port: u16) -> bool {
        self.tcp_streams.iter().flatten().all(|state| {
            self.sockets
                .get::<tcp::Socket>(state.handle)
                .local_endpoint()
                .is_none_or(|endpoint| endpoint.port != port)
        })
    }

    fn is_udp_local_port_free(&self, port: u16) -> bool {
        self.udp_sockets.iter().flatten().all(|state| {
            self.sockets
                .get::<udp::Socket>(state.handle)
                .endpoint()
                .port
                != port
        })
    }

    fn next_wait_duration(&mut self, now_nanos: u64, deadline_nanos: u64) -> Duration {
        let remaining_nanos = deadline_nanos.saturating_sub(now_nanos);
        let stack_wait = self
            .iface
            .poll_delay(smol_now(now_nanos), &self.sockets)
            .map(|delay| Duration::from_micros(delay.total_micros()));
        match stack_wait {
            Some(wait) => wait.min(Duration::from_nanos(remaining_nanos)),
            None => Duration::from_nanos(remaining_nanos),
        }
    }

    fn take_outbound(&mut self) -> Vec<Vec<u8>> {
        self.outbound.drain(..).collect()
    }
}

impl<'a> Device for QueueDevice<'a> {
    type RxToken<'b>
        = QueueRxToken
    where
        Self: 'b;
    type TxToken<'b>
        = QueueTxToken<'b>
    where
        Self: 'b;

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.inbound.pop_front()?;
        Some((
            QueueRxToken { frame },
            QueueTxToken {
                outbound: self.outbound,
                max_frame_len: self.max_frame_len,
            },
        ))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(QueueTxToken {
            outbound: self.outbound,
            max_frame_len: self.max_frame_len,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = self.max_frame_len;
        capabilities
    }
}

impl RxToken for QueueRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame)
    }
}

impl TxToken for QueueTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        assert!(
            len <= self.max_frame_len,
            "smoltcp requested frame length {} larger than virtio maximum {}",
            len,
            self.max_frame_len
        );
        let mut frame = vec![0; len];
        let output = f(&mut frame);
        self.outbound.push_back(frame);
        output
    }
}

fn smol_now(nanos: u64) -> SmolInstant {
    let micros = nanos / 1_000;
    let micros = micros.min(i64::MAX as u64);
    SmolInstant::from_micros(micros as i64)
}

fn parse_ipv4(input: &str) -> Option<Ipv4Address> {
    let mut octets = [0u8; 4];
    let mut parts = input.split('.');
    for octet in &mut octets {
        let part = parts.next()?;
        *octet = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(Ipv4Address::from_octets(octets))
}

fn tcp_stream_id(index: usize) -> TcpStreamId {
    let raw =
        u32::try_from(index + 1).unwrap_or_else(|_| panic!("tcp stream index {index} exceeds u32"));
    TcpStreamId(NonZeroU32::new(raw).unwrap_or_else(|| panic!("tcp stream ids must never be zero")))
}

fn stream_index(stream: TcpStreamId) -> usize {
    usize::try_from(stream.0.get() - 1)
        .unwrap_or_else(|_| panic!("tcp stream id {} does not fit into usize", stream.0.get()))
}

fn udp_socket_id(index: usize) -> UdpSocketId {
    let raw =
        u32::try_from(index + 1).unwrap_or_else(|_| panic!("udp socket index {index} exceeds u32"));
    UdpSocketId(NonZeroU32::new(raw).unwrap_or_else(|| panic!("udp socket ids must never be zero")))
}

fn socket_index(socket: UdpSocketId) -> usize {
    usize::try_from(socket.0.get() - 1)
        .unwrap_or_else(|_| panic!("udp socket id {} does not fit into usize", socket.0.get()))
}
