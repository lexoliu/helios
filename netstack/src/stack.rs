extern crate alloc;

use alloc::boxed::Box;
use core::fmt;
use core::mem::MaybeUninit;

use arrayvec::ArrayVec;
use bytes::Bytes;
use heapless::Deque;
use heapless::binary_heap::{BinaryHeap, Min};

use crate::{
    ArpOperation, ArpPacket, BbrV3, CongestionControl, DEFAULT_POLL_BUDGET, DhcpDnsServers,
    EthernetAddress, EthernetFrame, EthernetProtocol, Icmpv6Packet, IpAddress, IpCidr, Ipv4Address,
    Ipv4Cidr, Ipv4Packet, Ipv6Address, Ipv6Cidr, Ipv6Packet, PacketBuffer, StackError, TcpEndpoint,
    TcpFlags, TcpHeader, TcpHeaderOptions, TcpPacket, TcpSocket, TcpTransmitSegment, UdpPacket,
};

pub const MAX_ROUTES: usize = 32;
pub const MAX_NEIGHBORS: usize = 128;
pub const MAX_OUTBOUND_FRAMES: usize = 32;
pub const MAX_STACK_EVENTS: usize = 64;
pub const MAX_UDP_RX: usize = 64;
pub const MAX_TCP_ACCEPT: usize = 64;
pub const MAX_TCP_SOCKETS: usize = 256;
const TCP_SOCKET_SLAB_CHUNK_SLOTS: usize = 1;
const TCP_SOCKET_SLAB_CHUNKS: usize = MAX_TCP_SOCKETS / TCP_SOCKET_SLAB_CHUNK_SLOTS;
const TCP_ENDPOINT_INDEX_SLOTS: usize = MAX_TCP_SOCKETS * 2;
const TCP_LISTENER_INDEX_SLOTS: usize = MAX_TCP_SOCKETS * 2;
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
}

impl StackConfig {
    pub const fn new(mac: EthernetAddress, mtu: usize) -> Self {
        Self {
            mac,
            mtu,
            rx_budget: DEFAULT_POLL_BUDGET,
        }
    }

    pub const fn with_rx_budget(mut self, rx_budget: usize) -> Self {
        self.rx_budget = rx_budget;
        self
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
pub struct DnsQueryId(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcmpEchoKey {
    pub destination: IpAddress,
    pub identifier: u16,
    pub sequence: u16,
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
    pub bytes: Bytes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcpAccept {
    pub socket: SocketId,
    pub remote: TcpEndpoint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpConnectState {
    Pending,
    Connected,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TcpReadState {
    Pending,
    Data(Bytes),
    Eof,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StackEvent {
    DhcpConfigured(DhcpLease),
    NeighborUpdated(NeighborEntry),
    UdpDatagram { socket: SocketId },
    TcpConnected { socket: SocketId },
    TcpReadable { socket: SocketId },
    TcpClosed { socket: SocketId },
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

struct TcpSocketSlab<C>
where
    C: CongestionControl,
{
    slots: [Option<Box<[MaybeUninit<TcpSocket<C>>]>>; TCP_SOCKET_SLAB_CHUNKS],
    occupied: [bool; MAX_TCP_SOCKETS],
    free: [usize; MAX_TCP_SOCKETS],
    free_len: usize,
    active: [usize; MAX_TCP_SOCKETS],
    active_positions: [usize; MAX_TCP_SOCKETS],
    active_len: usize,
    endpoint_index: TcpEndpointIndex,
    listener_index: TcpListenerIndex,
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

trait TcpIndexKey: Copy + Eq {
    fn hash(self) -> usize;
}

type TcpEndpointIndex = TcpSocketIndex<TcpEndpointKey, TCP_ENDPOINT_INDEX_SLOTS>;
type TcpListenerIndex = TcpSocketIndex<TcpEndpoint, TCP_LISTENER_INDEX_SLOTS>;

impl<C> TcpSocketSlab<C>
where
    C: CongestionControl,
{
    fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| None),
            occupied: [false; MAX_TCP_SOCKETS],
            free: core::array::from_fn(|index| MAX_TCP_SOCKETS - 1 - index),
            free_len: MAX_TCP_SOCKETS,
            active: [0; MAX_TCP_SOCKETS],
            active_positions: [0; MAX_TCP_SOCKETS],
            active_len: 0,
            endpoint_index: TcpEndpointIndex::new(),
            listener_index: TcpListenerIndex::new(),
        }
    }

    const fn active_len(&self) -> usize {
        self.active_len
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
        let endpoint_key = socket
            .local_endpoint()
            .zip(socket.remote_endpoint())
            .map(|(local, remote)| TcpEndpointKey::new(local, remote));
        let listener_key = socket
            .local_endpoint()
            .filter(|_| socket.remote_endpoint().is_none());
        self.slot_storage_mut(index).write(socket);
        self.occupied[index] = true;
        if let Some(key) = endpoint_key {
            self.endpoint_index.insert(key, index);
        }
        if let Some(key) = listener_key {
            self.listener_index.insert(key, index);
        }
        self.insert_active(index);
        socket_id(index)
    }

    fn remove(&mut self, index: usize) -> Option<TcpSocket<C>> {
        if !self.occupied.get(index).copied().unwrap_or(false) {
            return None;
        }
        let socket = unsafe { self.slot_storage_mut(index).assume_init_read() };
        self.occupied[index] = false;
        if let Some((local, remote)) = socket.local_endpoint().zip(socket.remote_endpoint()) {
            self.endpoint_index
                .remove(TcpEndpointKey::new(local, remote));
        } else if let Some(local) = socket.local_endpoint() {
            self.listener_index.remove(local);
        }
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

    fn find_listener(&self, local: TcpEndpoint) -> Option<usize> {
        self.listener_index
            .lookup(local)
            .or_else(|| self.listener_index.lookup(wildcard_endpoint(local)))
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
        if self.slots[chunk].is_none() {
            self.slots[chunk] = Some(Box::<[TcpSocket<C>]>::new_uninit_slice(
                TCP_SOCKET_SLAB_CHUNK_SLOTS,
            ));
        }
    }

    fn chunk_ref(&self, chunk: usize) -> &[MaybeUninit<TcpSocket<C>>] {
        self.slots
            .get(chunk)
            .and_then(Option::as_deref)
            .expect("TCP socket chunk is not materialized")
    }

    fn chunk_mut(&mut self, chunk: usize) -> &mut [MaybeUninit<TcpSocket<C>>] {
        self.slots
            .get_mut(chunk)
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
            slots: core::array::from_fn(|chunk| {
                self.slots[chunk]
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
        let mut index = (removed_index + 1) % TCP_ENDPOINT_INDEX_SLOTS;
        while let Some(entry) = self.entries[index].take() {
            self.insert(entry.key, entry.socket_index);
            index = (index + 1) % TCP_ENDPOINT_INDEX_SLOTS;
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
    ipv4_addresses: ArrayVec<Ipv4Cidr, 8>,
    ipv6_addresses: ArrayVec<Ipv6Cidr, 16>,
    neighbors: ArrayVec<NeighborEntry, MAX_NEIGHBORS>,
    outbound: OutboundFrameQueue,
    events: Deque<StackEvent, MAX_STACK_EVENTS>,
    udp_rx: Deque<UdpReceive, MAX_UDP_RX>,
    tcp_accept: Deque<TcpAccept, MAX_TCP_ACCEPT>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TcpAckReplaceKey {
    local: TcpEndpoint,
    remote: TcpEndpoint,
    sequence: u32,
    acknowledgement: u32,
}

impl TcpAckReplaceKey {
    fn can_replace(self, replacement: Self) -> bool {
        self.local == replacement.local
            && self.remote == replacement.remote
            && self.sequence == replacement.sequence
            && crate::tcp::sequence_leq(self.acknowledgement, replacement.acknowledgement)
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
        submit: impl FnOnce(&[&[u8]]) -> Result<Option<usize>, E>,
    ) -> Result<OutboundBatchStatus, E> {
        if limit == 0 || self.ready.is_empty() {
            return Ok(OutboundBatchStatus::Empty);
        }

        let mut frames = ArrayVec::<&[u8], MAX_OUTBOUND_FRAMES>::new();
        for slot in self.ready.iter().copied().take(limit) {
            frames
                .try_push(self.frames[slot].as_slice())
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
            ipv4_addresses: ArrayVec::new(),
            ipv6_addresses: ArrayVec::new(),
            neighbors: ArrayVec::new(),
            outbound: OutboundFrameQueue::new(),
            events: Deque::new(),
            udp_rx: Deque::new(),
            tcp_accept: Deque::new(),
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

    pub fn neighbors(&self) -> impl Iterator<Item = NeighborEntry> + '_ {
        self.neighbors.iter().copied()
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
        }
    }

    pub fn clear_ipv4_addresses(&mut self) {
        self.ipv4_addresses.clear();
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

    pub fn primary_ipv6_address(&self) -> Option<Ipv6Cidr> {
        self.ipv6_addresses.first().copied()
    }

    pub fn ipv6_addresses(&self) -> impl Iterator<Item = Ipv6Cidr> + '_ {
        self.ipv6_addresses.iter().copied()
    }

    pub fn receive_frame(&mut self, frame: &[u8], now: StackInstant) -> Result<(), StackError> {
        self.receive_frame_with_backpressure(frame, now).map(|_| ())
    }

    pub fn receive_frame_with_backpressure(
        &mut self,
        frame: &[u8],
        now: StackInstant,
    ) -> Result<bool, StackError> {
        let frame = EthernetFrame::parse(frame).ok_or(StackError::MalformedPacket)?;
        match frame.protocol {
            EthernetProtocol::Ipv4 => self.receive_ipv4(frame.payload, now),
            EthernetProtocol::Ipv6 => self.receive_ipv6(frame.source, frame.payload, now),
            EthernetProtocol::Arp => self.receive_arp(frame.source, frame.payload, now),
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
        submit: impl FnOnce(&[&[u8]]) -> Result<Option<usize>, E>,
    ) -> Result<OutboundBatchStatus, E> {
        self.outbound.try_submit_slices(limit, submit)
    }

    fn queue_outbound_frame<R>(
        &mut self,
        encode: impl FnOnce(&mut PacketBuffer) -> Result<R, StackError>,
    ) -> Result<R, StackError> {
        let slot = self.outbound.reserve()?;
        match encode(self.outbound.frame_mut(slot)) {
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
        match encode(self.outbound.frame_mut(slot)) {
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

    pub fn take_udp(&mut self, local_port: u16) -> Option<UdpReceive> {
        let len = self.udp_rx.len();
        for _ in 0..len {
            let datagram = self
                .udp_rx
                .pop_front()
                .expect("UDP receive queue length changed while rotating");
            if datagram.destination_port == local_port {
                return Some(datagram);
            }
            self.udp_rx
                .push_back(datagram)
                .unwrap_or_else(|_| panic!("UDP receive queue lost capacity while rotating"));
        }
        None
    }

    pub fn open_tcp_connect(
        &mut self,
        local: TcpEndpoint,
        remote: TcpEndpoint,
        initial_sequence: u32,
    ) -> SocketId {
        let socket = TcpSocket::connect(local, remote, initial_sequence, self.congestion.clone());
        self.insert_tcp(socket)
    }

    pub fn open_tcp_listen(&mut self, local: TcpEndpoint) -> SocketId {
        let socket = TcpSocket::listen(local, self.congestion.clone());
        self.insert_tcp(socket)
    }

    pub fn tcp_connect_state(&self, socket: SocketId) -> Result<TcpConnectState, StackError> {
        let socket = self.tcp_socket(socket)?;
        Ok(match socket.state() {
            crate::TcpState::Established => TcpConnectState::Connected,
            crate::TcpState::Closed | crate::TcpState::TimeWait => TcpConnectState::Closed,
            _ => TcpConnectState::Pending,
        })
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
        let mut pending_ack =
            ArrayVec::<(usize, TcpEndpoint, TcpEndpoint, TcpHeader, TcpHeaderOptions), 16>::new();
        let mut pending_retransmit = ArrayVec::<(usize, TcpTransmitSegment), 16>::new();
        let mut pending_data = ArrayVec::<(usize, TcpTransmitSegment), 16>::new();
        for active_slot in 0..self.tcp.active_len() {
            let index = self.tcp.active_index(active_slot);
            let deadline = {
                let Some(socket) = self.tcp.get_mut(index) else {
                    panic!("TCP active socket index referenced a missing socket");
                };
                socket.expire_timers(now.nanos());
                if let (Some(local), Some(remote)) =
                    (socket.local_endpoint(), socket.remote_endpoint())
                {
                    let retransmitting =
                        if let Some(segment) = socket.pending_retransmission(now.nanos()) {
                            pending_retransmit
                                .try_push((index, segment))
                                .unwrap_or_else(|_| panic!("TCP retransmit burst overflowed"));
                            true
                        } else {
                            false
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
                        if let Some(segment) = socket.take_transmit_segment(now.nanos()) {
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
                            ))
                            .unwrap_or_else(|_| panic!("TCP ACK transmit burst overflowed"));
                    }
                }
                socket.next_deadline_nanos()
            };
            self.schedule_tcp_timer_deadline(index, deadline);
        }

        for (index, local, remote, header, options) in pending_syn {
            if self.queue_tcp(local, remote, header, options, &[], index as u16, now)? {
                let socket = self
                    .tcp
                    .get_mut(index)
                    .expect("TCP socket disappeared while queuing SYN");
                socket.mark_syn_queued(now.nanos());
                self.schedule_tcp_timer(index);
            }
        }
        for (index, local, remote, header, options) in pending_syn_ack {
            if self.queue_tcp(local, remote, header, options, &[], index as u16, now)? {
                let socket = self
                    .tcp
                    .get_mut(index)
                    .expect("TCP socket disappeared while queuing SYN-ACK");
                socket.mark_syn_ack_queued(now.nanos());
                self.schedule_tcp_timer(index);
            }
        }
        for (index, local, remote, header, options) in pending_ack {
            if self.queue_tcp(local, remote, header, options, &[], index as u16, now)? {
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
            if self.queue_tcp(
                segment.local,
                segment.remote,
                segment.header,
                segment.options,
                &segment.payload,
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
            self.queue_tcp(
                segment.local,
                segment.remote,
                segment.header,
                segment.options,
                &segment.payload,
                identification,
                now,
            )?;
            self.schedule_tcp_timer(index);
        }
        Ok(())
    }

    pub fn take_tcp_accept(&mut self, local_port: u16) -> Option<TcpAccept> {
        let len = self.tcp_accept.len();
        for _ in 0..len {
            let accepted = self
                .tcp_accept
                .pop_front()
                .expect("TCP accept queue length changed while rotating");
            let accepted_local_port = self
                .tcp
                .get(socket_index(accepted.socket))
                .and_then(TcpSocket::local_endpoint)
                .map(|endpoint| endpoint.port);
            if accepted_local_port == Some(local_port) {
                return Some(accepted);
            }
            self.tcp_accept
                .push_back(accepted)
                .unwrap_or_else(|_| panic!("TCP accept queue lost capacity while rotating"));
        }
        None
    }

    pub fn remove_tcp_socket(&mut self, socket: SocketId) -> Result<(), StackError> {
        let index = socket_index(socket);
        self.tcp.remove(index).ok_or(StackError::UnknownSocket)?;
        self.clear_tcp_timer(index);
        self.clear_tcp_receive_backpressure(index);
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

    pub fn send_udp_ipv4(
        &mut self,
        source_port: u16,
        destination: Ipv4Address,
        destination_port: u16,
        payload: &[u8],
        identification: u16,
        now: StackInstant,
    ) -> Result<usize, StackError> {
        let source = self
            .source_ipv4_for(destination)
            .ok_or(StackError::Unroutable)?;
        self.send_udp_ipv4_from(
            source,
            source_port,
            destination,
            destination_port,
            payload,
            identification,
            now,
        )
    }

    pub fn send_udp_ipv4_from(
        &mut self,
        source: Ipv4Address,
        source_port: u16,
        destination: Ipv4Address,
        destination_port: u16,
        payload: &[u8],
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
        } else {
            let Some(destination_mac) = self.neighbor_mac(IpAddress::Ipv4(next_hop)) else {
                self.queue_arp_request(source, next_hop, now)?;
                return Ok(0);
            };
            destination_mac
        };
        self.queue_udp_ipv4(
            source,
            source_port,
            destination,
            destination_port,
            destination_mac,
            payload,
            identification,
        )
    }

    pub fn send_udp_ipv6(
        &mut self,
        source_port: u16,
        destination: Ipv6Address,
        destination_port: u16,
        payload: &[u8],
        now: StackInstant,
    ) -> Result<usize, StackError> {
        let source = self
            .source_ipv6_for(destination)
            .ok_or(StackError::Unroutable)?;
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
            source,
            source_port,
            destination,
            destination_port,
            destination_mac,
            payload,
        )
    }

    fn queue_udp_ipv4(
        &mut self,
        source: Ipv4Address,
        source_port: u16,
        destination: Ipv4Address,
        destination_port: u16,
        destination_mac: EthernetAddress,
        payload: &[u8],
        identification: u16,
    ) -> Result<usize, StackError> {
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
                crate::IpProtocol::Udp,
                UdpPacket::HEADER_LEN + payload.len(),
                identification,
                64,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += UdpPacket::encode(
                &mut storage[offset..],
                IpAddress::Ipv4(source),
                IpAddress::Ipv4(destination),
                source_port,
                destination_port,
                payload,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            Ok(payload.len())
        })
    }

    fn queue_udp_ipv6(
        &mut self,
        source: Ipv6Address,
        source_port: u16,
        destination: Ipv6Address,
        destination_port: u16,
        destination_mac: EthernetAddress,
        payload: &[u8],
    ) -> Result<usize, StackError> {
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
                crate::IpProtocol::Udp,
                UdpPacket::HEADER_LEN + payload.len(),
                64,
            )
            .ok_or(StackError::OutputQueueFull)?;
            offset += UdpPacket::encode(
                &mut storage[offset..],
                IpAddress::Ipv6(source),
                IpAddress::Ipv6(destination),
                source_port,
                destination_port,
                payload,
            )
            .ok_or(StackError::OutputQueueFull)?;
            frame.set_len(offset);
            Ok(payload.len())
        })
    }

    fn queue_tcp(
        &mut self,
        local: TcpEndpoint,
        remote: TcpEndpoint,
        header: TcpHeader,
        options: TcpHeaderOptions,
        payload: &[u8],
        identification: u16,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        match (local.address, remote.address) {
            (IpAddress::Ipv4(source), IpAddress::Ipv4(destination)) => self.queue_tcp_ipv4(
                source,
                destination,
                header,
                options,
                payload,
                identification,
                now,
            ),
            (IpAddress::Ipv6(source), IpAddress::Ipv6(destination)) => {
                self.queue_tcp_ipv6(source, destination, header, options, payload, now)
            }
            _ => panic!("TCP endpoint address families must match"),
        }
    }

    fn queue_tcp_ipv4(
        &mut self,
        source: Ipv4Address,
        destination: Ipv4Address,
        header: TcpHeader,
        options: TcpHeaderOptions,
        payload: &[u8],
        identification: u16,
        now: StackInstant,
    ) -> Result<bool, StackError> {
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

        let local_mac = self.config.mac;
        if is_replaceable_tcp_ack(header, payload) {
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
                |frame| {
                    encode_tcp_ipv4_frame(
                        frame,
                        destination_mac,
                        local_mac,
                        source,
                        destination,
                        header,
                        options,
                        payload,
                        identification,
                    )
                },
            );
        }
        self.queue_outbound_frame(|frame| {
            encode_tcp_ipv4_frame(
                frame,
                destination_mac,
                local_mac,
                source,
                destination,
                header,
                options,
                payload,
                identification,
            )?;
            Ok(true)
        })
    }

    fn queue_tcp_ipv6(
        &mut self,
        source: Ipv6Address,
        destination: Ipv6Address,
        header: TcpHeader,
        options: TcpHeaderOptions,
        payload: &[u8],
        now: StackInstant,
    ) -> Result<bool, StackError> {
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

        let local_mac = self.config.mac;
        if is_replaceable_tcp_ack(header, payload) {
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
                |frame| {
                    encode_tcp_ipv6_frame(
                        frame,
                        destination_mac,
                        local_mac,
                        source,
                        destination,
                        header,
                        options,
                        payload,
                    )
                },
            );
        }
        self.queue_outbound_frame(|frame| {
            encode_tcp_ipv6_frame(
                frame,
                destination_mac,
                local_mac,
                source,
                destination,
                header,
                options,
                payload,
            )?;
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

    fn receive_ipv4(&mut self, bytes: &[u8], now: StackInstant) -> Result<bool, StackError> {
        let packet = Ipv4Packet::parse(bytes).ok_or(StackError::MalformedPacket)?;
        match packet.protocol {
            crate::IpProtocol::Tcp => self.receive_tcp(
                IpAddress::Ipv4(packet.source),
                IpAddress::Ipv4(packet.destination),
                packet.payload,
                now,
            ),
            crate::IpProtocol::Udp => {
                self.receive_udp(
                    IpAddress::Ipv4(packet.source),
                    IpAddress::Ipv4(packet.destination),
                    packet.payload,
                )?;
                Ok(false)
            }
            crate::IpProtocol::Icmp | crate::IpProtocol::Icmpv6 => Ok(false),
        }
    }

    fn receive_ipv6(
        &mut self,
        source_mac: EthernetAddress,
        bytes: &[u8],
        now: StackInstant,
    ) -> Result<bool, StackError> {
        let packet = Ipv6Packet::parse(bytes).ok_or(StackError::MalformedPacket)?;
        if !packet.source.is_unspecified() {
            self.learn_neighbor(NeighborEntry {
                ip: IpAddress::Ipv6(packet.source),
                mac: source_mac,
                state: NeighborState::Reachable,
                updated_at: now,
            });
        }
        match packet.next_header {
            crate::IpProtocol::Tcp => self.receive_tcp(
                IpAddress::Ipv6(packet.source),
                IpAddress::Ipv6(packet.destination),
                packet.payload,
                now,
            ),
            crate::IpProtocol::Udp => {
                self.receive_udp(
                    IpAddress::Ipv6(packet.source),
                    IpAddress::Ipv6(packet.destination),
                    packet.payload,
                )?;
                Ok(false)
            }
            crate::IpProtocol::Icmpv6 => self.receive_icmpv6(source_mac, packet, now),
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

    fn source_ipv6_for(&self, destination: Ipv6Address) -> Option<Ipv6Address> {
        self.ipv6_addresses
            .iter()
            .copied()
            .find(|cidr| cidr.contains(destination))
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

    fn receive_tcp(
        &mut self,
        source: IpAddress,
        destination: IpAddress,
        bytes: &[u8],
        now: StackInstant,
    ) -> Result<bool, StackError> {
        let packet = crate::TcpPacket::parse(bytes).ok_or(StackError::MalformedPacket)?;
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
                let outcome = socket.on_segment(packet, now.nanos());
                (
                    previous_state,
                    socket.state(),
                    outcome,
                    socket.receive_backpressured(),
                )
            };
            self.schedule_tcp_timer(index);
            self.update_tcp_receive_backpressure(index);
            if previous_state == crate::TcpState::SynReceived
                && current_state == crate::TcpState::Established
            {
                self.tcp_accept
                    .push_back(TcpAccept {
                        socket: socket_id(index),
                        remote: remote_endpoint,
                    })
                    .unwrap_or_else(|_| panic!("TCP accept queue is full"));
            }
            if current_state == crate::TcpState::Established {
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
            return Ok(receive_backpressured);
        }
        if packet.flags.contains(crate::TcpFlags::SYN)
            && !packet.flags.contains(crate::TcpFlags::ACK)
        {
            let Some(listener_index) = self.tcp.find_listener(local_endpoint) else {
                return Ok(false);
            };
            let listener = self
                .tcp
                .get(listener_index)
                .expect("TCP listener index referenced a missing socket");
            assert!(
                listener.is_listening_on(destination, packet.destination_port),
                "TCP listener index resolved a non-listening socket"
            );
            let local = TcpEndpoint {
                address: destination,
                port: packet.destination_port,
            };
            let initial_sequence = (now.nanos() as u32)
                .wrapping_add(u32::from(packet.destination_port))
                .wrapping_add(u32::from(packet.source_port));
            let mut child = TcpSocket::accept(
                local,
                remote_endpoint,
                packet.sequence.wrapping_add(1),
                initial_sequence,
                self.congestion.clone(),
            );
            child.record_peer_options(packet);
            self.insert_tcp(child);
        }
        Ok(false)
    }

    fn receive_icmpv6(
        &mut self,
        source_mac: EthernetAddress,
        packet: Ipv6Packet<'_>,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        match Icmpv6Packet::parse(packet.payload).ok_or(StackError::MalformedPacket)? {
            Icmpv6Packet::NeighborSolicitation { target } => {
                if packet.source.is_unspecified() {
                    return Ok(false);
                }
                if self
                    .ipv6_addresses
                    .iter()
                    .any(|address| address.address() == target)
                {
                    self.queue_icmpv6_neighbor_advertisement(
                        target,
                        packet.source,
                        target,
                        source_mac,
                    )?;
                }
            }
            Icmpv6Packet::NeighborAdvertisement { target } => {
                self.learn_neighbor(NeighborEntry {
                    ip: IpAddress::Ipv6(target),
                    mac: source_mac,
                    state: NeighborState::Reachable,
                    updated_at: now,
                });
            }
            Icmpv6Packet::EchoRequest(_) | Icmpv6Packet::EchoReply(_) => {}
        }
        Ok(false)
    }

    fn receive_udp(
        &mut self,
        source: IpAddress,
        destination: IpAddress,
        bytes: &[u8],
    ) -> Result<(), StackError> {
        let packet = UdpPacket::parse(bytes).ok_or(StackError::MalformedPacket)?;
        self.udp_rx
            .push_back(UdpReceive {
                source,
                destination,
                source_port: packet.source_port,
                destination_port: packet.destination_port,
                bytes: Bytes::copy_from_slice(packet.payload),
            })
            .map_err(|_| StackError::OutputQueueFull)?;
        Ok(())
    }

    fn insert_tcp(&mut self, socket: TcpSocket<C>) -> SocketId {
        let id = self.tcp.insert(socket);
        self.schedule_tcp_timer(socket_index(id));
        id
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

    fn remove_tcp_accepts_for(&mut self, socket: SocketId) {
        let len = self.tcp_accept.len();
        for _ in 0..len {
            let accepted = self
                .tcp_accept
                .pop_front()
                .expect("TCP accept queue length changed while pruning");
            if accepted.socket != socket {
                self.tcp_accept
                    .push_back(accepted)
                    .unwrap_or_else(|_| panic!("TCP accept queue lost capacity while pruning"));
            }
        }
    }
}

fn is_replaceable_tcp_ack(header: TcpHeader, payload: &[u8]) -> bool {
    payload.is_empty() && header.flags == TcpFlags::ACK
}

fn encode_tcp_ipv4_frame(
    frame: &mut PacketBuffer,
    destination_mac: EthernetAddress,
    local_mac: EthernetAddress,
    source: Ipv4Address,
    destination: Ipv4Address,
    header: TcpHeader,
    options: TcpHeaderOptions,
    payload: &[u8],
    identification: u16,
) -> Result<(), StackError> {
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
        64,
    )
    .ok_or(StackError::OutputQueueFull)?;
    offset += TcpPacket::encode_with_options(
        &mut storage[offset..],
        IpAddress::Ipv4(source),
        IpAddress::Ipv4(destination),
        header,
        options,
        payload,
    )
    .ok_or(StackError::OutputQueueFull)?;
    frame.set_len(offset);
    Ok(())
}

fn encode_tcp_ipv6_frame(
    frame: &mut PacketBuffer,
    destination_mac: EthernetAddress,
    local_mac: EthernetAddress,
    source: Ipv6Address,
    destination: Ipv6Address,
    header: TcpHeader,
    options: TcpHeaderOptions,
    payload: &[u8],
) -> Result<(), StackError> {
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
        64,
    )
    .ok_or(StackError::OutputQueueFull)?;
    offset += TcpPacket::encode_with_options(
        &mut storage[offset..],
        IpAddress::Ipv6(source),
        IpAddress::Ipv6(destination),
        header,
        options,
        payload,
    )
    .ok_or(StackError::OutputQueueFull)?;
    frame.set_len(offset);
    Ok(())
}

fn socket_id(index: usize) -> SocketId {
    let raw = u32::try_from(index + 1).unwrap_or_else(|_| panic!("socket index exceeds u32"));
    SocketId::new(raw)
}

fn socket_index(socket: SocketId) -> usize {
    usize::try_from(socket.raw() - 1)
        .unwrap_or_else(|_| panic!("socket id does not fit into usize"))
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

fn mix_ip_address(hash: u64, address: IpAddress) -> u64 {
    match address {
        IpAddress::Ipv4(address) => mix_bytes(hash, &address.octets()),
        IpAddress::Ipv6(address) => mix_bytes(hash, &address.octets()),
    }
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

fn ipv6_multicast_mac(address: Ipv6Address) -> EthernetAddress {
    let octets = address.octets();
    [0x33, 0x33, octets[12], octets[13], octets[14], octets[15]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewReno;
    use crate::TcpFlags;

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
        )
        .expect("test TCP segment should fit");
        (segment, len)
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
                assert_eq!(frames[0], b"first".as_slice());
                assert_eq!(frames[1], b"second".as_slice());
                assert_eq!(frames[2], b"third".as_slice());
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
                    queued,
                    TcpHeaderOptions::empty(),
                    &[],
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
                    replacement,
                    TcpHeaderOptions::empty(),
                    &[],
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
                    queued,
                    TcpHeaderOptions::empty(),
                    &[],
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
                    next_sequence,
                    TcpHeaderOptions::empty(),
                    &[],
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
    fn tcp_stack_accepts_replaceable_congestion_control() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new_with_congestion(
            StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES),
            NewReno::new(1460),
        );

        let socket = stack.open_tcp_connect(
            TcpEndpoint {
                address: IpAddress::Ipv4(local),
                port: 49152,
            },
            TcpEndpoint {
                address: IpAddress::Ipv4(peer),
                port: 80,
            },
            7,
        );

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
        stack.open_tcp_listen(TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            port: 8080,
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
                &syn[..syn_len],
                StackInstant::from_nanos(1),
            )
            .expect("SYN should be accepted by listener");
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
        assert_eq!(syn_ack.acknowledgement, 11);
        assert_eq!(syn_ack.options.maximum_segment_size(), Some(1460));
        assert_eq!(
            syn_ack.options.window_scale(),
            Some(crate::tcp::TCP_LOCAL_WINDOW_SCALE)
        );
        assert!(syn_ack.options.sack_permitted());
        assert!(syn_ack.options.timestamp().is_some());

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
                &ack[..ack_len],
                StackInstant::from_nanos(2),
            )
            .expect("final ACK should establish accepted socket");

        let accepted = stack
            .take_tcp_accept(8080)
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
        stack.open_tcp_listen(TcpEndpoint {
            address: IpAddress::Ipv4(local),
            port: 8080,
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
                &syn[..syn_len],
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
    fn tcp_read_reports_eof_after_fin_and_drained_payload() {
        let local = Ipv4Address::new([192, 0, 2, 10]);
        let peer = Ipv4Address::new([192, 0, 2, 20]);
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let socket = stack.open_tcp_connect(
            TcpEndpoint {
                address: IpAddress::Ipv4(local),
                port: 49152,
            },
            TcpEndpoint {
                address: IpAddress::Ipv4(peer),
                port: 80,
            },
            7,
        );
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
                &syn_ack[..syn_ack_len],
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
                &data_fin[..data_fin_len],
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
        let socket = stack.open_tcp_connect(
            TcpEndpoint {
                address: IpAddress::Ipv4(local),
                port: 49152,
            },
            TcpEndpoint {
                address: IpAddress::Ipv4(peer),
                port: 80,
            },
            7,
        );
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
                &syn_ack[..syn_ack_len],
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
                &early_fin[..early_fin_len],
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
                &payload[..payload_len],
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
        let socket = stack.open_tcp_connect(
            TcpEndpoint {
                address: IpAddress::Ipv4(local),
                port: 49152,
            },
            TcpEndpoint {
                address: IpAddress::Ipv4(peer),
                port: 80,
            },
            7,
        );
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
                &syn_ack[..syn_ack_len],
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
                &request[..request_len],
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
        );
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
                &syn_ack[..syn_ack_len],
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
                &request[..request_len],
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
        let socket = stack.open_tcp_connect(
            TcpEndpoint {
                address: IpAddress::Ipv4(local),
                port: 49152,
            },
            TcpEndpoint {
                address: IpAddress::Ipv4(peer),
                port: 80,
            },
            7,
        );
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
                &syn_ack[..syn_ack_len],
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
                    &segment[..segment_len],
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
        let first = stack.open_tcp_listen(TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            port: 8000,
        });
        let second = stack.open_tcp_listen(TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            port: 8001,
        });
        assert_eq!(stack.tcp.active_len(), 2);

        stack
            .remove_tcp_socket(first)
            .expect("allocated socket should be removable");
        assert_eq!(stack.tcp.active_len(), 1);
        assert_eq!(stack.tcp.active_index(0), socket_index(second));

        let reused = stack.open_tcp_listen(TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            port: 8002,
        });

        assert_eq!(stack.tcp.active_len(), 2);
        assert_eq!(stack.tcp.active_index(0), socket_index(second));
        assert_eq!(stack.tcp.active_index(1), socket_index(reused));
        assert_eq!(reused.raw(), first.raw());
        assert_ne!(reused.raw(), second.raw());
    }

    #[test]
    fn tcp_socket_slab_clone_preserves_ids_and_independent_ownership() {
        let mut stack = Stack::new(StackConfig::new(LOCAL_MAC, crate::ETHERNET_FRAME_BYTES));
        let first = stack.open_tcp_listen(TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            port: 8000,
        });
        let second = stack.open_tcp_listen(TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            port: 8001,
        });

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
}
