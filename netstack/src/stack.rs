extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;
use core::mem::MaybeUninit;

use arrayvec::ArrayVec;
use bytes::Bytes;
use heapless::Deque;
use heapless::binary_heap::{BinaryHeap, Min};

use crate::{
    ArpOperation, ArpPacket, BbrV3, CongestionControl, DEFAULT_POLL_BUDGET, DhcpDnsServers,
    EthernetAddress, EthernetFrame, EthernetProtocol, Icmpv4DestinationUnreachableCode,
    Icmpv4Packet, Icmpv6DestinationUnreachableCode, Icmpv6Packet, IpAddress, IpCidr, Ipv4Address,
    Ipv4Cidr, Ipv4Packet, Ipv6Address, Ipv6Cidr, Ipv6DnsServers, Ipv6Packet,
    Ipv6RouterConfiguration, Ipv6Scope, NeighborDiscovery, PacketBuffer, RxChecksumReport, RxFrame,
    RxFrameOffload, SegmentationOffload, StackError, TcpEndpoint, TcpFlags, TcpHeader,
    TcpHeaderOptions, TcpPacket, TcpReceiveBuffer, TcpSegmentBudget, TcpSocket, TcpTransmitSegment,
    TransportChecksum, TxChecksum, TxFrameRef, TxSegmentation, UdpPacket, icmpv6_checksum_valid,
    interpret_router_advertisement, ipv4_checksum, partial_transport_checksum_completes,
    tcp_checksum_valid, udp_checksum_valid,
};

pub const MAX_ROUTES: usize = 32;
pub const MAX_NEIGHBORS: usize = 128;
pub const MAX_OUTBOUND_FRAMES: usize = 32;
pub const MAX_STACK_EVENTS: usize = 64;
pub const MAX_UDP_RX: usize = 64;
/// Echo replies retained until a waiter claims them.
///
/// A ping has one request outstanding at a time, so this only has to
/// absorb replies that raced past their waiter's deadline plus any
/// unsolicited ones the link delivers. The oldest is evicted once the
/// buffer fills.
pub const MAX_ICMP_ECHO_REPLIES: usize = 8;
pub const MAX_UDP_SOCKETS: usize = 256;
pub const MAX_IPV4_MULTICAST_MEMBERSHIPS: usize = 64;
pub const MAX_PATH_MTU_ENTRIES: usize = 128;
pub const MAX_IPV4_REASSEMBLY_ENTRIES: usize = 32;
pub const MAX_IPV4_REASSEMBLY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_IPV4_REASSEMBLY_FRAGMENTS: usize = 8192;
const IPV4_MINIMUM_LINK_MTU: usize = 68;
const IPV6_MINIMUM_LINK_MTU: usize = 1280;
const MINIMUM_INTERFACE_FRAME_MTU: usize = 14 + IPV4_MINIMUM_LINK_MTU;
const IPV4_MAX_HEADER_LEN: usize = 60;
const IPV4_FRAGMENT_BLOCK_SIZE: usize = 8;
const IPV4_MAX_FRAGMENT_BLOCKS: u16 = 8191;
const IPV4_MAX_REASSEMBLED_PACKET_LEN: usize = u16::MAX as usize;
const IPV4_MAX_REASSEMBLED_PAYLOAD_LEN: usize =
    IPV4_MAX_REASSEMBLED_PACKET_LEN - Ipv4Packet::MIN_HEADER_LEN;
const IPV4_REASSEMBLY_TIMEOUT_NANOS: u64 = 30_000_000_000;
// Keep common small application datagrams out of the heap without moving a
// large inline block through the bounded receive queue.
// The previous path allocated one `Bytes` object per received UDP packet;
// divan showed `udp_receive_and_take/64` spending 64 allocations / 2.112 KiB.
// After this inline payload path, the same benchmark has zero measured
// allocations and moved 8.567 us -> 8.249 us; the mixed-port tail case moved
// 19.31 us -> 8.416 us because rotation no longer moves ref-counted payloads.
const UDP_INLINE_PAYLOAD_BYTES: usize = 128;
pub const MAX_TCP_SOCKETS: usize = 256;
pub const MAX_TCP_ACCEPT: usize = MAX_TCP_SOCKETS;
pub const DEFAULT_TCP_LISTEN_BACKLOG: TcpListenBacklog = TcpListenBacklog::new(128);
// Allocate TCP sockets in small slab chunks rather than one heap object per
// socket. Local divan `tcp_listen_many/64` moved from 64 allocations and
// 8.347 us median to 8 allocations and 8.033 us; chunk=4 measured worse at
// 16 allocations and 8.441 us.
const TCP_SOCKET_SLAB_CHUNK_SLOTS: usize = 8;
const TCP_SOCKET_SLAB_CHUNKS: usize = MAX_TCP_SOCKETS / TCP_SOCKET_SLAB_CHUNK_SLOTS;
// Keep the first chunk inline so the common first TCP listen/connect path does
// not allocate. Local divan moved `tcp_first_listen` from 8.868 us with one
// 3.776 KiB allocation to 5.915 us with no measured allocation; `tcp_listen_many/64`
// dropped one chunk allocation as well.
const TCP_SOCKET_HEAP_SLAB_CHUNKS: usize = TCP_SOCKET_SLAB_CHUNKS - 1;
const TCP_ENDPOINT_INDEX_SLOTS: usize = MAX_TCP_SOCKETS * 2;
const TCP_LISTENER_INDEX_SLOTS: usize = MAX_TCP_SOCKETS * 2;
const UDP_BINDING_INDEX_SLOTS: usize = MAX_UDP_SOCKETS * 2;
const MAX_TCP_TIMER_ENTRIES: usize = MAX_TCP_SOCKETS * 4;
const _: () = assert!(MAX_TCP_SOCKETS.is_multiple_of(TCP_SOCKET_SLAB_CHUNK_SLOTS));

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct StackInstant {
    nanos: u64,
}

impl StackInstant {
    pub const fn from_nanos(nanos: u64) -> Self {
        Self { nanos }
    }

    pub const fn nanos(self) -> u64 {
        self.nanos
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackConfig {
    pub mac: EthernetAddress,
    pub mtu: usize,
    pub rx_budget: usize,
    pub rx_checksum_offload: RxChecksumOffload,
    /// Seed TCP/UDP transmit checksums with the pseudo-header sum and
    /// let the device finish them; enabled by the embedding kernel only
    /// when the interface negotiated transmit checksum offload.
    pub tx_checksum_offload: bool,
    /// The interface's DMA can read borrowed packet bytes, so bulk TCP
    /// data leaves as a scatter frame: headers in the packet buffer,
    /// payload attached as the refcounted bytes the socket already
    /// holds, and the device reading them in place until it completes
    /// the descriptor.
    ///
    /// This is a device property the embedding kernel copies out of
    /// [`InterfaceCapabilities::direct_tx_dma`](crate::InterfaceCapabilities),
    /// not a policy knob: a device that can read borrowed bytes has no
    /// reason to be handed a copy. Scatter framing also needs
    /// `tx_checksum_offload`, because the device finishes the transport
    /// checksum over bytes the stack never assembled.
    pub direct_tx_dma: bool,
    /// Segmentation offload the embedding kernel enabled for this
    /// interface.
    ///
    /// With a TCP segmentation family on, the send path coalesces the
    /// MSS-sized segments it would have emitted one at a time into a
    /// single oversized frame carrying
    /// [`TxSegmentation`](crate::TxSegmentation) metadata, and the
    /// device splits it back up. This is a framing decision taken at
    /// transmit time only: the socket's in-flight queue still holds one
    /// record per MSS-sized segment, so retransmission, SACK, and loss
    /// recovery address exactly the segments the peer sees.
    pub segmentation: SegmentationOffload,
}

/// Byte offset of the checksum field inside a TCP header.
const TCP_CHECKSUM_FIELD_OFFSET: u16 = 16;
/// Minimum payload for the scatter transmit path; smaller segments copy
/// faster than the extra descriptor costs.
const TCP_SCATTER_MIN_PAYLOAD_BYTES: usize = 512;

/// The transport-layer half of an outgoing UDP datagram: everything
/// about it that does not depend on the address family, so that one
/// value travels from the send entry point down to the encoder.
#[derive(Clone, Copy)]
pub struct UdpEgress<'a> {
    pub source_port: u16,
    pub destination_port: u16,
    pub payload: &'a [u8],
    pub hop_limit: u8,
}

/// The transport-layer half of an outgoing TCP segment, the TCP
/// counterpart of [`UdpEgress`].
#[derive(Clone, Copy)]
struct TcpEgress<'a> {
    header: TcpHeader,
    options: TcpHeaderOptions,
    payload: TcpTxPayload<'a>,
    hop_limit: u8,
}

/// The link- and network-layer addresses one outgoing frame is stamped
/// with. The two are always chosen together — the next hop's MAC is
/// resolved from the network destination — so they travel together.
#[derive(Clone, Copy)]
struct FramePath<A> {
    destination_mac: EthernetAddress,
    local_mac: EthernetAddress,
    source: A,
    destination: A,
}

/// How a queued TCP segment carries its payload to the frame encoders.
#[derive(Clone, Copy)]
enum TcpTxPayload<'a> {
    /// Payload bytes are copied into the packet buffer.
    Flat(&'a [u8]),
    /// Payload stays in its refcounted buffer and is attached to the
    /// frame for in-place device reads.
    Scatter(&'a Bytes),
    /// Several MSS-sized wire segments coalesced into one buffer for
    /// the device to split. Always scatter: an oversized burst does not
    /// fit a packet buffer.
    Segmented {
        payload: &'a Bytes,
        segment_bytes: u16,
    },
}

impl TcpTxPayload<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Flat(bytes) => bytes.len(),
            Self::Scatter(bytes) => bytes.len(),
            Self::Segmented { payload, .. } => payload.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Payload bytes of the largest packet this frame puts on the wire.
    /// A coalesced burst is checked against the path MTU per wire
    /// segment, because that is what the device emits.
    fn wire_segment_len(&self) -> usize {
        match self {
            Self::Segmented { segment_bytes, .. } => usize::from(*segment_bytes),
            other => other.len(),
        }
    }

    /// The refcounted payload a scatter-capable transmit path attaches
    /// to the frame.
    fn scatter_bytes(&self) -> Option<&Bytes> {
        match self {
            Self::Flat(_) => None,
            Self::Scatter(payload) | Self::Segmented { payload, .. } => Some(payload),
        }
    }
}
/// Byte offset of the checksum field inside a UDP header.
const UDP_CHECKSUM_FIELD_OFFSET: u16 = 6;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxChecksumOffload {
    pub ipv4: bool,
    pub tcp: bool,
    pub udp: bool,
}

impl StackConfig {
    pub const fn new(mac: EthernetAddress, mtu: usize) -> Self {
        assert!(
            mtu >= MINIMUM_INTERFACE_FRAME_MTU,
            "MTU must hold a minimum IPv4 packet"
        );
        Self {
            mac,
            mtu,
            rx_budget: DEFAULT_POLL_BUDGET,
            rx_checksum_offload: RxChecksumOffload::none(),
            tx_checksum_offload: false,
            direct_tx_dma: false,
            segmentation: SegmentationOffload::none(),
        }
    }

    pub const fn with_rx_budget(mut self, rx_budget: usize) -> Self {
        self.rx_budget = rx_budget;
        self
    }

    pub const fn with_tx_checksum_offload(mut self, enabled: bool) -> Self {
        self.tx_checksum_offload = enabled;
        self
    }

    pub const fn with_direct_tx_dma(mut self, enabled: bool) -> Self {
        self.direct_tx_dma = enabled;
        self
    }

    pub const fn with_segmentation_offload(mut self, segmentation: SegmentationOffload) -> Self {
        self.segmentation = segmentation;
        self
    }

    pub const fn with_rx_checksum_offload(mut self, offload: RxChecksumOffload) -> Self {
        self.rx_checksum_offload = offload;
        self
    }
}

impl RxChecksumOffload {
    pub const fn none() -> Self {
        Self {
            ipv4: false,
            tcp: false,
            udp: false,
        }
    }

    pub const fn new(ipv4: bool, tcp: bool, udp: bool) -> Self {
        Self { ipv4, tcp, udp }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocketId(u32);

impl SocketId {
    pub const fn new(raw: u32) -> Self {
        assert!(raw != 0, "socket id must be non-zero");
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpSocketId(u32);

impl UdpSocketId {
    pub const fn new(raw: u32) -> Self {
        assert!(raw != 0, "UDP socket id must be non-zero");
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpListenBacklog(u16);

impl TcpListenBacklog {
    pub const fn new(value: u16) -> Self {
        assert!(value != 0, "TCP listen backlog must be non-zero");
        Self(value)
    }

    pub const fn try_new(value: u16) -> Result<Self, StackError> {
        if value == 0 {
            return Err(StackError::InvalidListenBacklog);
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u16 {
        self.0
    }

    const fn capacity(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DnsQueryId(u32);

/// Identifies one echo exchange. `destination` is the host that was
/// pinged, which is the source address of the reply that answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcmpEchoKey {
    pub destination: IpAddress,
    pub identifier: u16,
    pub sequence: u16,
}

/// A received echo reply, held until the waiter for its key claims it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcmpEchoReply {
    pub key: IcmpEchoKey,
    /// Length of the echoed data, which a caller compares against the
    /// payload it sent.
    pub payload_bytes: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeighborState {
    Incomplete,
    Reachable,
    Stale,
    Delay,
    Probe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NeighborEntry {
    pub ip: IpAddress,
    pub mac: EthernetAddress,
    pub state: NeighborState,
    pub updated_at: StackInstant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub destination: IpCidr,
    pub gateway: Option<IpAddress>,
    pub expires_at: Option<StackInstant>,
}

#[derive(Clone, Debug)]
pub struct RouteTable {
    routes: ArrayVec<Route, MAX_ROUTES>,
}

impl RouteTable {
    pub const fn new() -> Self {
        Self {
            routes: ArrayVec::new_const(),
        }
    }

    pub fn add(&mut self, route: Route) -> Result<(), StackError> {
        if self.routes.iter().any(|existing| {
            existing.destination == route.destination && existing.gateway == route.gateway
        }) {
            return Ok(());
        }
        self.routes
            .try_push(route)
            .map_err(|_| StackError::InconsistentState)
    }

    pub fn remove(&mut self, route: Route) {
        if let Some(index) = self.routes.iter().position(|existing| {
            existing.destination == route.destination && existing.gateway == route.gateway
        }) {
            self.routes.remove(index);
        }
    }

    pub fn clear_ipv4(&mut self) {
        self.routes
            .retain(|route| !matches!(route.destination, IpCidr::Ipv4(_)));
    }

    pub fn clear_ipv6(&mut self) {
        self.routes
            .retain(|route| !matches!(route.destination, IpCidr::Ipv6(_)));
    }

    pub fn resolve(&self, destination: IpAddress) -> Option<Route> {
        self.routes
            .iter()
            .copied()
            .filter(|route| match (route.destination, destination) {
                (IpCidr::Ipv4(cidr), IpAddress::Ipv4(address)) => cidr.contains(address),
                (IpCidr::Ipv6(cidr), IpAddress::Ipv6(address)) => cidr.contains(address),
                _ => false,
            })
            .max_by_key(|route| match route.destination {
                IpCidr::Ipv4(cidr) => cidr.prefix_len(),
                IpCidr::Ipv6(cidr) => cidr.prefix_len(),
            })
    }

    pub fn iter(&self) -> impl Iterator<Item = Route> + '_ {
        self.routes.iter().copied()
    }
}

impl Default for RouteTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DhcpLease {
    pub address: Ipv4Cidr,
    pub router: Option<Ipv4Address>,
    pub dns_servers: DhcpDnsServers,
    pub expires_at: Option<StackInstant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UdpReceive {
    pub source: IpAddress,
    pub destination: IpAddress,
    pub source_port: u16,
    pub destination_port: u16,
    pub bytes: UdpPayload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpEndpoint {
    pub address: IpAddress,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpSocketError {
    RemotePortUnreachable,
    RemoteNetworkUnreachable,
    RemoteHostUnreachable,
    RemoteProtocolUnreachable,
    RemoteCommunicationProhibited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpSocketBinding {
    pub local_address: Option<IpAddress>,
    pub local_port: u16,
    pub remote: Option<UdpEndpoint>,
}

impl UdpSocketBinding {
    pub const fn wildcard(local_port: u16) -> Self {
        Self {
            local_address: None,
            local_port,
            remote: None,
        }
    }

    pub const fn exact(local: UdpEndpoint) -> Self {
        Self {
            local_address: Some(local.address),
            local_port: local.port,
            remote: None,
        }
    }

    pub const fn connected(local: UdpEndpoint, remote: UdpEndpoint) -> Self {
        Self {
            local_address: Some(local.address),
            local_port: local.port,
            remote: Some(remote),
        }
    }

    fn accepts(self, source: IpAddress, source_port: u16, destination: IpAddress) -> bool {
        if !ip_address_families_match(source, destination) {
            return false;
        }
        if self.local_address.is_some_and(|local| local != destination) {
            return false;
        }
        match self.remote {
            Some(remote) => remote.address == source && remote.port == source_port,
            None => true,
        }
    }

    fn overlaps(self, other: Self) -> bool {
        self.local_port == other.local_port
            && optional_ip_addresses_overlap(self.local_address, other.local_address)
            && optional_udp_remotes_overlap(self.remote, other.remote)
    }

    fn exact_packet_binding(
        source: IpAddress,
        source_port: u16,
        destination: IpAddress,
        destination_port: u16,
    ) -> Self {
        Self {
            local_address: Some(destination),
            local_port: destination_port,
            remote: Some(UdpEndpoint {
                address: source,
                port: source_port,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4MulticastMembership {
    pub group: Ipv4Address,
    pub interface: Ipv4Address,
    joins: u32,
}

impl Ipv4MulticastMembership {
    pub const fn joins(self) -> u32 {
        self.joins
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UdpPayload {
    inline: [u8; UDP_INLINE_PAYLOAD_BYTES],
    inline_len: u16,
    heap: Option<Bytes>,
}

impl UdpPayload {
    pub fn copy_from_slice(bytes: &[u8]) -> Self {
        if bytes.len() <= UDP_INLINE_PAYLOAD_BYTES {
            return Self::inline_from_slice(bytes);
        }
        Self {
            inline: [0; UDP_INLINE_PAYLOAD_BYTES],
            inline_len: 0,
            heap: Some(Bytes::copy_from_slice(bytes)),
        }
    }

    /// Keeps small datagrams inline and retains large owning frame slices
    /// without re-copying their payload bytes.
    pub fn from_bytes(bytes: Bytes) -> Self {
        if bytes.len() <= UDP_INLINE_PAYLOAD_BYTES {
            return Self::inline_from_slice(bytes.as_ref());
        }
        Self {
            inline: [0; UDP_INLINE_PAYLOAD_BYTES],
            inline_len: 0,
            heap: Some(bytes),
        }
    }

    fn inline_from_slice(bytes: &[u8]) -> Self {
        let mut inline = [0; UDP_INLINE_PAYLOAD_BYTES];
        inline[..bytes.len()].copy_from_slice(bytes);
        Self {
            inline,
            inline_len: u16::try_from(bytes.len()).expect("UDP inline payload length exceeds u16"),
            heap: None,
        }
    }

    /// Returns the received payload bytes.
    pub fn as_slice(&self) -> &[u8] {
        if let Some(bytes) = self.heap.as_ref() {
            bytes.as_ref()
        } else {
            &self.inline[..usize::from(self.inline_len)]
        }
    }

    /// Converts the payload to reference-counted bytes for outer APIs.
    pub fn into_bytes(self) -> Bytes {
        self.heap
            .unwrap_or_else(|| Bytes::copy_from_slice(&self.inline[..usize::from(self.inline_len)]))
    }

    /// Converts at most `max_bytes` into reference-counted bytes.
    pub fn into_limited_bytes(self, max_bytes: usize) -> Bytes {
        let len = self.len().min(max_bytes);
        match self.heap {
            Some(bytes) => bytes.slice(..len),
            None => Bytes::copy_from_slice(&self.inline[..len]),
        }
    }

    #[cfg(test)]
    fn frame_slice_offset_for_test(&self, frame: &Bytes) -> Option<usize> {
        let payload = self.heap.as_ref()?;
        let frame_start = frame.as_ptr() as usize;
        let frame_end = frame_start + frame.len();
        let payload_start = payload.as_ptr() as usize;
        let payload_end = payload_start + payload.len();
        if payload_start >= frame_start && payload_end <= frame_end {
            Some(payload_start - frame_start)
        } else {
            None
        }
    }
}

impl AsRef<[u8]> for UdpPayload {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl core::ops::Deref for UdpPayload {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpConnectTerminalError {
    RemotePortUnreachable,
    RemoteNetworkUnreachable,
    RemoteHostUnreachable,
    RemoteProtocolUnreachable,
    RemoteCommunicationProhibited,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcpAccept {
    pub socket: SocketId,
    pub remote: TcpEndpoint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TcpQueuedAccept {
    listener: SocketId,
    accepted: TcpAccept,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TcpListenerQueue {
    backlog: TcpListenBacklog,
    pending: usize,
}

impl TcpListenerQueue {
    const fn new(backlog: TcpListenBacklog) -> Self {
        Self {
            backlog,
            pending: 0,
        }
    }

    fn reserve(&mut self) -> bool {
        if self.pending == self.backlog.capacity() {
            return false;
        }
        self.pending += 1;
        true
    }

    fn release(&mut self) {
        assert!(
            self.pending != 0,
            "TCP listener backlog accounting underflowed"
        );
        self.pending -= 1;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpConnectState {
    Pending,
    Connected,
    Closed(TcpConnectTerminalError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TcpReadState {
    Pending,
    Data(Bytes),
    Eof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpReadIntoState {
    Pending,
    Data(usize),
    Eof,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StackEvent {
    DhcpConfigured(DhcpLease),
    /// A Router Advertisement completed stateless autoconfiguration.
    Ipv6Autoconfigured(Ipv6RouterConfiguration),
    NeighborUpdated(NeighborEntry),
    UdpDatagram {
        socket: UdpSocketId,
    },
    TcpConnected {
        socket: SocketId,
    },
    TcpReadable {
        socket: SocketId,
    },
    TcpClosed {
        socket: SocketId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboundBatchStatus {
    Empty,
    Deferred,
    Submitted {
        offered: usize,
        accepted: usize,
        accepted_bytes: usize,
    },
}

/// One heap-allocated chunk of TCP socket slots. Slots stay
/// uninitialised until the slab hands one out.
type TcpSocketSlabChunk<C> = Box<[MaybeUninit<TcpSocket<C>>]>;

struct TcpSocketSlab<C>
where
    C: CongestionControl,
{
    inline_slots: [MaybeUninit<TcpSocket<C>>; TCP_SOCKET_SLAB_CHUNK_SLOTS],
    heap_slots: [Option<TcpSocketSlabChunk<C>>; TCP_SOCKET_HEAP_SLAB_CHUNKS],
    occupied: [bool; MAX_TCP_SOCKETS],
    free: [usize; MAX_TCP_SOCKETS],
    free_len: usize,
    active: [usize; MAX_TCP_SOCKETS],
    active_positions: [usize; MAX_TCP_SOCKETS],
    active_len: usize,
    endpoint_index: TcpEndpointIndex,
    listener_index: TcpListenerIndex,
    terminal_errors: [Option<TcpConnectTerminalError>; MAX_TCP_SOCKETS],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TcpEndpointKey {
    local: TcpEndpoint,
    remote: TcpEndpoint,
}

#[derive(Clone, Debug)]
struct TcpSocketIndex<Key: Copy + Eq, const SLOTS: usize> {
    entries: [Option<TcpSocketIndexEntry<Key>>; SLOTS],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TcpSocketIndexEntry<Key: Copy + Eq> {
    key: Key,
    socket_index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TcpTimerEntry {
    deadline_nanos: u64,
    generation: u32,
    socket_index: usize,
}

#[derive(Clone, Debug)]
struct UdpSocketSlab {
    sockets: [Option<UdpSocketState>; MAX_UDP_SOCKETS],
    free: [usize; MAX_UDP_SOCKETS],
    free_len: usize,
    active: [usize; MAX_UDP_SOCKETS],
    active_positions: [usize; MAX_UDP_SOCKETS],
    active_len: usize,
    binding_index: UdpBindingIndex,
}

#[derive(Clone, Debug)]
struct UdpSocketState {
    binding: UdpSocketBinding,
    pending_error: Option<UdpSocketError>,
    /// IPv4 TTL / IPv6 hop limit stamped on datagrams this socket sends.
    hop_limit: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IcmpQuotedEndpoint {
    local: IpAddress,
    local_port: u16,
    remote: IpAddress,
    remote_port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UdpQueuedReceive {
    socket: UdpSocketId,
    datagram: UdpReceive,
}

#[derive(Clone, Debug)]
struct UdpSocketIndex<Key: Copy + Eq, const SLOTS: usize> {
    entries: [Option<UdpSocketIndexEntry<Key>>; SLOTS],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UdpSocketIndexEntry<Key: Copy + Eq> {
    key: Key,
    socket_index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PathMtuKey {
    source: IpAddress,
    destination: IpAddress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PathMtuEntry {
    key: PathMtuKey,
    mtu: usize,
    updated_at: StackInstant,
}

#[derive(Clone, Debug)]
struct PathMtuTable {
    entries: ArrayVec<PathMtuEntry, MAX_PATH_MTU_ENTRIES>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ipv4ReassemblyKey {
    source: Ipv4Address,
    destination: Ipv4Address,
    protocol: crate::IpProtocol,
    identification: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Ipv4Fragment {
    offset: usize,
    bytes: Bytes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Ipv4ReassemblyEntry {
    key: Ipv4ReassemblyKey,
    header: [u8; IPV4_MAX_HEADER_LEN],
    header_len: usize,
    fragments: Vec<Ipv4Fragment>,
    total_payload_len: Option<usize>,
    cached_bytes: usize,
    updated_at: StackInstant,
}

#[derive(Clone, Debug)]
struct Ipv4ReassemblyCache {
    entries: ArrayVec<Ipv4ReassemblyEntry, MAX_IPV4_REASSEMBLY_ENTRIES>,
    cached_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ipv4ReassemblyResult {
    Complete(Bytes),
    NeedMoreFragments,
    InvalidFragment,
    CacheFull,
}

trait TcpIndexKey: Copy + Eq {
    fn hash(self) -> usize;
}

trait UdpIndexKey: Copy + Eq {
    fn hash(self) -> usize;
}

type TcpEndpointIndex = TcpSocketIndex<TcpEndpointKey, TCP_ENDPOINT_INDEX_SLOTS>;
type TcpListenerIndex = TcpSocketIndex<TcpEndpoint, TCP_LISTENER_INDEX_SLOTS>;
type UdpBindingIndex = UdpSocketIndex<UdpSocketBinding, UDP_BINDING_INDEX_SLOTS>;

impl PathMtuKey {
    const fn new(source: IpAddress, destination: IpAddress) -> Self {
        Self {
            source,
            destination,
        }
    }

    fn hash(self) -> usize {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        hash = mix_ip_address(hash, self.source);
        hash = mix_ip_address(hash, self.destination);
        hash as usize
    }

    const fn minimum_link_mtu(self) -> usize {
        match (self.source, self.destination) {
            (IpAddress::Ipv4(_), IpAddress::Ipv4(_)) => IPV4_MINIMUM_LINK_MTU,
            (IpAddress::Ipv6(_), IpAddress::Ipv6(_)) => IPV6_MINIMUM_LINK_MTU,
            _ => panic!("PMTU key address families must match"),
        }
    }
}

impl PathMtuTable {
    const fn new() -> Self {
        Self {
            entries: ArrayVec::new_const(),
        }
    }

    fn get(&self, key: PathMtuKey) -> Option<usize> {
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.mtu)
    }

    fn update_if_less(&mut self, key: PathMtuKey, new_mtu: usize, now: StackInstant) -> bool {
        if new_mtu < key.minimum_link_mtu() {
            return false;
        }
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.key == key) {
            if new_mtu < entry.mtu {
                entry.mtu = new_mtu;
                entry.updated_at = now;
                return true;
            }
            return false;
        }
        if self.entries.is_full() {
            let index = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(index, entry)| {
                    (entry.updated_at, key_distance(entry.key, key), *index)
                })
                .map(|(index, _)| index)
                .expect("full PMTU table must contain at least one entry");
            self.entries[index] = PathMtuEntry {
                key,
                mtu: new_mtu,
                updated_at: now,
            };
            return true;
        }
        self.entries
            .try_push(PathMtuEntry {
                key,
                mtu: new_mtu,
                updated_at: now,
            })
            .unwrap_or_else(|_| panic!("PMTU table reported capacity but push failed"));
        true
    }

    fn update_next_lower(&mut self, key: PathMtuKey, from: usize, now: StackInstant) -> bool {
        let Some(next) = next_lower_pmtu_plateau(from) else {
            return false;
        };
        self.update_if_less(key, next, now)
    }
}

impl Ipv4ReassemblyKey {
    const fn new(packet: Ipv4Packet<'_>) -> Self {
        Self {
            source: packet.source,
            destination: packet.destination,
            protocol: packet.protocol,
            identification: packet.identification,
        }
    }

    fn hash(self) -> usize {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        hash = mix_ip_address(hash, IpAddress::Ipv4(self.source));
        hash = mix_ip_address(hash, IpAddress::Ipv4(self.destination));
        hash = mix_u8(hash, self.protocol.number());
        hash = mix_u16(hash, self.identification);
        hash as usize
    }
}

impl Ipv4Fragment {
    fn end(&self) -> usize {
        self.offset
            .checked_add(self.bytes.len())
            .unwrap_or_else(|| panic!("IPv4 fragment byte range overflowed"))
    }

    fn range_matches(&self, offset: usize, bytes: &[u8]) -> bool {
        self.offset == offset && self.bytes.len() == bytes.len() && self.bytes.as_ref() == bytes
    }

    fn overlaps(&self, offset: usize, end: usize) -> bool {
        self.offset < end && offset < self.end()
    }
}

impl Ipv4ReassemblyEntry {
    fn new(key: Ipv4ReassemblyKey, updated_at: StackInstant) -> Self {
        Self {
            key,
            header: [0; IPV4_MAX_HEADER_LEN],
            header_len: 0,
            fragments: Vec::new(),
            total_payload_len: None,
            cached_bytes: 0,
            updated_at,
        }
    }

    const fn has_header(&self) -> bool {
        self.header_len != 0
    }

    fn expired(&self, now: StackInstant) -> bool {
        now.nanos().saturating_sub(self.updated_at.nanos()) >= IPV4_REASSEMBLY_TIMEOUT_NANOS
    }

    fn insert_fragment(
        &mut self,
        packet: Ipv4Packet<'_>,
        packet_bytes: &[u8],
        payload_bytes: Bytes,
        now: StackInstant,
    ) -> Ipv4FragmentInsert {
        let offset = usize::from(packet.fragment_offset_blocks) * IPV4_FRAGMENT_BLOCK_SIZE;
        let end = offset
            .checked_add(payload_bytes.len())
            .unwrap_or_else(|| panic!("validated IPv4 fragment offset overflowed"));
        let duplicate = self
            .fragments
            .iter()
            .find(|fragment| fragment.range_matches(offset, payload_bytes.as_ref()));
        if let Some(_duplicate) = duplicate {
            if offset == 0
                && self.has_header()
                && self.header_bytes() != &packet_bytes[..packet.header_len]
            {
                return Ipv4FragmentInsert::Invalid;
            }
            self.updated_at = now;
            return Ipv4FragmentInsert::Duplicate;
        }
        if self
            .fragments
            .iter()
            .any(|fragment| fragment.overlaps(offset, end))
        {
            return Ipv4FragmentInsert::Invalid;
        }
        if self.fragments.len() == MAX_IPV4_REASSEMBLY_FRAGMENTS {
            return Ipv4FragmentInsert::Invalid;
        }
        if let Some(total_payload_len) = self.total_payload_len
            && (end > total_payload_len || (packet.more_fragments && end >= total_payload_len))
        {
            return Ipv4FragmentInsert::Invalid;
        }
        let mut added_bytes = payload_bytes.len();
        if offset == 0 {
            if self.has_header() {
                return Ipv4FragmentInsert::Invalid;
            }
            self.header[..packet.header_len].copy_from_slice(&packet_bytes[..packet.header_len]);
            self.header_len = packet.header_len;
            added_bytes = added_bytes
                .checked_add(packet.header_len)
                .unwrap_or_else(|| panic!("IPv4 reassembly cached byte count overflowed"));
        }
        if !packet.more_fragments {
            if self
                .total_payload_len
                .is_some_and(|total_payload_len| total_payload_len != end)
            {
                return Ipv4FragmentInsert::Invalid;
            }
            self.total_payload_len = Some(end);
        }
        if self.has_header()
            && self
                .total_payload_len
                .is_some_and(|total_payload_len| total_payload_len > self.max_payload_len())
        {
            return Ipv4FragmentInsert::Invalid;
        }
        self.fragments.push(Ipv4Fragment {
            offset,
            bytes: payload_bytes,
        });
        self.fragments.sort_by_key(|fragment| fragment.offset);
        self.cached_bytes = self
            .cached_bytes
            .checked_add(added_bytes)
            .unwrap_or_else(|| panic!("IPv4 reassembly entry byte accounting overflowed"));
        self.updated_at = now;
        Ipv4FragmentInsert::Added {
            added_bytes,
            complete: self.complete(),
        }
    }

    fn reassemble(&self) -> Bytes {
        let total_payload_len = self
            .total_payload_len
            .expect("IPv4 reassembly requires final payload length");
        assert!(
            self.has_header(),
            "IPv4 reassembly requires first-fragment header"
        );
        assert!(
            self.complete(),
            "IPv4 reassembly cannot run with missing fragments"
        );
        let total_len = self
            .header_len
            .checked_add(total_payload_len)
            .unwrap_or_else(|| panic!("IPv4 reassembled packet length overflowed"));
        let total_len_u16 =
            u16::try_from(total_len).expect("IPv4 reassembled packet exceeds u16 total length");
        let mut packet = Vec::with_capacity(total_len);
        packet.extend_from_slice(self.header_bytes());
        packet.resize(total_len, 0);
        for fragment in &self.fragments {
            let start = self
                .header_len
                .checked_add(fragment.offset)
                .unwrap_or_else(|| panic!("IPv4 reassembled fragment start overflowed"));
            let end = start
                .checked_add(fragment.bytes.len())
                .unwrap_or_else(|| panic!("IPv4 reassembled fragment end overflowed"));
            packet[start..end].copy_from_slice(fragment.bytes.as_ref());
        }
        packet[2..4].copy_from_slice(&total_len_u16.to_be_bytes());
        packet[6..8].copy_from_slice(&0u16.to_be_bytes());
        packet[10..12].copy_from_slice(&0u16.to_be_bytes());
        let checksum = ipv4_checksum(&packet[..self.header_len]);
        packet[10..12].copy_from_slice(&checksum.to_be_bytes());
        Bytes::from(packet)
    }

    fn complete(&self) -> bool {
        let Some(total_payload_len) = self.total_payload_len else {
            return false;
        };
        if !self.has_header() {
            return false;
        }
        let mut next_offset = 0usize;
        for fragment in &self.fragments {
            if fragment.offset != next_offset {
                return false;
            }
            next_offset = fragment.end();
        }
        next_offset == total_payload_len
    }

    fn header_bytes(&self) -> &[u8] {
        &self.header[..self.header_len]
    }

    const fn max_payload_len(&self) -> usize {
        IPV4_MAX_REASSEMBLED_PACKET_LEN - self.header_len
    }
}

impl Ipv4ReassemblyCache {
    const fn new() -> Self {
        Self {
            entries: ArrayVec::new_const(),
            cached_bytes: 0,
        }
    }

    fn process_fragment(
        &mut self,
        packet: Ipv4Packet<'_>,
        packet_bytes: &[u8],
        payload_bytes: Bytes,
        now: StackInstant,
    ) -> Ipv4ReassemblyResult {
        self.expire(now);
        if !Self::fragment_shape_valid(packet) {
            self.remove_key(Ipv4ReassemblyKey::new(packet));
            return Ipv4ReassemblyResult::InvalidFragment;
        }
        let key = Ipv4ReassemblyKey::new(packet);
        let fragment_offset = usize::from(packet.fragment_offset_blocks) * IPV4_FRAGMENT_BLOCK_SIZE;
        let fragment_end = fragment_offset
            .checked_add(payload_bytes.len())
            .unwrap_or_else(|| panic!("validated IPv4 fragment offset overflowed"));
        if fragment_end > IPV4_MAX_REASSEMBLED_PAYLOAD_LEN {
            self.remove_key(key);
            return Ipv4ReassemblyResult::InvalidFragment;
        }
        let index = match self.entry_index_or_insert(key, now) {
            Some(index) => index,
            None => return Ipv4ReassemblyResult::CacheFull,
        };
        let header_added =
            if packet.fragment_offset_blocks == 0 && !self.entries[index].has_header() {
                packet.header_len
            } else {
                0
            };
        let expected_added = payload_bytes
            .len()
            .checked_add(header_added)
            .unwrap_or_else(|| panic!("IPv4 reassembly expected byte count overflowed"));
        if !self.make_room_for(index, expected_added) {
            return Ipv4ReassemblyResult::CacheFull;
        }
        let insert = self.entries[index].insert_fragment(packet, packet_bytes, payload_bytes, now);
        match insert {
            Ipv4FragmentInsert::Duplicate => Ipv4ReassemblyResult::NeedMoreFragments,
            Ipv4FragmentInsert::Invalid => {
                self.remove_index(index);
                Ipv4ReassemblyResult::InvalidFragment
            }
            Ipv4FragmentInsert::Added {
                added_bytes,
                complete,
            } => {
                self.cached_bytes = self
                    .cached_bytes
                    .checked_add(added_bytes)
                    .unwrap_or_else(|| panic!("IPv4 reassembly cache byte accounting overflowed"));
                if complete {
                    let packet = self.entries[index].reassemble();
                    self.remove_index(index);
                    Ipv4ReassemblyResult::Complete(packet)
                } else {
                    Ipv4ReassemblyResult::NeedMoreFragments
                }
            }
        }
    }

    fn retain_destinations(&mut self, mut keep: impl FnMut(Ipv4ReassemblyKey) -> bool) {
        let mut index = 0;
        while index < self.entries.len() {
            if keep(self.entries[index].key) {
                index += 1;
            } else {
                self.remove_index(index);
            }
        }
    }

    fn expire(&mut self, now: StackInstant) {
        let mut index = 0;
        while index < self.entries.len() {
            if self.entries[index].expired(now) {
                self.remove_index(index);
            } else {
                index += 1;
            }
        }
    }

    fn fragment_shape_valid(packet: Ipv4Packet<'_>) -> bool {
        packet.is_fragmented()
            && !packet.payload.is_empty()
            && (!packet.more_fragments
                || packet
                    .payload
                    .len()
                    .is_multiple_of(IPV4_FRAGMENT_BLOCK_SIZE))
            && packet.fragment_offset_blocks <= IPV4_MAX_FRAGMENT_BLOCKS
    }

    fn entry_index_or_insert(
        &mut self,
        key: Ipv4ReassemblyKey,
        now: StackInstant,
    ) -> Option<usize> {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            return Some(index);
        }
        if self.entries.is_full() {
            self.evict_for_key(key)?;
        }
        self.entries
            .try_push(Ipv4ReassemblyEntry::new(key, now))
            .unwrap_or_else(|_| panic!("IPv4 reassembly table reported room but insert failed"));
        Some(self.entries.len() - 1)
    }

    fn make_room_for(&mut self, protected_index: usize, bytes: usize) -> bool {
        while self
            .cached_bytes
            .checked_add(bytes)
            .is_some_and(|bytes| bytes > MAX_IPV4_REASSEMBLY_BYTES)
        {
            let Some(victim) = self.oldest_index_except(protected_index) else {
                return false;
            };
            self.remove_index(victim);
        }
        true
    }

    fn evict_for_key(&mut self, key: Ipv4ReassemblyKey) -> Option<()> {
        let index = self
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(index, entry)| {
                (
                    entry.updated_at,
                    reassembly_key_distance(entry.key, key),
                    *index,
                )
            })
            .map(|(index, _)| index)?;
        self.remove_index(index);
        Some(())
    }

    fn oldest_index_except(&self, protected_index: usize) -> Option<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != protected_index)
            .min_by_key(|(index, entry)| (entry.updated_at, *index))
            .map(|(index, _)| index)
    }

    fn remove_key(&mut self, key: Ipv4ReassemblyKey) {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            self.remove_index(index);
        }
    }

    fn remove_index(&mut self, index: usize) {
        let entry = self.entries.remove(index);
        self.cached_bytes = self
            .cached_bytes
            .checked_sub(entry.cached_bytes)
            .expect("IPv4 reassembly byte accounting underflowed");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ipv4FragmentInsert {
    Added { added_bytes: usize, complete: bool },
    Duplicate,
    Invalid,
}

impl<C> TcpSocketSlab<C>
where
    C: CongestionControl,
{
    fn new() -> Self {
        Self {
            inline_slots: [const { MaybeUninit::uninit() }; TCP_SOCKET_SLAB_CHUNK_SLOTS],
            heap_slots: core::array::from_fn(|_| None),
            occupied: [false; MAX_TCP_SOCKETS],
            free: core::array::from_fn(|index| MAX_TCP_SOCKETS - 1 - index),
            free_len: MAX_TCP_SOCKETS,
            active: [0; MAX_TCP_SOCKETS],
            active_positions: [0; MAX_TCP_SOCKETS],
            active_len: 0,
            endpoint_index: TcpEndpointIndex::new(),
            listener_index: TcpListenerIndex::new(),
            terminal_errors: [None; MAX_TCP_SOCKETS],
        }
    }

    const fn active_len(&self) -> usize {
        self.active_len
    }

    const fn has_free_slot(&self) -> bool {
        self.free_len != 0
    }

    fn active_index(&self, slot: usize) -> usize {
        *self
            .active
            .get(slot)
            .unwrap_or_else(|| panic!("TCP active socket slot {slot} is outside active set"))
    }

    fn get(&self, index: usize) -> Option<&TcpSocket<C>> {
        if self.occupied.get(index).copied().unwrap_or(false) {
            Some(self.slot_ref(index))
        } else {
            None
        }
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut TcpSocket<C>> {
        if self.occupied.get(index).copied().unwrap_or(false) {
            Some(self.slot_mut(index))
        } else {
            None
        }
    }

    fn insert(&mut self, socket: TcpSocket<C>) -> SocketId {
        if self.free_len == 0 {
            panic!("TCP socket slab is full");
        };
        self.free_len -= 1;
        let index = self.free[self.free_len];
        self.materialize_slot_chunk(index);
        assert!(
            !self.occupied[index],
            "TCP socket slab free list is corrupt"
        );
        self.slot_storage_mut(index).write(socket);
        self.occupied[index] = true;
        self.terminal_errors[index] = None;
        self.insert_indices(index);
        self.insert_active(index);
        socket_id(index)
    }

    fn remove(&mut self, index: usize) -> Option<TcpSocket<C>> {
        if !self.occupied.get(index).copied().unwrap_or(false) {
            return None;
        }
        self.remove_indices(index);
        let socket = unsafe { self.slot_storage_mut(index).assume_init_read() };
        self.occupied[index] = false;
        self.terminal_errors[index] = None;
        self.remove_active(index);
        self.free[self.free_len] = index;
        self.free_len += 1;
        Some(socket)
    }

    fn insert_active(&mut self, index: usize) {
        assert!(
            self.active_len < MAX_TCP_SOCKETS,
            "TCP active socket index is full"
        );
        self.active_positions[index] = self.active_len;
        self.active[self.active_len] = index;
        self.active_len += 1;
    }

    fn remove_active(&mut self, index: usize) {
        assert!(self.active_len != 0, "TCP active socket index underflowed");
        let position = self.active_positions[index];
        assert!(
            position < self.active_len && self.active[position] == index,
            "TCP active socket index is corrupt"
        );
        self.active_len -= 1;
        let moved = self.active[self.active_len];
        self.active[position] = moved;
        self.active_positions[moved] = position;
    }

    fn find_endpoint(&self, local: TcpEndpoint, remote: TcpEndpoint) -> Option<usize> {
        self.endpoint_index
            .lookup(TcpEndpointKey::new(local, remote))
    }

    fn terminal_error(&self, index: usize) -> Option<TcpConnectTerminalError> {
        *self
            .terminal_errors
            .get(index)
            .unwrap_or_else(|| panic!("TCP terminal error index {index} is outside socket slab"))
    }

    fn set_terminal_error(&mut self, index: usize, error: TcpConnectTerminalError) {
        let slot = self
            .terminal_errors
            .get_mut(index)
            .unwrap_or_else(|| panic!("TCP terminal error index {index} is outside socket slab"));
        *slot = Some(error);
    }

    fn find_listener(&self, local: TcpEndpoint) -> Option<usize> {
        self.listener_index
            .lookup(local)
            .or_else(|| self.listener_index.lookup(wildcard_endpoint(local)))
    }

    fn insert_indices(&mut self, index: usize) {
        let Some(socket) = self.get(index) else {
            panic!("TCP socket slab cannot index an unoccupied slot");
        };
        let (endpoint_key, listener_key) = tcp_socket_index_keys(socket);
        if let Some(key) = endpoint_key {
            self.endpoint_index.insert(key, index);
        }
        if let Some(key) = listener_key {
            self.listener_index.insert(key, index);
        }
    }

    fn remove_indices(&mut self, index: usize) {
        let Some(socket) = self.get(index) else {
            panic!("TCP socket slab cannot de-index an unoccupied slot");
        };
        let (endpoint_key, listener_key) = tcp_socket_endpoint_keys(socket);
        if let Some(key) = endpoint_key {
            self.endpoint_index.remove(key);
        }
        if let Some(key) = listener_key {
            self.listener_index.remove(key);
        }
    }

    fn slot_ref(&self, index: usize) -> &TcpSocket<C> {
        let (chunk, slot) = socket_slot_chunk(index);
        unsafe { self.chunk_ref(chunk)[slot].assume_init_ref() }
    }

    fn slot_mut(&mut self, index: usize) -> &mut TcpSocket<C> {
        let (chunk, slot) = socket_slot_chunk(index);
        unsafe { self.chunk_mut(chunk)[slot].assume_init_mut() }
    }

    fn slot_storage_mut(&mut self, index: usize) -> &mut MaybeUninit<TcpSocket<C>> {
        let (chunk, slot) = socket_slot_chunk(index);
        &mut self.chunk_mut(chunk)[slot]
    }

    fn materialize_slot_chunk(&mut self, index: usize) {
        let (chunk, _) = socket_slot_chunk(index);
        if chunk == 0 {
            return;
        }
        let heap_chunk = chunk - 1;
        if self.heap_slots[heap_chunk].is_none() {
            self.heap_slots[heap_chunk] = Some(Box::<[TcpSocket<C>]>::new_uninit_slice(
                TCP_SOCKET_SLAB_CHUNK_SLOTS,
            ));
        }
    }

    fn chunk_ref(&self, chunk: usize) -> &[MaybeUninit<TcpSocket<C>>] {
        if chunk == 0 {
            return &self.inline_slots;
        }
        self.heap_slots
            .get(chunk - 1)
            .and_then(Option::as_deref)
            .expect("TCP socket chunk is not materialized")
    }

    fn chunk_mut(&mut self, chunk: usize) -> &mut [MaybeUninit<TcpSocket<C>>] {
        if chunk == 0 {
            return &mut self.inline_slots;
        }
        self.heap_slots
            .get_mut(chunk - 1)
            .and_then(Option::as_deref_mut)
            .expect("TCP socket chunk is not materialized")
    }
}

impl<C> Clone for TcpSocketSlab<C>
where
    C: CongestionControl,
{
    fn clone(&self) -> Self {
        let mut cloned = Self {
            inline_slots: [const { MaybeUninit::uninit() }; TCP_SOCKET_SLAB_CHUNK_SLOTS],
            heap_slots: core::array::from_fn(|chunk| {
                self.heap_slots[chunk]
                    .as_ref()
                    .map(|_| Box::<[TcpSocket<C>]>::new_uninit_slice(TCP_SOCKET_SLAB_CHUNK_SLOTS))
            }),
            occupied: self.occupied,
            free: self.free,
            free_len: self.free_len,
            active: self.active,
            active_positions: self.active_positions,
            active_len: self.active_len,
            endpoint_index: self.endpoint_index.clone(),
            listener_index: self.listener_index.clone(),
            terminal_errors: self.terminal_errors,
        };
        for active_slot in 0..self.active_len {
            let index = self.active[active_slot];
            cloned
                .slot_storage_mut(index)
                .write(self.slot_ref(index).clone());
        }
        cloned
    }
}

impl<C> fmt::Debug for TcpSocketSlab<C>
where
    C: CongestionControl,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TcpSocketSlab")
            .field("free_len", &self.free_len)
            .field("active_len", &self.active_len)
            .finish_non_exhaustive()
    }
}

impl<C> Drop for TcpSocketSlab<C>
where
    C: CongestionControl,
{
    fn drop(&mut self) {
        for active_slot in 0..self.active_len {
            let index = self.active[active_slot];
            if self.occupied[index] {
                unsafe {
                    self.slot_storage_mut(index).assume_init_drop();
                }
                self.occupied[index] = false;
            }
        }
    }
}

impl TcpEndpointKey {
    const fn new(local: TcpEndpoint, remote: TcpEndpoint) -> Self {
        Self { local, remote }
    }
}

impl TcpIndexKey for TcpEndpointKey {
    fn hash(self) -> usize {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        hash = mix_endpoint(hash, self.local);
        hash = mix_endpoint(hash, self.remote);
        hash as usize
    }
}

impl TcpIndexKey for TcpEndpoint {
    fn hash(self) -> usize {
        mix_endpoint(0xcbf2_9ce4_8422_2325u64, self) as usize
    }
}

impl<Key: TcpIndexKey, const SLOTS: usize> TcpSocketIndex<Key, SLOTS> {
    fn new() -> Self {
        assert!(SLOTS != 0, "TCP socket index capacity must be non-zero");
        Self {
            entries: [None; SLOTS],
        }
    }

    fn insert(&mut self, key: Key, socket_index: usize) {
        for index in Self::probe_indices(key) {
            match self.entries[index] {
                Some(entry) if entry.key == key => {
                    panic!("TCP socket index duplicate key");
                }
                Some(_) => {}
                None => {
                    self.entries[index] = Some(TcpSocketIndexEntry { key, socket_index });
                    return;
                }
            }
        }
        panic!("TCP socket index is full");
    }

    fn remove(&mut self, key: Key) {
        for index in Self::probe_indices(key) {
            match self.entries[index] {
                Some(entry) if entry.key == key => {
                    self.entries[index] = None;
                    self.reinsert_probe_cluster_after(index);
                    return;
                }
                Some(_) => {}
                None => return,
            }
        }
    }

    fn lookup(&self, key: Key) -> Option<usize> {
        for index in Self::probe_indices(key) {
            match self.entries[index] {
                Some(entry) if entry.key == key => return Some(entry.socket_index),
                Some(_) => {}
                None => return None,
            }
        }
        None
    }

    fn reinsert_probe_cluster_after(&mut self, removed_index: usize) {
        let mut index = (removed_index + 1) % SLOTS;
        while let Some(entry) = self.entries[index].take() {
            self.insert(entry.key, entry.socket_index);
            index = (index + 1) % SLOTS;
        }
    }

    fn probe_indices(key: Key) -> impl Iterator<Item = usize> {
        let start = key.hash() % SLOTS;
        (0..SLOTS).map(move |offset| (start + offset) % SLOTS)
    }
}

impl UdpSocketState {
    const fn new(binding: UdpSocketBinding) -> Self {
        Self {
            binding,
            pending_error: None,
            hop_limit: crate::DEFAULT_HOP_LIMIT,
        }
    }
}

impl UdpSocketSlab {
    fn new() -> Self {
        Self {
            sockets: [const { None }; MAX_UDP_SOCKETS],
            free: core::array::from_fn(|index| MAX_UDP_SOCKETS - 1 - index),
            free_len: MAX_UDP_SOCKETS,
            active: [0; MAX_UDP_SOCKETS],
            active_positions: [0; MAX_UDP_SOCKETS],
            active_len: 0,
            binding_index: UdpBindingIndex::new(),
        }
    }

    fn insert(&mut self, binding: UdpSocketBinding) -> Result<UdpSocketId, StackError> {
        if self.binding_overlaps(binding) {
            return Err(StackError::AddressInUse);
        }
        if self.free_len == 0 {
            panic!("UDP socket slab is full");
        }
        self.free_len -= 1;
        let index = self.free[self.free_len];
        assert!(
            self.sockets[index].is_none(),
            "UDP socket slab free list is corrupt"
        );
        self.sockets[index] = Some(UdpSocketState::new(binding));
        self.binding_index.insert(binding, index);
        self.insert_active(index);
        Ok(udp_socket_id(index))
    }

    fn rebind(&mut self, socket: UdpSocketId, binding: UdpSocketBinding) -> Result<(), StackError> {
        let index = udp_socket_index(socket);
        let old_binding = self
            .sockets
            .get(index)
            .and_then(Option::as_ref)
            .ok_or(StackError::UnknownSocket)?
            .binding;
        if old_binding == binding {
            return Ok(());
        }
        if self.binding_overlaps_except(binding, index) {
            return Err(StackError::AddressInUse);
        }
        let state = self
            .sockets
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or(StackError::UnknownSocket)?;
        self.binding_index.remove(old_binding);
        state.binding = binding;
        state.pending_error = None;
        self.binding_index.insert(binding, index);
        Ok(())
    }

    fn remove(&mut self, index: usize) -> Option<UdpSocketState> {
        let socket = self.sockets.get_mut(index).and_then(Option::take)?;
        self.binding_index.remove(socket.binding);
        self.remove_active(index);
        self.free[self.free_len] = index;
        self.free_len += 1;
        Some(socket)
    }

    fn get(&self, index: usize) -> Option<&UdpSocketState> {
        self.sockets.get(index).and_then(Option::as_ref)
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut UdpSocketState> {
        self.sockets.get_mut(index).and_then(Option::as_mut)
    }

    fn binding_free(&self, binding: UdpSocketBinding) -> bool {
        !self.binding_overlaps(binding)
    }

    fn socket_for_packet(
        &self,
        source: IpAddress,
        source_port: u16,
        destination: IpAddress,
        destination_port: u16,
    ) -> Option<UdpSocketId> {
        let packet_binding = UdpSocketBinding::exact_packet_binding(
            source,
            source_port,
            destination,
            destination_port,
        );
        self.find_best_binding(packet_binding).map(udp_socket_id)
    }

    fn socket_for_connected_endpoint(
        &self,
        local: UdpEndpoint,
        remote: UdpEndpoint,
    ) -> Option<UdpSocketId> {
        self.binding_index
            .lookup(UdpSocketBinding::connected(local, remote))
            .or_else(|| {
                self.binding_index.lookup(UdpSocketBinding {
                    local_address: None,
                    local_port: local.port,
                    remote: Some(remote),
                })
            })
            .map(udp_socket_id)
    }

    fn set_pending_error(&mut self, socket: UdpSocketId, error: UdpSocketError) {
        let Some(state) = self
            .sockets
            .get_mut(udp_socket_index(socket))
            .and_then(Option::as_mut)
        else {
            panic!("UDP socket id resolved from index should exist");
        };
        state.pending_error = Some(error);
    }

    fn take_pending_error(
        &mut self,
        socket: UdpSocketId,
    ) -> Result<Option<UdpSocketError>, StackError> {
        self.sockets
            .get_mut(udp_socket_index(socket))
            .and_then(Option::as_mut)
            .map(|state| state.pending_error.take())
            .ok_or(StackError::UnknownSocket)
    }

    fn insert_active(&mut self, index: usize) {
        assert!(
            self.active_len < MAX_UDP_SOCKETS,
            "UDP active socket index is full"
        );
        self.active_positions[index] = self.active_len;
        self.active[self.active_len] = index;
        self.active_len += 1;
    }

    fn remove_active(&mut self, index: usize) {
        assert!(self.active_len != 0, "UDP active socket index underflowed");
        let position = self.active_positions[index];
        assert!(
            position < self.active_len && self.active[position] == index,
            "UDP active socket index is corrupt"
        );
        self.active_len -= 1;
        let moved = self.active[self.active_len];
        self.active[position] = moved;
        self.active_positions[moved] = position;
    }

    fn binding_overlaps(&self, binding: UdpSocketBinding) -> bool {
        self.binding_overlaps_except(binding, usize::MAX)
    }

    fn binding_overlaps_except(&self, binding: UdpSocketBinding, except_index: usize) -> bool {
        for active_slot in 0..self.active_len {
            let index = self.active[active_slot];
            if index == except_index {
                continue;
            }
            let socket = self
                .get(index)
                .expect("UDP active socket index referenced a missing socket");
            if socket.binding.overlaps(binding) {
                return true;
            }
        }
        false
    }

    fn find_best_binding(&self, packet_binding: UdpSocketBinding) -> Option<usize> {
        [
            packet_binding,
            UdpSocketBinding {
                remote: None,
                ..packet_binding
            },
            UdpSocketBinding {
                local_address: None,
                remote: packet_binding.remote,
                ..packet_binding
            },
            UdpSocketBinding {
                local_address: None,
                remote: None,
                ..packet_binding
            },
        ]
        .into_iter()
        .find_map(|candidate| self.binding_index.lookup(candidate))
        .or_else(|| self.find_overlapping_binding(packet_binding))
    }

    fn find_overlapping_binding(&self, packet_binding: UdpSocketBinding) -> Option<usize> {
        let remote = packet_binding
            .remote
            .expect("UDP packet binding must have a remote");
        let local_address = packet_binding
            .local_address
            .expect("UDP packet binding must have a local address");
        for active_slot in 0..self.active_len {
            let index = self.active[active_slot];
            let socket = self
                .get(index)
                .expect("UDP active socket index referenced a missing socket");
            if socket
                .binding
                .accepts(remote.address, remote.port, local_address)
            {
                return Some(index);
            }
        }
        None
    }
}

impl UdpIndexKey for UdpSocketBinding {
    fn hash(self) -> usize {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        hash = mix_optional_ip_address(hash, self.local_address);
        hash = mix_u16(hash, self.local_port);
        match self.remote {
            Some(remote) => {
                hash = mix_ip_address(hash, remote.address);
                hash = mix_u16(hash, remote.port);
            }
            None => {
                hash = mix_u8(hash, 0);
            }
        }
        hash as usize
    }
}

impl<Key: UdpIndexKey, const SLOTS: usize> UdpSocketIndex<Key, SLOTS> {
    fn new() -> Self {
        assert!(SLOTS != 0, "UDP socket index capacity must be non-zero");
        Self {
            entries: [None; SLOTS],
        }
    }

    fn insert(&mut self, key: Key, socket_index: usize) {
        for index in Self::probe_indices(key) {
            match self.entries[index] {
                Some(entry) if entry.key == key => {
                    panic!("UDP socket index duplicate key");
                }
                Some(_) => {}
                None => {
                    self.entries[index] = Some(UdpSocketIndexEntry { key, socket_index });
                    return;
                }
            }
        }
        panic!("UDP socket index is full");
    }

    fn remove(&mut self, key: Key) {
        for index in Self::probe_indices(key) {
            match self.entries[index] {
                Some(entry) if entry.key == key => {
                    self.entries[index] = None;
                    self.reinsert_probe_cluster_after(index);
                    return;
                }
                Some(_) => {}
                None => return,
            }
        }
    }

    fn lookup(&self, key: Key) -> Option<usize> {
        for index in Self::probe_indices(key) {
            match self.entries[index] {
                Some(entry) if entry.key == key => return Some(entry.socket_index),
                Some(_) => {}
                None => return None,
            }
        }
        None
    }

    fn reinsert_probe_cluster_after(&mut self, removed_index: usize) {
        let mut index = (removed_index + 1) % SLOTS;
        while let Some(entry) = self.entries[index].take() {
            self.insert(entry.key, entry.socket_index);
            index = (index + 1) % SLOTS;
        }
    }

    fn probe_indices(key: Key) -> impl Iterator<Item = usize> {
        let start = key.hash() % SLOTS;
        (0..SLOTS).map(move |offset| (start + offset) % SLOTS)
    }
}

#[derive(Clone, Debug)]
pub struct Stack<C = BbrV3>
where
    C: CongestionControl,
{
    config: StackConfig,
    congestion: C,
    routes: RouteTable,
    path_mtu: PathMtuTable,
    ipv4_reassembly: Ipv4ReassemblyCache,
    ipv4_addresses: ArrayVec<Ipv4Cidr, 8>,
    ipv6_addresses: ArrayVec<Ipv6Cidr, 16>,
    /// Router-solicitation scheduling for stateless autoconfiguration.
    /// Owned by this stack, therefore by exactly one network shard; see
    /// the concurrency contract on [`crate::NeighborDiscovery`].
    ipv6_discovery: NeighborDiscovery,
    /// Recursive resolvers learned from a Router Advertisement's RDNSS
    /// option, the IPv6 counterpart of the DHCPv4 lease's DNS servers.
    ipv6_dns_servers: Ipv6DnsServers,
    ipv4_multicast_memberships: ArrayVec<Ipv4MulticastMembership, MAX_IPV4_MULTICAST_MEMBERSHIPS>,
    neighbors: ArrayVec<NeighborEntry, MAX_NEIGHBORS>,
    outbound: OutboundFrameQueue,
    events: Deque<StackEvent, MAX_STACK_EVENTS>,
    udp: UdpSocketSlab,
    udp_rx: Deque<UdpQueuedReceive, MAX_UDP_RX>,
    /// Echo replies observed on this shard's receive path, oldest
    /// first. ICMP frames carry no port, so every one of them lands on
    /// shard 0 and this buffer belongs to that shard alone.
    icmp_echo_replies: ArrayVec<IcmpEchoReply, MAX_ICMP_ECHO_REPLIES>,
    udp_rx_counts: [u8; MAX_UDP_SOCKETS],
    tcp_accept: Deque<TcpQueuedAccept, MAX_TCP_ACCEPT>,
    tcp_listener_queues: TcpListenerQueues,
    tcp_listener_children: [Option<SocketId>; MAX_TCP_SOCKETS],
    tcp: TcpSocketSlab<C>,
    tcp_timers: BinaryHeap<TcpTimerEntry, Min, MAX_TCP_TIMER_ENTRIES>,
    tcp_timer_generations: [u32; MAX_TCP_SOCKETS],
    tcp_timer_deadlines: [Option<u64>; MAX_TCP_SOCKETS],
    tcp_receive_backpressured: [bool; MAX_TCP_SOCKETS],
    tcp_receive_backpressured_count: usize,
}

#[derive(Clone, Debug)]
struct OutboundFrameQueue {
    frames: [PacketBuffer; MAX_OUTBOUND_FRAMES],
    tcp_ack_keys: [Option<TcpAckReplaceKey>; MAX_OUTBOUND_FRAMES],
    ready: Deque<usize, MAX_OUTBOUND_FRAMES>,
    free: Deque<usize, MAX_OUTBOUND_FRAMES>,
}

#[derive(Clone)]
struct TcpListenerQueues {
    queues: [Option<TcpListenerQueue>; MAX_TCP_SOCKETS],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TcpAckReplaceKey {
    local: TcpEndpoint,
    remote: TcpEndpoint,
    sequence: u32,
    acknowledgement: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TcpResetResponse {
    local: TcpEndpoint,
    remote: TcpEndpoint,
    header: TcpHeader,
}

impl TcpAckReplaceKey {
    /// Whether a queued acknowledgement is made redundant by a newer one.
    ///
    /// Only a *strictly* newer acknowledgement supersedes: two frames
    /// carrying the same acknowledgement are duplicate ACKs, and their
    /// count is the loss signal the peer counts to three before it
    /// fast-retransmits. Folding them into one slot is what leaves a
    /// hole to be recovered by the peer's retransmission timer instead.
    fn can_replace(self, replacement: Self) -> bool {
        self.local == replacement.local
            && self.remote == replacement.remote
            && self.sequence == replacement.sequence
            && crate::tcp::sequence_lt(self.acknowledgement, replacement.acknowledgement)
    }
}

impl OutboundFrameQueue {
    fn new() -> Self {
        let mut free = Deque::new();
        for index in 0..MAX_OUTBOUND_FRAMES {
            free.push_back(index)
                .unwrap_or_else(|_| panic!("outbound frame free queue overflowed during init"));
        }
        Self {
            frames: core::array::from_fn(|_| PacketBuffer::new()),
            tcp_ack_keys: [None; MAX_OUTBOUND_FRAMES],
            ready: Deque::new(),
            free,
        }
    }

    fn reserve(&mut self) -> Result<usize, StackError> {
        self.free.pop_front().ok_or(StackError::OutputQueueFull)
    }

    fn frame_mut(&mut self, slot: usize) -> &mut PacketBuffer {
        self.frames
            .get_mut(slot)
            .unwrap_or_else(|| panic!("outbound frame slot {slot} is outside slab"))
    }

    fn commit(&mut self, slot: usize) {
        self.ready
            .push_back(slot)
            .unwrap_or_else(|_| panic!("outbound ready queue overflowed after slot reserve"));
    }

    fn release(&mut self, slot: usize) {
        self.frame_mut(slot).clear();
        self.tcp_ack_keys[slot] = None;
        self.free
            .push_back(slot)
            .unwrap_or_else(|_| panic!("outbound free queue overflowed after slot release"));
    }

    fn push_front(&mut self, frame: PacketBuffer) {
        let slot = self
            .reserve()
            .unwrap_or_else(|_| panic!("outbound frame queue had no slot for restore"));
        *self.frame_mut(slot) = frame;
        self.tcp_ack_keys[slot] = None;
        self.ready
            .push_front(slot)
            .unwrap_or_else(|_| panic!("outbound ready queue overflowed after frame restore"));
    }

    fn pop(&mut self) -> Option<PacketBuffer> {
        let slot = self.ready.pop_front()?;
        let frame = core::mem::take(self.frame_mut(slot));
        self.tcp_ack_keys[slot] = None;
        self.free
            .push_back(slot)
            .unwrap_or_else(|_| panic!("outbound free queue overflowed after frame pop"));
        Some(frame)
    }

    fn set_tcp_ack_key(&mut self, slot: usize, key: TcpAckReplaceKey) {
        self.tcp_ack_keys[slot] = Some(key);
    }

    fn replace_ready_tcp_ack(
        &mut self,
        key: TcpAckReplaceKey,
        encode: impl FnOnce(&mut PacketBuffer) -> Result<(), StackError>,
    ) -> Result<bool, StackError> {
        let mut replacement = None;
        for slot in self.ready.iter().copied() {
            if self.tcp_ack_keys[slot].is_some_and(|queued| queued.can_replace(key)) {
                replacement = Some(slot);
                break;
            }
        }
        let Some(slot) = replacement else {
            return Ok(false);
        };
        let frame = self.frame_mut(slot);
        frame.clear();
        encode(frame)?;
        self.tcp_ack_keys[slot] = Some(key);
        Ok(true)
    }

    fn try_submit_slices<E>(
        &mut self,
        limit: usize,
        submit: impl FnOnce(&[TxFrameRef<'_>]) -> Result<Option<usize>, E>,
    ) -> Result<OutboundBatchStatus, E> {
        if limit == 0 || self.ready.is_empty() {
            return Ok(OutboundBatchStatus::Empty);
        }

        let mut frames = ArrayVec::<TxFrameRef<'_>, MAX_OUTBOUND_FRAMES>::new();
        for slot in self.ready.iter().copied().take(limit) {
            frames
                .try_push(self.frames[slot].as_tx_frame())
                .unwrap_or_else(|_| panic!("outbound slice batch exceeded frame queue capacity"));
        }
        let offered = frames.len();
        let Some(accepted) = submit(frames.as_slice())? else {
            return Ok(OutboundBatchStatus::Deferred);
        };
        assert!(
            accepted <= offered,
            "network interface accepted more frames than offered"
        );
        drop(frames);

        let mut accepted_bytes = 0usize;
        for _ in 0..accepted {
            let slot = self
                .ready
                .pop_front()
                .expect("outbound ready queue lost submitted frame");
            accepted_bytes = accepted_bytes.saturating_add(self.frames[slot].as_slice().len());
            self.release(slot);
        }
        Ok(OutboundBatchStatus::Submitted {
            offered,
            accepted,
            accepted_bytes,
        })
    }
}

impl TcpListenerQueues {
    const fn new() -> Self {
        Self {
            queues: [None; MAX_TCP_SOCKETS],
        }
    }

    fn insert(&mut self, listener: SocketId, backlog: TcpListenBacklog) {
        let index = socket_index(listener);
        assert!(
            self.queues[index].is_none(),
            "TCP listener queue already exists for socket"
        );
        self.queues[index] = Some(TcpListenerQueue::new(backlog));
    }

    fn remove(&mut self, listener: SocketId) -> Option<TcpListenerQueue> {
        self.queues[socket_index(listener)].take()
    }

    fn reserve(&mut self, listener: SocketId) -> bool {
        self.queue_mut(listener).reserve()
    }

    fn release(&mut self, listener: SocketId) {
        self.queue_mut(listener).release();
    }

    fn queue_mut(&mut self, listener: SocketId) -> &mut TcpListenerQueue {
        self.queues[socket_index(listener)]
            .as_mut()
            .unwrap_or_else(|| panic!("TCP listener socket has no backlog queue"))
    }
}

impl fmt::Debug for TcpListenerQueues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let len = self.queues.iter().filter(|queue| queue.is_some()).count();
        formatter
            .debug_struct("TcpListenerQueues")
            .field("len", &len)
            .finish_non_exhaustive()
    }
}

impl Stack<BbrV3> {
    pub fn new(config: StackConfig) -> Self {
        Self::new_with_congestion(config, BbrV3::new(1460))
    }
}

impl<C> Stack<C>
where
    C: CongestionControl,
{
    pub fn new_with_congestion(config: StackConfig, congestion: C) -> Self {
        Self {
            config,
            congestion,
            routes: RouteTable::new(),
            path_mtu: PathMtuTable::new(),
            ipv4_reassembly: Ipv4ReassemblyCache::new(),
            ipv4_addresses: ArrayVec::new(),
            ipv6_addresses: ArrayVec::new(),
            ipv6_discovery: NeighborDiscovery::new(),
            ipv6_dns_servers: Ipv6DnsServers::new(),
            ipv4_multicast_memberships: ArrayVec::new(),
            neighbors: ArrayVec::new(),
            outbound: OutboundFrameQueue::new(),
            events: Deque::new(),
            udp: UdpSocketSlab::new(),
            udp_rx: Deque::new(),
            icmp_echo_replies: ArrayVec::new(),
            udp_rx_counts: [0; MAX_UDP_SOCKETS],
            tcp_accept: Deque::new(),
            tcp_listener_queues: TcpListenerQueues::new(),
            tcp_listener_children: [None; MAX_TCP_SOCKETS],
            tcp: TcpSocketSlab::new(),
            tcp_timers: BinaryHeap::new(),
            tcp_timer_generations: [0; MAX_TCP_SOCKETS],
            tcp_timer_deadlines: [None; MAX_TCP_SOCKETS],
            tcp_receive_backpressured: [false; MAX_TCP_SOCKETS],
            tcp_receive_backpressured_count: 0,
        }
    }

    pub const fn config(&self) -> StackConfig {
        self.config
    }

    pub fn receive_backpressured(&self) -> bool {
        self.tcp_receive_backpressured_count != 0
    }

    pub fn routes(&self) -> &RouteTable {
        &self.routes
    }

    pub fn routes_mut(&mut self) -> &mut RouteTable {
        &mut self.routes
    }

    pub fn replace_routes(&mut self, routes: RouteTable) {
        self.routes = routes;
    }

    pub fn neighbors(&self) -> impl Iterator<Item = NeighborEntry> + '_ {
        self.neighbors.iter().copied()
    }

    pub fn replace_neighbors(&mut self, neighbors: impl IntoIterator<Item = NeighborEntry>) {
        self.neighbors.clear();
        for entry in neighbors {
            self.neighbors
                .try_push(entry)
                .unwrap_or_else(|_| panic!("neighbor table is full"));
        }
    }

    pub fn learn_neighbor(&mut self, entry: NeighborEntry) {
        if let Some(existing) = self
            .neighbors
            .iter_mut()
            .find(|existing| existing.ip == entry.ip)
        {
            *existing = entry;
        } else {
            self.neighbors
                .try_push(entry)
                .unwrap_or_else(|_| panic!("neighbor table is full"));
        }
        Self::push_event_into(&mut self.events, StackEvent::NeighborUpdated(entry));
    }

    /// Records a neighbor observation that is only allowed to replace a
    /// cached link-layer address when `override_cache` is set.
    ///
    /// RFC 4861 §7.2.5: an observation that does not carry the override
    /// bit — a Neighbor Advertisement without it, or a passively
    /// observed data frame, which carries no NDP flags at all — may
    /// create a cache entry or refresh a matching one, but must leave a
    /// resolved entry with a different address alone.
    fn learn_neighbor_without_override(&mut self, entry: NeighborEntry, override_cache: bool) {
        let cached = self
            .neighbors
            .iter()
            .find(|existing| existing.ip == entry.ip)
            .copied();
        match cached {
            Some(cached)
                if cached.state != NeighborState::Incomplete
                    && cached.mac != entry.mac
                    && !override_cache => {}
            _ => self.learn_neighbor(entry),
        }
    }

    pub fn add_ipv4_address(&mut self, address: Ipv4Cidr) {
        if !self.ipv4_addresses.contains(&address) {
            self.ipv4_addresses
                .try_push(address)
                .unwrap_or_else(|_| panic!("IPv4 address table is full"));
            self.routes
                .add(Route {
                    destination: IpCidr::Ipv4(address),
                    gateway: None,
                    expires_at: None,
                })
                .unwrap_or_else(|_| panic!("route table is full"));
        }
    }

    pub fn remove_ipv4_address(&mut self, address: Ipv4Cidr) {
        if let Some(index) = self
            .ipv4_addresses
            .iter()
            .position(|existing| *existing == address)
        {
            self.ipv4_addresses.remove(index);
            self.ipv4_multicast_memberships
                .retain(|membership| membership.interface != address.address());
            self.prune_ipv4_reassembly_destinations();
        }
    }

    pub fn clear_ipv4_addresses(&mut self) {
        self.ipv4_addresses.clear();
        self.ipv4_multicast_memberships.clear();
        self.ipv4_reassembly.retain_destinations(|_| false);
    }

    pub fn join_ipv4_multicast(
        &mut self,
        group: Ipv4Address,
        interface: Ipv4Address,
    ) -> Result<(), StackError> {
        if !group.is_multicast() {
            return Err(StackError::InvalidMulticastGroup);
        }
        if !self.owns_ip_address(IpAddress::Ipv4(interface)) {
            return Err(StackError::MulticastInterfaceUnavailable);
        }
        if let Some(existing) = self
            .ipv4_multicast_memberships
            .iter_mut()
            .find(|membership| membership.group == group && membership.interface == interface)
        {
            existing.joins = existing
                .joins
                .checked_add(1)
                .expect("IPv4 multicast membership join count overflowed");
            return Ok(());
        }
        self.ipv4_multicast_memberships
            .try_push(Ipv4MulticastMembership {
                group,
                interface,
                joins: 1,
            })
            .map_err(|_| StackError::MulticastMembershipTableFull)
    }

    pub fn leave_ipv4_multicast(
        &mut self,
        group: Ipv4Address,
        interface: Ipv4Address,
    ) -> Result<(), StackError> {
        if !group.is_multicast() {
            return Err(StackError::InvalidMulticastGroup);
        }
        if !self.owns_ip_address(IpAddress::Ipv4(interface)) {
            return Err(StackError::MulticastInterfaceUnavailable);
        }
        let Some(index) = self
            .ipv4_multicast_memberships
            .iter()
            .position(|membership| membership.group == group && membership.interface == interface)
        else {
            return Err(StackError::MulticastMembershipNotFound);
        };
        let membership = &mut self.ipv4_multicast_memberships[index];
        membership.joins = membership
            .joins
            .checked_sub(1)
            .expect("IPv4 multicast membership join count underflowed");
        if membership.joins == 0 {
            self.ipv4_multicast_memberships.remove(index);
        }
        Ok(())
    }

    pub fn ipv4_multicast_memberships(&self) -> impl Iterator<Item = Ipv4MulticastMembership> + '_ {
        self.ipv4_multicast_memberships.iter().copied()
    }

    pub fn add_ipv6_address(&mut self, address: Ipv6Cidr) {
        if !self.ipv6_addresses.contains(&address) {
            self.ipv6_addresses
                .try_push(address)
                .unwrap_or_else(|_| panic!("IPv6 address table is full"));
            self.routes
                .add(Route {
                    destination: IpCidr::Ipv6(address),
                    gateway: None,
                    expires_at: None,
                })
                .unwrap_or_else(|_| panic!("route table is full"));
        }
    }

    pub fn remove_ipv6_address(&mut self, address: Ipv6Cidr) {
        if let Some(index) = self
            .ipv6_addresses
            .iter()
            .position(|existing| *existing == address)
        {
            self.ipv6_addresses.remove(index);
        }
    }

    pub fn clear_ipv6_addresses(&mut self) {
        self.ipv6_addresses.clear();
    }

    pub fn primary_ipv4_address(&self) -> Option<Ipv4Cidr> {
        self.ipv4_addresses.first().copied()
    }

    pub fn ipv4_addresses(&self) -> impl Iterator<Item = Ipv4Cidr> + '_ {
        self.ipv4_addresses.iter().copied()
    }

    pub fn replace_ipv4_addresses(&mut self, addresses: impl IntoIterator<Item = Ipv4Cidr>) {
        self.ipv4_addresses.clear();
        for address in addresses {
            self.ipv4_addresses
                .try_push(address)
                .unwrap_or_else(|_| panic!("IPv4 address table is full"));
        }
        let ipv4_addresses = self.ipv4_addresses.clone();
        self.ipv4_multicast_memberships.retain(|membership| {
            ipv4_addresses_own(ipv4_addresses.as_slice(), membership.interface)
        });
        self.prune_ipv4_reassembly_destinations();
    }

    pub fn primary_ipv6_address(&self) -> Option<Ipv6Cidr> {
        self.ipv6_addresses.first().copied()
    }

    pub fn ipv6_addresses(&self) -> impl Iterator<Item = Ipv6Cidr> + '_ {
        self.ipv6_addresses.iter().copied()
    }

    pub fn replace_ipv6_addresses(&mut self, addresses: impl IntoIterator<Item = Ipv6Cidr>) {
        self.ipv6_addresses.clear();
        for address in addresses {
            self.ipv6_addresses
                .try_push(address)
                .unwrap_or_else(|_| panic!("IPv6 address table is full"));
        }
    }

    /// Installs `fe80::/64` + modified EUI-64 for this interface's MAC.
    ///
    /// RFC 4862 §5.3 makes the link-local address a precondition for
    /// Neighbor Discovery: it is the source address of the router
    /// solicitations that start autoconfiguration, so the embedding
    /// kernel calls this once the interface's hardware address is known.
    pub fn configure_ipv6_link_local(&mut self) -> Ipv6Cidr {
        let address = Ipv6Cidr::new(
            Ipv6Address::link_local_from_mac(self.config.mac),
            Ipv6Address::LINK_LOCAL_PREFIX_LEN,
        );
        self.add_ipv6_address(address);
        address
    }

    /// The interface's link-local address, if one has been configured.
    pub fn ipv6_link_local_address(&self) -> Option<Ipv6Address> {
        self.ipv6_addresses
            .iter()
            .map(|cidr| cidr.address())
            .find(|address| address.is_link_local())
    }

    /// Whether a Router Advertisement has configured this interface.
    pub const fn ipv6_autoconfigured(&self) -> bool {
        self.ipv6_discovery.is_configured()
    }

    /// Recursive resolvers learned from Router Advertisements.
    pub fn ipv6_dns_servers(&self) -> impl Iterator<Item = Ipv6Address> + '_ {
        self.ipv6_dns_servers.iter().copied()
    }

    pub fn replace_ipv6_dns_servers(&mut self, servers: impl IntoIterator<Item = Ipv6Address>) {
        self.ipv6_dns_servers.clear();
        for server in servers {
            if self.ipv6_dns_servers.try_push(server).is_err() {
                break;
            }
        }
    }

    /// Returns the interface to the unconfigured state so router
    /// solicitation restarts.
    ///
    /// The kernel calls this when the link comes back: the router
    /// advertisement that configured the interface belongs to the link
    /// that just went away, and nothing else would ever solicit again.
    pub fn restart_ipv6_autoconfig(&mut self) {
        self.ipv6_discovery.reset();
    }

    /// Drives stateless address autoconfiguration one step: queues a
    /// Router Solicitation when one is due.
    ///
    /// Mirrors the DHCPv4 driver the kernel calls on its configuration
    /// poll. Both are pull-driven rather than timer-driven so the stack
    /// keeps no wall clock of its own; `now` comes from the caller.
    /// Solicitation stops once an advertisement arrives, so steady-state
    /// calls are free.
    pub fn drive_ipv6_autoconfig(&mut self, now: StackInstant) -> Result<(), StackError> {
        if !self.ipv6_discovery.poll_solicitation(now) {
            return Ok(());
        }
        // RFC 4861 §4.1: solicit from the link-local address when one is
        // configured, and only then may the Source Link-Layer Address
        // option appear.
        let source = self
            .ipv6_link_local_address()
            .unwrap_or(Ipv6Address::UNSPECIFIED);
        self.queue_icmpv6_router_solicitation(source)
    }

    fn queue_icmpv6_router_solicitation(&mut self, source: Ipv6Address) -> Result<(), StackError> {
        let local_mac = self.config.mac;
        let destination = Ipv6Address::ALL_ROUTERS_LINK_LOCAL;
        let destination_mac = ipv6_multicast_mac(destination);
        let source_mac = (!source.is_unspecified()).then_some(local_mac);
        let icmp_len = if source_mac.is_some() {
            Icmpv6Packet::ROUTER_SOLICITATION_LEN
        } else {
            Icmpv6Packet::ROUTER_SOLICITATION_ANONYMOUS_LEN
        };
        self.queue_outbound_frame(|frame| {
            let storage = frame.storage_mut();
            let mut offset = EthernetFrame::encode_header(
                storage,
                destination_mac,
                local_mac,
                EthernetProtocol::Ipv6,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Ipv6Packet::encode_header(
                &mut storage[offset..],
                source,
                destination,
                crate::IpProtocol::Icmpv6,
                icmp_len,
                255,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Icmpv6Packet::encode_router_solicitation(
                &mut storage[offset..],
                source,
                destination,
                source_mac,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            Ok(())
        })
    }

    /// Applies one Router Advertisement's configuration: SLAAC
    /// addresses, on-link prefixes, the default route, and the RDNSS
    /// resolver list.
    fn apply_ipv6_router_configuration(
        &mut self,
        configuration: Ipv6RouterConfiguration,
        now: StackInstant,
    ) {
        if let Some(router_mac) = configuration.router_mac {
            self.learn_neighbor(NeighborEntry {
                ip: IpAddress::Ipv6(configuration.router),
                mac: router_mac,
                state: NeighborState::Reachable,
                updated_at: now,
            });
        }

        for prefix in configuration.on_link_prefixes.iter().copied() {
            // A connected route: on-link destinations resolve through
            // Neighbor Discovery rather than the router.
            let _ = self.routes.add(Route {
                destination: IpCidr::Ipv6(prefix),
                gateway: None,
                expires_at: None,
            });
        }

        for address in configuration.addresses.iter().copied() {
            // A router that keeps advertising fresh prefixes must not be
            // able to overflow a fixed table, so a full table drops the
            // new address instead of asserting.
            if self.ipv6_addresses.is_full() && !self.ipv6_addresses.contains(&address) {
                continue;
            }
            self.add_ipv6_address(address);
        }

        let default_route = Route {
            destination: IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::UNSPECIFIED, 0)),
            gateway: Some(IpAddress::Ipv6(configuration.router)),
            expires_at: None,
        };
        if configuration.router_lifetime_seconds == 0 {
            self.routes.remove(default_route);
        } else {
            let _ = self.routes.add(default_route);
        }

        if !configuration.dns_servers.is_empty() {
            self.ipv6_dns_servers = configuration.dns_servers.clone();
        }

        if configuration.configures_interface() {
            self.ipv6_discovery.record_advertisement();
            Self::push_event_into(
                &mut self.events,
                StackEvent::Ipv6Autoconfigured(configuration),
            );
        }
    }

    pub fn source_ipv4_address_for(&self, destination: Ipv4Address) -> Option<Ipv4Address> {
        self.source_ipv4_for(destination)
    }

    pub fn source_ipv6_address_for(&self, destination: Ipv6Address) -> Option<Ipv6Address> {
        self.source_ipv6_for(destination)
    }

    fn prune_ipv4_reassembly_destinations(&mut self) {
        let ipv4_addresses = self.ipv4_addresses.clone();
        self.ipv4_reassembly.retain_destinations(|key| {
            ipv4_addresses_own(ipv4_addresses.as_slice(), key.destination)
        });
    }

    pub fn receive_frame(&mut self, frame: &[u8], now: StackInstant) -> Result<(), StackError> {
        self.receive_frame_with_backpressure(frame, now).map(|_| ())
    }

    pub fn receive_frame_with_backpressure(
        &mut self,
        frame: &[u8],
        now: StackInstant,
    ) -> Result<bool, StackError> {
        // Wrap the borrowed frame in an owning Bytes so segment
        // payloads further down can be passed as owning Bytes
        // without a per-segment copy. A caller that hands us a plain
        // slice has no device metadata to go with it, so the frame is
        // verified in software throughout.
        let frame_bytes = Bytes::copy_from_slice(frame);
        self.receive_rx_frame(RxFrame::new(frame_bytes), now)
    }

    /// Device receive entry. Takes ownership of the frame so segment
    /// payloads can be sliced (zero-copy) into TCP receive queues, and
    /// carries the per-frame offload metadata the device reported so the
    /// protocol layers can decide what still has to be verified in
    /// software.
    pub fn receive_rx_frame(
        &mut self,
        frame: RxFrame,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        let RxFrame {
            bytes: frame,
            offload,
        } = frame;
        let parsed = EthernetFrame::parse(frame.as_ref()).ok_or(StackError::MalformedPacket)?;
        // We need both the owning Bytes (for slicing payloads to
        // owning Bytes downstream) and the borrowed view (for
        // parser dispatch). Keep the same `frame` value alive for
        // the duration of the call.
        let payload_range = bytes_subrange(frame.as_ref(), parsed.payload);
        let payload_bytes = frame.slice(payload_range.clone());
        let offload = offload.advance(payload_range.start);
        match parsed.protocol {
            EthernetProtocol::Ipv4 => {
                self.receive_ipv4(parsed.source, &payload_bytes, offload, now)
            }
            EthernetProtocol::Ipv6 => {
                self.receive_ipv6(parsed.source, &payload_bytes, offload, now)
            }
            EthernetProtocol::Arp => self.receive_arp(parsed.source, parsed.payload, now),
        }
    }

    pub fn take_outbound(&mut self) -> Option<PacketBuffer> {
        self.outbound.pop()
    }

    pub fn push_outbound_front(&mut self, frame: PacketBuffer) {
        self.outbound.push_front(frame);
    }

    pub fn try_submit_outbound_slices<E>(
        &mut self,
        limit: usize,
        submit: impl FnOnce(&[TxFrameRef<'_>]) -> Result<Option<usize>, E>,
    ) -> Result<OutboundBatchStatus, E> {
        self.outbound.try_submit_slices(limit, submit)
    }

    fn queue_outbound_frame<R>(
        &mut self,
        encode: impl FnOnce(&mut PacketBuffer) -> Result<R, StackError>,
    ) -> Result<R, StackError> {
        let slot = self.outbound.reserve()?;
        let frame = self.outbound.frame_mut(slot);
        frame.set_tx_checksum(None);
        match encode(frame) {
            Ok(result) => {
                self.outbound.commit(slot);
                Ok(result)
            }
            Err(error) => {
                self.outbound.release(slot);
                Err(error)
            }
        }
    }

    fn queue_tcp_ack_frame(
        &mut self,
        key: TcpAckReplaceKey,
        encode: impl Fn(&mut PacketBuffer) -> Result<(), StackError>,
    ) -> Result<bool, StackError> {
        if self
            .outbound
            .replace_ready_tcp_ack(key, |frame| encode(frame))?
        {
            return Ok(true);
        }
        let slot = self.outbound.reserve()?;
        let frame = self.outbound.frame_mut(slot);
        frame.set_tx_checksum(None);
        match encode(frame) {
            Ok(()) => {
                self.outbound.set_tcp_ack_key(slot, key);
                self.outbound.commit(slot);
                Ok(true)
            }
            Err(error) => {
                self.outbound.release(slot);
                Err(error)
            }
        }
    }

    pub fn take_event(&mut self) -> Option<StackEvent> {
        self.events.pop_front()
    }

    fn push_event_into(events: &mut Deque<StackEvent, MAX_STACK_EVENTS>, event: StackEvent) {
        match event {
            StackEvent::NeighborUpdated(entry) => {
                if let Some(existing) = events.iter_mut().find_map(|event| match event {
                    StackEvent::NeighborUpdated(existing) if existing.ip == entry.ip => {
                        Some(existing)
                    }
                    _ => None,
                }) {
                    *existing = entry;
                    return;
                }
                events
                    .push_back(StackEvent::NeighborUpdated(entry))
                    .unwrap_or_else(|_| panic!("stack event queue is full"));
            }
            StackEvent::UdpDatagram { socket } => {
                if events
                    .iter()
                    .any(|event| matches!(event, StackEvent::UdpDatagram { socket: queued } if *queued == socket))
                {
                    return;
                }
                events
                    .push_back(StackEvent::UdpDatagram { socket })
                    .unwrap_or_else(|_| panic!("stack event queue is full"));
            }
            StackEvent::TcpReadable { socket } => {
                if events
                    .iter()
                    .any(|event| matches!(event, StackEvent::TcpReadable { socket: queued } if *queued == socket))
                {
                    return;
                }
                events
                    .push_back(StackEvent::TcpReadable { socket })
                    .unwrap_or_else(|_| panic!("stack event queue is full"));
            }
            event => events
                .push_back(event)
                .unwrap_or_else(|_| panic!("stack event queue is full")),
        }
    }

    pub fn open_udp(&mut self, binding: UdpSocketBinding) -> Result<UdpSocketId, StackError> {
        self.udp.insert(binding)
    }

    pub fn rebind_udp(
        &mut self,
        socket: UdpSocketId,
        binding: UdpSocketBinding,
    ) -> Result<(), StackError> {
        self.udp.rebind(socket, binding)?;
        self.clear_udp_receive_queue(socket)?;
        Ok(())
    }

    pub fn remove_udp_socket(&mut self, socket: UdpSocketId) -> Result<(), StackError> {
        self.clear_udp_receive_queue(socket)?;
        self.udp
            .remove(udp_socket_index(socket))
            .ok_or(StackError::UnknownSocket)?;
        Ok(())
    }

    pub fn udp_binding_free(&self, binding: UdpSocketBinding) -> bool {
        self.udp.binding_free(binding)
    }

    pub fn take_udp(&mut self, socket: UdpSocketId) -> Result<Option<UdpReceive>, StackError> {
        self.udp_socket(socket)?;
        let index = udp_socket_index(socket);
        if self.udp_rx_counts[index] == 0 {
            return Ok(None);
        }
        if self
            .udp_rx
            .front()
            .is_some_and(|queued| queued.socket == socket)
        {
            let queued = self
                .udp_rx
                .pop_front()
                .expect("UDP receive queue front disappeared");
            self.udp_rx_counts[index] = self.udp_rx_counts[index]
                .checked_sub(1)
                .expect("UDP receive queue socket count underflowed");
            return Ok(Some(queued.datagram));
        }
        let len = self.udp_rx.len();
        for _ in 0..len {
            let queued = self
                .udp_rx
                .pop_front()
                .expect("UDP receive queue length changed while rotating");
            if queued.socket == socket {
                self.udp_rx_counts[index] = self.udp_rx_counts[index]
                    .checked_sub(1)
                    .expect("UDP receive queue socket count underflowed");
                return Ok(Some(queued.datagram));
            }
            self.udp_rx
                .push_back(queued)
                .unwrap_or_else(|_| panic!("UDP receive queue lost capacity while rotating"));
        }
        panic!("UDP receive socket count had no matching queued datagram");
    }

    pub fn take_udp_error(
        &mut self,
        socket: UdpSocketId,
    ) -> Result<Option<UdpSocketError>, StackError> {
        self.udp.take_pending_error(socket)
    }

    pub fn open_tcp_connect(
        &mut self,
        local: TcpEndpoint,
        remote: TcpEndpoint,
        initial_sequence: u32,
    ) -> Result<SocketId, StackError> {
        self.ensure_tcp_connect_unique(local, remote)?;
        let socket = TcpSocket::connect(local, remote, initial_sequence, self.congestion.clone());
        Ok(self.insert_tcp(socket))
    }

    pub fn open_tcp_listen(
        &mut self,
        local: TcpEndpoint,
        backlog: TcpListenBacklog,
    ) -> Result<SocketId, StackError> {
        self.ensure_tcp_listen_unique(local)?;
        if backlog.capacity() > MAX_TCP_ACCEPT {
            return Err(StackError::InvalidListenBacklog);
        }
        let socket = TcpSocket::listen(local, self.congestion.clone());
        let socket = self.insert_tcp(socket);
        self.tcp_listener_queues.insert(socket, backlog);
        Ok(socket)
    }

    pub fn tcp_connect_state(&self, socket: SocketId) -> Result<TcpConnectState, StackError> {
        let index = socket_index(socket);
        let socket = self.tcp_socket(socket)?;
        Ok(match socket.state() {
            crate::TcpState::Established => TcpConnectState::Connected,
            crate::TcpState::Closed | crate::TcpState::TimeWait => TcpConnectState::Closed(
                self.tcp
                    .terminal_error(index)
                    .unwrap_or(TcpConnectTerminalError::Closed),
            ),
            _ => TcpConnectState::Pending,
        })
    }

    /// Report whether `listener` has a completed connection waiting in the
    /// accept queue, without dequeuing it.
    ///
    /// `take_tcp_accept` is destructive, so readiness reporting (`poll`,
    /// `epoll`) cannot use it: observing a listener must not consume the
    /// connection the caller is about to accept.
    pub fn tcp_accept_pending(&self, listener: SocketId) -> Result<bool, StackError> {
        self.tcp_socket(listener)?;
        Ok(self
            .tcp_accept
            .iter()
            .any(|queued| queued.listener == listener))
    }

    /// Report whether `socket` would accept bytes from `queue_send_bytes`.
    ///
    /// `tcp_connect_state` folds every non-established state into `Pending`,
    /// which loses the distinction readiness reporting needs: a peer that
    /// half-closed (`CloseWait`) stops delivering data but still accepts
    /// ours, so the socket remains writable. A full transmit queue parks
    /// writability until ACKs drain capacity — reporting writable there
    /// would spin poll/epoll callers whose next write accepts zero bytes.
    /// Hop limit of the socket behind a transmit-queue index.
    ///
    /// The drive loop batches frames by index and queues them after the
    /// per-socket borrow ends, so the option is read back here rather than
    /// carried through every batch tuple.
    fn tcp_socket_hop_limit(&self, index: usize) -> u8 {
        self.tcp
            .get(index)
            .map_or(crate::DEFAULT_HOP_LIMIT, TcpSocket::hop_limit)
    }

    /// IPv4 TTL / IPv6 hop limit a TCP socket stamps on its frames.
    pub fn tcp_hop_limit(&self, socket: SocketId) -> Result<u8, StackError> {
        Ok(self.tcp_socket(socket)?.hop_limit())
    }

    /// Sets the TTL / hop limit for a TCP socket's outgoing frames.
    ///
    /// A listener passes its value on to every connection it accepts, so
    /// setting it before `listen` covers the SYN-ACK and everything after.
    pub fn set_tcp_hop_limit(&mut self, socket: SocketId, hop_limit: u8) -> Result<(), StackError> {
        self.tcp_socket_mut(socket)?.set_hop_limit(hop_limit);
        Ok(())
    }

    /// Hop limit a UDP socket stamps on its datagrams.
    pub fn udp_hop_limit(&self, socket: UdpSocketId) -> Result<u8, StackError> {
        Ok(self.udp_socket(socket)?.hop_limit)
    }

    /// Sets the TTL / hop limit for a UDP socket's outgoing datagrams.
    pub fn set_udp_hop_limit(
        &mut self,
        socket: UdpSocketId,
        hop_limit: u8,
    ) -> Result<(), StackError> {
        let index = udp_socket_index(socket);
        self.udp_socket(socket)?;
        self.udp
            .get_mut(index)
            .expect("UDP socket disappeared after a successful lookup")
            .hop_limit = hop_limit;
        Ok(())
    }

    pub fn tcp_send_ready(&self, socket: SocketId) -> Result<bool, StackError> {
        let socket = self.tcp_socket(socket)?;
        Ok(matches!(
            socket.state(),
            crate::TcpState::Established | crate::TcpState::CloseWait
        ) && socket.send_capacity_bytes() != 0)
    }

    /// Report whether `socket` has a datagram waiting, without dequeuing it.
    ///
    /// The counterpart of `tcp_accept_pending` for UDP: `take_udp` consumes
    /// the datagram, so readiness reporting needs this non-destructive view.
    pub fn udp_receive_pending(&self, socket: UdpSocketId) -> Result<bool, StackError> {
        self.udp_socket(socket)?;
        Ok(self.udp_rx_counts[udp_socket_index(socket)] != 0)
    }

    pub fn next_tcp_deadline(&mut self) -> Option<StackInstant> {
        loop {
            let entry = self.tcp_timers.peek().copied()?;
            if self.tcp_timer_entry_current(entry) {
                return Some(StackInstant::from_nanos(entry.deadline_nanos));
            }
            let _ = self.tcp_timers.pop();
        }
    }

    pub fn drive_tcp(&mut self, now: StackInstant) -> Result<(), StackError> {
        let mut pending_syn =
            ArrayVec::<(usize, TcpEndpoint, TcpEndpoint, TcpHeader, TcpHeaderOptions), 16>::new();
        let mut pending_syn_ack =
            ArrayVec::<(usize, TcpEndpoint, TcpEndpoint, TcpHeader, TcpHeaderOptions), 16>::new();
        let mut pending_ack = ArrayVec::<
            (
                usize,
                TcpEndpoint,
                TcpEndpoint,
                TcpHeader,
                TcpHeaderOptions,
                u8,
            ),
            16,
        >::new();
        let mut pending_retransmit = ArrayVec::<(usize, TcpTransmitSegment), 16>::new();
        let mut pending_data = ArrayVec::<(usize, TcpTransmitSegment), 16>::new();
        for active_slot in 0..self.tcp.active_len() {
            let index = self.tcp.active_index(active_slot);
            let (deadline, closed) = {
                let segment_budget = {
                    let Some(socket) = self.tcp.get(index) else {
                        panic!("TCP active socket index referenced a missing socket");
                    };
                    socket
                        .local_endpoint()
                        .zip(socket.remote_endpoint())
                        .map(|(local, remote)| self.tcp_segment_budget_for(local, remote))
                };
                let Some(socket) = self.tcp.get_mut(index) else {
                    panic!("TCP active socket index referenced a missing socket");
                };
                socket.expire_timers(now.nanos());
                let closed = socket.state() == crate::TcpState::Closed;
                if !closed
                    && let (Some(local), Some(remote)) =
                        (socket.local_endpoint(), socket.remote_endpoint())
                {
                    let segment_budget =
                        segment_budget.expect("TCP connected socket lost PMTU endpoints");
                    let retransmitting = {
                        if let Some(segment) = socket.pending_retransmission_with_mtu(
                            now.nanos(),
                            segment_budget.wire_segment(),
                        ) {
                            pending_retransmit
                                .try_push((index, segment))
                                .unwrap_or_else(|_| panic!("TCP retransmit burst overflowed"));
                            true
                        } else {
                            false
                        }
                    };
                    if !retransmitting {
                        if let Some(header) = socket.pending_syn() {
                            pending_syn
                                .try_push((
                                    index,
                                    local,
                                    remote,
                                    header,
                                    socket.pending_syn_options(now.nanos()),
                                ))
                                .unwrap_or_else(|_| {
                                    panic!("TCP control transmit burst overflowed")
                                });
                        }
                        if let Some(header) = socket.pending_syn_ack() {
                            pending_syn_ack
                                .try_push((
                                    index,
                                    local,
                                    remote,
                                    header,
                                    socket.pending_syn_ack_options(now.nanos()),
                                ))
                                .unwrap_or_else(|_| {
                                    panic!("TCP SYN-ACK transmit burst overflowed")
                                });
                        }
                        if let Some(segment) =
                            socket.take_transmit_segment(now.nanos(), segment_budget)
                        {
                            pending_data
                                .try_push((index, segment))
                                .unwrap_or_else(|_| panic!("TCP data transmit burst overflowed"));
                        }
                    }
                    if let Some(header) = socket.pending_ack() {
                        pending_ack
                            .try_push((
                                index,
                                local,
                                remote,
                                header,
                                socket.pending_ack_options(now.nanos()),
                                socket.pending_ack_count(),
                            ))
                            .unwrap_or_else(|_| panic!("TCP ACK transmit burst overflowed"));
                    }
                }
                (socket.next_deadline_nanos(), closed)
            };
            if closed && self.tcp.terminal_error(index).is_none() {
                self.tcp.remove_indices(index);
                self.release_tcp_listener_child(index);
                self.remove_tcp_accepts_for(socket_id(index));
            }
            self.schedule_tcp_timer_deadline(index, deadline);
        }

        for (index, local, remote, header, options) in pending_syn {
            let hop_limit = self.tcp_socket_hop_limit(index);
            if self.queue_tcp(
                local,
                remote,
                TcpEgress {
                    header,
                    options,
                    payload: TcpTxPayload::Flat(&[]),
                    hop_limit,
                },
                index as u16,
                now,
            )? {
                let socket = self
                    .tcp
                    .get_mut(index)
                    .expect("TCP socket disappeared while queuing SYN");
                socket.mark_syn_queued(now.nanos());
                self.schedule_tcp_timer(index);
            }
        }
        for (index, local, remote, header, options) in pending_syn_ack {
            let hop_limit = self.tcp_socket_hop_limit(index);
            if self.queue_tcp(
                local,
                remote,
                TcpEgress {
                    header,
                    options,
                    payload: TcpTxPayload::Flat(&[]),
                    hop_limit,
                },
                index as u16,
                now,
            )? {
                let socket = self
                    .tcp
                    .get_mut(index)
                    .expect("TCP socket disappeared while queuing SYN-ACK");
                socket.mark_syn_ack_queued(now.nanos());
                self.schedule_tcp_timer(index);
            }
        }
        for (index, local, remote, header, options, acks) in pending_ack {
            let hop_limit = self.tcp_socket_hop_limit(index);
            // Every owed acknowledgement gets its own frame. All but the
            // first repeat the same acknowledgement number, which is
            // exactly the duplicate-ACK signal the peer counts before it
            // retransmits a hole ahead of its own timer.
            for ack in 0..acks {
                let queued = match self.queue_tcp(
                    local,
                    remote,
                    TcpEgress {
                        header,
                        options,
                        payload: TcpTxPayload::Flat(&[]),
                        hop_limit,
                    },
                    index as u16,
                    now,
                ) {
                    Ok(queued) => queued,
                    // The first acknowledgement is what the peer is
                    // owed; the duplicates behind it are a loss hint,
                    // and must not take the last outbound slot from the
                    // retransmission they are asking for.
                    Err(StackError::OutputQueueFull) if ack != 0 => break,
                    Err(error) => return Err(error),
                };
                if !queued {
                    break;
                }
                let socket = self
                    .tcp
                    .get_mut(index)
                    .expect("TCP socket disappeared while queuing ACK");
                socket.mark_ack_queued();
                self.schedule_tcp_timer(index);
            }
        }
        for (index, segment) in pending_retransmit {
            let sequence = segment.header.sequence;
            let identification = segment.sequence_len as u16;
            let hop_limit = self.tcp_socket_hop_limit(index);
            if self.queue_tcp(
                segment.local,
                segment.remote,
                TcpEgress {
                    header: segment.header,
                    options: segment.options,
                    payload: self.tcp_tx_payload(&segment),
                    hop_limit,
                },
                identification,
                now,
            )? {
                let socket = self
                    .tcp
                    .get_mut(index)
                    .expect("TCP socket disappeared while queuing retransmission");
                socket.mark_retransmission_queued(sequence, now.nanos());
                self.schedule_tcp_timer(index);
            }
        }
        for (index, segment) in pending_data {
            let identification = segment.sequence_len as u16;
            let hop_limit = self.tcp_socket_hop_limit(index);
            self.queue_tcp(
                segment.local,
                segment.remote,
                TcpEgress {
                    header: segment.header,
                    options: segment.options,
                    payload: self.tcp_tx_payload(&segment),
                    hop_limit,
                },
                identification,
                now,
            )?;
            self.schedule_tcp_timer(index);
        }
        Ok(())
    }

    pub fn take_tcp_accept(&mut self, listener: SocketId) -> Result<Option<TcpAccept>, StackError> {
        self.tcp_socket(listener)?;
        let len = self.tcp_accept.len();
        for _ in 0..len {
            let queued = self
                .tcp_accept
                .pop_front()
                .expect("TCP accept queue length changed while rotating");
            if queued.listener == listener {
                self.tcp_listener_queues.release(listener);
                self.tcp_listener_children[socket_index(queued.accepted.socket)] = None;
                return Ok(Some(queued.accepted));
            }
            self.tcp_accept
                .push_back(queued)
                .unwrap_or_else(|_| panic!("TCP accept queue lost capacity while rotating"));
        }
        Ok(None)
    }

    pub fn remove_tcp_socket(&mut self, socket: SocketId) -> Result<(), StackError> {
        let index = socket_index(socket);
        let listener = self.tcp_listener_children[index].take();
        let removing_listener = self.tcp_socket(socket)?.remote_endpoint().is_none();
        if removing_listener {
            self.remove_tcp_listener_children(socket);
            self.remove_tcp_accepts_by_listener(socket);
        }
        let removed = self.tcp.remove(index).ok_or(StackError::UnknownSocket)?;
        self.clear_tcp_timer(index);
        self.clear_tcp_receive_backpressure(index);
        if removed.remote_endpoint().is_none() {
            self.tcp_listener_queues.remove(socket);
        } else if let Some(listener) = listener {
            self.tcp_listener_queues.release(listener);
        }
        self.tcp_listener_children[index] = None;
        self.remove_tcp_accepts_for(socket);
        Ok(())
    }

    pub fn tcp_send(&mut self, socket: SocketId, bytes: &[u8]) -> Result<usize, StackError> {
        Ok(self.tcp_socket_mut(socket)?.queue_send(bytes))
    }

    pub fn tcp_send_bytes(
        &mut self,
        socket: SocketId,
        bytes: &mut Bytes,
    ) -> Result<usize, StackError> {
        Ok(self.tcp_socket_mut(socket)?.queue_send_bytes(bytes))
    }

    pub fn tcp_shutdown_send(&mut self, socket: SocketId) -> Result<(), StackError> {
        let index = socket_index(socket);
        self.tcp_socket_mut(socket)?.close_send();
        self.schedule_tcp_timer(index);
        Ok(())
    }

    pub fn tcp_read(
        &mut self,
        socket: SocketId,
        max_bytes: usize,
        now: StackInstant,
    ) -> Result<TcpReadState, StackError> {
        let index = socket_index(socket);
        let socket = self.tcp_socket_mut(socket)?;
        let state = match socket.receive(max_bytes, now.nanos()) {
            Some(bytes) => TcpReadState::Data(bytes),
            None if socket.state() == crate::TcpState::CloseWait => TcpReadState::Eof,
            None => TcpReadState::Pending,
        };
        self.schedule_tcp_timer(index);
        self.update_tcp_receive_backpressure(index);
        Ok(state)
    }

    pub fn tcp_read_into<S>(
        &mut self,
        socket: SocketId,
        sink: &mut S,
        now: StackInstant,
    ) -> Result<TcpReadIntoState, StackError>
    where
        S: TcpReceiveBuffer,
    {
        let index = socket_index(socket);
        let socket = self.tcp_socket_mut(socket)?;
        let state = match socket.receive_into(sink, now.nanos()) {
            Some(bytes) => TcpReadIntoState::Data(bytes),
            None if socket.state() == crate::TcpState::CloseWait => TcpReadIntoState::Eof,
            None => TcpReadIntoState::Pending,
        };
        self.schedule_tcp_timer(index);
        self.update_tcp_receive_backpressure(index);
        Ok(state)
    }

    pub fn send_udp(
        &mut self,
        socket: UdpSocketId,
        destination: IpAddress,
        destination_port: u16,
        payload: &[u8],
        identification: u16,
        now: StackInstant,
    ) -> Result<usize, StackError> {
        let socket_state = self.udp_socket(socket)?;
        let binding = socket_state.binding;
        let hop_limit = socket_state.hop_limit;
        if binding
            .remote
            .is_some_and(|remote| remote.address != destination || remote.port != destination_port)
        {
            return Err(StackError::RemoteAddressMismatch);
        }
        let datagram = UdpEgress {
            source_port: binding.local_port,
            destination_port,
            payload,
            hop_limit,
        };
        match (binding.local_address, destination) {
            (Some(IpAddress::Ipv4(source)), IpAddress::Ipv4(destination)) => {
                self.send_udp_ipv4_from(source, destination, datagram, identification, now)
            }
            (Some(IpAddress::Ipv6(source)), IpAddress::Ipv6(destination)) => {
                self.send_udp_ipv6_from(source, destination, datagram, now)
            }
            (None, IpAddress::Ipv4(destination)) => {
                let Some(source) = self.source_ipv4_for(destination) else {
                    return Err(StackError::Unroutable);
                };
                self.send_udp_ipv4_from(source, destination, datagram, identification, now)
            }
            (None, IpAddress::Ipv6(destination)) => {
                let Some(source) = self.source_ipv6_for(destination) else {
                    return Err(StackError::Unroutable);
                };
                self.send_udp_ipv6_from(source, destination, datagram, now)
            }
            (Some(_), _) => Err(StackError::AddressFamilyMismatch),
        }
    }

    /// Sends an IPv4 datagram from the source address the routing table
    /// picks for `destination`.
    pub fn send_udp_ipv4(
        &mut self,
        destination: Ipv4Address,
        datagram: UdpEgress<'_>,
        identification: u16,
        now: StackInstant,
    ) -> Result<usize, StackError> {
        let source = self
            .source_ipv4_for(destination)
            .ok_or(StackError::Unroutable)?;
        self.send_udp_ipv4_from(source, destination, datagram, identification, now)
    }

    /// Sends an IPv4 datagram from an explicit source address.
    ///
    /// The datagram carries its own TTL, so a guest's
    /// `set-unicast-hop-limit` reaches the wire through the same path
    /// that carries the stack default.
    pub fn send_udp_ipv4_from(
        &mut self,
        source: Ipv4Address,
        destination: Ipv4Address,
        datagram: UdpEgress<'_>,
        identification: u16,
        now: StackInstant,
    ) -> Result<usize, StackError> {
        let next_hop = self.next_hop(IpAddress::Ipv4(destination));
        let next_hop = match next_hop {
            Some(IpAddress::Ipv4(next_hop)) => next_hop,
            Some(IpAddress::Ipv6(_)) => panic!("IPv4 route resolved to IPv6 next hop"),
            None => destination,
        };
        let destination_mac = if destination == Ipv4Address::BROADCAST {
            [0xff; 6]
        } else if destination.is_multicast() {
            ipv4_multicast_mac(destination)
        } else {
            let Some(destination_mac) = self.neighbor_mac(IpAddress::Ipv4(next_hop)) else {
                self.queue_arp_request(source, next_hop, now)?;
                return Ok(0);
            };
            destination_mac
        };
        self.queue_udp_ipv4(
            FramePath {
                destination_mac,
                local_mac: self.config.mac,
                source,
                destination,
            },
            datagram,
            identification,
        )
    }

    /// Queues one ICMP echo request to `destination`, choosing ICMPv4 or
    /// ICMPv6 from the address family.
    ///
    /// Returns the number of payload bytes queued, or `0` when the next
    /// hop's link-layer address is still unknown: an ARP request or a
    /// neighbour solicitation went out in its place and the caller
    /// repeats the send once the neighbour answers, exactly as the UDP
    /// send path does.
    pub fn send_icmp_echo_request(
        &mut self,
        destination: IpAddress,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
        identification: u16,
        now: StackInstant,
    ) -> Result<usize, StackError> {
        match destination {
            IpAddress::Ipv4(destination) => self.send_icmpv4_echo_request(
                destination,
                identifier,
                sequence,
                payload,
                identification,
                now,
            ),
            IpAddress::Ipv6(destination) => {
                self.send_icmpv6_echo_request(destination, identifier, sequence, payload, now)
            }
        }
    }

    fn send_icmpv4_echo_request(
        &mut self,
        destination: Ipv4Address,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
        identification: u16,
        now: StackInstant,
    ) -> Result<usize, StackError> {
        let source = self
            .source_ipv4_for(destination)
            .ok_or(StackError::Unroutable)?;
        let next_hop = match self.next_hop(IpAddress::Ipv4(destination)) {
            Some(IpAddress::Ipv4(next_hop)) => next_hop,
            Some(IpAddress::Ipv6(_)) => panic!("IPv4 route resolved to IPv6 next hop"),
            None => destination,
        };
        let Some(destination_mac) = self.neighbor_mac(IpAddress::Ipv4(next_hop)) else {
            self.queue_arp_request(source, next_hop, now)?;
            return Ok(0);
        };
        let icmp_len = Icmpv4Packet::HEADER_LEN
            .checked_add(payload.len())
            .ok_or(StackError::PacketTooLarge)?;
        self.ensure_path_packet_fits(
            PathMtuKey::new(IpAddress::Ipv4(source), IpAddress::Ipv4(destination)),
            EthernetFrame::HEADER_LEN + Ipv4Packet::MIN_HEADER_LEN,
            icmp_len,
        )?;
        let local_mac = self.config.mac;
        self.queue_outbound_frame(|frame| {
            let storage = frame.storage_mut();
            let mut offset = EthernetFrame::encode_header(
                storage,
                destination_mac,
                local_mac,
                EthernetProtocol::Ipv4,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Ipv4Packet::encode_header(
                &mut storage[offset..],
                source,
                destination,
                crate::IpProtocol::Icmp,
                icmp_len,
                identification,
                crate::DEFAULT_HOP_LIMIT,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Icmpv4Packet::encode_echo_request(
                &mut storage[offset..],
                identifier,
                sequence,
                payload,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            Ok(payload.len())
        })
    }

    fn send_icmpv6_echo_request(
        &mut self,
        destination: Ipv6Address,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
        now: StackInstant,
    ) -> Result<usize, StackError> {
        let source = self
            .source_ipv6_for(destination)
            .ok_or(StackError::Unroutable)?;
        let next_hop = match self.next_hop(IpAddress::Ipv6(destination)) {
            Some(IpAddress::Ipv6(next_hop)) => next_hop,
            Some(IpAddress::Ipv4(_)) => panic!("IPv6 route resolved to IPv4 next hop"),
            None => destination,
        };
        let destination_mac = if destination.is_multicast() {
            ipv6_multicast_mac(destination)
        } else {
            let Some(destination_mac) = self.neighbor_mac(IpAddress::Ipv6(next_hop)) else {
                self.queue_ndp_solicitation(source, next_hop, now)?;
                return Ok(0);
            };
            destination_mac
        };
        let icmp_len = Icmpv6Packet::ECHO_HEADER_LEN
            .checked_add(payload.len())
            .ok_or(StackError::PacketTooLarge)?;
        self.ensure_path_packet_fits(
            PathMtuKey::new(IpAddress::Ipv6(source), IpAddress::Ipv6(destination)),
            EthernetFrame::HEADER_LEN + Ipv6Packet::HEADER_LEN,
            icmp_len,
        )?;
        let local_mac = self.config.mac;
        self.queue_outbound_frame(|frame| {
            let storage = frame.storage_mut();
            let mut offset = EthernetFrame::encode_header(
                storage,
                destination_mac,
                local_mac,
                EthernetProtocol::Ipv6,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Ipv6Packet::encode_header(
                &mut storage[offset..],
                source,
                destination,
                crate::IpProtocol::Icmpv6,
                icmp_len,
                crate::DEFAULT_HOP_LIMIT,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Icmpv6Packet::encode_echo_request(
                &mut storage[offset..],
                source,
                destination,
                identifier,
                sequence,
                payload,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            Ok(payload.len())
        })
    }

    /// Claims the reply to one outstanding echo exchange, if it has
    /// arrived. A reply nobody is waiting for ages out of the buffer
    /// rather than being reported to the next caller.
    pub fn take_icmp_echo_reply(&mut self, key: IcmpEchoKey) -> Option<IcmpEchoReply> {
        let index = self
            .icmp_echo_replies
            .iter()
            .position(|reply| reply.key == key)?;
        Some(self.icmp_echo_replies.remove(index))
    }

    /// Banks a received echo reply for whichever waiter owns its key.
    fn record_icmp_echo_reply(&mut self, key: IcmpEchoKey, payload_bytes: usize) {
        if self.icmp_echo_replies.is_full() {
            self.icmp_echo_replies.remove(0);
        }
        self.icmp_echo_replies.push(IcmpEchoReply {
            key,
            payload_bytes: u16::try_from(payload_bytes).unwrap_or(u16::MAX),
        });
    }

    /// Sends an IPv6 datagram from the source address the routing table
    /// picks for `destination`.
    pub fn send_udp_ipv6(
        &mut self,
        destination: Ipv6Address,
        datagram: UdpEgress<'_>,
        now: StackInstant,
    ) -> Result<usize, StackError> {
        let source = self
            .source_ipv6_for(destination)
            .ok_or(StackError::Unroutable)?;
        self.send_udp_ipv6_from(source, destination, datagram, now)
    }

    /// Sends an IPv6 datagram from an explicit source address, with the
    /// hop limit the datagram carries.
    pub fn send_udp_ipv6_from(
        &mut self,
        source: Ipv6Address,
        destination: Ipv6Address,
        datagram: UdpEgress<'_>,
        now: StackInstant,
    ) -> Result<usize, StackError> {
        let next_hop = self.next_hop(IpAddress::Ipv6(destination));
        let next_hop = match next_hop {
            Some(IpAddress::Ipv6(next_hop)) => next_hop,
            Some(IpAddress::Ipv4(_)) => panic!("IPv6 route resolved to IPv4 next hop"),
            None => destination,
        };
        let destination_mac = if destination.is_multicast() {
            ipv6_multicast_mac(destination)
        } else {
            let Some(destination_mac) = self.neighbor_mac(IpAddress::Ipv6(next_hop)) else {
                self.queue_ndp_solicitation(source, next_hop, now)?;
                return Ok(0);
            };
            destination_mac
        };
        self.queue_udp_ipv6(
            FramePath {
                destination_mac,
                local_mac: self.config.mac,
                source,
                destination,
            },
            datagram,
        )
    }

    fn queue_udp_ipv4(
        &mut self,
        path: FramePath<Ipv4Address>,
        datagram: UdpEgress<'_>,
        identification: u16,
    ) -> Result<usize, StackError> {
        let payload = datagram.payload;
        let udp_len = UdpPacket::HEADER_LEN
            .checked_add(payload.len())
            .ok_or(StackError::PacketTooLarge)?;
        self.ensure_path_packet_fits(
            PathMtuKey::new(
                IpAddress::Ipv4(path.source),
                IpAddress::Ipv4(path.destination),
            ),
            EthernetFrame::HEADER_LEN + Ipv4Packet::MIN_HEADER_LEN,
            udp_len,
        )?;
        let checksum = self.transport_checksum();
        self.queue_outbound_frame(|frame| {
            let storage = frame.storage_mut();
            let mut offset = EthernetFrame::encode_header(
                storage,
                path.destination_mac,
                path.local_mac,
                EthernetProtocol::Ipv4,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Ipv4Packet::encode_header(
                &mut storage[offset..],
                path.source,
                path.destination,
                crate::IpProtocol::Udp,
                udp_len,
                identification,
                datagram.hop_limit,
            )
            .ok_or(StackError::OutputQueueFull)?;
            let transport_offset = offset;
            offset += UdpPacket::encode(
                &mut storage[offset..],
                IpAddress::Ipv4(path.source),
                IpAddress::Ipv4(path.destination),
                datagram.source_port,
                datagram.destination_port,
                payload,
                checksum,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            if matches!(checksum, TransportChecksum::DeviceOffload) {
                frame.set_tx_checksum(Some(TxChecksum {
                    start: transport_offset as u16,
                    offset: UDP_CHECKSUM_FIELD_OFFSET,
                }));
            }
            Ok(payload.len())
        })
    }

    fn queue_udp_ipv6(
        &mut self,
        path: FramePath<Ipv6Address>,
        datagram: UdpEgress<'_>,
    ) -> Result<usize, StackError> {
        let payload = datagram.payload;
        let udp_len = UdpPacket::HEADER_LEN
            .checked_add(payload.len())
            .ok_or(StackError::PacketTooLarge)?;
        self.ensure_path_packet_fits(
            PathMtuKey::new(
                IpAddress::Ipv6(path.source),
                IpAddress::Ipv6(path.destination),
            ),
            EthernetFrame::HEADER_LEN + Ipv6Packet::HEADER_LEN,
            udp_len,
        )?;
        let checksum = self.transport_checksum();
        self.queue_outbound_frame(|frame| {
            let storage = frame.storage_mut();
            let mut offset = EthernetFrame::encode_header(
                storage,
                path.destination_mac,
                path.local_mac,
                EthernetProtocol::Ipv6,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Ipv6Packet::encode_header(
                &mut storage[offset..],
                path.source,
                path.destination,
                crate::IpProtocol::Udp,
                udp_len,
                datagram.hop_limit,
            )
            .ok_or(StackError::OutputQueueFull)?;
            let transport_offset = offset;
            offset += UdpPacket::encode(
                &mut storage[offset..],
                IpAddress::Ipv6(path.source),
                IpAddress::Ipv6(path.destination),
                datagram.source_port,
                datagram.destination_port,
                payload,
                checksum,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            if matches!(checksum, TransportChecksum::DeviceOffload) {
                frame.set_tx_checksum(Some(TxChecksum {
                    start: transport_offset as u16,
                    offset: UDP_CHECKSUM_FIELD_OFFSET,
                }));
            }
            Ok(payload.len())
        })
    }

    /// Transmit checksum strategy for outgoing TCP/UDP frames.
    const fn transport_checksum(&self) -> TransportChecksum {
        if self.config.tx_checksum_offload {
            TransportChecksum::DeviceOffload
        } else {
            TransportChecksum::Software
        }
    }

    /// Chooses the payload representation for a queued data segment:
    /// a coalesced burst is always scatter (nothing else can carry it),
    /// otherwise scatter when the device reads borrowed bytes, offload
    /// seeds the checksum, and the payload is large enough to beat the
    /// copy.
    fn tcp_tx_payload<'a>(&self, segment: &'a TcpTransmitSegment) -> TcpTxPayload<'a> {
        let payload = &segment.payload;
        let segment_bytes = segment.segment_bytes as usize;
        if segment_bytes != 0 && payload.len() > segment_bytes {
            return TcpTxPayload::Segmented {
                payload,
                segment_bytes: u16::try_from(segment_bytes)
                    .expect("TCP segmentation MSS exceeds a virtio gso_size"),
            };
        }
        if self.config.direct_tx_dma
            && self.config.tx_checksum_offload
            && payload.len() >= TCP_SCATTER_MIN_PAYLOAD_BYTES
        {
            TcpTxPayload::Scatter(payload)
        } else {
            TcpTxPayload::Flat(payload)
        }
    }

    /// What one transmit frame may carry on this path: the wire-segment
    /// budget from the path MTU, plus the coalesced-frame budget when
    /// the interface segments this address family.
    ///
    /// Both are measured the same way, so the socket subtracts its TCP
    /// header and options from them identically.
    ///
    /// Segmentation rides on transmit checksum offload: the device
    /// finishes each segment's TCP checksum, so a frame it has to split
    /// must also be one it has to checksum (virtio 1.2 §5.1.6.2).
    fn tcp_segment_budget_for(&self, local: TcpEndpoint, remote: TcpEndpoint) -> TcpSegmentBudget {
        let wire_segment = self.tcp_segment_mtu_for(local, remote);
        let (segments, packet_header_len) = match local.address {
            IpAddress::Ipv4(_) => (
                self.config.segmentation.tx_tcp_ipv4,
                EthernetFrame::HEADER_LEN + Ipv4Packet::MIN_HEADER_LEN,
            ),
            IpAddress::Ipv6(_) => (
                self.config.segmentation.tx_tcp_ipv6,
                EthernetFrame::HEADER_LEN + Ipv6Packet::HEADER_LEN,
            ),
        };
        if !segments || !self.config.tx_checksum_offload {
            return TcpSegmentBudget::wire_segments(wire_segment);
        }
        let coalesced = self
            .config
            .segmentation
            .max_tx_frame_bytes
            .saturating_sub(packet_header_len)
            .max(wire_segment);
        TcpSegmentBudget::coalesced(wire_segment, coalesced)
    }

    fn queue_tcp(
        &mut self,
        local: TcpEndpoint,
        remote: TcpEndpoint,
        segment: TcpEgress<'_>,
        identification: u16,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        match (local.address, remote.address) {
            (IpAddress::Ipv4(source), IpAddress::Ipv4(destination)) => {
                self.queue_tcp_ipv4(source, destination, segment, identification, now)
            }
            (IpAddress::Ipv6(source), IpAddress::Ipv6(destination)) => {
                self.queue_tcp_ipv6(source, destination, segment, now)
            }
            _ => panic!("TCP endpoint address families must match"),
        }
    }

    fn queue_tcp_ipv4(
        &mut self,
        source: Ipv4Address,
        destination: Ipv4Address,
        segment: TcpEgress<'_>,
        identification: u16,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        let TcpEgress {
            header, payload, ..
        } = segment;
        // A coalesced burst is measured per wire segment: the device
        // splits it into MSS-sized packets, and those are what the path
        // has to carry.
        let tcp_len = TcpPacket::MIN_HEADER_LEN
            .checked_add(segment.options.encoded_len())
            .and_then(|header_len| header_len.checked_add(payload.wire_segment_len()))
            .ok_or(StackError::PacketTooLarge)?;
        self.ensure_path_packet_fits(
            PathMtuKey::new(IpAddress::Ipv4(source), IpAddress::Ipv4(destination)),
            EthernetFrame::HEADER_LEN + Ipv4Packet::MIN_HEADER_LEN,
            tcp_len,
        )?;
        let next_hop = self.next_hop(IpAddress::Ipv4(destination));
        let next_hop = match next_hop {
            Some(IpAddress::Ipv4(next_hop)) => next_hop,
            Some(IpAddress::Ipv6(_)) => panic!("IPv4 route resolved to IPv6 next hop"),
            None => destination,
        };
        let Some(destination_mac) = self.neighbor_mac(IpAddress::Ipv4(next_hop)) else {
            self.queue_arp_request(source, next_hop, now)?;
            return Ok(false);
        };

        let checksum = self.transport_checksum();
        let path = FramePath {
            destination_mac,
            local_mac: self.config.mac,
            source,
            destination,
        };
        if payload.is_empty() && is_replaceable_tcp_ack(header, &[]) {
            return self.queue_tcp_ack_frame(
                TcpAckReplaceKey {
                    local: TcpEndpoint {
                        address: IpAddress::Ipv4(source),
                        port: header.source_port,
                    },
                    remote: TcpEndpoint {
                        address: IpAddress::Ipv4(destination),
                        port: header.destination_port,
                    },
                    sequence: header.sequence,
                    acknowledgement: header.acknowledgement,
                },
                |frame| encode_tcp_ipv4_frame(frame, path, segment, identification, checksum),
            );
        }
        self.queue_outbound_frame(|frame| {
            encode_tcp_ipv4_frame(frame, path, segment, identification, checksum)?;
            Ok(true)
        })
    }

    fn queue_tcp_ipv6(
        &mut self,
        source: Ipv6Address,
        destination: Ipv6Address,
        segment: TcpEgress<'_>,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        let TcpEgress {
            header, payload, ..
        } = segment;
        // Same per-wire-segment rule as the IPv4 path above.
        let tcp_len = TcpPacket::MIN_HEADER_LEN
            .checked_add(segment.options.encoded_len())
            .and_then(|header_len| header_len.checked_add(payload.wire_segment_len()))
            .ok_or(StackError::PacketTooLarge)?;
        self.ensure_path_packet_fits(
            PathMtuKey::new(IpAddress::Ipv6(source), IpAddress::Ipv6(destination)),
            EthernetFrame::HEADER_LEN + Ipv6Packet::HEADER_LEN,
            tcp_len,
        )?;
        let next_hop = self.next_hop(IpAddress::Ipv6(destination));
        let next_hop = match next_hop {
            Some(IpAddress::Ipv6(next_hop)) => next_hop,
            Some(IpAddress::Ipv4(_)) => panic!("IPv6 route resolved to IPv4 next hop"),
            None => destination,
        };
        let Some(destination_mac) = self.neighbor_mac(IpAddress::Ipv6(next_hop)) else {
            self.queue_ndp_solicitation(source, next_hop, now)?;
            return Ok(false);
        };

        let checksum = self.transport_checksum();
        let path = FramePath {
            destination_mac,
            local_mac: self.config.mac,
            source,
            destination,
        };
        if payload.is_empty() && is_replaceable_tcp_ack(header, &[]) {
            return self.queue_tcp_ack_frame(
                TcpAckReplaceKey {
                    local: TcpEndpoint {
                        address: IpAddress::Ipv6(source),
                        port: header.source_port,
                    },
                    remote: TcpEndpoint {
                        address: IpAddress::Ipv6(destination),
                        port: header.destination_port,
                    },
                    sequence: header.sequence,
                    acknowledgement: header.acknowledgement,
                },
                |frame| encode_tcp_ipv6_frame(frame, path, segment, checksum),
            );
        }
        self.queue_outbound_frame(|frame| {
            encode_tcp_ipv6_frame(frame, path, segment, checksum)?;
            Ok(true)
        })
    }

    fn receive_arp(
        &mut self,
        source_mac: EthernetAddress,
        bytes: &[u8],
        now: StackInstant,
    ) -> Result<bool, StackError> {
        let packet = ArpPacket::parse(bytes).ok_or(StackError::MalformedPacket)?;
        self.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(packet.sender_protocol),
            mac: packet.sender_hardware,
            state: NeighborState::Reachable,
            updated_at: now,
        });
        if packet.operation == ArpOperation::Request
            && self
                .ipv4_addresses
                .iter()
                .any(|address| address.address() == packet.target_protocol)
        {
            self.queue_arp_reply(packet.target_protocol, packet.sender_protocol, source_mac)?;
        }
        Ok(false)
    }

    fn receive_ipv4(
        &mut self,
        source_mac: EthernetAddress,
        frame: &Bytes,
        offload: RxFrameOffload,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        let packet = self.parse_ipv4(frame.as_ref())?;
        if !packet.source.is_unspecified() {
            self.learn_neighbor(NeighborEntry {
                ip: IpAddress::Ipv4(packet.source),
                mac: source_mac,
                state: NeighborState::Reachable,
                updated_at: now,
            });
        }
        if packet.is_fragmented() {
            let payload_bytes = frame.slice(bytes_subrange(frame.as_ref(), packet.payload));
            return match self.ipv4_reassembly.process_fragment(
                packet,
                frame.as_ref(),
                payload_bytes,
                now,
            ) {
                Ipv4ReassemblyResult::Complete(packet_bytes) => {
                    let packet = self.parse_ipv4(packet_bytes.as_ref())?;
                    assert!(
                        !packet.is_fragmented(),
                        "IPv4 reassembly produced a fragmented packet"
                    );
                    // A device checksum report describes the fragment it
                    // arrived with, not the datagram reassembly just
                    // produced, so the reassembled packet is verified in
                    // software like any frame without metadata.
                    self.dispatch_ipv4(
                        source_mac,
                        &packet_bytes,
                        packet,
                        RxFrameOffload::none(),
                        now,
                    )
                }
                Ipv4ReassemblyResult::NeedMoreFragments
                | Ipv4ReassemblyResult::InvalidFragment
                | Ipv4ReassemblyResult::CacheFull => Ok(false),
            };
        }
        self.dispatch_ipv4(source_mac, frame, packet, offload, now)
    }

    fn dispatch_ipv4(
        &mut self,
        source_mac: EthernetAddress,
        frame: &Bytes,
        packet: Ipv4Packet<'_>,
        offload: RxFrameOffload,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        let payload_range = bytes_subrange(frame.as_ref(), packet.payload);
        let offload = offload.advance(payload_range.start);
        let payload_bytes = frame.slice(payload_range);
        match packet.protocol {
            crate::IpProtocol::Tcp => {
                if !self.owns_ip_address(IpAddress::Ipv4(packet.destination)) {
                    return Ok(false);
                }
                self.receive_tcp(
                    IpAddress::Ipv4(packet.source),
                    IpAddress::Ipv4(packet.destination),
                    &payload_bytes,
                    offload,
                    now,
                )
            }
            crate::IpProtocol::Udp => {
                if !self.accepts_ipv4_udp_destination(packet.destination) {
                    return Ok(false);
                }
                let consumed = self.receive_udp(
                    IpAddress::Ipv4(packet.source),
                    IpAddress::Ipv4(packet.destination),
                    payload_bytes,
                    offload,
                )?;
                if !consumed && should_send_icmpv4_error_for_destination(packet.destination) {
                    self.queue_icmpv4_port_unreachable(source_mac, packet, frame.as_ref())?;
                }
                Ok(false)
            }
            crate::IpProtocol::Icmp => {
                if !self.owns_ip_address(IpAddress::Ipv4(packet.destination)) {
                    return Ok(false);
                }
                self.receive_icmpv4(packet, now)
            }
            crate::IpProtocol::Icmpv6 => Ok(false),
        }
    }

    fn parse_ipv4<'a>(&self, bytes: &'a [u8]) -> Result<Ipv4Packet<'a>, StackError> {
        if self.config.rx_checksum_offload.ipv4 {
            Ipv4Packet::parse_without_checksum(bytes).ok_or(StackError::MalformedPacket)
        } else {
            Ipv4Packet::parse(bytes).ok_or(StackError::MalformedPacket)
        }
    }

    fn receive_ipv6(
        &mut self,
        source_mac: EthernetAddress,
        frame: &Bytes,
        offload: RxFrameOffload,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        let packet = Ipv6Packet::parse(frame.as_ref()).ok_or(StackError::MalformedPacket)?;
        if !packet.source.is_unspecified() {
            // Passive learning: filling the cache from ordinary traffic
            // saves a solicitation round trip, but RFC 4861 §7.2.5 only
            // lets a Neighbor Advertisement carrying the override bit
            // replace a cached link-layer address, so a data frame must
            // never do it either.
            self.learn_neighbor_without_override(
                NeighborEntry {
                    ip: IpAddress::Ipv6(packet.source),
                    mac: source_mac,
                    state: NeighborState::Reachable,
                    updated_at: now,
                },
                false,
            );
        }
        let payload_range = bytes_subrange(frame.as_ref(), packet.payload);
        let offload = offload.advance(payload_range.start);
        let payload_bytes = frame.slice(payload_range);
        match packet.next_header {
            crate::IpProtocol::Tcp => {
                if !self.owns_ip_address(IpAddress::Ipv6(packet.destination)) {
                    return Ok(false);
                }
                self.receive_tcp(
                    IpAddress::Ipv6(packet.source),
                    IpAddress::Ipv6(packet.destination),
                    &payload_bytes,
                    offload,
                    now,
                )
            }
            crate::IpProtocol::Udp => {
                if !self.owns_ip_address(IpAddress::Ipv6(packet.destination)) {
                    return Ok(false);
                }
                let consumed = self.receive_udp(
                    IpAddress::Ipv6(packet.source),
                    IpAddress::Ipv6(packet.destination),
                    payload_bytes,
                    offload,
                )?;
                if !consumed {
                    self.queue_icmpv6_port_unreachable(source_mac, packet, frame.as_ref())?;
                }
                Ok(false)
            }
            crate::IpProtocol::Icmpv6 => {
                if !self.accepts_ipv6_icmp_destination(packet.destination) {
                    return Ok(false);
                }
                self.receive_icmpv6(source_mac, packet, now)
            }
            crate::IpProtocol::Icmp => Ok(false),
        }
    }

    fn source_ipv4_for(&self, destination: Ipv4Address) -> Option<Ipv4Address> {
        self.ipv4_addresses
            .iter()
            .copied()
            .find(|cidr| cidr.contains(destination))
            .or_else(|| self.ipv4_addresses.first().copied())
            .map(Ipv4Cidr::address)
    }

    /// Picks a source address for `destination` following the parts of
    /// RFC 6724 §5 that matter to an interface holding a link-local
    /// address plus whatever SLAAC configured.
    ///
    /// Rule 8 (longest matching prefix) first: an address on the
    /// destination's own prefix always wins. Then rule 2 (appropriate
    /// scope): a link-local destination must be answered from a
    /// link-local source, and a destination of wider scope must not be
    /// answered from a link-local source, which would be unroutable off
    /// the link. Only when nothing else applies does the first
    /// configured address stand in.
    fn source_ipv6_for(&self, destination: Ipv6Address) -> Option<Ipv6Address> {
        if let Some(cidr) = self
            .ipv6_addresses
            .iter()
            .copied()
            .find(|cidr| cidr.contains(destination))
        {
            return Some(cidr.address());
        }
        let destination_scope = destination.scope();
        let same_scope = self
            .ipv6_addresses
            .iter()
            .copied()
            .find(|cidr| cidr.address().scope() == destination_scope);
        let wider_scope = if destination_scope == Ipv6Scope::LinkLocal {
            // Nothing but a link-local source can reach a link-local
            // destination, so there is no wider-scope substitute.
            None
        } else {
            self.ipv6_addresses
                .iter()
                .copied()
                .find(|cidr| !cidr.address().is_link_local())
        };
        same_scope
            .or(wider_scope)
            .or_else(|| self.ipv6_addresses.first().copied())
            .map(Ipv6Cidr::address)
    }

    fn queue_ndp_solicitation(
        &mut self,
        source: Ipv6Address,
        target: Ipv6Address,
        now: StackInstant,
    ) -> Result<(), StackError> {
        self.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv6(target),
            mac: [0; 6],
            state: NeighborState::Incomplete,
            updated_at: now,
        });
        let destination = target.solicited_node_multicast();
        let destination_mac = ipv6_multicast_mac(destination);
        self.queue_icmpv6_neighbor_solicitation(source, destination, target, destination_mac)
    }

    fn queue_icmpv6_neighbor_solicitation(
        &mut self,
        source: Ipv6Address,
        destination: Ipv6Address,
        target: Ipv6Address,
        destination_mac: EthernetAddress,
    ) -> Result<(), StackError> {
        let local_mac = self.config.mac;
        self.queue_outbound_frame(|frame| {
            let storage = frame.storage_mut();
            let mut offset = EthernetFrame::encode_header(
                storage,
                destination_mac,
                local_mac,
                EthernetProtocol::Ipv6,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Ipv6Packet::encode_header(
                &mut storage[offset..],
                source,
                destination,
                crate::IpProtocol::Icmpv6,
                Icmpv6Packet::NEIGHBOR_MESSAGE_LEN,
                255,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Icmpv6Packet::encode_neighbor_solicitation(
                &mut storage[offset..],
                source,
                destination,
                target,
                local_mac,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            Ok(())
        })
    }

    fn queue_icmpv6_neighbor_advertisement(
        &mut self,
        source: Ipv6Address,
        destination: Ipv6Address,
        target: Ipv6Address,
        destination_mac: EthernetAddress,
    ) -> Result<(), StackError> {
        let local_mac = self.config.mac;
        self.queue_outbound_frame(|frame| {
            let storage = frame.storage_mut();
            let mut offset = EthernetFrame::encode_header(
                storage,
                destination_mac,
                local_mac,
                EthernetProtocol::Ipv6,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Ipv6Packet::encode_header(
                &mut storage[offset..],
                source,
                destination,
                crate::IpProtocol::Icmpv6,
                Icmpv6Packet::NEIGHBOR_MESSAGE_LEN,
                255,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Icmpv6Packet::encode_neighbor_advertisement(
                &mut storage[offset..],
                source,
                destination,
                target,
                local_mac,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            Ok(())
        })
    }

    fn next_hop(&self, destination: IpAddress) -> Option<IpAddress> {
        self.routes
            .resolve(destination)
            .and_then(|route| route.gateway)
    }

    fn neighbor_mac(&self, ip: IpAddress) -> Option<EthernetAddress> {
        self.neighbors
            .iter()
            .find(|entry| {
                entry.ip == ip
                    && matches!(
                        entry.state,
                        NeighborState::Reachable | NeighborState::Stale | NeighborState::Delay
                    )
            })
            .map(|entry| entry.mac)
    }

    fn queue_arp_request(
        &mut self,
        source: Ipv4Address,
        target: Ipv4Address,
        now: StackInstant,
    ) -> Result<(), StackError> {
        self.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(target),
            mac: [0; 6],
            state: NeighborState::Incomplete,
            updated_at: now,
        });
        self.queue_arp(
            [0xff; 6],
            ArpPacket {
                operation: ArpOperation::Request,
                sender_hardware: self.config.mac,
                sender_protocol: source,
                target_hardware: [0; 6],
                target_protocol: target,
            },
        )
    }

    fn queue_arp_reply(
        &mut self,
        source: Ipv4Address,
        target: Ipv4Address,
        target_mac: EthernetAddress,
    ) -> Result<(), StackError> {
        self.queue_arp(
            target_mac,
            ArpPacket {
                operation: ArpOperation::Reply,
                sender_hardware: self.config.mac,
                sender_protocol: source,
                target_hardware: target_mac,
                target_protocol: target,
            },
        )
    }

    fn queue_arp(
        &mut self,
        destination_mac: EthernetAddress,
        packet: ArpPacket,
    ) -> Result<(), StackError> {
        let local_mac = self.config.mac;
        self.queue_outbound_frame(|frame| {
            let storage = frame.storage_mut();
            let mut offset = EthernetFrame::encode_header(
                storage,
                destination_mac,
                local_mac,
                EthernetProtocol::Arp,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += packet
                .encode(&mut storage[offset..])
                .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            Ok(())
        })
    }

    fn queue_icmpv4_port_unreachable(
        &mut self,
        destination_mac: EthernetAddress,
        packet: Ipv4Packet<'_>,
        packet_bytes: &[u8],
    ) -> Result<(), StackError> {
        if !self.should_answer_ipv4_destination_unreachable(packet) {
            return Ok(());
        }
        let quoted_len = packet_bytes.len().min(Ipv4Packet::MIN_HEADER_LEN + 8);
        let quoted = &packet_bytes[..quoted_len];
        let icmp_len = Icmpv4Packet::HEADER_LEN
            .checked_add(quoted.len())
            .unwrap_or_else(|| panic!("ICMPv4 unreachable quote length overflowed"));
        let source = packet.destination;
        let destination = packet.source;
        let local_mac = self.config.mac;
        self.queue_outbound_frame(|frame| {
            let storage = frame.storage_mut();
            let mut offset = EthernetFrame::encode_header(
                storage,
                destination_mac,
                local_mac,
                EthernetProtocol::Ipv4,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Ipv4Packet::encode_header(
                &mut storage[offset..],
                source,
                destination,
                crate::IpProtocol::Icmp,
                icmp_len,
                packet.payload.len() as u16,
                64,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Icmpv4Packet::encode_destination_unreachable(
                &mut storage[offset..],
                Icmpv4Packet::DESTINATION_UNREACHABLE_PORT_CODE,
                quoted,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            Ok(())
        })
    }

    fn should_answer_ipv4_destination_unreachable(&self, packet: Ipv4Packet<'_>) -> bool {
        self.owns_ip_address(IpAddress::Ipv4(packet.destination))
            && !packet.source.is_unspecified()
            && !packet.destination.is_unspecified()
            && !packet.destination.is_multicast()
            && packet.destination.octets() != [255, 255, 255, 255]
    }

    fn queue_icmpv6_port_unreachable(
        &mut self,
        destination_mac: EthernetAddress,
        packet: Ipv6Packet<'_>,
        packet_bytes: &[u8],
    ) -> Result<(), StackError> {
        if !self.should_answer_ipv6_destination_unreachable(packet) {
            return Ok(());
        }
        let header_len = EthernetFrame::HEADER_LEN
            .checked_add(Ipv6Packet::HEADER_LEN)
            .and_then(|len| len.checked_add(Icmpv6Packet::DESTINATION_UNREACHABLE_HEADER_LEN))
            .unwrap_or_else(|| panic!("ICMPv6 unreachable header length overflowed"));
        let quote_capacity =
            self.config.mtu.checked_sub(header_len).unwrap_or_else(|| {
                panic!("MTU cannot hold ICMPv6 destination-unreachable headers")
            });
        let quoted = &packet_bytes[..packet_bytes.len().min(quote_capacity)];
        let icmp_len = Icmpv6Packet::DESTINATION_UNREACHABLE_HEADER_LEN
            .checked_add(quoted.len())
            .unwrap_or_else(|| panic!("ICMPv6 unreachable quote length overflowed"));
        let source = packet.destination;
        let destination = packet.source;
        let local_mac = self.config.mac;
        self.queue_outbound_frame(|frame| {
            let storage = frame.storage_mut();
            let mut offset = EthernetFrame::encode_header(
                storage,
                destination_mac,
                local_mac,
                EthernetProtocol::Ipv6,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Ipv6Packet::encode_header(
                &mut storage[offset..],
                source,
                destination,
                crate::IpProtocol::Icmpv6,
                icmp_len,
                64,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += Icmpv6Packet::encode_destination_unreachable(
                &mut storage[offset..],
                source,
                destination,
                Icmpv6Packet::DESTINATION_UNREACHABLE_PORT_CODE,
                quoted,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            Ok(())
        })
    }

    fn should_answer_ipv6_destination_unreachable(&self, packet: Ipv6Packet<'_>) -> bool {
        self.owns_ip_address(IpAddress::Ipv6(packet.destination))
            && !packet.source.is_unspecified()
            && !packet.source.is_multicast()
            && !packet.destination.is_unspecified()
            && !packet.destination.is_multicast()
    }

    /// Whether the device, not the stack, is answerable for this
    /// frame's transport checksum.
    ///
    /// Both halves have to agree. `negotiated` is the interface-wide
    /// capability the kernel configured from the feature set the device
    /// accepted, and the report is what the device said about the frame
    /// in hand: a device that validates checksums still delivers frames
    /// it did not validate, and one that never negotiated the feature
    /// cannot make a claim at all. A partial checksum is completed and
    /// checked against the packet the stack parsed (see
    /// [`partial_transport_checksum_completes`]); a frame that fails
    /// that falls through to the software verification, which fails it
    /// too, so a bad partial frame is dropped rather than delivered.
    fn device_owns_transport_checksum(
        &self,
        source: IpAddress,
        destination: IpAddress,
        protocol: crate::IpProtocol,
        segment: &[u8],
        offload: RxFrameOffload,
    ) -> bool {
        let (negotiated, checksum_field_offset) = match protocol {
            crate::IpProtocol::Tcp => (
                self.config.rx_checksum_offload.tcp,
                TCP_CHECKSUM_FIELD_OFFSET,
            ),
            crate::IpProtocol::Udp => (
                self.config.rx_checksum_offload.udp,
                UDP_CHECKSUM_FIELD_OFFSET,
            ),
            // No other protocol carries a transport checksum a device
            // reports on.
            _ => return false,
        };
        if !negotiated {
            return false;
        }
        match offload.checksum {
            RxChecksumReport::Unverified => false,
            RxChecksumReport::Validated => true,
            RxChecksumReport::Partial { start, offset } => {
                start == 0
                    && offset == checksum_field_offset
                    && partial_transport_checksum_completes(
                        source,
                        destination,
                        protocol,
                        segment,
                        usize::from(offset),
                    )
            }
        }
    }

    fn receive_tcp(
        &mut self,
        source: IpAddress,
        destination: IpAddress,
        bytes: &Bytes,
        offload: RxFrameOffload,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        if !self.device_owns_transport_checksum(
            source,
            destination,
            crate::IpProtocol::Tcp,
            bytes.as_ref(),
            offload,
        ) && !tcp_checksum_valid(source, destination, bytes.as_ref())
        {
            return Err(StackError::MalformedPacket);
        }
        let packet = crate::TcpPacket::parse(bytes.as_ref()).ok_or(StackError::MalformedPacket)?;
        // Slice an owning Bytes for the TCP payload so the receive
        // queue downstream can retain ownership without per-segment
        // copy. `slice_ref` reuses the parent buffer without
        // allocating.
        let payload_bytes = bytes.slice_ref(packet.payload);
        let local_endpoint = TcpEndpoint {
            address: destination,
            port: packet.destination_port,
        };
        let remote_endpoint = TcpEndpoint {
            address: source,
            port: packet.source_port,
        };
        if let Some(index) = self.tcp.find_endpoint(local_endpoint, remote_endpoint) {
            let (previous_state, current_state, outcome, receive_backpressured) = {
                let socket = self
                    .tcp
                    .get_mut(index)
                    .expect("TCP endpoint index referenced a missing socket");
                let previous_state = socket.state();
                let outcome = socket.on_segment(packet, payload_bytes, now.nanos());
                (
                    previous_state,
                    socket.state(),
                    outcome,
                    socket.receive_backpressured(),
                )
            };
            self.refresh_tcp_indices_after_state(index);
            self.schedule_tcp_timer(index);
            self.update_tcp_receive_backpressure(index);
            if previous_state == crate::TcpState::SynSent
                && current_state == crate::TcpState::Closed
            {
                self.tcp
                    .set_terminal_error(index, TcpConnectTerminalError::Closed);
            }
            if previous_state == crate::TcpState::SynReceived
                && current_state == crate::TcpState::Established
            {
                self.tcp_accept
                    .push_back(TcpQueuedAccept {
                        listener: self.tcp_listener_children[index]
                            .expect("accepted TCP child should resolve a listener"),
                        accepted: TcpAccept {
                            socket: socket_id(index),
                            remote: remote_endpoint,
                        },
                    })
                    .unwrap_or_else(|_| panic!("TCP accept queue is full"));
            }
            if outcome.readable {
                Self::push_event_into(
                    &mut self.events,
                    StackEvent::TcpReadable {
                        socket: socket_id(index),
                    },
                );
            }
            if outcome.receive_backpressure {
                return Err(StackError::ReceiveBackpressure);
            }
            if let Some(reset) = outcome.reset {
                self.queue_tcp(
                    reset.local,
                    reset.remote,
                    TcpEgress {
                        header: reset.header,
                        options: TcpHeaderOptions::empty(),
                        payload: TcpTxPayload::Flat(&[]),
                        hop_limit: crate::DEFAULT_HOP_LIMIT,
                    },
                    packet.acknowledgement as u16,
                    now,
                )?;
            }
            return Ok(receive_backpressured);
        }
        if packet.flags.contains(crate::TcpFlags::SYN)
            && !packet.flags.contains(crate::TcpFlags::ACK)
            && let Some(listener_index) = self.tcp.find_listener(local_endpoint)
        {
            let listener = self
                .tcp
                .get(listener_index)
                .expect("TCP listener index referenced a missing socket");
            assert!(
                listener.is_listening_on(destination, packet.destination_port),
                "TCP listener index resolved a non-listening socket"
            );
            if !self.tcp.has_free_slot() {
                return Ok(false);
            }
            let listener_socket = socket_id(listener_index);
            if !self.tcp_listener_queues.reserve(listener_socket) {
                return Ok(false);
            }
            let local = TcpEndpoint {
                address: destination,
                port: packet.destination_port,
            };
            let initial_sequence = (now.nanos() as u32)
                .wrapping_add(u32::from(packet.destination_port))
                .wrapping_add(u32::from(packet.source_port));
            let listener_hop_limit = listener.hop_limit();
            let mut child = TcpSocket::accept(
                local,
                remote_endpoint,
                packet.sequence.wrapping_add(1),
                initial_sequence,
                self.congestion.clone(),
            );
            child.set_hop_limit(listener_hop_limit);
            child.record_peer_options(packet);
            let child = self.insert_tcp(child);
            self.tcp_listener_children[socket_index(child)] = Some(listener_socket);
            return Ok(false);
        }
        if let Some(reset) = self.tcp_reset_response(source, destination, packet) {
            // A segment whose four-tuple has no socket here. For an
            // established connection that is always a defect — either
            // the socket left the endpoint index, or the frame reached
            // a stack that never owned the flow — and the RST tears
            // down a healthy connection. `sockets` separates the two:
            // a stack holding none of them was not the flow's owner.
            tracing::debug!(
                ?local_endpoint,
                ?remote_endpoint,
                flags = ?packet.flags,
                sequence = packet.sequence,
                acknowledgement = packet.acknowledgement,
                payload_len = packet.payload.len(),
                sockets = self.tcp.active_len(),
                "TCP segment for an unknown four-tuple, answering with RST"
            );
            self.queue_tcp(
                reset.local,
                reset.remote,
                TcpEgress {
                    header: reset.header,
                    options: TcpHeaderOptions::empty(),
                    payload: TcpTxPayload::Flat(&[]),
                    hop_limit: crate::DEFAULT_HOP_LIMIT,
                },
                packet.sequence as u16,
                now,
            )?;
        }
        Ok(false)
    }

    fn receive_icmpv6(
        &mut self,
        source_mac: EthernetAddress,
        packet: Ipv6Packet<'_>,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        if !icmpv6_checksum_valid(packet.source, packet.destination, packet.payload) {
            return Err(StackError::MalformedPacket);
        }
        match Icmpv6Packet::parse(packet.payload).ok_or(StackError::MalformedPacket)? {
            Icmpv6Packet::RouterAdvertisement(advertisement) => {
                // RFC 4861 §6.1.2: a valid advertisement has a link-local
                // source and an unmodified hop limit of 255.
                if !packet.source.is_link_local() || packet.hop_limit != 255 {
                    return Ok(false);
                }
                let configuration =
                    interpret_router_advertisement(advertisement, packet.source, self.config.mac);
                self.apply_ipv6_router_configuration(configuration, now);
            }
            Icmpv6Packet::NeighborSolicitation { target, options } => {
                if packet.source.is_unspecified() {
                    return Ok(false);
                }
                // RFC 4861 §4.3: prefer the solicitation's own source
                // link-layer option over the Ethernet header, which a
                // bridge or proxy may have rewritten.
                let neighbor_mac = options.source_link_layer_address().unwrap_or(source_mac);
                self.learn_neighbor(NeighborEntry {
                    ip: IpAddress::Ipv6(packet.source),
                    mac: neighbor_mac,
                    state: NeighborState::Reachable,
                    updated_at: now,
                });
                if self
                    .ipv6_addresses
                    .iter()
                    .any(|address| address.address() == target)
                {
                    self.queue_icmpv6_neighbor_advertisement(
                        target,
                        packet.source,
                        target,
                        neighbor_mac,
                    )?;
                }
            }
            Icmpv6Packet::NeighborAdvertisement {
                target,
                override_cache,
                options,
                ..
            } => {
                // RFC 4861 §4.4: the target link-layer option names the
                // node the advertisement speaks for, which is not the
                // Ethernet source when a router proxies for it.
                let target_mac = options.target_link_layer_address().unwrap_or(source_mac);
                // §7.2.5: without the override bit an advertisement may
                // not replace a cached link-layer address that differs.
                self.learn_neighbor_without_override(
                    NeighborEntry {
                        ip: IpAddress::Ipv6(target),
                        mac: target_mac,
                        state: NeighborState::Reachable,
                        updated_at: now,
                    },
                    override_cache,
                );
            }
            Icmpv6Packet::DestinationUnreachable(unreachable) => {
                self.receive_icmp_destination_unreachable(IcmpDestinationUnreachable::Ipv6(
                    unreachable,
                ));
            }
            Icmpv6Packet::PacketTooBig(packet_too_big) => {
                self.receive_icmpv6_packet_too_big(packet_too_big, now);
            }
            Icmpv6Packet::EchoReply(echo) => {
                self.record_icmp_echo_reply(
                    IcmpEchoKey {
                        destination: IpAddress::Ipv6(packet.source),
                        identifier: echo.identifier,
                        sequence: echo.sequence,
                    },
                    echo.payload.len(),
                );
            }
            Icmpv6Packet::EchoRequest(_) => {}
        }
        Ok(false)
    }

    fn receive_icmpv4(
        &mut self,
        packet: Ipv4Packet<'_>,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        match Icmpv4Packet::parse(packet.payload).ok_or(StackError::MalformedPacket)? {
            Icmpv4Packet::DestinationUnreachable(unreachable) => {
                if let Icmpv4DestinationUnreachableCode::FragmentationNeeded { next_hop_mtu } =
                    unreachable.code
                {
                    self.receive_icmpv4_fragmentation_needed(
                        unreachable.original,
                        next_hop_mtu,
                        now,
                    );
                    return Ok(false);
                }
                self.receive_icmp_destination_unreachable(IcmpDestinationUnreachable::Ipv4(
                    unreachable,
                ));
            }
            Icmpv4Packet::EchoReply(echo) => {
                self.record_icmp_echo_reply(
                    IcmpEchoKey {
                        destination: IpAddress::Ipv4(packet.source),
                        identifier: echo.identifier,
                        sequence: echo.sequence,
                    },
                    echo.payload.len(),
                );
            }
            Icmpv4Packet::EchoRequest(_) => {}
        }
        Ok(false)
    }

    fn receive_icmp_destination_unreachable(
        &mut self,
        unreachable: IcmpDestinationUnreachable<'_>,
    ) {
        let Some(error) = unreachable.transport_error() else {
            return;
        };
        match error.protocol {
            crate::IpProtocol::Udp => self.record_udp_remote_unreachable(error),
            crate::IpProtocol::Tcp => self.record_tcp_remote_unreachable(error),
            crate::IpProtocol::Icmp | crate::IpProtocol::Icmpv6 => {}
        }
    }

    fn receive_icmpv4_fragmentation_needed(
        &mut self,
        original: &[u8],
        next_hop_mtu: u16,
        now: StackInstant,
    ) {
        let Some(quoted) = Ipv4Packet::parse_quoted(original) else {
            return;
        };
        let key = PathMtuKey::new(
            IpAddress::Ipv4(quoted.source),
            IpAddress::Ipv4(quoted.destination),
        );
        let decreased = if next_hop_mtu == 0 {
            self.path_mtu.update_next_lower(key, quoted.total_len, now)
        } else {
            self.path_mtu
                .update_if_less(key, usize::from(next_hop_mtu), now)
        };
        if decreased {
            self.mark_tcp_path_mtu_reduced(quoted.protocol, quoted.payload, key);
        }
    }

    fn receive_icmpv6_packet_too_big(
        &mut self,
        packet_too_big: crate::packet::Icmpv6PacketTooBig<'_>,
        now: StackInstant,
    ) {
        let Some(quoted) = Ipv6Packet::parse_quoted(packet_too_big.original) else {
            return;
        };
        let Ok(mtu) = usize::try_from(packet_too_big.mtu) else {
            return;
        };
        let key = PathMtuKey::new(
            IpAddress::Ipv6(quoted.source),
            IpAddress::Ipv6(quoted.destination),
        );
        if self.path_mtu.update_if_less(key, mtu, now) {
            self.mark_tcp_path_mtu_reduced(quoted.next_header, quoted.payload, key);
        }
    }

    fn mark_tcp_path_mtu_reduced(
        &mut self,
        protocol: crate::IpProtocol,
        transport: &[u8],
        key: PathMtuKey,
    ) {
        if protocol != crate::IpProtocol::Tcp {
            return;
        }
        let Some(packet) = crate::TcpPacket::parse_quoted(transport) else {
            return;
        };
        let local = TcpEndpoint {
            address: key.source,
            port: packet.source_port,
        };
        let remote = TcpEndpoint {
            address: key.destination,
            port: packet.destination_port,
        };
        let Some(index) = self.tcp.find_endpoint(local, remote) else {
            return;
        };
        self.tcp
            .get_mut(index)
            .unwrap_or_else(|| panic!("TCP PMTU endpoint index referenced a missing socket"))
            .mark_path_mtu_reduced(packet.sequence);
        self.schedule_tcp_timer(index);
    }

    fn record_udp_remote_unreachable(&mut self, error: IcmpTransportError) {
        let local = UdpEndpoint {
            address: error.endpoint.local,
            port: error.endpoint.local_port,
        };
        let remote = UdpEndpoint {
            address: error.endpoint.remote,
            port: error.endpoint.remote_port,
        };
        if let Some(socket) = self.udp.socket_for_connected_endpoint(local, remote) {
            self.udp.set_pending_error(socket, error.kind.into_udp());
        }
    }

    fn record_tcp_remote_unreachable(&mut self, error: IcmpTransportError) {
        let local = TcpEndpoint {
            address: error.endpoint.local,
            port: error.endpoint.local_port,
        };
        let remote = TcpEndpoint {
            address: error.endpoint.remote,
            port: error.endpoint.remote_port,
        };
        let Some(index) = self.tcp.find_endpoint(local, remote) else {
            return;
        };
        if self
            .tcp
            .get(index)
            .unwrap_or_else(|| panic!("TCP endpoint index referenced a missing socket"))
            .state()
            != crate::TcpState::SynSent
        {
            return;
        }
        self.tcp.set_terminal_error(index, error.kind.into_tcp());
        self.tcp
            .get_mut(index)
            .unwrap_or_else(|| panic!("TCP endpoint index referenced a missing socket"))
            .abort();
        self.refresh_tcp_indices_after_state(index);
        self.clear_tcp_timer(index);
        Self::push_event_into(
            &mut self.events,
            StackEvent::TcpClosed {
                socket: socket_id(index),
            },
        );
    }

    fn receive_udp(
        &mut self,
        source: IpAddress,
        destination: IpAddress,
        bytes: Bytes,
        offload: RxFrameOffload,
    ) -> Result<bool, StackError> {
        if !self.device_owns_transport_checksum(
            source,
            destination,
            crate::IpProtocol::Udp,
            bytes.as_ref(),
            offload,
        ) && !udp_checksum_valid(source, destination, bytes.as_ref())
        {
            return Err(StackError::MalformedPacket);
        }
        let packet = UdpPacket::parse(bytes.as_ref()).ok_or(StackError::MalformedPacket)?;
        let Some(socket) = self.udp.socket_for_packet(
            source,
            packet.source_port,
            destination,
            packet.destination_port,
        ) else {
            return Ok(false);
        };
        let datagram = UdpReceive {
            source,
            destination,
            source_port: packet.source_port,
            destination_port: packet.destination_port,
            bytes: UdpPayload::from_bytes(
                bytes.slice(bytes_subrange(bytes.as_ref(), packet.payload)),
            ),
        };
        self.queue_udp_receive(socket, datagram);
        Ok(true)
    }

    fn queue_udp_receive(&mut self, socket: UdpSocketId, datagram: UdpReceive) {
        let index = udp_socket_index(socket);
        if self.udp_rx.is_full() && !self.evict_udp_receive_for(socket) {
            return;
        }
        self.udp_rx
            .push_back(UdpQueuedReceive { socket, datagram })
            .unwrap_or_else(|_| panic!("UDP receive queue reported full after fair eviction"));
        self.udp_rx_counts[index] = self.udp_rx_counts[index]
            .checked_add(1)
            .expect("UDP receive socket count overflowed");
    }

    fn evict_udp_receive_for(&mut self, socket: UdpSocketId) -> bool {
        let index = udp_socket_index(socket);
        let active = self.udp.active_len.max(1);
        let fair_share = MAX_UDP_RX.div_ceil(active).max(1);
        let victim = if usize::from(self.udp_rx_counts[index]) > fair_share {
            Some(socket)
        } else {
            self.udp.active[..self.udp.active_len]
                .iter()
                .copied()
                .filter(|candidate| usize::from(self.udp_rx_counts[*candidate]) > fair_share)
                .max_by_key(|candidate| self.udp_rx_counts[*candidate])
                .map(udp_socket_id)
        };
        let Some(victim) = victim else {
            return false;
        };
        let victim_index = udp_socket_index(victim);
        let len = self.udp_rx.len();
        for _ in 0..len {
            let queued = self
                .udp_rx
                .pop_front()
                .expect("UDP receive queue length changed during fair eviction");
            if queued.socket == victim {
                self.udp_rx_counts[victim_index] = self.udp_rx_counts[victim_index]
                    .checked_sub(1)
                    .expect("UDP receive queue socket count underflowed during fair eviction");
                return true;
            }
            self.udp_rx
                .push_back(queued)
                .unwrap_or_else(|_| panic!("UDP receive queue lost capacity during fair eviction"));
        }
        panic!("UDP receive socket count had no matching datagram during fair eviction");
    }

    fn insert_tcp(&mut self, socket: TcpSocket<C>) -> SocketId {
        let id = self.tcp.insert(socket);
        self.schedule_tcp_timer(socket_index(id));
        id
    }

    fn ensure_tcp_connect_unique(
        &self,
        local: TcpEndpoint,
        remote: TcpEndpoint,
    ) -> Result<(), StackError> {
        for active_slot in 0..self.tcp.active_len() {
            let index = self.tcp.active_index(active_slot);
            let Some(socket) = self.tcp.get(index) else {
                panic!("TCP active socket index referenced a missing socket");
            };
            if socket.state() == crate::TcpState::Closed {
                continue;
            }
            let Some(existing_local) = socket.local_endpoint() else {
                continue;
            };
            match socket.remote_endpoint() {
                Some(existing_remote) if existing_local == local && existing_remote == remote => {
                    return Err(StackError::AddressInUse);
                }
                None if tcp_listener_addresses_overlap(existing_local, local) => {
                    return Err(StackError::AddressInUse);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn ensure_tcp_listen_unique(&self, local: TcpEndpoint) -> Result<(), StackError> {
        for active_slot in 0..self.tcp.active_len() {
            let index = self.tcp.active_index(active_slot);
            let Some(socket) = self.tcp.get(index) else {
                panic!("TCP active socket index referenced a missing socket");
            };
            if socket.state() == crate::TcpState::Closed {
                continue;
            }
            let Some(existing_local) = socket.local_endpoint() else {
                continue;
            };
            if !tcp_listener_addresses_overlap(existing_local, local) {
                continue;
            }
            match socket.remote_endpoint() {
                None => return Err(StackError::AddressInUse),
                Some(_) if existing_local.port == local.port => {
                    return Err(StackError::AddressInUse);
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    fn refresh_tcp_indices_after_state(&mut self, index: usize) {
        let state = self
            .tcp
            .get(index)
            .expect("TCP socket disappeared while refreshing indices")
            .state();
        if state == crate::TcpState::Closed {
            self.tcp.remove_indices(index);
            self.clear_tcp_receive_backpressure(index);
            self.release_tcp_listener_child(index);
            self.remove_tcp_accepts_for(socket_id(index));
        }
    }

    fn release_tcp_listener_child(&mut self, index: usize) {
        if let Some(listener) = self.tcp_listener_children[index].take() {
            self.tcp_listener_queues.release(listener);
        }
    }

    fn tcp_reset_response(
        &self,
        source: IpAddress,
        destination: IpAddress,
        packet: TcpPacket<'_>,
    ) -> Option<TcpResetResponse> {
        if packet.flags.contains(TcpFlags::RST) || !self.owns_ip_address(destination) {
            return None;
        }
        let sequence_len = tcp_segment_sequence_len(packet);
        let (flags, sequence, acknowledgement) = if packet.flags.contains(TcpFlags::ACK) {
            (TcpFlags::RST, packet.acknowledgement, 0)
        } else {
            (
                TcpFlags::RST.union(TcpFlags::ACK),
                0,
                packet.sequence.wrapping_add(sequence_len),
            )
        };
        Some(TcpResetResponse {
            local: TcpEndpoint {
                address: destination,
                port: packet.destination_port,
            },
            remote: TcpEndpoint {
                address: source,
                port: packet.source_port,
            },
            header: TcpHeader {
                source_port: packet.destination_port,
                destination_port: packet.source_port,
                sequence,
                acknowledgement,
                flags,
                window_size: 0,
            },
        })
    }

    fn accepts_ipv4_udp_destination(&self, destination: Ipv4Address) -> bool {
        self.owns_ip_address(IpAddress::Ipv4(destination))
            || destination == Ipv4Address::BROADCAST
            || self.has_ipv4_multicast_membership(destination)
            || self
                .ipv4_addresses
                .iter()
                .copied()
                .any(|local| ipv4_directed_broadcast(local) == Some(destination))
    }

    fn has_ipv4_multicast_membership(&self, group: Ipv4Address) -> bool {
        group.is_multicast()
            && self
                .ipv4_multicast_memberships
                .iter()
                .any(|membership| membership.group == group)
    }

    fn accepts_ipv6_icmp_destination(&self, destination: Ipv6Address) -> bool {
        self.owns_ip_address(IpAddress::Ipv6(destination)) || destination.is_multicast()
    }

    fn owns_ip_address(&self, address: IpAddress) -> bool {
        match address {
            IpAddress::Ipv4(address) => self
                .ipv4_addresses
                .iter()
                .any(|local| local.address() == address),
            IpAddress::Ipv6(address) => self
                .ipv6_addresses
                .iter()
                .any(|local| local.address() == address),
        }
    }

    fn schedule_tcp_timer(&mut self, index: usize) {
        let deadline = self.tcp.get(index).and_then(TcpSocket::next_deadline_nanos);
        self.schedule_tcp_timer_deadline(index, deadline);
    }

    fn schedule_tcp_timer_deadline(&mut self, index: usize, deadline: Option<u64>) {
        if self.tcp_timer_deadlines[index] == deadline {
            return;
        }
        self.tcp_timer_generations[index] = self.tcp_timer_generations[index].wrapping_add(1);
        self.tcp_timer_deadlines[index] = deadline;
        if let Some(deadline_nanos) = deadline {
            let entry = TcpTimerEntry {
                deadline_nanos,
                generation: self.tcp_timer_generations[index],
                socket_index: index,
            };
            if self.tcp_timers.push(entry).is_err() {
                self.rebuild_tcp_timer_heap();
            }
        }
    }

    fn clear_tcp_timer(&mut self, index: usize) {
        self.tcp_timer_generations[index] = self.tcp_timer_generations[index].wrapping_add(1);
        self.tcp_timer_deadlines[index] = None;
    }

    fn update_tcp_receive_backpressure(&mut self, index: usize) {
        let backpressured = self
            .tcp
            .get(index)
            .is_some_and(TcpSocket::receive_backpressured);
        match (self.tcp_receive_backpressured[index], backpressured) {
            (false, true) => {
                self.tcp_receive_backpressured[index] = true;
                self.tcp_receive_backpressured_count += 1;
            }
            (true, false) => {
                self.tcp_receive_backpressured[index] = false;
                assert!(
                    self.tcp_receive_backpressured_count != 0,
                    "TCP receive backpressure count is corrupt"
                );
                self.tcp_receive_backpressured_count -= 1;
            }
            _ => {}
        }
    }

    fn clear_tcp_receive_backpressure(&mut self, index: usize) {
        if self.tcp_receive_backpressured[index] {
            self.tcp_receive_backpressured[index] = false;
            assert!(
                self.tcp_receive_backpressured_count != 0,
                "TCP receive backpressure count is corrupt"
            );
            self.tcp_receive_backpressured_count -= 1;
        }
    }

    fn tcp_timer_entry_current(&self, entry: TcpTimerEntry) -> bool {
        self.tcp_timer_generations[entry.socket_index] == entry.generation
            && self.tcp_timer_deadlines[entry.socket_index] == Some(entry.deadline_nanos)
            && self
                .tcp
                .get(entry.socket_index)
                .is_some_and(|socket| socket.next_deadline_nanos() == Some(entry.deadline_nanos))
    }

    fn rebuild_tcp_timer_heap(&mut self) {
        self.tcp_timers.clear();
        for active_slot in 0..self.tcp.active_len() {
            let index = self.tcp.active_index(active_slot);
            let Some(deadline_nanos) = self.tcp_timer_deadlines[index] else {
                continue;
            };
            self.tcp_timers
                .push(TcpTimerEntry {
                    deadline_nanos,
                    generation: self.tcp_timer_generations[index],
                    socket_index: index,
                })
                .unwrap_or_else(|_| panic!("TCP timer heap is full during rebuild"));
        }
    }

    fn tcp_socket(&self, id: SocketId) -> Result<&TcpSocket<C>, StackError> {
        self.tcp
            .get(socket_index(id))
            .ok_or(StackError::UnknownSocket)
    }

    fn tcp_socket_mut(&mut self, id: SocketId) -> Result<&mut TcpSocket<C>, StackError> {
        self.tcp
            .get_mut(socket_index(id))
            .ok_or(StackError::UnknownSocket)
    }

    fn udp_socket(&self, id: UdpSocketId) -> Result<&UdpSocketState, StackError> {
        self.udp
            .get(udp_socket_index(id))
            .ok_or(StackError::UnknownSocket)
    }

    fn clear_udp_receive_queue(&mut self, socket: UdpSocketId) -> Result<(), StackError> {
        let index = udp_socket_index(socket);
        self.udp_socket(socket)?;
        let len = self.udp_rx.len();
        for _ in 0..len {
            let queued = self
                .udp_rx
                .pop_front()
                .expect("UDP receive queue length changed while pruning");
            if queued.socket == socket {
                self.udp_rx_counts[index] = self.udp_rx_counts[index]
                    .checked_sub(1)
                    .expect("UDP receive queue socket count underflowed while pruning");
            } else {
                self.udp_rx
                    .push_back(queued)
                    .unwrap_or_else(|_| panic!("UDP receive queue lost capacity while pruning"));
            }
        }
        assert_eq!(
            self.udp_rx_counts[index], 0,
            "UDP receive queue count remained non-zero after pruning"
        );
        Ok(())
    }

    fn remove_tcp_accepts_for(&mut self, socket: SocketId) {
        let len = self.tcp_accept.len();
        for _ in 0..len {
            let queued = self
                .tcp_accept
                .pop_front()
                .expect("TCP accept queue length changed while pruning");
            if queued.accepted.socket != socket {
                self.tcp_accept
                    .push_back(queued)
                    .unwrap_or_else(|_| panic!("TCP accept queue lost capacity while pruning"));
            }
        }
    }

    fn remove_tcp_accepts_by_listener(&mut self, listener: SocketId) {
        let len = self.tcp_accept.len();
        for _ in 0..len {
            let queued = self
                .tcp_accept
                .pop_front()
                .expect("TCP accept queue length changed while pruning listener");
            if queued.listener == listener {
                self.tcp_listener_children[socket_index(queued.accepted.socket)] = None;
                self.remove_tcp_child_without_backlog_release(queued.accepted.socket);
            } else {
                self.tcp_accept.push_back(queued).unwrap_or_else(|_| {
                    panic!("TCP accept queue lost capacity while pruning listener")
                });
            }
        }
    }

    fn remove_tcp_listener_children(&mut self, listener: SocketId) {
        let mut children = ArrayVec::<SocketId, MAX_TCP_ACCEPT>::new();
        for active_slot in 0..self.tcp.active_len() {
            let index = self.tcp.active_index(active_slot);
            if socket_id(index) == listener {
                continue;
            }
            let Some(socket) = self.tcp.get(index) else {
                panic!("TCP active socket index referenced a missing socket");
            };
            if socket.remote_endpoint().is_none() {
                continue;
            }
            let Some(local) = socket.local_endpoint() else {
                continue;
            };
            let Some(listener_index) = self.tcp.find_listener(local) else {
                continue;
            };
            if socket_id(listener_index) == listener {
                children
                    .try_push(socket_id(index))
                    .unwrap_or_else(|_| panic!("TCP listener child collection overflowed"));
            }
        }
        for child in children {
            self.remove_tcp_child_without_backlog_release(child);
        }
    }

    fn remove_tcp_child_without_backlog_release(&mut self, socket: SocketId) {
        let index = socket_index(socket);
        self.tcp_listener_children[index] = None;
        if self.tcp.remove(index).is_none() {
            return;
        }
        self.clear_tcp_timer(index);
        self.clear_tcp_receive_backpressure(index);
        self.remove_tcp_accepts_for(socket);
    }

    fn effective_path_frame_mtu(&self, key: PathMtuKey) -> usize {
        let path_frame_mtu = self
            .path_mtu
            .get(key)
            .and_then(|mtu| mtu.checked_add(EthernetFrame::HEADER_LEN))
            .unwrap_or(self.config.mtu);
        self.config.mtu.min(path_frame_mtu)
    }

    fn tcp_segment_mtu_for(&self, local: TcpEndpoint, remote: TcpEndpoint) -> usize {
        let packet_header_len = match (local.address, remote.address) {
            (IpAddress::Ipv4(_), IpAddress::Ipv4(_)) => {
                EthernetFrame::HEADER_LEN + Ipv4Packet::MIN_HEADER_LEN
            }
            (IpAddress::Ipv6(_), IpAddress::Ipv6(_)) => {
                EthernetFrame::HEADER_LEN + Ipv6Packet::HEADER_LEN
            }
            _ => panic!("TCP PMTU endpoints must have matching address families"),
        };
        self.effective_path_frame_mtu(PathMtuKey::new(local.address, remote.address))
            .checked_sub(packet_header_len)
            .unwrap_or_else(|| panic!("path MTU cannot hold IP and Ethernet headers"))
    }

    fn ensure_path_packet_fits(
        &self,
        key: PathMtuKey,
        header_len: usize,
        payload_len: usize,
    ) -> Result<(), StackError> {
        let frame_len = header_len
            .checked_add(payload_len)
            .ok_or(StackError::PacketTooLarge)?;
        if frame_len > self.effective_path_frame_mtu(key) {
            return Err(StackError::PacketTooLarge);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IcmpDestinationUnreachable<'a> {
    Ipv4(crate::packet::Icmpv4DestinationUnreachable<'a>),
    Ipv6(crate::packet::Icmpv6DestinationUnreachable<'a>),
}

impl IcmpDestinationUnreachable<'_> {
    fn transport_error(self) -> Option<IcmpTransportError> {
        match self {
            Self::Ipv4(unreachable) => {
                let quoted = Ipv4Packet::parse_quoted(unreachable.original)?;
                let ports = transport_ports(quoted.protocol, quoted.payload)?;
                Some(IcmpTransportError {
                    protocol: quoted.protocol,
                    kind: IcmpTransportErrorKind::from_ipv4_code(unreachable.code)?,
                    endpoint: IcmpQuotedEndpoint {
                        local: IpAddress::Ipv4(quoted.source),
                        local_port: ports.source,
                        remote: IpAddress::Ipv4(quoted.destination),
                        remote_port: ports.destination,
                    },
                })
            }
            Self::Ipv6(unreachable) => {
                let quoted = Ipv6Packet::parse_quoted(unreachable.original)?;
                let ports = transport_ports(quoted.next_header, quoted.payload)?;
                Some(IcmpTransportError {
                    protocol: quoted.next_header,
                    kind: IcmpTransportErrorKind::from_ipv6_code(unreachable.code)?,
                    endpoint: IcmpQuotedEndpoint {
                        local: IpAddress::Ipv6(quoted.source),
                        local_port: ports.source,
                        remote: IpAddress::Ipv6(quoted.destination),
                        remote_port: ports.destination,
                    },
                })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IcmpTransportError {
    protocol: crate::IpProtocol,
    endpoint: IcmpQuotedEndpoint,
    kind: IcmpTransportErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IcmpTransportErrorKind {
    NetworkUnreachable,
    HostUnreachable,
    ProtocolUnreachable,
    PortUnreachable,
    CommunicationProhibited,
}

impl IcmpTransportErrorKind {
    const fn from_ipv4_code(code: Icmpv4DestinationUnreachableCode) -> Option<Self> {
        match code {
            Icmpv4DestinationUnreachableCode::Net
            | Icmpv4DestinationUnreachableCode::NetUnknown
            | Icmpv4DestinationUnreachableCode::NetTos => Some(Self::NetworkUnreachable),
            Icmpv4DestinationUnreachableCode::Host
            | Icmpv4DestinationUnreachableCode::HostUnknown
            | Icmpv4DestinationUnreachableCode::SourceHostIsolated
            | Icmpv4DestinationUnreachableCode::HostTos => Some(Self::HostUnreachable),
            Icmpv4DestinationUnreachableCode::Protocol => Some(Self::ProtocolUnreachable),
            Icmpv4DestinationUnreachableCode::Port => Some(Self::PortUnreachable),
            Icmpv4DestinationUnreachableCode::NetProhibited
            | Icmpv4DestinationUnreachableCode::HostProhibited
            | Icmpv4DestinationUnreachableCode::CommunicationProhibited => {
                Some(Self::CommunicationProhibited)
            }
            Icmpv4DestinationUnreachableCode::FragmentationNeeded { .. }
            | Icmpv4DestinationUnreachableCode::SourceRouteFailed
            | Icmpv4DestinationUnreachableCode::HostPrecedenceViolation
            | Icmpv4DestinationUnreachableCode::PrecedenceCutoff => None,
        }
    }

    const fn from_ipv6_code(code: Icmpv6DestinationUnreachableCode) -> Option<Self> {
        match code {
            Icmpv6DestinationUnreachableCode::NoRoute => Some(Self::NetworkUnreachable),
            Icmpv6DestinationUnreachableCode::AddressUnreachable
            | Icmpv6DestinationUnreachableCode::BeyondScope => Some(Self::HostUnreachable),
            Icmpv6DestinationUnreachableCode::PortUnreachable => Some(Self::PortUnreachable),
            Icmpv6DestinationUnreachableCode::AdminProhibited
            | Icmpv6DestinationUnreachableCode::RejectRoute => Some(Self::CommunicationProhibited),
            Icmpv6DestinationUnreachableCode::SourceAddressFailed => None,
        }
    }

    const fn into_udp(self) -> UdpSocketError {
        match self {
            Self::NetworkUnreachable => UdpSocketError::RemoteNetworkUnreachable,
            Self::HostUnreachable => UdpSocketError::RemoteHostUnreachable,
            Self::ProtocolUnreachable => UdpSocketError::RemoteProtocolUnreachable,
            Self::PortUnreachable => UdpSocketError::RemotePortUnreachable,
            Self::CommunicationProhibited => UdpSocketError::RemoteCommunicationProhibited,
        }
    }

    const fn into_tcp(self) -> TcpConnectTerminalError {
        match self {
            Self::NetworkUnreachable => TcpConnectTerminalError::RemoteNetworkUnreachable,
            Self::HostUnreachable => TcpConnectTerminalError::RemoteHostUnreachable,
            Self::ProtocolUnreachable => TcpConnectTerminalError::RemoteProtocolUnreachable,
            Self::PortUnreachable => TcpConnectTerminalError::RemotePortUnreachable,
            Self::CommunicationProhibited => TcpConnectTerminalError::RemoteCommunicationProhibited,
        }
    }
}

fn transport_ports(protocol: crate::IpProtocol, payload: &[u8]) -> Option<crate::TransportPorts> {
    match protocol {
        crate::IpProtocol::Tcp => TcpPacket::parse_ports(payload),
        crate::IpProtocol::Udp => UdpPacket::parse_ports(payload),
        crate::IpProtocol::Icmp | crate::IpProtocol::Icmpv6 => None,
    }
}

fn is_replaceable_tcp_ack(header: TcpHeader, payload: &[u8]) -> bool {
    payload.is_empty() && header.flags == TcpFlags::ACK
}

fn tcp_listener_addresses_overlap(left: TcpEndpoint, right: TcpEndpoint) -> bool {
    if left.port != right.port {
        return false;
    }
    match (left.address, right.address) {
        (IpAddress::Ipv4(left), IpAddress::Ipv4(right)) => {
            left.is_unspecified() || right.is_unspecified() || left == right
        }
        (IpAddress::Ipv6(left), IpAddress::Ipv6(right)) => {
            left.is_unspecified() || right.is_unspecified() || left == right
        }
        _ => false,
    }
}

fn tcp_socket_index_keys<C>(socket: &TcpSocket<C>) -> (Option<TcpEndpointKey>, Option<TcpEndpoint>)
where
    C: CongestionControl,
{
    if socket.state() == crate::TcpState::Closed {
        return (None, None);
    }
    tcp_socket_endpoint_keys(socket)
}

fn tcp_socket_endpoint_keys<C>(
    socket: &TcpSocket<C>,
) -> (Option<TcpEndpointKey>, Option<TcpEndpoint>)
where
    C: CongestionControl,
{
    let endpoint_key = socket
        .local_endpoint()
        .zip(socket.remote_endpoint())
        .map(|(local, remote)| TcpEndpointKey::new(local, remote));
    let listener_key = socket
        .local_endpoint()
        .filter(|_| socket.remote_endpoint().is_none());
    (endpoint_key, listener_key)
}

fn tcp_segment_sequence_len(packet: TcpPacket<'_>) -> u32 {
    u32::try_from(packet.payload.len())
        .unwrap_or_else(|_| panic!("TCP segment payload length exceeds u32"))
        .saturating_add(u32::from(packet.flags.contains(TcpFlags::SYN)))
        .saturating_add(u32::from(packet.flags.contains(TcpFlags::FIN)))
}

fn optional_ip_addresses_overlap(left: Option<IpAddress>, right: Option<IpAddress>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left == right || ip_address_unspecified_overlap(left, right),
        (Some(_), None) | (None, Some(_)) | (None, None) => true,
    }
}

fn optional_udp_remotes_overlap(left: Option<UdpEndpoint>, right: Option<UdpEndpoint>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.port == right.port
                && (left.address == right.address
                    || ip_address_unspecified_overlap(left.address, right.address))
        }
        (Some(_), None) | (None, Some(_)) | (None, None) => true,
    }
}

fn encode_tcp_ipv4_frame(
    frame: &mut PacketBuffer,
    path: FramePath<Ipv4Address>,
    segment: TcpEgress<'_>,
    identification: u16,
    checksum: TransportChecksum,
) -> Result<(), StackError> {
    let TcpEgress {
        header,
        options,
        payload,
        hop_limit,
    } = segment;
    let FramePath {
        destination_mac,
        local_mac,
        source,
        destination,
    } = path;
    let storage = frame.storage_mut();
    let mut offset =
        EthernetFrame::encode_header(storage, destination_mac, local_mac, EthernetProtocol::Ipv4)
            .ok_or(StackError::OutputQueueFull)?;
    let tcp_len = TcpPacket::MIN_HEADER_LEN
        .checked_add(options.encoded_len())
        .and_then(|header_len| header_len.checked_add(payload.len()))
        .ok_or(StackError::OutputQueueFull)?;
    offset += Ipv4Packet::encode_header(
        &mut storage[offset..],
        source,
        destination,
        crate::IpProtocol::Tcp,
        tcp_len,
        identification,
        hop_limit,
    )
    .ok_or(StackError::OutputQueueFull)?;
    let transport_offset = offset;
    match payload {
        TcpTxPayload::Flat(payload) => {
            offset += TcpPacket::encode_with_options(
                &mut storage[offset..],
                IpAddress::Ipv4(source),
                IpAddress::Ipv4(destination),
                header,
                options,
                payload,
                checksum,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
        }
        TcpTxPayload::Scatter(_) | TcpTxPayload::Segmented { .. } => {
            assert!(
                matches!(checksum, TransportChecksum::DeviceOffload),
                "scatter TCP frames require transmit checksum offload"
            );
            let scatter = payload
                .scatter_bytes()
                .expect("scatter TCP payload lost its refcounted bytes");
            offset += TcpPacket::encode_headers_scatter(
                &mut storage[offset..],
                IpAddress::Ipv4(source),
                IpAddress::Ipv4(destination),
                header,
                options,
                scatter.len(),
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            frame.set_payload(Some(scatter.clone()));
            if let TcpTxPayload::Segmented { segment_bytes, .. } = payload {
                frame.set_tx_segmentation(Some(TxSegmentation {
                    ipv6: false,
                    header_len: u16::try_from(offset)
                        .expect("TCP frame header prefix exceeds a virtio hdr_len"),
                    segment_bytes,
                    ecn: header.flags.contains(TcpFlags::CWR),
                }));
            }
        }
    }
    if matches!(checksum, TransportChecksum::DeviceOffload) {
        frame.set_tx_checksum(Some(TxChecksum {
            start: transport_offset as u16,
            offset: TCP_CHECKSUM_FIELD_OFFSET,
        }));
    }
    Ok(())
}

fn encode_tcp_ipv6_frame(
    frame: &mut PacketBuffer,
    path: FramePath<Ipv6Address>,
    segment: TcpEgress<'_>,
    checksum: TransportChecksum,
) -> Result<(), StackError> {
    let TcpEgress {
        header,
        options,
        payload,
        hop_limit,
    } = segment;
    let FramePath {
        destination_mac,
        local_mac,
        source,
        destination,
    } = path;
    let storage = frame.storage_mut();
    let mut offset =
        EthernetFrame::encode_header(storage, destination_mac, local_mac, EthernetProtocol::Ipv6)
            .ok_or(StackError::OutputQueueFull)?;
    let tcp_len = TcpPacket::MIN_HEADER_LEN
        .checked_add(options.encoded_len())
        .and_then(|header_len| header_len.checked_add(payload.len()))
        .ok_or(StackError::OutputQueueFull)?;
    offset += Ipv6Packet::encode_header(
        &mut storage[offset..],
        source,
        destination,
        crate::IpProtocol::Tcp,
        tcp_len,
        hop_limit,
    )
    .ok_or(StackError::OutputQueueFull)?;
    let transport_offset = offset;
    match payload {
        TcpTxPayload::Flat(payload) => {
            offset += TcpPacket::encode_with_options(
                &mut storage[offset..],
                IpAddress::Ipv6(source),
                IpAddress::Ipv6(destination),
                header,
                options,
                payload,
                checksum,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
        }
        TcpTxPayload::Scatter(_) | TcpTxPayload::Segmented { .. } => {
            assert!(
                matches!(checksum, TransportChecksum::DeviceOffload),
                "scatter TCP frames require transmit checksum offload"
            );
            let scatter = payload
                .scatter_bytes()
                .expect("scatter TCP payload lost its refcounted bytes");
            offset += TcpPacket::encode_headers_scatter(
                &mut storage[offset..],
                IpAddress::Ipv6(source),
                IpAddress::Ipv6(destination),
                header,
                options,
                scatter.len(),
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            frame.set_payload(Some(scatter.clone()));
            if let TcpTxPayload::Segmented { segment_bytes, .. } = payload {
                frame.set_tx_segmentation(Some(TxSegmentation {
                    ipv6: true,
                    header_len: u16::try_from(offset)
                        .expect("TCP frame header prefix exceeds a virtio hdr_len"),
                    segment_bytes,
                    ecn: header.flags.contains(TcpFlags::CWR),
                }));
            }
        }
    }
    if matches!(checksum, TransportChecksum::DeviceOffload) {
        frame.set_tx_checksum(Some(TxChecksum {
            start: transport_offset as u16,
            offset: TCP_CHECKSUM_FIELD_OFFSET,
        }));
    }
    Ok(())
}

/// Returns the `start..end` byte range of `subset` within `parent`.
/// Used to convert a borrowed slice that the parser produced into
/// an owning `Bytes::slice` view of the parent buffer.
///
/// Panics if `subset` is not a sub-slice of `parent`. Callers
/// always derive `subset` from `parent` via parsing helpers, so
/// this is a programming-error check rather than a runtime
/// validation.
fn bytes_subrange(parent: &[u8], subset: &[u8]) -> core::ops::Range<usize> {
    let parent_start = parent.as_ptr() as usize;
    let parent_end = parent_start + parent.len();
    let subset_start = subset.as_ptr() as usize;
    let subset_end = subset_start + subset.len();
    assert!(
        subset_start >= parent_start && subset_end <= parent_end,
        "bytes_subrange: subset is not within parent"
    );
    let start = subset_start - parent_start;
    start..start + subset.len()
}

fn socket_id(index: usize) -> SocketId {
    let raw = u32::try_from(index + 1).unwrap_or_else(|_| panic!("socket index exceeds u32"));
    SocketId::new(raw)
}

fn udp_socket_id(index: usize) -> UdpSocketId {
    let raw = u32::try_from(index + 1).unwrap_or_else(|_| panic!("UDP socket index exceeds u32"));
    UdpSocketId::new(raw)
}

fn socket_index(socket: SocketId) -> usize {
    usize::try_from(socket.raw() - 1)
        .unwrap_or_else(|_| panic!("socket id does not fit into usize"))
}

fn udp_socket_index(socket: UdpSocketId) -> usize {
    usize::try_from(socket.raw() - 1)
        .unwrap_or_else(|_| panic!("UDP socket id does not fit into usize"))
}

const fn socket_slot_chunk(index: usize) -> (usize, usize) {
    (
        index / TCP_SOCKET_SLAB_CHUNK_SLOTS,
        index % TCP_SOCKET_SLAB_CHUNK_SLOTS,
    )
}

fn mix_endpoint(mut hash: u64, endpoint: TcpEndpoint) -> u64 {
    hash = mix_ip_address(hash, endpoint.address);
    mix_u16(hash, endpoint.port)
}

fn wildcard_endpoint(endpoint: TcpEndpoint) -> TcpEndpoint {
    TcpEndpoint {
        address: match endpoint.address {
            IpAddress::Ipv4(_) => IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            IpAddress::Ipv6(_) => IpAddress::Ipv6(Ipv6Address::UNSPECIFIED),
        },
        port: endpoint.port,
    }
}

fn ip_address_families_match(left: IpAddress, right: IpAddress) -> bool {
    matches!(
        (left, right),
        (IpAddress::Ipv4(_), IpAddress::Ipv4(_)) | (IpAddress::Ipv6(_), IpAddress::Ipv6(_))
    )
}

fn ip_address_unspecified_overlap(left: IpAddress, right: IpAddress) -> bool {
    match (left, right) {
        (IpAddress::Ipv4(left), IpAddress::Ipv4(right)) => {
            left.is_unspecified() || right.is_unspecified()
        }
        (IpAddress::Ipv6(left), IpAddress::Ipv6(right)) => {
            left.is_unspecified() || right.is_unspecified()
        }
        _ => false,
    }
}

fn mix_ip_address(hash: u64, address: IpAddress) -> u64 {
    match address {
        IpAddress::Ipv4(address) => mix_bytes(hash, &address.octets()),
        IpAddress::Ipv6(address) => mix_bytes(hash, &address.octets()),
    }
}

fn mix_optional_ip_address(hash: u64, address: Option<IpAddress>) -> u64 {
    match address {
        Some(address) => mix_ip_address(mix_u8(hash, 1), address),
        None => mix_u8(hash, 0),
    }
}

fn mix_u8(hash: u64, value: u8) -> u64 {
    mix_bytes(hash, &[value])
}

fn mix_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn mix_u16(hash: u64, value: u16) -> u64 {
    mix_bytes(hash, &value.to_be_bytes())
}

fn key_distance(left: PathMtuKey, right: PathMtuKey) -> usize {
    left.hash().abs_diff(right.hash())
}

fn reassembly_key_distance(left: Ipv4ReassemblyKey, right: Ipv4ReassemblyKey) -> usize {
    left.hash().abs_diff(right.hash())
}

fn next_lower_pmtu_plateau(from: usize) -> Option<usize> {
    [
        65_535usize,
        32_000,
        17_914,
        8_166,
        4_352,
        2_002,
        1_492,
        1_280,
        1_006,
        508,
        296,
        IPV4_MINIMUM_LINK_MTU,
    ]
    .into_iter()
    .find(|plateau| *plateau < from)
}

fn ipv4_directed_broadcast(cidr: Ipv4Cidr) -> Option<Ipv4Address> {
    if cidr.prefix_len() >= 31 {
        return None;
    }
    let mask = if cidr.prefix_len() == 0 {
        0
    } else {
        u32::MAX << (32 - cidr.prefix_len())
    };
    let network = u32::from_be_bytes(cidr.address().octets()) & mask;
    Some(Ipv4Address::new((network | !mask).to_be_bytes()))
}

fn ipv4_addresses_own(addresses: &[Ipv4Cidr], address: Ipv4Address) -> bool {
    addresses.iter().any(|local| local.address() == address)
}

fn should_send_icmpv4_error_for_destination(destination: Ipv4Address) -> bool {
    !destination.is_multicast() && destination != Ipv4Address::BROADCAST
}

fn ipv4_multicast_mac(address: Ipv4Address) -> EthernetAddress {
    let octets = address.octets();
    [0x01, 0x00, 0x5e, octets[1] & 0x7f, octets[2], octets[3]]
}

fn ipv6_multicast_mac(address: Ipv6Address) -> EthernetAddress {
    let octets = address.octets();
    [0x33, 0x33, octets[12], octets[13], octets[14], octets[15]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewReno;
    use crate::TcpFlags;
    use alloc::vec;

    const LOCAL_MAC: EthernetAddress = [0x02, 0, 0, 0, 0, 1];
    const PEER_MAC: EthernetAddress = [0x02, 0, 0, 0, 0, 2];

    fn tcp_segment(
        source: Ipv4Address,
        destination: Ipv4Address,
        header: TcpHeader,
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        tcp_segment_with_payload(source, destination, header, &[])
    }

    fn tcp_segment_with_payload(
        source: Ipv4Address,
        destination: Ipv4Address,
        header: TcpHeader,
        payload: &[u8],
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        let mut segment = [0u8; crate::ETHERNET_FRAME_BYTES];
        let len = TcpPacket::encode(
            &mut segment,
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            header,
            payload,
            TransportChecksum::Software,
        )
        .expect("test TCP segment should fit");
        (segment, len)
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestTcpSynAck {
        sequence: u32,
        acknowledgement: u32,
        flags: TcpFlags,
        maximum_segment_size: Option<u16>,
        window_scale: Option<u8>,
        sack_permitted: bool,
        timestamp_present: bool,
    }

    fn drive_passive_open_to_syn_ack(
        stack: &mut Stack,
        peer: Ipv4Address,
        local: Ipv4Address,
        remote_port: u16,
        now: u64,
    ) -> TestTcpSynAck {
        let (syn, syn_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: remote_port,
                destination_port: 8080,
                sequence: 10,
                acknowledgement: 0,
                flags: TcpFlags::SYN,
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn[..syn_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(now),
            )
            .expect("SYN should be handled by listener");
        stack
            .drive_tcp(StackInstant::from_nanos(now))
            .expect("SYN-ACK should be queued");

        let frame = stack
            .take_outbound()
            .expect("SYN-ACK frame should be queued");
        let bytes = Bytes::copy_from_slice(frame.as_slice());
        let ethernet = EthernetFrame::parse(bytes.as_ref()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let tcp = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        TestTcpSynAck {
            sequence: tcp.sequence,
            acknowledgement: tcp.acknowledgement,
            flags: tcp.flags,
            maximum_segment_size: tcp.options.maximum_segment_size(),
            window_scale: tcp.options.window_scale(),
            sack_permitted: tcp.options.sack_permitted(),
            timestamp_present: tcp.options.timestamp().is_some(),
        }
    }

    fn open_established_tcp_stack(local: Ipv4Address, peer: Ipv4Address) -> (Stack, SocketId) {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let socket = stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");
        let (syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN-ACK should establish the socket");
        stack
            .drive_tcp(StackInstant::from_nanos(1))
            .expect("handshake ACK should be queued");
        let _ = stack
            .take_outbound()
            .expect("handshake ACK frame should be queued");
        while stack.take_event().is_some() {}
        (stack, socket)
    }

    fn open_pending_tcp_connect_stack(
        config: StackConfig,
    ) -> (Stack, SocketId, Ipv4Address, Ipv4Address) {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(config);
        let socket = stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");
        (stack, socket, local, peer)
    }

    fn udp_datagram(
        source: Ipv4Address,
        source_port: u16,
        destination: Ipv4Address,
        destination_port: u16,
        payload: &[u8],
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        let mut datagram = [0u8; crate::ETHERNET_FRAME_BYTES];
        let len = UdpPacket::encode(
            &mut datagram,
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            source_port,
            destination_port,
            payload,
            TransportChecksum::Software,
        )
        .expect("test UDP datagram should fit");
        (datagram, len)
    }

    fn udp_datagram_v6(
        source: Ipv6Address,
        source_port: u16,
        destination: Ipv6Address,
        destination_port: u16,
        payload: &[u8],
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        let mut datagram = [0u8; crate::ETHERNET_FRAME_BYTES];
        let len = UdpPacket::encode(
            &mut datagram,
            IpAddress::Ipv6(source),
            IpAddress::Ipv6(destination),
            source_port,
            destination_port,
            payload,
            TransportChecksum::Software,
        )
        .expect("test UDP datagram should fit");
        (datagram, len)
    }

    fn ipv4_udp_frame(
        source: Ipv4Address,
        source_port: u16,
        destination: Ipv4Address,
        destination_port: u16,
        payload: &[u8],
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        let mut frame = [0u8; crate::ETHERNET_FRAME_BYTES];
        let mut offset =
            EthernetFrame::encode_header(&mut frame, LOCAL_MAC, PEER_MAC, EthernetProtocol::Ipv4)
                .expect("test Ethernet header should fit");
        offset += Ipv4Packet::encode_header(
            &mut frame[offset..],
            source,
            destination,
            crate::IpProtocol::Udp,
            UdpPacket::HEADER_LEN + payload.len(),
            7,
            64,
        )
        .expect("test IPv4 header should fit");
        offset += UdpPacket::encode(
            &mut frame[offset..],
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            source_port,
            destination_port,
            payload,
            TransportChecksum::Software,
        )
        .expect("test UDP datagram should fit");
        (frame, offset)
    }

    fn ipv4_fragment_frame(
        source: Ipv4Address,
        destination: Ipv4Address,
        protocol: crate::IpProtocol,
        identification: u16,
        fragment_offset: usize,
        more_fragments: bool,
        payload: &[u8],
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        assert!(
            fragment_offset.is_multiple_of(IPV4_FRAGMENT_BLOCK_SIZE),
            "test fragment offset must be in IPv4 fragment blocks"
        );
        let mut frame = [0u8; crate::ETHERNET_FRAME_BYTES];
        let mut offset =
            EthernetFrame::encode_header(&mut frame, LOCAL_MAC, PEER_MAC, EthernetProtocol::Ipv4)
                .expect("test Ethernet header should fit");
        let ipv4_header = offset;
        offset += Ipv4Packet::encode_header(
            &mut frame[offset..],
            source,
            destination,
            protocol,
            payload.len(),
            identification,
            64,
        )
        .expect("test IPv4 header should fit");
        let fragment_offset_blocks = u16::try_from(fragment_offset / IPV4_FRAGMENT_BLOCK_SIZE)
            .expect("test fragment offset should fit IPv4 field");
        let fragment = fragment_offset_blocks | if more_fragments { 0x2000 } else { 0 };
        frame[ipv4_header + 6..ipv4_header + 8].copy_from_slice(&fragment.to_be_bytes());
        frame[ipv4_header + 10..ipv4_header + 12].copy_from_slice(&0u16.to_be_bytes());
        let checksum = ipv4_checksum(&frame[ipv4_header..ipv4_header + Ipv4Packet::MIN_HEADER_LEN]);
        frame[ipv4_header + 10..ipv4_header + 12].copy_from_slice(&checksum.to_be_bytes());
        frame[offset..offset + payload.len()].copy_from_slice(payload);
        offset += payload.len();
        (frame, offset)
    }

    fn ipv4_udp_fragments(
        source: Ipv4Address,
        source_port: u16,
        destination: Ipv4Address,
        destination_port: u16,
        payload: &[u8],
        split_at: usize,
        identification: u16,
    ) -> (
        ([u8; crate::ETHERNET_FRAME_BYTES], usize),
        ([u8; crate::ETHERNET_FRAME_BYTES], usize),
    ) {
        assert!(
            split_at.is_multiple_of(IPV4_FRAGMENT_BLOCK_SIZE),
            "test first UDP fragment must be 8-byte aligned"
        );
        let (datagram, datagram_len) =
            udp_datagram(source, source_port, destination, destination_port, payload);
        let transport = &datagram[..datagram_len];
        let first = ipv4_fragment_frame(
            source,
            destination,
            crate::IpProtocol::Udp,
            identification,
            0,
            true,
            &transport[..split_at],
        );
        let second = ipv4_fragment_frame(
            source,
            destination,
            crate::IpProtocol::Udp,
            identification,
            split_at,
            false,
            &transport[split_at..],
        );
        (first, second)
    }

    fn ipv4_tcp_fragments(
        source: Ipv4Address,
        destination: Ipv4Address,
        header: TcpHeader,
        payload: &[u8],
        split_at: usize,
        identification: u16,
    ) -> (
        ([u8; crate::ETHERNET_FRAME_BYTES], usize),
        ([u8; crate::ETHERNET_FRAME_BYTES], usize),
    ) {
        assert!(
            split_at.is_multiple_of(IPV4_FRAGMENT_BLOCK_SIZE),
            "test first TCP fragment must be 8-byte aligned"
        );
        let (segment, segment_len) = tcp_segment_with_payload(source, destination, header, payload);
        let transport = &segment[..segment_len];
        let first = ipv4_fragment_frame(
            source,
            destination,
            crate::IpProtocol::Tcp,
            identification,
            0,
            true,
            &transport[..split_at],
        );
        let second = ipv4_fragment_frame(
            source,
            destination,
            crate::IpProtocol::Tcp,
            identification,
            split_at,
            false,
            &transport[split_at..],
        );
        (first, second)
    }

    fn ipv6_udp_frame(
        source: Ipv6Address,
        source_port: u16,
        destination: Ipv6Address,
        destination_port: u16,
        payload: &[u8],
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        let mut frame = [0u8; crate::ETHERNET_FRAME_BYTES];
        let mut offset =
            EthernetFrame::encode_header(&mut frame, LOCAL_MAC, PEER_MAC, EthernetProtocol::Ipv6)
                .expect("test Ethernet header should fit");
        offset += Ipv6Packet::encode_header(
            &mut frame[offset..],
            source,
            destination,
            crate::IpProtocol::Udp,
            UdpPacket::HEADER_LEN + payload.len(),
            64,
        )
        .expect("test IPv6 header should fit");
        offset += UdpPacket::encode(
            &mut frame[offset..],
            IpAddress::Ipv6(source),
            IpAddress::Ipv6(destination),
            source_port,
            destination_port,
            payload,
            TransportChecksum::Software,
        )
        .expect("test UDP datagram should fit");
        (frame, offset)
    }

    fn ipv4_icmp_port_unreachable_for_udp_frame(
        router: Ipv4Address,
        local: Ipv4Address,
        quoted_frame: &[u8],
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        ipv4_icmp_unreachable_for_udp_frame(
            router,
            local,
            quoted_frame,
            Icmpv4Packet::DESTINATION_UNREACHABLE_PORT_CODE,
        )
    }

    fn ipv4_icmp_unreachable_for_udp_frame(
        router: Ipv4Address,
        local: Ipv4Address,
        quoted_frame: &[u8],
        code: Icmpv4DestinationUnreachableCode,
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        let ethernet =
            EthernetFrame::parse(quoted_frame).expect("quoted Ethernet frame should parse");
        let quoted = ethernet.payload;
        let quoted_len = quoted.len().min(Ipv4Packet::MIN_HEADER_LEN + 8);
        ipv4_icmp_unreachable(router, local, code, &quoted[..quoted_len])
    }

    fn ipv4_icmp_port_unreachable_for_tcp(
        router: Ipv4Address,
        local: Ipv4Address,
        source: Ipv4Address,
        destination: Ipv4Address,
        header: TcpHeader,
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        ipv4_icmp_unreachable_for_tcp(
            router,
            local,
            source,
            destination,
            header,
            Icmpv4Packet::DESTINATION_UNREACHABLE_PORT_CODE,
        )
    }

    fn ipv4_icmp_unreachable_for_tcp(
        router: Ipv4Address,
        local: Ipv4Address,
        source: Ipv4Address,
        destination: Ipv4Address,
        header: TcpHeader,
        code: Icmpv4DestinationUnreachableCode,
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        let mut quoted = [0u8; crate::ETHERNET_FRAME_BYTES];
        let mut offset = Ipv4Packet::encode_header(
            &mut quoted,
            source,
            destination,
            crate::IpProtocol::Tcp,
            TcpPacket::MIN_HEADER_LEN,
            9,
            64,
        )
        .expect("quoted IPv4 header should fit");
        offset += TcpPacket::encode(
            &mut quoted[offset..],
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            header,
            &[],
            TransportChecksum::Software,
        )
        .expect("quoted TCP packet should fit");
        ipv4_icmp_unreachable(
            router,
            local,
            code,
            &quoted[..offset.min(Ipv4Packet::MIN_HEADER_LEN + 8)],
        )
    }

    fn ipv4_icmp_unreachable(
        router: Ipv4Address,
        local: Ipv4Address,
        code: Icmpv4DestinationUnreachableCode,
        quoted: &[u8],
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        let mut frame = [0u8; crate::ETHERNET_FRAME_BYTES];
        let icmp_len = Icmpv4Packet::HEADER_LEN + quoted.len();
        let mut offset =
            EthernetFrame::encode_header(&mut frame, LOCAL_MAC, PEER_MAC, EthernetProtocol::Ipv4)
                .expect("test Ethernet header should fit");
        offset += Ipv4Packet::encode_header(
            &mut frame[offset..],
            router,
            local,
            crate::IpProtocol::Icmp,
            icmp_len,
            11,
            64,
        )
        .expect("ICMPv4 outer header should fit");
        offset += Icmpv4Packet::encode_destination_unreachable(&mut frame[offset..], code, quoted)
            .expect("ICMPv4 unreachable should fit");
        (frame, offset)
    }

    fn ipv6_icmp_port_unreachable_for_udp_frame(
        router: Ipv6Address,
        local: Ipv6Address,
        quoted_frame: &[u8],
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        ipv6_icmp_unreachable_for_udp_frame(
            router,
            local,
            quoted_frame,
            Icmpv6Packet::DESTINATION_UNREACHABLE_PORT_CODE,
        )
    }

    fn ipv6_icmp_unreachable_for_udp_frame(
        router: Ipv6Address,
        local: Ipv6Address,
        quoted_frame: &[u8],
        code: Icmpv6DestinationUnreachableCode,
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        let ethernet =
            EthernetFrame::parse(quoted_frame).expect("quoted Ethernet frame should parse");
        let quoted = ethernet.payload;
        let mut frame = [0u8; crate::ETHERNET_FRAME_BYTES];
        let icmp_len = Icmpv6Packet::DESTINATION_UNREACHABLE_HEADER_LEN + quoted.len();
        let mut offset =
            EthernetFrame::encode_header(&mut frame, LOCAL_MAC, PEER_MAC, EthernetProtocol::Ipv6)
                .expect("test Ethernet header should fit");
        offset += Ipv6Packet::encode_header(
            &mut frame[offset..],
            router,
            local,
            crate::IpProtocol::Icmpv6,
            icmp_len,
            64,
        )
        .expect("ICMPv6 outer header should fit");
        offset += Icmpv6Packet::encode_destination_unreachable(
            &mut frame[offset..],
            router,
            local,
            code,
            quoted,
        )
        .expect("ICMPv6 unreachable should fit");
        (frame, offset)
    }

    fn ipv6_icmp_packet_too_big_for_frame(
        router: Ipv6Address,
        local: Ipv6Address,
        quoted_frame: &[u8],
        mtu: u32,
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        let ethernet =
            EthernetFrame::parse(quoted_frame).expect("quoted Ethernet frame should parse");
        let quoted = ethernet.payload;
        let mut frame = [0u8; crate::ETHERNET_FRAME_BYTES];
        let icmp_len = Icmpv6Packet::PACKET_TOO_BIG_HEADER_LEN + quoted.len();
        let mut offset =
            EthernetFrame::encode_header(&mut frame, LOCAL_MAC, PEER_MAC, EthernetProtocol::Ipv6)
                .expect("test Ethernet header should fit");
        offset += Ipv6Packet::encode_header(
            &mut frame[offset..],
            router,
            local,
            crate::IpProtocol::Icmpv6,
            icmp_len,
            64,
        )
        .expect("ICMPv6 outer header should fit");
        offset +=
            Icmpv6Packet::encode_packet_too_big(&mut frame[offset..], router, local, mtu, quoted)
                .expect("ICMPv6 packet-too-big should fit");
        (frame, offset)
    }

    fn packet_buffer(bytes: &[u8]) -> PacketBuffer {
        let mut buffer = PacketBuffer::new();
        buffer.spare_capacity_mut()[..bytes.len()].copy_from_slice(bytes);
        buffer.set_len(bytes.len());
        buffer
    }

    #[test]
    fn outbound_restore_preserves_front_order() {
        let mut queue = OutboundFrameQueue::new();
        queue.push_front(packet_buffer(b"third"));
        queue.push_front(packet_buffer(b"second"));
        queue.push_front(packet_buffer(b"first"));

        assert_eq!(
            queue.pop().expect("first frame").as_slice(),
            b"first".as_slice()
        );
        assert_eq!(
            queue.pop().expect("second frame").as_slice(),
            b"second".as_slice()
        );
        assert_eq!(
            queue.pop().expect("third frame").as_slice(),
            b"third".as_slice()
        );
    }

    #[test]
    fn outbound_slice_submit_releases_only_accepted_prefix() {
        let mut queue = OutboundFrameQueue::new();
        queue.push_front(packet_buffer(b"third"));
        queue.push_front(packet_buffer(b"second"));
        queue.push_front(packet_buffer(b"first"));

        let status = queue
            .try_submit_slices(3, |frames| {
                assert_eq!(frames.len(), 3);
                assert_eq!(frames[0].bytes, b"first".as_slice());
                assert_eq!(frames[1].bytes, b"second".as_slice());
                assert_eq!(frames[2].bytes, b"third".as_slice());
                Ok::<Option<usize>, ()>(Some(2))
            })
            .expect("slice submit should succeed");
        assert_eq!(
            status,
            OutboundBatchStatus::Submitted {
                offered: 3,
                accepted: 2,
                accepted_bytes: b"first".len() + b"second".len()
            }
        );
        assert_eq!(
            queue.pop().expect("third frame").as_slice(),
            b"third".as_slice()
        );
        assert!(queue.pop().is_none());
    }

    #[test]
    fn outbound_slice_submit_deferred_preserves_ready_queue() {
        let mut queue = OutboundFrameQueue::new();
        queue.push_front(packet_buffer(b"second"));
        queue.push_front(packet_buffer(b"first"));

        let status = queue
            .try_submit_slices(2, |frames| {
                assert_eq!(frames.len(), 2);
                Ok::<Option<usize>, ()>(None)
            })
            .expect("deferred submit should succeed");
        assert_eq!(status, OutboundBatchStatus::Deferred);
        assert_eq!(
            queue.pop().expect("first frame").as_slice(),
            b"first".as_slice()
        );
        assert_eq!(
            queue.pop().expect("second frame").as_slice(),
            b"second".as_slice()
        );
    }

    #[test]
    fn queued_tcp_ack_is_replaced_before_submission() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });

        let queued = TcpHeader {
            source_port: 49152,
            destination_port: 80,
            sequence: 8,
            acknowledgement: 100,
            flags: TcpFlags::ACK,
            window_size: u16::MAX,
        };
        let replacement = TcpHeader {
            acknowledgement: 200,
            ..queued
        };
        assert!(
            stack
                .queue_tcp_ipv4(
                    local,
                    peer,
                    TcpEgress {
                        header: queued,
                        options: TcpHeaderOptions::empty(),
                        payload: TcpTxPayload::Flat(&[]),
                        hop_limit: crate::DEFAULT_HOP_LIMIT,
                    },
                    1,
                    StackInstant::from_nanos(1),
                )
                .expect("first ACK should queue")
        );
        assert!(
            stack
                .queue_tcp_ipv4(
                    local,
                    peer,
                    TcpEgress {
                        header: replacement,
                        options: TcpHeaderOptions::empty(),
                        payload: TcpTxPayload::Flat(&[]),
                        hop_limit: crate::DEFAULT_HOP_LIMIT,
                    },
                    2,
                    StackInstant::from_nanos(2),
                )
                .expect("replacement ACK should queue")
        );

        let frame = stack
            .take_outbound()
            .expect("coalesced ACK frame should remain queued");
        assert!(stack.take_outbound().is_none());
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let ack = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert_eq!(ack.acknowledgement, replacement.acknowledgement);
        assert_eq!(ack.sequence, replacement.sequence);
        assert_eq!(ack.flags, TcpFlags::ACK);
        assert!(ack.payload.is_empty());
    }

    /// A burst of out-of-order segments leaves three duplicate ACKs on
    /// the wire, so the peer fast-retransmits the hole.
    ///
    /// Two things used to collapse the burst into one frame: the socket
    /// recorded a single pending-ACK flag, and the outbound queue folded
    /// a queued ACK into any later one carrying the same acknowledgement.
    /// A peer without SACK then saw at most one duplicate, never reached
    /// the three-duplicate threshold, and waited out its retransmission
    /// timer instead — the 1.4–1.9 s stalls behind the aarch64 bench
    /// lane's `tcp-throughput` read timeout.
    #[test]
    fn an_out_of_order_burst_leaves_duplicate_acks_on_the_wire() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let (mut stack, _socket) =
            established_tcp_stack(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let payload = [0u8; 100];

        let deliver = |stack: &mut Stack, sequence: u32, now: u64| {
            let (segment, len) = tcp_segment_with_payload(
                peer,
                local,
                TcpHeader {
                    source_port: 80,
                    destination_port: 49152,
                    sequence,
                    acknowledgement: 8,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                },
                &payload,
            );
            stack
                .receive_tcp(
                    IpAddress::Ipv4(peer),
                    IpAddress::Ipv4(local),
                    &Bytes::copy_from_slice(&segment[..len]),
                    RxFrameOffload::none(),
                    StackInstant::from_nanos(now),
                )
                .expect("test TCP segment should be accepted");
        };

        // In-order data, acknowledged on its own drive: this is the
        // cumulative acknowledgement the duplicates repeat.
        deliver(&mut stack, 101, 2);
        stack
            .drive_tcp(StackInstant::from_nanos(2))
            .expect("cumulative ACK should be queued");
        while stack.take_outbound().is_some() {}

        // One received burst: the segment at 201 is lost and everything
        // behind it arrives before the stack is driven again, exactly as
        // the packet pump drains a receive batch.
        for index in 0..4u32 {
            deliver(&mut stack, 301 + index * 100, 3 + u64::from(index));
        }
        stack
            .drive_tcp(StackInstant::from_nanos(7))
            .expect("duplicate ACKs should be queued");

        let mut duplicates = 0;
        while let Some(frame) = stack.take_outbound() {
            let ethernet =
                EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
            let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
            let ack = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
            assert_eq!(ack.acknowledgement, 201);
            assert!(ack.payload.is_empty());
            duplicates += 1;
        }
        assert!(
            duplicates >= 3,
            "an out-of-order burst must leave at least three duplicate ACKs, got {duplicates}"
        );
    }

    #[test]
    fn queued_tcp_ack_with_different_sequence_is_not_replaced() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });

        let queued = TcpHeader {
            source_port: 49152,
            destination_port: 80,
            sequence: 8,
            acknowledgement: 100,
            flags: TcpFlags::ACK,
            window_size: u16::MAX,
        };
        let next_sequence = TcpHeader {
            sequence: 9,
            acknowledgement: 200,
            ..queued
        };
        assert!(
            stack
                .queue_tcp_ipv4(
                    local,
                    peer,
                    TcpEgress {
                        header: queued,
                        options: TcpHeaderOptions::empty(),
                        payload: TcpTxPayload::Flat(&[]),
                        hop_limit: crate::DEFAULT_HOP_LIMIT,
                    },
                    1,
                    StackInstant::from_nanos(1),
                )
                .expect("first ACK should queue")
        );
        assert!(
            stack
                .queue_tcp_ipv4(
                    local,
                    peer,
                    TcpEgress {
                        header: next_sequence,
                        options: TcpHeaderOptions::empty(),
                        payload: TcpTxPayload::Flat(&[]),
                        hop_limit: crate::DEFAULT_HOP_LIMIT,
                    },
                    2,
                    StackInstant::from_nanos(2),
                )
                .expect("different sequence ACK should queue")
        );

        let first = stack
            .take_outbound()
            .expect("first ACK frame should remain queued");
        let second = stack
            .take_outbound()
            .expect("second ACK frame should remain queued");
        assert!(stack.take_outbound().is_none());
        let first = EthernetFrame::parse(first.as_slice()).expect("Ethernet frame should parse");
        let first = Ipv4Packet::parse(first.payload).expect("IPv4 packet should parse");
        let first = TcpPacket::parse(first.payload).expect("TCP packet should parse");
        let second = EthernetFrame::parse(second.as_slice()).expect("Ethernet frame should parse");
        let second = Ipv4Packet::parse(second.payload).expect("IPv4 packet should parse");
        let second = TcpPacket::parse(second.payload).expect("TCP packet should parse");
        assert_eq!(first.sequence, queued.sequence);
        assert_eq!(second.sequence, next_sequence.sequence);
    }

    #[test]
    fn tcp_connect_rejects_duplicate_four_tuple() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let local = TcpEndpoint {
            address: IpAddress::Ipv4(local),
            port: 49152,
        };
        let remote = TcpEndpoint {
            address: IpAddress::Ipv4(peer),
            port: 80,
        };

        stack
            .open_tcp_connect(local, remote, 7)
            .expect("first TCP connect should allocate");

        assert_eq!(
            stack.open_tcp_connect(local, remote, 9),
            Err(StackError::AddressInUse)
        );
    }

    #[test]
    fn tcp_listen_rejects_overlapping_address_sets() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));

        stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                    port: 8080,
                },
                DEFAULT_TCP_LISTEN_BACKLOG,
            )
            .expect("wildcard TCP listener should allocate");

        assert_eq!(
            stack.open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 8080,
                },
                DEFAULT_TCP_LISTEN_BACKLOG
            ),
            Err(StackError::AddressInUse)
        );
    }

    #[test]
    fn tcp_connect_rejects_listener_address_overlap() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));

        stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                    port: 49152,
                },
                DEFAULT_TCP_LISTEN_BACKLOG,
            )
            .expect("wildcard TCP listener should allocate");

        assert_eq!(
            stack.open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            ),
            Err(StackError::AddressInUse)
        );
    }

    #[test]
    fn tcp_receive_rejects_bad_checksum() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let (mut stack, socket, _, _) = open_pending_tcp_connect_stack(StackConfig::new(
            LOCAL_MAC,
            crate::ETHERNET_FRAME_BYTES,
        ));
        let (mut syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        syn_ack[16] ^= 0x80;

        assert_eq!(
            stack.receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            ),
            Err(StackError::MalformedPacket)
        );
        assert_eq!(
            stack.tcp_connect_state(socket),
            Ok(TcpConnectState::Pending)
        );
    }

    /// A device that reports it validated the checksum is believed even
    /// when the checksum on the wire is wrong: the frame it validated is
    /// the one it delivered, and the bytes it handed over are the ones
    /// it summed.
    #[test]
    fn tcp_receive_trusts_a_device_validated_frame() {
        let config = StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES)
            .with_rx_checksum_offload(RxChecksumOffload::new(false, true, false));
        let (mut stack, socket, local, peer) = open_pending_tcp_connect_stack(config);
        let (mut syn_ack, syn_ack_len) = corrupt_tcp_syn_ack(peer, local);

        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::validated(),
                StackInstant::from_nanos(1),
            )
            .expect("TCP checksum-offloaded SYN-ACK should be accepted");
        assert_eq!(
            stack.tcp_connect_state(socket),
            Ok(TcpConnectState::Connected)
        );
        let _ = &mut syn_ack;
    }

    /// The negotiated capability alone proves nothing: a device that can
    /// validate checksums still delivers frames it did not validate, and
    /// those are verified in software like any other.
    #[test]
    fn tcp_receive_verifies_a_frame_the_device_did_not_validate() {
        let config = StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES)
            .with_rx_checksum_offload(RxChecksumOffload::new(false, true, false));
        let (mut stack, socket, local, peer) = open_pending_tcp_connect_stack(config);
        let (syn_ack, syn_ack_len) = corrupt_tcp_syn_ack(peer, local);

        assert_eq!(
            stack.receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            ),
            Err(StackError::MalformedPacket)
        );
        assert_eq!(
            stack.tcp_connect_state(socket),
            Ok(TcpConnectState::Pending)
        );
    }

    /// A frame carrying an unfinished partial checksum is completed by
    /// the stack: the sender left the pseudo-header sum in the checksum
    /// field, which is exactly what this stack's own transmit offload
    /// path writes, so the completion succeeds and the segment is
    /// delivered even though a software verification would reject it.
    #[test]
    fn tcp_receive_completes_a_partial_checksum_frame() {
        let config = StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES)
            .with_rx_checksum_offload(RxChecksumOffload::new(false, true, false));
        let (mut stack, socket, local, peer) = open_pending_tcp_connect_stack(config);
        let (mut syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        let partial = crate::tcp_pseudo_header_checksum(
            IpAddress::Ipv4(peer),
            IpAddress::Ipv4(local),
            syn_ack_len,
        );
        syn_ack[16..18].copy_from_slice(&partial.to_be_bytes());
        assert!(
            !crate::tcp_checksum_valid(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &syn_ack[..syn_ack_len],
            ),
            "a partial checksum must not pass software verification"
        );

        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::partial(0, 16),
                StackInstant::from_nanos(1),
            )
            .expect("a completed partial checksum should be accepted");
        assert_eq!(
            stack.tcp_connect_state(socket),
            Ok(TcpConnectState::Connected)
        );
    }

    /// A partial report whose stored sum does not match the packet the
    /// stack parsed completes to a checksum the segment does not carry,
    /// and the frame is dropped.
    #[test]
    fn tcp_receive_drops_an_inconsistent_partial_checksum_frame() {
        let config = StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES)
            .with_rx_checksum_offload(RxChecksumOffload::new(false, true, false));
        let (mut stack, socket, local, peer) = open_pending_tcp_connect_stack(config);
        let (mut syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        // The pseudo-header sum of a longer segment than the one that
        // actually arrived: a truncated or mis-framed delivery.
        let partial = crate::tcp_pseudo_header_checksum(
            IpAddress::Ipv4(peer),
            IpAddress::Ipv4(local),
            syn_ack_len + 8,
        );
        syn_ack[16..18].copy_from_slice(&partial.to_be_bytes());

        assert_eq!(
            stack.receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::partial(0, 16),
                StackInstant::from_nanos(1),
            ),
            Err(StackError::MalformedPacket)
        );
        assert_eq!(
            stack.tcp_connect_state(socket),
            Ok(TcpConnectState::Pending)
        );
    }

    /// A device with large receive offload coalesces several wire
    /// segments into one frame, so the receive path has to take a TCP
    /// segment far larger than the path MTU. Nothing between the
    /// Ethernet parser and the socket receive queue may cap a segment at
    /// the interface frame size: the coalesced frame is one segment, and
    /// its length is bounded by the receive window, not by the MTU.
    #[test]
    fn tcp_receive_accepts_a_coalesced_large_receive_segment() {
        const COALESCED_PAYLOAD_BYTES: usize = 16 * 1024;
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let (mut stack, socket) = open_established_tcp_stack(local, peer);
        let payload = alloc::vec![0xa5_u8; COALESCED_PAYLOAD_BYTES];
        let frame = ipv4_tcp_frame(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
            &payload,
        );
        assert!(
            frame.len() > crate::ETHERNET_FRAME_BYTES,
            "a coalesced frame must exceed the interface frame size for this test to mean anything"
        );

        stack
            .receive_rx_frame(
                RxFrame::with_offload(
                    Bytes::from(frame),
                    RxFrameOffload {
                        checksum: RxChecksumReport::Validated,
                        large_receive_segment_bytes: Some(1460),
                        flow_hash: None,
                    },
                ),
                StackInstant::from_nanos(2),
            )
            .expect("a coalesced large receive frame should be accepted");

        let TcpReadState::Data(received) = stack
            .tcp_read(socket, COALESCED_PAYLOAD_BYTES, StackInstant::from_nanos(3))
            .expect("TCP socket should still exist")
        else {
            panic!("the coalesced segment payload should be readable");
        };
        assert_eq!(received.len(), COALESCED_PAYLOAD_BYTES);
        assert!(received.iter().all(|byte| *byte == 0xa5));
    }

    /// Encodes one Ethernet/IPv4/TCP frame of any length, for segments
    /// that do not fit the fixed interface frame buffer.
    fn ipv4_tcp_frame(
        source: Ipv4Address,
        destination: Ipv4Address,
        header: TcpHeader,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut frame = alloc::vec![
            0_u8;
            EthernetFrame::HEADER_LEN
                + Ipv4Packet::MIN_HEADER_LEN
                + TcpPacket::MIN_HEADER_LEN
                + payload.len()
        ];
        let mut offset =
            EthernetFrame::encode_header(&mut frame, LOCAL_MAC, PEER_MAC, EthernetProtocol::Ipv4)
                .expect("test Ethernet header should fit");
        offset += Ipv4Packet::encode_header(
            &mut frame[offset..],
            source,
            destination,
            crate::IpProtocol::Tcp,
            TcpPacket::MIN_HEADER_LEN + payload.len(),
            7,
            64,
        )
        .expect("test IPv4 header should fit");
        offset += TcpPacket::encode(
            &mut frame[offset..],
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            header,
            payload,
            TransportChecksum::Software,
        )
        .expect("test TCP segment should fit");
        frame.truncate(offset);
        frame
    }

    fn corrupt_tcp_syn_ack(
        peer: Ipv4Address,
        local: Ipv4Address,
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        let (mut syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        syn_ack[16] ^= 0x80;
        (syn_ack, syn_ack_len)
    }

    #[test]
    fn tcp_connect_unacceptable_syn_ack_queues_reset_and_stays_pending() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let socket = stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");
        let (syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 9,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );

        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("unacceptable SYN-ACK should be handled");

        assert_eq!(
            stack.tcp_connect_state(socket),
            Ok(TcpConnectState::Pending)
        );
        let frame = stack.take_outbound().expect("RST frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let reset = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert_eq!(reset.source_port, 49152);
        assert_eq!(reset.destination_port, 80);
        assert_eq!(reset.sequence, 9);
        assert_eq!(reset.acknowledgement, 0);
        assert_eq!(reset.flags, TcpFlags::RST);
    }

    #[test]
    fn ipv4_icmp_port_unreachable_closes_pending_tcp_connect() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let router = Ipv4Address::new([192, 0, 2, 1]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let local_endpoint = TcpEndpoint {
            address: IpAddress::Ipv4(local),
            port: 49152,
        };
        let remote_endpoint = TcpEndpoint {
            address: IpAddress::Ipv4(peer),
            port: 80,
        };
        let socket = stack
            .open_tcp_connect(local_endpoint, remote_endpoint, 7)
            .expect("test TCP connect should allocate a socket");
        let (icmp, icmp_len) = ipv4_icmp_port_unreachable_for_tcp(
            router,
            local,
            local,
            peer,
            TcpHeader {
                source_port: 49152,
                destination_port: 80,
                sequence: 7,
                acknowledgement: 0,
                flags: TcpFlags::SYN,
                window_size: u16::MAX,
            },
        );

        stack
            .receive_frame(&icmp[..icmp_len], StackInstant::from_nanos(1))
            .expect("ICMPv4 unreachable should be handled");

        assert_eq!(
            stack.tcp_connect_state(socket),
            Ok(TcpConnectState::Closed(
                TcpConnectTerminalError::RemotePortUnreachable
            ))
        );
    }

    #[test]
    fn ipv4_icmp_host_unreachable_closes_pending_tcp_connect() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let router = Ipv4Address::new([192, 0, 2, 1]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let local_endpoint = TcpEndpoint {
            address: IpAddress::Ipv4(local),
            port: 49152,
        };
        let remote_endpoint = TcpEndpoint {
            address: IpAddress::Ipv4(peer),
            port: 80,
        };
        let socket = stack
            .open_tcp_connect(local_endpoint, remote_endpoint, 7)
            .expect("test TCP connect should allocate a socket");
        let (icmp, icmp_len) = ipv4_icmp_unreachable_for_tcp(
            router,
            local,
            local,
            peer,
            TcpHeader {
                source_port: 49152,
                destination_port: 80,
                sequence: 7,
                acknowledgement: 0,
                flags: TcpFlags::SYN,
                window_size: u16::MAX,
            },
            Icmpv4DestinationUnreachableCode::Host,
        );

        stack
            .receive_frame(&icmp[..icmp_len], StackInstant::from_nanos(1))
            .expect("ICMPv4 host-unreachable should be handled");

        assert_eq!(
            stack.tcp_connect_state(socket),
            Ok(TcpConnectState::Closed(
                TcpConnectTerminalError::RemoteHostUnreachable
            ))
        );
    }

    #[test]
    fn tcp_rst_during_connect_survives_drive_as_terminal_error() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let socket = stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");
        let (rst, rst_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::RST.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );

        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&rst[..rst_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("RST should close pending connect");
        stack
            .drive_tcp(StackInstant::from_nanos(2))
            .expect("closed terminal connect should survive TCP drive");

        assert_eq!(
            stack.tcp_connect_state(socket),
            Ok(TcpConnectState::Closed(TcpConnectTerminalError::Closed))
        );
    }

    #[test]
    fn icmp_port_unreachable_does_not_close_established_tcp() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let router = Ipv4Address::new([192, 0, 2, 1]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let local_endpoint = TcpEndpoint {
            address: IpAddress::Ipv4(local),
            port: 49152,
        };
        let remote_endpoint = TcpEndpoint {
            address: IpAddress::Ipv4(peer),
            port: 80,
        };
        let socket = stack
            .open_tcp_connect(local_endpoint, remote_endpoint, 7)
            .expect("test TCP connect should allocate a socket");
        let (syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN-ACK should establish the socket");
        let (icmp, icmp_len) = ipv4_icmp_port_unreachable_for_tcp(
            router,
            local,
            local,
            peer,
            TcpHeader {
                source_port: 49152,
                destination_port: 80,
                sequence: 7,
                acknowledgement: 0,
                flags: TcpFlags::SYN,
                window_size: u16::MAX,
            },
        );

        stack
            .receive_frame(&icmp[..icmp_len], StackInstant::from_nanos(2))
            .expect("late ICMPv4 unreachable should be handled");

        assert_eq!(
            stack.tcp_connect_state(socket),
            Ok(TcpConnectState::Connected)
        );
    }

    #[test]
    fn udp_open_rejects_overlapping_address_sets() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));

        stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("wildcard UDP socket should bind");

        assert_eq!(
            stack.open_udp(UdpSocketBinding::exact(UdpEndpoint {
                address: IpAddress::Ipv4(local),
                port: 4040,
            })),
            Err(StackError::AddressInUse)
        );
        assert_eq!(
            stack.open_udp(UdpSocketBinding::connected(
                UdpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 4040,
                },
                UdpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 53,
                },
            )),
            Err(StackError::AddressInUse)
        );
    }

    #[test]
    fn udp_connected_sockets_demux_by_four_tuple() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let first_peer = Ipv4Address::new([192, 0, 2, 20]);
        let second_peer = Ipv4Address::new([192, 0, 2, 30]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let first = stack
            .open_udp(UdpSocketBinding::connected(
                UdpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 4040,
                },
                UdpEndpoint {
                    address: IpAddress::Ipv4(first_peer),
                    port: 53,
                },
            ))
            .expect("first connected UDP socket should bind");
        let second = stack
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
            .expect("second connected UDP socket should bind");

        let (second_datagram, second_len) = udp_datagram(second_peer, 53, local, 4040, b"second");
        let (first_datagram, first_len) = udp_datagram(first_peer, 53, local, 4040, b"first");

        assert!(
            stack
                .receive_udp(
                    IpAddress::Ipv4(second_peer),
                    IpAddress::Ipv4(local),
                    Bytes::copy_from_slice(&second_datagram[..second_len]),
                    RxFrameOffload::none(),
                )
                .expect("second UDP datagram should be accepted")
        );
        assert!(
            stack
                .receive_udp(
                    IpAddress::Ipv4(first_peer),
                    IpAddress::Ipv4(local),
                    Bytes::copy_from_slice(&first_datagram[..first_len]),
                    RxFrameOffload::none(),
                )
                .expect("first UDP datagram should be accepted")
        );

        let first_datagram = stack
            .take_udp(first)
            .expect("first UDP socket should exist")
            .expect("first UDP datagram should be queued");
        assert_eq!(first_datagram.bytes.as_ref(), b"first");
        let second_datagram = stack
            .take_udp(second)
            .expect("second UDP socket should exist")
            .expect("second UDP datagram should be queued");
        assert_eq!(second_datagram.bytes.as_ref(), b"second");
    }

    #[test]
    fn udp_exact_bindings_on_same_port_are_address_family_scoped() {
        let local_v4 = Ipv4Address::new([192, 0, 2, 10]);
        let peer_v4 = Ipv4Address::new([192, 0, 2, 20]);
        let local_v6 =
            Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let peer_v6 =
            Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let v4 = stack
            .open_udp(UdpSocketBinding::exact(UdpEndpoint {
                address: IpAddress::Ipv4(local_v4),
                port: 4040,
            }))
            .expect("IPv4 exact UDP socket should bind");
        let v6 = stack
            .open_udp(UdpSocketBinding::exact(UdpEndpoint {
                address: IpAddress::Ipv6(local_v6),
                port: 4040,
            }))
            .expect("IPv6 exact UDP socket should bind on the same port");

        let (v6_datagram, v6_len) = udp_datagram_v6(peer_v6, 53, local_v6, 4040, b"v6");
        let (v4_datagram, v4_len) = udp_datagram(peer_v4, 53, local_v4, 4040, b"v4");

        assert!(
            stack
                .receive_udp(
                    IpAddress::Ipv6(peer_v6),
                    IpAddress::Ipv6(local_v6),
                    Bytes::copy_from_slice(&v6_datagram[..v6_len]),
                    RxFrameOffload::none(),
                )
                .expect("IPv6 UDP datagram should be accepted")
        );
        assert!(
            stack
                .receive_udp(
                    IpAddress::Ipv4(peer_v4),
                    IpAddress::Ipv4(local_v4),
                    Bytes::copy_from_slice(&v4_datagram[..v4_len]),
                    RxFrameOffload::none(),
                )
                .expect("IPv4 UDP datagram should be accepted")
        );

        let v4_datagram = stack
            .take_udp(v4)
            .expect("IPv4 UDP socket should exist")
            .expect("IPv4 UDP datagram should be queued");
        assert_eq!(v4_datagram.bytes.as_ref(), b"v4");
        assert_eq!(v4_datagram.destination, IpAddress::Ipv4(local_v4));
        let v6_datagram = stack
            .take_udp(v6)
            .expect("IPv6 UDP socket should exist")
            .expect("IPv6 UDP datagram should be queued");
        assert_eq!(v6_datagram.bytes.as_ref(), b"v6");
        assert_eq!(v6_datagram.destination, IpAddress::Ipv6(local_v6));
    }

    #[test]
    fn udp_exact_ipv4_binding_does_not_accept_ipv6_datagram() {
        let local_v4 = Ipv4Address::new([192, 0, 2, 10]);
        let local_v6 =
            Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let peer_v6 =
            Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let socket = stack
            .open_udp(UdpSocketBinding::exact(UdpEndpoint {
                address: IpAddress::Ipv4(local_v4),
                port: 4040,
            }))
            .expect("IPv4 exact UDP socket should bind");
        let (datagram, datagram_len) = udp_datagram_v6(peer_v6, 53, local_v6, 4040, b"v6");

        assert!(
            !stack
                .receive_udp(
                    IpAddress::Ipv6(peer_v6),
                    IpAddress::Ipv6(local_v6),
                    Bytes::copy_from_slice(&datagram[..datagram_len]),
                    RxFrameOffload::none(),
                )
                .expect("IPv6 UDP datagram should parse")
        );
        assert_eq!(stack.take_udp(socket), Ok(None));
    }

    #[test]
    fn ipv4_udp_foreign_unicast_destination_is_not_delivered() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let foreign = Ipv4Address::new([192, 0, 2, 11]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("wildcard UDP socket should bind");
        let (frame, frame_len) = ipv4_udp_frame(peer, 53, foreign, 4040, b"foreign");

        stack
            .receive_frame(&frame[..frame_len], StackInstant::from_nanos(1))
            .expect("foreign UDP frame should parse");

        assert_eq!(stack.take_udp(socket), Ok(None));
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn ipv4_udp_limited_broadcast_destination_is_delivered() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("wildcard UDP socket should bind");
        let (frame, frame_len) =
            ipv4_udp_frame(peer, 53, Ipv4Address::BROADCAST, 4040, b"broadcast");

        stack
            .receive_frame(&frame[..frame_len], StackInstant::from_nanos(1))
            .expect("broadcast UDP frame should parse");

        let received = stack
            .take_udp(socket)
            .expect("wildcard UDP socket should exist")
            .expect("broadcast UDP frame should be delivered");
        assert_eq!(
            received.destination,
            IpAddress::Ipv4(Ipv4Address::BROADCAST)
        );
        assert_eq!(received.bytes.as_ref(), b"broadcast");
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn ipv4_udp_directed_broadcast_destination_is_delivered() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("wildcard UDP socket should bind");
        let directed_broadcast = Ipv4Address::new([192, 0, 2, 255]);
        let (frame, frame_len) = ipv4_udp_frame(peer, 53, directed_broadcast, 4040, b"directed");

        stack
            .receive_frame(&frame[..frame_len], StackInstant::from_nanos(1))
            .expect("directed broadcast UDP frame should parse");

        let received = stack
            .take_udp(socket)
            .expect("wildcard UDP socket should exist")
            .expect("directed broadcast UDP frame should be delivered");
        assert_eq!(received.destination, IpAddress::Ipv4(directed_broadcast));
        assert_eq!(received.bytes.as_ref(), b"directed");
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn ipv4_udp_joined_multicast_destination_is_delivered() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack
            .join_ipv4_multicast(group, local)
            .expect("IPv4 multicast group should join");
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("wildcard UDP socket should bind");
        let (frame, frame_len) = ipv4_udp_frame(peer, 5353, group, 4040, b"multicast");

        stack
            .receive_frame(&frame[..frame_len], StackInstant::from_nanos(1))
            .expect("joined multicast UDP frame should parse");

        let received = stack
            .take_udp(socket)
            .expect("wildcard UDP socket should exist")
            .expect("joined multicast UDP frame should be delivered");
        assert_eq!(received.destination, IpAddress::Ipv4(group));
        assert_eq!(received.bytes.as_ref(), b"multicast");
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn ipv4_udp_unjoined_multicast_destination_is_ignored_without_icmp() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("wildcard UDP socket should bind");
        let (frame, frame_len) = ipv4_udp_frame(peer, 5353, group, 4040, b"multicast");

        stack
            .receive_frame(&frame[..frame_len], StackInstant::from_nanos(1))
            .expect("unjoined multicast UDP frame should parse");

        assert_eq!(stack.take_udp(socket), Ok(None));
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn ipv4_udp_multicast_leave_stops_delivery_after_last_reference() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack
            .join_ipv4_multicast(group, local)
            .expect("first IPv4 multicast join should succeed");
        stack
            .join_ipv4_multicast(group, local)
            .expect("second IPv4 multicast join should increment membership");
        assert_eq!(
            stack
                .ipv4_multicast_memberships()
                .next()
                .expect("multicast membership should exist")
                .joins(),
            2
        );
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("wildcard UDP socket should bind");
        let (first, first_len) = ipv4_udp_frame(peer, 5353, group, 4040, b"first");
        let (second, second_len) = ipv4_udp_frame(peer, 5353, group, 4040, b"second");
        let (third, third_len) = ipv4_udp_frame(peer, 5353, group, 4040, b"third");

        stack
            .leave_ipv4_multicast(group, local)
            .expect("first IPv4 multicast leave should decrement membership");
        stack
            .receive_frame(&first[..first_len], StackInstant::from_nanos(1))
            .expect("multicast frame should parse while one reference remains");
        assert_eq!(
            stack
                .take_udp(socket)
                .expect("wildcard UDP socket should exist")
                .expect("multicast frame should be delivered")
                .bytes
                .as_ref(),
            b"first"
        );

        stack
            .leave_ipv4_multicast(group, local)
            .expect("second IPv4 multicast leave should remove membership");
        assert!(stack.ipv4_multicast_memberships().next().is_none());
        stack
            .receive_frame(&second[..second_len], StackInstant::from_nanos(2))
            .expect("unjoined multicast frame should parse after final leave");
        assert_eq!(stack.take_udp(socket), Ok(None));
        assert!(stack.take_outbound().is_none());

        assert_eq!(
            stack.leave_ipv4_multicast(group, local),
            Err(StackError::MulticastMembershipNotFound)
        );
        stack
            .receive_frame(&third[..third_len], StackInstant::from_nanos(3))
            .expect("unjoined multicast frame should still parse after failed leave");
        assert_eq!(stack.take_udp(socket), Ok(None));
    }

    #[test]
    fn ipv4_udp_multicast_join_validates_group_and_interface() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let other = Ipv4Address::new([192, 0, 2, 11]);
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let unicast = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));

        assert_eq!(
            stack.join_ipv4_multicast(unicast, local),
            Err(StackError::InvalidMulticastGroup)
        );
        assert_eq!(
            stack.join_ipv4_multicast(group, other),
            Err(StackError::MulticastInterfaceUnavailable)
        );
        stack
            .join_ipv4_multicast(group, local)
            .expect("owned interface should join IPv4 multicast group");
        stack.remove_ipv4_address(Ipv4Cidr::new(local, 24));
        assert!(stack.ipv4_multicast_memberships().next().is_none());
    }

    #[test]
    fn ipv4_udp_to_closed_joined_multicast_port_does_not_queue_icmp() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        stack
            .join_ipv4_multicast(group, local)
            .expect("IPv4 multicast group should join");
        let (frame, frame_len) = ipv4_udp_frame(peer, 5353, group, 4040, b"multicast");

        stack
            .receive_frame(&frame[..frame_len], StackInstant::from_nanos(1))
            .expect("closed multicast UDP frame should be handled");

        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn udp_send_ipv4_multicast_uses_multicast_mac_without_neighbor() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("wildcard UDP socket should bind");

        assert_eq!(
            stack.send_udp(
                socket,
                IpAddress::Ipv4(group),
                5353,
                b"multicast",
                1,
                StackInstant::from_nanos(1),
            ),
            Ok(b"multicast".len())
        );

        let frame = stack
            .take_outbound()
            .expect("IPv4 multicast UDP frame should be queued without ARP");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        assert_eq!(ethernet.destination, [0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb]);
        assert_eq!(ethernet.source, LOCAL_MAC);
        assert_eq!(ethernet.protocol, EthernetProtocol::Ipv4);
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        assert_eq!(ipv4.source, local);
        assert_eq!(ipv4.destination, group);
        let udp = UdpPacket::parse(ipv4.payload).expect("UDP packet should parse");
        assert_eq!(udp.source_port, 4040);
        assert_eq!(udp.destination_port, 5353);
        assert_eq!(udp.payload, b"multicast");
        assert!(stack.take_outbound().is_none());
    }

    /// A socket's TTL must reach the wire, not just the socket record.
    ///
    /// The datagram path stamped a hardcoded 64 before, so `IP_TTL` and
    /// `set-unicast-hop-limit` were accepted and then discarded.
    #[test]
    fn udp_send_stamps_the_socket_hop_limit_on_the_ipv4_header() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("wildcard UDP socket should bind");
        assert_eq!(stack.udp_hop_limit(socket), Ok(crate::DEFAULT_HOP_LIMIT));

        stack
            .set_udp_hop_limit(socket, 7)
            .expect("bound UDP socket should accept a hop limit");
        assert_eq!(stack.udp_hop_limit(socket), Ok(7));
        assert_eq!(
            stack.send_udp(
                socket,
                IpAddress::Ipv4(group),
                5353,
                b"ttl",
                1,
                StackInstant::from_nanos(1),
            ),
            Ok(b"ttl".len())
        );

        let frame = stack.take_outbound().expect("UDP frame should be queued");
        let ipv4 = Ipv4Packet::parse(&frame.as_slice()[EthernetFrame::HEADER_LEN..])
            .expect("IPv4 packet should parse");
        assert_eq!(ipv4.hop_limit, 7);
    }

    /// A listener's hop limit has to survive into the connections it accepts,
    /// so the SYN-ACK and everything after carries the guest's TTL.
    #[test]
    fn accepted_connections_inherit_the_listener_hop_limit() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let listener = stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 8080,
                },
                TcpListenBacklog::new(1),
            )
            .expect("listener should open");
        stack
            .set_tcp_hop_limit(listener, 9)
            .expect("listener should accept a hop limit");

        let (syn, syn_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 4040,
                destination_port: 8080,
                sequence: 1000,
                acknowledgement: 0,
                flags: TcpFlags::SYN,
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn[..syn_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN should be accepted by the listener");
        stack
            .drive_tcp(StackInstant::from_nanos(2))
            .expect("stack should emit the SYN-ACK");

        let frame = stack.take_outbound().expect("SYN-ACK should be queued");
        let ipv4 = Ipv4Packet::parse(&frame.as_slice()[EthernetFrame::HEADER_LEN..])
            .expect("IPv4 packet should parse");
        assert_eq!(ipv4.hop_limit, 9);
    }

    #[test]
    fn udp_send_with_tx_offload_seeds_pseudo_sum_and_metadata() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let mut stack = Stack::new(
            StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES).with_tx_checksum_offload(true),
        );
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("wildcard UDP socket should bind");

        assert_eq!(
            stack.send_udp(
                socket,
                IpAddress::Ipv4(group),
                5353,
                b"offloaded",
                1,
                StackInstant::from_nanos(1),
            ),
            Ok(b"offloaded".len())
        );

        let frame = stack
            .take_outbound()
            .expect("offloaded UDP frame should be queued");
        let transport_offset = EthernetFrame::HEADER_LEN + Ipv4Packet::MIN_HEADER_LEN;
        let metadata = frame
            .tx_checksum()
            .expect("offloaded frame must carry checksum metadata");
        assert_eq!(metadata.start as usize, transport_offset);
        assert_eq!(metadata.offset, 6);

        // The checksum field holds the folded pseudo-header sum: when the
        // device continues the one's-complement sum over the transport
        // bytes (with the seeded field in place) and complements it, the
        // result must equal the software checksum of the same datagram.
        let bytes = frame.as_slice();
        let datagram = &bytes[transport_offset..];
        let seeded = u16::from_be_bytes([datagram[6], datagram[7]]);
        let device_sum = {
            let mut sum = u32::from(seeded);
            let mut chunks = datagram.chunks_exact(2);
            for chunk in chunks.by_ref() {
                sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
            }
            if let [last] = chunks.remainder() {
                sum += u32::from(*last) << 8;
            }
            // The seeded field itself was included once in the loop; the
            // device sums payload bytes treating the field as part of the
            // data, exactly as one's-complement arithmetic requires.
            sum -= u32::from(seeded);
            while (sum >> 16) != 0 {
                sum = (sum & 0xffff) + (sum >> 16);
            }
            !(sum as u16)
        };
        let mut verified = [0u8; crate::ETHERNET_FRAME_BYTES];
        verified[..datagram.len()].copy_from_slice(datagram);
        verified[6..8].copy_from_slice(&device_sum.to_be_bytes());
        assert!(
            crate::udp_checksum_valid(
                IpAddress::Ipv4(local),
                IpAddress::Ipv4(group),
                &verified[..datagram.len()],
            ),
            "device-completed checksum must validate in software"
        );
        assert!(stack.take_outbound().is_none());
    }

    /// Brings a TCP socket to Established over `config` and returns the
    /// stack plus the socket, with the handshake frames already drained.
    fn established_tcp_stack(config: StackConfig) -> (Stack, SocketId) {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(config);
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let socket = stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");
        stack
            .drive_tcp(StackInstant::from_nanos(1))
            .expect("SYN should be queued");
        let _ = stack.take_outbound().expect("SYN frame should be queued");
        let (syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN-ACK should establish the socket");
        stack
            .drive_tcp(StackInstant::from_nanos(1))
            .expect("handshake ACK should be queued");
        let _ = stack
            .take_outbound()
            .expect("handshake ACK frame should be queued");
        (stack, socket)
    }

    /// Segmentation capability of a device that splits IPv4 TCP up to a
    /// full IP packet, with the ECN modifier available.
    fn tso_ipv4_offload() -> SegmentationOffload {
        SegmentationOffload {
            tx_tcp_ipv4: true,
            tx_tcp_ipv6: false,
            rx_tcp_ipv4: false,
            rx_tcp_ipv6: false,
            max_tx_frame_bytes: u16::MAX as usize + EthernetFrame::HEADER_LEN,
            max_rx_frame_bytes: crate::ETHERNET_FRAME_BYTES,
            tx_tcp_ecn: true,
        }
    }

    /// Bulk TCP data leaves as a scatter frame whenever the interface
    /// says its DMA can read borrowed bytes: the payload the socket
    /// already holds is attached to the frame instead of copied into
    /// it, and the packet buffer carries only the headers.
    #[test]
    fn bulk_tcp_data_is_scattered_when_the_device_reads_borrowed_bytes() {
        let (mut stack, socket) = established_tcp_stack(
            StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES)
                .with_tx_checksum_offload(true)
                .with_direct_tx_dma(true),
        );
        let payload = vec![0x5a_u8; TCP_SCATTER_MIN_PAYLOAD_BYTES];
        stack
            .tcp_send(socket, &payload)
            .expect("bulk TCP send should queue");
        stack
            .drive_tcp(StackInstant::from_nanos(2))
            .expect("TCP data should be queued");

        let frame = stack.take_outbound().expect("data frame should be queued");
        let attached = frame
            .payload()
            .expect("a scatter frame carries its payload by reference");
        assert_eq!(attached.as_ref(), payload.as_slice());
        assert_eq!(
            frame.as_slice().len(),
            EthernetFrame::HEADER_LEN + Ipv4Packet::MIN_HEADER_LEN + TcpPacket::MIN_HEADER_LEN,
            "the packet buffer holds framing only"
        );
        assert!(frame.tx_segmentation().is_none());
    }

    /// The same send over an interface whose DMA cannot reach borrowed
    /// bytes copies the payload into the packet buffer, because that is
    /// the only memory the device will read.
    #[test]
    fn bulk_tcp_data_is_copied_when_the_device_cannot_read_borrowed_bytes() {
        let (mut stack, socket) = established_tcp_stack(
            StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES).with_tx_checksum_offload(true),
        );
        let payload = vec![0x5a_u8; TCP_SCATTER_MIN_PAYLOAD_BYTES];
        stack
            .tcp_send(socket, &payload)
            .expect("bulk TCP send should queue");
        stack
            .drive_tcp(StackInstant::from_nanos(2))
            .expect("TCP data should be queued");

        let frame = stack.take_outbound().expect("data frame should be queued");
        assert!(frame.payload().is_none());
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let tcp = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert_eq!(tcp.payload, payload.as_slice());
    }

    /// A TSO-capable interface gets one oversized frame instead of a
    /// train of MSS-sized ones: the whole congestion window in a single
    /// scatter payload, plus the metadata the device needs to split it
    /// back up. The payload bytes are the same bytes the socket queued,
    /// in the same order.
    #[test]
    fn a_tso_interface_receives_one_coalesced_frame_with_gso_metadata() {
        let (mut stack, socket) = established_tcp_stack(
            StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES)
                .with_tx_checksum_offload(true)
                .with_segmentation_offload(tso_ipv4_offload()),
        );
        let payload: Vec<u8> = (0..40 * 1024).map(|index| index as u8).collect();
        let queued = stack
            .tcp_send(socket, &payload)
            .expect("bulk TCP send should queue");
        assert_eq!(queued, payload.len());
        stack
            .drive_tcp(StackInstant::from_nanos(2))
            .expect("coalesced TCP data should be queued");

        let frame = stack
            .take_outbound()
            .expect("segmented data frame should be queued");
        let scattered = frame
            .payload()
            .expect("a coalesced frame carries its payload by reference");
        let segmentation = frame
            .tx_segmentation()
            .expect("a coalesced frame carries segmentation metadata");
        assert!(
            scattered.len() > usize::from(segmentation.segment_bytes),
            "a coalesced frame must outgrow one wire segment"
        );
        assert_eq!(scattered.as_ref(), &payload[..scattered.len()]);
        assert!(!segmentation.ipv6);
        assert!(!segmentation.ecn);
        // The header prefix the device replicates is exactly what the
        // encoder wrote into the packet buffer.
        assert_eq!(usize::from(segmentation.header_len), frame.as_slice().len());
        // The oversized frame on the wire is the header prefix followed
        // by the scatter payload; its IP length field describes the
        // whole burst, which is what the device fixes up per segment.
        let mut wire = Vec::from(frame.as_slice());
        wire.extend_from_slice(scattered.as_ref());
        let ethernet = EthernetFrame::parse(&wire).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let tcp = TcpPacket::parse(ipv4.payload).expect("TCP header should parse");
        assert_eq!(tcp.payload.len(), scattered.len());
        assert_eq!(
            usize::from(segmentation.segment_bytes),
            crate::ETHERNET_FRAME_BYTES
                - EthernetFrame::HEADER_LEN
                - Ipv4Packet::MIN_HEADER_LEN
                - TcpPacket::MIN_HEADER_LEN
                - tcp.options.raw().len(),
            "the device segments at the path MSS"
        );
    }

    /// The same send over an interface that segments nothing produces
    /// the MSS-sized frames the socket has always produced, with no
    /// segmentation metadata for a device that would not understand it.
    #[test]
    fn an_interface_without_segmentation_still_receives_mss_sized_frames() {
        let (mut stack, socket) = established_tcp_stack(
            StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES).with_tx_checksum_offload(true),
        );
        let payload: Vec<u8> = (0..40 * 1024).map(|index| index as u8).collect();
        stack
            .tcp_send(socket, &payload)
            .expect("bulk TCP send should queue");
        stack
            .drive_tcp(StackInstant::from_nanos(2))
            .expect("TCP data should be queued");

        let frame = stack.take_outbound().expect("data frame should be queued");
        assert!(frame.tx_segmentation().is_none());
        assert!(frame.payload().is_none());
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let tcp = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert_eq!(
            tcp.payload.len(),
            crate::ETHERNET_FRAME_BYTES
                - EthernetFrame::HEADER_LEN
                - Ipv4Packet::MIN_HEADER_LEN
                - TcpPacket::MIN_HEADER_LEN
                - tcp.options.raw().len()
        );
        assert_eq!(tcp.payload, &payload[..tcp.payload.len()]);
    }

    /// Coalescing changes framing, not accounting: the socket still
    /// records one in-flight segment per MSS-sized wire segment, so a
    /// retransmission after the burst addresses one segment, not the
    /// whole frame.
    #[test]
    fn a_coalesced_burst_still_retransmits_one_segment_at_a_time() {
        let (mut stack, socket) = established_tcp_stack(
            StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES)
                .with_tx_checksum_offload(true)
                .with_segmentation_offload(tso_ipv4_offload()),
        );
        let payload: Vec<u8> = (0..40 * 1024).map(|index| index as u8).collect();
        stack
            .tcp_send(socket, &payload)
            .expect("bulk TCP send should queue");
        stack
            .drive_tcp(StackInstant::from_nanos(2))
            .expect("coalesced TCP data should be queued");
        let burst = stack
            .take_outbound()
            .expect("segmented data frame should be queued");
        let burst_bytes = burst
            .payload()
            .expect("a coalesced frame carries its payload")
            .len();
        let segment_bytes = usize::from(
            burst
                .tx_segmentation()
                .expect("a coalesced frame carries segmentation metadata")
                .segment_bytes,
        );
        assert!(burst_bytes > segment_bytes);

        let socket_state = stack.tcp_socket(socket).expect("socket should still exist");
        let retransmission = socket_state
            .pending_retransmission(crate::tcp::TCP_INITIAL_RTO_NANOS + 2)
            .expect("the first in-flight segment should come due");
        assert_eq!(retransmission.payload.len(), segment_bytes);
        assert_eq!(retransmission.payload.as_ref(), &payload[..segment_bytes]);
    }

    #[test]
    fn scatter_tcp_send_attaches_payload_and_seeds_pseudo_sum() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer_addr = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(
            StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES)
                .with_tx_checksum_offload(true)
                .with_direct_tx_dma(true),
        );
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer_addr),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });

        let header = TcpHeader {
            source_port: 40000,
            destination_port: 80,
            sequence: 1,
            acknowledgement: 1,
            flags: TcpFlags::ACK,
            window_size: u16::MAX,
        };
        let payload = Bytes::from(vec![0x5a_u8; TCP_SCATTER_MIN_PAYLOAD_BYTES]);
        assert!(
            stack
                .queue_tcp_ipv4(
                    local,
                    peer_addr,
                    TcpEgress {
                        header,
                        options: TcpHeaderOptions::empty(),
                        payload: TcpTxPayload::Scatter(&payload),
                        hop_limit: crate::DEFAULT_HOP_LIMIT,
                    },
                    1,
                    StackInstant::from_nanos(1),
                )
                .expect("scatter TCP segment should queue")
        );

        let frame = stack
            .take_outbound()
            .expect("scatter frame should be queued");
        let attached = frame.payload().expect("scatter frame keeps its payload");
        assert_eq!(attached.len(), payload.len());
        assert_eq!(attached.as_ref(), payload.as_ref());
        // Header buffer holds only the framing, not the payload bytes.
        assert!(frame.as_slice().len() < payload.len());
        let transport_offset = EthernetFrame::HEADER_LEN + Ipv4Packet::MIN_HEADER_LEN;
        let metadata = frame
            .tx_checksum()
            .expect("scatter frame carries offload metadata");
        assert_eq!(metadata.start as usize, transport_offset);
        assert_eq!(metadata.offset, 16);

        // The seeded checksum equals the software pseudo-header sum over
        // the full segment length (headers + scattered payload).
        let segment_len = frame.as_slice().len() - transport_offset + payload.len();
        let seeded = u16::from_be_bytes([
            frame.as_slice()[transport_offset + 16],
            frame.as_slice()[transport_offset + 17],
        ]);
        assert_eq!(
            seeded,
            crate::tcp_pseudo_header_checksum(
                IpAddress::Ipv4(local),
                IpAddress::Ipv4(peer_addr),
                segment_len,
            )
        );
    }

    #[test]
    fn udp_send_without_tx_offload_carries_no_metadata() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let group = Ipv4Address::new([224, 0, 0, 251]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("wildcard UDP socket should bind");
        stack
            .send_udp(
                socket,
                IpAddress::Ipv4(group),
                5353,
                b"software",
                1,
                StackInstant::from_nanos(1),
            )
            .expect("software-checksum send should queue");

        let frame = stack.take_outbound().expect("frame should be queued");
        assert_eq!(frame.tx_checksum(), None);
    }

    #[test]
    fn ipv6_udp_foreign_unicast_destination_is_not_delivered() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let foreign =
            Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
        let peer = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv6_address(Ipv6Cidr::new(local, 64));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("wildcard UDP socket should bind");
        let (frame, frame_len) = ipv6_udp_frame(peer, 53, foreign, 4040, b"foreign");

        stack
            .receive_frame(&frame[..frame_len], StackInstant::from_nanos(1))
            .expect("foreign IPv6 UDP frame should parse");

        assert_eq!(stack.take_udp(socket), Ok(None));
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn ipv4_icmp_port_unreachable_records_connected_udp_error() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let router = Ipv4Address::new([192, 0, 2, 1]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::connected(
                UdpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                UdpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 53,
                },
            ))
            .expect("connected UDP socket should bind");
        let (quoted, quoted_len) = ipv4_udp_frame(local, 49152, peer, 53, b"payload");
        let (icmp, icmp_len) =
            ipv4_icmp_port_unreachable_for_udp_frame(router, local, &quoted[..quoted_len]);

        stack
            .receive_frame(&icmp[..icmp_len], StackInstant::from_nanos(1))
            .expect("ICMPv4 unreachable should be handled");

        assert_eq!(
            stack.take_udp_error(socket),
            Ok(Some(UdpSocketError::RemotePortUnreachable))
        );
        assert_eq!(stack.take_udp_error(socket), Ok(None));
    }

    #[test]
    fn ipv6_icmp_port_unreachable_records_connected_udp_error() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let peer = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let router = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv6_address(Ipv6Cidr::new(local, 64));
        let socket = stack
            .open_udp(UdpSocketBinding::connected(
                UdpEndpoint {
                    address: IpAddress::Ipv6(local),
                    port: 49152,
                },
                UdpEndpoint {
                    address: IpAddress::Ipv6(peer),
                    port: 53,
                },
            ))
            .expect("connected UDP socket should bind");
        let (quoted, quoted_len) = ipv6_udp_frame(local, 49152, peer, 53, b"payload");
        let (icmp, icmp_len) =
            ipv6_icmp_port_unreachable_for_udp_frame(router, local, &quoted[..quoted_len]);

        stack
            .receive_frame(&icmp[..icmp_len], StackInstant::from_nanos(1))
            .expect("ICMPv6 unreachable should be handled");

        assert_eq!(
            stack.take_udp_error(socket),
            Ok(Some(UdpSocketError::RemotePortUnreachable))
        );
        assert_eq!(stack.take_udp_error(socket), Ok(None));
    }

    #[test]
    fn ipv4_icmp_network_unreachable_records_connected_udp_error() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let router = Ipv4Address::new([192, 0, 2, 1]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::connected(
                UdpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                UdpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 53,
                },
            ))
            .expect("connected UDP socket should bind");
        let (quoted, quoted_len) = ipv4_udp_frame(local, 49152, peer, 53, b"payload");
        let (icmp, icmp_len) = ipv4_icmp_unreachable_for_udp_frame(
            router,
            local,
            &quoted[..quoted_len],
            Icmpv4DestinationUnreachableCode::Net,
        );

        stack
            .receive_frame(&icmp[..icmp_len], StackInstant::from_nanos(1))
            .expect("ICMPv4 network-unreachable should be handled");

        assert_eq!(
            stack.take_udp_error(socket),
            Ok(Some(UdpSocketError::RemoteNetworkUnreachable))
        );
    }

    #[test]
    fn ipv6_icmp_admin_prohibited_records_connected_udp_error() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let peer = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let router = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv6_address(Ipv6Cidr::new(local, 64));
        let socket = stack
            .open_udp(UdpSocketBinding::connected(
                UdpEndpoint {
                    address: IpAddress::Ipv6(local),
                    port: 49152,
                },
                UdpEndpoint {
                    address: IpAddress::Ipv6(peer),
                    port: 53,
                },
            ))
            .expect("connected UDP socket should bind");
        let (quoted, quoted_len) = ipv6_udp_frame(local, 49152, peer, 53, b"payload");
        let (icmp, icmp_len) = ipv6_icmp_unreachable_for_udp_frame(
            router,
            local,
            &quoted[..quoted_len],
            Icmpv6DestinationUnreachableCode::AdminProhibited,
        );

        stack
            .receive_frame(&icmp[..icmp_len], StackInstant::from_nanos(1))
            .expect("ICMPv6 admin-prohibited should be handled");

        assert_eq!(
            stack.take_udp_error(socket),
            Ok(Some(UdpSocketError::RemoteCommunicationProhibited))
        );
    }

    #[test]
    fn icmp_unreachable_ignores_unmatched_udp_quote() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let other_peer = Ipv4Address::new([192, 0, 2, 30]);
        let router = Ipv4Address::new([192, 0, 2, 1]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::connected(
                UdpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                UdpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 53,
                },
            ))
            .expect("connected UDP socket should bind");
        let (quoted, quoted_len) = ipv4_udp_frame(local, 49152, other_peer, 53, b"payload");
        let (icmp, icmp_len) =
            ipv4_icmp_port_unreachable_for_udp_frame(router, local, &quoted[..quoted_len]);

        stack
            .receive_frame(&icmp[..icmp_len], StackInstant::from_nanos(1))
            .expect("unmatched ICMPv4 unreachable should be handled");

        assert_eq!(stack.take_udp_error(socket), Ok(None));
    }

    #[test]
    fn udp_receive_rejects_bad_checksum() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (mut datagram, datagram_len) = udp_datagram(peer, 53, local, 4040, b"payload");
        datagram[6] ^= 0x80;

        assert_eq!(
            stack.receive_udp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                Bytes::copy_from_slice(&datagram[..datagram_len]),
                RxFrameOffload::none(),
            ),
            Err(StackError::MalformedPacket)
        );
        assert_eq!(stack.take_udp(socket), Ok(None));
    }

    #[test]
    fn udp_receive_trusts_a_device_validated_datagram() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let config = StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES)
            .with_rx_checksum_offload(RxChecksumOffload::new(false, false, true));
        let mut stack = Stack::new(config);
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (mut datagram, datagram_len) = udp_datagram(peer, 53, local, 4040, b"payload");
        datagram[6] ^= 0x80;

        assert!(
            stack
                .receive_udp(
                    IpAddress::Ipv4(peer),
                    IpAddress::Ipv4(local),
                    Bytes::copy_from_slice(&datagram[..datagram_len]),
                    RxFrameOffload::validated(),
                )
                .expect("UDP checksum-offloaded datagram should be accepted")
        );
        let received = stack
            .take_udp(socket)
            .expect("UDP socket should exist")
            .expect("UDP datagram should be queued");
        assert_eq!(received.bytes.as_ref(), b"payload");
    }

    #[test]
    fn udp_receive_completes_a_partial_checksum_datagram() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let config = StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES)
            .with_rx_checksum_offload(RxChecksumOffload::new(false, false, true));
        let mut stack = Stack::new(config);
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (mut datagram, datagram_len) = udp_datagram(peer, 53, local, 4040, b"payload");
        let partial = crate::udp_pseudo_header_checksum(
            IpAddress::Ipv4(peer),
            IpAddress::Ipv4(local),
            datagram_len,
        );
        datagram[6..8].copy_from_slice(&partial.to_be_bytes());

        assert!(
            stack
                .receive_udp(
                    IpAddress::Ipv4(peer),
                    IpAddress::Ipv4(local),
                    Bytes::copy_from_slice(&datagram[..datagram_len]),
                    RxFrameOffload::partial(0, 6),
                )
                .expect("a completed partial checksum should be accepted")
        );
        let received = stack
            .take_udp(socket)
            .expect("UDP socket should exist")
            .expect("UDP datagram should be queued");
        assert_eq!(received.bytes.as_ref(), b"payload");
    }

    #[test]
    fn ipv4_udp_receive_accepts_zero_checksum() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (mut datagram, datagram_len) = udp_datagram(peer, 53, local, 4040, b"payload");
        datagram[6] = 0;
        datagram[7] = 0;

        assert!(
            stack
                .receive_udp(
                    IpAddress::Ipv4(peer),
                    IpAddress::Ipv4(local),
                    Bytes::copy_from_slice(&datagram[..datagram_len]),
                    RxFrameOffload::none(),
                )
                .expect("IPv4 UDP zero checksum should be accepted")
        );
        let received = stack
            .take_udp(socket)
            .expect("UDP socket should exist")
            .expect("UDP datagram should be queued");
        assert_eq!(received.bytes.as_ref(), b"payload");
    }

    #[test]
    fn ipv4_receive_trusts_configured_rx_header_checksum_offload() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let config = StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES)
            .with_rx_checksum_offload(RxChecksumOffload::new(true, false, false));
        let mut stack = Stack::new(config);
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (mut frame, frame_len) = ipv4_udp_frame(peer, 53, local, 4040, b"payload");
        frame[EthernetFrame::HEADER_LEN + 10] ^= 0x80;

        stack
            .receive_frame(&frame[..frame_len], StackInstant::from_nanos(1))
            .expect("IPv4 header-checksum-offloaded frame should be accepted");

        let received = stack
            .take_udp(socket)
            .expect("UDP socket should exist")
            .expect("UDP datagram should be queued");
        assert_eq!(received.bytes.as_ref(), b"payload");
    }

    #[test]
    fn ipv4_udp_large_payload_retains_owning_frame_slice() {
        const LARGE_PAYLOAD: [u8; 1024] = [0x5a; 1024];
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (frame, frame_len) = ipv4_udp_frame(peer, 53, local, 4040, &LARGE_PAYLOAD);
        let frame = Bytes::copy_from_slice(&frame[..frame_len]);

        stack
            .receive_rx_frame(RxFrame::new(frame.clone()), StackInstant::from_nanos(1))
            .expect("large UDP frame should parse");

        let received = stack
            .take_udp(socket)
            .expect("UDP socket should exist")
            .expect("large UDP datagram should be queued");
        assert_eq!(received.bytes.as_ref(), LARGE_PAYLOAD.as_slice());
        assert_eq!(
            received.bytes.frame_slice_offset_for_test(&frame),
            Some(EthernetFrame::HEADER_LEN + Ipv4Packet::MIN_HEADER_LEN + UdpPacket::HEADER_LEN)
        );
    }

    #[test]
    fn ipv4_udp_fragments_reassemble_and_deliver() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let payload = b"fragmented-udp-payload";
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (first, second) = ipv4_udp_fragments(peer, 53, local, 4040, payload, 16, 0x1234);

        stack
            .receive_frame(&first.0[..first.1], StackInstant::from_nanos(1))
            .expect("first UDP fragment should be accepted");
        assert_eq!(stack.take_udp(socket), Ok(None));
        stack
            .receive_frame(&second.0[..second.1], StackInstant::from_nanos(2))
            .expect("second UDP fragment should complete reassembly");

        let received = stack
            .take_udp(socket)
            .expect("UDP socket should exist")
            .expect("reassembled UDP datagram should be queued");
        assert_eq!(received.source, IpAddress::Ipv4(peer));
        assert_eq!(received.destination, IpAddress::Ipv4(local));
        assert_eq!(received.bytes.as_ref(), payload);
    }

    #[test]
    fn ipv4_udp_fragments_reassemble_out_of_order() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let payload = b"fragmented-udp-payload";
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (first, second) = ipv4_udp_fragments(peer, 53, local, 4040, payload, 16, 0x1235);

        stack
            .receive_frame(&second.0[..second.1], StackInstant::from_nanos(1))
            .expect("final UDP fragment should be accepted first");
        assert_eq!(stack.take_udp(socket), Ok(None));
        stack
            .receive_frame(&first.0[..first.1], StackInstant::from_nanos(2))
            .expect("initial UDP fragment should complete reassembly");

        let received = stack
            .take_udp(socket)
            .expect("UDP socket should exist")
            .expect("out-of-order reassembled datagram should be queued");
        assert_eq!(received.bytes.as_ref(), payload);
    }

    #[test]
    fn ipv4_udp_duplicate_fragment_is_ignored_until_missing_fragment_arrives() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let payload = b"fragmented-udp-payload";
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (first, second) = ipv4_udp_fragments(peer, 53, local, 4040, payload, 16, 0x1236);

        stack
            .receive_frame(&first.0[..first.1], StackInstant::from_nanos(1))
            .expect("first UDP fragment should be accepted");
        stack
            .receive_frame(&first.0[..first.1], StackInstant::from_nanos(2))
            .expect("duplicate UDP fragment should be ignored");
        assert_eq!(stack.take_udp(socket), Ok(None));
        stack
            .receive_frame(&second.0[..second.1], StackInstant::from_nanos(3))
            .expect("missing UDP fragment should complete reassembly");

        let received = stack
            .take_udp(socket)
            .expect("UDP socket should exist")
            .expect("deduplicated UDP datagram should be queued");
        assert_eq!(received.bytes.as_ref(), payload);
    }

    #[test]
    fn ipv4_udp_overlapping_fragment_drops_reassembly_state() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let payload = b"fragmented-udp-payload";
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (first, second) = ipv4_udp_fragments(peer, 53, local, 4040, payload, 16, 0x1237);
        let (datagram, datagram_len) = udp_datagram(peer, 53, local, 4040, payload);
        let overlap = ipv4_fragment_frame(
            peer,
            local,
            crate::IpProtocol::Udp,
            0x1237,
            8,
            true,
            &datagram[8..24.min(datagram_len)],
        );

        stack
            .receive_frame(&first.0[..first.1], StackInstant::from_nanos(1))
            .expect("first UDP fragment should be accepted");
        stack
            .receive_frame(&overlap.0[..overlap.1], StackInstant::from_nanos(2))
            .expect("overlapping UDP fragment should be dropped without surfacing an error");
        stack
            .receive_frame(&second.0[..second.1], StackInstant::from_nanos(3))
            .expect("remaining UDP fragment should not complete dropped state");

        assert_eq!(stack.take_udp(socket), Ok(None));
    }

    #[test]
    fn ipv4_udp_bad_non_final_fragment_is_ignored() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let payload = b"fragmented-udp-payload";
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (datagram, _) = udp_datagram(peer, 53, local, 4040, payload);
        let bad = ipv4_fragment_frame(
            peer,
            local,
            crate::IpProtocol::Udp,
            0x1238,
            0,
            true,
            &datagram[..9],
        );

        stack
            .receive_frame(&bad.0[..bad.1], StackInstant::from_nanos(1))
            .expect("misaligned non-final UDP fragment should be consumed");

        assert_eq!(stack.take_udp(socket), Ok(None));
        let (good, good_len) = ipv4_udp_frame(peer, 53, local, 4040, b"after-bad-fragment");
        stack
            .receive_frame(&good[..good_len], StackInstant::from_nanos(2))
            .expect("later non-fragment UDP frame should still parse");
        let received = stack
            .take_udp(socket)
            .expect("UDP socket should exist")
            .expect("later UDP datagram should be queued");
        assert_eq!(received.bytes.as_ref(), b"after-bad-fragment");
    }

    #[test]
    fn ipv6_udp_receive_rejects_zero_checksum() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let peer = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (mut datagram, datagram_len) = udp_datagram_v6(peer, 53, local, 4040, b"payload");
        datagram[6] = 0;
        datagram[7] = 0;

        assert_eq!(
            stack.receive_udp(
                IpAddress::Ipv6(peer),
                IpAddress::Ipv6(local),
                Bytes::copy_from_slice(&datagram[..datagram_len]),
                RxFrameOffload::none(),
            ),
            Err(StackError::MalformedPacket)
        );
        assert_eq!(stack.take_udp(socket), Ok(None));
    }

    #[test]
    fn ipv4_udp_to_closed_local_port_queues_icmp_port_unreachable() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let (frame, frame_len) = ipv4_udp_frame(peer, 53, local, 4040, b"payload");

        stack
            .receive_frame(&frame[..frame_len], StackInstant::from_nanos(1))
            .expect("closed UDP frame should be handled");

        let response = stack
            .take_outbound()
            .expect("ICMP port-unreachable frame should be queued");
        let ethernet =
            EthernetFrame::parse(response.as_slice()).expect("Ethernet frame should parse");
        assert_eq!(ethernet.destination, PEER_MAC);
        assert_eq!(ethernet.source, LOCAL_MAC);
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        assert_eq!(ipv4.source, local);
        assert_eq!(ipv4.destination, peer);
        assert_eq!(ipv4.protocol, crate::IpProtocol::Icmp);
        let icmp = Icmpv4Packet::parse(ipv4.payload).expect("ICMPv4 packet should parse");
        let Icmpv4Packet::DestinationUnreachable(unreachable) = icmp else {
            panic!("expected ICMP destination unreachable");
        };
        assert_eq!(
            unreachable.code,
            Icmpv4Packet::DESTINATION_UNREACHABLE_PORT_CODE
        );
        assert_eq!(
            unreachable.original.len(),
            Ipv4Packet::MIN_HEADER_LEN + UdpPacket::HEADER_LEN
        );
        assert_eq!(&unreachable.original[12..16], &peer.octets());
        assert_eq!(&unreachable.original[16..20], &local.octets());
        assert_eq!(unreachable.original[9], crate::IpProtocol::Udp.number());
        let quoted_udp = &unreachable.original[Ipv4Packet::MIN_HEADER_LEN..];
        assert_eq!(u16::from_be_bytes([quoted_udp[0], quoted_udp[1]]), 53);
        assert_eq!(u16::from_be_bytes([quoted_udp[2], quoted_udp[3]]), 4040);
    }

    #[test]
    fn udp_send_rejects_payload_larger_than_mtu() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let payload = [0u8; crate::ETHERNET_FRAME_BYTES];

        assert_eq!(
            stack.send_udp(
                socket,
                IpAddress::Ipv4(peer),
                53,
                &payload,
                1,
                StackInstant::from_nanos(1),
            ),
            Err(StackError::PacketTooLarge)
        );
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn ipv6_packet_too_big_updates_udp_path_mtu() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let peer = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let router = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv6_address(Ipv6Cidr::new(local, 64));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv6(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let payload = [0u8; 1233];
        assert_eq!(
            stack.send_udp(
                socket,
                IpAddress::Ipv6(peer),
                53,
                &payload,
                1,
                StackInstant::from_nanos(1),
            ),
            Ok(payload.len())
        );
        let quoted = stack
            .take_outbound()
            .expect("initial UDP datagram should be queued");
        let (icmp, icmp_len) =
            ipv6_icmp_packet_too_big_for_frame(router, local, quoted.as_slice(), 1280);

        stack
            .receive_frame(&icmp[..icmp_len], StackInstant::from_nanos(2))
            .expect("ICMPv6 Packet Too Big should update PMTU");

        assert_eq!(
            stack.send_udp(
                socket,
                IpAddress::Ipv6(peer),
                53,
                &payload,
                2,
                StackInstant::from_nanos(3),
            ),
            Err(StackError::PacketTooLarge)
        );
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn ipv6_udp_to_closed_local_port_queues_icmp_port_unreachable() {
        let local = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let peer = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv6_address(Ipv6Cidr::new(local, 64));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv6(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let (frame, frame_len) = ipv6_udp_frame(peer, 53, local, 4040, b"payload");

        stack
            .receive_frame(&frame[..frame_len], StackInstant::from_nanos(1))
            .expect("closed UDP IPv6 frame should be handled");

        let response = stack
            .take_outbound()
            .expect("ICMPv6 port-unreachable frame should be queued");
        let ethernet =
            EthernetFrame::parse(response.as_slice()).expect("Ethernet frame should parse");
        assert_eq!(ethernet.destination, PEER_MAC);
        assert_eq!(ethernet.source, LOCAL_MAC);
        let ipv6 = Ipv6Packet::parse(ethernet.payload).expect("IPv6 packet should parse");
        assert_eq!(ipv6.source, local);
        assert_eq!(ipv6.destination, peer);
        assert_eq!(ipv6.next_header, crate::IpProtocol::Icmpv6);
        let icmp = Icmpv6Packet::parse(ipv6.payload).expect("ICMPv6 packet should parse");
        let Icmpv6Packet::DestinationUnreachable(unreachable) = icmp else {
            panic!("expected ICMPv6 destination unreachable");
        };
        assert_eq!(
            unreachable.code,
            Icmpv6Packet::DESTINATION_UNREACHABLE_PORT_CODE
        );
        assert_eq!(
            unreachable.original.len(),
            Ipv6Packet::HEADER_LEN + UdpPacket::HEADER_LEN + b"payload".len()
        );
        assert_eq!(unreachable.original[6], crate::IpProtocol::Udp.number());
        assert_eq!(&unreachable.original[8..24], &peer.octets());
        assert_eq!(&unreachable.original[24..40], &local.octets());
        let quoted_udp = &unreachable.original[Ipv6Packet::HEADER_LEN..];
        assert_eq!(u16::from_be_bytes([quoted_udp[0], quoted_udp[1]]), 53);
        assert_eq!(u16::from_be_bytes([quoted_udp[2], quoted_udp[3]]), 4040);
    }

    #[test]
    fn udp_remove_socket_drops_queued_datagrams() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let socket = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("UDP socket should bind");
        let (datagram, datagram_len) = udp_datagram(peer, 53, local, 4040, b"payload");
        assert!(
            stack
                .receive_udp(
                    IpAddress::Ipv4(peer),
                    IpAddress::Ipv4(local),
                    Bytes::copy_from_slice(&datagram[..datagram_len]),
                    RxFrameOffload::none(),
                )
                .expect("UDP datagram should be accepted")
        );

        stack
            .remove_udp_socket(socket)
            .expect("UDP socket should be removable");

        assert_eq!(stack.take_udp(socket), Err(StackError::UnknownSocket));
        assert!(stack.open_udp(UdpSocketBinding::wildcard(4040)).is_ok());
    }

    #[test]
    fn udp_full_socket_queue_does_not_starve_other_socket() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let saturated = stack
            .open_udp(UdpSocketBinding::wildcard(4040))
            .expect("first UDP socket should bind");
        let other = stack
            .open_udp(UdpSocketBinding::wildcard(4041))
            .expect("second UDP socket should bind");
        let (saturated_datagram, saturated_len) = udp_datagram(peer, 53, local, 4040, b"saturated");
        for index in 0..MAX_UDP_RX {
            assert!(
                stack
                    .receive_udp(
                        IpAddress::Ipv4(peer),
                        IpAddress::Ipv4(local),
                        Bytes::copy_from_slice(&saturated_datagram[..saturated_len]),
                        RxFrameOffload::none(),
                    )
                    .unwrap_or_else(|error| {
                        panic!("UDP datagram {index} should be accepted: {error}")
                    })
            );
        }
        assert!(!stack.receive_backpressured());
        let (dropped, dropped_len) = udp_datagram(peer, 53, local, 4040, b"dropped");
        assert!(
            stack
                .receive_udp(
                    IpAddress::Ipv4(peer),
                    IpAddress::Ipv4(local),
                    Bytes::copy_from_slice(&dropped[..dropped_len]),
                    RxFrameOffload::none(),
                )
                .expect("full UDP socket queue should drop without global backpressure")
        );
        let (other_datagram, other_len) = udp_datagram(peer, 53, local, 4041, b"other");
        assert!(
            stack
                .receive_udp(
                    IpAddress::Ipv4(peer),
                    IpAddress::Ipv4(local),
                    Bytes::copy_from_slice(&other_datagram[..other_len]),
                    RxFrameOffload::none(),
                )
                .expect("other UDP socket should not be starved by saturated socket")
        );

        assert_eq!(
            stack
                .take_udp(other)
                .expect("other UDP socket should exist")
                .expect("other UDP datagram should be queued")
                .bytes
                .as_ref(),
            b"other"
        );
        let mut saturated_count = 0;
        while let Some(datagram) = stack
            .take_udp(saturated)
            .expect("saturated UDP socket should exist")
        {
            assert!(
                matches!(datagram.bytes.as_ref(), b"saturated" | b"dropped"),
                "unexpected saturated UDP payload"
            );
            saturated_count += 1;
        }
        assert_eq!(saturated_count, MAX_UDP_RX - 1);
    }

    #[test]
    fn ipv4_receive_learns_source_mac_before_tcp_reset() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));

        let (segment, segment_len) = tcp_segment(
            peer,
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
        let mut frame = [0u8; crate::ETHERNET_FRAME_BYTES];
        let mut offset =
            EthernetFrame::encode_header(&mut frame, LOCAL_MAC, PEER_MAC, EthernetProtocol::Ipv4)
                .expect("Ethernet header should fit");
        offset += Ipv4Packet::encode_header(
            &mut frame[offset..],
            peer,
            local,
            crate::IpProtocol::Tcp,
            segment_len,
            1,
            64,
        )
        .expect("IPv4 header should fit");
        frame[offset..offset + segment_len].copy_from_slice(&segment[..segment_len]);
        offset += segment_len;

        stack
            .receive_frame(&frame[..offset], StackInstant::from_nanos(1))
            .expect("closed-port SYN should queue a reset");

        assert_eq!(
            stack.neighbor_mac(IpAddress::Ipv4(peer)),
            Some(PEER_MAC),
            "IPv4 receive path should learn the Ethernet source before replying"
        );
        let reset = stack.take_outbound().expect("RST frame should be queued");
        let ethernet = EthernetFrame::parse(reset.as_slice()).expect("Ethernet frame should parse");
        assert_eq!(ethernet.destination, PEER_MAC);
    }

    #[test]
    fn tcp_unknown_syn_to_local_address_queues_reset_ack() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let (syn, syn_len) = tcp_segment(
            peer,
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

        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn[..syn_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("closed-port SYN should be handled");

        let frame = stack.take_outbound().expect("RST frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let reset = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert_eq!(reset.source_port, 8080);
        assert_eq!(reset.destination_port, 49152);
        assert_eq!(reset.sequence, 0);
        assert_eq!(reset.acknowledgement, 11);
        assert_eq!(reset.flags, TcpFlags::RST.union(TcpFlags::ACK));
        assert!(reset.payload.is_empty());
    }

    #[test]
    fn tcp_unknown_ack_to_local_address_queues_reset() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let (ack, ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 49152,
                destination_port: 8080,
                sequence: 10,
                acknowledgement: 99,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
        );

        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&ack[..ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("closed-port ACK should be handled");

        let frame = stack.take_outbound().expect("RST frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let reset = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert_eq!(reset.source_port, 8080);
        assert_eq!(reset.destination_port, 49152);
        assert_eq!(reset.sequence, 99);
        assert_eq!(reset.acknowledgement, 0);
        assert_eq!(reset.flags, TcpFlags::RST);
        assert!(reset.payload.is_empty());
    }

    #[test]
    fn tcp_unknown_reset_does_not_answer_reset() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let (reset, reset_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 49152,
                destination_port: 8080,
                sequence: 10,
                acknowledgement: 99,
                flags: TcpFlags::RST,
                window_size: 0,
            },
        );

        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&reset[..reset_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("unknown RST should be ignored");

        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn tcp_wildcard_listener_ignores_foreign_unicast_destination() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let foreign = Ipv4Address::new([192, 0, 2, 11]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        let listener = stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                    port: 8080,
                },
                DEFAULT_TCP_LISTEN_BACKLOG,
            )
            .expect("wildcard TCP listener should bind");
        let (segment, segment_len) = tcp_segment(
            peer,
            foreign,
            TcpHeader {
                source_port: 49152,
                destination_port: 8080,
                sequence: 10,
                acknowledgement: 0,
                flags: TcpFlags::SYN,
                window_size: u16::MAX,
            },
        );
        let mut frame = [0u8; crate::ETHERNET_FRAME_BYTES];
        let mut offset =
            EthernetFrame::encode_header(&mut frame, LOCAL_MAC, PEER_MAC, EthernetProtocol::Ipv4)
                .expect("Ethernet header should fit");
        offset += Ipv4Packet::encode_header(
            &mut frame[offset..],
            peer,
            foreign,
            crate::IpProtocol::Tcp,
            segment_len,
            1,
            64,
        )
        .expect("IPv4 header should fit");
        frame[offset..offset + segment_len].copy_from_slice(&segment[..segment_len]);
        offset += segment_len;

        stack
            .receive_frame(&frame[..offset], StackInstant::from_nanos(1))
            .expect("foreign TCP frame should parse");
        stack
            .drive_tcp(StackInstant::from_nanos(1))
            .expect("TCP drive should not emit for foreign destination");

        assert_eq!(stack.take_tcp_accept(listener), Ok(None));
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn tcp_pure_ack_does_not_queue_readable_event() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let (mut stack, _) = open_established_tcp_stack(local, peer);
        let (ack, ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
        );

        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&ack[..ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(2),
            )
            .expect("pure ACK should be accepted");

        assert!(stack.take_event().is_none());
    }

    #[test]
    fn tcp_payload_queues_readable_event() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let (mut stack, socket) = open_established_tcp_stack(local, peer);
        let (payload, payload_len) = tcp_segment_with_payload(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
            b"payload",
        );

        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&payload[..payload_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(2),
            )
            .expect("payload should be accepted");

        assert_eq!(stack.take_event(), Some(StackEvent::TcpReadable { socket }));
        assert!(stack.take_event().is_none());
    }

    #[test]
    fn tcp_fin_queues_readable_event_for_eof() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let (mut stack, socket) = open_established_tcp_stack(local, peer);
        let (fin, fin_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK.union(TcpFlags::FIN),
                window_size: u16::MAX,
            },
        );

        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&fin[..fin_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(2),
            )
            .expect("FIN should be accepted");

        assert_eq!(stack.take_event(), Some(StackEvent::TcpReadable { socket }));
        assert_eq!(
            stack.tcp_read(socket, 1, StackInstant::from_nanos(3)),
            Ok(TcpReadState::Eof)
        );
    }

    #[test]
    fn tcp_time_wait_retains_tuple_until_deadline() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let local_endpoint = TcpEndpoint {
            address: IpAddress::Ipv4(local),
            port: 49152,
        };
        let remote_endpoint = TcpEndpoint {
            address: IpAddress::Ipv4(peer),
            port: 80,
        };
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let socket = stack
            .open_tcp_connect(local_endpoint, remote_endpoint, 7)
            .expect("test TCP connect should allocate a socket");
        let (syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN-ACK should establish the socket");
        stack
            .tcp_shutdown_send(socket)
            .expect("active close should start");
        stack
            .drive_tcp(StackInstant::from_nanos(2))
            .expect("FIN should be queued");
        let fin_frame = stack.take_outbound().expect("FIN frame should be queued");
        let ethernet =
            EthernetFrame::parse(fin_frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let fin = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert!(fin.flags.contains(TcpFlags::FIN));

        let (fin_ack, fin_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: fin.sequence.wrapping_add(1),
                flags: TcpFlags::ACK.union(TcpFlags::FIN),
                window_size: u16::MAX,
            },
        );
        let time_wait_started = 3;
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&fin_ack[..fin_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(time_wait_started),
            )
            .expect("peer FIN should enter TIME_WAIT");

        assert_eq!(
            stack.open_tcp_connect(local_endpoint, remote_endpoint, 99),
            Err(StackError::AddressInUse)
        );
        stack
            .drive_tcp(StackInstant::from_nanos(
                time_wait_started + crate::tcp::TCP_TIME_WAIT_NANOS,
            ))
            .expect("TIME_WAIT timer should expire");
        assert!(
            stack
                .open_tcp_connect(local_endpoint, remote_endpoint, 99)
                .is_ok()
        );
    }

    #[test]
    fn tcp_stack_accepts_replaceable_congestion_control() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new_with_congestion(
            StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES),
            NewReno::new(1460),
        );

        let socket = stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");

        assert_eq!(
            stack
                .tcp_socket(socket)
                .expect("test socket should exist")
                .congestion()
                .algorithm_name(),
            "newreno"
        );
    }

    #[test]
    fn passive_open_queues_accepted_stream_after_handshake() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let listener = stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                    port: 8080,
                },
                DEFAULT_TCP_LISTEN_BACKLOG,
            )
            .expect("wildcard TCP listener should allocate");

        let syn_ack = drive_passive_open_to_syn_ack(&mut stack, peer, local, 49152, 1);
        assert!(syn_ack.flags.contains(TcpFlags::SYN.union(TcpFlags::ACK)));
        assert_eq!(syn_ack.acknowledgement, 11);
        assert_eq!(syn_ack.maximum_segment_size, Some(1460));
        assert_eq!(
            syn_ack.window_scale,
            Some(crate::tcp::TCP_LOCAL_WINDOW_SCALE)
        );
        assert!(syn_ack.sack_permitted);
        assert!(syn_ack.timestamp_present);

        let (ack, ack_len) = tcp_segment(
            peer,
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
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&ack[..ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(2),
            )
            .expect("final ACK should establish accepted socket");

        let accepted = stack
            .take_tcp_accept(listener)
            .expect("accept poll should succeed")
            .expect("accepted stream should be queued");
        assert_eq!(
            accepted.remote,
            TcpEndpoint {
                address: IpAddress::Ipv4(peer),
                port: 49152,
            }
        );
    }

    #[test]
    fn passive_open_backlog_limits_pending_and_ready_children() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let first_peer = Ipv4Address::new([192, 0, 2, 20]);
        let second_peer = Ipv4Address::new([192, 0, 2, 21]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        for peer in [first_peer, second_peer] {
            stack.learn_neighbor(NeighborEntry {
                ip: IpAddress::Ipv4(peer),
                mac: PEER_MAC,
                state: NeighborState::Reachable,
                updated_at: StackInstant::from_nanos(0),
            });
        }
        let listener = stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                    port: 8080,
                },
                TcpListenBacklog::new(1),
            )
            .expect("TCP listener should allocate");

        let first_syn_ack = drive_passive_open_to_syn_ack(&mut stack, first_peer, local, 49152, 1);
        let (second_syn, second_syn_len) = tcp_segment(
            second_peer,
            local,
            TcpHeader {
                source_port: 49153,
                destination_port: 8080,
                sequence: 10,
                acknowledgement: 0,
                flags: TcpFlags::SYN,
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(second_peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&second_syn[..second_syn_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(2),
            )
            .expect("full-backlog SYN should be consumed");
        stack
            .drive_tcp(StackInstant::from_nanos(2))
            .expect("full-backlog drive should not fail");
        assert!(stack.take_outbound().is_none());

        let (first_ack, first_ack_len) = tcp_segment(
            first_peer,
            local,
            TcpHeader {
                source_port: 49152,
                destination_port: 8080,
                sequence: 11,
                acknowledgement: first_syn_ack.sequence.wrapping_add(1),
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(first_peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&first_ack[..first_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(3),
            )
            .expect("first final ACK should establish accepted socket");
        assert!(stack.take_outbound().is_none());

        let (second_retry, second_retry_len) = tcp_segment(
            second_peer,
            local,
            TcpHeader {
                source_port: 49153,
                destination_port: 8080,
                sequence: 10,
                acknowledgement: 0,
                flags: TcpFlags::SYN,
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(second_peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&second_retry[..second_retry_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(4),
            )
            .expect("ready-but-unaccepted backlog should still reject SYN");
        stack
            .drive_tcp(StackInstant::from_nanos(4))
            .expect("ready-backlog drive should not fail");
        assert!(stack.take_outbound().is_none());

        let accepted = stack
            .take_tcp_accept(listener)
            .expect("accept poll should succeed")
            .expect("first accepted stream should be queued");
        assert_eq!(accepted.remote.port, 49152);
        let second_syn_ack =
            drive_passive_open_to_syn_ack(&mut stack, second_peer, local, 49153, 5);
        assert!(
            second_syn_ack
                .flags
                .contains(TcpFlags::SYN.union(TcpFlags::ACK))
        );
    }

    #[test]
    fn passive_open_rst_releases_half_open_backlog_slot() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let first_peer = Ipv4Address::new([192, 0, 2, 20]);
        let second_peer = Ipv4Address::new([192, 0, 2, 21]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        for peer in [first_peer, second_peer] {
            stack.learn_neighbor(NeighborEntry {
                ip: IpAddress::Ipv4(peer),
                mac: PEER_MAC,
                state: NeighborState::Reachable,
                updated_at: StackInstant::from_nanos(0),
            });
        }
        stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                    port: 8080,
                },
                TcpListenBacklog::new(1),
            )
            .expect("TCP listener should allocate");

        let _first_syn_ack = drive_passive_open_to_syn_ack(&mut stack, first_peer, local, 49152, 1);
        let (rst, rst_len) = tcp_segment(
            first_peer,
            local,
            TcpHeader {
                source_port: 49152,
                destination_port: 8080,
                sequence: 11,
                acknowledgement: 0,
                flags: TcpFlags::RST,
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(first_peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&rst[..rst_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(2),
            )
            .expect("half-open RST should be handled");

        let second_syn_ack =
            drive_passive_open_to_syn_ack(&mut stack, second_peer, local, 49153, 3);
        assert!(
            second_syn_ack
                .flags
                .contains(TcpFlags::SYN.union(TcpFlags::ACK))
        );
    }

    #[test]
    fn passive_open_out_of_window_rst_does_not_release_backlog_slot() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let first_peer = Ipv4Address::new([192, 0, 2, 20]);
        let second_peer = Ipv4Address::new([192, 0, 2, 21]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        for peer in [first_peer, second_peer] {
            stack.learn_neighbor(NeighborEntry {
                ip: IpAddress::Ipv4(peer),
                mac: PEER_MAC,
                state: NeighborState::Reachable,
                updated_at: StackInstant::from_nanos(0),
            });
        }
        stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                    port: 8080,
                },
                TcpListenBacklog::new(1),
            )
            .expect("TCP listener should allocate");

        let _first_syn_ack = drive_passive_open_to_syn_ack(&mut stack, first_peer, local, 49152, 1);
        let (rst, rst_len) = tcp_segment(
            first_peer,
            local,
            TcpHeader {
                source_port: 49152,
                destination_port: 8080,
                sequence: 11 + (u32::from(u16::MAX) << crate::tcp::TCP_LOCAL_WINDOW_SCALE) + 1,
                acknowledgement: 0,
                flags: TcpFlags::RST,
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(first_peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&rst[..rst_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(2),
            )
            .expect("out-of-window half-open RST should be ignored");

        let (second_syn, second_syn_len) = tcp_segment(
            second_peer,
            local,
            TcpHeader {
                source_port: 49153,
                destination_port: 8080,
                sequence: 10,
                acknowledgement: 0,
                flags: TcpFlags::SYN,
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(second_peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&second_syn[..second_syn_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(3),
            )
            .expect("full backlog should consume second SYN");
        stack
            .drive_tcp(StackInstant::from_nanos(3))
            .expect("full-backlog drive should not fail");
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn tcp_exact_listener_accepts_syn() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 8080,
                },
                DEFAULT_TCP_LISTEN_BACKLOG,
            )
            .expect("exact TCP listener should allocate");

        let (syn, syn_len) = tcp_segment(
            peer,
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
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn[..syn_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN should be accepted by exact listener");
        stack
            .drive_tcp(StackInstant::from_nanos(1))
            .expect("SYN-ACK should be queued");

        let frame = stack
            .take_outbound()
            .expect("SYN-ACK frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let syn_ack = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert!(syn_ack.flags.contains(TcpFlags::SYN.union(TcpFlags::ACK)));
    }

    #[test]
    fn ipv4_tcp_fragments_reassemble_and_reach_listener() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 8080,
                },
                DEFAULT_TCP_LISTEN_BACKLOG,
            )
            .expect("TCP listener should bind");
        let (first, second) = ipv4_tcp_fragments(
            peer,
            local,
            TcpHeader {
                source_port: 49152,
                destination_port: 8080,
                sequence: 10,
                acknowledgement: 0,
                flags: TcpFlags::SYN,
                window_size: u16::MAX,
            },
            &[],
            16,
            0x2345,
        );

        stack
            .receive_frame(&second.0[..second.1], StackInstant::from_nanos(1))
            .expect("final TCP fragment should be accepted first");
        stack
            .receive_frame(&first.0[..first.1], StackInstant::from_nanos(2))
            .expect("initial TCP fragment should complete SYN reassembly");
        stack
            .drive_tcp(StackInstant::from_nanos(2))
            .expect("SYN-ACK should be queued after fragmented SYN");

        let frame = stack
            .take_outbound()
            .expect("fragmented SYN should queue SYN-ACK");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let syn_ack = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert!(syn_ack.flags.contains(TcpFlags::SYN.union(TcpFlags::ACK)));
    }

    #[test]
    fn tcp_read_reports_eof_after_fin_and_drained_payload() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let socket = stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");
        let (syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN-ACK should establish the socket");

        let (data_fin, data_fin_len) = tcp_segment_with_payload(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK.union(TcpFlags::FIN),
                window_size: u16::MAX,
            },
            b"ok",
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&data_fin[..data_fin_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(2),
            )
            .expect("data FIN should be accepted");

        assert_eq!(
            stack
                .tcp_read(socket, 8, StackInstant::from_nanos(3))
                .unwrap(),
            TcpReadState::Data(Bytes::from_static(b"ok"))
        );
        assert_eq!(
            stack
                .tcp_read(socket, 8, StackInstant::from_nanos(4))
                .unwrap(),
            TcpReadState::Eof
        );
    }

    #[test]
    fn tcp_read_waits_for_missing_bytes_before_out_of_order_fin_eof() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let socket = stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");
        let (syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN-ACK should establish the socket");

        let (early_fin, early_fin_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 103,
                acknowledgement: 8,
                flags: TcpFlags::ACK.union(TcpFlags::FIN),
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&early_fin[..early_fin_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(2),
            )
            .expect("out-of-order FIN should be tracked");
        assert_eq!(
            stack
                .tcp_read(socket, 8, StackInstant::from_nanos(3))
                .unwrap(),
            TcpReadState::Pending
        );

        let (payload, payload_len) = tcp_segment_with_payload(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
            b"ok",
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&payload[..payload_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(4),
            )
            .expect("missing payload should complete before FIN");

        assert_eq!(
            stack
                .tcp_read(socket, 8, StackInstant::from_nanos(5))
                .unwrap(),
            TcpReadState::Data(Bytes::from_static(b"ok"))
        );
        assert_eq!(
            stack
                .tcp_read(socket, 8, StackInstant::from_nanos(6))
                .unwrap(),
            TcpReadState::Eof
        );
    }

    #[test]
    fn tcp_drive_piggybacks_ack_on_data_segment() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let socket = stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");
        let (syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN-ACK should establish the socket");
        let (request, request_len) = tcp_segment_with_payload(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
            b"r",
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&request[..request_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(2),
            )
            .expect("payload should request an ACK");
        assert_eq!(stack.tcp_send(socket, b"w").unwrap(), 1);

        stack
            .drive_tcp(StackInstant::from_nanos(3))
            .expect("data should be queued");

        let frame = stack.take_outbound().expect("data frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let data = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert!(data.flags.contains(TcpFlags::ACK));
        assert_eq!(data.payload, b"w");
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn ipv4_fragmentation_needed_caps_tcp_transmit_segment() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let router = Ipv4Address::new([192, 0, 2, 1]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let socket = stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");
        let (syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN-ACK should establish the socket");
        stack
            .drive_tcp(StackInstant::from_nanos(1))
            .expect("handshake ACK should be queued");
        let _ = stack
            .take_outbound()
            .expect("handshake ACK frame should be queued");
        let (icmp, icmp_len) = ipv4_icmp_unreachable_for_tcp(
            router,
            local,
            local,
            peer,
            TcpHeader {
                source_port: 49152,
                destination_port: 80,
                sequence: 8,
                acknowledgement: 101,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
            Icmpv4DestinationUnreachableCode::FragmentationNeeded { next_hop_mtu: 576 },
        );
        stack
            .receive_frame(&icmp[..icmp_len], StackInstant::from_nanos(2))
            .expect("ICMPv4 fragmentation-needed should update PMTU");

        let payload = [0u8; 1024];
        assert_eq!(stack.tcp_send(socket, &payload), Ok(payload.len()));
        stack
            .drive_tcp(StackInstant::from_nanos(3))
            .expect("PMTU-limited TCP data should be queued");

        let frame = stack.take_outbound().expect("data frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let data = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert_eq!(
            data.payload.len(),
            576 - Ipv4Packet::MIN_HEADER_LEN - TcpPacket::MIN_HEADER_LEN - data.options.raw().len()
        );
    }

    #[test]
    fn ipv4_fragmentation_needed_eagerly_retransmits_in_flight_tcp_segment() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let router = Ipv4Address::new([192, 0, 2, 1]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.add_ipv4_address(Ipv4Cidr::new(local, 24));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        let socket = stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");
        let (syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN-ACK should establish the socket");
        stack
            .drive_tcp(StackInstant::from_nanos(1))
            .expect("handshake ACK should be queued");
        let _ = stack
            .take_outbound()
            .expect("handshake ACK frame should be queued");

        let payload = [0u8; 1024];
        assert_eq!(stack.tcp_send(socket, &payload), Ok(payload.len()));
        stack
            .drive_tcp(StackInstant::from_nanos(2))
            .expect("full-size TCP data should be queued");
        let original = stack
            .take_outbound()
            .expect("initial data frame should be queued");
        let ethernet =
            EthernetFrame::parse(original.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let original_tcp = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert_eq!(original_tcp.payload.len(), payload.len());

        let (icmp, icmp_len) = ipv4_icmp_unreachable_for_tcp(
            router,
            local,
            local,
            peer,
            TcpHeader {
                source_port: 49152,
                destination_port: 80,
                sequence: original_tcp.sequence,
                acknowledgement: original_tcp.acknowledgement,
                flags: original_tcp.flags,
                window_size: original_tcp.window_size,
            },
            Icmpv4DestinationUnreachableCode::FragmentationNeeded { next_hop_mtu: 576 },
        );
        stack
            .receive_frame(&icmp[..icmp_len], StackInstant::from_nanos(3))
            .expect("ICMPv4 fragmentation-needed should update PMTU");
        stack
            .drive_tcp(StackInstant::from_nanos(3))
            .expect("PMTU update should queue an eager retransmission");

        let frame = stack
            .take_outbound()
            .expect("PMTU-limited retransmission should be queued before RTO");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let retransmit = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert_eq!(retransmit.sequence, original_tcp.sequence);
        assert_eq!(
            retransmit.payload.len(),
            576 - Ipv4Packet::MIN_HEADER_LEN
                - TcpPacket::MIN_HEADER_LEN
                - retransmit.options.raw().len()
        );
    }

    #[test]
    fn tcp_drive_queues_delayed_ack_at_deadline() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv4(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });
        stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");
        let (syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN-ACK should establish the socket");
        stack
            .drive_tcp(StackInstant::from_nanos(1))
            .expect("handshake ACK should be queued");
        let _ = stack
            .take_outbound()
            .expect("handshake ACK frame should be queued");

        let payload = [0u8; crate::tcp::TCP_RECEIVE_SEGMENT_BYTES];
        let received_at = 2;
        let (request, request_len) = tcp_segment_with_payload(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
            },
            &payload,
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&request[..request_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(received_at),
            )
            .expect("payload should be accepted");
        assert_eq!(
            stack.next_tcp_deadline(),
            Some(StackInstant::from_nanos(
                received_at + crate::tcp::TCP_DELAYED_ACK_NANOS
            ))
        );
        stack
            .drive_tcp(StackInstant::from_nanos(
                received_at + crate::tcp::TCP_DELAYED_ACK_NANOS - 1,
            ))
            .expect("early TCP drive should not queue ACK");
        assert!(stack.take_outbound().is_none());

        stack
            .drive_tcp(StackInstant::from_nanos(
                received_at + crate::tcp::TCP_DELAYED_ACK_NANOS,
            ))
            .expect("delayed ACK should be queued");
        let frame = stack
            .take_outbound()
            .expect("delayed ACK frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv4 = Ipv4Packet::parse(ethernet.payload).expect("IPv4 packet should parse");
        let ack = TcpPacket::parse(ipv4.payload).expect("TCP packet should parse");
        assert!(ack.flags.contains(TcpFlags::ACK));
        assert!(ack.payload.is_empty());
        assert!(stack.next_tcp_deadline().is_none());
    }

    #[test]
    fn tcp_receive_backpressure_is_indexed() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let socket = stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: IpAddress::Ipv4(local),
                    port: 49152,
                },
                TcpEndpoint {
                    address: IpAddress::Ipv4(peer),
                    port: 80,
                },
                7,
            )
            .expect("test TCP connect should allocate a socket");
        let (syn_ack, syn_ack_len) = tcp_segment(
            peer,
            local,
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
        );
        stack
            .receive_tcp(
                IpAddress::Ipv4(peer),
                IpAddress::Ipv4(local),
                &Bytes::copy_from_slice(&syn_ack[..syn_ack_len]),
                RxFrameOffload::none(),
                StackInstant::from_nanos(1),
            )
            .expect("SYN-ACK should establish the socket");

        let payload = [0u8; crate::tcp::TCP_RECEIVE_SEGMENT_BYTES];
        for index in 0..crate::tcp::TCP_RECEIVE_BACKPRESSURE_SEGMENTS {
            let (segment, segment_len) = tcp_segment_with_payload(
                peer,
                local,
                TcpHeader {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101 + index as u32 * crate::tcp::TCP_RECEIVE_SEGMENT_BYTES as u32,
                    acknowledgement: 8,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                },
                &payload,
            );
            stack
                .receive_tcp(
                    IpAddress::Ipv4(peer),
                    IpAddress::Ipv4(local),
                    &Bytes::copy_from_slice(&segment[..segment_len]),
                    RxFrameOffload::none(),
                    StackInstant::from_nanos(index as u64 + 2),
                )
                .expect("payload should be accepted");
        }
        assert!(stack.receive_backpressured());

        assert!(matches!(
            stack.tcp_read(
                socket,
                crate::tcp::TCP_RECEIVE_SEGMENT_BYTES,
                StackInstant::from_nanos(1000)
            ),
            Ok(TcpReadState::Data(_))
        ));
        assert!(!stack.receive_backpressured());
    }

    #[test]
    fn tcp_socket_slab_reuses_removed_slot() {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let first = stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                    port: 8000,
                },
                DEFAULT_TCP_LISTEN_BACKLOG,
            )
            .expect("first TCP listener should allocate");
        let second = stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                    port: 8001,
                },
                DEFAULT_TCP_LISTEN_BACKLOG,
            )
            .expect("second TCP listener should allocate");
        assert_eq!(stack.tcp.active_len(), 2);

        stack
            .remove_tcp_socket(first)
            .expect("allocated socket should be removable");
        assert_eq!(stack.tcp.active_len(), 1);
        assert_eq!(stack.tcp.active_index(0), socket_index(second));

        let reused = stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                    port: 8002,
                },
                DEFAULT_TCP_LISTEN_BACKLOG,
            )
            .expect("reused TCP listener should allocate");

        assert_eq!(stack.tcp.active_len(), 2);
        assert_eq!(stack.tcp.active_index(0), socket_index(second));
        assert_eq!(stack.tcp.active_index(1), socket_index(reused));
        assert_eq!(reused.raw(), first.raw());
        assert_ne!(reused.raw(), second.raw());
    }

    #[test]
    fn tcp_socket_slab_clone_preserves_ids_and_independent_ownership() {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let first = stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                    port: 8000,
                },
                DEFAULT_TCP_LISTEN_BACKLOG,
            )
            .expect("first TCP listener should allocate");
        let second = stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
                    port: 8001,
                },
                DEFAULT_TCP_LISTEN_BACKLOG,
            )
            .expect("second TCP listener should allocate");

        let mut cloned = stack.clone();
        assert_eq!(cloned.tcp.active_len(), 2);
        assert_eq!(
            cloned
                .tcp_socket(first)
                .expect("first cloned socket should exist")
                .local_endpoint()
                .expect("first cloned socket should be bound")
                .port,
            8000
        );
        assert_eq!(
            cloned
                .tcp_socket(second)
                .expect("second cloned socket should exist")
                .local_endpoint()
                .expect("second cloned socket should be bound")
                .port,
            8001
        );

        cloned
            .remove_tcp_socket(first)
            .expect("cloned socket should be removable");
        assert!(cloned.tcp_socket(first).is_err());
        assert!(stack.tcp_socket(first).is_ok());
    }

    // ---------------------------------------------------------------
    // IPv6 Neighbor Discovery / SLAAC frame-level coverage.
    //
    // The prefix and router addresses below mirror QEMU's user-mode
    // network, which advertises fec0::/64 with the router at fec0::2
    // (link-local fe80::2) and a resolver at fec0::3.
    // ---------------------------------------------------------------

    const ROUTER_LINK_LOCAL: Ipv6Address =
        Ipv6Address::new([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02]);
    const ROUTER_MAC: EthernetAddress = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];

    fn slaac_expected_address(mac: EthernetAddress) -> Ipv6Cidr {
        Ipv6Cidr::new(
            Ipv6Address::from_prefix_and_eui64(
                Ipv6Address::new([0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                mac,
            ),
            64,
        )
    }

    /// Encodes a Router Advertisement body carrying a source
    /// link-layer address, a `fec0::/64` on-link + autonomous prefix,
    /// an MTU option, and an RDNSS option for `fec0::3`.
    fn router_advertisement_body(
        output: &mut [u8],
        source: Ipv6Address,
        destination: Ipv6Address,
        router_lifetime_seconds: u16,
    ) -> usize {
        output[..16].fill(0);
        output[0] = 134;
        output[4] = 64;
        output[6..8].copy_from_slice(&router_lifetime_seconds.to_be_bytes());
        let mut offset = 16;

        output[offset..offset + 8].fill(0);
        output[offset] = 1;
        output[offset + 1] = 1;
        output[offset + 2..offset + 8].copy_from_slice(&ROUTER_MAC);
        offset += 8;

        output[offset..offset + 32].fill(0);
        output[offset] = 3;
        output[offset + 1] = 4;
        output[offset + 2] = 64;
        output[offset + 3] = 0xc0;
        output[offset + 4..offset + 8].copy_from_slice(&86_400u32.to_be_bytes());
        output[offset + 8..offset + 12].copy_from_slice(&14_400u32.to_be_bytes());
        output[offset + 16] = 0xfe;
        output[offset + 17] = 0xc0;
        offset += 32;

        output[offset..offset + 8].fill(0);
        output[offset] = 5;
        output[offset + 1] = 1;
        output[offset + 4..offset + 8].copy_from_slice(&1500u32.to_be_bytes());
        offset += 8;

        output[offset..offset + 24].fill(0);
        output[offset] = 25;
        output[offset + 1] = 3;
        output[offset + 4..offset + 8].copy_from_slice(&600u32.to_be_bytes());
        output[offset + 8] = 0xfe;
        output[offset + 9] = 0xc0;
        output[offset + 23] = 3;
        offset += 24;

        let checksum = crate::icmpv6_checksum(source, destination, &output[..offset]);
        output[2..4].copy_from_slice(&checksum.to_be_bytes());
        offset
    }

    fn router_advertisement_frame(
        router_lifetime_seconds: u16,
        hop_limit: u8,
        source: Ipv6Address,
    ) -> ([u8; crate::ETHERNET_FRAME_BYTES], usize) {
        let destination = Ipv6Address::ALL_NODES_LINK_LOCAL;
        let mut body = [0u8; 256];
        let body_len =
            router_advertisement_body(&mut body, source, destination, router_lifetime_seconds);
        let mut frame = [0u8; crate::ETHERNET_FRAME_BYTES];
        let mut offset = EthernetFrame::encode_header(
            &mut frame,
            ipv6_multicast_mac(destination),
            ROUTER_MAC,
            EthernetProtocol::Ipv6,
        )
        .expect("test Ethernet header should fit");
        offset += Ipv6Packet::encode_header(
            &mut frame[offset..],
            source,
            destination,
            crate::IpProtocol::Icmpv6,
            body_len,
            hop_limit,
        )
        .expect("test IPv6 header should fit");
        frame[offset..offset + body_len].copy_from_slice(&body[..body_len]);
        offset += body_len;
        (frame, offset)
    }

    fn autoconfigured_stack() -> Stack {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.configure_ipv6_link_local();
        let (frame, len) = router_advertisement_frame(300, 255, ROUTER_LINK_LOCAL);
        stack
            .receive_frame(&frame[..len], StackInstant::from_nanos(1))
            .expect("router advertisement should be handled");
        stack
    }

    #[test]
    fn ipv6_link_local_is_derived_from_the_interface_mac() {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let link_local = stack.configure_ipv6_link_local();

        assert_eq!(
            link_local,
            Ipv6Cidr::new(Ipv6Address::link_local_from_mac(LOCAL_MAC), 64)
        );
        assert_eq!(
            stack.ipv6_link_local_address(),
            Some(Ipv6Address::link_local_from_mac(LOCAL_MAC))
        );
        // The link-local prefix is installed as a connected route.
        assert_eq!(
            stack
                .routes()
                .resolve(IpAddress::Ipv6(ROUTER_LINK_LOCAL))
                .map(|route| route.gateway),
            Some(None)
        );
    }

    #[test]
    fn ipv6_autoconfig_queues_router_solicitation_from_link_local() {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.configure_ipv6_link_local();

        stack
            .drive_ipv6_autoconfig(StackInstant::from_nanos(0))
            .expect("router solicitation should queue");

        let frame = stack
            .take_outbound()
            .expect("router solicitation frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        assert_eq!(
            ethernet.destination,
            ipv6_multicast_mac(Ipv6Address::ALL_ROUTERS_LINK_LOCAL)
        );
        let ipv6 = Ipv6Packet::parse(ethernet.payload).expect("IPv6 packet should parse");
        assert_eq!(ipv6.source, Ipv6Address::link_local_from_mac(LOCAL_MAC));
        assert_eq!(ipv6.destination, Ipv6Address::ALL_ROUTERS_LINK_LOCAL);
        // RFC 4861 requires hop limit 255 on every NDP message.
        assert_eq!(ipv6.hop_limit, 255);
        assert!(crate::icmpv6_checksum_valid(
            ipv6.source,
            ipv6.destination,
            ipv6.payload
        ));
        assert_eq!(ipv6.payload[0], 133);
        // Source Link-Layer Address option.
        assert_eq!(ipv6.payload[8], 1);
        assert_eq!(&ipv6.payload[10..16], &LOCAL_MAC);

        // Inside the retransmit interval nothing more is queued.
        stack
            .drive_ipv6_autoconfig(StackInstant::from_nanos(1))
            .expect("second poll should be a no-op");
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn router_advertisement_configures_slaac_address_route_and_resolvers() {
        let mut stack = autoconfigured_stack();

        assert!(stack.ipv6_autoconfigured());
        assert!(
            stack
                .ipv6_addresses()
                .any(|address| address == slaac_expected_address(LOCAL_MAC))
        );
        let default_route = stack
            .routes()
            .resolve(IpAddress::Ipv6(Ipv6Address::new([
                0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
            ])))
            .expect("default IPv6 route should exist");
        assert_eq!(
            default_route.gateway,
            Some(IpAddress::Ipv6(ROUTER_LINK_LOCAL))
        );
        assert_eq!(
            stack.ipv6_dns_servers().collect::<Vec<_>>(),
            vec![Ipv6Address::new([
                0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3
            ])]
        );
        // The router's link-layer address came from the advertisement's
        // source link-layer option, so no solicitation is needed to
        // reach it.
        assert!(
            stack
                .neighbors()
                .any(|entry| entry.ip == IpAddress::Ipv6(ROUTER_LINK_LOCAL)
                    && entry.mac == ROUTER_MAC)
        );
        let mut autoconfigured = false;
        while let Some(event) = stack.take_event() {
            if let StackEvent::Ipv6Autoconfigured(configuration) = event {
                assert_eq!(configuration.router, ROUTER_LINK_LOCAL);
                autoconfigured = true;
            }
        }
        assert!(
            autoconfigured,
            "an autoconfiguration event should be queued"
        );

        // Autoconfiguration stops soliciting once it has an answer.
        stack
            .drive_ipv6_autoconfig(StackInstant::from_nanos(2))
            .expect("configured interface should not solicit");
        assert!(stack.take_outbound().is_none());
    }

    #[test]
    fn router_advertisement_with_modified_hop_limit_is_rejected() {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.configure_ipv6_link_local();
        let (frame, len) = router_advertisement_frame(300, 64, ROUTER_LINK_LOCAL);

        stack
            .receive_frame(&frame[..len], StackInstant::from_nanos(1))
            .expect("forwarded advertisement should be dropped without error");

        assert!(!stack.ipv6_autoconfigured());
        assert!(
            !stack
                .ipv6_addresses()
                .any(|address| address == slaac_expected_address(LOCAL_MAC))
        );
    }

    #[test]
    fn router_advertisement_from_global_source_is_rejected() {
        let global = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.configure_ipv6_link_local();
        let (frame, len) = router_advertisement_frame(300, 255, global);

        stack
            .receive_frame(&frame[..len], StackInstant::from_nanos(1))
            .expect("non-link-local advertisement should be dropped without error");

        assert!(!stack.ipv6_autoconfigured());
    }

    #[test]
    fn zero_router_lifetime_withdraws_the_default_ipv6_route() {
        let mut stack = autoconfigured_stack();
        let (frame, len) = router_advertisement_frame(0, 255, ROUTER_LINK_LOCAL);

        stack
            .receive_frame(&frame[..len], StackInstant::from_nanos(2))
            .expect("router advertisement should be handled");

        let off_link = IpAddress::Ipv6(Ipv6Address::new([
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ]));
        assert!(stack.routes().resolve(off_link).is_none());
        // The on-link prefix survives: it is a connected route, not a
        // router-lifetime-scoped one.
        assert!(
            stack
                .routes()
                .resolve(IpAddress::Ipv6(Ipv6Address::new([
                    0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9
                ])))
                .is_some()
        );
    }

    #[test]
    fn neighbor_solicitation_for_a_slaac_address_is_answered() {
        let mut stack = autoconfigured_stack();
        let target = slaac_expected_address(LOCAL_MAC).address();
        let peer = Ipv6Address::new([0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9]);
        let destination = target.solicited_node_multicast();

        let mut body = [0u8; 64];
        let body_len = Icmpv6Packet::encode_neighbor_solicitation(
            &mut body,
            peer,
            destination,
            target,
            PEER_MAC,
        )
        .expect("neighbor solicitation should encode");
        let mut frame = [0u8; crate::ETHERNET_FRAME_BYTES];
        let mut offset = EthernetFrame::encode_header(
            &mut frame,
            ipv6_multicast_mac(destination),
            PEER_MAC,
            EthernetProtocol::Ipv6,
        )
        .expect("test Ethernet header should fit");
        offset += Ipv6Packet::encode_header(
            &mut frame[offset..],
            peer,
            destination,
            crate::IpProtocol::Icmpv6,
            body_len,
            255,
        )
        .expect("test IPv6 header should fit");
        frame[offset..offset + body_len].copy_from_slice(&body[..body_len]);
        offset += body_len;

        stack
            .receive_frame(&frame[..offset], StackInstant::from_nanos(3))
            .expect("neighbor solicitation should be handled");

        let response = stack
            .take_outbound()
            .expect("neighbor advertisement should be queued");
        let ethernet =
            EthernetFrame::parse(response.as_slice()).expect("Ethernet frame should parse");
        assert_eq!(ethernet.destination, PEER_MAC);
        let ipv6 = Ipv6Packet::parse(ethernet.payload).expect("IPv6 packet should parse");
        assert_eq!(ipv6.source, target);
        assert_eq!(ipv6.destination, peer);
        assert_eq!(ipv6.hop_limit, 255);
        let Some(Icmpv6Packet::NeighborAdvertisement {
            target: advertised,
            solicited,
            override_cache,
            options,
        }) = Icmpv6Packet::parse(ipv6.payload)
        else {
            panic!("expected a neighbor advertisement");
        };
        assert_eq!(advertised, target);
        assert!(solicited);
        assert!(override_cache);
        assert_eq!(options.target_link_layer_address(), Some(LOCAL_MAC));
        // The solicitation's own source link-layer option seeded the cache.
        assert!(
            stack
                .neighbors()
                .any(|entry| entry.ip == IpAddress::Ipv6(peer) && entry.mac == PEER_MAC)
        );
    }

    #[test]
    fn neighbor_advertisement_without_override_keeps_the_cached_address() {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        stack.configure_ipv6_link_local();
        let peer = Ipv6Address::new([0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9]);
        stack.learn_neighbor(NeighborEntry {
            ip: IpAddress::Ipv6(peer),
            mac: PEER_MAC,
            state: NeighborState::Reachable,
            updated_at: StackInstant::from_nanos(0),
        });

        let impostor: EthernetAddress = [0x02, 0, 0, 0, 0, 0x99];
        let mut body = [0u8; 64];
        let body_len = Icmpv6Packet::encode_neighbor_advertisement(
            &mut body,
            peer,
            Ipv6Address::link_local_from_mac(LOCAL_MAC),
            peer,
            impostor,
        )
        .expect("neighbor advertisement should encode");
        // Clear the override bit that the encoder sets by default and
        // restore the checksum for the edited body.
        body[4] &= !0x20;
        body[2..4].fill(0);
        let checksum = crate::icmpv6_checksum(
            peer,
            Ipv6Address::link_local_from_mac(LOCAL_MAC),
            &body[..body_len],
        );
        body[2..4].copy_from_slice(&checksum.to_be_bytes());

        let mut frame = [0u8; crate::ETHERNET_FRAME_BYTES];
        let mut offset =
            EthernetFrame::encode_header(&mut frame, LOCAL_MAC, impostor, EthernetProtocol::Ipv6)
                .expect("test Ethernet header should fit");
        offset += Ipv6Packet::encode_header(
            &mut frame[offset..],
            peer,
            Ipv6Address::link_local_from_mac(LOCAL_MAC),
            crate::IpProtocol::Icmpv6,
            body_len,
            255,
        )
        .expect("test IPv6 header should fit");
        frame[offset..offset + body_len].copy_from_slice(&body[..body_len]);
        offset += body_len;

        stack
            .receive_frame(&frame[..offset], StackInstant::from_nanos(4))
            .expect("neighbor advertisement should be handled");

        assert!(
            stack
                .neighbors()
                .any(|entry| entry.ip == IpAddress::Ipv6(peer) && entry.mac == PEER_MAC),
            "an advertisement without the override bit must not replace a cached address"
        );
    }

    #[test]
    fn ipv6_udp_send_to_an_unknown_neighbor_solicits_it() {
        let mut stack = autoconfigured_stack();
        let peer = Ipv6Address::new([0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9]);

        assert_eq!(
            stack.send_udp_ipv6(
                peer,
                UdpEgress {
                    source_port: 4040,
                    destination_port: 8080,
                    payload: b"probe",
                    hop_limit: crate::DEFAULT_HOP_LIMIT,
                },
                StackInstant::from_nanos(5)
            ),
            Ok(0),
            "no payload bytes go out while the neighbor is being resolved"
        );

        let frame = stack
            .take_outbound()
            .expect("neighbor solicitation should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        assert_eq!(
            ethernet.destination,
            ipv6_multicast_mac(peer.solicited_node_multicast())
        );
        let ipv6 = Ipv6Packet::parse(ethernet.payload).expect("IPv6 packet should parse");
        assert_eq!(ipv6.source, slaac_expected_address(LOCAL_MAC).address());
        let Some(Icmpv6Packet::NeighborSolicitation { target, options }) =
            Icmpv6Packet::parse(ipv6.payload)
        else {
            panic!("expected a neighbor solicitation");
        };
        assert_eq!(target, peer);
        assert_eq!(options.source_link_layer_address(), Some(LOCAL_MAC));
    }

    #[test]
    fn ipv6_udp_datagram_is_routed_through_the_advertised_default_router() {
        let mut stack = autoconfigured_stack();
        let remote = Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

        stack
            .send_udp_ipv6(
                remote,
                UdpEgress {
                    source_port: 4040,
                    destination_port: 8080,
                    payload: b"payload",
                    hop_limit: crate::DEFAULT_HOP_LIMIT,
                },
                StackInstant::from_nanos(6),
            )
            .expect("datagram should route through the advertised router");

        let frame = stack.take_outbound().expect("datagram should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        // Next hop is the router, so the frame carries the router's MAC
        // while the IPv6 destination stays the final remote address.
        assert_eq!(ethernet.destination, ROUTER_MAC);
        let ipv6 = Ipv6Packet::parse(ethernet.payload).expect("IPv6 packet should parse");
        assert_eq!(ipv6.source, slaac_expected_address(LOCAL_MAC).address());
        assert_eq!(ipv6.destination, remote);
        let udp = UdpPacket::parse(ipv6.payload).expect("UDP datagram should parse");
        assert_eq!(udp.payload, b"payload");
        assert!(crate::udp_checksum_valid(
            IpAddress::Ipv6(ipv6.source),
            IpAddress::Ipv6(ipv6.destination),
            ipv6.payload
        ));
    }

    #[test]
    fn tcp_over_ipv6_completes_the_handshake_against_frame_fixtures() {
        let mut stack = autoconfigured_stack();
        let local_address = slaac_expected_address(LOCAL_MAC).address();
        let remote_address =
            Ipv6Address::new([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let local = TcpEndpoint {
            address: IpAddress::Ipv6(local_address),
            port: 49152,
        };
        let remote = TcpEndpoint {
            address: IpAddress::Ipv6(remote_address),
            port: 80,
        };

        let socket = stack
            .open_tcp_connect(local, remote, 1000)
            .expect("IPv6 TCP connect should allocate");
        stack
            .drive_tcp(StackInstant::from_nanos(10))
            .expect("SYN should be queued");

        let frame = stack.take_outbound().expect("SYN frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        assert_eq!(ethernet.destination, ROUTER_MAC);
        let ipv6 = Ipv6Packet::parse(ethernet.payload).expect("IPv6 packet should parse");
        assert_eq!(ipv6.source, local_address);
        assert_eq!(ipv6.destination, remote_address);
        assert_eq!(ipv6.next_header, crate::IpProtocol::Tcp);
        let syn = TcpPacket::parse(ipv6.payload).expect("TCP segment should parse");
        assert!(syn.flags.contains(TcpFlags::SYN));
        assert!(crate::tcp_checksum_valid(
            IpAddress::Ipv6(ipv6.source),
            IpAddress::Ipv6(ipv6.destination),
            ipv6.payload
        ));

        // Feed the peer's SYN-ACK back in as a real v6 frame.
        let mut segment = [0u8; crate::ETHERNET_FRAME_BYTES];
        let segment_len = TcpPacket::encode(
            &mut segment,
            IpAddress::Ipv6(remote_address),
            IpAddress::Ipv6(local_address),
            TcpHeader {
                source_port: 80,
                destination_port: 49152,
                sequence: 5000,
                acknowledgement: syn.sequence.wrapping_add(1),
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
            },
            &[],
            TransportChecksum::Software,
        )
        .expect("test SYN-ACK should fit");
        let mut frame = [0u8; crate::ETHERNET_FRAME_BYTES];
        let mut offset =
            EthernetFrame::encode_header(&mut frame, LOCAL_MAC, ROUTER_MAC, EthernetProtocol::Ipv6)
                .expect("test Ethernet header should fit");
        offset += Ipv6Packet::encode_header(
            &mut frame[offset..],
            remote_address,
            local_address,
            crate::IpProtocol::Tcp,
            segment_len,
            64,
        )
        .expect("test IPv6 header should fit");
        frame[offset..offset + segment_len].copy_from_slice(&segment[..segment_len]);
        offset += segment_len;

        stack
            .receive_frame(&frame[..offset], StackInstant::from_nanos(11))
            .expect("SYN-ACK should be handled");

        assert_eq!(
            stack.tcp_connect_state(socket),
            Ok(TcpConnectState::Connected)
        );

        stack
            .drive_tcp(StackInstant::from_nanos(12))
            .expect("handshake ACK should be queued");
        let frame = stack.take_outbound().expect("ACK frame should be queued");
        let ethernet = EthernetFrame::parse(frame.as_slice()).expect("Ethernet frame should parse");
        let ipv6 = Ipv6Packet::parse(ethernet.payload).expect("IPv6 packet should parse");
        let ack = TcpPacket::parse(ipv6.payload).expect("TCP segment should parse");
        assert!(ack.flags.contains(TcpFlags::ACK));
        assert!(!ack.flags.contains(TcpFlags::SYN));
        assert_eq!(ack.acknowledgement, 5001);
        assert!(crate::tcp_checksum_valid(
            IpAddress::Ipv6(ipv6.source),
            IpAddress::Ipv6(ipv6.destination),
            ipv6.payload
        ));
    }
}
