extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::{
    AckSample, CongestionControl, CongestionEvent, IpAddress, RecoveryAction, TcpFlags, TcpHeader,
    TcpPacket,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TcpEndpoint {
    pub address: IpAddress,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

#[derive(Clone, Debug)]
pub struct TcpSocket<C>
where
    C: CongestionControl,
{
    local: Option<TcpEndpoint>,
    remote: Option<TcpEndpoint>,
    state: TcpState,
    congestion: C,
    send_next: u32,
    send_unacknowledged: u32,
    receive_next: u32,
    advertised_window: u16,
    receive_queue: VecDeque<Vec<u8>>,
    transmit_queue: VecDeque<Vec<u8>>,
    bytes_in_flight: u32,
    delivered_bytes: u64,
    syn_queued: bool,
}

impl<C> TcpSocket<C>
where
    C: CongestionControl,
{
    pub fn closed(congestion: C) -> Self {
        Self {
            local: None,
            remote: None,
            state: TcpState::Closed,
            congestion,
            send_next: 0,
            send_unacknowledged: 0,
            receive_next: 0,
            advertised_window: u16::MAX,
            receive_queue: VecDeque::new(),
            transmit_queue: VecDeque::new(),
            bytes_in_flight: 0,
            delivered_bytes: 0,
            syn_queued: false,
        }
    }

    pub fn listen(local: TcpEndpoint, congestion: C) -> Self {
        let mut socket = Self::closed(congestion);
        socket.local = Some(local);
        socket.state = TcpState::Listen;
        socket
    }

    pub fn connect(
        local: TcpEndpoint,
        remote: TcpEndpoint,
        initial_sequence: u32,
        congestion: C,
    ) -> Self {
        let mut socket = Self::closed(congestion);
        socket.local = Some(local);
        socket.remote = Some(remote);
        socket.state = TcpState::SynSent;
        socket.send_next = initial_sequence.wrapping_add(1);
        socket.send_unacknowledged = initial_sequence;
        socket
    }

    pub const fn state(&self) -> TcpState {
        self.state
    }

    pub const fn local_endpoint(&self) -> Option<TcpEndpoint> {
        self.local
    }

    pub const fn remote_endpoint(&self) -> Option<TcpEndpoint> {
        self.remote
    }

    pub fn congestion(&self) -> &C {
        &self.congestion
    }

    pub fn pending_syn(&self) -> Option<TcpHeader> {
        if self.state != TcpState::SynSent || self.syn_queued {
            return None;
        }
        let local = self.local?;
        let remote = self.remote?;
        Some(TcpHeader {
            source_port: local.port,
            destination_port: remote.port,
            sequence: self.send_unacknowledged,
            acknowledgement: 0,
            flags: TcpFlags::SYN,
            window_size: self.advertised_window,
        })
    }

    pub fn mark_syn_queued(&mut self, now_nanos: u64) {
        assert!(
            self.state == TcpState::SynSent,
            "SYN can only be queued while connecting"
        );
        self.syn_queued = true;
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(1);
        self.congestion
            .on_packet_sent(1, self.bytes_in_flight, now_nanos);
    }

    pub const fn advertised_window(&self) -> u16 {
        self.advertised_window
    }

    pub fn queue_send(&mut self, bytes: &[u8]) -> usize {
        let window = self
            .congestion
            .congestion_window()
            .bytes()
            .saturating_sub(self.bytes_in_flight) as usize;
        let writable = bytes.len().min(window);
        if writable != 0 {
            self.transmit_queue.push_back(bytes[..writable].to_vec());
        }
        writable
    }

    pub fn receive(&mut self, max_bytes: usize) -> Option<Vec<u8>> {
        let mut bytes = self.receive_queue.pop_front()?;
        bytes.truncate(max_bytes);
        Some(bytes)
    }

    pub fn on_segment(&mut self, packet: TcpPacket<'_>, now_nanos: u64) -> Option<RecoveryAction> {
        match self.state {
            TcpState::SynSent if packet.flags.contains(TcpFlags::SYN.union(TcpFlags::ACK)) => {
                self.remote = self.remote.map(|mut remote| {
                    remote.port = packet.source_port;
                    remote
                });
                self.receive_next = packet.sequence.wrapping_add(1);
                self.send_unacknowledged = packet.acknowledgement;
                self.state = TcpState::Established;
                Some(self.congestion.on_ack(AckSample {
                    acked_bytes: 1,
                    delivered_bytes: self.delivered_bytes,
                    bytes_in_flight: self.bytes_in_flight,
                    rtt_nanos: 0,
                    interval_nanos: 0,
                    now_nanos,
                    app_limited: false,
                }))
            }
            TcpState::Established => {
                let mut action = None;
                if packet.flags.contains(TcpFlags::ACK)
                    && packet.acknowledgement != self.send_unacknowledged
                {
                    let acked = packet
                        .acknowledgement
                        .wrapping_sub(self.send_unacknowledged);
                    self.send_unacknowledged = packet.acknowledgement;
                    self.bytes_in_flight = self.bytes_in_flight.saturating_sub(acked);
                    self.delivered_bytes = self.delivered_bytes.saturating_add(u64::from(acked));
                    action = Some(self.congestion.on_ack(AckSample {
                        acked_bytes: acked,
                        delivered_bytes: self.delivered_bytes,
                        bytes_in_flight: self.bytes_in_flight,
                        rtt_nanos: 0,
                        interval_nanos: 0,
                        now_nanos,
                        app_limited: self.transmit_queue.is_empty(),
                    }));
                }
                if packet.sequence == self.receive_next && !packet.payload.is_empty() {
                    self.receive_next = self
                        .receive_next
                        .wrapping_add(u32::try_from(packet.payload.len()).unwrap_or(u32::MAX));
                    self.receive_queue.push_back(packet.payload.to_vec());
                }
                if packet.flags.contains(TcpFlags::FIN) {
                    self.receive_next = self.receive_next.wrapping_add(1);
                    self.state = TcpState::CloseWait;
                }
                action
            }
            _ if packet.flags.contains(TcpFlags::RST) => {
                self.state = TcpState::Closed;
                Some(
                    self.congestion
                        .on_congestion_event(CongestionEvent::RetransmissionTimeout { now_nanos }),
                )
            }
            _ => None,
        }
    }

    pub fn close_send(&mut self) {
        self.state = match self.state {
            TcpState::Established => TcpState::FinWait1,
            TcpState::CloseWait => TcpState::LastAck,
            state => state,
        };
    }
}
