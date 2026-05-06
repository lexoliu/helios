extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::num::NonZeroU32;
use core::task::Poll;
use core::time::Duration;

use bytes::Bytes;
use helios_hal::cpu::Cpu;
use helios_hal::io::IoError;
use helios_netstack::{
    DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DNS_PORT, DhcpClientMessage, DhcpDnsServers,
    DhcpMessageType, DhcpPacket, DnsQuestionWriter, DnsResponse, IpAddress, IpCidr, Ipv4Address,
    Ipv4Cidr, NetworkInterface as NetworkDevice, PacketBuffer, Route, Stack, StackConfig,
    StackError, StackInstant, TcpConnectState, TcpEndpoint, TcpReadState,
};

use crate::{
    ComponentNetworkService, ComponentRuntimeState, DnsError, DnsErrorKind,
    Ipv4Address as KernelIpv4Address, Ipv4Cidr as KernelIpv4Cidr, Ipv4Route as KernelIpv4Route,
    MacAddress, NetworkAdminBackend, NetworkBridgeRequest, NetworkControlError, NetworkErrorDetail,
    NetworkPortId, PingError, PingErrorKind, PingReply, TcpAccepted, TcpError, TcpErrorKind,
    TcpListener, Timer, UdpBinding, UdpDatagram, UdpError, UdpErrorKind,
};
use triomphe::Arc;

const EPHEMERAL_PORT_START: u16 = 49_152;
const EPHEMERAL_PORT_END: u16 = 65_535;
const INTERNAL_DNS_PORT: u16 = 49_151;
const LOCAL_NETWORK_PORT: NetworkPortId = NetworkPortId::new(0);
const DHCP_RETRANSMIT_NANOS: u64 = 1_000_000_000;
const MAX_TCP_STREAM_HANDLES: usize = 256;
const MAX_TCP_LISTENER_HANDLES: usize = 64;
const MAX_UDP_SOCKET_HANDLES: usize = 256;
const NETWORK_PROGRESS_WAIT: Duration = Duration::from_micros(50);
const NETWORK_TX_BATCH_FRAMES: usize = 8;
const NETWORK_MIN_POLL_BUDGET: usize = 8;
const NETWORK_MAX_POLL_BUDGET: usize = 128;

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
pub struct TcpListenerId(NonZeroU32);

#[cfg(feature = "wasmtime-runtime")]
impl crate::ComponentHostTcpListenerToken for TcpListenerId {
    fn into_raw(self) -> u64 {
        u64::from(self.0.get())
    }

    fn from_raw(raw: u64) -> Self {
        let raw = u32::try_from(raw)
            .unwrap_or_else(|_| panic!("tcp listener handle {raw} does not fit in u32"));
        let raw =
            NonZeroU32::new(raw).unwrap_or_else(|| panic!("tcp listener handle must be non-zero"));
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

struct NetworkState {
    stack: Stack,
    next_tcp_local_port: u16,
    next_udp_local_port: u16,
    tcp_streams: HandleSlab<helios_netstack::SocketId, MAX_TCP_STREAM_HANDLES>,
    tcp_listeners: HandleSlab<TcpListenerState, MAX_TCP_LISTENER_HANDLES>,
    udp_sockets: HandleSlab<UdpSocketState, MAX_UDP_SOCKET_HANDLES>,
    poll: NetworkPollState,
    dhcp: DhcpClientState,
    dns_servers: DhcpDnsServers,
    next_dns_query_id: u16,
}

#[derive(Clone, Copy)]
struct NetworkPollBudget {
    rx_frames: usize,
    tx_completions: usize,
}

#[derive(Clone, Copy)]
struct NetworkPollProgress {
    received_frames: usize,
    reclaimed_tx: usize,
    transmitted_frames: usize,
}

impl NetworkPollProgress {
    const fn is_idle(self) -> bool {
        self.received_frames == 0 && self.reclaimed_tx == 0 && self.transmitted_frames == 0
    }

    const fn saturated(self, budget: NetworkPollBudget) -> bool {
        self.received_frames >= budget.rx_frames || self.reclaimed_tx >= budget.tx_completions
    }
}

struct NetworkPollState {
    base_rx_budget: usize,
    base_tx_completion_budget: usize,
    rx_budget: usize,
    tx_completion_budget: usize,
}

struct HandleSlab<T, const CAPACITY: usize> {
    slots: [Option<T>; CAPACITY],
    free: [usize; CAPACITY],
    free_len: usize,
}

impl<T, const CAPACITY: usize> HandleSlab<T, CAPACITY> {
    fn new() -> Self {
        assert!(
            CAPACITY != 0,
            "network handle slab capacity must be non-zero"
        );
        Self {
            slots: core::array::from_fn(|_| None),
            free: core::array::from_fn(|index| CAPACITY - 1 - index),
            free_len: CAPACITY,
        }
    }

    fn insert(&mut self, value: T) -> usize {
        if self.free_len == 0 {
            panic!("network handle slab is full");
        };
        self.free_len -= 1;
        let index = self.free[self.free_len];
        let slot = &mut self.slots[index];
        assert!(slot.is_none(), "network handle slab free list is corrupt");
        *slot = Some(value);
        index
    }

    fn get(&self, index: usize) -> Option<&T> {
        self.slots.get(index).and_then(Option::as_ref)
    }

    fn remove(&mut self, index: usize) -> Option<T> {
        let value = self.slots.get_mut(index).and_then(Option::take)?;
        self.free[self.free_len] = index;
        self.free_len += 1;
        Some(value)
    }

    fn iter(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().flatten()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DhcpClientState {
    Init {
        transaction_id: u32,
    },
    Selecting {
        transaction_id: u32,
        last_sent: StackInstant,
    },
    Requesting {
        transaction_id: u32,
        requested_ip: Ipv4Address,
        server_identifier: Ipv4Address,
        last_sent: StackInstant,
    },
    Bound,
}

struct TcpListenerState {
    local_port: u16,
}

struct UdpSocketState {
    local_port: u16,
}

enum TcpConnectProgress {
    Pending,
    Connected,
}

enum TcpReadProgress {
    Pending,
    Data(Bytes),
    Eof,
}

enum NetworkConfigurationError {
    Device(IoError),
    Control(NetworkControlError),
}

fn map_ipv4_address(address: Ipv4Address) -> KernelIpv4Address {
    KernelIpv4Address::new(address.octets())
}

fn map_kernel_ipv4_address(address: KernelIpv4Address) -> Ipv4Address {
    Ipv4Address::new(address.octets())
}

fn map_kernel_ipv4_cidr(cidr: KernelIpv4Cidr) -> Ipv4Cidr {
    Ipv4Cidr::new(map_kernel_ipv4_address(cidr.address()), cidr.prefix_len())
}

fn map_ipv4_cidr(cidr: Ipv4Cidr) -> KernelIpv4Cidr {
    KernelIpv4Cidr::new(map_ipv4_address(cidr.address()), cidr.prefix_len())
}

fn require_local_network_port(port: NetworkPortId) -> Result<(), NetworkControlError> {
    if port == LOCAL_NETWORK_PORT {
        Ok(())
    } else {
        Err(NetworkControlError::PortUnavailable)
    }
}

fn ipv4_mask_prefix_len(mask: Ipv4Address) -> Result<u8, NetworkControlError> {
    let raw = u32::from_be_bytes(mask.octets());
    let prefix_len = raw.leading_ones() as u8;
    let expected = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    };
    if raw == expected {
        Ok(prefix_len)
    } else {
        Err(NetworkControlError::InvalidAddress)
    }
}

fn ping_configuration_timeout() -> PingError {
    PingError {
        kind: PingErrorKind::Timeout,
        detail: NetworkErrorDetail::NetworkConfigurationTimeout,
    }
}

fn ping_configuration_error(error: NetworkConfigurationError) -> PingError {
    match error {
        NetworkConfigurationError::Device(error) => {
            PingError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
        }
        NetworkConfigurationError::Control(error) => PingError {
            kind: PingErrorKind::Unavailable,
            detail: network_configuration_control_detail(error),
        },
    }
}

fn dns_configuration_timeout() -> DnsError {
    DnsError {
        kind: DnsErrorKind::Timeout,
        detail: NetworkErrorDetail::NetworkConfigurationTimeout,
    }
}

fn dns_configuration_error(error: NetworkConfigurationError) -> DnsError {
    match error {
        NetworkConfigurationError::Device(error) => {
            DnsError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
        }
        NetworkConfigurationError::Control(error) => DnsError {
            kind: DnsErrorKind::Unavailable,
            detail: network_configuration_control_detail(error),
        },
    }
}

fn tcp_configuration_timeout() -> TcpError {
    TcpError {
        kind: TcpErrorKind::Timeout,
        detail: NetworkErrorDetail::NetworkConfigurationTimeout,
    }
}

fn tcp_configuration_error(error: NetworkConfigurationError) -> TcpError {
    match error {
        NetworkConfigurationError::Device(error) => {
            TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
        }
        NetworkConfigurationError::Control(error) => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: network_configuration_control_detail(error),
        },
    }
}

fn udp_configuration_timeout() -> UdpError {
    UdpError {
        kind: UdpErrorKind::Timeout,
        detail: NetworkErrorDetail::NetworkConfigurationTimeout,
    }
}

fn udp_configuration_error(error: NetworkConfigurationError) -> UdpError {
    match error {
        NetworkConfigurationError::Device(error) => {
            UdpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
        }
        NetworkConfigurationError::Control(error) => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: network_configuration_control_detail(error),
        },
    }
}

fn network_configuration_control_detail(error: NetworkControlError) -> NetworkErrorDetail {
    match error {
        NetworkControlError::PortUnavailable
        | NetworkControlError::BridgeUnavailable
        | NetworkControlError::InvalidBridgeRequest
        | NetworkControlError::InvalidAddress
        | NetworkControlError::InvalidRoute
        | NetworkControlError::RouteTimestampOutOfRange
        | NetworkControlError::BackendFault => NetworkErrorDetail::NetworkServiceUnavailable,
    }
}

const fn next_transaction_id(current: u32) -> u32 {
    let next = current.wrapping_add(1);
    if next == 0 { 1 } else { next }
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
        let transaction_id = cpu.now().ticks() as u32;
        let capabilities = device.capabilities();
        Self {
            inner: Arc::new(NetworkServiceInner {
                cpu,
                runtime_state,
                timer,
                state: crate::Mutex::new(NetworkState::new(
                    device.mac_address(),
                    device.max_frame_len(),
                    capabilities.events.rx_poll_budget,
                    transaction_id,
                )),
                device,
            }),
        }
    }

    pub async fn ping(&self, host: &str, timeout_nanos: u64) -> Result<PingReply, PingError> {
        self.execute_ping(host, timeout_nanos).await
    }

    pub async fn dns_resolve(
        &self,
        host: &str,
        timeout_nanos: u64,
    ) -> Result<Vec<KernelIpv4Address>, DnsError> {
        self.execute_dns_resolve(host, timeout_nanos).await
    }

    pub async fn tcp_connect(
        &self,
        host: &str,
        port: u16,
        timeout_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        self.tcp_connect_from(host, port, 0, timeout_nanos).await
    }

    pub async fn tcp_connect_from(
        &self,
        host: &str,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        self.execute_tcp_connect(host, port, local_port, timeout_nanos)
            .await
    }

    pub async fn tcp_listen(
        &self,
        local_port: u16,
        backlog: u16,
    ) -> Result<TcpListener<TcpListenerId>, TcpError> {
        self.execute_tcp_listen(local_port, backlog).await
    }

    pub async fn tcp_accept(
        &self,
        listener: TcpListenerId,
        timeout_nanos: u64,
    ) -> Result<TcpAccepted<TcpStreamId>, TcpError> {
        self.execute_tcp_accept(listener, timeout_nanos).await
    }

    pub async fn tcp_write_all(
        &self,
        stream: TcpStreamId,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<(), TcpError> {
        self.execute_tcp_write_all_bytes(stream, Bytes::copy_from_slice(bytes), timeout_nanos)
            .await
    }

    pub async fn tcp_write_all_bytes(
        &self,
        stream: TcpStreamId,
        bytes: Bytes,
        timeout_nanos: u64,
    ) -> Result<(), TcpError> {
        self.execute_tcp_write_all_bytes(stream, bytes, timeout_nanos)
            .await
    }

    pub async fn tcp_read(
        &self,
        stream: TcpStreamId,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Result<Option<Bytes>, TcpError> {
        self.execute_tcp_read(stream, max_bytes, timeout_nanos)
            .await
    }

    pub async fn tcp_shutdown_send(&self, stream: TcpStreamId) -> Result<(), TcpError> {
        self.inner.state.lock().await.shutdown_tcp_send(stream)?;
        self.drive_tcp().await
    }

    pub async fn tcp_close(&self, stream: TcpStreamId) {
        self.inner.state.lock().await.remove_tcp_stream(stream);
    }

    pub async fn run_packet_pump(&self) -> ! {
        loop {
            match self.poll_network_once().await {
                Ok((progress, budget)) if !progress.is_idle() || progress.saturated(budget) => {
                    crate::yield_now().await;
                }
                Ok(_) => self.wait_for_progress(NETWORK_PROGRESS_WAIT).await,
                Err(error) => {
                    tracing::debug!(?error, "network packet pump failed to drive device");
                    self.wait_for_progress(NETWORK_PROGRESS_WAIT).await;
                }
            }
        }
    }

    pub async fn udp_bind(&self, local_port: u16) -> Result<UdpBinding<UdpSocketId>, UdpError> {
        self.execute_udp_bind(local_port).await
    }

    pub async fn udp_send(
        &self,
        socket: UdpSocketId,
        host: &str,
        port: u16,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<u64, UdpError> {
        self.execute_udp_send(socket, host, port, bytes, timeout_nanos)
            .await
    }

    pub async fn udp_receive(
        &self,
        socket: UdpSocketId,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Result<Option<UdpDatagram>, UdpError> {
        self.execute_udp_receive(socket, max_bytes, timeout_nanos)
            .await
    }

    pub async fn udp_join_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        self.execute_udp_join_multicast_v4(group, interface).await
    }

    pub async fn udp_leave_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        self.execute_udp_leave_multicast_v4(group, interface).await
    }

    pub async fn udp_close(&self, socket: UdpSocketId) {
        self.inner.state.lock().await.remove_udp_socket(socket);
    }

    async fn execute_ping(&self, host: &str, timeout_nanos: u64) -> Result<PingReply, PingError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        self.wait_for_ipv4_ping(deadline_nanos).await?;
        let destination = self.resolve_host_ping(host, deadline_nanos).await?;
        loop {
            self.drive_ping().await?;
            if self.now_nanos() >= deadline_nanos {
                return Err(PingError {
                    kind: PingErrorKind::Timeout,
                    detail: NetworkErrorDetail::IcmpEchoTimeout,
                });
            }
            self.wait_for_progress(NETWORK_PROGRESS_WAIT).await;
            let _ = destination;
        }
    }

    async fn execute_dns_resolve(
        &self,
        host: &str,
        timeout_nanos: u64,
    ) -> Result<Vec<KernelIpv4Address>, DnsError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(vec![map_ipv4_address(address)]);
        }
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        self.wait_for_ipv4_dns(deadline_nanos).await?;
        let query_id = {
            let mut state = self.inner.state.lock().await;
            state.next_dns_query_id()
        };

        loop {
            self.drive_dns().await?;
            let now = StackInstant::from_nanos(self.now_nanos());
            let query = {
                let mut state = self.inner.state.lock().await;
                state.send_dns_query(query_id, host, now)?;
                state.take_dns_response(query_id)?
            };
            if let Some(addresses) = query {
                return Ok(addresses.into_iter().map(map_ipv4_address).collect());
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(DnsError {
                    kind: DnsErrorKind::Timeout,
                    detail: NetworkErrorDetail::DnsLookupTimeout,
                });
            }
            self.drive_dns().await?;
            self.wait_for_progress(NETWORK_PROGRESS_WAIT).await;
        }
    }

    async fn execute_tcp_connect(
        &self,
        host: &str,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        self.wait_for_ipv4_tcp(deadline_nanos).await?;
        let destination = self.resolve_host_tcp(host, deadline_nanos).await?;
        let stream = {
            let mut state = self.inner.state.lock().await;
            state.start_tcp_connect(destination, port, local_port)?
        };

        loop {
            self.drive_tcp().await?;
            let now_nanos = self.now_nanos();
            {
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
                    }
                    Err(error) => {
                        state.remove_tcp_stream(stream);
                        return Err(error);
                    }
                }
            };
            self.wait_for_tcp_progress(deadline_nanos).await;
        }
    }

    async fn execute_tcp_listen(
        &self,
        local_port: u16,
        _backlog: u16,
    ) -> Result<TcpListener<TcpListenerId>, TcpError> {
        let mut state = self.inner.state.lock().await;
        state.start_tcp_listen(local_port)
    }

    async fn execute_tcp_accept(
        &self,
        listener: TcpListenerId,
        timeout_nanos: u64,
    ) -> Result<TcpAccepted<TcpStreamId>, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        loop {
            self.drive_tcp().await?;
            let accepted = {
                let mut state = self.inner.state.lock().await;
                state.poll_tcp_accept(listener)?
            };
            if let Some(accepted) = accepted {
                return Ok(accepted);
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(TcpError {
                    kind: TcpErrorKind::Timeout,
                    detail: NetworkErrorDetail::TcpAcceptTimeout,
                });
            }
            self.wait_for_tcp_progress(deadline_nanos).await;
        }
    }

    async fn execute_tcp_write_all_bytes(
        &self,
        stream: TcpStreamId,
        mut bytes: Bytes,
        timeout_nanos: u64,
    ) -> Result<(), TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        while !bytes.is_empty() {
            self.drive_tcp().await?;
            let written = {
                let mut state = self.inner.state.lock().await;
                state.try_write_tcp_bytes(stream, &mut bytes)?
            };
            if written != 0 {
                continue;
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(TcpError {
                    kind: TcpErrorKind::Timeout,
                    detail: NetworkErrorDetail::TcpWriteTimeout,
                });
            }
            self.wait_for_tcp_progress(deadline_nanos).await;
        }
        Ok(())
    }

    async fn execute_tcp_read(
        &self,
        stream: TcpStreamId,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Result<Option<Bytes>, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        loop {
            self.drive_tcp().await?;
            let read = {
                let mut state = self.inner.state.lock().await;
                state.poll_tcp_read(stream, max_bytes as usize)?
            };
            match read {
                TcpReadProgress::Data(bytes) => return Ok(Some(bytes)),
                TcpReadProgress::Eof => return Ok(None),
                TcpReadProgress::Pending => {
                    if self.now_nanos() >= deadline_nanos {
                        return Err(TcpError {
                            kind: TcpErrorKind::Timeout,
                            detail: NetworkErrorDetail::TcpReadTimeout,
                        });
                    }
                    self.wait_for_tcp_progress(deadline_nanos).await;
                }
            }
        }
    }

    async fn execute_udp_bind(&self, local_port: u16) -> Result<UdpBinding<UdpSocketId>, UdpError> {
        self.inner.state.lock().await.start_udp_bind(local_port)
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
        let now = StackInstant::from_nanos(self.now_nanos());
        let written =
            self.inner
                .state
                .lock()
                .await
                .try_send_udp(socket, destination, port, bytes, now)?;
        Ok(u64::try_from(written).unwrap_or_else(|_| panic!("udp write length exceeds u64")))
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
            let received = {
                let mut state = self.inner.state.lock().await;
                state.poll_udp_receive(socket, max_bytes as usize)?
            };
            match received {
                Some(datagram) => return Ok(Some(datagram)),
                None => {
                    if self.now_nanos() >= deadline_nanos {
                        return Err(UdpError {
                            kind: UdpErrorKind::Timeout,
                            detail: NetworkErrorDetail::UdpReceiveTimeout,
                        });
                    }
                    self.wait_for_progress(NETWORK_PROGRESS_WAIT).await;
                }
            }
        }
    }

    async fn execute_udp_join_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        self.inner
            .state
            .lock()
            .await
            .join_multicast_v4(group, interface)
    }

    async fn execute_udp_leave_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        self.inner
            .state
            .lock()
            .await
            .leave_multicast_v4(group, interface)
    }

    async fn wait_for_ipv4_ping(&self, deadline_nanos: u64) -> Result<(), PingError> {
        self.wait_for_ipv4_configured(
            deadline_nanos,
            ping_configuration_timeout,
            ping_configuration_error,
        )
        .await
    }

    async fn wait_for_ipv4_dns(&self, deadline_nanos: u64) -> Result<(), DnsError> {
        self.wait_for_ipv4_configured(
            deadline_nanos,
            dns_configuration_timeout,
            dns_configuration_error,
        )
        .await
    }

    async fn wait_for_ipv4_tcp(&self, deadline_nanos: u64) -> Result<(), TcpError> {
        self.wait_for_ipv4_configured(
            deadline_nanos,
            tcp_configuration_timeout,
            tcp_configuration_error,
        )
        .await
    }

    async fn wait_for_ipv4_udp(&self, deadline_nanos: u64) -> Result<(), UdpError> {
        self.wait_for_ipv4_configured(
            deadline_nanos,
            udp_configuration_timeout,
            udp_configuration_error,
        )
        .await
    }

    async fn wait_for_ipv4_configured<Error>(
        &self,
        deadline_nanos: u64,
        timeout_error: fn() -> Error,
        configuration_error: fn(NetworkConfigurationError) -> Error,
    ) -> Result<(), Error> {
        loop {
            if self
                .drive_ipv4_configuration()
                .await
                .map_err(configuration_error)?
            {
                return Ok(());
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(timeout_error());
            }
            self.wait_for_progress(NETWORK_PROGRESS_WAIT).await;
        }
    }

    async fn drive_ipv4_configuration(&self) -> Result<bool, NetworkConfigurationError> {
        self.drive_network()
            .await
            .map_err(NetworkConfigurationError::Device)?;
        let now = StackInstant::from_nanos(self.now_nanos());
        let configured = {
            let mut state = self.inner.state.lock().await;
            state
                .drive_dhcp(now)
                .map_err(NetworkConfigurationError::Control)?;
            state.is_configured()
        };
        self.drive_network()
            .await
            .map_err(NetworkConfigurationError::Device)?;
        Ok(configured)
    }

    async fn resolve_host_ping(
        &self,
        host: &str,
        deadline_nanos: u64,
    ) -> Result<Ipv4Address, PingError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(address);
        }
        let timeout_nanos = deadline_nanos.saturating_sub(self.now_nanos());
        let addresses = self
            .execute_dns_resolve(host, timeout_nanos)
            .await
            .map_err(|error| PingError {
                kind: match error.kind {
                    DnsErrorKind::Timeout => PingErrorKind::Timeout,
                    DnsErrorKind::Unavailable | DnsErrorKind::Internal => {
                        PingErrorKind::Unavailable
                    }
                    DnsErrorKind::UnresolvedHost => PingErrorKind::UnresolvedHost,
                },
                detail: error.detail,
            })?;
        addresses
            .into_iter()
            .next()
            .map(map_kernel_ipv4_address)
            .ok_or(PingError {
                kind: PingErrorKind::UnresolvedHost,
                detail: NetworkErrorDetail::DnsNoIpv4Address,
            })
    }

    async fn resolve_host_tcp(
        &self,
        host: &str,
        deadline_nanos: u64,
    ) -> Result<Ipv4Address, TcpError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(address);
        }
        let timeout_nanos = deadline_nanos.saturating_sub(self.now_nanos());
        let addresses = self
            .execute_dns_resolve(host, timeout_nanos)
            .await
            .map_err(|error| TcpError {
                kind: match error.kind {
                    DnsErrorKind::Timeout => TcpErrorKind::Timeout,
                    DnsErrorKind::Unavailable | DnsErrorKind::Internal => TcpErrorKind::Unavailable,
                    DnsErrorKind::UnresolvedHost => TcpErrorKind::UnresolvedHost,
                },
                detail: error.detail,
            })?;
        addresses
            .into_iter()
            .next()
            .map(map_kernel_ipv4_address)
            .ok_or(TcpError {
                kind: TcpErrorKind::UnresolvedHost,
                detail: NetworkErrorDetail::DnsNoIpv4Address,
            })
    }

    async fn resolve_host_udp(
        &self,
        host: &str,
        deadline_nanos: u64,
    ) -> Result<Ipv4Address, UdpError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(address);
        }
        let timeout_nanos = deadline_nanos.saturating_sub(self.now_nanos());
        let addresses = self
            .execute_dns_resolve(host, timeout_nanos)
            .await
            .map_err(|error| UdpError {
                kind: match error.kind {
                    DnsErrorKind::Timeout => UdpErrorKind::Timeout,
                    DnsErrorKind::Unavailable | DnsErrorKind::Internal => UdpErrorKind::Unavailable,
                    DnsErrorKind::UnresolvedHost => UdpErrorKind::UnresolvedHost,
                },
                detail: error.detail,
            })?;
        addresses
            .into_iter()
            .next()
            .map(map_kernel_ipv4_address)
            .ok_or(UdpError {
                kind: UdpErrorKind::UnresolvedHost,
                detail: NetworkErrorDetail::DnsNoIpv4Address,
            })
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
        let _ = self.poll_network_once().await?;
        Ok(())
    }

    async fn poll_network_once(&self) -> Result<(NetworkPollProgress, NetworkPollBudget), IoError> {
        let budget = {
            let state = self.inner.state.lock().await;
            state.poll.budget()
        };

        let reclaim_started = self.profile_start();
        let reclaimed = self
            .inner
            .device
            .reclaim_transmit_completions(budget.tx_completions)
            .await?;
        if reclaimed != 0 {
            self.record_network_profile("tx-reclaim", reclaim_started);
        }

        let mut received = 0usize;
        let receive_started = self.profile_start();
        let stack_rx_budget = {
            let state = self.inner.state.lock().await;
            if state.stack.receive_backpressured() {
                0
            } else {
                state.stack.config().rx_budget
            }
        };
        loop {
            if stack_rx_budget == 0 {
                break;
            }
            let Some(frame) = self.inner.device.try_receive_frame().await? else {
                break;
            };
            let (result, receive_backpressured) = {
                let mut state = self.inner.state.lock().await;
                let result = state.stack.receive_frame_with_backpressure(
                    frame.as_ref(),
                    StackInstant::from_nanos(self.now_nanos()),
                );
                match result {
                    Ok(receive_backpressured) => (Ok(()), receive_backpressured),
                    Err(error) => (Err(error), false),
                }
            };
            self.inner.device.repost_rx_frame(frame).await?;
            if let Err(error) = result {
                if error == StackError::ReceiveBackpressure {
                    break;
                }
                tracing::debug!(?error, "dropped malformed network frame");
            }
            received += 1;
            if receive_backpressured || received >= budget.rx_frames.min(stack_rx_budget) {
                break;
            }
        }
        self.record_network_profile("rx-drain", receive_started);

        let tcp_started = self.profile_start();
        {
            let mut state = self.inner.state.lock().await;
            state
                .stack
                .drive_tcp(StackInstant::from_nanos(self.now_nanos()))
                .unwrap_or_else(|error| tracing::debug!(?error, "failed to drive TCP control"));
        }
        self.record_network_profile("tcp-drive", tcp_started);

        let mut transmitted = 0usize;
        let transmit_started = self.profile_start();
        loop {
            let mut frames = smallvec::SmallVec::<[PacketBuffer; NETWORK_TX_BATCH_FRAMES]>::new();
            {
                let mut state = self.inner.state.lock().await;
                while frames.len() < NETWORK_TX_BATCH_FRAMES {
                    let Some(frame) = state.stack.take_outbound() else {
                        break;
                    };
                    frames.push(frame);
                }
            }
            if frames.is_empty() {
                break;
            }
            self.inner.device.transmit_packet_batch(&frames).await?;
            transmitted += frames.len();
        }
        if transmitted != 0 {
            self.record_network_profile("tx-submit", transmit_started);
        }
        let progress = NetworkPollProgress {
            received_frames: received,
            reclaimed_tx: reclaimed,
            transmitted_frames: transmitted,
        };
        {
            let mut state = self.inner.state.lock().await;
            state.poll.complete(progress);
        }
        Ok((progress, budget))
    }

    async fn wait_for_progress(&self, duration: Duration) {
        if duration.is_zero() {
            return;
        }

        if !self.inner.device.capabilities().events.interrupts {
            self.inner.timer.sleep_for(duration).await;
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

    async fn wait_for_tcp_progress(&self, operation_deadline_nanos: u64) {
        let now_nanos = self.now_nanos();
        if now_nanos >= operation_deadline_nanos {
            return;
        }
        let next_tcp_deadline = {
            let mut state = self.inner.state.lock().await;
            state.stack.next_tcp_deadline().map(StackInstant::nanos)
        };
        let next_deadline = next_tcp_deadline
            .unwrap_or(operation_deadline_nanos)
            .min(operation_deadline_nanos);
        let timer_wait = Duration::from_nanos(next_deadline.saturating_sub(now_nanos));
        let wait = if self.inner.device.capabilities().events.interrupts {
            timer_wait
        } else {
            timer_wait.min(NETWORK_PROGRESS_WAIT)
        };
        self.wait_for_progress(wait).await;
    }

    fn now_nanos(&self) -> u64 {
        self.inner
            .runtime_state
            .uptime_nanos(self.inner.cpu.now().ticks())
    }

    fn profile_start(&self) -> Option<u64> {
        self.inner
            .runtime_state
            .profiling_enabled()
            .then(|| self.now_nanos())
    }

    fn record_network_profile(&self, phase: &'static str, started_nanos: Option<u64>) {
        if let Some(started_nanos) = started_nanos {
            self.inner.runtime_state.record_profile_stack_parts_nanos(
                crate::ProfileScope::Kernel,
                "kernel;network;",
                phase,
                self.now_nanos().saturating_sub(started_nanos),
            );
        }
    }

    pub fn hardware_address(&self) -> [u8; 6] {
        self.inner.device.mac_address()
    }

    pub async fn ipv4_cidr(&self) -> Option<crate::Ipv4Cidr> {
        self.inner
            .state
            .lock()
            .await
            .stack
            .primary_ipv4_address()
            .map(map_ipv4_cidr)
    }

    async fn acquire_dhcp_address(&self) -> Result<KernelIpv4Cidr, NetworkControlError> {
        loop {
            self.drive_network()
                .await
                .map_err(|_| NetworkControlError::BackendFault)?;
            let now = StackInstant::from_nanos(self.now_nanos());
            let next = {
                let mut state = self.inner.state.lock().await;
                state.drive_dhcp(now)?;
                state.stack.primary_ipv4_address().map(map_ipv4_cidr)
            };
            if let Some(cidr) = next {
                return Ok(cidr);
            }
            self.drive_network()
                .await
                .map_err(|_| NetworkControlError::BackendFault)?;
            self.wait_for_progress(NETWORK_PROGRESS_WAIT).await;
        }
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
    type TcpListener = TcpListenerId;
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

    fn tcp_connect_from<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Self::TcpStream, TcpError>> + Send + 'a {
        async move {
            NetworkService::tcp_connect_from(self, host, port, local_port, timeout_nanos).await
        }
    }

    fn tcp_listen(
        &self,
        local_port: u16,
        backlog: u16,
    ) -> impl core::future::Future<Output = Result<TcpListener<Self::TcpListener>, TcpError>> + Send + '_
    {
        async move { NetworkService::tcp_listen(self, local_port, backlog).await }
    }

    fn tcp_accept(
        &self,
        listener: Self::TcpListener,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<TcpAccepted<Self::TcpStream>, TcpError>> + Send + '_
    {
        async move { NetworkService::tcp_accept(self, listener, timeout_nanos).await }
    }

    fn tcp_write_all<'a>(
        &'a self,
        stream: Self::TcpStream,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<(), TcpError>> + Send + 'a {
        async move { NetworkService::tcp_write_all(self, stream, bytes, timeout_nanos).await }
    }

    fn tcp_write_all_bytes<'a>(
        &'a self,
        stream: Self::TcpStream,
        bytes: Bytes,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<(), TcpError>> + Send + 'a {
        async move { NetworkService::tcp_write_all_bytes(self, stream, bytes, timeout_nanos).await }
    }

    fn tcp_read<'a>(
        &'a self,
        stream: Self::TcpStream,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Option<Bytes>, TcpError>> + Send + 'a {
        async move { NetworkService::tcp_read(self, stream, max_bytes, timeout_nanos).await }
    }

    fn tcp_shutdown_send(
        &self,
        stream: Self::TcpStream,
    ) -> impl core::future::Future<Output = Result<(), TcpError>> + Send + '_ {
        async move { NetworkService::tcp_shutdown_send(self, stream).await }
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

    fn udp_join_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> impl core::future::Future<Output = Result<(), UdpError>> + Send + '_ {
        async move { NetworkService::udp_join_multicast_v4(self, group, interface).await }
    }

    fn udp_leave_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> impl core::future::Future<Output = Result<(), UdpError>> + Send + '_ {
        async move { NetworkService::udp_leave_multicast_v4(self, group, interface).await }
    }

    fn udp_close(
        &self,
        socket: Self::UdpSocket,
    ) -> impl core::future::Future<Output = ()> + Send + '_ {
        async move { NetworkService::udp_close(self, socket).await }
    }
}

impl<CpuImpl, Runtime, DeviceImpl> NetworkAdminBackend
    for NetworkService<CpuImpl, Runtime, DeviceImpl>
where
    CpuImpl: Cpu + Clone,
    Runtime: ComponentRuntimeState + Sync,
    DeviceImpl: NetworkDevice,
{
    async fn bridge_port(
        &self,
        port: NetworkPortId,
        _: NetworkBridgeRequest,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        Err(NetworkControlError::BridgeUnavailable)
    }

    async fn unbridge_port(&self, port: NetworkPortId) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        Err(NetworkControlError::BridgeUnavailable)
    }

    async fn acquire_dhcp(
        &self,
        port: NetworkPortId,
    ) -> Result<KernelIpv4Cidr, NetworkControlError> {
        require_local_network_port(port)?;
        self.acquire_dhcp_address().await
    }

    async fn add_address(
        &self,
        port: NetworkPortId,
        address: KernelIpv4Cidr,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.state.lock().await.add_ipv4_address(address)
    }

    async fn remove_address(
        &self,
        port: NetworkPortId,
        address: KernelIpv4Cidr,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.state.lock().await.remove_ipv4_address(address);
        Ok(())
    }

    async fn clear_addresses(&self, port: NetworkPortId) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.state.lock().await.clear_ipv4_addresses();
        Ok(())
    }

    async fn list_addresses(
        &self,
        port: NetworkPortId,
    ) -> Result<Vec<KernelIpv4Cidr>, NetworkControlError> {
        require_local_network_port(port)?;
        Ok(self.inner.state.lock().await.list_ipv4_addresses())
    }

    async fn mac_address(&self, port: NetworkPortId) -> Result<MacAddress, NetworkControlError> {
        require_local_network_port(port)?;
        Ok(MacAddress::new(self.hardware_address()))
    }

    async fn set_gateway(
        &self,
        port: NetworkPortId,
        gateway: KernelIpv4Address,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner
            .state
            .lock()
            .await
            .set_default_ipv4_gateway(gateway)
    }

    async fn add_route(
        &self,
        port: NetworkPortId,
        route: KernelIpv4Route,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.state.lock().await.add_ipv4_route(route)
    }

    async fn remove_route(
        &self,
        port: NetworkPortId,
        route: KernelIpv4Route,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.state.lock().await.remove_ipv4_route(route);
        Ok(())
    }

    async fn clear_routes(&self, port: NetworkPortId) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.state.lock().await.clear_ipv4_routes();
        Ok(())
    }

    async fn list_routes(
        &self,
        port: NetworkPortId,
    ) -> Result<Vec<KernelIpv4Route>, NetworkControlError> {
        require_local_network_port(port)?;
        Ok(self.inner.state.lock().await.list_ipv4_routes())
    }
}

impl NetworkState {
    fn new(mac: [u8; 6], max_frame_len: usize, rx_poll_budget: usize, transaction_id: u32) -> Self {
        Self {
            stack: Stack::new(StackConfig::new(mac, max_frame_len).with_rx_budget(rx_poll_budget)),
            next_tcp_local_port: EPHEMERAL_PORT_START,
            next_udp_local_port: EPHEMERAL_PORT_START,
            tcp_streams: HandleSlab::new(),
            tcp_listeners: HandleSlab::new(),
            udp_sockets: HandleSlab::new(),
            poll: NetworkPollState::new(rx_poll_budget, rx_poll_budget),
            dhcp: DhcpClientState::Init { transaction_id },
            dns_servers: DhcpDnsServers::new(),
            next_dns_query_id: 1,
        }
    }

    fn is_configured(&self) -> bool {
        self.stack.primary_ipv4_address().is_some()
    }

    fn drive_dhcp(&mut self, now: StackInstant) -> Result<(), NetworkControlError> {
        while let Some(datagram) = self.stack.take_udp(DHCP_CLIENT_PORT) {
            let Some(message) =
                DhcpPacket::parse(&datagram.bytes).and_then(DhcpPacket::server_message)
            else {
                continue;
            };
            match (self.dhcp, message.message_type) {
                (DhcpClientState::Selecting { transaction_id, .. }, DhcpMessageType::Offer)
                    if message.transaction_id == transaction_id =>
                {
                    let server_identifier = message
                        .server_identifier
                        .ok_or(NetworkControlError::BackendFault)?;
                    self.send_dhcp_request(
                        transaction_id,
                        message.your_ip,
                        server_identifier,
                        now,
                    )?;
                    self.dhcp = DhcpClientState::Requesting {
                        transaction_id,
                        requested_ip: message.your_ip,
                        server_identifier,
                        last_sent: now,
                    };
                }
                (DhcpClientState::Requesting { transaction_id, .. }, DhcpMessageType::Ack)
                    if message.transaction_id == transaction_id =>
                {
                    let prefix_len = message
                        .subnet_mask
                        .map(ipv4_mask_prefix_len)
                        .transpose()?
                        .ok_or(NetworkControlError::InvalidAddress)?;
                    self.stack
                        .add_ipv4_address(Ipv4Cidr::new(message.your_ip, prefix_len));
                    if let Some(router) = message.router {
                        self.set_default_ipv4_gateway(map_ipv4_address(router))?;
                    }
                    self.dns_servers.clear();
                    self.dns_servers.extend(message.dns_servers);
                    self.dhcp = DhcpClientState::Bound;
                }
                (_, DhcpMessageType::Nak) => {
                    self.stack.clear_ipv4_addresses();
                    self.dhcp = DhcpClientState::Init {
                        transaction_id: next_transaction_id(message.transaction_id),
                    };
                }
                _ => {}
            }
        }

        if let DhcpClientState::Init { transaction_id } = self.dhcp {
            self.send_dhcp_discover(transaction_id, now)?;
            self.dhcp = DhcpClientState::Selecting {
                transaction_id,
                last_sent: now,
            };
        } else {
            self.retransmit_dhcp(now)?;
        }
        Ok(())
    }

    fn retransmit_dhcp(&mut self, now: StackInstant) -> Result<(), NetworkControlError> {
        match self.dhcp {
            DhcpClientState::Selecting {
                transaction_id,
                last_sent,
            } if now.nanos().saturating_sub(last_sent.nanos()) >= DHCP_RETRANSMIT_NANOS => {
                self.send_dhcp_discover(transaction_id, now)?;
                self.dhcp = DhcpClientState::Selecting {
                    transaction_id,
                    last_sent: now,
                };
            }
            DhcpClientState::Requesting {
                transaction_id,
                requested_ip,
                server_identifier,
                last_sent,
            } if now.nanos().saturating_sub(last_sent.nanos()) >= DHCP_RETRANSMIT_NANOS => {
                self.send_dhcp_request(transaction_id, requested_ip, server_identifier, now)?;
                self.dhcp = DhcpClientState::Requesting {
                    transaction_id,
                    requested_ip,
                    server_identifier,
                    last_sent: now,
                };
            }
            _ => {}
        }
        Ok(())
    }

    fn send_dhcp_discover(
        &mut self,
        transaction_id: u32,
        now: StackInstant,
    ) -> Result<(), NetworkControlError> {
        let message = DhcpClientMessage::discover(transaction_id, self.stack.config().mac);
        self.send_dhcp_message(message, transaction_id as u16, now)
    }

    fn send_dhcp_request(
        &mut self,
        transaction_id: u32,
        requested_ip: Ipv4Address,
        server_identifier: Ipv4Address,
        now: StackInstant,
    ) -> Result<(), NetworkControlError> {
        let message = DhcpClientMessage::request(
            transaction_id,
            self.stack.config().mac,
            requested_ip,
            server_identifier,
        );
        self.send_dhcp_message(message, transaction_id as u16, now)
    }

    fn send_dhcp_message(
        &mut self,
        message: DhcpClientMessage,
        identification: u16,
        now: StackInstant,
    ) -> Result<(), NetworkControlError> {
        let mut payload = [0u8; 548];
        let len = message
            .encode(&mut payload)
            .ok_or(NetworkControlError::BackendFault)?;
        self.stack
            .send_udp_ipv4_from(
                Ipv4Address::UNSPECIFIED,
                DHCP_CLIENT_PORT,
                Ipv4Address::BROADCAST,
                DHCP_SERVER_PORT,
                &payload[..len],
                identification,
                now,
            )
            .map(|_| ())
            .map_err(|_| NetworkControlError::BackendFault)
    }

    fn next_dns_query_id(&mut self) -> u16 {
        let id = self.next_dns_query_id;
        self.next_dns_query_id = self.next_dns_query_id.wrapping_add(1);
        if self.next_dns_query_id == 0 {
            self.next_dns_query_id = 1;
        }
        id
    }

    fn send_dns_query(
        &mut self,
        query_id: u16,
        host: &str,
        now: StackInstant,
    ) -> Result<(), DnsError> {
        let Some(server) = self.dns_servers.first().copied() else {
            return Err(DnsError {
                kind: DnsErrorKind::Unavailable,
                detail: NetworkErrorDetail::DnsServersUnavailable,
            });
        };
        let mut payload = [0u8; 512];
        let len = DnsQuestionWriter::new(&mut payload)
            .write_a_query(query_id, host)
            .ok_or(DnsError {
                kind: DnsErrorKind::UnresolvedHost,
                detail: NetworkErrorDetail::DnsQueryStartFailed,
            })?;
        self.stack
            .send_udp_ipv4(
                INTERNAL_DNS_PORT,
                server,
                DNS_PORT,
                &payload[..len],
                query_id,
                now,
            )
            .map(|_| ())
            .map_err(|_| DnsError {
                kind: DnsErrorKind::Unavailable,
                detail: NetworkErrorDetail::DnsQueryStartFailed,
            })
    }

    fn take_dns_response(&mut self, query_id: u16) -> Result<Option<Vec<Ipv4Address>>, DnsError> {
        while let Some(datagram) = self.stack.take_udp(INTERNAL_DNS_PORT) {
            let Some(message) = DnsResponse::parse(&datagram.bytes).and_then(DnsResponse::message)
            else {
                continue;
            };
            if message.id != query_id {
                continue;
            }
            if message.addresses.is_empty() {
                return Err(DnsError {
                    kind: DnsErrorKind::UnresolvedHost,
                    detail: NetworkErrorDetail::DnsNoIpv4Address,
                });
            }
            return Ok(Some(message.addresses.into_iter().collect()));
        }
        Ok(None)
    }

    fn start_tcp_connect(
        &mut self,
        destination: Ipv4Address,
        port: u16,
        local_port: u16,
    ) -> Result<TcpStreamId, TcpError> {
        let local_port = if local_port == 0 {
            self.allocate_tcp_local_port()?
        } else if self.is_tcp_local_port_free(local_port) {
            local_port
        } else {
            return Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpConnectStartFailed,
            });
        };
        let Some(local) = self.stack.primary_ipv4_address().map(|cidr| cidr.address()) else {
            return Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::NetworkServiceUnavailable,
            });
        };
        let socket = self.stack.open_tcp_connect(
            TcpEndpoint {
                address: IpAddress::Ipv4(local),
                port: local_port,
            },
            TcpEndpoint {
                address: IpAddress::Ipv4(destination),
                port,
            },
            1,
        );
        Ok(self.insert_tcp_stream(socket))
    }

    fn start_tcp_listen(
        &mut self,
        local_port: u16,
    ) -> Result<TcpListener<TcpListenerId>, TcpError> {
        let local_port = if local_port == 0 {
            self.allocate_tcp_local_port()?
        } else if self.is_tcp_local_port_free(local_port) {
            local_port
        } else {
            return Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpListenStartFailed,
            });
        };
        self.stack.open_tcp_listen(TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            port: local_port,
        });
        Ok(TcpListener {
            listener: self.insert_tcp_listener(local_port),
            local_port,
        })
    }

    fn poll_tcp_accept(
        &mut self,
        listener: TcpListenerId,
    ) -> Result<Option<TcpAccepted<TcpStreamId>>, TcpError> {
        let local_port = self.tcp_listener(listener)?.local_port;
        loop {
            let Some(accepted) = self.stack.take_tcp_accept(local_port) else {
                return Ok(None);
            };
            let IpAddress::Ipv4(address) = accepted.remote.address else {
                continue;
            };
            let stream = self.insert_tcp_stream(accepted.socket);
            return Ok(Some(TcpAccepted {
                stream,
                address: map_ipv4_address(address),
                port: accepted.remote.port,
            }));
        }
    }

    fn poll_tcp_connect(&mut self, stream: TcpStreamId) -> Result<TcpConnectProgress, TcpError> {
        let socket = self.tcp_socket(stream)?;
        match self.stack.tcp_connect_state(socket).map_err(|_| TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownTcpStream,
        })? {
            TcpConnectState::Connected => Ok(TcpConnectProgress::Connected),
            TcpConnectState::Pending => Ok(TcpConnectProgress::Pending),
            TcpConnectState::Closed => Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpClosedDuringConnect,
            }),
        }
    }

    fn try_write_tcp_bytes(
        &mut self,
        stream: TcpStreamId,
        bytes: &mut Bytes,
    ) -> Result<usize, TcpError> {
        let socket = self.tcp_socket(stream)?;
        self.stack
            .tcp_send_bytes(socket, bytes)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpWriteQueueFailed,
            })
    }

    fn shutdown_tcp_send(&mut self, stream: TcpStreamId) -> Result<(), TcpError> {
        let socket = self.tcp_socket(stream)?;
        self.stack.tcp_shutdown_send(socket).map_err(|_| TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownTcpStream,
        })
    }

    fn poll_tcp_read(
        &mut self,
        stream: TcpStreamId,
        max_bytes: usize,
    ) -> Result<TcpReadProgress, TcpError> {
        let socket = self.tcp_socket(stream)?;
        match self
            .stack
            .tcp_read(socket, max_bytes)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpReceiveFailed,
            })? {
            TcpReadState::Pending => Ok(TcpReadProgress::Pending),
            TcpReadState::Data(bytes) => Ok(TcpReadProgress::Data(bytes)),
            TcpReadState::Eof => Ok(TcpReadProgress::Eof),
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
        Ok(UdpBinding {
            socket: self.insert_udp_socket(local_port),
            local_port,
        })
    }

    fn try_send_udp(
        &mut self,
        socket: UdpSocketId,
        destination: Ipv4Address,
        port: u16,
        bytes: &[u8],
        now: StackInstant,
    ) -> Result<usize, UdpError> {
        let local_port = self.udp_socket(socket)?.local_port;
        self.stack
            .send_udp_ipv4(
                local_port,
                destination,
                port,
                bytes,
                socket.0.get() as u16,
                now,
            )
            .map_err(|error| {
                tracing::debug!(?error, "failed to queue UDP datagram");
                UdpError {
                    kind: UdpErrorKind::Unavailable,
                    detail: NetworkErrorDetail::UdpQueueFailed,
                }
            })
    }

    fn poll_udp_receive(
        &mut self,
        socket: UdpSocketId,
        _max_bytes: usize,
    ) -> Result<Option<UdpDatagram>, UdpError> {
        let local_port = self.udp_socket(socket)?.local_port;
        loop {
            let Some(datagram) = self.stack.take_udp(local_port) else {
                return Ok(None);
            };
            let IpAddress::Ipv4(address) = datagram.source else {
                continue;
            };
            return Ok(Some(UdpDatagram {
                address: map_ipv4_address(address),
                port: datagram.source_port,
                bytes: datagram.bytes.to_vec(),
            }));
        }
    }

    fn remove_tcp_stream(&mut self, stream: TcpStreamId) {
        if let Some(socket) = self.tcp_streams.remove(stream_index(stream)) {
            self.stack
                .remove_tcp_socket(socket)
                .unwrap_or_else(|_| panic!("TCP stream referenced an unknown stack socket"));
        }
    }

    fn remove_udp_socket(&mut self, socket: UdpSocketId) {
        let _ = self.udp_sockets.remove(socket_index(socket));
    }

    fn insert_tcp_stream(&mut self, socket: helios_netstack::SocketId) -> TcpStreamId {
        tcp_stream_id(self.tcp_streams.insert(socket))
    }

    fn insert_tcp_listener(&mut self, local_port: u16) -> TcpListenerId {
        tcp_listener_id(self.tcp_listeners.insert(TcpListenerState { local_port }))
    }

    fn insert_udp_socket(&mut self, local_port: u16) -> UdpSocketId {
        udp_socket_id(self.udp_sockets.insert(UdpSocketState { local_port }))
    }

    fn tcp_socket(&self, stream: TcpStreamId) -> Result<helios_netstack::SocketId, TcpError> {
        self.tcp_streams
            .get(stream_index(stream))
            .copied()
            .ok_or_else(|| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UnknownTcpStream,
            })
    }

    fn tcp_listener(&self, listener: TcpListenerId) -> Result<&TcpListenerState, TcpError> {
        self.tcp_listeners
            .get(tcp_listener_index(listener))
            .ok_or_else(|| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpListenerClosedUnexpectedly,
            })
    }

    fn udp_socket(&self, socket: UdpSocketId) -> Result<&UdpSocketState, UdpError> {
        self.udp_sockets
            .get(socket_index(socket))
            .ok_or_else(|| UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UnknownUdpSocket,
            })
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
        self.tcp_listeners
            .iter()
            .all(|state| state.local_port != port)
    }

    fn is_udp_local_port_free(&self, port: u16) -> bool {
        self.udp_sockets
            .iter()
            .all(|state| state.local_port != port)
    }

    fn join_multicast_v4(
        &mut self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        self.require_multicast_interface(interface)?;
        let group = map_kernel_ipv4_address(group);
        if !group.is_multicast() {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpMulticastJoinFailed,
            });
        }
        Ok(())
    }

    fn leave_multicast_v4(
        &mut self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        self.require_multicast_interface(interface)?;
        let group = map_kernel_ipv4_address(group);
        if !group.is_multicast() {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpMulticastLeaveFailed,
            });
        }
        Ok(())
    }

    fn require_multicast_interface(&self, interface: KernelIpv4Address) -> Result<(), UdpError> {
        if interface.octets() == [0, 0, 0, 0] {
            return Ok(());
        }
        let Some(cidr) = self.stack.primary_ipv4_address() else {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpMulticastInterfaceUnavailable,
            });
        };
        if cidr.address() != map_kernel_ipv4_address(interface) {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpMulticastInterfaceUnavailable,
            });
        }
        Ok(())
    }

    fn add_ipv4_address(&mut self, cidr: KernelIpv4Cidr) -> Result<(), NetworkControlError> {
        self.stack.add_ipv4_address(map_kernel_ipv4_cidr(cidr));
        Ok(())
    }

    fn remove_ipv4_address(&mut self, cidr: KernelIpv4Cidr) {
        self.stack.remove_ipv4_address(map_kernel_ipv4_cidr(cidr));
    }

    fn clear_ipv4_addresses(&mut self) {
        self.stack.clear_ipv4_addresses();
    }

    fn list_ipv4_addresses(&self) -> Vec<KernelIpv4Cidr> {
        self.stack.ipv4_addresses().map(map_ipv4_cidr).collect()
    }

    fn set_default_ipv4_gateway(
        &mut self,
        gateway: KernelIpv4Address,
    ) -> Result<(), NetworkControlError> {
        self.stack
            .routes_mut()
            .add(Route {
                destination: IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0)),
                gateway: Some(IpAddress::Ipv4(map_kernel_ipv4_address(gateway))),
                expires_at: None,
            })
            .map_err(|_| NetworkControlError::InvalidRoute)
    }

    fn add_ipv4_route(&mut self, route: KernelIpv4Route) -> Result<(), NetworkControlError> {
        self.stack
            .routes_mut()
            .add(Route {
                destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(route.destination())),
                gateway: Some(IpAddress::Ipv4(map_kernel_ipv4_address(route.gateway()))),
                expires_at: route.expires_at_nanos().map(StackInstant::from_nanos),
            })
            .map_err(|_| NetworkControlError::InvalidRoute)
    }

    fn remove_ipv4_route(&mut self, route: KernelIpv4Route) {
        self.stack.routes_mut().remove(Route {
            destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(route.destination())),
            gateway: Some(IpAddress::Ipv4(map_kernel_ipv4_address(route.gateway()))),
            expires_at: route.expires_at_nanos().map(StackInstant::from_nanos),
        });
    }

    fn clear_ipv4_routes(&mut self) {
        self.stack.routes_mut().clear_ipv4();
    }

    fn list_ipv4_routes(&self) -> Vec<KernelIpv4Route> {
        self.stack
            .routes()
            .iter()
            .filter_map(|route| match (route.destination, route.gateway) {
                (IpCidr::Ipv4(destination), Some(IpAddress::Ipv4(gateway))) => {
                    Some(KernelIpv4Route::with_lifetimes(
                        map_ipv4_cidr(destination),
                        map_ipv4_address(gateway),
                        None,
                        route.expires_at.map(StackInstant::nanos),
                    ))
                }
                _ => None,
            })
            .collect()
    }
}

impl NetworkPollState {
    fn new(rx_budget: usize, tx_completion_budget: usize) -> Self {
        let rx_budget = clamp_poll_budget(rx_budget);
        let tx_completion_budget = clamp_poll_budget(tx_completion_budget);
        Self {
            base_rx_budget: rx_budget,
            base_tx_completion_budget: tx_completion_budget,
            rx_budget,
            tx_completion_budget,
        }
    }

    const fn budget(&self) -> NetworkPollBudget {
        NetworkPollBudget {
            rx_frames: self.rx_budget,
            tx_completions: self.tx_completion_budget,
        }
    }

    fn complete(&mut self, progress: NetworkPollProgress) {
        self.rx_budget = adjust_poll_budget(
            self.rx_budget,
            self.base_rx_budget,
            progress.received_frames >= self.rx_budget,
            progress.is_idle(),
        );
        self.tx_completion_budget = adjust_poll_budget(
            self.tx_completion_budget,
            self.base_tx_completion_budget,
            progress.reclaimed_tx >= self.tx_completion_budget,
            progress.is_idle(),
        );
    }
}

const fn clamp_poll_budget(budget: usize) -> usize {
    if budget < NETWORK_MIN_POLL_BUDGET {
        NETWORK_MIN_POLL_BUDGET
    } else if budget > NETWORK_MAX_POLL_BUDGET {
        NETWORK_MAX_POLL_BUDGET
    } else {
        budget
    }
}

fn adjust_poll_budget(current: usize, base: usize, saturated: bool, idle: bool) -> usize {
    if saturated {
        return clamp_poll_budget(current.saturating_mul(2));
    }
    if idle && current > base {
        return current / 2;
    }
    current
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
    Some(Ipv4Address::new(octets))
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

fn tcp_listener_id(index: usize) -> TcpListenerId {
    let raw = u32::try_from(index + 1)
        .unwrap_or_else(|_| panic!("tcp listener index {index} exceeds u32"));
    TcpListenerId(
        NonZeroU32::new(raw).unwrap_or_else(|| panic!("tcp listener ids must never be zero")),
    )
}

fn tcp_listener_index(listener: TcpListenerId) -> usize {
    usize::try_from(listener.0.get() - 1).unwrap_or_else(|_| {
        panic!(
            "tcp listener id {} does not fit into usize",
            listener.0.get()
        )
    })
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

#[cfg(test)]
mod tests {
    use super::HandleSlab;

    #[test]
    fn handle_slab_reuses_removed_slot() {
        let mut slab = HandleSlab::<u32, 3>::new();

        assert_eq!(slab.insert(10), 0);
        assert_eq!(slab.insert(20), 1);
        assert_eq!(slab.insert(30), 2);
        assert_eq!(slab.remove(1), Some(20));
        assert_eq!(slab.insert(40), 1);
        assert_eq!(slab.get(1), Some(&40));
    }

    #[test]
    #[should_panic(expected = "network handle slab is full")]
    fn handle_slab_panics_when_full() {
        let mut slab = HandleSlab::<u32, 1>::new();
        assert_eq!(slab.insert(10), 0);
        let _ = slab.insert(20);
    }
}
