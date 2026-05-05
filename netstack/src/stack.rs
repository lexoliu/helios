extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use arrayvec::ArrayVec;

use crate::{
    ArpOperation, ArpPacket, BbrV3, DEFAULT_POLL_BUDGET, EthernetAddress, EthernetFrame,
    EthernetProtocol, IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv4Packet, Ipv6Cidr, Ipv6Packet,
    PacketBuffer, StackError, TcpEndpoint, TcpHeader, TcpPacket, TcpSocket, TcpTransmitSegment,
    UdpPacket,
};

pub const MAX_ROUTES: usize = 32;
pub const MAX_NEIGHBORS: usize = 128;

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
    pub bytes: Vec<u8>,
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
pub struct Stack {
    config: StackConfig,
    routes: RouteTable,
    ipv4_addresses: ArrayVec<Ipv4Cidr, 8>,
    ipv6_addresses: ArrayVec<Ipv6Cidr, 16>,
    neighbors: ArrayVec<NeighborEntry, MAX_NEIGHBORS>,
    outbound: VecDeque<PacketBuffer>,
    events: VecDeque<StackEvent>,
    udp_rx: VecDeque<UdpReceive>,
    tcp: Vec<Option<TcpSocket<BbrV3>>>,
}

impl Stack {
    pub fn new(config: StackConfig) -> Self {
        Self {
            config,
            routes: RouteTable::new(),
            ipv4_addresses: ArrayVec::new(),
            ipv6_addresses: ArrayVec::new(),
            neighbors: ArrayVec::new(),
            outbound: VecDeque::new(),
            events: VecDeque::new(),
            udp_rx: VecDeque::new(),
            tcp: Vec::new(),
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
        self.events.push_back(StackEvent::NeighborUpdated(entry));
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
            EthernetProtocol::Ipv6 => self.receive_ipv6(frame.payload, now),
            EthernetProtocol::Arp => self.receive_arp(frame.source, frame.payload, now),
        }
    }

    pub fn take_outbound(&mut self) -> Option<PacketBuffer> {
        self.outbound.pop_front()
    }

    pub fn take_event(&mut self) -> Option<StackEvent> {
        self.events.pop_front()
    }

    pub fn take_udp(&mut self, local_port: u16) -> Option<UdpReceive> {
        let index = self
            .udp_rx
            .iter()
            .position(|datagram| datagram.destination_port == local_port)?;
        self.udp_rx.remove(index)
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
        let mut pending_ack = ArrayVec::<(usize, TcpEndpoint, TcpEndpoint, TcpHeader), 16>::new();
        let mut pending_data = ArrayVec::<TcpTransmitSegment, 16>::new();
        for (index, socket) in self.tcp.iter().enumerate() {
            let Some(socket) = socket else {
                continue;
            };
            let (Some(local), Some(remote)) = (socket.local_endpoint(), socket.remote_endpoint())
            else {
                continue;
            };
            if let Some(header) = socket.pending_syn() {
                pending_syn
                    .try_push((index, local, remote, header))
                    .unwrap_or_else(|_| panic!("TCP control transmit burst overflowed"));
            }
            if let Some(header) = socket.pending_ack() {
                pending_ack
                    .try_push((index, local, remote, header))
                    .unwrap_or_else(|_| panic!("TCP ACK transmit burst overflowed"));
            }
        }

        for socket in self.tcp.iter_mut().flatten() {
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
        self.outbound.push_back(frame);
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
            (IpAddress::Ipv6(_), IpAddress::Ipv6(_)) => Ok(false),
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
        self.outbound.push_back(frame);
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

    fn receive_ipv6(&mut self, bytes: &[u8], now: StackInstant) -> Result<(), StackError> {
        let packet = Ipv6Packet::parse(bytes).ok_or(StackError::MalformedPacket)?;
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
            crate::IpProtocol::Icmp | crate::IpProtocol::Icmpv6 => Ok(()),
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
        self.outbound.push_back(frame);
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
                socket.on_segment(packet, now.nanos());
                if socket.state() == crate::TcpState::Established {
                    self.events.push_back(StackEvent::TcpReadable {
                        socket: socket_id(index),
                    });
                }
                return Ok(());
            }
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
        self.udp_rx.push_back(UdpReceive {
            source,
            destination,
            source_port: packet.source_port,
            destination_port: packet.destination_port,
            bytes: packet.payload.to_vec(),
        });
        Ok(())
    }

    fn insert_tcp(&mut self, socket: TcpSocket<BbrV3>) -> SocketId {
        if let Some((index, slot)) = self
            .tcp
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.is_none())
        {
            *slot = Some(socket);
            return socket_id(index);
        }
        self.tcp.push(Some(socket));
        socket_id(self.tcp.len() - 1)
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
}

fn socket_id(index: usize) -> SocketId {
    let raw = u32::try_from(index + 1).unwrap_or_else(|_| panic!("socket index exceeds u32"));
    SocketId::new(raw)
}

fn socket_index(socket: SocketId) -> usize {
    usize::try_from(socket.raw() - 1)
        .unwrap_or_else(|_| panic!("socket id does not fit into usize"))
}
