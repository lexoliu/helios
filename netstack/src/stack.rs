extern crate alloc;

use alloc::vec::Vec;

use arrayvec::ArrayVec;
use bytes::Bytes;
use heapless::Deque;

use crate::{
    ArpOperation, ArpPacket, BbrV3, DEFAULT_POLL_BUDGET, EthernetAddress, EthernetFrame,
    EthernetProtocol, Icmpv6Packet, IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv4Packet,
    Ipv6Address, Ipv6Cidr, Ipv6Packet, PacketBuffer, StackError, TcpEndpoint, TcpHeader, TcpPacket,
    TcpSocket, TcpTransmitSegment, UdpPacket,
};

pub const MAX_ROUTES: usize = 32;
pub const MAX_NEIGHBORS: usize = 128;
pub const MAX_OUTBOUND_FRAMES: usize = 32;
pub const MAX_STACK_EVENTS: usize = 64;
pub const MAX_UDP_RX: usize = 64;
pub const MAX_TCP_ACCEPT: usize = 64;
pub const MAX_TCP_SOCKETS: usize = 256;

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
    pub dns_servers: Vec<Ipv4Address>,
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
    Data(Vec<u8>),
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

#[derive(Clone, Debug)]
struct TcpSocketSlab {
    slots: Vec<Option<TcpSocket<BbrV3>>>,
}

impl TcpSocketSlab {
    fn new() -> Self {
        let mut slots = Vec::with_capacity(MAX_TCP_SOCKETS);
        slots.resize_with(MAX_TCP_SOCKETS, || None);
        Self { slots }
    }

    fn iter(&self) -> impl Iterator<Item = &Option<TcpSocket<BbrV3>>> {
        self.slots.iter()
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut Option<TcpSocket<BbrV3>>> {
        self.slots.iter_mut()
    }

    fn get(&self, index: usize) -> Option<&Option<TcpSocket<BbrV3>>> {
        self.slots.get(index)
    }

    fn get_mut(&mut self, index: usize) -> Option<&mut Option<TcpSocket<BbrV3>>> {
        self.slots.get_mut(index)
    }

    fn insert(&mut self, socket: TcpSocket<BbrV3>) -> SocketId {
        let Some((index, slot)) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.is_none())
        else {
            panic!("TCP socket slab is full");
        };
        *slot = Some(socket);
        socket_id(index)
    }
}

#[derive(Clone, Debug)]
pub struct Stack {
    config: StackConfig,
    routes: RouteTable,
    ipv4_addresses: ArrayVec<Ipv4Cidr, 8>,
    ipv6_addresses: ArrayVec<Ipv6Cidr, 16>,
    neighbors: ArrayVec<NeighborEntry, MAX_NEIGHBORS>,
    outbound: Deque<PacketBuffer, MAX_OUTBOUND_FRAMES>,
    events: Deque<StackEvent, MAX_STACK_EVENTS>,
    udp_rx: Deque<UdpReceive, MAX_UDP_RX>,
    tcp_accept: Deque<TcpAccept, MAX_TCP_ACCEPT>,
    tcp: TcpSocketSlab,
}

impl Stack {
    pub fn new(config: StackConfig) -> Self {
        Self {
            config,
            routes: RouteTable::new(),
            ipv4_addresses: ArrayVec::new(),
            ipv6_addresses: ArrayVec::new(),
            neighbors: ArrayVec::new(),
            outbound: Deque::new(),
            events: Deque::new(),
            udp_rx: Deque::new(),
            tcp_accept: Deque::new(),
            tcp: TcpSocketSlab::new(),
        }
    }

    pub const fn config(&self) -> StackConfig {
        self.config
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
        let frame = EthernetFrame::parse(frame).ok_or(StackError::MalformedPacket)?;
        match frame.protocol {
            EthernetProtocol::Ipv4 => self.receive_ipv4(frame.payload, now),
            EthernetProtocol::Ipv6 => self.receive_ipv6(frame.source, frame.payload, now),
            EthernetProtocol::Arp => self.receive_arp(frame.source, frame.payload, now),
        }
    }

    pub fn take_outbound(&mut self) -> Option<PacketBuffer> {
        self.outbound.pop_front()
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
        let socket = TcpSocket::connect(local, remote, initial_sequence, BbrV3::new(1460));
        self.insert_tcp(socket)
    }

    pub fn open_tcp_listen(&mut self, local: TcpEndpoint) -> SocketId {
        let socket = TcpSocket::listen(local, BbrV3::new(1460));
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

    pub fn drive_tcp(&mut self, now: StackInstant) -> Result<(), StackError> {
        let mut pending_syn = ArrayVec::<(usize, TcpEndpoint, TcpEndpoint, TcpHeader), 16>::new();
        let mut pending_syn_ack =
            ArrayVec::<(usize, TcpEndpoint, TcpEndpoint, TcpHeader), 16>::new();
        let mut pending_ack = ArrayVec::<(usize, TcpEndpoint, TcpEndpoint, TcpHeader), 16>::new();
        let mut pending_retransmit = ArrayVec::<(usize, TcpTransmitSegment), 16>::new();
        let mut pending_data = ArrayVec::<TcpTransmitSegment, 16>::new();
        for (index, socket) in self.tcp.iter().enumerate() {
            let Some(socket) = socket else {
                continue;
            };
            let (Some(local), Some(remote)) = (socket.local_endpoint(), socket.remote_endpoint())
            else {
                continue;
            };
            if let Some(segment) = socket.pending_retransmission(now.nanos()) {
                pending_retransmit
                    .try_push((index, segment))
                    .unwrap_or_else(|_| panic!("TCP retransmit burst overflowed"));
                continue;
            }
            if let Some(header) = socket.pending_syn() {
                pending_syn
                    .try_push((index, local, remote, header))
                    .unwrap_or_else(|_| panic!("TCP control transmit burst overflowed"));
            }
            if let Some(header) = socket.pending_syn_ack() {
                pending_syn_ack
                    .try_push((index, local, remote, header))
                    .unwrap_or_else(|_| panic!("TCP SYN-ACK transmit burst overflowed"));
            }
            if let Some(header) = socket.pending_ack() {
                pending_ack
                    .try_push((index, local, remote, header))
                    .unwrap_or_else(|_| panic!("TCP ACK transmit burst overflowed"));
            }
        }

        for socket in self.tcp.iter_mut().flatten() {
            if socket.pending_retransmission(now.nanos()).is_some() {
                continue;
            }
            if let Some(segment) = socket.take_transmit_segment(now.nanos()) {
                pending_data
                    .try_push(segment)
                    .unwrap_or_else(|_| panic!("TCP data transmit burst overflowed"));
            }
        }

        for (index, local, remote, header) in pending_syn {
            if self.queue_tcp(local, remote, header, &[], index as u16, now)? {
                let socket = self
                    .tcp
                    .get_mut(index)
                    .and_then(Option::as_mut)
                    .expect("TCP socket disappeared while queuing SYN");
                socket.mark_syn_queued(now.nanos());
            }
        }
        for (index, local, remote, header) in pending_syn_ack {
            if self.queue_tcp(local, remote, header, &[], index as u16, now)? {
                let socket = self
                    .tcp
                    .get_mut(index)
                    .and_then(Option::as_mut)
                    .expect("TCP socket disappeared while queuing SYN-ACK");
                socket.mark_syn_ack_queued(now.nanos());
            }
        }
        for (index, local, remote, header) in pending_ack {
            if self.queue_tcp(local, remote, header, &[], index as u16, now)? {
                let socket = self
                    .tcp
                    .get_mut(index)
                    .and_then(Option::as_mut)
                    .expect("TCP socket disappeared while queuing ACK");
                socket.mark_ack_queued();
            }
        }
        for (index, segment) in pending_retransmit {
            let sequence = segment.header.sequence;
            let identification = segment.sequence_len as u16;
            if self.queue_tcp(
                segment.local,
                segment.remote,
                segment.header,
                &segment.payload,
                identification,
                now,
            )? {
                let socket = self
                    .tcp
                    .get_mut(index)
                    .and_then(Option::as_mut)
                    .expect("TCP socket disappeared while queuing retransmission");
                socket.mark_retransmission_queued(sequence, now.nanos());
            }
        }
        for segment in pending_data {
            let identification = segment.sequence_len as u16;
            self.queue_tcp(
                segment.local,
                segment.remote,
                segment.header,
                &segment.payload,
                identification,
                now,
            )?;
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
                .and_then(Option::as_ref)
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
        let slot = self
            .tcp
            .get_mut(socket_index(socket))
            .ok_or(StackError::UnknownSocket)?;
        if slot.take().is_none() {
            return Err(StackError::UnknownSocket);
        }
        self.remove_tcp_accepts_for(socket);
        Ok(())
    }

    pub fn tcp_send(&mut self, socket: SocketId, bytes: &[u8]) -> Result<usize, StackError> {
        Ok(self.tcp_socket_mut(socket)?.queue_send(bytes))
    }

    pub fn tcp_read(
        &mut self,
        socket: SocketId,
        max_bytes: usize,
    ) -> Result<TcpReadState, StackError> {
        let socket = self.tcp_socket_mut(socket)?;
        Ok(match socket.receive(max_bytes) {
            Some(bytes) => TcpReadState::Data(bytes),
            None if socket.state() == crate::TcpState::CloseWait => TcpReadState::Eof,
            None => TcpReadState::Pending,
        })
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
        let mut frame = PacketBuffer::new();
        let storage = frame.storage_mut();
        let mut offset = EthernetFrame::encode_header(
            storage,
            destination_mac,
            self.config.mac,
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
        self.outbound
            .push_back(frame)
            .map_err(|_| StackError::OutputQueueFull)?;
        Ok(payload.len())
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
        let mut frame = PacketBuffer::new();
        let storage = frame.storage_mut();
        let mut offset = EthernetFrame::encode_header(
            storage,
            destination_mac,
            self.config.mac,
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
        self.outbound
            .push_back(frame)
            .map_err(|_| StackError::OutputQueueFull)?;
        Ok(payload.len())
    }

    fn queue_tcp(
        &mut self,
        local: TcpEndpoint,
        remote: TcpEndpoint,
        header: TcpHeader,
        payload: &[u8],
        identification: u16,
        now: StackInstant,
    ) -> Result<bool, StackError> {
        match (local.address, remote.address) {
            (IpAddress::Ipv4(source), IpAddress::Ipv4(destination)) => {
                self.queue_tcp_ipv4(source, destination, header, payload, identification, now)
            }
            (IpAddress::Ipv6(source), IpAddress::Ipv6(destination)) => {
                self.queue_tcp_ipv6(source, destination, header, payload, now)
            }
            _ => panic!("TCP endpoint address families must match"),
        }
    }

    fn queue_tcp_ipv4(
        &mut self,
        source: Ipv4Address,
        destination: Ipv4Address,
        header: TcpHeader,
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

        let mut frame = PacketBuffer::new();
        let storage = frame.storage_mut();
        let mut offset = EthernetFrame::encode_header(
            storage,
            destination_mac,
            self.config.mac,
            EthernetProtocol::Ipv4,
        )
        .ok_or(StackError::OutputQueueFull)?;
        offset += Ipv4Packet::encode_header(
            &mut storage[offset..],
            source,
            destination,
            crate::IpProtocol::Tcp,
            TcpPacket::MIN_HEADER_LEN + payload.len(),
            identification,
            64,
        )
        .ok_or(StackError::OutputQueueFull)?;
        offset += TcpPacket::encode(
            &mut storage[offset..],
            IpAddress::Ipv4(source),
            IpAddress::Ipv4(destination),
            header,
            payload,
        )
        .ok_or(StackError::OutputQueueFull)?;
        frame.set_len(offset);
        self.outbound
            .push_back(frame)
            .map_err(|_| StackError::OutputQueueFull)?;
        Ok(true)
    }

    fn queue_tcp_ipv6(
        &mut self,
        source: Ipv6Address,
        destination: Ipv6Address,
        header: TcpHeader,
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

        let mut frame = PacketBuffer::new();
        let storage = frame.storage_mut();
        let mut offset = EthernetFrame::encode_header(
            storage,
            destination_mac,
            self.config.mac,
            EthernetProtocol::Ipv6,
        )
        .ok_or(StackError::OutputQueueFull)?;
        offset += Ipv6Packet::encode_header(
            &mut storage[offset..],
            source,
            destination,
            crate::IpProtocol::Tcp,
            TcpPacket::MIN_HEADER_LEN + payload.len(),
            64,
        )
        .ok_or(StackError::OutputQueueFull)?;
        offset += TcpPacket::encode(
            &mut storage[offset..],
            IpAddress::Ipv6(source),
            IpAddress::Ipv6(destination),
            header,
            payload,
        )
        .ok_or(StackError::OutputQueueFull)?;
        frame.set_len(offset);
        self.outbound
            .push_back(frame)
            .map_err(|_| StackError::OutputQueueFull)?;
        Ok(true)
    }

    fn receive_arp(
        &mut self,
        source_mac: EthernetAddress,
        bytes: &[u8],
        now: StackInstant,
    ) -> Result<(), StackError> {
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
        Ok(())
    }

    fn receive_ipv4(&mut self, bytes: &[u8], now: StackInstant) -> Result<(), StackError> {
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
                Ok(())
            }
            crate::IpProtocol::Icmp | crate::IpProtocol::Icmpv6 => Ok(()),
        }
    }

    fn receive_ipv6(
        &mut self,
        source_mac: EthernetAddress,
        bytes: &[u8],
        now: StackInstant,
    ) -> Result<(), StackError> {
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
                Ok(())
            }
            crate::IpProtocol::Icmpv6 => self.receive_icmpv6(source_mac, packet, now),
            crate::IpProtocol::Icmp => Ok(()),
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
        let mut frame = PacketBuffer::new();
        let storage = frame.storage_mut();
        let mut offset = EthernetFrame::encode_header(
            storage,
            destination_mac,
            self.config.mac,
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
            self.config.mac,
        )
        .ok_or(StackError::OutputQueueFull)?;
        frame.set_len(offset);
        self.outbound
            .push_back(frame)
            .map_err(|_| StackError::OutputQueueFull)?;
        Ok(())
    }

    fn queue_icmpv6_neighbor_advertisement(
        &mut self,
        source: Ipv6Address,
        destination: Ipv6Address,
        target: Ipv6Address,
        destination_mac: EthernetAddress,
    ) -> Result<(), StackError> {
        let mut frame = PacketBuffer::new();
        let storage = frame.storage_mut();
        let mut offset = EthernetFrame::encode_header(
            storage,
            destination_mac,
            self.config.mac,
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
            self.config.mac,
        )
        .ok_or(StackError::OutputQueueFull)?;
        frame.set_len(offset);
        self.outbound
            .push_back(frame)
            .map_err(|_| StackError::OutputQueueFull)?;
        Ok(())
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
        let mut frame = PacketBuffer::new();
        let storage = frame.storage_mut();
        let mut offset = EthernetFrame::encode_header(
            storage,
            destination_mac,
            self.config.mac,
            EthernetProtocol::Arp,
        )
        .ok_or(StackError::OutputQueueFull)?;
        offset += packet
            .encode(&mut storage[offset..])
            .ok_or(StackError::OutputQueueFull)?;
        frame.set_len(offset);
        self.outbound
            .push_back(frame)
            .map_err(|_| StackError::OutputQueueFull)?;
        Ok(())
    }

    fn receive_tcp(
        &mut self,
        source: IpAddress,
        destination: IpAddress,
        bytes: &[u8],
        now: StackInstant,
    ) -> Result<(), StackError> {
        let packet = crate::TcpPacket::parse(bytes).ok_or(StackError::MalformedPacket)?;
        for (index, socket) in self.tcp.iter_mut().enumerate() {
            let Some(socket) = socket else {
                continue;
            };
            let local = socket.local_endpoint();
            let remote = socket.remote_endpoint();
            if local
                == Some(TcpEndpoint {
                    address: destination,
                    port: packet.destination_port,
                })
                && remote
                    == Some(TcpEndpoint {
                        address: source,
                        port: packet.source_port,
                    })
            {
                let previous_state = socket.state();
                socket.on_segment(packet, now.nanos());
                if previous_state == crate::TcpState::SynReceived
                    && socket.state() == crate::TcpState::Established
                {
                    self.tcp_accept
                        .push_back(TcpAccept {
                            socket: socket_id(index),
                            remote: TcpEndpoint {
                                address: source,
                                port: packet.source_port,
                            },
                        })
                        .unwrap_or_else(|_| panic!("TCP accept queue is full"));
                }
                if socket.state() == crate::TcpState::Established {
                    Self::push_event_into(
                        &mut self.events,
                        StackEvent::TcpReadable {
                            socket: socket_id(index),
                        },
                    );
                }
                return Ok(());
            }
        }
        if packet.flags.contains(crate::TcpFlags::SYN)
            && !packet.flags.contains(crate::TcpFlags::ACK)
        {
            let Some(_) = self
                .tcp
                .iter()
                .flatten()
                .find(|socket| socket.is_listening_on(destination, packet.destination_port))
            else {
                return Ok(());
            };
            let local = TcpEndpoint {
                address: destination,
                port: packet.destination_port,
            };
            let remote = TcpEndpoint {
                address: source,
                port: packet.source_port,
            };
            let initial_sequence = (now.nanos() as u32)
                .wrapping_add(u32::from(packet.destination_port))
                .wrapping_add(u32::from(packet.source_port));
            let child = TcpSocket::accept(
                local,
                remote,
                packet.sequence.wrapping_add(1),
                initial_sequence,
                BbrV3::new(1460),
            );
            self.insert_tcp(child);
        }
        Ok(())
    }

    fn receive_icmpv6(
        &mut self,
        source_mac: EthernetAddress,
        packet: Ipv6Packet<'_>,
        now: StackInstant,
    ) -> Result<(), StackError> {
        match Icmpv6Packet::parse(packet.payload).ok_or(StackError::MalformedPacket)? {
            Icmpv6Packet::NeighborSolicitation { target } => {
                if packet.source.is_unspecified() {
                    return Ok(());
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
        Ok(())
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

    fn insert_tcp(&mut self, socket: TcpSocket<BbrV3>) -> SocketId {
        self.tcp.insert(socket)
    }

    fn tcp_socket(&self, id: SocketId) -> Result<&TcpSocket<BbrV3>, StackError> {
        self.tcp
            .get(socket_index(id))
            .and_then(Option::as_ref)
            .ok_or(StackError::UnknownSocket)
    }

    fn tcp_socket_mut(&mut self, id: SocketId) -> Result<&mut TcpSocket<BbrV3>, StackError> {
        self.tcp
            .get_mut(socket_index(id))
            .and_then(Option::as_mut)
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

fn socket_id(index: usize) -> SocketId {
    let raw = u32::try_from(index + 1).unwrap_or_else(|_| panic!("socket index exceeds u32"));
    SocketId::new(raw)
}

fn socket_index(socket: SocketId) -> usize {
    usize::try_from(socket.raw() - 1)
        .unwrap_or_else(|_| panic!("socket id does not fit into usize"))
}

fn ipv6_multicast_mac(address: Ipv6Address) -> EthernetAddress {
    let octets = address.octets();
    [0x33, 0x33, octets[12], octets[13], octets[14], octets[15]]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TcpFlags;

    const LOCAL_MAC: EthernetAddress = [0x02, 0, 0, 0, 0, 1];
    const PEER_MAC: EthernetAddress = [0x02, 0, 0, 0, 0, 2];

    fn tcp_segment(
        source: Ipv4Address,
        destination: Ipv4Address,
        header: TcpHeader,
    ) -> ([u8; 64], usize) {
        tcp_segment_with_payload(source, destination, header, &[])
    }

    fn tcp_segment_with_payload(
        source: Ipv4Address,
        destination: Ipv4Address,
        header: TcpHeader,
        payload: &[u8],
    ) -> ([u8; 64], usize) {
        let mut segment = [0u8; 64];
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
            stack.tcp_read(socket, 8).unwrap(),
            TcpReadState::Data(b"ok".to_vec())
        );
        assert_eq!(stack.tcp_read(socket, 8).unwrap(), TcpReadState::Eof);
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

        stack
            .remove_tcp_socket(first)
            .expect("allocated socket should be removable");
        let reused = stack.open_tcp_listen(TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
            port: 8002,
        });

        assert_eq!(reused.raw(), first.raw());
        assert_ne!(reused.raw(), second.raw());
    }
}
