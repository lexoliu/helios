mod address;
use address::*;
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
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use core::task::Poll;
use core::time::Duration;

use bytes::Bytes;
use helios_hal::cpu::{Cpu, HardwarePerfCounters};
use helios_hal::io::IoError;
use helios_netstack::{
    DEFAULT_HOP_LIMIT, DHCP_CLIENT_PORT, DHCP_SERVER_PORT, DNS_PORT, DhcpClientMessage,
    DhcpDnsServers, DhcpMessageType, DhcpPacket, DnsQuestionWriter, DnsRecordType, DnsResponse,
    EthernetFrame, EthernetProtocol, FlowTuple, IcmpEchoKey, IcmpEchoReply, Icmpv4Packet,
    Icmpv6Packet, IpAddress, IpCidr, IpProtocol, Ipv4Address, Ipv4Cidr, Ipv4Packet, Ipv6Address,
    Ipv6Cidr, Ipv6Packet, MAX_OUTBOUND_FRAMES, NeighborEntry, NetworkInterface as NetworkDevice,
    OutboundBatchStatus, Route, RouteTable, RxChecksumOffload, RxFrame, SegmentationOffload, Stack,
    StackConfig, StackError, StackEvent, StackInstant, TcpConnectState, TcpConnectTerminalError,
    TcpEndpoint, TcpListenBacklog, TcpPacket, TcpReadIntoState, TcpReadState, UdpEgress,
    UdpEndpoint, UdpPacket, UdpPayload, UdpSocketBinding, UdpSocketError, flow_hash,
};
use spin::{Mutex as SpinMutex, RwLock as SpinRwLock};

use crate::SocketReadiness;
use crate::{
    ComponentNetworkService, ComponentRuntimeState, DnsError, DnsErrorKind,
    Ipv4Address as KernelIpv4Address, Ipv4Cidr as KernelIpv4Cidr, Ipv4Route as KernelIpv4Route,
    MacAddress, NetworkAdminBackend, NetworkBridgeRequest, NetworkControlError, NetworkErrorDetail,
    NetworkIpAddress, NetworkPortId, PingError, PingErrorKind, PingReply, ProgressMark,
    ProgressSignal, RegisteredTcpReadBuffer, TcpAccepted, TcpError, TcpErrorKind, TcpListener,
    Timer, UdpBinding, UdpDatagram, UdpError, UdpErrorKind,
};
use triomphe::Arc;

const EPHEMERAL_PORT_START: u16 = 49_152;
const EPHEMERAL_PORT_END: u16 = 65_535;

/// The shard that owns everything the flow hash cannot name.
///
/// ARP and ICMP carry no four-tuple, and the DHCP client's exchange is
/// broadcast at a moment when the interface has no address to hash
/// with, so all of them are demultiplexed here on every backend. It is
/// also where unqualified control-plane operations run, so a route the
/// ARP reply taught and the query that reads it meet on one shard.
const DEFAULT_SHARD_IDX: usize = 0;

const INTERNAL_DHCP_SOCKET_INDEX: usize = 0;
/// Datagram slab slots every shard keeps for its own use, so a
/// replicated bind never lands on top of one. Only the DHCP client
/// needs one: the resolver uses an ordinary connected socket.
const INTERNAL_UDP_RESERVED_SLOTS: usize = 1;
const LOCAL_NETWORK_PORT: NetworkPortId = NetworkPortId::new(0);
const DHCP_RETRANSMIT_NANOS: u64 = 1_000_000_000;
const MAX_TCP_STREAM_HANDLES: usize = 256;
const MAX_TCP_LISTENER_HANDLES: usize = 64;
const MAX_UDP_SOCKET_HANDLES: usize = 256;
/// Polling cadence for a device that cannot interrupt.
///
/// Only the polling-only device model needs it: nothing on such a
/// device wakes a parked task, so a wait is cut into slices and the
/// caller re-drives the device on each one. Every interrupt-capable
/// path is event-driven instead — the device event for what this
/// processor drained, the owning shard's arrival signal for what
/// another processor drained on its behalf.
const NETWORK_PROGRESS_WAIT: Duration = Duration::from_micros(50);
/// How long an operation that owns a retransmission duty parks before
/// it re-drives that duty.
///
/// A query or a solicitation that was dropped on the wire produces no
/// event at all, so a purely event-driven wait would sit until the
/// caller's deadline and give up without ever asking twice. The
/// callers that own such a duty — the DNS resolver and the ping walk
/// while its next hop is still unresolved — bound their wait by this
/// instead. DHCP has a retransmission interval of its own
/// (`DHCP_RETRANSMIT_NANOS`) and bounds its wait by that.
const NETWORK_RETRANSMIT_WAIT: Duration = Duration::from_millis(250);
// AArch64/HVF local TCP diagnostics showed that matching the borrowed RX
// batch to the virtio polling budget moves receive work in the right
// direction without changing protocol semantics: the two 64 MiB TCP
// throughput workloads, one over kernel sockets and one over guest
// sockets, went 92/102 ms -> 89/97 ms, and rx-drain ns/event went
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
// to the guest, amortizing component-host and TCP drive cost without waiting
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
    /// Carrier the service last acted on. The device publishes link
    /// state out of its configuration-change interrupt, and every poll
    /// round compares it against this, so a link bounce reconfigures
    /// the interface exactly once without a task or a device poll of
    /// its own.
    link_up: AtomicBool,
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

/// What one queue pair's shard has moved, and how often its processor
/// was interrupted for it.
///
/// One record per shard, which is one per processor: the spread across
/// them is what says whether steering is working. A device that steers
/// puts each flow's frames on the queue whose processor owns the socket,
/// so both the frame counts and the interrupt counts spread; a device
/// that cannot leaves every frame on the default shard and every
/// interrupt on one processor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetworkQueueStats {
    /// The shard, and therefore the processor and the queue pair.
    pub id: u32,
    /// Frames this shard's stack took off the device.
    pub rx_frames: u64,
    /// Frames it handed back to the device.
    pub tx_frames: u64,
    /// Interrupts the device raised for this pair's own message. Zero on
    /// a transport that cannot tell its queues apart.
    pub interrupts: u64,
}

/// Per-shard network counters, one entry per processor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkStats {
    pub queues: Vec<NetworkQueueStats>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TcpStreamId(NonZeroU32);

impl From<TcpStreamId> for u64 {
    fn from(id: TcpStreamId) -> Self {
        u64::from(id.0.get())
    }
}

impl From<TcpStreamId> for ShardHandle {
    fn from(id: TcpStreamId) -> Self {
        ShardHandle::from_raw(id.0)
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

impl From<TcpListenerId> for ReplicaHandle {
    fn from(id: TcpListenerId) -> Self {
        ReplicaHandle::from_raw(id.0)
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

impl From<UdpSocketId> for ReplicaHandle {
    fn from(id: UdpSocketId) -> Self {
        ReplicaHandle::from_raw(id.0)
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

    /// Drops everything the interface learned from the link.
    ///
    /// The shards pick this up through `synchronize_shard` on their next
    /// poll, so the reset does not have to reach into them.
    fn reset_link_configuration(&self) {
        self.ipv4_addresses
            .store(StdArc::new(NetworkIpv4AddressTable::new()));
        self.ipv6_addresses
            .store(StdArc::new(NetworkIpv6AddressTable::new()));
        self.ipv6_dns_servers
            .store(StdArc::new(NetworkIpv6DnsServers::default()));
        self.routes.store(StdArc::new(RouteTable::new()));
        self.neighbors
            .store(StdArc::new(NetworkNeighborTable::new()));
        self.dns_servers.store(StdArc::new(DhcpDnsServers::new()));
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

    /// Fills a slot chosen by the caller rather than by the free list.
    ///
    /// A replicated socket occupies the same slot in every shard's slab,
    /// so the slot is picked once by `ReplicaSlots` and then written
    /// here on each shard. The slot is removed from this slab's free
    /// list so a later `insert` cannot hand it out again.
    fn insert_at(&mut self, index: usize, value: T) {
        let slot = self
            .slots
            .get_mut(index)
            .unwrap_or_else(|| panic!("network handle slot {index} is outside the slab"));
        assert!(
            slot.is_none(),
            "network handle slot {index} is already occupied"
        );
        *slot = Some(value);
        let position = self.free[..self.free_len]
            .iter()
            .position(|free| *free == index)
            .unwrap_or_else(|| panic!("network handle slot {index} was not free"));
        self.free_len -= 1;
        self.free.swap(position, self.free_len);
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
        // An oversized frame never fits a packet buffer: its payload
        // leaves as a borrowed scatter descriptor or not at all. A
        // device whose DMA cannot read borrowed bytes therefore
        // segments nothing, whatever its feature bits say.
        let segmentation = if capabilities.direct_tx_dma {
            capabilities.segmentation
        } else {
            SegmentationOffload::none()
        };
        let stack_config = StackConfig::new(mac, max_frame_len)
            .with_rx_budget(rx_poll_budget)
            .with_rx_checksum_offload(rx_checksum_offload)
            .with_tx_checksum_offload(tx_checksum_offload)
            .with_direct_tx_dma(capabilities.direct_tx_dma)
            .with_segmentation_offload(segmentation);
        let stack_rx_budget = Stack::new(stack_config).config().rx_budget;
        let state = NetworkShardSet::new(shard_count, |index| {
            let staggered_xid = transaction_id.wrapping_add(index as u32);
            NetworkShard::new(stack_config, staggered_xid, index, shard_count)
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
                link_up: AtomicBool::new(device.link_state().is_up()),
                device,
            }),
        }
    }

    /// Per-shard frame and interrupt counters.
    ///
    /// Read without locking a shard: the counters are relaxed atomics
    /// outside the shard mutex, so a statistics sweep never contends
    /// with the receive path it is measuring.
    pub fn stats(&self) -> NetworkStats {
        NetworkStats {
            queues: (0..self.inner.state.shard_count())
                .map(|idx| {
                    let (rx_frames, tx_frames) = self.inner.state.frame_counts(idx);
                    NetworkQueueStats {
                        id: idx as u32,
                        rx_frames,
                        tx_frames,
                        interrupts: self.inner.device.queue_interrupts(idx),
                    }
                })
                .collect(),
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
        let candidates = self.resolve_host_ping(host, deadline_nanos).await?;
        // One identifier covers the whole request; the sequence number
        // separates the attempts, so a late reply to an abandoned
        // candidate can never be mistaken for the current one.
        let identifier = self
            .inner
            .state
            .with_mut(NetworkShard::next_icmp_echo_identifier);
        let mut sequence: u16 = 0;
        // Every candidate shares the caller's deadline, exactly as the
        // connect walk does: a destination that is unreachable in one
        // family hands whatever time is left to the next address.
        attempt_each_address(&candidates, move |destination| {
            sequence = sequence.wrapping_add(1);
            self.echo_address(
                IcmpEchoKey {
                    destination,
                    identifier,
                    sequence,
                },
                deadline_nanos,
            )
        })
        .await
    }

    /// Sends one echo request and resolves with its round-trip time.
    ///
    /// The request is retransmitted only while the next hop's
    /// link-layer address is still unresolved; once it is on the wire
    /// the task parks on the device event, the default shard's arrival
    /// signal, or its own deadline — whichever comes first.
    async fn echo_address(
        &self,
        key: IcmpEchoKey,
        deadline_nanos: u64,
    ) -> Result<PingReply, PingError> {
        if matches!(key.destination, IpAddress::Ipv4(_)) {
            self.wait_for_ipv4_ping(deadline_nanos).await?;
        }
        let payload = icmp_echo_payload();
        let mut sent_at_nanos = None;
        loop {
            // ICMP carries no local port, so echo replies demux to the
            // default shard; the mark is taken before the reply is
            // looked for so a reply another processor drains in between
            // does not sleep this task to its deadline.
            let wait = self.inner.state.default_shard_wait();
            self.drive_ping().await?;
            let now_nanos = self.now_nanos();
            match sent_at_nanos {
                None => {
                    let now = StackInstant::from_nanos(now_nanos);
                    let transmitted = self
                        .inner
                        .state
                        .with_mut(|state| state.send_icmp_echo_request(key, &payload, now))?;
                    if transmitted {
                        sent_at_nanos = Some(now_nanos);
                    }
                }
                Some(sent_at) => {
                    let reply: Option<IcmpEchoReply> = self
                        .inner
                        .state
                        .with_mut(|state| state.take_icmp_echo_reply(key));
                    if let Some(reply) = reply {
                        return Ok(PingReply {
                            address: map_ip_address(key.destination),
                            round_trip_nanos: self.now_nanos().saturating_sub(sent_at),
                            payload_bytes: reply.payload_bytes,
                        });
                    }
                }
            }
            if now_nanos >= deadline_nanos {
                return Err(PingError {
                    kind: PingErrorKind::Timeout,
                    detail: NetworkErrorDetail::IcmpEchoTimeout,
                });
            }
            self.drive_ping().await?;
            // Until the request is actually on the wire this walk owes
            // a retransmission — the next hop's link-layer address is
            // still being resolved and nothing will report that but the
            // caller's own retry. Once it is sent, the reply is the only
            // thing worth waking for.
            let bound = match sent_at_nanos {
                None => self.retransmit_wait(deadline_nanos, NETWORK_RETRANSMIT_WAIT),
                Some(_) => self.deadline_wait(deadline_nanos),
            };
            self.wait_for_shard_progress(wait, bound).await;
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

    /// The source address this interface would use to reach
    /// `destination`.
    ///
    /// Read from the default shard because the control plane
    /// republishes the address table to every shard, so any of them
    /// answers the same — and it has to be answerable before a shard is
    /// chosen, since the answer is half of the flow the choice hashes.
    fn local_address_for(&self, destination: IpAddress) -> Option<IpAddress> {
        self.inner.state.with(|state| match destination {
            IpAddress::Ipv4(_) => state
                .stack
                .primary_ipv4_address()
                .map(|cidr| IpAddress::Ipv4(cidr.address())),
            IpAddress::Ipv6(_) => state
                .stack
                .primary_ipv6_address()
                .map(|cidr| IpAddress::Ipv6(cidr.address())),
        })
    }

    /// Every resolved address worth attempting for this destination,
    /// in the order the connect walk should try them.
    ///
    /// A dual-family lookup routinely returns AAAA records on a link
    /// where nothing configured an IPv6 address, and vice versa, so
    /// connecting blindly to the first answer would fail on exactly the
    /// hosts that also published a usable one. `ConnectCandidates`
    /// owns that ordering; `None` means the lookup produced nothing to
    /// attempt.
    fn usable_addresses(
        &self,
        addresses: impl IntoIterator<Item = NetworkIpAddress>,
    ) -> Option<ConnectCandidates> {
        ConnectCandidates::new(
            !self.inner.control.list_ipv4_addresses().is_empty(),
            !self.inner.control.list_ipv6_addresses().is_empty(),
            addresses,
        )
    }

    async fn wait_for_ipv4_configured<Error>(
        &self,
        deadline_nanos: u64,
        timeout_error: fn() -> Error,
        configuration_error: fn(NetworkConfigurationError) -> Error,
    ) -> Result<(), Error> {
        self.wait_for_configured(
            deadline_nanos,
            timeout_error,
            configuration_error,
            |ready| ready,
        )
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
        self.wait_for_configured(
            deadline_nanos,
            timeout_error,
            configuration_error,
            |ready| ready || self.has_ipv6_resolver(),
        )
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
            // Interface configuration runs on the default shard, which
            // is where the DHCP exchange and the Router Advertisements
            // that answer it demux back to: neither has a flow the hash
            // can name.
            let wait = self
                .inner
                .state
                .shard_wait(self.inner.state.default_shard_idx());
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
            // A dropped DISCOVER or router solicitation produces no
            // event at all, so the wait is capped by the retransmission
            // interval the client state machine expects.
            self.wait_for_shard_progress(
                wait,
                self.retransmit_wait(deadline_nanos, Duration::from_nanos(DHCP_RETRANSMIT_NANOS)),
            )
            .await;
        }
    }

    async fn drive_ipv4_configuration(&self) -> Result<bool, NetworkConfigurationError> {
        self.drive_network(NetworkPollSource::Configuration)
            .await
            .map_err(NetworkConfigurationError::Device)?;
        let now = StackInstant::from_nanos(self.now_nanos());
        // DHCP runs on the default shard, and so does IPv6 stateless
        // autoconfiguration: the DHCP exchange is broadcast at a moment
        // when the interface has no address to hash a flow with, and a
        // Router Advertisement is ICMPv6 with no ports at all, so the
        // shard that solicits is the shard that receives.
        // `publish_from_shard` republishes the resulting addresses and
        // routes to the rest of the set.
        let configured = {
            let mut state = self.inner.state.shard_for_default().lock();
            state
                .drive_dhcp(now)
                .map_err(NetworkConfigurationError::Control)?;
            state
                .drive_ipv6_autoconfig(now)
                .map_err(NetworkConfigurationError::Control)?;
            if state.is_configured() {
                self.inner.control.publish_from_shard(&state);
            }
            state.is_configured()
        };
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

    /// Acts on a carrier change the device reported.
    ///
    /// Everything the interface learned — addresses, routes, neighbours,
    /// resolvers — describes the link that was there, so losing carrier
    /// drops it and regaining carrier starts configuration again from
    /// DHCP DISCOVER and router solicitation. Nothing here waits on the
    /// device: the driver publishes link state from its
    /// configuration-change interrupt and this is one atomic load per
    /// poll round.
    fn synchronize_link_state(&self) {
        let link_up = self.inner.device.link_state().is_up();
        if self.inner.link_up.swap(link_up, AtomicOrdering::AcqRel) == link_up {
            return;
        }
        // A link that just came back has stale configuration from the
        // link that went away, and one that just went away has
        // configuration that no longer describes anything, so both
        // transitions drop it.
        self.inner.control.reset_link_configuration();
        if !link_up {
            tracing::info!("network link down: interface configuration dropped");
            return;
        }
        let transaction_id = next_transaction_id(self.now_nanos() as u32);
        self.inner.state.for_each(|shard| {
            shard.restart_link_configuration(transaction_id.wrapping_add(shard.shard_idx as u32));
        });
        tracing::info!("network link up: reconfiguring the interface");
    }

    /// The wait the packet pump takes when a poll round found nothing.
    ///
    /// The pump has no caller deadline, but it does own the stack's
    /// timer duties — DHCP retransmission, TCP retransmit and
    /// delayed-ACK deadlines — so an interrupt-driven device still needs
    /// the wait bounded by the soonest of them. Left purely
    /// event-driven, a lost DHCP reply would never be retransmitted,
    /// because the wake that would drive the retransmit is the reply
    /// that never came.
    fn pump_wait(&self) -> Duration {
        let now = self.now_nanos();
        let next_stack_deadline = self
            .inner
            .state
            .min_tcp_deadline_nanos()
            .map_or(DHCP_RETRANSMIT_NANOS, |deadline| {
                deadline.saturating_sub(now).min(DHCP_RETRANSMIT_NANOS)
            });
        self.progress_wait(Duration::from_nanos(next_stack_deadline))
    }

    /// The wait to hand [`Self::wait_for_progress`] for a caller that
    /// owns a bound of its own — an operation deadline, a retransmit
    /// interval, the next protocol timer.
    ///
    /// An interrupt-driven device wakes the wait the moment it makes
    /// progress, so the timer only has to cover the caller's own bound.
    /// A polling-only device has nothing to wake it, so the wait is cut
    /// into polling-cadence slices and the caller re-drives the device
    /// on each one.
    fn progress_wait(&self, bound: Duration) -> Duration {
        if self.inner.device.capabilities().events.interrupts {
            bound
        } else {
            bound.min(NETWORK_PROGRESS_WAIT)
        }
    }

    /// The wait the packet pump takes: it is the producer for every
    /// shard, so it parks on the device — on the pair this processor
    /// drains, which is the only one its next poll will look at.
    async fn wait_for_progress(&self, duration: Duration) {
        if duration.is_zero() {
            return;
        }

        if !self.inner.device.capabilities().events.interrupts {
            self.inner.timer.sleep_for(duration).await;
            return;
        }

        let event = self.inner.device.wait_for_event_on(
            self.inner
                .state
                .shard_idx_for_processor(self.inner.cpu.current_processor()),
        );
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

    /// The wait every per-operation caller takes.
    ///
    /// An operation belongs to exactly one shard, and the frame that
    /// ends its wait can be drained by any processor. Three things can
    /// end it, and all three are races rather than polls:
    ///
    /// * the shard's arrival signal, raised by whichever processor
    ///   placed a frame in this shard — the cross-processor hand-off,
    ///   and the only one that covers a reply drained on a foreign CPU;
    /// * the device event, which covers transmit completions, link
    ///   changes and anything else the device reports;
    /// * `duration`, the caller's own bound — its deadline, or the
    ///   interval at which it owes a retransmission.
    ///
    /// `wait` must have been sampled before the caller inspected its
    /// shard; [`NetworkShardSet::shard_wait`] and its siblings are the
    /// only way to build one, and an arrival that lands between that
    /// inspection and this park resolves immediately rather than
    /// sleeping through the wake. A replicated socket samples the
    /// set-wide signal instead, because the shard its next connection
    /// or datagram lands on is not known until the flow is hashed.
    async fn wait_for_shard_progress(&self, wait: ShardWait, duration: Duration) {
        if duration.is_zero() {
            return;
        }

        let arrival = self.inner.state.arrival_for(wait.target).changed(wait.mark);
        let mut arrival = core::pin::pin!(arrival);

        if !self.inner.device.capabilities().events.interrupts {
            // A polling-only device wakes nothing by itself, so
            // `progress_wait` has already cut `duration` into polling
            // slices and the caller re-drives on each one. Another
            // processor's drain still cuts the slice short.
            let timer = self.inner.timer.sleep_for(duration);
            let mut timer = core::pin::pin!(timer);
            core::future::poll_fn(|cx| {
                if arrival.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(());
                }
                if timer.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(());
                }
                Poll::Pending
            })
            .await;
            return;
        }

        // The device event is taken on the pair this shard drains: a
        // completion on another pair is not progress this operation can
        // use, and on a device with per-queue interrupts that pair's
        // message is already delivered to this processor.
        let event = self
            .inner
            .device
            .wait_for_event_on(self.event_queue_idx(wait));
        let mut event = core::pin::pin!(event);
        if core::future::poll_fn(|cx| {
            Poll::Ready(arrival.as_mut().poll(cx).is_ready() || event.as_mut().poll(cx).is_ready())
        })
        .await
        {
            return;
        }

        let timer = self.inner.timer.sleep_for(duration);
        let mut timer = core::pin::pin!(timer);

        core::future::poll_fn(|cx| {
            if arrival.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
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

    /// The queue pair a wait should watch.
    ///
    /// A shard drains the pair with its own index, so an operation on
    /// one shard watches that pair. A replicated socket has no single
    /// shard, so it watches the pair this processor drains — the
    /// cross-shard hand-off is the arrival signal's job, not the
    /// device's.
    fn event_queue_idx(&self, wait: ShardWait) -> usize {
        match wait.target {
            WaitTarget::Shard(idx) => idx,
            WaitTarget::AnyShard => self
                .inner
                .state
                .shard_idx_for_processor(self.inner.cpu.current_processor()),
        }
    }

    /// The wait for a caller whose only bound is its own deadline.
    fn deadline_wait(&self, deadline_nanos: u64) -> Duration {
        self.progress_wait(Duration::from_nanos(
            deadline_nanos.saturating_sub(self.now_nanos()),
        ))
    }

    /// The wait for a caller that owes a retransmission: whichever of
    /// its deadline and its retransmission interval comes first.
    fn retransmit_wait(&self, deadline_nanos: u64, interval: Duration) -> Duration {
        self.deadline_wait(deadline_nanos).min(interval)
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
        self.inner.runtime_state.record_perf_metric_parts(
            crate::ProfileScope::Kernel,
            "kernel;network;",
            phase,
            crate::PerfSample {
                events: usize_to_u64(events, "network profile event count"),
                elapsed_nanos,
                counters,
                bytes: usize_to_u64(bytes, "network profile byte count"),
            },
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

/// Data an echo request carries.
///
/// A conventional `ping` sends 56 data bytes, so a target or middlebox
/// that sizes its reply on the request sees the shape it expects; the
/// contents are a fixed ramp so a truncated or rewritten echo shows up
/// in the reported `payload_bytes`.
const ICMP_ECHO_PAYLOAD_BYTES: usize = 56;

fn icmp_echo_payload() -> [u8; ICMP_ECHO_PAYLOAD_BYTES] {
    core::array::from_fn(|index| index as u8)
}

fn usize_to_u64(value: usize, label: &'static str) -> u64 {
    u64::try_from(value).unwrap_or_else(|_| panic!("{label} does not fit into u64"))
}

#[cfg(test)]
mod tests {
    use helios_netstack::{
        ETHERNET_FRAME_BYTES, EthernetFrame, EthernetProtocol, IcmpEchoKey, Icmpv4Packet,
        Icmpv6Packet, IpAddress, IpCidr, IpProtocol, Ipv4Address, Ipv4Cidr, Ipv4Packet,
        Ipv6Address, Ipv6Cidr, Ipv6Packet, MAX_OUTBOUND_FRAMES, NeighborEntry, NeighborState,
        Route, StackConfig, StackInstant, TcpFlags, TcpHeader, TcpListenBacklog, TcpPacket,
        TransportChecksum, UdpEndpoint, UdpPacket, UdpPayload, UdpSocketBinding, internet_checksum,
    };

    use alloc::vec::Vec;
    use bytes::Bytes;
    use futures_lite::future::{block_on, poll_once};
    use helios_netstack::RxFrame;

    use crate::test_support::RecordingSmpCpu;

    use super::{
        AddressAttemptError, DhcpClientState, HandleSlab, NETWORK_BUSY_POLL_ROUNDS,
        NETWORK_TX_BATCH_FRAMES, NetworkIpAddress, NetworkPollBudget, NetworkPollProgress,
        NetworkPollState, NetworkPumpAction, NetworkPumpCadence, NetworkShard, ReplicaHandle,
        TcpListenerId, UdpSocketId, icmp_echo_payload, limit_udp_datagram_bytes, map_ipv4_address,
        parse_ipv6,
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

    fn ipv4_tcp_frame(
        source: Ipv4Address,
        destination: Ipv4Address,
        header: TcpHeader,
    ) -> ([u8; ETHERNET_FRAME_BYTES], usize) {
        let mut frame = [0; ETHERNET_FRAME_BYTES];
        let mut offset = EthernetFrame::encode_header(
            &mut frame,
            [0x02, 0, 0, 0, 0, 1],
            [0x02, 0, 0, 0, 0, 2],
            EthernetProtocol::Ipv4,
        )
        .expect("test Ethernet header should fit");
        let tcp_start = offset + Ipv4Packet::MIN_HEADER_LEN;
        let tcp_len = TcpPacket::encode(
            &mut frame[tcp_start..],
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            header,
            &[],
            TransportChecksum::Software,
        )
        .expect("test TCP segment should fit");
        offset += Ipv4Packet::encode_header(
            &mut frame[offset..],
            source,
            destination,
            IpProtocol::Tcp,
            tcp_len,
            1,
            64,
        )
        .expect("test IPv4 header should fit");
        (frame, offset + tcp_len)
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

    fn test_stack_config() -> StackConfig {
        StackConfig::new([0x02, 0, 0, 0, 0, 1], ETHERNET_FRAME_BYTES).with_rx_budget(8)
    }

    /// The first datagram slot a replicated bind may take: the DHCP
    /// client owns everything below it on every shard.
    const FIRST_TEST_UDP_SLOT: usize = super::INTERNAL_UDP_RESERVED_SLOTS;
    /// Listeners reserve nothing, so their first slot is zero.
    const FIRST_TEST_LISTENER_SLOT: usize = 0;

    /// A received frame the device reported no hash for, which is what
    /// every backend without `HASH_REPORT` delivers.
    fn unhashed(bytes: &[u8]) -> RxFrame {
        RxFrame::new(Bytes::copy_from_slice(bytes))
    }

    fn test_network_shard() -> NetworkShard {
        NetworkShard::new(test_stack_config(), 1, 0, 1)
    }

    /// Installs one shard's replica of a bound datagram socket and
    /// returns the id that names it, the way `execute_udp_bind` does
    /// for the whole set.
    fn bind_udp(shard: &mut NetworkShard, slot: usize, local_port: u16) -> UdpSocketId {
        shard
            .install_udp_bind(slot, local_port)
            .expect("test UDP socket should bind");
        UdpSocketId(ReplicaHandle::new(slot).get())
    }

    /// The same for a listener.
    fn listen_tcp(
        shard: &mut NetworkShard,
        slot: usize,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: TcpListenBacklog,
    ) -> TcpListenerId {
        shard
            .install_tcp_listener(
                slot,
                local_address,
                local_port,
                backlog,
                helios_netstack::DEFAULT_HOP_LIMIT,
            )
            .expect("test TCP listener should bind");
        TcpListenerId(ReplicaHandle::new(slot).get())
    }

    /// A link that comes back is not the link that went away: the lease,
    /// the routes it pointed at and the resolvers that came with it are
    /// all stale, and nothing would ask the new link for an address
    /// unless the client is put back at the start.
    #[test]
    fn a_link_bounce_restarts_interface_configuration() {
        let mut shard = test_network_shard();
        shard
            .stack
            .add_ipv4_address(Ipv4Cidr::new(Ipv4Address::new([192, 0, 2, 10]), 24));
        shard
            .stack
            .routes_mut()
            .add(Route {
                destination: IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0)),
                gateway: Some(IpAddress::Ipv4(Ipv4Address::new([192, 0, 2, 1]))),
                expires_at: None,
            })
            .expect("test gateway should be accepted");
        shard.dhcp = DhcpClientState::Bound;
        shard
            .dns_servers
            .push(helios_netstack::Ipv4Address::new([192, 0, 2, 1]));

        shard.restart_link_configuration(9);

        assert_eq!(shard.dhcp, DhcpClientState::Init { transaction_id: 9 });
        assert_eq!(shard.stack.ipv4_addresses().count(), 0);
        assert!(shard.stack.routes().iter().next().is_none());
        assert!(shard.dns_servers.is_empty());

        // With the client back at Init the next configuration round
        // sends a fresh DISCOVER for the new link.
        shard
            .drive_dhcp(StackInstant::from_nanos(1))
            .expect("a restarted DHCP client should send DISCOVER");
        assert_eq!(
            shard.dhcp,
            DhcpClientState::Selecting {
                transaction_id: 9,
                last_sent: StackInstant::from_nanos(1),
            }
        );
        let frame = shard
            .stack
            .take_outbound()
            .expect("a DHCP DISCOVER frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let udp = UdpPacket::parse(ipv4.payload).expect("UDP datagram should parse");
        assert_eq!(udp.destination_port, helios_netstack::DHCP_SERVER_PORT);
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
                IpAddress::Ipv6(local),
                IpAddress::Ipv6(remote),
                443,
                49_152,
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

    /// The IPv6 half of the QEMU user-networking failure in issue #36:
    /// slirp answers an IPv6 SYN with a RST on an IPv6-less host. The
    /// resulting error has to classify as address-specific, or the
    /// connect walk would report it instead of trying the A record.
    #[test]
    fn refused_ipv6_connect_reports_an_address_specific_error() {
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
        let stream = state
            .start_tcp_connect(
                IpAddress::Ipv6(local),
                IpAddress::Ipv6(remote),
                443,
                49_152,
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
        let syn = TcpPacket::parse(ipv6.payload).expect("TCP packet should parse");

        let (reset, reset_len) = ipv6_tcp_frame(
            remote,
            local,
            TcpHeader {
                source_port: 443,
                destination_port: syn.source_port,
                sequence: 0,
                acknowledgement: syn.sequence.wrapping_add(1),
                flags: TcpFlags::RST.union(TcpFlags::ACK),
                window_size: 0,
            },
        );
        state
            .stack
            .receive_frame(&reset[..reset_len], StackInstant::from_nanos(2))
            .expect("IPv6 RST should be accepted");

        let error = state
            .poll_tcp_connect(stream)
            .expect_err("a refused connect should fail");
        assert_eq!(
            error.detail,
            crate::NetworkErrorDetail::TcpClosedDuringConnect
        );
        assert!(
            AddressAttemptError::is_address_specific(&error),
            "a refused candidate must fall through to the next resolved address"
        );
    }

    /// Builds the ICMPv4 echo reply a host would send back for
    /// `request`, so the test drives the same bytes the wire would.
    fn ipv4_echo_reply_frame(
        source: Ipv4Address,
        destination: Ipv4Address,
        identifier: u16,
        sequence: u16,
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
        let icmp_len = Icmpv4Packet::HEADER_LEN + payload.len();
        offset += Ipv4Packet::encode_header(
            &mut frame[offset..],
            source,
            destination,
            IpProtocol::Icmp,
            icmp_len,
            1,
            64,
        )
        .expect("test IPv4 header should fit");
        // RFC 792 echo reply: type 0, code 0, then the identifier,
        // sequence and echoed data the request carried.
        let icmp = &mut frame[offset..offset + icmp_len];
        icmp.fill(0);
        icmp[4..6].copy_from_slice(&identifier.to_be_bytes());
        icmp[6..8].copy_from_slice(&sequence.to_be_bytes());
        icmp[Icmpv4Packet::HEADER_LEN..].copy_from_slice(payload);
        let checksum = internet_checksum(icmp);
        icmp[2..4].copy_from_slice(&checksum.to_be_bytes());
        (frame, offset + icmp_len)
    }

    /// The core of issue #38: a ping has to put an echo request on the
    /// wire, and the reply that answers it has to be claimable by the
    /// identifier and sequence the request carried.
    #[test]
    fn icmp_echo_request_is_transmitted_and_its_reply_is_claimed() {
        let local = Ipv4Address::new([10, 0, 2, 15]);
        let remote = Ipv4Address::new([10, 0, 2, 2]);
        let mut state = test_network_shard();
        state.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        state.stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(remote),
            mac: [0x02, 0, 0, 0, 0, 2],
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let key = IcmpEchoKey {
            destination: IpAddress::Ipv4(remote),
            identifier: state.next_icmp_echo_identifier(),
            sequence: 1,
        };
        let payload = icmp_echo_payload();

        assert!(
            state
                .send_icmp_echo_request(key, &payload, StackInstant::from_nanos(1))
                .expect("the echo request should queue"),
            "a resolved neighbour puts the request on the wire immediately"
        );

        let frame = state
            .stack
            .take_outbound()
            .expect("an echo request frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        assert_eq!(ipv4.source, local);
        assert_eq!(ipv4.destination, remote);
        assert_eq!(ipv4.protocol, IpProtocol::Icmp);
        let request = match Icmpv4Packet::parse(ipv4.payload) {
            Some(Icmpv4Packet::EchoRequest(echo)) => echo,
            other => panic!("expected an ICMPv4 echo request, got {other:?}"),
        };
        assert_eq!(request.identifier, key.identifier);
        assert_eq!(request.sequence, key.sequence);
        assert_eq!(request.payload, payload.as_slice());

        assert!(
            state.take_icmp_echo_reply(key).is_none(),
            "no reply has arrived yet"
        );

        let (reply_frame, reply_len) =
            ipv4_echo_reply_frame(remote, local, key.identifier, key.sequence, request.payload);
        state
            .stack
            .receive_frame(&reply_frame[..reply_len], StackInstant::from_nanos(2))
            .expect("the echo reply should be accepted");

        let reply = state
            .take_icmp_echo_reply(key)
            .expect("the matching echo reply should be claimable");
        assert_eq!(reply.key, key);
        assert_eq!(usize::from(reply.payload_bytes), payload.len());
        assert!(
            state.take_icmp_echo_reply(key).is_none(),
            "a claimed reply is consumed"
        );
    }

    /// A reply whose identifier or sequence belongs to some other
    /// exchange must not satisfy this one, or a stale answer would be
    /// reported as this ping's round-trip time.
    #[test]
    fn icmp_echo_reply_with_another_key_is_not_claimed() {
        let local = Ipv4Address::new([10, 0, 2, 15]);
        let remote = Ipv4Address::new([10, 0, 2, 2]);
        let mut state = test_network_shard();
        state.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        state.stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(remote),
            mac: [0x02, 0, 0, 0, 0, 2],
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let key = IcmpEchoKey {
            destination: IpAddress::Ipv4(remote),
            identifier: 0x4242,
            sequence: 1,
        };
        let payload = icmp_echo_payload();
        state
            .send_icmp_echo_request(key, &payload, StackInstant::from_nanos(1))
            .expect("the echo request should queue");
        let _ = state.stack.take_outbound();

        for (identifier, sequence) in [
            (key.identifier.wrapping_add(1), key.sequence),
            (key.identifier, key.sequence.wrapping_add(1)),
        ] {
            let (frame, len) = ipv4_echo_reply_frame(remote, local, identifier, sequence, &payload);
            state
                .stack
                .receive_frame(&frame[..len], StackInstant::from_nanos(2))
                .expect("the echo reply should be accepted");
        }

        assert!(
            state.take_icmp_echo_reply(key).is_none(),
            "a reply for another exchange must not answer this one"
        );
    }

    /// IPv6 targets take the ICMPv6 path with an IPv6 source address,
    /// which is what makes a AAAA-only host pingable at all.
    #[test]
    fn icmpv6_echo_request_uses_the_ipv6_source_address() {
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
        let key = IcmpEchoKey {
            destination: IpAddress::Ipv6(remote),
            identifier: 7,
            sequence: 3,
        };
        let payload = icmp_echo_payload();

        assert!(
            state
                .send_icmp_echo_request(key, &payload, StackInstant::from_nanos(1))
                .expect("the ICMPv6 echo request should queue")
        );

        let frame = state
            .stack
            .take_outbound()
            .expect("an ICMPv6 echo request frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        assert_eq!(ethernet.protocol, EthernetProtocol::Ipv6);
        let ipv6 = Ipv6Packet::parse(ethernet.payload).expect("IPv6 packet should parse");
        assert_eq!(ipv6.source, local);
        assert_eq!(ipv6.destination, remote);
        let request = match Icmpv6Packet::parse(ipv6.payload) {
            Some(Icmpv6Packet::EchoRequest(echo)) => echo,
            other => panic!("expected an ICMPv6 echo request, got {other:?}"),
        };
        assert_eq!(request.identifier, key.identifier);
        assert_eq!(request.sequence, key.sequence);
        assert_eq!(request.payload, payload.as_slice());
    }

    /// An unresolved next hop sends an ARP request instead, and the
    /// caller learns nothing went out so it can retry rather than start
    /// timing a round trip that never began.
    #[test]
    fn icmp_echo_request_waits_for_the_neighbour_to_answer() {
        let local = Ipv4Address::new([10, 0, 2, 15]);
        let remote = Ipv4Address::new([10, 0, 2, 2]);
        let mut state = test_network_shard();
        state.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let key = IcmpEchoKey {
            destination: IpAddress::Ipv4(remote),
            identifier: 9,
            sequence: 1,
        };

        assert!(
            !state
                .send_icmp_echo_request(key, &icmp_echo_payload(), StackInstant::from_nanos(1))
                .expect("an unresolved neighbour is not an error"),
            "nothing is on the wire until ARP resolves the next hop"
        );
        let frame = state
            .stack
            .take_outbound()
            .expect("an ARP request should be queued in its place");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        assert_eq!(ethernet.protocol, EthernetProtocol::Arp);
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
        let socket = bind_udp(&mut state, FIRST_TEST_UDP_SLOT, 4040);

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
        let socket = bind_udp(&mut state, FIRST_TEST_UDP_SLOT, 4040);
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
        let socket = bind_udp(&mut state, FIRST_TEST_UDP_SLOT, 4040);
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
        let socket = bind_udp(&mut state, FIRST_TEST_UDP_SLOT, 4040);
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
        let socket = bind_udp(&mut state, FIRST_TEST_UDP_SLOT, 4040);
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
        let socket = bind_udp(&mut state, FIRST_TEST_UDP_SLOT, 4040);
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
        let first = bind_udp(&mut state, FIRST_TEST_UDP_SLOT, 4040);
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
        let error = state
            .disconnect_udp_socket(first)
            .expect_err("disconnect should reject ambiguous wildcard binding");
        assert_eq!(error.kind, crate::UdpErrorKind::Unavailable);
        assert_eq!(error.detail, crate::NetworkErrorDetail::UdpPortInUse);
        state
            .stack
            .remove_udp_socket(second_stack_socket)
            .expect("the second connected socket should close");
    }

    /// The regression #31 describes: a reply for a socket on shard 1
    /// drained by the pump running on processor 0.
    ///
    /// Before the arrival signal existed nothing told shard 1 that its
    /// datagram had landed, and the waiting operation slept on the
    /// device event until its own deadline expired — which is what made
    /// `--smp 4` DNS lookups time out. The assertions here are that the
    /// wait ends *by notification*: it is pending before the foreign
    /// drain and ready after it, with no timer in the test at all, and
    /// the owning processor is sent an IPI because it is not the one
    /// that drained.
    #[test]
    fn reply_drained_on_a_foreign_processor_wakes_the_owning_shard() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        // Two shards, and this fixture is processor 0 — the shard that
        // owns port 49_153 is shard 1, so every hand-off below is
        // cross-processor.
        let cpu = RecordingSmpCpu::new(0, 2);
        let control = super::NetworkControlPlane::new();
        let state = super::NetworkShardSet::new(2, |index| {
            NetworkShard::new(test_stack_config(), 1 + index as u32, index, 2)
        });
        // A bound datagram socket is replicated: the same slot on every
        // shard, with the receive path deciding which replica gets a
        // given datagram.
        let socket = UdpSocketId(ReplicaHandle::new(FIRST_TEST_UDP_SLOT).get());
        state
            .install_replica(
                FIRST_TEST_UDP_SLOT,
                |shard, slot| {
                    shard.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
                    shard.install_udp_bind(slot, 49_153)
                },
                NetworkShard::remove_udp_replica,
            )
            .expect("the bind should install on every shard");
        // The flow decides the shard, so the peer port is chosen to
        // make this a genuinely cross-processor delivery: the fixture
        // drains as processor 0 and the socket's replica on shard 1 is
        // the one that must be released.
        let owner = 1;
        let peer_port = (1024..u16::MAX)
            .find(|peer_port| {
                super::shard_idx_for_flow(
                    IpAddress::Ipv4(local),
                    49_153,
                    IpAddress::Ipv4(peer),
                    *peer_port,
                    state.shard_count(),
                ) == owner
            })
            .expect("some peer port hashes to the foreign shard");

        // The operation's park, taken the way every per-operation wait
        // takes it: mark first, then inspect.
        let wait = state.shard_wait(owner);
        assert!(
            state
                .shard_at(owner)
                .lock()
                .poll_udp_receive(socket, usize::MAX)
                .expect("bound UDP socket should poll")
                .is_none(),
            "nothing has arrived yet"
        );
        let mut parked = core::pin::pin!(state.arrival(owner).changed(wait.mark));
        assert!(
            block_on(poll_once(parked.as_mut())).is_none(),
            "the wait must park while the reply is still on the wire"
        );

        // Processor 0 drains the reply and demuxes it, exactly as the
        // packet pump does.
        let (reply, reply_len) = ipv4_udp_frame(peer, peer_port, local, 49_153, b"reply");
        let mut arrivals = super::ShardArrivals::new();
        match state.dispatch_rx_frame(
            &RxFrame::new(Bytes::copy_from_slice(&reply[..reply_len])),
            StackInstant::from_nanos(1),
            &control,
        ) {
            super::RxFrameDispatch::Delivered {
                shard_idx,
                backpressured,
            } => {
                assert_eq!(shard_idx, owner, "the reply belongs to the socket's shard");
                assert!(!backpressured);
                arrivals.record(shard_idx);
            }
            _ => panic!("the owning shard should have taken the reply"),
        }
        state.notify_arrivals(&arrivals, &cpu);

        assert!(
            block_on(poll_once(parked)).is_some(),
            "the foreign drain must release the wait, not its deadline"
        );
        assert_eq!(
            cpu.woken(),
            alloc::vec![helios_hal::cpu::ProcessorId::new(1)],
            "the owning processor must be pulled out of its idle park"
        );
        assert!(
            state
                .shard_at(owner)
                .lock()
                .poll_udp_receive(socket, usize::MAX)
                .expect("bound UDP socket should poll")
                .is_some(),
            "the released operation must find its datagram"
        );
    }

    /// The receive rule and the placement rule have to agree, or a
    /// socket is opened on a shard its own frames never reach.
    #[test]
    fn a_frame_lands_on_the_shard_its_flow_was_placed_on() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([198, 51, 100, 20]);
        for shard_count in [1usize, 2, 3, 4, 8] {
            for local_port in [49_152u16, 50_000, 60_123] {
                for peer_port in [53u16, 443, 8080] {
                    let placed = super::shard_idx_for_flow(
                        IpAddress::Ipv4(local),
                        local_port,
                        IpAddress::Ipv4(peer),
                        peer_port,
                        shard_count,
                    );
                    let (frame, len) =
                        ipv4_udp_frame(peer, peer_port, local, local_port, b"payload");
                    assert_eq!(
                        super::shard_idx_for_frame(&unhashed(&frame[..len]), shard_count),
                        placed,
                        "the demux and the placement rule must agree"
                    );
                    assert!(placed < shard_count);
                }
            }
        }
    }

    /// A device that reports its hash and one that does not must reach
    /// the same shard for the same frame, or a flow would move when the
    /// backend changes.
    #[test]
    fn a_reported_hash_and_a_computed_one_agree() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([198, 51, 100, 20]);
        for shard_count in [1usize, 2, 3, 4, 8] {
            for peer_port in [53u16, 443, 8080, 40_000] {
                let (frame, len) = ipv4_udp_frame(peer, peer_port, local, 49_153, b"payload");
                let computed = super::shard_idx_for_frame(&unhashed(&frame[..len]), shard_count);

                // What the device would have written into the receive
                // header: the same function over the same bytes.
                let tuple = helios_netstack::FlowTuple::ipv4(peer, peer_port, local, 49_153);
                let reported = RxFrame::with_offload(
                    Bytes::copy_from_slice(&frame[..len]),
                    helios_netstack::RxFrameOffload {
                        flow_hash: Some(helios_netstack::flow_hash(&tuple)),
                        ..helios_netstack::RxFrameOffload::none()
                    },
                );
                assert_eq!(
                    super::shard_idx_for_frame(&reported, shard_count),
                    computed,
                    "a steered frame and a software-hashed one belong to the same shard"
                );
            }
        }
    }

    /// A DHCP exchange has no address to hash a flow with, so it is
    /// owned by the default shard however its ports would hash.
    #[test]
    fn the_dhcp_exchange_stays_on_the_default_shard() {
        let server = Ipv4Address::new([192, 0, 2, 1]);
        let client = Ipv4Address::new([192, 0, 2, 10]);
        let broadcast = Ipv4Address::new([255, 255, 255, 255]);
        for shard_count in [1usize, 2, 4, 8] {
            let (offer, offer_len) = ipv4_udp_frame(
                server,
                helios_netstack::DHCP_SERVER_PORT,
                client,
                helios_netstack::DHCP_CLIENT_PORT,
                b"offer",
            );
            assert_eq!(
                super::shard_idx_for_frame(&unhashed(&offer[..offer_len]), shard_count),
                super::DEFAULT_SHARD_IDX
            );
            let (discover, discover_len) = ipv4_udp_frame(
                Ipv4Address::UNSPECIFIED,
                helios_netstack::DHCP_CLIENT_PORT,
                broadcast,
                helios_netstack::DHCP_SERVER_PORT,
                b"discover",
            );
            assert_eq!(
                super::shard_idx_for_frame(&unhashed(&discover[..discover_len]), shard_count),
                super::DEFAULT_SHARD_IDX
            );
        }
    }

    /// An ICMP error quotes the packet that provoked it, so it belongs
    /// to that flow's shard — the one with a socket to tell — and not
    /// to the default shard it would fall back to.
    #[test]
    fn an_icmp_error_follows_the_flow_it_quotes() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let peer = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let router = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
        let shard_count = 4;
        let (sent, sent_len) = ipv6_udp_frame(local, 49_153, peer, 53, b"query");
        let (error, error_len) = ipv6_packet_too_big_frame(router, local, &sent[..sent_len], 1280);

        let flow = super::shard_idx_for_flow(
            IpAddress::Ipv6(local),
            49_153,
            IpAddress::Ipv6(peer),
            53,
            shard_count,
        );
        assert_eq!(
            super::shard_idx_for_frame(&unhashed(&error[..error_len]), shard_count),
            flow
        );
    }

    /// Frames with no flow at all fall to the default shard, which is
    /// where the control plane that handles them runs.
    #[test]
    fn a_frame_without_a_flow_falls_to_the_default_shard() {
        for shard_count in [1usize, 2, 4] {
            assert_eq!(
                super::shard_idx_for_frame(&unhashed(&[]), shard_count),
                super::DEFAULT_SHARD_IDX,
                "an unparseable frame has no flow"
            );
            let mut arp = [0u8; ETHERNET_FRAME_BYTES];
            let len = EthernetFrame::encode_header(
                &mut arp,
                [0x02, 0, 0, 0, 0, 1],
                [0xff; 6],
                EthernetProtocol::Arp,
            )
            .expect("test Ethernet header should fit");
            assert_eq!(
                super::shard_idx_for_frame(&unhashed(&arp[..len]), shard_count),
                super::DEFAULT_SHARD_IDX,
                "ARP carries no ports"
            );
        }
    }

    /// Each shard walks its own contiguous slice of the ephemeral
    /// range, so two shards never hand out the same port at once and
    /// every port belongs to exactly one window.
    #[test]
    fn ephemeral_windows_partition_the_range() {
        for shard_count in [1usize, 2, 3, 4, 8] {
            let mut previous_end = None;
            for shard_idx in 0..shard_count {
                let shard = NetworkShard::new(test_stack_config(), 1, shard_idx, shard_count);
                let (first, last) = shard.ephemeral_window();
                assert!(first <= last);
                match previous_end {
                    None => assert_eq!(first, super::EPHEMERAL_PORT_START),
                    Some(previous) => assert_eq!(first, previous + 1),
                }
                previous_end = Some(last);
                assert_eq!(
                    shard.advance_ephemeral_port(last),
                    first,
                    "the walk wraps inside its own window"
                );
                assert_eq!(
                    shard.ephemeral_port_attempts(),
                    usize::from(last - first) + 1
                );
            }
            assert_eq!(previous_end, Some(super::EPHEMERAL_PORT_END));
        }
    }

    /// A listener exists on every shard, and `accept` starts at the
    /// caller's own shard and then visits the rest, so a connection the
    /// receive path placed on a foreign shard is never stranded.
    #[test]
    fn accept_starts_at_the_callers_shard_and_visits_the_rest() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([198, 51, 100, 20]);
        let state = super::NetworkShardSet::new(3, |index| {
            NetworkShard::new(test_stack_config(), 1 + index as u32, index, 3)
        });
        let listener = TcpListenerId(ReplicaHandle::new(FIRST_TEST_LISTENER_SLOT).get());
        state
            .install_replica(
                FIRST_TEST_LISTENER_SLOT,
                |shard, slot| {
                    shard.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
                    shard.stack.learn_neighbor(NeighborEntry {
                        ip: IpAddress::Ipv4(peer),
                        mac: [0x02, 0, 0, 0, 0, 2],
                        state: NeighborState::Reachable,
                        updated_at: StackInstant::from_nanos(0),
                    });
                    shard.install_tcp_listener(
                        slot,
                        NetworkIpAddress::Ipv4(map_ipv4_address(local)),
                        8080,
                        TcpListenBacklog::new(2),
                        helios_netstack::DEFAULT_HOP_LIMIT,
                    )
                },
                NetworkShard::remove_tcp_listener,
            )
            .expect("the listener should install on every shard");

        // A handshake delivered to shard 2 only: that is the replica
        // whose accept queue fills.
        let (syn, syn_len) = ipv4_tcp_frame(
            peer,
            local,
            TcpHeader {
                source_port: 40_000,
                destination_port: 8080,
                sequence: 10,
                acknowledgement: 0,
                flags: TcpFlags::SYN,
                window_size: u16::MAX,
            },
        );
        let syn_ack_sequence = {
            let mut shard = state.shard_at(2).lock();
            shard
                .stack
                .receive_frame(&syn[..syn_len], StackInstant::from_nanos(1))
                .expect("the foreign replica should take the SYN");
            shard
                .stack
                .drive_tcp(StackInstant::from_nanos(1))
                .expect("the replica should queue a SYN-ACK");
            let frame = shard
                .stack
                .take_outbound()
                .expect("the replica should queue a SYN-ACK");
            let ethernet =
                EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
            let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
            let syn_ack = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
            assert!(syn_ack.flags.contains(TcpFlags::SYN.union(TcpFlags::ACK)));
            syn_ack.sequence
        };
        let (ack, ack_len) = ipv4_tcp_frame(
            peer,
            local,
            TcpHeader {
                source_port: 40_000,
                destination_port: 8080,
                sequence: 11,
                acknowledgement: syn_ack_sequence.wrapping_add(1),
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
        );
        state
            .shard_at(2)
            .lock()
            .stack
            .receive_frame(&ack[..ack_len], StackInstant::from_nanos(2))
            .expect("the final ACK should establish the accepted socket");

        // Starting the walk on an empty shard still finds it, and the
        // accepted stream belongs to the shard that produced it.
        let accepted = state
            .find_in_replicas(0, |shard| shard.poll_tcp_accept(listener))
            .expect("the accept walk should not fail")
            .expect("a connection queued on any replica is acceptable");
        assert_eq!(
            state.shard_idx_for_handle(accepted.stream),
            2,
            "an accepted stream belongs to the shard whose replica took the SYN"
        );
        assert_eq!(accepted.port, 40_000);

        // Nothing is left over: the connection was taken exactly once.
        assert!(
            state
                .find_in_replicas(0, |shard| shard.poll_tcp_accept(listener))
                .expect("the accept walk should not fail")
                .is_none()
        );
    }

    /// A handle carries its owner, so an operation routes to the shard
    /// that minted it without anybody re-deriving where the socket
    /// "should" have been placed.
    #[test]
    fn a_handle_names_the_shard_that_minted_it() {
        use super::ShardHandle;

        for owner in [0usize, 1, 7, 4095] {
            for slot in [0usize, 1, 255, 4095] {
                let handle = ShardHandle::new(owner, slot);
                assert_eq!(handle.owner(), owner);
                assert_eq!(handle.slot(), slot);
                assert_eq!(ShardHandle::from_raw(handle.get()), handle);
            }
        }
    }

    /// Slot 0 of shard 0 has to be a usable handle, and no handle may
    /// be zero — the public ids are `NonZeroU32`.
    #[test]
    fn the_first_slot_of_the_first_shard_is_a_valid_handle() {
        let handle = super::ShardHandle::new(0, 0);
        assert_eq!(handle.get().get(), 1);
        assert_eq!(handle.owner(), 0);
        assert_eq!(handle.slot(), 0);
    }

    #[test]
    #[should_panic(expected = "carries no slab slot")]
    fn a_raw_handle_without_a_slot_is_rejected() {
        let raw = core::num::NonZeroU32::new(1 << 16).expect("owner-only handle is non-zero");
        let _ = super::ShardHandle::from_raw(raw);
    }

    /// A batch that lands several frames in the same shard releases it
    /// once, and a shard that took nothing is never signalled.
    #[test]
    fn shard_arrivals_deduplicate_within_one_receive_batch() {
        let cpu = RecordingSmpCpu::new(0, 4);
        let state = super::NetworkShardSet::new(4, |index| {
            NetworkShard::new(test_stack_config(), 1 + index as u32, index, 4)
        });
        let mut arrivals = super::ShardArrivals::new();
        arrivals.record(2);
        arrivals.record(2);
        arrivals.record(0);

        assert_eq!(arrivals.iter().collect::<Vec<_>>(), alloc::vec![2, 0]);

        let marks: Vec<_> = (0..4).map(|idx| (idx, state.shard_wait(idx))).collect();
        state.notify_arrivals(&arrivals, &cpu);

        for (shard_idx, wait) in marks {
            let touched = arrivals.iter().any(|idx| idx == shard_idx);
            let mut parked = core::pin::pin!(state.arrival(shard_idx).changed(wait.mark));
            assert_eq!(
                block_on(poll_once(parked.as_mut())).is_some(),
                touched,
                "only a shard that took a frame is released"
            );
        }
        // Shard 0 is this processor's own, so only shard 2 costs an IPI.
        assert_eq!(
            cpu.woken(),
            alloc::vec![helios_hal::cpu::ProcessorId::new(2)]
        );
    }

    /// A replicated bind exists on every shard, and an ICMPv6 Packet
    /// Too Big teaches the path MTU to the replica that receives it —
    /// which is the replica that will send on that path again.
    #[test]
    fn a_packet_too_big_caps_the_replica_that_received_it() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let peer = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let router = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
        let state = super::NetworkShardSet::new(2, |index| {
            NetworkShard::new(test_stack_config(), 1 + index as u32, index, 2)
        });
        let socket = UdpSocketId(ReplicaHandle::new(FIRST_TEST_UDP_SLOT).get());
        state
            .install_replica(
                FIRST_TEST_UDP_SLOT,
                |shard, slot| {
                    shard.stack.add_ipv6_address(Ipv6Cidr::new(local, 64));
                    shard.stack.learn_neighbor(NeighborEntry {
                        ip: IpAddress::Ipv6(peer),
                        mac: [0x02, 0, 0, 0, 0, 2],
                        state: NeighborState::Reachable,
                        updated_at: StackInstant::from_nanos(0),
                    });
                    shard.install_udp_bind(slot, 49_153)
                },
                NetworkShard::remove_udp_replica,
            )
            .expect("the bind should install on every shard");

        let sender = super::shard_idx_for_flow(
            IpAddress::Ipv6(local),
            49_153,
            IpAddress::Ipv6(peer),
            53,
            state.shard_count(),
        );
        let payload = [0u8; 1233];
        let (packet_too_big, packet_too_big_len) = {
            let mut shard = state.shard_at(sender).lock();
            let written = shard
                .try_send_udp(
                    socket,
                    IpAddress::Ipv6(peer),
                    53,
                    &payload,
                    StackInstant::from_nanos(1),
                )
                .expect("the replica should queue the initial IPv6 UDP datagram");
            assert_eq!(written, payload.len());
            let quoted = shard
                .stack
                .take_outbound()
                .expect("the replica should queue the initial IPv6 UDP datagram");
            ipv6_packet_too_big_frame(router, local, quoted.as_slice(), 1280)
        };

        state
            .shard_at(sender)
            .lock()
            .stack
            .receive_frame(
                &packet_too_big[..packet_too_big_len],
                StackInstant::from_nanos(2),
            )
            .expect("ICMPv6 Packet Too Big should update the replica's PMTU");

        let error = state
            .shard_at(sender)
            .lock()
            .try_send_udp(
                socket,
                IpAddress::Ipv6(peer),
                53,
                &payload,
                StackInstant::from_nanos(3),
            )
            .expect_err("the learned PMTU should cap later UDP sends");
        assert_eq!(error.detail, crate::NetworkErrorDetail::UdpDatagramTooLarge);
    }

    #[test]
    fn udp_multicast_join_and_leave_update_stack_delivery() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let mut state = test_network_shard();
        state.stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = bind_udp(&mut state, FIRST_TEST_UDP_SLOT, 4040);
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
        let listener = listen_tcp(
            &mut state,
            FIRST_TEST_LISTENER_SLOT,
            NetworkIpAddress::Ipv6(local),
            8080,
            TcpListenBacklog::new(1),
        );

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
            .poll_tcp_accept(listener)
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
