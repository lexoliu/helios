extern crate alloc;

use bytes::{Bytes, BytesMut};
use heapless::{Deque, Vec as HeapVec};

use crate::{
    AckSample, CongestionControl, CongestionEvent, IpAddress, RecoveryAction, TcpFlags, TcpHeader,
    TcpHeaderOptions, TcpPacket, TcpSackBlock, TcpSackBlocks, TcpTimestampOption,
};

pub const MAX_TCP_RECEIVE_SEGMENTS: usize = 128;
pub const MAX_TCP_OUT_OF_ORDER_SEGMENTS: usize = 32;
pub const MAX_TCP_QUEUED_SEGMENTS: usize = 32;
pub(crate) const TCP_RECEIVE_SEGMENT_BYTES: usize = 1460;
const TCP_RECEIVE_COALESCE_BYTES: usize = 64 * 1024;
const TCP_RECEIVE_BYTES: usize = MAX_TCP_RECEIVE_SEGMENTS * TCP_RECEIVE_SEGMENT_BYTES;
const TCP_RECEIVE_BACKPRESSURE_BYTES: usize =
    TCP_RECEIVE_BACKPRESSURE_SEGMENTS * TCP_RECEIVE_SEGMENT_BYTES;
const TCP_SMALL_PAYLOAD_ACK_BYTES: usize = TCP_RECEIVE_SEGMENT_BYTES / 2;
const TCP_DELAYED_ACK_SEGMENTS: u8 = 2;
const TCP_WINDOW_UPDATE_BYTES: u16 = (TCP_RECEIVE_SEGMENT_BYTES * 4) as u16;
const TCP_LOCAL_WINDOW_SCALE: u8 = 2;
const TCP_MAX_WINDOW_SCALE: u8 = 14;
pub(crate) const TCP_RECEIVE_BACKPRESSURE_SEGMENTS: usize = MAX_TCP_RECEIVE_SEGMENTS - 4;
pub const TCP_INITIAL_RTO_NANOS: u64 = 1_000_000_000;
pub const TCP_MIN_RTO_NANOS: u64 = 200_000_000;
pub const TCP_MAX_RTO_NANOS: u64 = 60_000_000_000;
pub const TCP_MAX_RETRANSMISSIONS: u8 = 5;
pub const TCP_DELAYED_ACK_NANOS: u64 = 40_000_000;
pub const TCP_TIME_WAIT_NANOS: u64 = 60_000_000_000;

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
    pub options: TcpHeaderOptions,
    pub payload: Bytes,
    pub sequence_len: u32,
    pub retransmission: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpSegmentOutcome {
    pub recovery: Option<RecoveryAction>,
    pub receive_backpressure: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TcpInFlightSegment {
    header: TcpHeader,
    options: TcpHeaderOptions,
    payload: Bytes,
    sequence_len: u32,
    sent_at_nanos: u64,
    retransmissions: u8,
    sacked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TcpOutOfOrderSegment {
    sequence: u32,
    payload: Bytes,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TcpAckTiming {
    rtt_nanos: u64,
    interval_nanos: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TcpRetransmissionTimer {
    smoothed_rtt_nanos: Option<u64>,
    rtt_variance_nanos: u64,
    timeout_nanos: u64,
}

impl TcpRetransmissionTimer {
    const fn new() -> Self {
        Self {
            smoothed_rtt_nanos: None,
            rtt_variance_nanos: 0,
            timeout_nanos: TCP_INITIAL_RTO_NANOS,
        }
    }

    const fn timeout_nanos(self) -> u64 {
        self.timeout_nanos
    }

    fn note_rtt(&mut self, rtt_nanos: u64) {
        if rtt_nanos == 0 {
            return;
        }
        if let Some(smoothed) = self.smoothed_rtt_nanos {
            let error = smoothed.abs_diff(rtt_nanos);
            self.rtt_variance_nanos = (self.rtt_variance_nanos * 3 + error) / 4;
            self.smoothed_rtt_nanos = Some((smoothed * 7 + rtt_nanos) / 8);
        } else {
            self.smoothed_rtt_nanos = Some(rtt_nanos);
            self.rtt_variance_nanos = rtt_nanos / 2;
        }
        let smoothed = self
            .smoothed_rtt_nanos
            .expect("TCP RTO estimator lost initialized RTT");
        self.timeout_nanos = smoothed
            .saturating_add(self.rtt_variance_nanos.saturating_mul(4))
            .clamp(TCP_MIN_RTO_NANOS, TCP_MAX_RTO_NANOS);
    }

    fn backed_off_timeout_nanos(self, retransmissions: u8) -> u64 {
        let shift = retransmissions.min(6);
        self.timeout_nanos().saturating_mul(1u64 << shift)
    }
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
    peer_max_segment_size: usize,
    peer_window_scale: u8,
    peer_receive_window: u32,
    peer_sack_permitted: bool,
    peer_timestamp: Option<TcpTimestampOption>,
    receive_queue: Deque<BytesMut, MAX_TCP_RECEIVE_SEGMENTS>,
    receive_queued_bytes: usize,
    out_of_order: HeapVec<TcpOutOfOrderSegment, MAX_TCP_OUT_OF_ORDER_SEGMENTS>,
    out_of_order_queued_bytes: usize,
    transmit_queue: Deque<Bytes, MAX_TCP_QUEUED_SEGMENTS>,
    in_flight: Deque<TcpInFlightSegment, MAX_TCP_QUEUED_SEGMENTS>,
    bytes_in_flight: u32,
    delivered_bytes: u64,
    syn_queued: bool,
    syn_ack_queued: bool,
    fin_queued: bool,
    ack_pending: bool,
    duplicate_ack_count: u8,
    fast_retransmit_pending: bool,
    retransmission_timer: TcpRetransmissionTimer,
    next_pacing_send_nanos: Option<u64>,
    delayed_ack_deadline_nanos: Option<u64>,
    time_wait_deadline_nanos: Option<u64>,
    unacked_receive_segments: u8,
    pending_window_update_bytes: u16,
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
            advertised_window: receive_window_size(0, TCP_LOCAL_WINDOW_SCALE),
            peer_max_segment_size: TCP_RECEIVE_SEGMENT_BYTES,
            peer_window_scale: 0,
            peer_receive_window: u32::from(u16::MAX),
            peer_sack_permitted: false,
            peer_timestamp: None,
            receive_queue: Deque::new(),
            receive_queued_bytes: 0,
            out_of_order: HeapVec::new(),
            out_of_order_queued_bytes: 0,
            transmit_queue: Deque::new(),
            in_flight: Deque::new(),
            bytes_in_flight: 0,
            delivered_bytes: 0,
            syn_queued: false,
            syn_ack_queued: false,
            fin_queued: false,
            ack_pending: false,
            duplicate_ack_count: 0,
            fast_retransmit_pending: false,
            retransmission_timer: TcpRetransmissionTimer::new(),
            next_pacing_send_nanos: None,
            delayed_ack_deadline_nanos: None,
            time_wait_deadline_nanos: None,
            unacked_receive_segments: 0,
            pending_window_update_bytes: 0,
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

    pub fn accept(
        local: TcpEndpoint,
        remote: TcpEndpoint,
        receive_next: u32,
        initial_sequence: u32,
        congestion: C,
    ) -> Self {
        let mut socket = Self::closed(congestion);
        socket.local = Some(local);
        socket.remote = Some(remote);
        socket.state = TcpState::SynReceived;
        socket.receive_next = receive_next;
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

    pub fn pending_syn_ack(&self) -> Option<TcpHeader> {
        if self.state != TcpState::SynReceived || self.syn_ack_queued {
            return None;
        }
        let local = self.local?;
        let remote = self.remote?;
        Some(TcpHeader {
            source_port: local.port,
            destination_port: remote.port,
            sequence: self.send_unacknowledged,
            acknowledgement: self.receive_next,
            flags: TcpFlags::SYN.union(TcpFlags::ACK),
            window_size: self.advertised_window,
        })
    }

    pub fn mark_syn_queued(&mut self, now_nanos: u64) {
        assert!(
            self.state == TcpState::SynSent,
            "SYN can only be queued while connecting"
        );
        let header = self.pending_syn().expect("SYN header disappeared");
        self.syn_queued = true;
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(1);
        self.in_flight
            .push_back(TcpInFlightSegment {
                header,
                options: syn_header_options(now_nanos, None),
                payload: Bytes::new(),
                sequence_len: 1,
                sent_at_nanos: now_nanos,
                retransmissions: 0,
                sacked: false,
            })
            .unwrap_or_else(|_| panic!("TCP in-flight queue is full"));
        self.congestion
            .on_packet_sent(1, self.bytes_in_flight, now_nanos);
    }

    pub fn mark_syn_ack_queued(&mut self, now_nanos: u64) {
        assert!(
            self.state == TcpState::SynReceived,
            "SYN-ACK can only be queued for a half-open accepted socket"
        );
        let header = self.pending_syn_ack().expect("SYN-ACK header disappeared");
        self.syn_ack_queued = true;
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(1);
        self.in_flight
            .push_back(TcpInFlightSegment {
                header,
                options: syn_header_options(
                    now_nanos,
                    self.peer_timestamp.map(|timestamp| timestamp.value),
                ),
                payload: Bytes::new(),
                sequence_len: 1,
                sent_at_nanos: now_nanos,
                retransmissions: 0,
                sacked: false,
            })
            .unwrap_or_else(|_| panic!("TCP in-flight queue is full"));
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
        self.delayed_ack_deadline_nanos = None;
    }

    pub fn pending_ack_options(&self, now_nanos: u64) -> TcpHeaderOptions {
        let mut blocks = TcpSackBlocks::empty();
        if self.peer_sack_permitted {
            for segment in &self.out_of_order {
                if blocks
                    .push(TcpSackBlock {
                        left_edge: segment.sequence,
                        right_edge: segment_end_from_parts(segment.sequence, segment.payload.len()),
                    })
                    .is_err()
                {
                    break;
                }
            }
        }
        self.timestamped_options(now_nanos).with_sack_blocks(blocks)
    }

    pub const fn advertised_window(&self) -> u16 {
        self.advertised_window
    }

    pub fn pending_syn_options(&self, now_nanos: u64) -> TcpHeaderOptions {
        syn_header_options(now_nanos, None)
    }

    pub fn pending_syn_ack_options(&self, now_nanos: u64) -> TcpHeaderOptions {
        syn_header_options(
            now_nanos,
            self.peer_timestamp.map(|timestamp| timestamp.value),
        )
    }

    pub const fn peer_max_segment_size(&self) -> usize {
        self.peer_max_segment_size
    }

    pub const fn peer_window_scale(&self) -> u8 {
        self.peer_window_scale
    }

    pub const fn peer_receive_window(&self) -> u32 {
        self.peer_receive_window
    }

    pub const fn peer_sack_permitted(&self) -> bool {
        self.peer_sack_permitted
    }

    pub const fn peer_timestamp(&self) -> Option<TcpTimestampOption> {
        self.peer_timestamp
    }

    pub fn receive_backpressured(&self) -> bool {
        self.receive_buffered_bytes() >= TCP_RECEIVE_BACKPRESSURE_BYTES
    }

    pub fn is_listening_on(&self, address: IpAddress, port: u16) -> bool {
        let Some(local) = self.local else {
            return false;
        };
        self.state == TcpState::Listen
            && local.port == port
            && match (local.address, address) {
                (IpAddress::Ipv4(local), IpAddress::Ipv4(address)) => {
                    local.is_unspecified() || local == address
                }
                (IpAddress::Ipv6(local), IpAddress::Ipv6(address)) => {
                    local.is_unspecified() || local == address
                }
                _ => false,
            }
    }

    pub fn queue_send(&mut self, bytes: &[u8]) -> usize {
        let mut bytes = Bytes::copy_from_slice(bytes);
        self.queue_send_bytes(&mut bytes)
    }

    pub fn queue_send_bytes(&mut self, bytes: &mut Bytes) -> usize {
        if !matches!(self.state, TcpState::Established | TcpState::CloseWait) {
            return 0;
        }
        if self.transmit_queue.is_full() {
            return 0;
        }
        let window = self.available_send_window();
        let writable = bytes.len().min(window);
        if writable != 0 {
            let segment = bytes.split_to(writable);
            self.transmit_queue.push_back(segment).unwrap_or_else(|_| {
                panic!("TCP transmit queue reported full after capacity check")
            });
        }
        writable
    }

    pub fn take_transmit_segment(&mut self, now_nanos: u64) -> Option<TcpTransmitSegment> {
        let local = self.local?;
        let remote = self.remote?;
        let available_window = self.available_send_window();
        if available_window == 0 {
            return None;
        }
        if matches!(
            self.state,
            TcpState::Established | TcpState::CloseWait | TcpState::FinWait1 | TcpState::LastAck
        ) && !self.fin_queued
        {
            if let Some(segment) =
                self.take_data_segment(local, remote, available_window, now_nanos)
            {
                return Some(segment);
            }
        }

        if !self.transmit_queue.is_empty() {
            return None;
        }

        if !matches!(self.state, TcpState::FinWait1 | TcpState::LastAck) || self.fin_queued {
            return None;
        }

        let header = TcpHeader {
            source_port: local.port,
            destination_port: remote.port,
            sequence: self.send_next,
            acknowledgement: self.receive_next,
            flags: TcpFlags::ACK.union(TcpFlags::FIN),
            window_size: self.advertised_window,
        };
        self.fin_queued = true;
        self.ack_pending = false;
        self.send_next = self.send_next.wrapping_add(1);
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(1);
        self.congestion
            .on_packet_sent(1, self.bytes_in_flight, now_nanos);
        self.in_flight
            .push_back(TcpInFlightSegment {
                header,
                options: self.timestamped_options(now_nanos),
                payload: Bytes::new(),
                sequence_len: 1,
                sent_at_nanos: now_nanos,
                retransmissions: 0,
                sacked: false,
            })
            .unwrap_or_else(|_| panic!("TCP in-flight queue is full"));
        Some(TcpTransmitSegment {
            local,
            remote,
            header,
            options: self.timestamped_options(now_nanos),
            payload: Bytes::new(),
            sequence_len: 1,
            retransmission: false,
        })
    }

    pub fn expire_timers(&mut self, now_nanos: u64) {
        if self
            .delayed_ack_deadline_nanos
            .is_some_and(|deadline| now_nanos >= deadline)
        {
            self.request_ack();
        }
        if self.state == TcpState::TimeWait
            && self
                .time_wait_deadline_nanos
                .is_some_and(|deadline| now_nanos >= deadline)
        {
            self.state = TcpState::Closed;
            self.time_wait_deadline_nanos = None;
        }
    }

    pub fn next_deadline_nanos(&self) -> Option<u64> {
        let rto_deadline = self.in_flight.front().map(|segment| {
            segment.sent_at_nanos.saturating_add(
                self.retransmission_timer
                    .backed_off_timeout_nanos(segment.retransmissions),
            )
        });
        let pacing_deadline = self
            .next_pacing_send_nanos
            .filter(|_| !self.transmit_queue.is_empty());
        min_deadline(
            min_deadline(
                min_deadline(rto_deadline, pacing_deadline),
                self.delayed_ack_deadline_nanos,
            ),
            self.time_wait_deadline_nanos,
        )
    }

    fn take_data_segment(
        &mut self,
        local: TcpEndpoint,
        remote: TcpEndpoint,
        available: usize,
        now_nanos: u64,
    ) -> Option<TcpTransmitSegment> {
        if self
            .next_pacing_send_nanos
            .is_some_and(|deadline| now_nanos < deadline)
        {
            return None;
        }
        let mut payload = self.transmit_queue.pop_front()?;
        let maximum_segment = available.min(self.peer_max_segment_size);
        if payload.len() > maximum_segment {
            let tail = payload.split_off(maximum_segment);
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
        self.schedule_next_pacing_send(sequence_len, now_nanos);
        self.in_flight
            .push_back(TcpInFlightSegment {
                header,
                options: self.timestamped_options(now_nanos),
                payload: payload.clone(),
                sequence_len,
                sent_at_nanos: now_nanos,
                retransmissions: 0,
                sacked: false,
            })
            .unwrap_or_else(|_| panic!("TCP in-flight queue is full"));
        self.ack_pending = false;
        self.delayed_ack_deadline_nanos = None;
        Some(TcpTransmitSegment {
            local,
            remote,
            header,
            options: self.timestamped_options(now_nanos),
            payload,
            sequence_len,
            retransmission: false,
        })
    }

    pub fn pending_retransmission(&self, now_nanos: u64) -> Option<TcpTransmitSegment> {
        let in_flight = if self.fast_retransmit_pending {
            self.first_unsacked_in_flight()?
        } else {
            let in_flight = self.in_flight.front()?;
            if now_nanos.saturating_sub(in_flight.sent_at_nanos)
                < self
                    .retransmission_timer
                    .backed_off_timeout_nanos(in_flight.retransmissions)
            {
                return None;
            }
            in_flight
        };
        let local = self.local?;
        let remote = self.remote?;
        Some(TcpTransmitSegment {
            local,
            remote,
            header: in_flight.header,
            options: in_flight.options,
            payload: in_flight.payload.clone(),
            sequence_len: in_flight.sequence_len,
            retransmission: true,
        })
    }

    fn first_unsacked_in_flight(&self) -> Option<&TcpInFlightSegment> {
        self.in_flight.iter().find(|segment| !segment.sacked)
    }

    pub fn mark_retransmission_queued(&mut self, sequence: u32, now_nanos: u64) {
        let fast_retransmit = self.fast_retransmit_pending;
        if fast_retransmit {
            self.fast_retransmit_pending = false;
        }
        let Some(in_flight) = self
            .in_flight
            .iter_mut()
            .find(|segment| segment.header.sequence == sequence)
        else {
            return;
        };
        in_flight.sent_at_nanos = now_nanos;
        in_flight.retransmissions = in_flight.retransmissions.saturating_add(1);
        if in_flight.retransmissions > TCP_MAX_RETRANSMISSIONS {
            self.state = TcpState::Closed;
            self.bytes_in_flight = 0;
            self.in_flight.clear();
            return;
        }
        if !fast_retransmit {
            self.congestion
                .on_congestion_event(CongestionEvent::RetransmissionTimeout { now_nanos });
        }
    }

    pub fn receive(&mut self, max_bytes: usize) -> Option<Bytes> {
        if max_bytes == 0 {
            return (!self.receive_queue.is_empty()).then(Bytes::new);
        }
        let previous_window = self.advertised_window;
        let mut bytes = self.pop_receive_segment()?;
        if bytes.len() >= max_bytes {
            if bytes.len() > max_bytes {
                let tail = bytes.split_off(max_bytes);
                self.push_front_receive_segment(tail);
            }
            let drained_segments = self.drain_contiguous_out_of_order();
            self.refresh_advertised_window();
            if drained_segments != 0 || self.should_advertise_window_update(previous_window) {
                self.request_ack();
            }
            return Some(bytes.freeze());
        }

        if self.receive_queue.is_empty() {
            let drained_segments = self.drain_contiguous_out_of_order();
            self.refresh_advertised_window();
            if drained_segments != 0 || self.should_advertise_window_update(previous_window) {
                self.request_ack();
            }
            return Some(bytes.freeze());
        }

        let merge_len = self.receive_merge_len(bytes.len(), max_bytes);
        while bytes.len() < merge_len {
            let Some(mut next) = self.pop_receive_segment() else {
                break;
            };
            let remaining = merge_len - bytes.len();
            if next.len() > remaining {
                let tail = next.split_off(remaining);
                bytes.extend_from_slice(&next);
                self.push_front_receive_segment(tail);
                break;
            }
            bytes.extend_from_slice(&next);
        }
        let drained_segments = self.drain_contiguous_out_of_order();
        self.refresh_advertised_window();
        if drained_segments != 0 || self.should_advertise_window_update(previous_window) {
            self.request_ack();
        }
        Some(bytes.freeze())
    }

    fn receive_merge_len(&self, first_len: usize, max_bytes: usize) -> usize {
        first_len
            .saturating_add(self.receive_queued_bytes)
            .min(max_bytes)
    }

    pub fn on_segment(&mut self, packet: TcpPacket<'_>, now_nanos: u64) -> TcpSegmentOutcome {
        if packet.flags.contains(TcpFlags::RST) {
            self.state = TcpState::Closed;
            return TcpSegmentOutcome {
                recovery: Some(
                    self.congestion
                        .on_congestion_event(CongestionEvent::RetransmissionTimeout { now_nanos }),
                ),
                receive_backpressure: false,
            };
        }
        self.update_peer_receive_window(packet.window_size);
        self.record_peer_timestamp(packet.options.timestamp());

        match self.state {
            TcpState::SynSent if packet.flags.contains(TcpFlags::SYN.union(TcpFlags::ACK)) => {
                self.record_peer_options(packet);
                self.remote = self.remote.map(|mut remote| {
                    remote.port = packet.source_port;
                    remote
                });
                self.receive_next = packet.sequence.wrapping_add(1);
                let action = self.acknowledge_sent(
                    packet.acknowledgement,
                    now_nanos,
                    false,
                    packet.options.timestamp(),
                );
                self.state = TcpState::Established;
                self.ack_pending = true;
                TcpSegmentOutcome {
                    recovery: action,
                    receive_backpressure: false,
                }
            }
            TcpState::SynReceived if packet.flags.contains(TcpFlags::ACK) => {
                if packet.acknowledgement != self.send_next {
                    return TcpSegmentOutcome::default();
                }
                let action = self.acknowledge_sent(
                    packet.acknowledgement,
                    now_nanos,
                    true,
                    packet.options.timestamp(),
                );
                self.state = TcpState::Established;
                TcpSegmentOutcome {
                    recovery: action,
                    receive_backpressure: false,
                }
            }
            TcpState::Established
            | TcpState::FinWait1
            | TcpState::FinWait2
            | TcpState::CloseWait
            | TcpState::Closing
            | TcpState::LastAck => {
                let mut action = None;
                if packet.flags.contains(TcpFlags::ACK) {
                    self.apply_sack_blocks(packet.options.sack_blocks());
                    action = self.acknowledge_sent(
                        packet.acknowledgement,
                        now_nanos,
                        self.transmit_queue.is_empty(),
                        packet.options.timestamp(),
                    );
                }
                if !packet.payload.is_empty()
                    && matches!(
                        self.state,
                        TcpState::Established | TcpState::FinWait1 | TcpState::FinWait2
                    )
                {
                    if self
                        .receive_payload(packet.sequence, packet.payload, now_nanos)
                        .is_err()
                    {
                        self.refresh_advertised_window();
                        self.request_ack();
                        return TcpSegmentOutcome {
                            recovery: action,
                            receive_backpressure: true,
                        };
                    }
                }
                if packet.flags.contains(TcpFlags::FIN) {
                    self.receive_next = self.receive_next.wrapping_add(1);
                    match self.state {
                        TcpState::Established => self.state = TcpState::CloseWait,
                        TcpState::FinWait1 => self.state = TcpState::Closing,
                        TcpState::FinWait2 => self.enter_time_wait(now_nanos),
                        _ => {}
                    }
                    self.request_ack();
                }
                TcpSegmentOutcome {
                    recovery: action,
                    receive_backpressure: false,
                }
            }
            TcpState::TimeWait => {
                if packet.flags.contains(TcpFlags::FIN) {
                    self.enter_time_wait(now_nanos);
                    self.request_ack();
                }
                TcpSegmentOutcome::default()
            }
            _ => TcpSegmentOutcome::default(),
        }
    }

    pub fn close_send(&mut self) {
        self.state = match self.state {
            TcpState::Established => TcpState::FinWait1,
            TcpState::CloseWait => TcpState::LastAck,
            state => state,
        };
    }

    fn discard_acked_segments(&mut self, acknowledgement: u32) {
        while self
            .in_flight
            .front()
            .is_some_and(|segment| sequence_leq(segment_end(segment), acknowledgement))
        {
            let _ = self.in_flight.pop_front();
        }
    }

    fn acknowledge_sent(
        &mut self,
        acknowledgement: u32,
        now_nanos: u64,
        app_limited: bool,
        timestamp: Option<TcpTimestampOption>,
    ) -> Option<RecoveryAction> {
        if sequence_leq(acknowledgement, self.send_unacknowledged) {
            return (acknowledgement == self.send_unacknowledged)
                .then(|| self.note_duplicate_ack(now_nanos))
                .flatten();
        }
        let acked = acknowledgement.wrapping_sub(self.send_unacknowledged);
        let timing = self.ack_sample_timing(acknowledgement, now_nanos, timestamp);
        self.retransmission_timer.note_rtt(timing.rtt_nanos);
        self.send_unacknowledged = acknowledgement;
        self.duplicate_ack_count = 0;
        self.fast_retransmit_pending = false;
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(acked);
        self.discard_acked_segments(acknowledgement);
        self.delivered_bytes = self.delivered_bytes.saturating_add(u64::from(acked));
        if self.fin_queued && sequence_leq(self.send_next, acknowledgement) {
            match self.state {
                TcpState::FinWait1 => self.state = TcpState::FinWait2,
                TcpState::Closing => self.enter_time_wait(now_nanos),
                TcpState::LastAck => self.state = TcpState::Closed,
                _ => {}
            }
        }
        Some(self.congestion.on_ack(AckSample {
            acked_bytes: acked,
            delivered_bytes: self.delivered_bytes,
            bytes_in_flight: self.bytes_in_flight,
            rtt_nanos: timing.rtt_nanos,
            interval_nanos: timing.interval_nanos,
            now_nanos,
            app_limited,
        }))
    }

    fn note_duplicate_ack(&mut self, now_nanos: u64) -> Option<RecoveryAction> {
        let Some(lost_bytes) = self
            .first_unsacked_in_flight()
            .map(|segment| segment.sequence_len)
        else {
            return None;
        };
        self.duplicate_ack_count = self.duplicate_ack_count.saturating_add(1);
        if self.duplicate_ack_count < 3 || self.fast_retransmit_pending {
            return None;
        }
        self.fast_retransmit_pending = true;
        Some(
            self.congestion
                .on_congestion_event(CongestionEvent::PacketLoss {
                    lost_bytes,
                    bytes_in_flight: self.bytes_in_flight,
                    now_nanos,
                }),
        )
    }

    fn apply_sack_blocks(&mut self, blocks: TcpSackBlocks) {
        for block in blocks.as_slice() {
            for segment in &mut self.in_flight {
                if sequence_leq(block.left_edge, segment.header.sequence)
                    && sequence_leq(segment_end(segment), block.right_edge)
                {
                    segment.sacked = true;
                }
            }
        }
    }

    fn ack_sample_timing(
        &self,
        acknowledgement: u32,
        now_nanos: u64,
        timestamp: Option<TcpTimestampOption>,
    ) -> TcpAckTiming {
        let mut rtt_nanos = 0;
        let mut first_sent_at = None;
        let mut last_sent_at = None;
        for segment in &self.in_flight {
            if !sequence_leq(segment_end(segment), acknowledgement) {
                break;
            }
            if segment.retransmissions != 0 {
                continue;
            }
            first_sent_at.get_or_insert(segment.sent_at_nanos);
            last_sent_at = Some(segment.sent_at_nanos);
            rtt_nanos = now_nanos.saturating_sub(segment.sent_at_nanos);
        }
        let interval_nanos = first_sent_at
            .zip(last_sent_at)
            .map(|(first, last)| {
                now_nanos
                    .saturating_sub(first)
                    .max(last.saturating_sub(first))
            })
            .unwrap_or(0);
        let timestamp_rtt_nanos = timestamp.and_then(|timestamp| {
            (timestamp.echo_reply != 0).then(|| timestamp_echo_rtt_nanos(now_nanos, timestamp))
        });
        let rtt_nanos = timestamp_rtt_nanos.unwrap_or(rtt_nanos);
        let interval_nanos = if interval_nanos == 0 {
            timestamp_rtt_nanos.unwrap_or(0)
        } else {
            interval_nanos
        };
        TcpAckTiming {
            rtt_nanos,
            interval_nanos,
        }
    }

    fn refresh_advertised_window(&mut self) {
        self.advertised_window =
            receive_window_size(self.receive_buffered_bytes(), TCP_LOCAL_WINDOW_SCALE);
    }

    fn receive_buffered_bytes(&self) -> usize {
        self.receive_queued_bytes
            .checked_add(self.out_of_order_queued_bytes)
            .expect("TCP receive buffered byte count overflowed")
    }

    fn push_receive_payload(&mut self, payload: &[u8]) -> Result<(), ()> {
        if payload.is_empty() {
            return Ok(());
        }

        if let Some(back) = self.receive_queue.back_mut()
            && back.len().saturating_add(payload.len()) <= TCP_RECEIVE_COALESCE_BYTES
        {
            back.extend_from_slice(payload);
            self.receive_queued_bytes = self
                .receive_queued_bytes
                .checked_add(payload.len())
                .expect("TCP receive queued byte count overflowed");
            return Ok(());
        }

        let bytes = BytesMut::from(payload);
        self.push_receive_segment(bytes).map_err(|_| ())
    }

    fn push_receive_segment(&mut self, bytes: BytesMut) -> Result<(), BytesMut> {
        let len = bytes.len();
        self.receive_queue.push_back(bytes)?;
        self.receive_queued_bytes = self
            .receive_queued_bytes
            .checked_add(len)
            .expect("TCP receive queued byte count overflowed");
        Ok(())
    }

    fn push_front_receive_segment(&mut self, bytes: BytesMut) {
        let len = bytes.len();
        self.receive_queue
            .push_front(bytes)
            .unwrap_or_else(|_| panic!("TCP receive queue lost capacity while splitting"));
        self.receive_queued_bytes = self
            .receive_queued_bytes
            .checked_add(len)
            .expect("TCP receive queued byte count overflowed");
    }

    fn pop_receive_segment(&mut self) -> Option<BytesMut> {
        let bytes = self.receive_queue.pop_front()?;
        assert!(
            self.receive_queued_bytes >= bytes.len(),
            "TCP receive queued byte count is corrupt"
        );
        self.receive_queued_bytes -= bytes.len();
        Some(bytes)
    }

    fn receive_payload(&mut self, sequence: u32, payload: &[u8], now_nanos: u64) -> Result<(), ()> {
        let payload_len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
        let payload_end = sequence.wrapping_add(payload_len);
        if sequence_leq(payload_end, self.receive_next) {
            self.request_ack();
            return Ok(());
        }

        if sequence_lt(sequence, self.receive_next) {
            let trim = usize::try_from(self.receive_next.wrapping_sub(sequence))
                .expect("TCP receive overlap trim does not fit usize");
            return self.receive_payload(self.receive_next, &payload[trim..], now_nanos);
        }

        if sequence == self.receive_next {
            self.push_contiguous_payload(payload, now_nanos)?;
            return Ok(());
        }

        self.insert_out_of_order_payload(sequence, payload)?;
        self.refresh_advertised_window();
        self.request_ack();
        Ok(())
    }

    fn push_contiguous_payload(&mut self, payload: &[u8], now_nanos: u64) -> Result<(), ()> {
        self.push_receive_payload(payload)?;
        self.receive_next = self
            .receive_next
            .wrapping_add(u32::try_from(payload.len()).unwrap_or(u32::MAX));
        let drained_segments = self.drain_contiguous_out_of_order();
        self.refresh_advertised_window();
        if drained_segments == 0 {
            self.note_inbound_payload(payload.len(), now_nanos);
        } else {
            self.request_ack();
        }
        Ok(())
    }

    fn drain_contiguous_out_of_order(&mut self) -> usize {
        let mut drained = 0;
        loop {
            let Some(segment) = self.out_of_order.first() else {
                return drained;
            };
            let segment_end = segment_end_from_parts(segment.sequence, segment.payload.len());
            if sequence_leq(segment_end, self.receive_next) {
                let stale = self.out_of_order.remove(0);
                self.out_of_order_queued_bytes = self
                    .out_of_order_queued_bytes
                    .checked_sub(stale.payload.len())
                    .expect("TCP out-of-order byte count underflowed");
                continue;
            }
            if sequence_lt(self.receive_next, segment.sequence) {
                return drained;
            }
            if self.receive_queue.is_full() {
                return drained;
            }

            let mut segment = self.out_of_order.remove(0);
            self.out_of_order_queued_bytes = self
                .out_of_order_queued_bytes
                .checked_sub(segment.payload.len())
                .expect("TCP out-of-order byte count underflowed");
            if sequence_lt(segment.sequence, self.receive_next) {
                let trim = usize::try_from(self.receive_next.wrapping_sub(segment.sequence))
                    .expect("TCP out-of-order overlap trim does not fit usize");
                let _ = segment.payload.split_to(trim);
                segment.sequence = self.receive_next;
            }
            let len = segment.payload.len();
            self.push_receive_segment(BytesMut::from(segment.payload.as_ref()))
                .unwrap_or_else(|_| panic!("TCP receive queue reported full after capacity check"));
            self.receive_next = self
                .receive_next
                .wrapping_add(u32::try_from(len).unwrap_or(u32::MAX));
            drained += 1;
        }
    }

    fn insert_out_of_order_payload(&mut self, sequence: u32, payload: &[u8]) -> Result<(), ()> {
        let end = segment_end_from_parts(sequence, payload.len());
        let mut cursor_sequence = sequence;
        let mut cursor_offset = 0;
        let mut fragments = HeapVec::<(u32, usize, usize), MAX_TCP_OUT_OF_ORDER_SEGMENTS>::new();

        for existing in &self.out_of_order {
            let existing_end = segment_end_from_parts(existing.sequence, existing.payload.len());
            if sequence_leq(existing_end, cursor_sequence) {
                continue;
            }
            if sequence_leq(end, existing.sequence) {
                break;
            }
            if sequence_lt(cursor_sequence, existing.sequence) {
                let fragment_len = usize::try_from(existing.sequence.wrapping_sub(cursor_sequence))
                    .expect("TCP out-of-order fragment length does not fit usize");
                fragments
                    .push((cursor_sequence, cursor_offset, cursor_offset + fragment_len))
                    .map_err(|_| ())?;
            }
            if sequence_leq(end, existing_end) {
                cursor_sequence = end;
                cursor_offset = payload.len();
                break;
            }
            let skip = usize::try_from(existing_end.wrapping_sub(cursor_sequence))
                .expect("TCP out-of-order overlap length does not fit usize");
            cursor_sequence = existing_end;
            cursor_offset += skip;
        }

        if sequence_lt(cursor_sequence, end) {
            fragments
                .push((cursor_sequence, cursor_offset, payload.len()))
                .map_err(|_| ())?;
        }

        for (fragment_sequence, start, end) in fragments {
            self.insert_out_of_order_fragment(fragment_sequence, &payload[start..end])?;
        }
        Ok(())
    }

    fn insert_out_of_order_fragment(&mut self, sequence: u32, payload: &[u8]) -> Result<(), ()> {
        if payload.is_empty() {
            return Ok(());
        }
        let segment = TcpOutOfOrderSegment {
            sequence,
            payload: Bytes::copy_from_slice(payload),
        };
        let len = segment.payload.len();
        self.out_of_order.push(segment).map_err(|_| ())?;
        self.out_of_order_queued_bytes = self
            .out_of_order_queued_bytes
            .checked_add(len)
            .expect("TCP out-of-order byte count overflowed");

        let mut index = self.out_of_order.len() - 1;
        while index != 0
            && sequence_lt(
                self.out_of_order[index].sequence,
                self.out_of_order[index - 1].sequence,
            )
        {
            self.out_of_order.swap(index, index - 1);
            index -= 1;
        }
        Ok(())
    }

    fn note_inbound_payload(&mut self, payload_len: usize, now_nanos: u64) {
        self.unacked_receive_segments = self.unacked_receive_segments.saturating_add(1);
        if self.unacked_receive_segments >= TCP_DELAYED_ACK_SEGMENTS
            || payload_len < TCP_SMALL_PAYLOAD_ACK_BYTES
        {
            self.request_ack();
        } else if self.delayed_ack_deadline_nanos.is_none() {
            self.delayed_ack_deadline_nanos = Some(now_nanos.saturating_add(TCP_DELAYED_ACK_NANOS));
        }
    }

    pub(crate) fn record_peer_options(&mut self, packet: TcpPacket<'_>) {
        if let Some(mss) = packet.options.maximum_segment_size() {
            self.peer_max_segment_size = usize::from(mss).min(TCP_RECEIVE_SEGMENT_BYTES).max(1);
        }
        if let Some(shift) = packet.options.window_scale() {
            self.peer_window_scale = shift.min(TCP_MAX_WINDOW_SCALE);
            self.update_peer_receive_window(packet.window_size);
        }
        self.peer_sack_permitted = packet.options.sack_permitted();
        self.record_peer_timestamp(packet.options.timestamp());
    }

    fn record_peer_timestamp(&mut self, timestamp: Option<TcpTimestampOption>) {
        if let Some(timestamp) = timestamp {
            self.peer_timestamp = Some(timestamp);
        }
    }

    fn update_peer_receive_window(&mut self, window_size: u16) {
        self.peer_receive_window = u32::from(window_size) << self.peer_window_scale;
    }

    fn available_send_window(&self) -> usize {
        let congestion_window = self
            .congestion
            .congestion_window()
            .bytes()
            .saturating_sub(self.bytes_in_flight);
        let receive_window = self
            .peer_receive_window
            .saturating_sub(self.bytes_in_flight);
        congestion_window.min(receive_window) as usize
    }

    fn schedule_next_pacing_send(&mut self, bytes: u32, now_nanos: u64) {
        self.next_pacing_send_nanos = self
            .congestion
            .pacing_rate()
            .map(|rate| now_nanos.saturating_add(pacing_interval_nanos(bytes, rate)));
    }

    fn timestamped_options(&self, now_nanos: u64) -> TcpHeaderOptions {
        let Some(peer_timestamp) = self.peer_timestamp else {
            return TcpHeaderOptions::empty();
        };
        TcpHeaderOptions::empty().with_timestamp(TcpTimestampOption {
            value: timestamp_value(now_nanos),
            echo_reply: peer_timestamp.value,
        })
    }

    fn request_ack(&mut self) {
        self.ack_pending = true;
        self.delayed_ack_deadline_nanos = None;
        self.unacked_receive_segments = 0;
        self.pending_window_update_bytes = 0;
    }

    fn enter_time_wait(&mut self, now_nanos: u64) {
        self.state = TcpState::TimeWait;
        self.time_wait_deadline_nanos = Some(now_nanos.saturating_add(TCP_TIME_WAIT_NANOS));
    }

    fn should_advertise_window_update(&mut self, previous_window: u16) -> bool {
        if self.advertised_window <= previous_window {
            return false;
        }
        if previous_window == 0 {
            return true;
        }
        self.pending_window_update_bytes = self
            .pending_window_update_bytes
            .saturating_add(self.advertised_window - previous_window);
        self.pending_window_update_bytes >= TCP_WINDOW_UPDATE_BYTES
    }
}

fn receive_window_size(queued_bytes: usize, scale: u8) -> u16 {
    let bytes = TCP_RECEIVE_BYTES.saturating_sub(queued_bytes);
    (bytes >> scale).min(u16::MAX as usize) as u16
}

fn syn_header_options(now_nanos: u64, echo_reply: Option<u32>) -> TcpHeaderOptions {
    TcpHeaderOptions::empty()
        .with_maximum_segment_size(TCP_RECEIVE_SEGMENT_BYTES as u16)
        .with_window_scale(TCP_LOCAL_WINDOW_SCALE)
        .with_sack_permitted()
        .with_timestamp(TcpTimestampOption {
            value: timestamp_value(now_nanos),
            echo_reply: echo_reply.unwrap_or(0),
        })
}

fn segment_end(segment: &TcpInFlightSegment) -> u32 {
    segment.header.sequence.wrapping_add(segment.sequence_len)
}

fn segment_end_from_parts(sequence: u32, len: usize) -> u32 {
    sequence.wrapping_add(u32::try_from(len).unwrap_or(u32::MAX))
}

fn sequence_leq(lhs: u32, rhs: u32) -> bool {
    lhs == rhs || (rhs.wrapping_sub(lhs) as i32) >= 0
}

fn sequence_lt(lhs: u32, rhs: u32) -> bool {
    lhs != rhs && sequence_leq(lhs, rhs)
}

fn timestamp_value(now_nanos: u64) -> u32 {
    (now_nanos / 1_000_000) as u32
}

fn timestamp_echo_rtt_nanos(now_nanos: u64, timestamp: TcpTimestampOption) -> u64 {
    u64::from(timestamp_value(now_nanos).wrapping_sub(timestamp.echo_reply)) * 1_000_000
}

fn pacing_interval_nanos(bytes: u32, rate: crate::PacingRate) -> u64 {
    ((u64::from(bytes) * 1_000_000_000) / rate.bytes_per_second()).max(1)
}

fn min_deadline(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BbrV3, CongestionWindow, Ipv4Address, PacingRate, TcpOptions, TcpSackBlock, TcpSackBlocks,
    };

    #[derive(Clone, Debug)]
    struct FixedPacing {
        cwnd: CongestionWindow,
        pacing_rate: Option<PacingRate>,
    }

    impl FixedPacing {
        const fn new(cwnd: CongestionWindow, pacing_rate: Option<PacingRate>) -> Self {
            Self { cwnd, pacing_rate }
        }
    }

    impl CongestionControl for FixedPacing {
        fn algorithm_name(&self) -> &'static str {
            "fixed-pacing-test"
        }

        fn on_packet_sent(&mut self, _bytes: u32, _bytes_in_flight: u32, _now_nanos: u64) {}

        fn on_ack(&mut self, _sample: AckSample) -> RecoveryAction {
            RecoveryAction {
                cwnd: self.cwnd,
                pacing_rate: self.pacing_rate,
            }
        }

        fn on_congestion_event(&mut self, _event: CongestionEvent) -> RecoveryAction {
            RecoveryAction {
                cwnd: self.cwnd,
                pacing_rate: self.pacing_rate,
            }
        }

        fn congestion_window(&self) -> CongestionWindow {
            self.cwnd
        }

        fn pacing_rate(&self) -> Option<PacingRate> {
            self.pacing_rate
        }
    }

    fn endpoint(port: u16) -> TcpEndpoint {
        TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::new([192, 0, 2, 1])),
            port,
        }
    }

    fn peer(port: u16) -> TcpEndpoint {
        TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::new([192, 0, 2, 2])),
            port,
        }
    }

    fn established_socket() -> TcpSocket<BbrV3> {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );
        socket.mark_ack_queued();
        socket
    }

    fn established_fixed_pacing_socket(rate: PacingRate) -> TcpSocket<FixedPacing> {
        let mut socket = TcpSocket::connect(
            endpoint(49152),
            peer(80),
            7,
            FixedPacing::new(CongestionWindow::new(16), Some(rate)),
        );
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );
        socket.mark_ack_queued();
        socket
    }

    #[test]
    fn syn_is_tracked_for_rto_retransmission() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));

        socket.mark_syn_queued(0);

        assert_eq!(
            socket.pending_retransmission(TCP_INITIAL_RTO_NANOS - 1),
            None
        );
        let segment = socket
            .pending_retransmission(TCP_INITIAL_RTO_NANOS)
            .expect("SYN should retransmit after initial RTO");
        assert!(segment.retransmission);
        assert_eq!(segment.header.sequence, 7);
        assert!(segment.header.flags.contains(TcpFlags::SYN));
    }

    #[test]
    fn retransmission_timer_tracks_rtt_and_clamps_timeout() {
        let mut timer = TcpRetransmissionTimer::new();

        timer.note_rtt(20_000_000);

        assert_eq!(timer.timeout_nanos(), TCP_MIN_RTO_NANOS);
        assert_eq!(timer.backed_off_timeout_nanos(2), TCP_MIN_RTO_NANOS * 4);
    }

    #[test]
    fn ack_discards_fully_covered_in_flight_data() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );

        assert_eq!(socket.queue_send(b"hello"), 5);
        let data = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS)
            .expect("established socket should transmit queued bytes");
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: data.header.sequence + data.sequence_len,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(
            socket.pending_retransmission(TCP_INITIAL_RTO_NANOS * 4),
            None
        );
    }

    #[test]
    fn ack_sample_timing_uses_in_flight_send_timestamp() {
        let mut socket = established_socket();
        assert_eq!(socket.queue_send(b"hello"), 5);
        let sent_at = TCP_INITIAL_RTO_NANOS + 7;
        let data = socket
            .take_transmit_segment(sent_at)
            .expect("established socket should transmit queued bytes");
        let acked_at = sent_at + 123;

        let timing =
            socket.ack_sample_timing(data.header.sequence + data.sequence_len, acked_at, None);

        assert_eq!(
            timing,
            TcpAckTiming {
                rtt_nanos: 123,
                interval_nanos: 123,
            }
        );
    }

    #[test]
    fn ack_sample_timing_uses_timestamp_echo_when_present() {
        let socket = established_socket();
        let timing = socket.ack_sample_timing(
            8,
            TCP_INITIAL_RTO_NANOS + 7_000_000,
            Some(TcpTimestampOption {
                value: 0,
                echo_reply: timestamp_value(TCP_INITIAL_RTO_NANOS),
            }),
        );

        assert_eq!(
            timing,
            TcpAckTiming {
                rtt_nanos: 7_000_000,
                interval_nanos: 7_000_000,
            }
        );
    }

    #[test]
    fn three_duplicate_acks_queue_fast_retransmit_before_rto() {
        let mut socket = established_socket();
        assert_eq!(socket.queue_send(b"hello"), 5);
        let sent_at = TCP_INITIAL_RTO_NANOS + 7;
        let data = socket
            .take_transmit_segment(sent_at)
            .expect("established socket should transmit queued bytes");

        for index in 0..3 {
            let _ = socket.on_segment(
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101,
                    acknowledgement: data.header.sequence,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload: &[],
                },
                sent_at + index,
            );
        }

        let retransmit = socket
            .pending_retransmission(sent_at + 3)
            .expect("third duplicate ACK should trigger fast retransmit");
        assert!(retransmit.retransmission);
        assert_eq!(retransmit.header.sequence, data.header.sequence);
        assert_eq!(retransmit.payload, data.payload);
    }

    #[test]
    fn fast_retransmit_skips_sacked_in_flight_segments() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::parse(&[2, 4, 0, 4]).expect("MSS option should parse"),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );
        socket.mark_ack_queued();
        assert_eq!(socket.queue_send(b"abcdefgh"), 8);
        let sent_at = TCP_INITIAL_RTO_NANOS + 7;
        let first = socket
            .take_transmit_segment(sent_at)
            .expect("first segment should transmit");
        let second_send_at = socket
            .next_deadline_nanos()
            .expect("paced second segment should have a deadline");
        let second = socket
            .take_transmit_segment(second_send_at)
            .expect("second segment should transmit");
        let mut blocks = TcpSackBlocks::empty();
        blocks
            .push(TcpSackBlock {
                left_edge: first.header.sequence,
                right_edge: first.header.sequence + first.sequence_len,
            })
            .expect("single SACK block should fit");
        socket.apply_sack_blocks(blocks);

        for index in 0..3 {
            let _ = socket.on_segment(
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101,
                    acknowledgement: first.header.sequence,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload: &[],
                },
                sent_at + 2 + index,
            );
        }

        let retransmit = socket
            .pending_retransmission(sent_at + 5)
            .expect("fast retransmit should choose unsacked data");
        assert_eq!(retransmit.header.sequence, second.header.sequence);
        assert_eq!(retransmit.payload, second.payload);
    }

    #[test]
    fn queue_send_bytes_reuses_payload_storage() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );

        let mut payload = Bytes::from_static(b"hello");
        let payload_ptr = payload.as_ptr();
        assert_eq!(socket.queue_send_bytes(&mut payload), 5);
        assert!(payload.is_empty());
        let segment = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS)
            .expect("established socket should transmit queued bytes");
        assert_eq!(segment.payload.as_ref(), b"hello");
        assert_eq!(segment.payload.as_ptr(), payload_ptr);
    }

    #[test]
    fn peer_mss_option_caps_transmit_segment_size() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::parse(&[2, 4, 0, 4]).expect("MSS option should parse"),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );

        assert_eq!(socket.peer_max_segment_size(), 4);
        assert_eq!(socket.queue_send(b"abcdefghij"), 10);
        let first = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS + 1)
            .expect("queued bytes should produce a capped segment");

        assert_eq!(first.payload.as_ref(), b"abcd");
    }

    #[test]
    fn peer_window_scale_expands_tracked_receive_window() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: 4,
                options: TcpOptions::parse(&[3, 3, 3, 0]).expect("window scale should parse"),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );

        assert_eq!(socket.peer_window_scale(), 3);
        assert_eq!(socket.peer_receive_window(), 32);
        assert_eq!(socket.queue_send(b"abcdefghijklmnopqrstuvwxyz"), 26);
        let segment = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS + 1)
            .expect("scaled peer receive window should allow data");

        assert_eq!(segment.payload.len(), 26);
    }

    #[test]
    fn peer_receive_window_caps_queued_send_bytes() {
        let mut socket = established_socket();
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: 4,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(socket.peer_receive_window(), 4);
        assert_eq!(socket.queue_send(b"abcdefghij"), 4);
    }

    #[test]
    fn pacing_rate_delays_next_data_segment() {
        let mut socket = established_fixed_pacing_socket(PacingRate::from_bytes_per_second(1_000));
        assert_eq!(socket.queue_send(b"abcdefgh"), 8);
        let first = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS + 1)
            .expect("first paced segment should transmit immediately");
        assert_eq!(first.payload.as_ref(), b"abcdefgh");
        assert_eq!(socket.queue_send(b"ijkl"), 4);
        assert_eq!(
            socket.next_deadline_nanos(),
            Some(TCP_INITIAL_RTO_NANOS + 1 + 8_000_000)
        );
        assert_eq!(
            socket.take_transmit_segment(TCP_INITIAL_RTO_NANOS + 2),
            None
        );

        let second = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS + 1 + 8_000_000)
            .expect("paced deadline should release the next segment");
        assert_eq!(second.payload.as_ref(), b"ijkl");
    }

    #[test]
    fn paced_close_waits_for_queued_data_before_fin() {
        let mut socket = established_fixed_pacing_socket(PacingRate::from_bytes_per_second(1_000));
        assert_eq!(socket.queue_send(b"abcdefgh"), 8);
        let first = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS + 1)
            .expect("first paced segment should transmit immediately");
        assert_eq!(first.payload.as_ref(), b"abcdefgh");
        assert_eq!(socket.queue_send(b"ijkl"), 4);

        socket.close_send();
        assert_eq!(socket.state(), TcpState::FinWait1);
        assert_eq!(
            socket.take_transmit_segment(TCP_INITIAL_RTO_NANOS + 2),
            None
        );

        let second = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS + 1 + 8_000_000)
            .expect("paced data must be transmitted before FIN");
        assert_eq!(second.payload.as_ref(), b"ijkl");

        let fin = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS + 1 + 12_000_000)
            .expect("FIN should follow drained queued data");
        assert!(fin.payload.is_empty());
        assert!(fin.header.flags.contains(TcpFlags::FIN));
    }

    #[test]
    fn active_close_sends_queued_data_before_fin_and_enters_fin_wait2() {
        let mut socket = established_socket();
        assert_eq!(socket.queue_send(b"hi"), 2);
        socket.close_send();
        assert_eq!(socket.state(), TcpState::FinWait1);

        let data = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS + 1)
            .expect("active close must drain queued data before FIN");
        assert_eq!(data.payload.as_ref(), b"hi");
        assert!(data.header.flags.contains(TcpFlags::PSH));
        assert!(!data.header.flags.contains(TcpFlags::FIN));

        let fin = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS + 2)
            .expect("active close must queue FIN after data");
        assert!(fin.payload.is_empty());
        assert_eq!(fin.sequence_len, 1);
        assert!(fin.header.flags.contains(TcpFlags::FIN));
        assert_eq!(
            fin.header.sequence,
            data.header.sequence + data.sequence_len
        );

        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: fin.header.sequence + 1,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 3,
        );
        assert_eq!(socket.state(), TcpState::FinWait2);
    }

    #[test]
    fn time_wait_closes_after_deadline() {
        let mut socket = established_socket();
        socket.close_send();
        let fin = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS + 1)
            .expect("active close must queue FIN");
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: fin.header.sequence + 1,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 2,
        );
        assert_eq!(socket.state(), TcpState::FinWait2);

        let fin_received_at = TCP_INITIAL_RTO_NANOS + 3;
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: fin.header.sequence + 1,
                flags: TcpFlags::ACK.union(TcpFlags::FIN),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            fin_received_at,
        );
        assert_eq!(socket.state(), TcpState::TimeWait);
        assert_eq!(
            socket.next_deadline_nanos(),
            Some(fin_received_at + TCP_TIME_WAIT_NANOS)
        );

        socket.expire_timers(fin_received_at + TCP_TIME_WAIT_NANOS - 1);
        assert_eq!(socket.state(), TcpState::TimeWait);
        socket.expire_timers(fin_received_at + TCP_TIME_WAIT_NANOS);
        assert_eq!(socket.state(), TcpState::Closed);
        assert_eq!(socket.next_deadline_nanos(), None);
    }

    #[test]
    fn passive_close_sends_fin_and_closes_after_fin_ack() {
        let mut socket = established_socket();
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK.union(TcpFlags::FIN),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );
        assert_eq!(socket.state(), TcpState::CloseWait);

        socket.close_send();
        assert_eq!(socket.state(), TcpState::LastAck);
        let fin = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS + 2)
            .expect("passive close must queue FIN from LAST-ACK");
        assert!(fin.header.flags.contains(TcpFlags::FIN));

        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 102,
                acknowledgement: fin.header.sequence + 1,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 3,
        );
        assert_eq!(socket.state(), TcpState::Closed);
    }

    #[test]
    fn data_transmit_piggybacks_pending_ack() {
        let mut socket = established_socket();
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: b"r",
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );
        assert!(socket.pending_ack().is_some());
        assert_eq!(socket.queue_send(b"w"), 1);

        let segment = socket
            .take_transmit_segment(TCP_INITIAL_RTO_NANOS + 2)
            .expect("queued data must transmit");
        assert!(segment.header.flags.contains(TcpFlags::ACK));
        assert_eq!(socket.pending_ack(), None);
    }

    #[test]
    fn receive_window_tracks_queued_payload_segments() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );
        let open_window = socket.advertised_window();
        let payload = [0u8; TCP_RECEIVE_SEGMENT_BYTES];
        let queued_segments = 4;
        for index in 0..queued_segments {
            let _ = socket.on_segment(
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101 + index as u32 * TCP_RECEIVE_SEGMENT_BYTES as u32,
                    acknowledgement: 8,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload: &payload,
                },
                TCP_INITIAL_RTO_NANOS + index as u64 + 1,
            );
        }
        assert!(socket.advertised_window() < open_window);
        assert_eq!(
            socket.receive(TCP_RECEIVE_SEGMENT_BYTES).as_deref(),
            Some(&payload[..])
        );
        assert!(socket.advertised_window() < open_window);
        for _ in 1..queued_segments {
            assert_eq!(
                socket.receive(TCP_RECEIVE_SEGMENT_BYTES).as_deref(),
                Some(&payload[..])
            );
        }
        assert_eq!(socket.advertised_window(), open_window);
    }

    #[test]
    fn full_size_receive_payloads_ack_every_two_segments() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );
        socket.mark_ack_queued();

        let payload = [0u8; TCP_RECEIVE_SEGMENT_BYTES];
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &payload,
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );
        assert_eq!(socket.pending_ack(), None);

        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101 + TCP_RECEIVE_SEGMENT_BYTES as u32,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &payload,
            },
            TCP_INITIAL_RTO_NANOS + 2,
        );
        assert!(socket.pending_ack().is_some());
    }

    #[test]
    fn full_size_receive_payload_delayed_ack_expires() {
        let mut socket = established_socket();
        let payload = [0u8; TCP_RECEIVE_SEGMENT_BYTES];
        let received_at = TCP_INITIAL_RTO_NANOS + 1;
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &payload,
            },
            received_at,
        );
        assert_eq!(socket.pending_ack(), None);
        assert_eq!(
            socket.next_deadline_nanos(),
            Some(received_at + TCP_DELAYED_ACK_NANOS)
        );

        socket.expire_timers(received_at + TCP_DELAYED_ACK_NANOS - 1);
        assert_eq!(socket.pending_ack(), None);
        socket.expire_timers(received_at + TCP_DELAYED_ACK_NANOS);
        assert!(socket.pending_ack().is_some());
        assert_eq!(socket.next_deadline_nanos(), None);
    }

    #[test]
    fn small_receive_payload_ack_is_immediate() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );
        socket.mark_ack_queued();

        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: b"ok",
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );
        assert!(socket.pending_ack().is_some());
    }

    #[test]
    fn receive_aggregates_segments_up_to_read_budget() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );
        socket.mark_ack_queued();

        for (index, payload) in [b"abc".as_slice(), b"def".as_slice(), b"ghij".as_slice()]
            .into_iter()
            .enumerate()
        {
            let _ = socket.on_segment(
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101 + index as u32 * 3,
                    acknowledgement: 8,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload,
                },
                TCP_INITIAL_RTO_NANOS + index as u64 + 1,
            );
        }

        assert_eq!(socket.receive(7).as_deref(), Some(b"abcdefg".as_slice()));
        assert_eq!(socket.receive(7).as_deref(), Some(b"hij".as_slice()));
    }

    #[test]
    fn receive_single_segment_does_not_allocate_merge_buffer() {
        let mut socket = established_socket();
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: b"abc",
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(
            socket.receive(usize::MAX).as_deref(),
            Some(b"abc".as_slice())
        );
    }

    #[test]
    fn receive_merge_len_uses_available_bytes_not_read_budget() {
        let mut socket = established_socket();
        for (index, payload) in [b"abc".as_slice(), b"defg".as_slice()]
            .into_iter()
            .enumerate()
        {
            let _ = socket.on_segment(
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101 + index as u32 * 3,
                    acknowledgement: 8,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload,
                },
                TCP_INITIAL_RTO_NANOS + index as u64 + 1,
            );
        }

        assert_eq!(socket.receive_merge_len(0, usize::MAX), 7);
        assert_eq!(socket.receive_merge_len(0, 5), 5);
    }

    #[test]
    fn out_of_order_receive_payload_reassembles_after_gap_arrives() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );
        socket.mark_ack_queued();

        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 104,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: b"def",
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        let ack = socket
            .pending_ack()
            .expect("out-of-order payload must request duplicate ACK");
        assert_eq!(ack.acknowledgement, 101);
        assert_eq!(socket.receive(16), None);

        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: b"abc",
            },
            TCP_INITIAL_RTO_NANOS + 2,
        );

        let ack = socket
            .pending_ack()
            .expect("gap fill should acknowledge the reassembled range");
        assert_eq!(ack.acknowledgement, 107);
        assert_eq!(socket.receive(16).as_deref(), Some(b"abcdef".as_slice()));
    }

    #[test]
    fn out_of_order_receive_payload_drains_sorted_segments() {
        let mut socket = established_socket();
        socket.mark_ack_queued();

        for (sequence, payload) in [(107, b"ghi".as_slice()), (104, b"def".as_slice())] {
            let _ = socket.on_segment(
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence,
                    acknowledgement: 8,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload,
                },
                TCP_INITIAL_RTO_NANOS + u64::from(sequence),
            );
        }
        assert_eq!(socket.receive(16), None);

        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: b"abc",
            },
            TCP_INITIAL_RTO_NANOS + 3,
        );

        assert_eq!(socket.receive(16).as_deref(), Some(b"abcdefghi".as_slice()));
    }

    #[test]
    fn pending_ack_options_report_out_of_order_sack_blocks() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::parse(&[4, 2, 0, 0]).expect("SACK-permitted should parse"),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );
        socket.mark_ack_queued();

        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 104,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: b"def",
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(
            socket
                .pending_ack_options(TCP_INITIAL_RTO_NANOS + 2)
                .sack_blocks()
                .as_slice(),
            &[TcpSackBlock {
                left_edge: 104,
                right_edge: 107,
            }]
        );
    }

    #[test]
    fn pending_ack_options_echo_peer_timestamp() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::parse(&[8, 10, 0, 0, 0, 9, 0, 0, 0, 0])
                    .expect("timestamp option should parse"),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );

        assert_eq!(
            socket
                .pending_ack_options(TCP_INITIAL_RTO_NANOS + 2_000_000)
                .timestamp(),
            Some(TcpTimestampOption {
                value: timestamp_value(TCP_INITIAL_RTO_NANOS + 2_000_000),
                echo_reply: 9,
            })
        );
    }

    #[test]
    fn receive_window_update_ack_is_thresholded() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );
        socket.mark_ack_queued();

        let payload = [0u8; TCP_RECEIVE_SEGMENT_BYTES];
        let queued_segments =
            MAX_TCP_RECEIVE_SEGMENTS - (u16::MAX as usize / TCP_RECEIVE_SEGMENT_BYTES) + 4;
        for index in 0..queued_segments {
            let _ = socket.on_segment(
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101 + index as u32 * TCP_RECEIVE_SEGMENT_BYTES as u32,
                    acknowledgement: 8,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload: &payload,
                },
                TCP_INITIAL_RTO_NANOS + index as u64 + 1,
            );
        }
        socket.mark_ack_queued();

        let update_segments = (usize::from(TCP_WINDOW_UPDATE_BYTES) << TCP_LOCAL_WINDOW_SCALE)
            / TCP_RECEIVE_SEGMENT_BYTES;
        for _ in 1..update_segments {
            assert_eq!(
                socket.receive(TCP_RECEIVE_SEGMENT_BYTES).as_deref(),
                Some(&payload[..])
            );
            assert_eq!(socket.pending_ack(), None);
        }
        assert_eq!(
            socket.receive(TCP_RECEIVE_SEGMENT_BYTES).as_deref(),
            Some(&payload[..])
        );
        assert!(socket.pending_ack().is_some());
    }
}
