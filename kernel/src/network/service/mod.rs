mod error;
use error::*;
mod pump;
use pump::*;
mod tcp;
use tcp::*;
mod udp;
use udp::*;
mod component;
use component::*;
mod shard;
use shard::*;

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
    DhcpMessageType, DhcpPacket, DnsQuestionWriter, DnsRecordType, DnsResponse, EthernetFrame,
    EthernetProtocol, Icmpv4Packet, Icmpv6Packet, IpAddress, IpCidr, IpProtocol, Ipv4Address,
    Ipv4Cidr, Ipv4Packet, Ipv6Address, Ipv6Cidr, Ipv6Packet, MAX_OUTBOUND_FRAMES, NeighborEntry,
    NetworkInterface as NetworkDevice,
    OutboundBatchStatus, PacketBuffer, Route, RouteTable, RxChecksumOffload, Stack, StackConfig,
    StackError, StackEvent, StackInstant, TcpConnectState, TcpConnectTerminalError, TcpEndpoint,
    TcpListenBacklog, TcpPacket, TcpReadIntoState, TcpReadState, UdpEndpoint, UdpPacket,
    UdpPayload, UdpSocketBinding, UdpSocketError,
};
use spin::{Mutex as SpinMutex, RwLock as SpinRwLock};

use crate::SocketReadiness;
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
    /// SLAAC-configured and link-local IPv6 addresses. Republished to
    /// every shard the same way the IPv4 table is, so a socket created
    /// on any processor sees the addresses autoconfiguration installed
    /// on the shard that received the Router Advertisement.
    ipv6_addresses: SnapshotCell<NetworkIpv6AddressTable>,
    routes: SnapshotCell<RouteTable>,
    neighbors: SnapshotCell<NetworkNeighborTable>,
    dns_servers: SnapshotCell<DhcpDnsServers>,
    /// Recursive resolvers learned from a Router Advertisement's RDNSS
    /// option, the IPv6 counterpart of the DHCPv4 resolver list.
    ipv6_dns_servers: SnapshotCell<NetworkIpv6DnsServers>,
}

struct SnapshotCell<T> {
    current: SpinRwLock<StdArc<T>>,
}

#[derive(Clone, Debug)]
struct NetworkIpv4AddressTable {
    entries: Vec<Ipv4Cidr>,
}

#[derive(Clone, Debug)]
struct NetworkIpv6AddressTable {
    entries: Vec<Ipv6Cidr>,
}

#[derive(Clone, Debug, Default)]
struct NetworkIpv6DnsServers {
    entries: Vec<Ipv6Address>,
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

impl NetworkControlPlane {
    fn new() -> Self {
        Self {
            ipv4_addresses: SnapshotCell::new(NetworkIpv4AddressTable::new()),
            ipv6_addresses: SnapshotCell::new(NetworkIpv6AddressTable::new()),
            routes: SnapshotCell::new(RouteTable::new()),
            neighbors: SnapshotCell::new(NetworkNeighborTable::new()),
            dns_servers: SnapshotCell::new(DhcpDnsServers::new()),
            ipv6_dns_servers: SnapshotCell::new(NetworkIpv6DnsServers::default()),
        }
    }

    fn synchronize_shard(&self, shard: &mut NetworkShard) {
        shard
            .stack
            .replace_ipv4_addresses(self.ipv4_addresses.load_full().entries.iter().copied());
        shard
            .stack
            .replace_ipv6_addresses(self.ipv6_addresses.load_full().entries.iter().copied());
        shard
            .stack
            .replace_ipv6_dns_servers(self.ipv6_dns_servers.load_full().entries.iter().copied());
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
        self.ipv6_addresses
            .store(StdArc::new(NetworkIpv6AddressTable {
                entries: shard.stack.ipv6_addresses().collect(),
            }));
        self.ipv6_dns_servers
            .store(StdArc::new(NetworkIpv6DnsServers {
                entries: shard.stack.ipv6_dns_servers().collect(),
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

    fn list_ipv6_addresses(&self) -> Vec<Ipv6Cidr> {
        self.ipv6_addresses.load_full().entries.clone()
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

impl NetworkIpv6AddressTable {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
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
        // The stack only ever attaches offload metadata when the device
        // finishes both TCP and UDP checksums; partial support would
        // force a per-frame protocol branch for no measured win.
        let tx_checksum_offload = capabilities.checksum.tx_tcp && capabilities.checksum.tx_udp;
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
            .with_rx_checksum_offload(rx_checksum_offload)
            .with_tx_checksum_offload(tx_checksum_offload);
        let stack_rx_budget = Stack::new(stack_config).config().rx_budget;
        let state = NetworkShardSet::new(shard_count, |index| {
            let staggered_xid = transaction_id.wrapping_add(index as u32);
            NetworkShard::new(
                mac,
                max_frame_len,
                rx_poll_budget,
                rx_checksum_offload,
                tx_checksum_offload,
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

    async fn wait_for_ipv4_ping(&self, deadline_nanos: u64) -> Result<(), PingError> {
        self.wait_for_ipv4_configured(
            deadline_nanos,
            ping_configuration_timeout,
            ping_configuration_error,
        )
        .await
    }

    /// Picks the first resolved address this interface can actually
    /// reach, in resolver order.
    ///
    /// A dual-family lookup routinely returns AAAA records on a link
    /// where nothing configured an IPv6 address, and vice versa, so
    /// connecting blindly to the first answer would fail on exactly the
    /// hosts that also published a usable one. Addresses of a family
    /// this interface holds no source address for are skipped; when no
    /// family is configured the resolver order stands, leaving the
    /// unreachability to surface from the send path.
    fn first_usable_address(
        &self,
        addresses: impl IntoIterator<Item = NetworkIpAddress>,
    ) -> Option<IpAddress> {
        let has_ipv4 = !self.inner.control.list_ipv4_addresses().is_empty();
        let has_ipv6 = !self.inner.control.list_ipv6_addresses().is_empty();
        let mut fallback = None;
        for address in addresses {
            let address = map_network_ip_address(address);
            let usable = match address {
                IpAddress::Ipv4(_) => has_ipv4,
                IpAddress::Ipv6(_) => has_ipv6,
            };
            if usable {
                return Some(address);
            }
            fallback.get_or_insert(address);
        }
        fallback
    }

    async fn wait_for_ipv4_configured<Error>(
        &self,
        deadline_nanos: u64,
        timeout_error: fn() -> Error,
        configuration_error: fn(NetworkConfigurationError) -> Error,
    ) -> Result<(), Error> {
        self.wait_for_configured(deadline_nanos, timeout_error, configuration_error, |ready| {
            ready
        })
        .await
    }

    /// Waits until a resolver is reachable in either family.
    ///
    /// DNS does not require IPv4 specifically: a link that only ever
    /// completed IPv6 autoconfiguration still has a resolver if a Router
    /// Advertisement carried an RDNSS option, and the query goes out
    /// over IPv6. Gating this on the DHCPv4 lease alone would make that
    /// path unreachable.
    async fn wait_for_dns_configured<Error>(
        &self,
        deadline_nanos: u64,
        timeout_error: fn() -> Error,
        configuration_error: fn(NetworkConfigurationError) -> Error,
    ) -> Result<(), Error> {
        self.wait_for_configured(deadline_nanos, timeout_error, configuration_error, |ready| {
            ready || self.has_ipv6_resolver()
        })
        .await
    }

    /// Whether IPv6 autoconfiguration produced both an address to send
    /// from and a resolver to send to.
    fn has_ipv6_resolver(&self) -> bool {
        self.inner
            .state
            .with(|state| state.stack.ipv6_dns_servers().next().is_some())
            && !self.inner.control.list_ipv6_addresses().is_empty()
    }

    async fn wait_for_configured<Error>(
        &self,
        deadline_nanos: u64,
        timeout_error: fn() -> Error,
        configuration_error: fn(NetworkConfigurationError) -> Error,
        ready: impl Fn(bool) -> bool,
    ) -> Result<(), Error> {
        loop {
            let ipv4_configured = self
                .drive_ipv4_configuration()
                .await
                .map_err(configuration_error)?;
            if ready(ipv4_configured) {
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
        //
        // IPv6 stateless autoconfiguration rides the same poll and the
        // same shard. Router Advertisements are ICMPv6, which the RX
        // demux has no local port for and therefore also routes to
        // shard 0, so the shard that solicits is the shard that
        // receives — and `publish_from_shard` republishes the resulting
        // addresses and routes to the rest of the set.
        let configured = self
            .inner
            .state
            .with_local_port(DHCP_CLIENT_PORT, |state| {
                state
                    .drive_dhcp(now)
                    .map_err(NetworkConfigurationError::Control)?;
                state
                    .drive_ipv6_autoconfig(now)
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

    async fn drive_ping(&self) -> Result<(), PingError> {
        self.drive_network(NetworkPollSource::Ping)
            .await
            .map_err(|error| PingError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))
    }

    async fn drive_network(&self, source: NetworkPollSource) -> Result<(), IoError> {
        let _ = self.poll_network_once(source).await?;
        Ok(())
    }

    fn synchronize_control_plane(&self) {
        self.inner
            .state
            .for_each(|state| self.inner.control.synchronize_shard(state));
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

    pub fn hardware_address(&self) -> [u8; 6] {
        self.inner.device.mac_address()
    }

    pub async fn ipv4_cidr(&self) -> Option<crate::Ipv4Cidr> {
        self.inner.control.list_ipv4_addresses().into_iter().next()
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

fn usize_to_u64(value: usize, label: &'static str) -> u64 {
    u64::try_from(value).unwrap_or_else(|_| panic!("{label} does not fit into u64"))
}

#[cfg(test)]
mod tests {
    use helios_netstack::{
        ETHERNET_FRAME_BYTES, EthernetFrame, EthernetProtocol, Icmpv6Packet, IpAddress, IpProtocol,
        Ipv4Address, Ipv4Cidr, Ipv4Packet, Ipv6Address, Ipv6Cidr, Ipv6Packet, MAX_OUTBOUND_FRAMES,
        NeighborEntry, NeighborState, RxChecksumOffload, StackInstant, TcpFlags, TcpHeader,
        TcpListenBacklog, TcpPacket, TransportChecksum, UdpEndpoint, UdpPacket, UdpPayload,
        UdpSocketBinding,
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
            TransportChecksum::Software,
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
            TransportChecksum::Software,
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
            TransportChecksum::Software,
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
            false,
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
            .start_tcp_connect(
                IpAddress::Ipv6(remote),
                443,
                0,
                helios_netstack::DEFAULT_HOP_LIMIT,
            )
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
                false,
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
                helios_netstack::DEFAULT_HOP_LIMIT,
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
