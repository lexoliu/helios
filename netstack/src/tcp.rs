extern crate alloc;

use alloc::vec::Vec;

use heapless::Deque;

use crate::{
    AckSample, CongestionControl, CongestionEvent, IpAddress, RecoveryAction, TcpFlags, TcpHeader,
    TcpPacket,
};

pub const MAX_TCP_QUEUED_SEGMENTS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TcpEndpoint {
    pub address: IpAddress,
    pub port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TcpTransmitSegment {
    pub local: TcpEndpoint,
    pub remote: TcpEndpoint,
    pub header: TcpHeader,
    pub payload: Vec<u8>,
    pub sequence_len: u32,
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
    receive_queue: Deque<Vec<u8>, MAX_TCP_QUEUED_SEGMENTS>,
    transmit_queue: Deque<Vec<u8>, MAX_TCP_QUEUED_SEGMENTS>,
    bytes_in_flight: u32,
    delivered_bytes: u64,
    syn_queued: bool,
    ack_pending: bool,
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
            receive_queue: Deque::new(),
            transmit_queue: Deque::new(),
            bytes_in_flight: 0,
            delivered_bytes: 0,
            syn_queued: false,
            ack_pending: false,
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

    pub fn pending_ack(&self) -> Option<TcpHeader> {
        if !self.ack_pending {
            return None;
        }
        let local = self.local?;
        let remote = self.remote?;
        Some(TcpHeader {
            source_port: local.port,
            destination_port: remote.port,
            sequence: self.send_next,
            acknowledgement: self.receive_next,
            flags: TcpFlags::ACK,
            window_size: self.advertised_window,
        })
    }

    pub fn mark_ack_queued(&mut self) {
        self.ack_pending = false;
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
            if self
                .transmit_queue
                .push_back(bytes[..writable].to_vec())
                .is_err()
            {
                return 0;
            }
        }
        writable
    }

    pub fn take_transmit_segment(&mut self, now_nanos: u64) -> Option<TcpTransmitSegment> {
        if self.state != TcpState::Established {
            return None;
        }
        let local = self.local?;
        let remote = self.remote?;
        let cwnd = self.congestion.congestion_window().bytes();
        if self.bytes_in_flight >= cwnd {
            return None;
        }
        let mut payload = self.transmit_queue.pop_front()?;
        let available = usize::try_from(cwnd - self.bytes_in_flight).unwrap_or(usize::MAX);
        if payload.len() > available {
            let tail = payload.split_off(available);
            self.transmit_queue
                .push_front(tail)
                .unwrap_or_else(|_| panic!("TCP transmit queue lost capacity while splitting"));
        }
        let sequence_len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
        let header = TcpHeader {
            source_port: local.port,
            destination_port: remote.port,
            sequence: self.send_next,
            acknowledgement: self.receive_next,
            flags: TcpFlags::ACK.union(TcpFlags::PSH),
            window_size: self.advertised_window,
        };
        self.send_next = self.send_next.wrapping_add(sequence_len);
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(sequence_len);
        self.congestion
            .on_packet_sent(sequence_len, self.bytes_in_flight, now_nanos);
        Some(TcpTransmitSegment {
            local,
            remote,
            header,
            payload,
            sequence_len,
        })
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
                self.ack_pending = true;
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
                    if self
                        .receive_queue
                        .push_back(packet.payload.to_vec())
                        .is_err()
                    {
                        return action;
                    }
                    self.receive_next = self
                        .receive_next
                        .wrapping_add(u32::try_from(packet.payload.len()).unwrap_or(u32::MAX));
                    self.ack_pending = true;
                }
                if packet.flags.contains(TcpFlags::FIN) {
                    self.receive_next = self.receive_next.wrapping_add(1);
                    self.state = TcpState::CloseWait;
                    self.ack_pending = true;
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
