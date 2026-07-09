extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc as StdArc;
use alloc::vec;
use alloc::vec::Vec;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use core::task::Poll;
use core::time::Duration;

use bytes::Bytes;
use helios_hal::cpu::{Cpu, HardwarePerfCounters};
use helios_hal::io::IoError;
use helios_netstack::{
    DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DNS_PORT, DhcpClientMessage, DhcpDnsServers,
    DhcpMessageType, DhcpPacket, DnsQuestionWriter, DnsResponse, EthernetFrame, EthernetProtocol,
    Icmpv4Packet, Icmpv6Packet, IpAddress, IpCidr, IpProtocol, Ipv4Address, Ipv4Cidr, Ipv4Packet,
    Ipv6Address, Ipv6Packet, MAX_OUTBOUND_FRAMES, NeighborEntry, NetworkInterface as NetworkDevice,
    OutboundBatchStatus, PacketBuffer, Route, RouteTable, RxChecksumOffload, Stack, StackConfig,
    StackError, StackEvent, StackInstant, TcpConnectState, TcpConnectTerminalError, TcpEndpoint,
    TcpListenBacklog, TcpPacket, TcpReadIntoState, TcpReadState, UdpEndpoint, UdpPacket,
    UdpPayload, UdpSocketBinding, UdpSocketError,
};
use spin::{Mutex as SpinMutex, RwLock as SpinRwLock};

use crate::{
    ComponentNetworkService, ComponentRuntimeState, DnsError, DnsErrorKind,
    Ipv4Address as KernelIpv4Address, Ipv4Cidr as KernelIpv4Cidr, Ipv4Route as KernelIpv4Route,
    MacAddress, NetworkAdminBackend, NetworkBridgeRequest, NetworkControlError, NetworkErrorDetail,
    NetworkIpAddress, NetworkPortId, PingError, PingErrorKind, PingReply, RegisteredTcpReadBuffer,
    TcpAccepted, TcpError, TcpErrorKind, TcpListener, Timer, UdpBinding, UdpDatagram, UdpError,
    UdpErrorKind,
};
use triomphe::Arc;

const EPHEMERAL_PORT_START: u16 = 49_152;
const EPHEMERAL_PORT_END: u16 = 65_535;

/// Peek the L4 destination port out of a parsed Ethernet frame.
/// Used by the RX demux to route frames to the shard that owns the
/// local port. Returns `None` for non-IP frames (ARP), non-TCP/UDP
/// IP frames (ICMP), or malformed packets — all of which route to
/// shard 0 in the dispatch path because there is no socket-local
/// owner to identify.
fn peek_local_port(frame: &[u8]) -> Option<u16> {
    let ethernet = EthernetFrame::parse(frame)?;
    let payload = ethernet.payload;
    let (protocol, l4_payload) = match ethernet.protocol {
        EthernetProtocol::Ipv4 => {
            let packet = Ipv4Packet::parse(payload)?;
            (packet.protocol, packet.payload)
        }
        EthernetProtocol::Ipv6 => {
            let packet = Ipv6Packet::parse(payload)?;
            (packet.next_header, packet.payload)
        }
        _ => return None,
    };
    match protocol {
        IpProtocol::Tcp => Some(TcpPacket::parse(l4_payload)?.destination_port),
        IpProtocol::Udp => Some(UdpPacket::parse(l4_payload)?.destination_port),
        IpProtocol::Icmp => peek_icmpv4_quoted_local_port(l4_payload),
        IpProtocol::Icmpv6 => peek_icmpv6_quoted_local_port(l4_payload),
    }
}

fn peek_icmpv4_quoted_local_port(bytes: &[u8]) -> Option<u16> {
    let Icmpv4Packet::DestinationUnreachable(unreachable) = Icmpv4Packet::parse(bytes)? else {
        return None;
    };
    let quoted = Ipv4Packet::parse_quoted(unreachable.original)?;
    match quoted.protocol {
        IpProtocol::Tcp => Some(TcpPacket::parse_ports(quoted.payload)?.source),
        IpProtocol::Udp => Some(UdpPacket::parse_ports(quoted.payload)?.source),
        _ => None,
    }
}

fn peek_icmpv6_quoted_local_port(bytes: &[u8]) -> Option<u16> {
    let original = match Icmpv6Packet::parse(bytes)? {
        Icmpv6Packet::DestinationUnreachable(unreachable) => unreachable.original,
        Icmpv6Packet::PacketTooBig(packet_too_big) => packet_too_big.original,
        _ => return None,
    };
    let quoted = Ipv6Packet::parse_quoted(original)?;
    match quoted.next_header {
        IpProtocol::Tcp => Some(TcpPacket::parse_ports(quoted.payload)?.source),
        IpProtocol::Udp => Some(UdpPacket::parse_ports(quoted.payload)?.source),
        _ => None,
    }
}

/// Maps a local port to the index of the shard that owns it. Server
/// listening ports (anything below `EPHEMERAL_PORT_START`) always
/// route to shard 0; ephemeral ports stride across shards so a
/// freshly-allocated outgoing port `EPHEMERAL_PORT_START + k` lands
/// on shard `k % shard_count`. Frames whose port is `None` (ARP,
/// ICMP, …) or unparseable also map to shard 0.
fn shard_idx_for_port(port: Option<u16>, shard_count: usize) -> usize {
    let Some(port) = port else { return 0 };
    if port < EPHEMERAL_PORT_START {
        return 0;
    }
    let stride_idx = port - EPHEMERAL_PORT_START;
    usize::from(stride_idx) % shard_count
}
const INTERNAL_DNS_PORT: u16 = 49_151;
const INTERNAL_DHCP_SOCKET_INDEX: usize = 0;
const INTERNAL_DNS_SOCKET_INDEX: usize = 1;
const LOCAL_NETWORK_PORT: NetworkPortId = NetworkPortId::new(0);
const DHCP_RETRANSMIT_NANOS: u64 = 1_000_000_000;
const MAX_TCP_STREAM_HANDLES: usize = 256;
const MAX_TCP_LISTENER_HANDLES: usize = 64;
const MAX_UDP_SOCKET_HANDLES: usize = 256;
const NETWORK_PROGRESS_WAIT: Duration = Duration::from_micros(50);
// AArch64/HVF local TCP diagnostics showed that matching the borrowed RX
// batch to the virtio polling budget moves receive work in the right
// direction without changing protocol semantics: 64 MiB raw tcp/wasix
// medians went 92/102 ms -> 89/97 ms, and rx-drain ns/event went
// 915/962 -> 838/842. This is not the final network win; it just keeps the
// device-facing receive loop from reacquiring/reposting after every 8 frames.
const NETWORK_RX_BATCH_FRAMES: usize = 32;
const NETWORK_TX_BATCH_FRAMES: usize = MAX_OUTBOUND_FRAMES;
const NETWORK_MIN_POLL_BUDGET: usize = 8;
const NETWORK_MAX_POLL_BUDGET: usize = 128;
const NETWORK_BUSY_POLL_ROUNDS: usize = 8;
const NETWORK_POLLING_TCP_READ_ROUNDS: usize = NETWORK_BUSY_POLL_ROUNDS * 2;
// Helios-vs-Linux flamegraphs at ccc73e8 put the IO-bound hot path in
// `sock_recv` -> component-host TCP read -> `tcp-read-drive-network`, not in
// the syscall wrapper itself. The same run read 64 MiB through 11660 kernel TCP
// reads, only about 28.8 KiB/read despite a 1 MiB guest buffer. For polling NICs
// we therefore drain a bounded burst of already-ready RX frames before returning
// to the guest, amortizing WIT/component-host and TCP drive cost without waiting
// for an interrupt or timer.
const NETWORK_TCP_READ_BURST_ROUNDS: usize = NETWORK_BUSY_POLL_ROUNDS;

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
    state: NetworkShardSet,
    control: NetworkControlPlane,
    /// Adaptive poll budget shared across all shards. Lifted out of
    /// `NetworkShard` so its atomic-load reads on the network poll
    /// fast path do not have to acquire the per-shard SpinMutex.
    poll: NetworkPollState,
    /// `Stack::config().rx_budget` snapshot. The configured stack
    /// budget is set at Stack construction and never mutates, so we
    /// cache it outside the lock and avoid the
    /// `state.with(|s| s.stack.config().rx_budget)` round-trip on
    /// every receive iteration.
    stack_rx_budget: usize,
}

struct NetworkControlPlane {
    ipv4_addresses: SnapshotCell<NetworkIpv4AddressTable>,
    routes: SnapshotCell<RouteTable>,
    neighbors: SnapshotCell<NetworkNeighborTable>,
    dns_servers: SnapshotCell<DhcpDnsServers>,
}

struct SnapshotCell<T> {
    current: SpinRwLock<StdArc<T>>,
}

#[derive(Clone, Debug)]
struct NetworkIpv4AddressTable {
    entries: Vec<Ipv4Cidr>,
}

#[derive(Clone, Debug)]
struct NetworkNeighborTable {
    entries: Vec<NeighborEntry>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TcpStreamId(NonZeroU32);

impl From<TcpStreamId> for u64 {
    fn from(id: TcpStreamId) -> Self {
        u64::from(id.0.get())
    }
}

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

impl From<TcpListenerId> for u64 {
    fn from(id: TcpListenerId) -> Self {
        u64::from(id.0.get())
    }
}

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

impl From<UdpSocketId> for u64 {
    fn from(id: UdpSocketId) -> Self {
        u64::from(id.0.get())
    }
}

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

struct NetworkShard {
    stack: Box<Stack>,
    /// This shard's index inside the parent `NetworkShardSet`.
    /// Encoded into every public socket id this shard mints so the
    /// inverse mapping `(id - 1) % shard_count == shard_idx` can
    /// route operations back to the owning shard without an extra
    /// table lookup.
    shard_idx: usize,
    /// Total number of shards in the parent set. Required for the
    /// stride-based handle encoding.
    shard_count: usize,
    next_tcp_local_port: u16,
    next_udp_local_port: u16,
    tcp_streams: HandleSlab<helios_netstack::SocketId, MAX_TCP_STREAM_HANDLES>,
    tcp_listeners: HandleSlab<TcpListenerState, MAX_TCP_LISTENER_HANDLES>,
    udp_sockets: HandleSlab<UdpSocketState, MAX_UDP_SOCKET_HANDLES>,
    dhcp: DhcpClientState,
    dns_servers: DhcpDnsServers,
    next_dns_query_id: u16,
}

/// A set of `NetworkShard` instances laid out per-CPU.
///
/// Cache-line padding around each shard avoids false sharing once
/// multiple shards live in the box: without it, two adjacent
/// `SpinMutex` fields would share a cache line and ping-pong on
/// every cross-CPU lock operation. We pay the padding cost in the
/// single-shard build to keep the layout invariant.
#[repr(align(64))]
struct PaddedShard {
    inner: SpinMutex<NetworkShard>,
}

struct NetworkShardSet {
    shards: Box<[PaddedShard]>,
}

impl NetworkShardSet {
    /// Builds a shard set sized to `shard_count`. Each shard owns
    /// an independent `NetworkShard`, produced by `factory(i)` so
    /// the ctor can stagger per-shard fields (port allocator base,
    /// DHCP transaction id) across the set.
    fn new<F>(shard_count: usize, mut factory: F) -> Self
    where
        F: FnMut(usize) -> NetworkShard,
    {
        assert!(shard_count != 0, "network shard count must be non-zero");
        let mut shards: Vec<PaddedShard> = Vec::with_capacity(shard_count);
        for index in 0..shard_count {
            shards.push(PaddedShard {
                inner: SpinMutex::new(factory(index)),
            });
        }
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Picks the shard responsible for an unqualified operation
    /// (control-plane queries, admin commands that target every
    /// shard, etc.). Implemented in terms of `shard_for_handle(0)`
    /// so the routing rule lives in exactly one place.
    #[inline]
    fn shard_for_default(&self) -> &SpinMutex<NetworkShard> {
        // Handle id 1 is the canonical "shard 0" id under the
        // stride encoding (slot 0 in shard 0 -> id = 1), so
        // shard_for_default and shard_for_handle agree on shard 0.
        self.shard_for_handle(1u64)
    }

    /// Picks the shard owning a given socket / connection handle
    /// using the inverse of `NetworkShard::encode_handle_id`:
    /// `(handle - 1) % shard_count == shard_idx`.
    #[inline]
    fn shard_for_handle<H: Into<u64>>(&self, handle: H) -> &SpinMutex<NetworkShard> {
        let handle: u64 = handle.into();
        let normalized = handle.saturating_sub(1) as usize;
        let idx = normalized % self.shards.len();
        &self.shards[idx].inner
    }

    /// Locks shard `idx` directly. Used by the RX demux which has
    /// already computed the target shard from the frame's
    /// destination port; bypasses the handle-encoding round-trip.
    #[inline]
    fn shard_at(&self, idx: usize) -> &SpinMutex<NetworkShard> {
        &self.shards[idx].inner
    }

    /// Picks the shard that should receive a new socket created on
    /// the given processor. Future RX traffic for the socket's
    /// stride-allocated ephemeral port will demux back to this
    /// shard, so creation must place the slab entry here. Falls
    /// back to shard 0 for processor ids out of the configured
    /// range.
    #[inline]
    fn shard_for_processor(
        &self,
        processor: helios_hal::cpu::ProcessorId,
    ) -> &SpinMutex<NetworkShard> {
        let idx = (processor.id() as usize) % self.shards.len();
        &self.shards[idx].inner
    }

    fn with<R>(&self, f: impl FnOnce(&NetworkShard) -> R) -> R {
        let state = self.shard_for_default().lock();
        f(&state)
    }

    fn with_mut<R>(&self, f: impl FnOnce(&mut NetworkShard) -> R) -> R {
        let mut state = self.shard_for_default().lock();
        f(&mut state)
    }

    /// Locks the shard owning `handle` and runs the closure against
    /// it under `&mut`. Used by every op that takes a socket /
    /// listener handle so the dispatch decision stays in
    /// `shard_for_handle`. Mutable form is the universal one because
    /// every socket op the caller might run (read drains the socket
    /// receive queue, write enqueues, close removes the slab entry,
    /// etc.) needs interior mutation; a read-only sibling would only
    /// be useful for diagnostic peeks the kernel does not currently
    /// expose.
    fn with_handle<H: Into<u64>, R>(&self, handle: H, f: impl FnOnce(&mut NetworkShard) -> R) -> R {
        let mut state = self.shard_for_handle(handle).lock();
        f(&mut state)
    }

    /// Locks the shard owning the given processor and runs the
    /// closure against it. Used by socket-creation paths so the
    /// new socket lives on the processor that minted it; ephemeral
    /// ports allocated under the stride scheme will demux back to
    /// this shard for incoming traffic.
    fn with_processor<R>(
        &self,
        processor: helios_hal::cpu::ProcessorId,
        f: impl FnOnce(&mut NetworkShard) -> R,
    ) -> R {
        let mut state = self.shard_for_processor(processor).lock();
        f(&mut state)
    }

    /// Locks the shard owning a fixed local port (server listener,
    /// explicit UDP bind). Maps via `shard_idx_for_port` so RX
    /// demux for the same port reaches the same shard.
    fn with_local_port<R>(&self, port: u16, f: impl FnOnce(&mut NetworkShard) -> R) -> R {
        let idx = shard_idx_for_port(Some(port), self.shards.len());
        let mut state = self.shards[idx].inner.lock();
        f(&mut state)
    }

    /// Iterates every shard in the set, calling `f` once per shard
    /// under its own lock. Used by control-plane ops that target the
    /// whole stack — clearing routes, listing IPv4 addresses, the
    /// upcoming control task pushing DNS results to all shards, etc.
    fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(&mut NetworkShard),
    {
        for shard in &self.shards {
            let mut guard = shard.inner.lock();
            f(&mut guard);
        }
    }

    fn min_tcp_deadline_nanos(&self) -> Option<u64> {
        let mut next = None;
        self.for_each(|shard| {
            if let Some(deadline) = shard.stack.next_tcp_deadline().map(StackInstant::nanos) {
                next = Some(next.map_or(deadline, |current: u64| current.min(deadline)));
            }
        });
        next
    }
}

impl NetworkControlPlane {
    fn new() -> Self {
        Self {
            ipv4_addresses: SnapshotCell::new(NetworkIpv4AddressTable::new()),
            routes: SnapshotCell::new(RouteTable::new()),
            neighbors: SnapshotCell::new(NetworkNeighborTable::new()),
            dns_servers: SnapshotCell::new(DhcpDnsServers::new()),
        }
    }

    fn synchronize_shard(&self, shard: &mut NetworkShard) {
        shard
            .stack
            .replace_ipv4_addresses(self.ipv4_addresses.load_full().entries.iter().copied());
        shard
            .stack
            .replace_routes(self.routes.load_full().as_ref().clone());
        shard
            .stack
            .replace_neighbors(self.neighbors.load_full().entries.iter().copied());
        shard.dns_servers = self.dns_servers.load_full().as_ref().clone();
    }

    fn publish_from_shard(&self, shard: &NetworkShard) {
        self.ipv4_addresses
            .store(StdArc::new(NetworkIpv4AddressTable {
                entries: shard.stack.ipv4_addresses().collect(),
            }));
        self.routes.store(StdArc::new(shard.stack.routes().clone()));
        self.neighbors.store(StdArc::new(NetworkNeighborTable {
            entries: shard.stack.neighbors().collect(),
        }));
        self.dns_servers
            .store(StdArc::new(shard.dns_servers.clone()));
    }

    fn update_routes(
        &self,
        update: impl FnOnce(&mut RouteTable) -> Result<(), NetworkControlError>,
    ) -> Result<(), NetworkControlError> {
        let mut routes = self.routes.load_full().as_ref().clone();
        update(&mut routes)?;
        self.routes.store(StdArc::new(routes));
        Ok(())
    }

    fn update_ipv4_addresses(
        &self,
        update: impl FnOnce(&mut NetworkIpv4AddressTable) -> Result<(), NetworkControlError>,
    ) -> Result<(), NetworkControlError> {
        let mut addresses = self.ipv4_addresses.load_full().as_ref().clone();
        update(&mut addresses)?;
        self.ipv4_addresses.store(StdArc::new(addresses));
        Ok(())
    }

    fn update_neighbors(&self, update: impl FnOnce(&mut NetworkNeighborTable)) {
        let mut neighbors = self.neighbors.load_full().as_ref().clone();
        update(&mut neighbors);
        self.neighbors.store(StdArc::new(neighbors));
    }

    fn list_ipv4_routes(&self) -> Vec<KernelIpv4Route> {
        self.routes
            .load_full()
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

    fn list_ipv4_addresses(&self) -> Vec<KernelIpv4Cidr> {
        self.ipv4_addresses
            .load_full()
            .entries
            .iter()
            .copied()
            .map(map_ipv4_cidr)
            .collect()
    }
}

impl<T> SnapshotCell<T> {
    fn new(value: T) -> Self {
        Self {
            current: SpinRwLock::new(StdArc::new(value)),
        }
    }

    fn load_full(&self) -> StdArc<T> {
        self.current.read().clone()
    }

    fn store(&self, value: StdArc<T>) {
        *self.current.write() = value;
    }
}

impl NetworkIpv4AddressTable {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn add(&mut self, address: Ipv4Cidr) {
        if !self.entries.contains(&address) {
            self.entries.push(address);
        }
    }

    fn remove(&mut self, address: Ipv4Cidr) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|existing| *existing == address)
        {
            self.entries.remove(index);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

impl NetworkNeighborTable {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn learn(&mut self, entry: NeighborEntry) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|existing| existing.ip == entry.ip)
        {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }
}

#[derive(Clone, Copy)]
struct NetworkPollBudget {
    rx_frames: usize,
    tx_completions: usize,
    tx_frames: usize,
}

#[derive(Clone, Copy)]
struct NetworkPollProgress {
    received_frames: usize,
    reclaimed_tx: usize,
    transmitted_frames: usize,
}

#[derive(Clone, Copy)]
struct NetworkTcpReadProbe {
    stream: TcpStreamId,
    max_bytes: usize,
    profile_prefix: &'static str,
}

struct NetworkPollOutcome {
    progress: NetworkPollProgress,
    budget: NetworkPollBudget,
    tcp_read: Option<Result<TcpReadProgress, TcpError>>,
}

#[derive(Clone, Copy)]
enum NetworkTransmitStop {
    Drained,
    Budget,
    RingFull,
}

#[derive(Clone, Copy)]
enum NetworkPollSource {
    Pump,
    Ping,
    Dns,
    Tcp,
    Udp,
    Configuration,
}

impl NetworkPollProgress {
    const fn is_idle(self) -> bool {
        self.received_frames == 0 && self.reclaimed_tx == 0 && self.transmitted_frames == 0
    }

    const fn receive_saturated(self, budget: NetworkPollBudget) -> bool {
        budget.rx_frames != 0 && self.received_frames >= budget.rx_frames
    }

    const fn saturated(self, budget: NetworkPollBudget) -> bool {
        self.receive_saturated(budget)
            || self.reclaimed_tx >= budget.tx_completions
            || self.transmitted_frames >= budget.tx_frames
    }
}

impl NetworkTransmitStop {
    const fn profile_phase(self, source: NetworkPollSource) -> &'static str {
        match (self, source) {
            (Self::Drained, NetworkPollSource::Pump) => "tx-submit-drained-pump",
            (Self::Drained, NetworkPollSource::Ping) => "tx-submit-drained-ping",
            (Self::Drained, NetworkPollSource::Dns) => "tx-submit-drained-dns",
            (Self::Drained, NetworkPollSource::Tcp) => "tx-submit-drained-tcp",
            (Self::Drained, NetworkPollSource::Udp) => "tx-submit-drained-udp",
            (Self::Drained, NetworkPollSource::Configuration) => "tx-submit-drained-configuration",
            (Self::Budget, NetworkPollSource::Pump) => "tx-submit-budget-pump",
            (Self::Budget, NetworkPollSource::Ping) => "tx-submit-budget-ping",
            (Self::Budget, NetworkPollSource::Dns) => "tx-submit-budget-dns",
            (Self::Budget, NetworkPollSource::Tcp) => "tx-submit-budget-tcp",
            (Self::Budget, NetworkPollSource::Udp) => "tx-submit-budget-udp",
            (Self::Budget, NetworkPollSource::Configuration) => "tx-submit-budget-configuration",
            (Self::RingFull, NetworkPollSource::Pump) => "tx-submit-ring-full-pump",
            (Self::RingFull, NetworkPollSource::Ping) => "tx-submit-ring-full-ping",
            (Self::RingFull, NetworkPollSource::Dns) => "tx-submit-ring-full-dns",
            (Self::RingFull, NetworkPollSource::Tcp) => "tx-submit-ring-full-tcp",
            (Self::RingFull, NetworkPollSource::Udp) => "tx-submit-ring-full-udp",
            (Self::RingFull, NetworkPollSource::Configuration) => {
                "tx-submit-ring-full-configuration"
            }
        }
    }
}

impl NetworkPollSource {
    const fn tx_immediate_phase(self) -> &'static str {
        match self {
            Self::Pump => "tx-submit-immediate-pump",
            Self::Ping => "tx-submit-immediate-ping",
            Self::Dns => "tx-submit-immediate-dns",
            Self::Tcp => "tx-submit-immediate-tcp",
            Self::Udp => "tx-submit-immediate-udp",
            Self::Configuration => "tx-submit-immediate-configuration",
        }
    }

    const fn tx_immediate_device_phase(self) -> &'static str {
        match self {
            Self::Pump => "tx-submit-immediate-device-pump",
            Self::Ping => "tx-submit-immediate-device-ping",
            Self::Dns => "tx-submit-immediate-device-dns",
            Self::Tcp => "tx-submit-immediate-device-tcp",
            Self::Udp => "tx-submit-immediate-device-udp",
            Self::Configuration => "tx-submit-immediate-device-configuration",
        }
    }

    const fn tx_batch_collect_phase(self) -> &'static str {
        match self {
            Self::Pump => "tx-submit-batch-collect-pump",
            Self::Ping => "tx-submit-batch-collect-ping",
            Self::Dns => "tx-submit-batch-collect-dns",
            Self::Tcp => "tx-submit-batch-collect-tcp",
            Self::Udp => "tx-submit-batch-collect-udp",
            Self::Configuration => "tx-submit-batch-collect-configuration",
        }
    }

    const fn tx_batch_device_phase(self) -> &'static str {
        match self {
            Self::Pump => "tx-submit-batch-device-pump",
            Self::Ping => "tx-submit-batch-device-ping",
            Self::Dns => "tx-submit-batch-device-dns",
            Self::Tcp => "tx-submit-batch-device-tcp",
            Self::Udp => "tx-submit-batch-device-udp",
            Self::Configuration => "tx-submit-batch-device-configuration",
        }
    }

    const fn rx_drain_phase(self) -> &'static str {
        match self {
            Self::Pump => "rx-drain-pump",
            Self::Ping => "rx-drain-ping",
            Self::Dns => "rx-drain-dns",
            Self::Tcp => "rx-drain-tcp",
            Self::Udp => "rx-drain-udp",
            Self::Configuration => "rx-drain-configuration",
        }
    }

    const fn tx_reclaim_phase(self) -> &'static str {
        match self {
            Self::Pump => "tx-reclaim-pump",
            Self::Ping => "tx-reclaim-ping",
            Self::Dns => "tx-reclaim-dns",
            Self::Tcp => "tx-reclaim-tcp",
            Self::Udp => "tx-reclaim-udp",
            Self::Configuration => "tx-reclaim-configuration",
        }
    }

    const fn tcp_drive_phase(self) -> &'static str {
        match self {
            Self::Pump => "tcp-drive-pump",
            Self::Ping => "tcp-drive-ping",
            Self::Dns => "tcp-drive-dns",
            Self::Tcp => "tcp-drive-tcp",
            Self::Udp => "tcp-drive-udp",
            Self::Configuration => "tcp-drive-configuration",
        }
    }
}

/// Adaptive poll budget. The base values are constants from device
/// capabilities; the live budgets are tuned in `complete()` based on
/// per-cycle progress so a saturated stack widens its budget while
/// an idle stack contracts it.
///
/// Reads happen on every network poll iteration. Storing the live
/// fields as `AtomicUsize` keeps that read off the lock — pre-Phase
/// 4.x the read had to acquire the global `SpinMutex<NetworkShard>`
/// just to copy three integers. Writes only happen in `complete()`,
/// which is racy by design (concurrent shards complete and update
/// the same atomic) but the outcome is a heuristic so the natural
/// last-writer-wins is acceptable.
struct NetworkPollState {
    base_rx_budget: usize,
    base_tx_completion_budget: usize,
    base_tx_frame_budget: usize,
    rx_budget: AtomicUsize,
    tx_completion_budget: AtomicUsize,
    tx_frame_budget: AtomicUsize,
}

struct NetworkPumpCadence {
    busy_rounds: usize,
}

#[derive(Clone, Copy)]
struct NetworkPerfStart {
    nanos: u64,
    counters: HardwarePerfCounters,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NetworkPumpAction {
    Continue,
    Yield,
    Wait,
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

    fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.slots.get_mut(index).and_then(Option::as_mut)
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
    stack_socket: helios_netstack::SocketId,
    local_port: u16,
}

#[derive(Clone, Copy)]
struct UdpSocketState {
    stack_socket: helios_netstack::UdpSocketId,
    binding: UdpSocketBinding,
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

fn map_ip_address(address: IpAddress) -> NetworkIpAddress {
    match address {
        IpAddress::Ipv4(address) => NetworkIpAddress::Ipv4(map_ipv4_address(address)),
        IpAddress::Ipv6(address) => NetworkIpAddress::Ipv6(address),
    }
}

fn map_network_ip_address(address: NetworkIpAddress) -> IpAddress {
    match address {
        NetworkIpAddress::Ipv4(address) => IpAddress::Ipv4(map_kernel_ipv4_address(address)),
        NetworkIpAddress::Ipv6(address) => IpAddress::Ipv6(address),
    }
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

fn map_tcp_connect_terminal_error(error: TcpConnectTerminalError) -> TcpError {
    match error {
        TcpConnectTerminalError::RemotePortUnreachable => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpRemotePortUnreachable,
        },
        TcpConnectTerminalError::RemoteNetworkUnreachable => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpRemoteNetworkUnreachable,
        },
        TcpConnectTerminalError::RemoteHostUnreachable => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpRemoteHostUnreachable,
        },
        TcpConnectTerminalError::RemoteProtocolUnreachable => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpRemoteProtocolUnreachable,
        },
        TcpConnectTerminalError::RemoteCommunicationProhibited => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpRemoteCommunicationProhibited,
        },
        TcpConnectTerminalError::Closed => TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpClosedDuringConnect,
        },
    }
}

fn map_udp_socket_error(error: UdpSocketError) -> UdpError {
    match error {
        UdpSocketError::RemotePortUnreachable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpRemotePortUnreachable,
        },
        UdpSocketError::RemoteNetworkUnreachable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpRemoteNetworkUnreachable,
        },
        UdpSocketError::RemoteHostUnreachable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpRemoteHostUnreachable,
        },
        UdpSocketError::RemoteProtocolUnreachable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpRemoteProtocolUnreachable,
        },
        UdpSocketError::RemoteCommunicationProhibited => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpRemoteCommunicationProhibited,
        },
    }
}

fn map_udp_bind_error(error: StackError) -> UdpError {
    match error {
        StackError::AddressInUse => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpPortInUse,
        },
        StackError::UnknownSocket => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownUdpSocket,
        },
        StackError::AddressFamilyMismatch | StackError::RemoteAddressMismatch => UdpError {
            kind: UdpErrorKind::Unsupported,
            detail: NetworkErrorDetail::UnsupportedAddressFamily,
        },
        StackError::Unroutable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::NetworkServiceUnavailable,
        },
        StackError::PacketTooLarge => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpDatagramTooLarge,
        },
        _ => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpBindFailed,
        },
    }
}

fn map_udp_connect_error(error: StackError) -> UdpError {
    match error {
        StackError::AddressInUse => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpPortInUse,
        },
        StackError::UnknownSocket => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownUdpSocket,
        },
        StackError::AddressFamilyMismatch | StackError::RemoteAddressMismatch => UdpError {
            kind: UdpErrorKind::Unsupported,
            detail: NetworkErrorDetail::UnsupportedAddressFamily,
        },
        StackError::Unroutable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::NetworkServiceUnavailable,
        },
        StackError::PacketTooLarge => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpDatagramTooLarge,
        },
        _ => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpConnectFailed,
        },
    }
}

fn map_udp_disconnect_error(error: StackError) -> UdpError {
    match error {
        StackError::AddressInUse => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpPortInUse,
        },
        StackError::UnknownSocket => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownUdpSocket,
        },
        StackError::AddressFamilyMismatch | StackError::RemoteAddressMismatch => UdpError {
            kind: UdpErrorKind::Unsupported,
            detail: NetworkErrorDetail::UnsupportedAddressFamily,
        },
        StackError::Unroutable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::NetworkServiceUnavailable,
        },
        StackError::PacketTooLarge => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpDatagramTooLarge,
        },
        _ => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpDisconnectFailed,
        },
    }
}

fn map_udp_send_error(error: StackError) -> UdpError {
    match error {
        StackError::UnknownSocket => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownUdpSocket,
        },
        StackError::AddressFamilyMismatch | StackError::RemoteAddressMismatch => UdpError {
            kind: UdpErrorKind::Unsupported,
            detail: NetworkErrorDetail::UnsupportedAddressFamily,
        },
        StackError::Unroutable => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::NetworkServiceUnavailable,
        },
        StackError::PacketTooLarge => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpDatagramTooLarge,
        },
        _ => UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpQueueFailed,
        },
    }
}

fn multicast_join_error(error: StackError) -> UdpError {
    let detail = match error {
        StackError::MulticastInterfaceUnavailable => {
            NetworkErrorDetail::UdpMulticastInterfaceUnavailable
        }
        StackError::InvalidMulticastGroup | StackError::MulticastMembershipTableFull => {
            NetworkErrorDetail::UdpMulticastJoinFailed
        }
        _ => NetworkErrorDetail::UdpMulticastJoinFailed,
    };
    UdpError {
        kind: UdpErrorKind::Unavailable,
        detail,
    }
}

fn multicast_leave_error(error: StackError) -> UdpError {
    let detail = match error {
        StackError::MulticastInterfaceUnavailable => {
            NetworkErrorDetail::UdpMulticastInterfaceUnavailable
        }
        StackError::InvalidMulticastGroup | StackError::MulticastMembershipNotFound => {
            NetworkErrorDetail::UdpMulticastLeaveFailed
        }
        _ => NetworkErrorDetail::UdpMulticastLeaveFailed,
    };
    UdpError {
        kind: UdpErrorKind::Unavailable,
        detail,
    }
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
        let rx_poll_budget = capabilities.events.rx_poll_budget;
        let tx_completion_budget = capabilities.events.tx_completion_budget;
        let rx_checksum_offload = RxChecksumOffload::new(
            capabilities.checksum.rx_ipv4,
            capabilities.checksum.rx_tcp,
            capabilities.checksum.rx_udp,
        );
        let mac = device.mac_address();
        let max_frame_len = device.max_frame_len();
        // One NetworkShard per processor. Each shard owns an
        // independent Stack, socket slabs, DHCP/DNS state, and port
        // allocator. Routes / ARP / neighbour entries are still
        // per-shard (each Stack maintains its own caches off the
        // RX path it observes); cross-shard route / DNS broadcasts
        // are a follow-up. The `transaction_id` is staggered per
        // shard so DHCP DISCOVER messages from different shards do
        // not race over the same XID.
        let shard_count = cpu.processor_count().max(1);
        // We probe the configured rx_budget once via a throwaway
        // Stack — every shard's Stack uses the same StackConfig so
        // the value is identical across shards.
        let stack_config = StackConfig::new(mac, max_frame_len)
            .with_rx_budget(rx_poll_budget)
            .with_rx_checksum_offload(rx_checksum_offload);
        let stack_rx_budget = Stack::new(stack_config).config().rx_budget;
        let state = NetworkShardSet::new(shard_count, |index| {
            let staggered_xid = transaction_id.wrapping_add(index as u32);
            NetworkShard::new(
                mac,
                max_frame_len,
                rx_poll_budget,
                rx_checksum_offload,
                staggered_xid,
                index,
                shard_count,
            )
        });
        Self {
            inner: Arc::new(NetworkServiceInner {
                cpu,
                runtime_state,
                timer,
                state,
                control: NetworkControlPlane::new(),
                poll: NetworkPollState::new(rx_poll_budget, tx_completion_budget, rx_poll_budget),
                stack_rx_budget,
                device,
            }),
        }
    }

    /// Returns the number of `NetworkShard` instances backing this
    /// service. Equal to `Cpu::processor_count()` at construction.
    /// Surfaced for diagnostics, perf bench dispatch, and the
    /// inspector statistics path.
    pub fn shard_count(&self) -> usize {
        self.inner.state.shard_count()
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

    pub async fn tcp_connect_address(
        &self,
        remote_address: NetworkIpAddress,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        self.execute_tcp_connect_address(
            map_network_ip_address(remote_address),
            port,
            local_port,
            timeout_nanos,
        )
        .await
    }

    pub async fn tcp_listen(
        &self,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: u16,
    ) -> Result<TcpListener<TcpListenerId>, TcpError> {
        self.execute_tcp_listen(local_address, local_port, backlog)
            .await
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

    pub async fn tcp_read_into(
        &self,
        stream: TcpStreamId,
        buffer: RegisteredTcpReadBuffer<'_>,
        timeout_nanos: u64,
    ) -> Result<Option<usize>, TcpError> {
        self.execute_tcp_read_into(stream, buffer, timeout_nanos)
            .await
    }

    pub async fn tcp_shutdown_send(&self, stream: TcpStreamId) -> Result<(), TcpError> {
        self.inner
            .state
            .with_handle(stream, |state| state.shutdown_tcp_send(stream))?;
        self.drive_tcp().await
    }

    pub async fn tcp_close(&self, stream: TcpStreamId) {
        self.inner.state.with_handle(stream, |state| {
            state.remove_tcp_stream(stream);
        });
    }

    pub async fn run_packet_pump(&self) -> ! {
        let mut cadence = NetworkPumpCadence::new();
        loop {
            match self.poll_network_once(NetworkPollSource::Pump).await {
                Ok((progress, budget)) => match cadence.complete(progress, budget) {
                    NetworkPumpAction::Continue => {}
                    NetworkPumpAction::Yield => crate::yield_now().await,
                    NetworkPumpAction::Wait => self.wait_for_progress(NETWORK_PROGRESS_WAIT).await,
                },
                Err(error) => {
                    cadence.reset();
                    tracing::debug!(?error, "network packet pump failed to drive device");
                    self.wait_for_progress(NETWORK_PROGRESS_WAIT).await;
                }
            }
        }
    }

    pub async fn udp_bind(&self, local_port: u16) -> Result<UdpBinding<UdpSocketId>, UdpError> {
        self.execute_udp_bind(local_port).await
    }

    pub fn udp_connect(
        &self,
        socket: UdpSocketId,
        remote_address: NetworkIpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        self.execute_udp_connect(socket, remote_address, port)
    }

    pub fn udp_disconnect(&self, socket: UdpSocketId) -> Result<(), UdpError> {
        self.execute_udp_disconnect(socket)
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

    pub async fn udp_send_address(
        &self,
        socket: UdpSocketId,
        remote_address: NetworkIpAddress,
        port: u16,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<u64, UdpError> {
        self.execute_udp_send_address(socket, remote_address, port, bytes, timeout_nanos)
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
        self.inner.state.with_handle(socket, |state| {
            state.remove_udp_socket(socket);
        });
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
        let query_id = self.inner.state.with_mut(NetworkShard::next_dns_query_id);

        loop {
            self.drive_dns().await?;
            let now = StackInstant::from_nanos(self.now_nanos());
            let query = self.inner.state.with_mut(
                |state| -> Result<Option<Vec<Ipv4Address>>, DnsError> {
                    state.send_dns_query(query_id, host, now)?;
                    state.take_dns_response(query_id)
                },
            )?;
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
        let destination = self.resolve_host_tcp(host, deadline_nanos).await?;
        self.execute_tcp_connect_address_until(destination, port, local_port, deadline_nanos)
            .await
    }

    async fn execute_tcp_connect_address(
        &self,
        destination: IpAddress,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        self.execute_tcp_connect_address_until(destination, port, local_port, deadline_nanos)
            .await
    }

    async fn execute_tcp_connect_address_until(
        &self,
        destination: IpAddress,
        port: u16,
        local_port: u16,
        deadline_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        if matches!(destination, IpAddress::Ipv4(_)) {
            self.wait_for_ipv4_tcp(deadline_nanos).await?;
        }
        // A caller-selected local port already has a deterministic
        // shard owner; route there so RX demux and handle routing agree.
        // Port 0 allocates from the current processor's shard.
        let stream = if local_port == 0 {
            let processor = self.inner.cpu.current_processor();
            self.inner.state.with_processor(processor, |state| {
                state.start_tcp_connect(destination, port, local_port)
            })
        } else {
            self.inner.state.with_local_port(local_port, |state| {
                state.start_tcp_connect(destination, port, local_port)
            })
        }?;

        loop {
            self.drive_tcp().await?;
            let now_nanos = self.now_nanos();
            let poll_connect = self.inner.state.with_handle(stream, |state| {
                match state.poll_tcp_connect(stream) {
                    Ok(TcpConnectProgress::Connected) => Ok(TcpConnectProgress::Connected),
                    Ok(TcpConnectProgress::Pending) => {
                        if now_nanos >= deadline_nanos {
                            state.remove_tcp_stream(stream);
                            Err(TcpError {
                                kind: TcpErrorKind::Timeout,
                                detail: NetworkErrorDetail::TcpConnectTimeout,
                            })
                        } else {
                            Ok(TcpConnectProgress::Pending)
                        }
                    }
                    Err(error) => {
                        state.remove_tcp_stream(stream);
                        Err(error)
                    }
                }
            })?;
            if matches!(poll_connect, TcpConnectProgress::Connected) {
                return Ok(stream);
            }
            self.wait_for_tcp_progress(deadline_nanos).await;
        }
    }

    async fn execute_tcp_listen(
        &self,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: u16,
    ) -> Result<TcpListener<TcpListenerId>, TcpError> {
        let backlog = TcpListenBacklog::try_new(backlog).map_err(|_| TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpListenStartFailed,
        })?;
        // Listener lives on the shard that owns its local port: a
        // well-known port (< EPHEMERAL_PORT_START) goes to shard 0,
        // an explicit ephemeral port stride-maps to its owner. RX
        // demux for the same port routes back here.
        self.inner.state.with_local_port(local_port, |state| {
            state.start_tcp_listen(local_address, local_port, backlog)
        })
    }

    async fn execute_tcp_accept(
        &self,
        listener: TcpListenerId,
        timeout_nanos: u64,
    ) -> Result<TcpAccepted<TcpStreamId>, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        loop {
            self.drive_tcp().await?;
            let accepted = self
                .inner
                .state
                .with_handle(listener, |state| state.poll_tcp_accept(listener))?;
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
            let written = self.inner.state.with_handle(stream, |state| {
                state.try_write_tcp_bytes(stream, &mut bytes)
            })?;
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
        let max_bytes = max_bytes as usize;
        loop {
            match self.poll_tcp_read_once(stream, max_bytes, "tcp-read-initial")? {
                TcpReadProgress::Data(bytes) => return Ok(Some(bytes)),
                TcpReadProgress::Eof => return Ok(None),
                TcpReadProgress::Pending => {}
            }

            let drive_started = self.profile_start();
            let read = self.drive_tcp_read_burst(stream, max_bytes).await?;
            self.record_network_profile("tcp-read-drive-network", drive_started);
            match read {
                TcpReadProgress::Data(bytes) => return Ok(Some(bytes)),
                TcpReadProgress::Eof => return Ok(None),
                TcpReadProgress::Pending => {}
            }
            match self
                .poll_tcp_read_without_interrupt_sleep(stream, max_bytes, deadline_nanos)
                .await?
            {
                TcpReadProgress::Data(bytes) => return Ok(Some(bytes)),
                TcpReadProgress::Eof => return Ok(None),
                TcpReadProgress::Pending => {}
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(TcpError {
                    kind: TcpErrorKind::Timeout,
                    detail: NetworkErrorDetail::TcpReadTimeout,
                });
            }
            let wait_started = self.profile_start();
            self.wait_for_tcp_progress(deadline_nanos).await;
            self.record_network_profile("tcp-read-wait", wait_started);
        }
    }

    async fn execute_tcp_read_into(
        &self,
        stream: TcpStreamId,
        mut buffer: RegisteredTcpReadBuffer<'_>,
        timeout_nanos: u64,
    ) -> Result<Option<usize>, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        let max_bytes = buffer.capacity();
        loop {
            match self.poll_tcp_read_into_once(stream, &mut buffer, "tcp-read-into-initial")? {
                TcpReadIntoState::Data(bytes) => return Ok(Some(bytes)),
                TcpReadIntoState::Eof => return Ok(None),
                TcpReadIntoState::Pending => {}
            }

            let drive_started = self.profile_start();
            self.drive_tcp_read_network_burst(max_bytes).await?;
            self.record_network_profile("tcp-read-into-drive-network", drive_started);
            match self.poll_tcp_read_into_once(stream, &mut buffer, "tcp-read-into-after-drive")? {
                TcpReadIntoState::Data(bytes) => return Ok(Some(bytes)),
                TcpReadIntoState::Eof => return Ok(None),
                TcpReadIntoState::Pending => {}
            }
            match self
                .poll_tcp_read_into_without_interrupt_sleep(stream, &mut buffer, deadline_nanos)
                .await?
            {
                TcpReadIntoState::Data(bytes) => return Ok(Some(bytes)),
                TcpReadIntoState::Eof => return Ok(None),
                TcpReadIntoState::Pending => {}
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(TcpError {
                    kind: TcpErrorKind::Timeout,
                    detail: NetworkErrorDetail::TcpReadTimeout,
                });
            }
            let wait_started = self.profile_start();
            self.wait_for_tcp_progress(deadline_nanos).await;
            self.record_network_profile("tcp-read-into-wait", wait_started);
        }
    }

    fn poll_tcp_read_once(
        &self,
        stream: TcpStreamId,
        max_bytes: usize,
        profile_prefix: &'static str,
    ) -> Result<TcpReadProgress, TcpError> {
        let started = self.profile_start();
        let now = StackInstant::from_nanos(self.now_nanos());
        let read = self
            .inner
            .state
            .with_handle(stream, |state| state.poll_tcp_read(stream, max_bytes, now))?;
        self.record_tcp_read_progress(profile_prefix, started, &read);
        Ok(read)
    }

    fn poll_tcp_read_into_once(
        &self,
        stream: TcpStreamId,
        buffer: &mut RegisteredTcpReadBuffer<'_>,
        profile_prefix: &'static str,
    ) -> Result<TcpReadIntoState, TcpError> {
        let started = self.profile_start();
        let now = StackInstant::from_nanos(self.now_nanos());
        let read = self.inner.state.with_handle(stream, |state| {
            state.poll_tcp_read_into(stream, buffer, now)
        })?;
        self.record_tcp_read_into_progress(profile_prefix, started, &read);
        Ok(read)
    }

    async fn poll_tcp_read_without_interrupt_sleep(
        &self,
        stream: TcpStreamId,
        max_bytes: usize,
        deadline_nanos: u64,
    ) -> Result<TcpReadProgress, TcpError> {
        let capabilities = self.inner.device.capabilities().events;
        if !capabilities.polling || capabilities.interrupts {
            return Ok(TcpReadProgress::Pending);
        }

        for _ in 0..NETWORK_POLLING_TCP_READ_ROUNDS {
            if self.now_nanos() >= deadline_nanos {
                return Ok(TcpReadProgress::Pending);
            }
            let yield_started = self.profile_start();
            crate::yield_now().await;
            self.record_network_profile("tcp-read-polling-yield", yield_started);

            let drive_started = self.profile_start();
            let outcome = self
                .poll_network_once_with_tcp_read(
                    NetworkPollSource::Tcp,
                    Some(NetworkTcpReadProbe {
                        stream,
                        max_bytes,
                        profile_prefix: "tcp-read-polling",
                    }),
                    true,
                )
                .await
                .map_err(|error| {
                    TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
                })?;
            self.record_network_profile("tcp-read-polling-drive-network", drive_started);
            let read = outcome
                .tcp_read
                .expect("TCP read probe did not produce a read result")?;
            match read {
                ready @ (TcpReadProgress::Data(_) | TcpReadProgress::Eof) => return Ok(ready),
                TcpReadProgress::Pending => {}
            }
            // AArch64/HVF local-loopback profiling at 028b6ef showed fixed
            // polling reads spending 121.5 ms across 8173 network drives while
            // only 115 probes became ready. The cheap Helios syscall path is
            // not the bottleneck there; empty device/stack drives are. Keep
            // spinning only while RX/TX/completion work is actually moving.
            if outcome.progress.is_idle() {
                break;
            }
        }
        Ok(TcpReadProgress::Pending)
    }

    async fn poll_tcp_read_into_without_interrupt_sleep(
        &self,
        stream: TcpStreamId,
        buffer: &mut RegisteredTcpReadBuffer<'_>,
        deadline_nanos: u64,
    ) -> Result<TcpReadIntoState, TcpError> {
        let capabilities = self.inner.device.capabilities().events;
        if !capabilities.polling || capabilities.interrupts {
            return Ok(TcpReadIntoState::Pending);
        }

        for _ in 0..NETWORK_POLLING_TCP_READ_ROUNDS {
            if self.now_nanos() >= deadline_nanos {
                return Ok(TcpReadIntoState::Pending);
            }
            let yield_started = self.profile_start();
            crate::yield_now().await;
            self.record_network_profile("tcp-read-into-polling-yield", yield_started);

            let drive_started = self.profile_start();
            let outcome = self
                .poll_network_once(NetworkPollSource::Tcp)
                .await
                .map_err(|error| {
                    TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
                })?;
            self.record_network_profile("tcp-read-into-polling-drive-network", drive_started);
            match self.poll_tcp_read_into_once(stream, buffer, "tcp-read-into-polling")? {
                ready @ (TcpReadIntoState::Data(_) | TcpReadIntoState::Eof) => return Ok(ready),
                TcpReadIntoState::Pending => {}
            }
            if outcome.0.is_idle() {
                break;
            }
        }
        Ok(TcpReadIntoState::Pending)
    }

    async fn execute_udp_bind(&self, local_port: u16) -> Result<UdpBinding<UdpSocketId>, UdpError> {
        // local_port == 0 means "allocate ephemeral"; the binding
        // should live on the current processor's shard so its
        // freshly stride-allocated port demuxes RX traffic back
        // here. A non-zero `local_port` is fixed by the caller, so
        // route by the port's stride owner instead.
        if local_port == 0 {
            let processor = self.inner.cpu.current_processor();
            self.inner
                .state
                .with_processor(processor, |state| state.start_udp_bind(local_port))
        } else {
            self.inner
                .state
                .with_local_port(local_port, |state| state.start_udp_bind(local_port))
        }
    }

    fn execute_udp_connect(
        &self,
        socket: UdpSocketId,
        remote_address: NetworkIpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        self.synchronize_control_plane();
        let destination = map_network_ip_address(remote_address);
        self.inner.state.with_handle(socket, |state| {
            state.connect_udp_socket(socket, destination, port)
        })
    }

    fn execute_udp_disconnect(&self, socket: UdpSocketId) -> Result<(), UdpError> {
        self.inner
            .state
            .with_handle(socket, |state| state.disconnect_udp_socket(socket))
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
        let destination = self.resolve_host_udp(host, deadline_nanos).await?;
        self.execute_udp_send_ipv4(socket, destination, port, bytes, deadline_nanos)
            .await
    }

    async fn execute_udp_send_address(
        &self,
        socket: UdpSocketId,
        remote_address: NetworkIpAddress,
        port: u16,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<u64, UdpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        match remote_address {
            NetworkIpAddress::Ipv4(destination) => {
                self.execute_udp_send_ipv4(
                    socket,
                    map_kernel_ipv4_address(destination),
                    port,
                    bytes,
                    deadline_nanos,
                )
                .await
            }
            NetworkIpAddress::Ipv6(destination) => {
                self.execute_udp_send_ip(socket, IpAddress::Ipv6(destination), port, bytes)
                    .await
            }
        }
    }

    async fn execute_udp_send_ipv4(
        &self,
        socket: UdpSocketId,
        destination: Ipv4Address,
        port: u16,
        bytes: &[u8],
        deadline_nanos: u64,
    ) -> Result<u64, UdpError> {
        self.wait_for_ipv4_udp(deadline_nanos).await?;
        self.execute_udp_send_ip(socket, IpAddress::Ipv4(destination), port, bytes)
            .await
    }

    async fn execute_udp_send_ip(
        &self,
        socket: UdpSocketId,
        destination: IpAddress,
        port: u16,
        bytes: &[u8],
    ) -> Result<u64, UdpError> {
        self.synchronize_control_plane();
        let now = StackInstant::from_nanos(self.now_nanos());
        let written = self.inner.state.with_handle(socket, |state| {
            state.try_send_udp(socket, destination, port, bytes, now)
        })?;
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
            let received = self.inner.state.with_handle(socket, |state| {
                state.poll_udp_receive(socket, max_bytes as usize)
            })?;
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
            .with_mut(|state| state.join_multicast_v4(group, interface))
    }

    async fn execute_udp_leave_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        self.inner
            .state
            .with_mut(|state| state.leave_multicast_v4(group, interface))
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
        self.drive_network(NetworkPollSource::Configuration)
            .await
            .map_err(NetworkConfigurationError::Device)?;
        let now = StackInstant::from_nanos(self.now_nanos());
        // DHCP runs on the shard that owns DHCP_CLIENT_PORT (68);
        // since 68 < EPHEMERAL_PORT_START it always routes to
        // shard 0, which is where DHCP responses also demux back.
        let configured = self
            .inner
            .state
            .with_local_port(DHCP_CLIENT_PORT, |state| {
                state
                    .drive_dhcp(now)
                    .map_err(NetworkConfigurationError::Control)?;
                if state.is_configured() {
                    self.inner.control.publish_from_shard(state);
                }
                Ok::<bool, NetworkConfigurationError>(state.is_configured())
            })?;
        if configured {
            self.synchronize_control_plane();
        }
        self.drive_network(NetworkPollSource::Configuration)
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
    ) -> Result<IpAddress, TcpError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(IpAddress::Ipv4(address));
        }
        if let Some(address) = parse_ipv6(host) {
            return Ok(IpAddress::Ipv6(address));
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
            .map(IpAddress::Ipv4)
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
        self.drive_network(NetworkPollSource::Ping)
            .await
            .map_err(|error| PingError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))
    }

    async fn drive_dns(&self) -> Result<(), DnsError> {
        self.drive_network(NetworkPollSource::Dns)
            .await
            .map_err(|error| DnsError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))
    }

    async fn drive_tcp(&self) -> Result<(), TcpError> {
        self.drive_network(NetworkPollSource::Tcp)
            .await
            .map_err(|error| TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))
    }

    async fn drive_tcp_read_burst(
        &self,
        stream: TcpStreamId,
        max_bytes: usize,
    ) -> Result<TcpReadProgress, TcpError> {
        self.drive_tcp_read_network_burst(max_bytes).await?;
        self.poll_tcp_read_once(stream, max_bytes, "tcp-read-after-drive")
    }

    async fn drive_tcp_read_network_burst(&self, max_bytes: usize) -> Result<(), TcpError> {
        let capabilities = self.inner.device.capabilities().events;
        let rounds = if capabilities.polling && max_bytes > self.inner.device.max_frame_len() {
            NETWORK_TCP_READ_BURST_ROUNDS
        } else {
            1
        };
        let outcome = self
            .poll_network_once(NetworkPollSource::Tcp)
            .await
            .map_err(|error| TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))?;
        if rounds > 1 && outcome.0.receive_saturated(outcome.1) {
            let mut deferred_transmit = false;
            for _ in 1..rounds {
                let outcome = self
                    .poll_network_receive_once(NetworkPollSource::Tcp)
                    .await
                    .map_err(|error| {
                        TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
                    })?;
                deferred_transmit |= outcome.0.received_frames != 0;
                if !outcome.0.receive_saturated(outcome.1) {
                    break;
                }
            }
            if deferred_transmit {
                // Receive-only TCP bursts already drove the stack; the hot path
                // only needs to publish generated ACK/window-update frames here.
                // Re-entering a full network poll showed up as extra
                // `tcp-read-drive-network` work in local AArch64/HVF profiles.
                let budget = self.inner.poll.budget();
                let (transmitted, _) = self
                    .submit_network_transmit(NetworkPollSource::Tcp, budget)
                    .await
                    .map_err(|error| {
                        TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
                    })?;
                if transmitted != 0 {
                    self.inner.poll.complete(NetworkPollProgress {
                        received_frames: 0,
                        reclaimed_tx: 0,
                        transmitted_frames: transmitted,
                    });
                }
            }
        }
        Ok(())
    }

    async fn drive_udp(&self) -> Result<(), UdpError> {
        self.drive_network(NetworkPollSource::Udp)
            .await
            .map_err(|error| UdpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))
    }

    async fn drive_network(&self, source: NetworkPollSource) -> Result<(), IoError> {
        let _ = self.poll_network_once(source).await?;
        Ok(())
    }

    async fn poll_network_once(
        &self,
        source: NetworkPollSource,
    ) -> Result<(NetworkPollProgress, NetworkPollBudget), IoError> {
        let outcome = self
            .poll_network_once_with_tcp_read(source, None, true)
            .await?;
        Ok((outcome.progress, outcome.budget))
    }

    async fn poll_network_receive_once(
        &self,
        source: NetworkPollSource,
    ) -> Result<(NetworkPollProgress, NetworkPollBudget), IoError> {
        let outcome = self
            .poll_network_once_with_tcp_read(source, None, false)
            .await?;
        Ok((outcome.progress, outcome.budget))
    }

    async fn poll_network_once_with_tcp_read(
        &self,
        source: NetworkPollSource,
        tcp_read_probe: Option<NetworkTcpReadProbe>,
        submit_transmit: bool,
    ) -> Result<NetworkPollOutcome, IoError> {
        self.synchronize_control_plane();
        let budget = self.inner.poll.budget();

        let reclaim_started = self.profile_start();
        let reclaimed = match self
            .inner
            .device
            .reclaim_transmit_completions_immediate(budget.tx_completions)?
        {
            Some(reclaimed) => reclaimed,
            None => {
                self.inner
                    .device
                    .reclaim_transmit_completions(budget.tx_completions)
                    .await?
            }
        };
        if reclaimed != 0 {
            self.record_network_profile_events(
                source.tx_reclaim_phase(),
                reclaim_started,
                reclaimed,
            );
        }

        let mut received = 0usize;
        let mut received_bytes = 0usize;
        let receive_started = self.profile_start();
        // `stack_rx_budget` is fixed at Stack construction so we read
        // it from the cached service-level value (no shard lock).
        // Backpressure remains a per-shard query because it tracks
        // live receive-window state.
        let stack_rx_budget = self.inner.state.with(|state| {
            if state.stack.receive_backpressured() {
                0
            } else {
                self.inner.stack_rx_budget
            }
        });
        loop {
            let remaining_rx_budget = budget
                .rx_frames
                .min(stack_rx_budget)
                .saturating_sub(received);
            if stack_rx_budget == 0 || remaining_rx_budget == 0 {
                break;
            }

            let receive_limit = remaining_rx_budget.min(NETWORK_RX_BATCH_FRAMES);
            let mut frames: [Option<Bytes>; NETWORK_RX_BATCH_FRAMES] =
                core::array::from_fn(|_| None);
            let received_batch = match self
                .inner
                .device
                .try_receive_frames_immediate(&mut frames[..receive_limit])?
            {
                Some(received_batch) => received_batch,
                None => {
                    let mut received_batch = 0usize;
                    for frame in &mut frames[..receive_limit] {
                        let Some(received_frame) = self.inner.device.try_receive_frame().await?
                        else {
                            break;
                        };
                        *frame = Some(received_frame);
                        received_batch += 1;
                    }
                    received_batch
                }
            };
            if received_batch == 0 {
                break;
            }

            let mut receive_backpressured = false;
            let received_at = StackInstant::from_nanos(self.now_nanos());
            // Demux each frame to the shard owning its destination
            // port. The previous single-shard path locked
            // `shard_for_default` once per batch; under multi-shard
            // we lock the owning shard per-frame so different
            // ports can be processed in parallel by other CPUs and
            // each shard's Stack only sees the connections it
            // actually owns. Non-IP / non-TCP-UDP frames (ARP,
            // ICMP, malformed) route to shard 0 via
            // `shard_idx_for_port(None, ...)`.
            let shard_count = self.inner.state.shard_count();
            for frame in frames[..received_batch].iter().flatten() {
                if receive_backpressured {
                    break;
                }
                let frame_len = frame.len();
                let port = peek_local_port(frame.as_ref());
                let shard_idx = shard_idx_for_port(port, shard_count);
                let mut shard = self.inner.state.shard_at(shard_idx).lock();
                match shard
                    .stack
                    .receive_frame_bytes_with_backpressure(frame.clone(), received_at)
                {
                    Ok(backpressured) => {
                        received += 1;
                        received_bytes = received_bytes.saturating_add(frame_len);
                        if backpressured {
                            receive_backpressured = true;
                        }
                    }
                    Err(StackError::ReceiveBackpressure) => {
                        receive_backpressured = true;
                    }
                    Err(error) => {
                        tracing::debug!(?error, "dropped malformed network frame");
                        received += 1;
                        received_bytes = received_bytes.saturating_add(frame_len);
                    }
                }
                shard.drain_control_events(&self.inner.control);
            }

            if self
                .inner
                .device
                .repost_rx_frames_immediate(&mut frames[..received_batch])?
                .is_none()
            {
                for frame in &mut frames[..received_batch] {
                    drop(frame.take());
                }
            }

            if receive_backpressured {
                break;
            }
        }
        self.record_network_profile_events_bytes(
            source.rx_drain_phase(),
            receive_started,
            received,
            received_bytes,
        );

        let tcp_started = self.profile_start();
        let now = StackInstant::from_nanos(self.now_nanos());
        let mut tcp_read = None;
        let mut tcp_read_started = None;
        let mut tcp_read_finished = None;
        // Each shard owns its own TCP connections, so the timer
        // drive must hit every shard's Stack.
        self.inner.state.for_each(|state| {
            state
                .stack
                .drive_tcp(now)
                .unwrap_or_else(|error| tracing::debug!(?error, "failed to drive TCP control"));
        });
        let tcp_finished = self.profile_start();
        if let Some(probe) = tcp_read_probe {
            tcp_read_started = self.profile_start();
            tcp_read = Some(self.inner.state.with_handle(probe.stream, |state| {
                state.poll_tcp_read(probe.stream, probe.max_bytes, now)
            }));
            tcp_read_finished = self.profile_start();
        }
        self.record_network_profile_between(source.tcp_drive_phase(), tcp_started, tcp_finished);
        if let (Some(probe), Some(Ok(read))) = (tcp_read_probe, tcp_read.as_ref()) {
            self.record_tcp_read_progress_between(
                probe.profile_prefix,
                tcp_read_started,
                tcp_read_finished,
                read,
            );
        }

        let (transmitted, _) = if submit_transmit {
            self.submit_network_transmit(source, budget).await?
        } else {
            (0, 0)
        };
        let progress = NetworkPollProgress {
            received_frames: received,
            reclaimed_tx: reclaimed,
            transmitted_frames: transmitted,
        };
        self.inner.poll.complete(progress);
        Ok(NetworkPollOutcome {
            progress,
            budget,
            tcp_read,
        })
    }

    fn synchronize_control_plane(&self) {
        self.inner
            .state
            .for_each(|state| self.inner.control.synchronize_shard(state));
    }

    async fn submit_network_transmit(
        &self,
        source: NetworkPollSource,
        budget: NetworkPollBudget,
    ) -> Result<(usize, usize), IoError> {
        let mut transmitted = 0usize;
        let mut transmitted_bytes = 0usize;
        let transmit_started = self.profile_start();
        let mut transmit_stop = NetworkTransmitStop::Drained;
        while transmitted < budget.tx_frames {
            let mut immediate_submitted = false;
            let mut immediate_deferred = false;
            let shard_count = self.inner.state.shard_count();
            for shard_idx in 0..shard_count {
                if transmitted >= budget.tx_frames {
                    break;
                }
                let remaining_budget = budget.tx_frames - transmitted;
                let immediate_started = self.profile_start();
                let mut immediate_device_started = None;
                let mut immediate_device_finished = None;
                let immediate = {
                    let mut state = self.inner.state.shard_at(shard_idx).lock();
                    state.stack.try_submit_outbound_slices(
                        remaining_budget.min(NETWORK_TX_BATCH_FRAMES),
                        |frames| {
                            immediate_device_started = self.profile_start();
                            let result = self
                                .inner
                                .device
                                .try_transmit_slices_immediate_on(shard_idx, frames);
                            immediate_device_finished = self.profile_start();
                            result
                        },
                    )
                }?;
                match immediate {
                    OutboundBatchStatus::Empty => {}
                    OutboundBatchStatus::Deferred => {
                        immediate_deferred = true;
                    }
                    OutboundBatchStatus::Submitted {
                        offered,
                        accepted,
                        accepted_bytes,
                    } => {
                        immediate_submitted = true;
                        self.record_network_profile_events_bytes_between(
                            source.tx_immediate_device_phase(),
                            immediate_device_started,
                            immediate_device_finished,
                            accepted,
                            accepted_bytes,
                        );
                        self.record_network_profile_events_bytes(
                            source.tx_immediate_phase(),
                            immediate_started,
                            accepted,
                            accepted_bytes,
                        );
                        transmitted += accepted;
                        transmitted_bytes = transmitted_bytes.saturating_add(accepted_bytes);
                        if accepted < offered {
                            transmit_stop = NetworkTransmitStop::RingFull;
                            break;
                        }
                    }
                }
            }
            if matches!(transmit_stop, NetworkTransmitStop::RingFull) {
                break;
            }
            if immediate_submitted {
                continue;
            }
            if !immediate_deferred {
                break;
            }

            // Per-shard collect+submit: each shard drains its
            // outbound frames into its OWN device queue. Multi-queue
            // virtio (Phase 4.2) means shard N's TX submission hits
            // queue pair N's `tx_state` SpinMutex, with no
            // contention against shards on other CPUs. Devices with
            // fewer negotiated queue pairs than shards map shards
            // onto the live queue-pair ring.
            let collect_started = self.profile_start();
            let remaining_budget = budget.tx_frames - transmitted;
            let collect_limit = NETWORK_TX_BATCH_FRAMES.min(remaining_budget);
            let shard_count = self.inner.state.shard_count();
            let mut total_collected = 0usize;
            let mut total_collected_bytes = 0usize;
            let mut total_submitted = 0usize;
            let mut total_submitted_bytes = 0usize;
            let mut ring_full = false;
            for shard_idx in 0..shard_count {
                if total_collected >= collect_limit {
                    break;
                }
                let mut shard_frames =
                    smallvec::SmallVec::<[PacketBuffer; NETWORK_TX_BATCH_FRAMES]>::new();
                {
                    let mut state = self.inner.state.shard_at(shard_idx).lock();
                    while shard_frames.len() < (collect_limit - total_collected) {
                        let Some(frame) = state.stack.take_outbound() else {
                            break;
                        };
                        shard_frames.push(frame);
                    }
                }
                if shard_frames.is_empty() {
                    continue;
                }
                let collected_bytes: usize = shard_frames.iter().map(|f| f.as_ref().len()).sum();
                total_collected += shard_frames.len();
                total_collected_bytes += collected_bytes;
                let submitted = self
                    .inner
                    .device
                    .try_transmit_packet_batch_on(shard_idx, &shard_frames)
                    .await?;
                let submitted_bytes: usize = shard_frames
                    .iter()
                    .take(submitted)
                    .map(|f| f.as_ref().len())
                    .sum();
                total_submitted += submitted;
                total_submitted_bytes += submitted_bytes;
                if submitted < shard_frames.len() {
                    // RingFull on this shard's queue. Push leftovers
                    // back to the same shard (and only that shard)
                    // so per-pair ordering is preserved, then mark
                    // the round as RingFull.
                    let mut state = self.inner.state.shard_at(shard_idx).lock();
                    while shard_frames.len() > submitted {
                        let frame = shard_frames
                            .pop()
                            .expect("TX restore lost an unsubmitted outbound frame");
                        state.stack.push_outbound_front(frame);
                    }
                    ring_full = true;
                    break;
                }
            }
            if total_collected == 0 {
                break;
            }
            self.record_network_profile_events_bytes(
                source.tx_batch_collect_phase(),
                collect_started,
                total_collected,
                total_collected_bytes,
            );
            self.record_network_profile_events_bytes(
                source.tx_batch_device_phase(),
                collect_started,
                total_submitted,
                total_submitted_bytes,
            );
            transmitted += total_submitted;
            transmitted_bytes = transmitted_bytes.saturating_add(total_submitted_bytes);
            if ring_full {
                transmit_stop = NetworkTransmitStop::RingFull;
                break;
            }
        }
        if transmitted >= budget.tx_frames {
            transmit_stop = NetworkTransmitStop::Budget;
        }
        if transmitted != 0 {
            self.record_network_profile_events_bytes(
                transmit_stop.profile_phase(source),
                transmit_started,
                transmitted,
                transmitted_bytes,
            );
        }
        Ok((transmitted, transmitted_bytes))
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
        let mut event = core::pin::pin!(event);
        if core::future::poll_fn(|cx| Poll::Ready(event.as_mut().poll(cx).is_ready())).await {
            return;
        }

        let timer = self.inner.timer.sleep_for(duration);
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
        let next_tcp_deadline = self.inner.state.min_tcp_deadline_nanos();
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

    fn profile_start(&self) -> Option<NetworkPerfStart> {
        self.inner
            .runtime_state
            .profiling_enabled()
            .then(|| NetworkPerfStart {
                nanos: self.now_nanos(),
                counters: self.inner.cpu.hardware_perf_counters(),
            })
    }

    fn record_network_profile(&self, phase: &'static str, start: Option<NetworkPerfStart>) {
        self.record_network_profile_events(phase, start, 0);
    }

    fn record_network_profile_between(
        &self,
        phase: &'static str,
        start: Option<NetworkPerfStart>,
        end: Option<NetworkPerfStart>,
    ) {
        self.record_network_profile_events_bytes_between(phase, start, end, 0, 0);
    }

    fn record_network_profile_events(
        &self,
        phase: &'static str,
        start: Option<NetworkPerfStart>,
        events: usize,
    ) {
        self.record_network_profile_events_bytes(phase, start, events, 0);
    }

    fn record_network_profile_events_bytes(
        &self,
        phase: &'static str,
        start: Option<NetworkPerfStart>,
        events: usize,
        bytes: usize,
    ) {
        if let Some(start) = start {
            self.record_network_profile_events_bytes_between(
                phase,
                Some(start),
                self.profile_start(),
                events,
                bytes,
            );
        }
    }

    fn record_network_profile_events_bytes_between(
        &self,
        phase: &'static str,
        start: Option<NetworkPerfStart>,
        end: Option<NetworkPerfStart>,
        events: usize,
        bytes: usize,
    ) {
        let (Some(start), Some(end)) = (start, end) else {
            return;
        };
        let counters = end.counters.delta_since(start.counters);
        let elapsed_nanos = end.nanos.saturating_sub(start.nanos);
        self.inner.runtime_state.record_profile_stack_parts_nanos(
            crate::ProfileScope::Kernel,
            "kernel;network;",
            phase,
            elapsed_nanos,
        );
        self.inner
            .runtime_state
            .record_perf_metric_parts_events_nanos(
                crate::ProfileScope::Kernel,
                "kernel;network;",
                phase,
                usize_to_u64(events, "network profile event count"),
                elapsed_nanos,
                counters,
                usize_to_u64(bytes, "network profile byte count"),
            );
    }

    fn record_tcp_read_progress(
        &self,
        prefix: &'static str,
        start: Option<NetworkPerfStart>,
        read: &TcpReadProgress,
    ) {
        let (phase, bytes) = match read {
            TcpReadProgress::Pending => (tcp_read_profile_phase(prefix, "pending"), 0),
            TcpReadProgress::Data(bytes) => (tcp_read_profile_phase(prefix, "ready"), bytes.len()),
            TcpReadProgress::Eof => (tcp_read_profile_phase(prefix, "eof"), 0),
        };
        self.record_network_profile_events_bytes(phase, start, 1, bytes);
    }

    fn record_tcp_read_progress_between(
        &self,
        prefix: &'static str,
        start: Option<NetworkPerfStart>,
        end: Option<NetworkPerfStart>,
        read: &TcpReadProgress,
    ) {
        let (phase, bytes) = match read {
            TcpReadProgress::Pending => (tcp_read_profile_phase(prefix, "pending"), 0),
            TcpReadProgress::Data(bytes) => (tcp_read_profile_phase(prefix, "ready"), bytes.len()),
            TcpReadProgress::Eof => (tcp_read_profile_phase(prefix, "eof"), 0),
        };
        self.record_network_profile_events_bytes_between(phase, start, end, 1, bytes);
    }

    fn record_tcp_read_into_progress(
        &self,
        prefix: &'static str,
        start: Option<NetworkPerfStart>,
        read: &TcpReadIntoState,
    ) {
        let (phase, bytes) = match read {
            TcpReadIntoState::Pending => (tcp_read_profile_phase(prefix, "pending"), 0),
            TcpReadIntoState::Data(bytes) => (tcp_read_profile_phase(prefix, "ready"), *bytes),
            TcpReadIntoState::Eof => (tcp_read_profile_phase(prefix, "eof"), 0),
        };
        self.record_network_profile_events_bytes(phase, start, 1, bytes);
    }

    pub fn hardware_address(&self) -> [u8; 6] {
        self.inner.device.mac_address()
    }

    pub async fn ipv4_cidr(&self) -> Option<crate::Ipv4Cidr> {
        self.inner.control.list_ipv4_addresses().into_iter().next()
    }

    async fn acquire_dhcp_address(&self) -> Result<KernelIpv4Cidr, NetworkControlError> {
        loop {
            self.drive_network(NetworkPollSource::Configuration)
                .await
                .map_err(|_| NetworkControlError::BackendFault)?;
            let now = StackInstant::from_nanos(self.now_nanos());
            let next = self.inner.state.with_mut(|state| {
                state.drive_dhcp(now)?;
                if state.is_configured() {
                    self.inner.control.publish_from_shard(state);
                }
                Ok(state.stack.primary_ipv4_address().map(map_ipv4_cidr))
            })?;
            if let Some(cidr) = next {
                self.synchronize_control_plane();
                return Ok(cidr);
            }
            self.drive_network(NetworkPollSource::Configuration)
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

    fn tcp_connect_address(
        &self,
        remote_address: NetworkIpAddress,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Self::TcpStream, TcpError>> + Send + '_ {
        async move {
            NetworkService::tcp_connect_address(
                self,
                remote_address,
                port,
                local_port,
                timeout_nanos,
            )
            .await
        }
    }

    fn tcp_listen(
        &self,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: u16,
    ) -> impl core::future::Future<Output = Result<TcpListener<Self::TcpListener>, TcpError>> + Send + '_
    {
        async move { NetworkService::tcp_listen(self, local_address, local_port, backlog).await }
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

    fn tcp_read_into<'a>(
        &'a self,
        stream: Self::TcpStream,
        buffer: RegisteredTcpReadBuffer<'a>,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Option<usize>, TcpError>> + Send + 'a {
        async move { NetworkService::tcp_read_into(self, stream, buffer, timeout_nanos).await }
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

    fn udp_connect(
        &self,
        socket: Self::UdpSocket,
        remote_address: NetworkIpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        NetworkService::udp_connect(self, socket, remote_address, port)
    }

    fn udp_disconnect(&self, socket: Self::UdpSocket) -> Result<(), UdpError> {
        NetworkService::udp_disconnect(self, socket)
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

    fn udp_send_address<'a>(
        &'a self,
        socket: Self::UdpSocket,
        remote_address: NetworkIpAddress,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<u64, UdpError>> + Send + 'a {
        async move {
            NetworkService::udp_send_address(
                self,
                socket,
                remote_address,
                port,
                bytes,
                timeout_nanos,
            )
            .await
        }
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
        self.inner.control.update_ipv4_addresses(|addresses| {
            addresses.add(map_kernel_ipv4_cidr(address));
            Ok(())
        })?;
        let connected_route = Route {
            destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(address)),
            gateway: None,
            expires_at: None,
        };
        self.inner.control.update_routes(|routes| {
            routes
                .add(connected_route)
                .map_err(|_| NetworkControlError::InvalidRoute)
        })?;
        let mut result = Ok(());
        self.inner.state.for_each(|state| {
            if result.is_ok() {
                self.inner.control.synchronize_shard(state);
                if let Err(error) = state.add_ipv4_address(address) {
                    result = Err(error);
                }
            }
        });
        result
    }

    async fn remove_address(
        &self,
        port: NetworkPortId,
        address: KernelIpv4Cidr,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.control.update_ipv4_addresses(|addresses| {
            addresses.remove(map_kernel_ipv4_cidr(address));
            Ok(())
        })?;
        self.inner.control.update_routes(|routes| {
            routes.remove(Route {
                destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(address)),
                gateway: None,
                expires_at: None,
            });
            Ok(())
        })?;
        self.inner.state.for_each(|state| {
            self.inner.control.synchronize_shard(state);
            state.remove_ipv4_address(address);
        });
        Ok(())
    }

    async fn clear_addresses(&self, port: NetworkPortId) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.control.update_ipv4_addresses(|addresses| {
            addresses.clear();
            Ok(())
        })?;
        self.inner.control.update_routes(|routes| {
            for cidr in self.inner.state.with(NetworkShard::list_ipv4_addresses) {
                routes.remove(Route {
                    destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(cidr)),
                    gateway: None,
                    expires_at: None,
                });
            }
            Ok(())
        })?;
        self.inner
            .state
            .for_each(NetworkShard::clear_ipv4_addresses);
        Ok(())
    }

    async fn list_addresses(
        &self,
        port: NetworkPortId,
    ) -> Result<Vec<KernelIpv4Cidr>, NetworkControlError> {
        require_local_network_port(port)?;
        Ok(self.inner.control.list_ipv4_addresses())
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
        self.inner.control.update_routes(|routes| {
            routes
                .add(Route {
                    destination: IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0)),
                    gateway: Some(IpAddress::Ipv4(map_kernel_ipv4_address(gateway))),
                    expires_at: None,
                })
                .map_err(|_| NetworkControlError::InvalidRoute)
        })?;
        let mut result = Ok(());
        self.inner.state.for_each(|state| {
            if result.is_ok() {
                self.inner.control.synchronize_shard(state);
                if let Err(error) = state.set_default_ipv4_gateway(gateway) {
                    result = Err(error);
                }
            }
        });
        result
    }

    async fn add_route(
        &self,
        port: NetworkPortId,
        route: KernelIpv4Route,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.control.update_routes(|routes| {
            routes
                .add(Route {
                    destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(route.destination())),
                    gateway: Some(IpAddress::Ipv4(map_kernel_ipv4_address(route.gateway()))),
                    expires_at: route.expires_at_nanos().map(StackInstant::from_nanos),
                })
                .map_err(|_| NetworkControlError::InvalidRoute)
        })?;
        let mut result = Ok(());
        self.inner.state.for_each(|state| {
            if result.is_ok() {
                self.inner.control.synchronize_shard(state);
                if let Err(error) = state.add_ipv4_route(route) {
                    result = Err(error);
                }
            }
        });
        result
    }

    async fn remove_route(
        &self,
        port: NetworkPortId,
        route: KernelIpv4Route,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.control.update_routes(|routes| {
            routes.remove(Route {
                destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(route.destination())),
                gateway: Some(IpAddress::Ipv4(map_kernel_ipv4_address(route.gateway()))),
                expires_at: route.expires_at_nanos().map(StackInstant::from_nanos),
            });
            Ok(())
        })?;
        self.inner.state.for_each(|state| {
            self.inner.control.synchronize_shard(state);
            state.remove_ipv4_route(route);
        });
        Ok(())
    }

    async fn clear_routes(&self, port: NetworkPortId) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.control.update_routes(|routes| {
            routes.clear_ipv4();
            Ok(())
        })?;
        self.inner.state.for_each(NetworkShard::clear_ipv4_routes);
        Ok(())
    }

    async fn list_routes(
        &self,
        port: NetworkPortId,
    ) -> Result<Vec<KernelIpv4Route>, NetworkControlError> {
        require_local_network_port(port)?;
        Ok(self.inner.control.list_ipv4_routes())
    }
}

impl NetworkShard {
    fn new(
        mac: [u8; 6],
        max_frame_len: usize,
        rx_poll_budget: usize,
        rx_checksum_offload: RxChecksumOffload,
        transaction_id: u32,
        shard_idx: usize,
        shard_count: usize,
    ) -> Self {
        assert!(shard_count != 0, "network shard count must be non-zero");
        assert!(
            shard_idx < shard_count,
            "shard idx {shard_idx} out of range for {shard_count} shards"
        );
        let initial_ephemeral = EPHEMERAL_PORT_START + shard_idx as u16;
        let mut stack = Box::new(Stack::new(
            StackConfig::new(mac, max_frame_len)
                .with_rx_budget(rx_poll_budget)
                .with_rx_checksum_offload(rx_checksum_offload),
        ));
        let mut udp_sockets = HandleSlab::new();
        if shard_idx == shard_idx_for_port(Some(DHCP_CLIENT_PORT), shard_count) {
            let binding = UdpSocketBinding::wildcard(DHCP_CLIENT_PORT);
            let stack_socket = stack
                .open_udp(binding)
                .unwrap_or_else(|error| panic!("failed to open DHCP UDP socket: {error}"));
            let slot = udp_sockets.insert(UdpSocketState {
                stack_socket,
                binding,
            });
            assert_eq!(
                slot, INTERNAL_DHCP_SOCKET_INDEX,
                "DHCP internal UDP socket slot changed"
            );
        }
        if shard_idx == shard_idx_for_port(Some(INTERNAL_DNS_PORT), shard_count) {
            let binding = UdpSocketBinding::wildcard(INTERNAL_DNS_PORT);
            let stack_socket = stack
                .open_udp(binding)
                .unwrap_or_else(|error| panic!("failed to open DNS UDP socket: {error}"));
            let slot = udp_sockets.insert(UdpSocketState {
                stack_socket,
                binding,
            });
            assert_eq!(
                slot, INTERNAL_DNS_SOCKET_INDEX,
                "DNS internal UDP socket slot changed"
            );
        }
        Self {
            stack,
            shard_idx,
            shard_count,
            next_tcp_local_port: initial_ephemeral,
            next_udp_local_port: initial_ephemeral,
            tcp_streams: HandleSlab::new(),
            tcp_listeners: HandleSlab::new(),
            udp_sockets,
            dhcp: DhcpClientState::Init { transaction_id },
            dns_servers: DhcpDnsServers::new(),
            next_dns_query_id: 1,
        }
    }

    /// Encodes a per-shard slab index into a globally unique handle
    /// id whose value satisfies `(id - 1) % shard_count == shard_idx`.
    /// This is the inverse of `NetworkShardSet::shard_for_handle`
    /// so an operation arriving with a handle can route directly to
    /// the shard that minted it without consulting a side table.
    fn encode_handle_id(&self, slot: usize) -> u32 {
        let stride = self.shard_count;
        let raw = slot
            .checked_mul(stride)
            .and_then(|product| product.checked_add(self.shard_idx + 1))
            .unwrap_or_else(|| {
                panic!(
                    "network handle slot {slot} overflowed stride {stride} encoding for shard {}",
                    self.shard_idx
                )
            });
        u32::try_from(raw).unwrap_or_else(|_| panic!("encoded handle id {raw} exceeds u32 range"))
    }

    /// Reverses `encode_handle_id`. Panics if the handle was minted
    /// by a different shard, since misrouted handles indicate a
    /// caller-side dispatch bug.
    fn decode_handle_slot(&self, raw: u32) -> usize {
        let value = (raw - 1) as usize;
        assert_eq!(
            value % self.shard_count,
            self.shard_idx,
            "handle id {raw} routed to shard {} but encodes shard {}",
            self.shard_idx,
            value % self.shard_count
        );
        value / self.shard_count
    }

    fn is_configured(&self) -> bool {
        self.stack.primary_ipv4_address().is_some()
    }

    fn drain_control_events(&mut self, control: &NetworkControlPlane) {
        while let Some(event) = self.stack.take_event() {
            match event {
                StackEvent::NeighborUpdated(entry) => {
                    control.update_neighbors(|neighbors| neighbors.learn(entry));
                }
                StackEvent::DhcpConfigured(_) => control.publish_from_shard(self),
                StackEvent::UdpDatagram { .. }
                | StackEvent::TcpConnected { .. }
                | StackEvent::TcpReadable { .. }
                | StackEvent::TcpClosed { .. } => {}
            }
        }
    }

    fn drive_dhcp(&mut self, now: StackInstant) -> Result<(), NetworkControlError> {
        let socket = self.internal_udp_socket(INTERNAL_DHCP_SOCKET_INDEX, DHCP_CLIENT_PORT);
        while let Some(datagram) = self
            .stack
            .take_udp(socket)
            .unwrap_or_else(|error| panic!("DHCP UDP socket disappeared: {error}"))
        {
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
        let socket = self.internal_udp_socket(INTERNAL_DNS_SOCKET_INDEX, INTERNAL_DNS_PORT);
        while let Some(datagram) = self.stack.take_udp(socket).map_err(|_| DnsError {
            kind: DnsErrorKind::Unavailable,
            detail: NetworkErrorDetail::DnsQueryStartFailed,
        })? {
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
        destination: IpAddress,
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
        let local = match destination {
            IpAddress::Ipv4(_) => self
                .stack
                .primary_ipv4_address()
                .map(|cidr| IpAddress::Ipv4(cidr.address())),
            IpAddress::Ipv6(_) => self
                .stack
                .primary_ipv6_address()
                .map(|cidr| IpAddress::Ipv6(cidr.address())),
        };
        let Some(local) = local else {
            return Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::NetworkServiceUnavailable,
            });
        };
        let socket = self
            .stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: local,
                    port: local_port,
                },
                TcpEndpoint {
                    address: destination,
                    port,
                },
                1,
            )
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpConnectStartFailed,
            })?;
        Ok(self.insert_tcp_stream(socket))
    }

    fn start_tcp_listen(
        &mut self,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: TcpListenBacklog,
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
        let stack_socket = self
            .stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: map_network_ip_address(local_address),
                    port: local_port,
                },
                backlog,
            )
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpListenStartFailed,
            })?;
        Ok(TcpListener {
            listener: self.insert_tcp_listener(stack_socket, local_port),
            local_port,
        })
    }

    fn poll_tcp_accept(
        &mut self,
        listener: TcpListenerId,
    ) -> Result<Option<TcpAccepted<TcpStreamId>>, TcpError> {
        let stack_socket = self.tcp_listener(listener)?.stack_socket;
        let Some(accepted) = self
            .stack
            .take_tcp_accept(stack_socket)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpListenerClosedUnexpectedly,
            })?
        else {
            return Ok(None);
        };
        let stream = self.insert_tcp_stream(accepted.socket);
        Ok(Some(TcpAccepted {
            stream,
            address: map_ip_address(accepted.remote.address),
            port: accepted.remote.port,
        }))
    }

    fn poll_tcp_connect(&mut self, stream: TcpStreamId) -> Result<TcpConnectProgress, TcpError> {
        let socket = self.tcp_socket(stream)?;
        match self.stack.tcp_connect_state(socket).map_err(|_| TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownTcpStream,
        })? {
            TcpConnectState::Connected => Ok(TcpConnectProgress::Connected),
            TcpConnectState::Pending => Ok(TcpConnectProgress::Pending),
            TcpConnectState::Closed(error) => Err(map_tcp_connect_terminal_error(error)),
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
        now: StackInstant,
    ) -> Result<TcpReadProgress, TcpError> {
        let socket = self.tcp_socket(stream)?;
        match self
            .stack
            .tcp_read(socket, max_bytes, now)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpReceiveFailed,
            })? {
            TcpReadState::Pending => Ok(TcpReadProgress::Pending),
            TcpReadState::Data(bytes) => Ok(TcpReadProgress::Data(bytes)),
            TcpReadState::Eof => Ok(TcpReadProgress::Eof),
        }
    }

    fn poll_tcp_read_into(
        &mut self,
        stream: TcpStreamId,
        buffer: &mut RegisteredTcpReadBuffer<'_>,
        now: StackInstant,
    ) -> Result<TcpReadIntoState, TcpError> {
        let socket = self.tcp_socket(stream)?;
        self.stack
            .tcp_read_into(socket, buffer, now)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpReceiveFailed,
            })
    }

    fn start_udp_bind(&mut self, local_port: u16) -> Result<UdpBinding<UdpSocketId>, UdpError> {
        let local_port = if local_port == 0 {
            self.allocate_udp_local_port()?
        } else if self
            .stack
            .udp_binding_free(UdpSocketBinding::wildcard(local_port))
        {
            local_port
        } else {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpPortInUse,
            });
        };
        let binding = UdpSocketBinding::wildcard(local_port);
        let stack_socket = self.stack.open_udp(binding).map_err(map_udp_bind_error)?;
        Ok(UdpBinding {
            socket: self.insert_udp_socket(stack_socket, binding),
            local_port,
        })
    }

    fn connect_udp_socket(
        &mut self,
        socket: UdpSocketId,
        destination: IpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        let slot = self.decode_handle_slot(socket.0.get());
        let state = *self.udp_socket(socket)?;
        let local = self
            .udp_local_endpoint(state.binding.local_port, destination)
            .map_err(map_udp_connect_error)?;
        let remote = UdpEndpoint {
            address: destination,
            port,
        };
        let binding = UdpSocketBinding::connected(local, remote);
        self.stack
            .rebind_udp(state.stack_socket, binding)
            .map_err(map_udp_connect_error)?;
        let state = self
            .udp_sockets
            .get_mut(slot)
            .unwrap_or_else(|| panic!("UDP socket disappeared during connect"));
        state.binding = binding;
        Ok(())
    }

    fn disconnect_udp_socket(&mut self, socket: UdpSocketId) -> Result<(), UdpError> {
        let slot = self.decode_handle_slot(socket.0.get());
        let state = *self.udp_socket(socket)?;
        if state.binding.remote.is_none() {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpDisconnectFailed,
            });
        }
        let binding = UdpSocketBinding::wildcard(state.binding.local_port);
        self.stack
            .rebind_udp(state.stack_socket, binding)
            .map_err(map_udp_disconnect_error)?;
        let state = self
            .udp_sockets
            .get_mut(slot)
            .unwrap_or_else(|| panic!("UDP socket disappeared during disconnect"));
        state.binding = binding;
        Ok(())
    }

    fn try_send_udp(
        &mut self,
        socket: UdpSocketId,
        destination: IpAddress,
        port: u16,
        bytes: &[u8],
        now: StackInstant,
    ) -> Result<usize, UdpError> {
        let stack_socket = self.udp_socket(socket)?.stack_socket;
        if let Some(error) = self
            .stack
            .take_udp_error(stack_socket)
            .map_err(|_| UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UnknownUdpSocket,
            })?
        {
            return Err(map_udp_socket_error(error));
        }
        self.stack
            .send_udp(
                stack_socket,
                destination,
                port,
                bytes,
                socket.0.get() as u16,
                now,
            )
            .map_err(|error| {
                tracing::debug!(?error, "failed to queue UDP datagram");
                map_udp_send_error(error)
            })
    }

    fn poll_udp_receive(
        &mut self,
        socket: UdpSocketId,
        max_bytes: usize,
    ) -> Result<Option<UdpDatagram>, UdpError> {
        let stack_socket = self.udp_socket(socket)?.stack_socket;
        if let Some(error) = self
            .stack
            .take_udp_error(stack_socket)
            .map_err(|_| UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UnknownUdpSocket,
            })?
        {
            return Err(map_udp_socket_error(error));
        }
        let Some(datagram) = self.stack.take_udp(stack_socket).map_err(|_| UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpReceiveFailed,
        })?
        else {
            return Ok(None);
        };
        Ok(Some(UdpDatagram {
            address: map_ip_address(datagram.source),
            port: datagram.source_port,
            bytes: limit_udp_datagram_bytes(datagram.bytes, max_bytes),
        }))
    }

    fn remove_tcp_stream(&mut self, stream: TcpStreamId) {
        let slot = self.decode_handle_slot(stream.0.get());
        if let Some(socket) = self.tcp_streams.remove(slot) {
            self.stack
                .remove_tcp_socket(socket)
                .unwrap_or_else(|_| panic!("TCP stream referenced an unknown stack socket"));
        }
    }

    fn remove_udp_socket(&mut self, socket: UdpSocketId) {
        let slot = self.decode_handle_slot(socket.0.get());
        if let Some(socket) = self.udp_sockets.remove(slot) {
            self.stack
                .remove_udp_socket(socket.stack_socket)
                .unwrap_or_else(|_| panic!("UDP handle referenced an unknown stack socket"));
        }
    }

    fn insert_tcp_stream(&mut self, socket: helios_netstack::SocketId) -> TcpStreamId {
        let slot = self.tcp_streams.insert(socket);
        TcpStreamId(
            NonZeroU32::new(self.encode_handle_id(slot))
                .unwrap_or_else(|| panic!("tcp stream ids must never be zero")),
        )
    }

    fn insert_tcp_listener(
        &mut self,
        stack_socket: helios_netstack::SocketId,
        local_port: u16,
    ) -> TcpListenerId {
        let slot = self.tcp_listeners.insert(TcpListenerState {
            stack_socket,
            local_port,
        });
        TcpListenerId(
            NonZeroU32::new(self.encode_handle_id(slot))
                .unwrap_or_else(|| panic!("tcp listener ids must never be zero")),
        )
    }

    fn insert_udp_socket(
        &mut self,
        stack_socket: helios_netstack::UdpSocketId,
        binding: UdpSocketBinding,
    ) -> UdpSocketId {
        let slot = self.udp_sockets.insert(UdpSocketState {
            stack_socket,
            binding,
        });
        UdpSocketId(
            NonZeroU32::new(self.encode_handle_id(slot))
                .unwrap_or_else(|| panic!("udp socket ids must never be zero")),
        )
    }

    fn tcp_socket(&self, stream: TcpStreamId) -> Result<helios_netstack::SocketId, TcpError> {
        let slot = self.decode_handle_slot(stream.0.get());
        self.tcp_streams.get(slot).copied().ok_or_else(|| TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownTcpStream,
        })
    }

    fn tcp_listener(&self, listener: TcpListenerId) -> Result<&TcpListenerState, TcpError> {
        let slot = self.decode_handle_slot(listener.0.get());
        self.tcp_listeners.get(slot).ok_or_else(|| TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpListenerClosedUnexpectedly,
        })
    }

    fn udp_socket(&self, socket: UdpSocketId) -> Result<&UdpSocketState, UdpError> {
        let slot = self.decode_handle_slot(socket.0.get());
        self.udp_sockets.get(slot).ok_or_else(|| UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownUdpSocket,
        })
    }

    fn internal_udp_socket(&self, slot: usize, expected_port: u16) -> helios_netstack::UdpSocketId {
        let state = self
            .udp_sockets
            .get(slot)
            .unwrap_or_else(|| panic!("internal UDP socket slot {slot} is not initialized"));
        assert_eq!(
            state.binding.local_port, expected_port,
            "internal UDP socket slot {slot} has unexpected local port"
        );
        state.stack_socket
    }

    fn allocate_tcp_local_port(&mut self) -> Result<u16, TcpError> {
        for _ in 0..self.ephemeral_port_attempts() {
            let candidate = self.next_tcp_local_port;
            self.next_tcp_local_port = self.advance_ephemeral_port(self.next_tcp_local_port);
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
        for _ in 0..self.ephemeral_port_attempts() {
            let candidate = self.next_udp_local_port;
            self.next_udp_local_port = self.advance_ephemeral_port(self.next_udp_local_port);
            if self.is_udp_local_port_free(candidate) {
                return Ok(candidate);
            }
        }
        Err(UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpNoEphemeralPorts,
        })
    }

    /// Number of ephemeral ports owned by this shard under the
    /// stride-allocation scheme. Equal to the total ephemeral
    /// range divided by the shard count, rounded up.
    fn ephemeral_port_attempts(&self) -> usize {
        let total = usize::from(EPHEMERAL_PORT_END - EPHEMERAL_PORT_START) + 1;
        total.div_ceil(self.shard_count)
    }

    /// Advances the rolling allocator pointer by `shard_count`,
    /// wrapping back to the shard's first ephemeral port when we
    /// step past `EPHEMERAL_PORT_END`. The result is always a port
    /// that satisfies `(port - EPHEMERAL_PORT_START) % shard_count
    /// == shard_idx`, matching `shard_idx_for_port` so RX demux
    /// routes back to this shard.
    fn advance_ephemeral_port(&self, current: u16) -> u16 {
        let stride = self.shard_count as u16;
        let next = current.checked_add(stride);
        match next {
            Some(value) if value <= EPHEMERAL_PORT_END => value,
            _ => EPHEMERAL_PORT_START + self.shard_idx as u16,
        }
    }

    fn is_tcp_local_port_free(&self, port: u16) -> bool {
        self.tcp_listeners
            .iter()
            .all(|state| state.local_port != port)
    }

    fn is_udp_local_port_free(&self, port: u16) -> bool {
        self.stack
            .udp_binding_free(UdpSocketBinding::wildcard(port))
    }

    fn udp_local_endpoint(
        &self,
        local_port: u16,
        destination: IpAddress,
    ) -> Result<UdpEndpoint, StackError> {
        let address = match destination {
            IpAddress::Ipv4(destination) => self
                .stack
                .source_ipv4_address_for(destination)
                .map(IpAddress::Ipv4)
                .or_else(|| {
                    self.stack
                        .primary_ipv4_address()
                        .map(|cidr| IpAddress::Ipv4(cidr.address()))
                }),
            IpAddress::Ipv6(destination) => self
                .stack
                .source_ipv6_address_for(destination)
                .map(IpAddress::Ipv6)
                .or_else(|| {
                    self.stack
                        .primary_ipv6_address()
                        .map(|cidr| IpAddress::Ipv6(cidr.address()))
                }),
        }
        .ok_or(StackError::Unroutable)?;
        Ok(UdpEndpoint {
            address,
            port: local_port,
        })
    }

    fn join_multicast_v4(
        &mut self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        let group = map_kernel_ipv4_address(group);
        let interface = self.require_multicast_interface(interface)?;
        self.stack
            .join_ipv4_multicast(group, interface)
            .map_err(multicast_join_error)
    }

    fn leave_multicast_v4(
        &mut self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        let group = map_kernel_ipv4_address(group);
        let interface = self.require_multicast_interface(interface)?;
        self.stack
            .leave_ipv4_multicast(group, interface)
            .map_err(multicast_leave_error)
    }

    fn require_multicast_interface(
        &self,
        interface: KernelIpv4Address,
    ) -> Result<Ipv4Address, UdpError> {
        let Some(cidr) = self.stack.primary_ipv4_address() else {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpMulticastInterfaceUnavailable,
            });
        };
        if interface.octets() == [0, 0, 0, 0] {
            return Ok(cidr.address());
        }
        let interface = map_kernel_ipv4_address(interface);
        if cidr.address() != interface {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpMulticastInterfaceUnavailable,
            });
        }
        Ok(interface)
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
}

impl NetworkPollState {
    fn new(rx_budget: usize, tx_completion_budget: usize, tx_frame_budget: usize) -> Self {
        let rx_budget = clamp_poll_budget(rx_budget);
        let tx_completion_budget = clamp_poll_budget(tx_completion_budget);
        let tx_frame_budget = clamp_poll_budget(tx_frame_budget);
        Self {
            base_rx_budget: rx_budget,
            base_tx_completion_budget: tx_completion_budget,
            base_tx_frame_budget: tx_frame_budget,
            rx_budget: AtomicUsize::new(rx_budget),
            tx_completion_budget: AtomicUsize::new(tx_completion_budget),
            tx_frame_budget: AtomicUsize::new(tx_frame_budget),
        }
    }

    fn budget(&self) -> NetworkPollBudget {
        NetworkPollBudget {
            rx_frames: self.rx_budget.load(AtomicOrdering::Relaxed),
            tx_completions: self.tx_completion_budget.load(AtomicOrdering::Relaxed),
            tx_frames: self.tx_frame_budget.load(AtomicOrdering::Relaxed),
        }
    }

    fn complete(&self, progress: NetworkPollProgress) {
        let rx_current = self.rx_budget.load(AtomicOrdering::Relaxed);
        self.rx_budget.store(
            adjust_poll_budget(
                rx_current,
                self.base_rx_budget,
                progress.received_frames >= rx_current,
                progress.is_idle(),
            ),
            AtomicOrdering::Relaxed,
        );
        let tx_completion_current = self.tx_completion_budget.load(AtomicOrdering::Relaxed);
        self.tx_completion_budget.store(
            adjust_poll_budget(
                tx_completion_current,
                self.base_tx_completion_budget,
                progress.reclaimed_tx >= tx_completion_current,
                progress.is_idle(),
            ),
            AtomicOrdering::Relaxed,
        );
        let tx_frame_current = self.tx_frame_budget.load(AtomicOrdering::Relaxed);
        self.tx_frame_budget.store(
            adjust_poll_budget(
                tx_frame_current,
                self.base_tx_frame_budget,
                progress.transmitted_frames >= tx_frame_current,
                progress.is_idle(),
            ),
            AtomicOrdering::Relaxed,
        );
    }
}

impl NetworkPumpCadence {
    const fn new() -> Self {
        Self { busy_rounds: 0 }
    }

    fn complete(
        &mut self,
        progress: NetworkPollProgress,
        budget: NetworkPollBudget,
    ) -> NetworkPumpAction {
        if progress.is_idle() {
            self.reset();
            return NetworkPumpAction::Wait;
        }

        self.busy_rounds = self.busy_rounds.saturating_add(1);
        if self.busy_rounds >= NETWORK_BUSY_POLL_ROUNDS {
            self.reset();
            return NetworkPumpAction::Yield;
        }

        if progress.saturated(budget) {
            return NetworkPumpAction::Continue;
        }

        NetworkPumpAction::Continue
    }

    fn reset(&mut self) {
        self.busy_rounds = 0;
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

fn tcp_read_profile_phase(prefix: &'static str, outcome: &'static str) -> &'static str {
    match (prefix, outcome) {
        ("tcp-read-initial", "pending") => "tcp-read-initial-pending",
        ("tcp-read-initial", "ready") => "tcp-read-initial-ready",
        ("tcp-read-initial", "eof") => "tcp-read-initial-eof",
        ("tcp-read-after-drive", "pending") => "tcp-read-after-drive-pending",
        ("tcp-read-after-drive", "ready") => "tcp-read-after-drive-ready",
        ("tcp-read-after-drive", "eof") => "tcp-read-after-drive-eof",
        ("tcp-read-polling", "pending") => "tcp-read-polling-pending",
        ("tcp-read-polling", "ready") => "tcp-read-polling-ready",
        ("tcp-read-polling", "eof") => "tcp-read-polling-eof",
        _ => panic!("unknown TCP read profile phase"),
    }
}

fn limit_udp_datagram_bytes(bytes: UdpPayload, max_bytes: usize) -> Bytes {
    bytes.into_limited_bytes(max_bytes)
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

fn parse_ipv6(input: &str) -> Option<Ipv6Address> {
    input
        .parse::<core::net::Ipv6Addr>()
        .ok()
        .map(|address| Ipv6Address::new(address.octets()))
}

fn usize_to_u64(value: usize, label: &'static str) -> u64 {
    u64::try_from(value).unwrap_or_else(|_| panic!("{label} does not fit into u64"))
}

#[cfg(test)]
mod tests {
    use helios_netstack::{
        ETHERNET_FRAME_BYTES, EthernetFrame, EthernetProtocol, Icmpv6Packet, IpAddress, IpProtocol,
        Ipv4Address, Ipv4Cidr, Ipv4Packet, Ipv6Address, Ipv6Cidr, Ipv6Packet, MAX_OUTBOUND_FRAMES,
        NeighborEntry, NeighborState, RxChecksumOffload, StackInstant, TcpFlags, TcpHeader,
        TcpListenBacklog, TcpPacket, UdpEndpoint, UdpPacket, UdpPayload, UdpSocketBinding,
    };

    use super::{
        HandleSlab, NETWORK_BUSY_POLL_ROUNDS, NETWORK_TX_BATCH_FRAMES, NetworkIpAddress,
        NetworkPollBudget, NetworkPollProgress, NetworkPollState, NetworkPumpAction,
        NetworkPumpCadence, NetworkShard, limit_udp_datagram_bytes, map_ipv4_address, parse_ipv6,
    };

    fn ipv6_tcp_frame(
        source: Ipv6Address,
        destination: Ipv6Address,
        header: TcpHeader,
    ) -> ([u8; ETHERNET_FRAME_BYTES], usize) {
        let mut frame = [0; ETHERNET_FRAME_BYTES];
        let mut offset = EthernetFrame::encode_header(
            &mut frame,
            [0x02, 0, 0, 0, 0, 1],
            [0x02, 0, 0, 0, 0, 2],
            EthernetProtocol::Ipv6,
        )
        .expect("test Ethernet header should fit");
        let tcp_start = offset + Ipv6Packet::HEADER_LEN;
        let tcp_len = TcpPacket::encode(
            &mut frame[tcp_start..],
            IpAddress::Ipv6(source),
            IpAddress::Ipv6(destination),
            header,
            &[],
        )
        .expect("test TCP segment should fit");
        offset += Ipv6Packet::encode_header(
            &mut frame[offset..],
            source,
            destination,
            IpProtocol::Tcp,
            tcp_len,
            64,
        )
        .expect("test IPv6 header should fit");
        (frame, offset + tcp_len)
    }

    fn ipv6_udp_frame(
        source: Ipv6Address,
        source_port: u16,
        destination: Ipv6Address,
        destination_port: u16,
        payload: &[u8],
    ) -> ([u8; ETHERNET_FRAME_BYTES], usize) {
        let mut frame = [0; ETHERNET_FRAME_BYTES];
        let mut offset = EthernetFrame::encode_header(
            &mut frame,
            [0x02, 0, 0, 0, 0, 1],
            [0x02, 0, 0, 0, 0, 2],
            EthernetProtocol::Ipv6,
        )
        .expect("test Ethernet header should fit");
        let udp_start = offset + Ipv6Packet::HEADER_LEN;
        let udp_len = UdpPacket::encode(
            &mut frame[udp_start..],
            IpAddress::Ipv6(source),
            IpAddress::Ipv6(destination),
            source_port,
            destination_port,
            payload,
        )
        .expect("test UDP datagram should fit");
        offset += Ipv6Packet::encode_header(
            &mut frame[offset..],
            source,
            destination,
            IpProtocol::Udp,
            udp_len,
            64,
        )
        .expect("test IPv6 header should fit");
        (frame, offset + udp_len)
    }

    fn ipv6_packet_too_big_frame(
        router: Ipv6Address,
        local: Ipv6Address,
        quoted_frame: &[u8],
        mtu: u32,
    ) -> ([u8; ETHERNET_FRAME_BYTES], usize) {
        let ethernet =
            EthernetFrame::parse(quoted_frame).expect("quoted Ethernet frame should parse");
        let quoted = ethernet.payload;
        let mut frame = [0; ETHERNET_FRAME_BYTES];
        let icmp_len = Icmpv6Packet::PACKET_TOO_BIG_HEADER_LEN + quoted.len();
        let mut offset = EthernetFrame::encode_header(
            &mut frame,
            [0x02, 0, 0, 0, 0, 1],
            [0x02, 0, 0, 0, 0, 2],
            EthernetProtocol::Ipv6,
        )
        .expect("test Ethernet header should fit");
        offset += Ipv6Packet::encode_header(
            &mut frame[offset..],
            router,
            local,
            IpProtocol::Icmpv6,
            icmp_len,
            64,
        )
        .expect("test IPv6 ICMPv6 header should fit");
        offset +=
            Icmpv6Packet::encode_packet_too_big(&mut frame[offset..], router, local, mtu, quoted)
                .expect("test ICMPv6 Packet Too Big should fit");
        (frame, offset)
    }

    fn ipv4_udp_frame(
        source: Ipv4Address,
        source_port: u16,
        destination: Ipv4Address,
        destination_port: u16,
        payload: &[u8],
    ) -> ([u8; ETHERNET_FRAME_BYTES], usize) {
        let mut frame = [0; ETHERNET_FRAME_BYTES];
        let mut offset = EthernetFrame::encode_header(
            &mut frame,
            [0x02, 0, 0, 0, 0, 1],
            [0x02, 0, 0, 0, 0, 2],
            EthernetProtocol::Ipv4,
        )
        .expect("test Ethernet header should fit");
        let udp_start = offset + Ipv4Packet::MIN_HEADER_LEN;
        let udp_len = UdpPacket::encode(
            &mut frame[udp_start..],
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            source_port,
            destination_port,
            payload,
        )
        .expect("test UDP datagram should fit");
        offset += Ipv4Packet::encode_header(
            &mut frame[offset..],
            source,
            destination,
            IpProtocol::Udp,
            udp_len,
            1,
            64,
        )
        .expect("test IPv4 header should fit");
        (frame, offset + udp_len)
    }

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

    #[test]
    fn udp_datagram_limit_copies_only_at_outer_api_boundary() {
        let bytes = UdpPayload::copy_from_slice(b"abcdef");

        let limited = limit_udp_datagram_bytes(bytes, 3);

        assert_eq!(limited.as_ref(), b"abc");
    }

    fn test_network_shard() -> NetworkShard {
        NetworkShard::new(
            [0x02, 0, 0, 0, 0, 1],
            ETHERNET_FRAME_BYTES,
            8,
            RxChecksumOffload::none(),
            1,
            0,
            1,
        )
    }

    #[test]
    fn parse_ipv6_accepts_compressed_numeric_hosts() {
        assert_eq!(
            parse_ipv6("2001:db8::1")
                .expect("compressed IPv6 address should parse")
                .octets(),
            [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        );
        assert!(parse_ipv6("2001:db8:::1").is_none());
    }

    #[test]
    fn tcp_connect_to_ipv6_destination_uses_ipv6_source_address() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let remote = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let mut state = test_network_shard();
        state.stack.add_ipv6_address(Ipv6Cidr::new(local, 64));
        state.stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv6(remote),
            mac: [0x02, 0, 0, 0, 0, 2],
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });

        state
            .start_tcp_connect(IpAddress::Ipv6(remote), 443, 0)
            .expect("IPv6 TCP connect should allocate a socket");
        state
            .stack
            .drive_tcp(StackInstant::from_nanos(1))
            .expect("IPv6 TCP SYN should be queued");

        let frame = state
            .stack
            .take_outbound()
            .expect("IPv6 SYN frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv6 = Ipv6Packet::parse(ethernet.payload).expect("IPv6 packet should parse");
        let tcp = TcpPacket::parse(ipv6.payload).expect("TCP packet should parse");
        assert_eq!(ipv6.source, local);
        assert_eq!(ipv6.destination, remote);
        assert_eq!(tcp.destination_port, 443);
        assert!(tcp.flags.contains(TcpFlags::SYN));
    }

    #[test]
    fn udp_send_to_ipv6_destination_uses_ipv6_source_address() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let remote = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let mut state = test_network_shard();
        state.stack.add_ipv6_address(Ipv6Cidr::new(local, 64));
        state.stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv6(remote),
            mac: [0x02, 0, 0, 0, 0, 2],
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let socket = state
            .start_udp_bind(4040)
            .expect("IPv6 UDP test socket should bind")
            .socket;

        let written = state
            .try_send_udp(
                socket,
                IpAddress::Ipv6(remote),
                53,
                b"hello",
                StackInstant::from_nanos(1),
            )
            .expect("IPv6 UDP datagram should queue");
        assert_eq!(written, 5);
        let frame = state
            .stack
            .take_outbound()
            .expect("IPv6 UDP frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        assert_eq!(ethernet.protocol, EthernetProtocol::Ipv6);
        let ipv6 = Ipv6Packet::parse(ethernet.payload).expect("IPv6 packet should parse");
        assert_eq!(ipv6.source, local);
        assert_eq!(ipv6.destination, remote);
        assert_eq!(ipv6.next_header, IpProtocol::Udp);
        let udp = UdpPacket::parse(ipv6.payload).expect("UDP packet should parse");
        assert_eq!(udp.source_port, 4040);
        assert_eq!(udp.destination_port, 53);
        assert_eq!(udp.payload, b"hello");
    }

    #[test]
    fn oversized_udp_send_reports_datagram_too_large() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let remote = Ipv4Address::new([192, 0, 2, 20]);
        let mut state = test_network_shard();
        state.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        state.stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(remote),
            mac: [0x02, 0, 0, 0, 0, 2],
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let socket = state
            .start_udp_bind(4040)
            .expect("UDP test socket should bind")
            .socket;
        let payload = [0u8; ETHERNET_FRAME_BYTES];

        let error = state
            .try_send_udp(
                socket,
                IpAddress::Ipv4(remote),
                53,
                &payload,
                StackInstant::from_nanos(1),
            )
            .expect_err("oversized UDP datagram should fail");

        assert_eq!(error.detail, crate::NetworkErrorDetail::UdpDatagramTooLarge);
    }

    #[test]
    fn udp_receive_from_ipv6_source_preserves_typed_peer_address() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let remote = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let mut state = test_network_shard();
        state.stack.add_ipv6_address(Ipv6Cidr::new(local, 64));
        let socket = state
            .start_udp_bind(4040)
            .expect("IPv6 UDP test socket should bind")
            .socket;
        let (frame, len) = ipv6_udp_frame(remote, 53, local, 4040, b"hello");

        state
            .stack
            .receive_frame(&frame[..len], StackInstant::from_nanos(1))
            .expect("IPv6 UDP frame should be received");
        let datagram = state
            .poll_udp_receive(socket, usize::MAX)
            .expect("IPv6 UDP receive should poll")
            .expect("IPv6 UDP datagram should be queued");

        assert_eq!(datagram.address, NetworkIpAddress::Ipv6(remote));
        assert_eq!(datagram.port, 53);
        assert_eq!(datagram.bytes.as_ref(), b"hello");
    }

    #[test]
    fn connected_udp_receive_filters_at_stack_binding() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let expected_peer = Ipv4Address::new([192, 0, 2, 20]);
        let other_peer = Ipv4Address::new([192, 0, 2, 30]);
        let mut state = test_network_shard();
        state.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = state
            .start_udp_bind(4040)
            .expect("UDP test socket should bind")
            .socket;
        state
            .connect_udp_socket(socket, IpAddress::Ipv4(expected_peer), 53)
            .expect("UDP socket should connect");

        let (other_frame, other_len) = ipv4_udp_frame(other_peer, 53, local, 4040, b"other");
        state
            .stack
            .receive_frame(&other_frame[..other_len], StackInstant::from_nanos(1))
            .expect("unmatched UDP frame should be handled");
        let (expected_frame, expected_len) =
            ipv4_udp_frame(expected_peer, 53, local, 4040, b"expected");
        state
            .stack
            .receive_frame(&expected_frame[..expected_len], StackInstant::from_nanos(2))
            .expect("matched UDP frame should be handled");

        let datagram = state
            .poll_udp_receive(socket, usize::MAX)
            .expect("UDP receive should poll")
            .expect("matched UDP datagram should be queued");
        assert_eq!(
            datagram.address,
            NetworkIpAddress::Ipv4(map_ipv4_address(expected_peer))
        );
        assert_eq!(datagram.port, 53);
        assert_eq!(datagram.bytes.as_ref(), b"expected");
        assert!(
            state
                .poll_udp_receive(socket, usize::MAX)
                .expect("UDP receive should poll")
                .is_none()
        );
    }

    #[test]
    fn udp_reconnect_updates_stack_binding() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let first_peer = Ipv4Address::new([192, 0, 2, 20]);
        let second_peer = Ipv4Address::new([192, 0, 2, 30]);
        let mut state = test_network_shard();
        state.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = state
            .start_udp_bind(4040)
            .expect("UDP test socket should bind")
            .socket;
        state
            .connect_udp_socket(socket, IpAddress::Ipv4(first_peer), 53)
            .expect("UDP socket should connect");
        state
            .connect_udp_socket(socket, IpAddress::Ipv4(second_peer), 53)
            .expect("UDP socket should reconnect");

        let (first_frame, first_len) = ipv4_udp_frame(first_peer, 53, local, 4040, b"first");
        state
            .stack
            .receive_frame(&first_frame[..first_len], StackInstant::from_nanos(1))
            .expect("old peer UDP frame should be handled");
        let (second_frame, second_len) = ipv4_udp_frame(second_peer, 53, local, 4040, b"second");
        state
            .stack
            .receive_frame(&second_frame[..second_len], StackInstant::from_nanos(2))
            .expect("new peer UDP frame should be handled");

        let datagram = state
            .poll_udp_receive(socket, usize::MAX)
            .expect("UDP receive should poll")
            .expect("new peer UDP datagram should be queued");
        assert_eq!(
            datagram.address,
            NetworkIpAddress::Ipv4(map_ipv4_address(second_peer))
        );
        assert_eq!(datagram.bytes.as_ref(), b"second");
    }

    #[test]
    fn udp_disconnect_restores_wildcard_when_unambiguous() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut state = test_network_shard();
        state.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = state
            .start_udp_bind(4040)
            .expect("UDP test socket should bind")
            .socket;
        state
            .connect_udp_socket(socket, IpAddress::Ipv4(peer), 53)
            .expect("UDP socket should connect");
        state
            .disconnect_udp_socket(socket)
            .expect("UDP socket should disconnect");

        let (frame, len) = ipv4_udp_frame(peer, 53, local, 4040, b"wildcard");
        state
            .stack
            .receive_frame(&frame[..len], StackInstant::from_nanos(1))
            .expect("UDP frame should be handled after disconnect");
        let datagram = state
            .poll_udp_receive(socket, usize::MAX)
            .expect("UDP receive should poll")
            .expect("wildcard UDP datagram should be queued");
        assert_eq!(datagram.bytes.as_ref(), b"wildcard");
    }

    #[test]
    fn udp_disconnect_rejects_ambiguous_wildcard_binding() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let first_peer = Ipv4Address::new([192, 0, 2, 20]);
        let second_peer = Ipv4Address::new([192, 0, 2, 30]);
        let mut state = test_network_shard();
        state.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let first = state
            .start_udp_bind(4040)
            .expect("first UDP test socket should bind")
            .socket;
        state
            .connect_udp_socket(first, IpAddress::Ipv4(first_peer), 53)
            .expect("first UDP socket should connect");
        let second_stack_socket = state
            .stack
            .open_udp(UdpSocketBinding::connected(
                UdpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 4040,
                },
                UdpEndpoint {
                    address: IpAddress::Ipv4(second_peer),
                    port: 53,
                },
            ))
            .expect("second connected stack UDP socket should bind");
        let second = state.insert_udp_socket(
            second_stack_socket,
            UdpSocketBinding::connected(
                UdpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 4040,
                },
                UdpEndpoint {
                    address: IpAddress::Ipv4(second_peer),
                    port: 53,
                },
            ),
        );

        let error = state
            .disconnect_udp_socket(first)
            .expect_err("disconnect should reject ambiguous wildcard binding");
        assert_eq!(error.kind, crate::UdpErrorKind::Unavailable);
        assert_eq!(error.detail, crate::NetworkErrorDetail::UdpPortInUse);
        state.remove_udp_socket(second);
    }

    #[test]
    fn explicit_ephemeral_udp_binding_ids_route_to_port_owner_shard() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let peer = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let router = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
        let state = super::NetworkShardSet::new(2, |index| {
            NetworkShard::new(
                [0x02, 0, 0, 0, 0, 1],
                ETHERNET_FRAME_BYTES,
                8,
                RxChecksumOffload::none(),
                1 + index as u32,
                index,
                2,
            )
        });
        let binding = state
            .with_local_port(49_153, |shard| shard.start_udp_bind(49_153))
            .expect("explicit ephemeral UDP bind should allocate on owner shard");

        state.with_handle(binding.socket, |shard| {
            assert_eq!(shard.shard_idx, 1);
            assert_eq!(
                shard
                    .udp_socket(binding.socket)
                    .expect("bound UDP socket should be on handle shard")
                    .binding
                    .local_port,
                49_153
            );
        });

        let payload = [0u8; 1233];
        let (packet_too_big, packet_too_big_len) = state.with_handle(binding.socket, |shard| {
            shard.stack.add_ipv6_address(Ipv6Cidr::new(local, 64));
            shard.stack.learn_neighbor(NeighborEntry {
                ip: IpAddress::Ipv6(peer),
                mac: [0x02, 0, 0, 0, 0, 2],
                state: NeighborState::Reachable,
                updated_at: StackInstant::from_nanos(0),
            });
            let written = shard
                .try_send_udp(
                    binding.socket,
                    IpAddress::Ipv6(peer),
                    53,
                    &payload,
                    StackInstant::from_nanos(1),
                )
                .expect("owner shard should queue initial IPv6 UDP datagram");
            assert_eq!(written, payload.len());
            let quoted = shard
                .stack
                .take_outbound()
                .expect("owner shard should queue initial IPv6 UDP datagram");
            ipv6_packet_too_big_frame(router, local, quoted.as_slice(), 1280)
        });

        let port = super::peek_local_port(&packet_too_big[..packet_too_big_len]);
        assert_eq!(port, Some(49_153));
        let shard_idx = super::shard_idx_for_port(port, state.shard_count());
        assert_eq!(shard_idx, 1);
        state
            .shard_at(shard_idx)
            .lock()
            .stack
            .receive_frame(
                &packet_too_big[..packet_too_big_len],
                StackInstant::from_nanos(2),
            )
            .expect("ICMPv6 Packet Too Big should update owner shard PMTU");

        state.with_handle(binding.socket, |shard| {
            let error = shard
                .try_send_udp(
                    binding.socket,
                    IpAddress::Ipv6(peer),
                    53,
                    &payload,
                    StackInstant::from_nanos(3),
                )
                .expect_err("owner shard PMTU should cap later UDP sends");
            assert_eq!(error.detail, crate::NetworkErrorDetail::UdpDatagramTooLarge);
        });
    }

    #[test]
    fn udp_multicast_join_and_leave_update_stack_delivery() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let mut state = test_network_shard();
        state.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = state
            .start_udp_bind(4040)
            .expect("UDP test socket should bind")
            .socket;
        let (first, first_len) = ipv4_udp_frame(peer, 5353, group, 4040, b"first");
        let (second, second_len) = ipv4_udp_frame(peer, 5353, group, 4040, b"second");
        let (third, third_len) = ipv4_udp_frame(peer, 5353, group, 4040, b"third");

        state
            .stack
            .receive_frame(&first[..first_len], StackInstant::from_nanos(1))
            .expect("unjoined multicast frame should parse");
        assert!(
            state
                .poll_udp_receive(socket, usize::MAX)
                .expect("UDP receive should poll")
                .is_none()
        );

        state
            .join_multicast_v4(
                map_ipv4_address(group),
                crate::Ipv4Address::new([0, 0, 0, 0]),
            )
            .expect("unspecified multicast interface should resolve to primary IPv4");
        state
            .stack
            .receive_frame(&second[..second_len], StackInstant::from_nanos(2))
            .expect("joined multicast frame should parse");
        let datagram = state
            .poll_udp_receive(socket, usize::MAX)
            .expect("UDP receive should poll")
            .expect("joined multicast frame should be queued");
        assert_eq!(
            datagram.address,
            NetworkIpAddress::Ipv4(map_ipv4_address(peer))
        );
        assert_eq!(datagram.port, 5353);
        assert_eq!(datagram.bytes.as_ref(), b"second");

        state
            .leave_multicast_v4(
                map_ipv4_address(group),
                crate::Ipv4Address::new([0, 0, 0, 0]),
            )
            .expect("unspecified multicast interface should leave primary IPv4");
        state
            .stack
            .receive_frame(&third[..third_len], StackInstant::from_nanos(3))
            .expect("left multicast frame should parse");
        assert!(
            state
                .poll_udp_receive(socket, usize::MAX)
                .expect("UDP receive should poll")
                .is_none()
        );
    }

    #[test]
    fn udp_multicast_join_maps_invalid_interface_and_group_errors() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let other = Ipv4Address::new([192, 0, 2, 11]);
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let unicast = Ipv4Address::new([192, 0, 2, 20]);
        let mut state = test_network_shard();
        state.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));

        let error = state
            .join_multicast_v4(map_ipv4_address(group), map_ipv4_address(other))
            .expect_err("unowned multicast interface should fail");
        assert_eq!(
            error.detail,
            crate::NetworkErrorDetail::UdpMulticastInterfaceUnavailable
        );

        let error = state
            .join_multicast_v4(map_ipv4_address(unicast), map_ipv4_address(local))
            .expect_err("unicast multicast group should fail");
        assert_eq!(
            error.detail,
            crate::NetworkErrorDetail::UdpMulticastJoinFailed
        );

        let error = state
            .leave_multicast_v4(map_ipv4_address(group), map_ipv4_address(local))
            .expect_err("unjoined multicast group leave should fail");
        assert_eq!(
            error.detail,
            crate::NetworkErrorDetail::UdpMulticastLeaveFailed
        );
    }

    #[test]
    fn tcp_listen_on_ipv6_address_accepts_ipv6_peer() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let remote = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let mut state = test_network_shard();
        state.stack.add_ipv6_address(Ipv6Cidr::new(local, 64));
        state.stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv6(remote),
            mac: [0x02, 0, 0, 0, 0, 2],
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let listener = state
            .start_tcp_listen(
                NetworkIpAddress::Ipv6(local),
                8080,
                TcpListenBacklog::new(1),
            )
            .expect("IPv6 TCP listen should allocate a listener");

        let (syn, syn_len) = ipv6_tcp_frame(
            remote,
            local,
            TcpHeader {
                source_port: 49152,
                destination_port: 8080,
                sequence: 10,
                acknowledgement: 0,
                flags: TcpFlags::SYN,
                window_size: u16::MAX,
            },
        );
        state
            .stack
            .receive_frame(&syn[..syn_len], StackInstant::from_nanos(1))
            .expect("IPv6 SYN should be accepted by listener");
        state
            .stack
            .drive_tcp(StackInstant::from_nanos(1))
            .expect("IPv6 SYN-ACK should be queued");

        let frame = state
            .stack
            .take_outbound()
            .expect("IPv6 SYN-ACK frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv6 = Ipv6Packet::parse(ethernet.payload).expect("IPv6 packet should parse");
        let syn_ack = TcpPacket::parse(ipv6.payload).expect("TCP packet should parse");
        assert!(syn_ack.flags.contains(TcpFlags::SYN.union(TcpFlags::ACK)));

        let (ack, ack_len) = ipv6_tcp_frame(
            remote,
            local,
            TcpHeader {
                source_port: 49152,
                destination_port: 8080,
                sequence: 11,
                acknowledgement: syn_ack.sequence.wrapping_add(1),
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
        );
        state
            .stack
            .receive_frame(&ack[..ack_len], StackInstant::from_nanos(2))
            .expect("IPv6 final ACK should establish accepted socket");

        let accepted = state
            .poll_tcp_accept(listener.listener)
            .expect("IPv6 accept should poll")
            .expect("accepted IPv6 stream should be queued");
        assert_eq!(accepted.address, NetworkIpAddress::Ipv6(remote));
        assert_eq!(accepted.port, 49152);
    }

    #[test]
    fn network_poll_state_adapts_tx_submit_budget() {
        let poll = NetworkPollState::new(8, 8, 8);
        assert_eq!(poll.budget().tx_frames, 8);

        poll.complete(NetworkPollProgress {
            received_frames: 0,
            reclaimed_tx: 0,
            transmitted_frames: 8,
        });
        assert_eq!(poll.budget().tx_frames, 16);

        poll.complete(NetworkPollProgress {
            received_frames: 0,
            reclaimed_tx: 0,
            transmitted_frames: 0,
        });
        assert_eq!(poll.budget().tx_frames, 8);
    }

    #[test]
    fn network_poll_progress_separates_receive_saturation() {
        let budget = NetworkPollBudget {
            rx_frames: 8,
            tx_completions: 8,
            tx_frames: 8,
        };
        let tx_only = NetworkPollProgress {
            received_frames: 0,
            reclaimed_tx: 8,
            transmitted_frames: 8,
        };
        let rx_full = NetworkPollProgress {
            received_frames: 8,
            reclaimed_tx: 0,
            transmitted_frames: 0,
        };

        assert!(!tx_only.receive_saturated(budget));
        assert!(tx_only.saturated(budget));
        assert!(rx_full.receive_saturated(budget));
    }

    #[test]
    fn network_poll_progress_counts_any_device_or_stack_work_as_busy() {
        assert!(
            NetworkPollProgress {
                received_frames: 0,
                reclaimed_tx: 0,
                transmitted_frames: 0,
            }
            .is_idle()
        );

        for progress in [
            NetworkPollProgress {
                received_frames: 1,
                reclaimed_tx: 0,
                transmitted_frames: 0,
            },
            NetworkPollProgress {
                received_frames: 0,
                reclaimed_tx: 1,
                transmitted_frames: 0,
            },
            NetworkPollProgress {
                received_frames: 0,
                reclaimed_tx: 0,
                transmitted_frames: 1,
            },
        ] {
            assert!(!progress.is_idle());
        }
    }

    #[test]
    fn tx_batch_matches_outbound_slab_capacity() {
        assert_eq!(NETWORK_TX_BATCH_FRAMES, MAX_OUTBOUND_FRAMES);
    }

    #[test]
    fn packet_pump_continues_busy_slice_before_yielding() {
        let mut cadence = NetworkPumpCadence::new();
        let progress = NetworkPollProgress {
            received_frames: 1,
            reclaimed_tx: 0,
            transmitted_frames: 0,
        };
        let budget = NetworkPollBudget {
            rx_frames: 8,
            tx_completions: 8,
            tx_frames: 8,
        };

        for _ in 1..NETWORK_BUSY_POLL_ROUNDS {
            assert_eq!(
                cadence.complete(progress, budget),
                NetworkPumpAction::Continue
            );
        }
        assert_eq!(cadence.complete(progress, budget), NetworkPumpAction::Yield);
        assert_eq!(
            cadence.complete(progress, budget),
            NetworkPumpAction::Continue
        );
    }

    #[test]
    fn packet_pump_idle_progress_resets_busy_slice() {
        let mut cadence = NetworkPumpCadence::new();
        let busy = NetworkPollProgress {
            received_frames: 0,
            reclaimed_tx: 1,
            transmitted_frames: 0,
        };
        let idle = NetworkPollProgress {
            received_frames: 0,
            reclaimed_tx: 0,
            transmitted_frames: 0,
        };
        let budget = NetworkPollBudget {
            rx_frames: 8,
            tx_completions: 8,
            tx_frames: 8,
        };

        assert_eq!(cadence.complete(busy, budget), NetworkPumpAction::Continue);
        assert_eq!(cadence.complete(idle, budget), NetworkPumpAction::Wait);
        for _ in 1..NETWORK_BUSY_POLL_ROUNDS {
            assert_eq!(cadence.complete(busy, budget), NetworkPumpAction::Continue);
        }
        assert_eq!(cadence.complete(busy, budget), NetworkPumpAction::Yield);
    }
}
