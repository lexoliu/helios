extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use core::num::NonZeroU32;
use core::task::Poll;
use core::time::Duration;

use fdt::Fdt;
use fdt::node::FdtNode;
use helios_hal::cpu::Cpu;
use helios_hal::io::IoError;
use helios_kernel::{Kernel, Notify, Timer};
use plic::Plic;
use smoltcp::iface::{Config as InterfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, dns, icmp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    DnsQueryType, EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr,
    Ipv4Address, Ipv4Cidr,
};

use crate::RiscvCpu;
use crate::debug_state::RuntimeState;

const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
const VIRTIO_MMIO_MODERN_VERSION: u32 = 2;
const VIRTIO_MMIO_NETWORK_DEVICE_ID: u32 = 1;
const VIRTIO_MMIO_MAGIC_OFFSET: usize = 0x000;
const VIRTIO_MMIO_VERSION_OFFSET: usize = 0x004;
const VIRTIO_MMIO_DEVICE_ID_OFFSET: usize = 0x008;
const SUPERVISOR_EXTERNAL_INTERRUPT: u32 = 9;
const DHCP_PARAMETERS: &[u8] = &[1, 3, 6];
const ICMP_IDENTIFIER: u16 = 0x4845;
const ICMP_PAYLOAD: &[u8] = b"helios";
const ICMP_BUFFER_BYTES: usize = 512;
const ICMP_BUFFER_PACKETS: usize = 4;

#[derive(Clone)]
pub(crate) struct NetworkService {
    inner: Arc<NetworkServiceInner>,
}

struct NetworkServiceInner {
    cpu: RiscvCpu,
    debug_state: RuntimeState,
    timer: Timer<RiscvCpu>,
    device: Arc<helios_virtio::VirtioMmioNetDevice>,
    state: helios_kernel::Mutex<NetworkState>,
    requests: ConcurrentQueue<PingRequest>,
    request_ready: Notify,
}

pub(crate) struct ExternalInterrupts {
    plic: &'static Plic,
    context: PlicContext,
    network: NetworkInterrupt,
}

struct NetworkInterrupt {
    source: InterruptSourceId,
    service: NetworkService,
}

#[derive(Clone, Copy)]
pub(crate) enum PingErrorKind {
    UnresolvedHost,
    Timeout,
    Unavailable,
    Internal,
}

pub(crate) struct PingError {
    pub(crate) kind: PingErrorKind,
    pub(crate) detail: String,
}

pub(crate) struct PingReply {
    pub(crate) address: Ipv4Address,
    pub(crate) round_trip_nanos: u64,
    pub(crate) payload_bytes: u16,
}

struct PingRequest {
    host: String,
    timeout_nanos: u64,
    response: PingResponse,
}

#[derive(Clone)]
struct PingResponse {
    inner: Arc<PingResponseInner>,
}

struct PingResponseInner {
    result: helios_kernel::Mutex<Option<Result<PingReply, PingError>>>,
    ready: Notify,
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
    max_frame_len: usize,
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

#[derive(Clone, Copy)]
struct PlicContext(usize);

#[derive(Clone, Copy, PartialEq, Eq)]
struct InterruptSourceId(NonZeroU32);

struct NetworkProbe {
    device: Arc<helios_virtio::VirtioMmioNetDevice>,
    irq_source: InterruptSourceId,
    plic: &'static Plic,
    context: PlicContext,
}

pub(crate) fn install_network_service(
    cpu: &RiscvCpu,
    kernel: &Kernel<RiscvCpu>,
    fdt: &Fdt<'_>,
    debug_state: &RuntimeState,
) -> Option<ExternalInterrupts> {
    let probe = NetworkProbe::discover(fdt, cpu.bootstrap_processor().id())?;
    let service = NetworkService::new(
        cpu.clone(),
        debug_state.clone(),
        kernel.timer(),
        probe.device.clone(),
    );
    debug_state.install_network_service(service.clone());
    let network_task = service.clone();
    kernel.spawn_local_detached(async move {
        network_task.run_requests().await;
    });
    probe.enable();
    tracing::info!(
        "virtio network online irq={} context={}",
        probe.irq_source.0.get(),
        probe.context.0
    );
    Some(ExternalInterrupts {
        plic: probe.plic,
        context: probe.context,
        network: NetworkInterrupt {
            source: probe.irq_source,
            service,
        },
    })
}

impl NetworkService {
    fn new(
        cpu: RiscvCpu,
        debug_state: RuntimeState,
        timer: Timer<RiscvCpu>,
        device: Arc<helios_virtio::VirtioMmioNetDevice>,
    ) -> Self {
        let random_seed = cpu.now().ticks();
        Self {
            inner: Arc::new(NetworkServiceInner {
                cpu,
                debug_state,
                timer,
                device: device.clone(),
                state: helios_kernel::Mutex::new(NetworkState::new(
                    device.mac_address(),
                    device.max_frame_len(),
                    random_seed,
                )),
                requests: ConcurrentQueue::unbounded(),
                request_ready: Notify::new(),
            }),
        }
    }

    pub(crate) fn handle_interrupt(&self) {
        self.inner.device.handle_interrupt();
    }

    pub(crate) async fn ping(
        &self,
        host: &str,
        timeout_nanos: u64,
    ) -> Result<PingReply, PingError> {
        let response = PingResponse::new();
        let request = PingRequest {
            host: host.to_owned(),
            timeout_nanos,
            response: response.clone(),
        };
        match self.inner.requests.push(request) {
            Ok(()) => self.inner.request_ready.notify_one(),
            Err(PushError::Full(_)) => unreachable!("unbounded network queue reported full"),
            Err(PushError::Closed(_)) => panic!("network request queue was closed unexpectedly"),
        }
        response.wait().await
    }

    async fn run_requests(&self) {
        loop {
            let request = self.next_request().await;
            let result = self
                .execute_ping(&request.host, request.timeout_nanos)
                .await;
            request.response.complete(result).await;
        }
    }

    async fn next_request(&self) -> PingRequest {
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
        self.wait_for_ipv4(deadline_nanos).await?;
        let destination = self.resolve_host(host, deadline_nanos).await?;
        let (sequence, sent_at_nanos) = {
            let mut state = self.inner.state.lock().await;
            let sequence = state.start_ping(destination)?;
            (sequence, self.now_nanos())
        };

        let payload_bytes = self
            .wait_until(
                deadline_nanos,
                PingError {
                    kind: PingErrorKind::Timeout,
                    detail: format!(
                        "icmp echo request to {} timed out",
                        format_ipv4(destination)
                    ),
                },
                move |state| Ok(state.take_ping_reply(destination, sequence)),
            )
            .await?;
        Ok(PingReply {
            address: destination,
            round_trip_nanos: self.now_nanos().saturating_sub(sent_at_nanos),
            payload_bytes,
        })
    }

    async fn wait_for_ipv4(&self, deadline_nanos: u64) -> Result<(), PingError> {
        self.wait_until(
            deadline_nanos,
            PingError {
                kind: PingErrorKind::Timeout,
                detail: "network configuration timed out".to_owned(),
            },
            |state| Ok(state.is_configured().then_some(())),
        )
        .await
    }

    async fn resolve_host(
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
            .wait_until(
                deadline_nanos,
                PingError {
                    kind: PingErrorKind::Timeout,
                    detail: format!("dns lookup for {host} timed out"),
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

    async fn wait_until<T>(
        &self,
        deadline_nanos: u64,
        timeout_error: PingError,
        mut check: impl FnMut(&mut NetworkState) -> Result<Option<T>, PingError>,
    ) -> Result<T, PingError> {
        loop {
            self.drive().await?;

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

    async fn drive(&self) -> Result<(), PingError> {
        loop {
            let mut received = Vec::new();
            loop {
                match self.inner.device.try_receive().await {
                    Ok(Some(frame)) => received.push(frame),
                    Ok(None) => break,
                    Err(error) => {
                        return Err(PingError::from_io(error, "failed to receive virtio frame"));
                    }
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

            // Virtio may signal TX completion and RX readiness with the same interrupt. Loop
            // back after every burst so inbound packets that arrived while transmitting are
            // drained before we sleep again.
            for frame in outbound {
                self.inner.device.transmit(&frame).await.map_err(|error| {
                    PingError::from_io(error, "failed to transmit virtio frame")
                })?;
            }
        }
    }

    async fn wait_for_progress(&self, duration: Duration) {
        if duration.is_zero() {
            return;
        }

        let interrupt = self.inner.device.wait_for_interrupt();
        let timer = self.inner.timer.sleep_for(duration);
        let mut interrupt = core::pin::pin!(interrupt);
        let mut timer = core::pin::pin!(timer);

        core::future::poll_fn(|cx| {
            if interrupt.as_mut().poll(cx).is_ready() {
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
            .debug_state
            .uptime_nanos(self.inner.cpu.now().ticks())
    }
}

impl ExternalInterrupts {
    pub(crate) fn handle(&self) {
        while let Some(source) = self.plic.claim(self.context) {
            let source = InterruptSourceId(source);
            if source == self.network.source {
                self.network.service.handle_interrupt();
                self.plic.complete(self.context, source);
                continue;
            }

            panic!(
                "unexpected external interrupt source={} on PLIC context={}",
                source.0.get(),
                self.context.0
            );
        }
    }
}

impl PingError {
    fn from_io(error: IoError, context: &str) -> Self {
        let kind = match error {
            IoError::Unsupported | IoError::PermissionDenied | IoError::ReadOnly => {
                PingErrorKind::Unavailable
            }
            IoError::InvalidBufferLength { .. } | IoError::OutOfBounds | IoError::DeviceFault => {
                PingErrorKind::Internal
            }
        };
        Self {
            kind,
            detail: format!("{context}: {error}"),
        }
    }
}

impl PingResponse {
    fn new() -> Self {
        Self {
            inner: Arc::new(PingResponseInner {
                result: helios_kernel::Mutex::new(None),
                ready: Notify::new(),
            }),
        }
    }

    async fn complete(&self, result: Result<PingReply, PingError>) {
        *self.inner.result.lock().await = Some(result);
        self.inner.ready.notify_one();
    }

    async fn wait(&self) -> Result<PingReply, PingError> {
        loop {
            if let Some(result) = self.inner.result.lock().await.take() {
                return result;
            }
            self.inner.ready.notified().await;
        }
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
                detail: "no DNS servers are configured yet".to_owned(),
            });
        }

        self.sockets
            .get_mut::<dns::Socket>(self.dns_handle)
            .start_query(self.iface.context(), host, DnsQueryType::A)
            .map_err(|error| PingError {
                kind: PingErrorKind::UnresolvedHost,
                detail: format!("failed to start DNS query for {host}: {error}"),
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
                detail: "DNS lookup failed".to_owned(),
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
                detail: format!("failed to queue ICMP echo request: {error:?}"),
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

impl NetworkProbe {
    fn discover(fdt: &Fdt<'_>, bootstrap_hart: u16) -> Option<Self> {
        let (device, irq_source) = discover_network_device(fdt)?;
        let (plic, context) = discover_plic_context(fdt, bootstrap_hart)?;
        Some(Self {
            device,
            irq_source,
            plic,
            context,
        })
    }

    fn enable(&self) {
        self.plic.set_priority(self.irq_source, 1);
        self.plic.enable(self.irq_source, self.context);
        self.plic.set_threshold(self.context, 0);
    }
}

impl plic::HartContext for PlicContext {
    fn index(self) -> usize {
        self.0
    }
}

impl plic::InterruptSource for InterruptSourceId {
    fn id(self) -> NonZeroU32 {
        self.0
    }
}

fn discover_network_device(
    fdt: &Fdt<'_>,
) -> Option<(Arc<helios_virtio::VirtioMmioNetDevice>, InterruptSourceId)> {
    for node in fdt.all_nodes() {
        if !node
            .compatible()
            .is_some_and(|compatible| compatible.all().any(|entry| entry == "virtio,mmio"))
        {
            continue;
        }

        let region = node.reg()?.next()?;
        let base = region.starting_address as usize;
        if !is_network_mmio_device(base) {
            continue;
        }

        let header = core::ptr::NonNull::new(base as *mut u8)
            .unwrap_or_else(|| panic!("virtio MMIO base {base:#x} was unexpectedly null"));
        let mmio_size = region.size.unwrap();
        let irq_source = node
            .interrupts()?
            .next()
            .and_then(|irq| NonZeroU32::new(irq as u32))
            .map(InterruptSourceId)
            .unwrap_or_else(|| {
                panic!("virtio-net node at {base:#x} has no valid interrupt source")
            });
        let device =
            unsafe { helios_virtio::net_from_mmio(header, mmio_size) }.unwrap_or_else(|error| {
                panic!("failed to initialize virtio-net device at {base:#x}: {error}")
            });
        return Some((Arc::new(device), irq_source));
    }

    None
}

fn discover_plic_context(fdt: &Fdt<'_>, hart_id: u16) -> Option<(&'static Plic, PlicContext)> {
    let node = fdt.find_compatible(&["sifive,plic-1.0.0", "riscv,plic0"])?;
    let base = node.reg()?.next()?.starting_address as usize;
    let plic = unsafe { Plic::from_addr(base) };
    let context = parse_plic_context(fdt, node, hart_id)?;
    Some((plic, context))
}

fn parse_plic_context(fdt: &Fdt<'_>, node: FdtNode<'_, '_>, hart_id: u16) -> Option<PlicContext> {
    let interrupts_extended = node.property("interrupts-extended")?;
    let mut cells = interrupts_extended.value.chunks_exact(4);
    let mut index = 0usize;

    while let Some(phandle) = cells.next().map(read_be_u32) {
        let controller = fdt.find_phandle(phandle).unwrap_or_else(|| {
            panic!("PLIC references unknown interrupt controller phandle {phandle}")
        });
        let interrupt_cells = controller.interrupt_cells().unwrap_or_else(|| {
            panic!("interrupt controller phandle {phandle} is missing #interrupt-cells")
        });
        let spec =
            read_be_u32(cells.next().unwrap_or_else(|| {
                panic!("PLIC interrupts-extended truncated for phandle {phandle}")
            }));
        for _ in 1..interrupt_cells {
            let _ = cells.next().unwrap_or_else(|| {
                panic!("PLIC interrupts-extended truncated extra cells for phandle {phandle}")
            });
        }
        if controller_hart_id(fdt, phandle) == Some(hart_id)
            && spec == SUPERVISOR_EXTERNAL_INTERRUPT
        {
            return Some(PlicContext(index));
        }
        index = index.saturating_add(1);
    }

    None
}

fn controller_hart_id(fdt: &Fdt<'_>, phandle: u32) -> Option<u16> {
    let cpus = fdt.find_node("/cpus")?;
    let address_cells = cpus.cell_sizes().address_cells;

    for cpu in cpus.children() {
        if cpu
            .property("device_type")
            .is_none_or(|property| property.value != b"cpu\0")
        {
            continue;
        }

        let hart = parse_cpu_hart_id(cpu, address_cells)?;
        for child in cpu.children() {
            if child.property("interrupt-controller").is_none() {
                continue;
            }
            if node_phandle(child) == Some(phandle) {
                return Some(hart);
            }
        }
    }
    None
}

fn node_phandle(node: FdtNode<'_, '_>) -> Option<u32> {
    node.property("phandle")
        .or_else(|| node.property("linux,phandle"))
        .map(|property| read_be_u32(property.value))
}

fn is_network_mmio_device(base: usize) -> bool {
    unsafe {
        read_u32(base + VIRTIO_MMIO_MAGIC_OFFSET) == VIRTIO_MMIO_MAGIC
            && read_u32(base + VIRTIO_MMIO_VERSION_OFFSET) == VIRTIO_MMIO_MODERN_VERSION
            && read_u32(base + VIRTIO_MMIO_DEVICE_ID_OFFSET) == VIRTIO_MMIO_NETWORK_DEVICE_ID
    }
}

unsafe fn read_u32(addr: usize) -> u32 {
    unsafe { (addr as *const u32).read_volatile() }
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

fn format_ipv4(address: Ipv4Address) -> String {
    let octets = address.octets();
    format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3])
}

fn parse_cpu_hart_id(node: FdtNode<'_, '_>, address_cells: usize) -> Option<u16> {
    let reg = node.property("reg")?;
    let hart = match address_cells {
        1 => read_be_u32(reg.value) as usize,
        2 => {
            let high = read_be_u32(&reg.value[..4]) as u64;
            let low = read_be_u32(&reg.value[4..8]) as u64;
            ((high << 32) | low) as usize
        }
        cells => panic!("unsupported CPU address-cells value {cells}"),
    };
    Some(hart as u16)
}

fn read_be_u32(bytes: &[u8]) -> u32 {
    let bytes: [u8; 4] = bytes
        .get(..4)
        .unwrap_or_else(|| panic!("expected a 4-byte device-tree cell"))
        .try_into()
        .unwrap_or_else(|_| panic!("expected a 4-byte device-tree cell"));
    u32::from_be_bytes(bytes)
}
