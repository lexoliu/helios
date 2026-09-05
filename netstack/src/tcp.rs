//! TCP connection state machine.
//!
//! # Segmentation is a framing decision, not a protocol one
//!
//! A socket always accounts for what it sends at MSS granularity: one
//! [`TcpInFlightSegment`] per MSS-sized segment, one congestion-control
//! `on_packet_sent` per MSS-sized segment, and retransmission, SACK,
//! and loss recovery addressing individual MSS-sized sequence ranges.
//!
//! When the interface segments in hardware,
//! [`TcpSocket::take_transmit_segment`] coalesces the run of MSS-sized
//! segments it was about to emit into one oversized buffer and reports
//! the MSS in [`TcpTransmitSegment::segment_bytes`], so the frame
//! encoder can attach the device's segmentation metadata. The bytes,
//! the sequence numbers, and the bookkeeping are identical either way;
//! only the number of descriptors handed to the NIC changes. Nothing
//! below this module may assume a transmitted segment and an in-flight
//! record are one to one.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::VecDeque;

use bytes::{Bytes, BytesMut};
use heapless::{Deque, Vec as HeapVec};

use crate::{
    AckSample, CongestionControl, CongestionEvent, IpAddress, RecoveryAction, TcpFlags, TcpHeader,
    TcpHeaderOptions, TcpPacket, TcpReceiveBuffer, TcpSackBlock, TcpSackBlocks, TcpTimestampOption,
};

pub(crate) const TCP_RECEIVE_SEGMENT_BYTES: usize = 1460;
pub const TCP_RECEIVE_WINDOW_BYTES: usize = 1024 * 1024;
pub const MAX_TCP_RECEIVE_SEGMENTS: usize =
    TCP_RECEIVE_WINDOW_BYTES.div_ceil(TCP_RECEIVE_SEGMENT_BYTES);
pub const MAX_TCP_OUT_OF_ORDER_SEGMENTS: usize = 32;
pub const MAX_TCP_QUEUED_SEGMENTS: usize = 32;
/// In-flight (sent, unacknowledged) segment capacity. Sized so a full
/// transmit window of MSS-sized segments fits: the old 32-segment cap
/// (about 46 KiB) sat below a plain 64 KiB peer window and the first
/// bulk upload overflowed it.
pub const MAX_TCP_IN_FLIGHT_SEGMENTS: usize = MAX_TCP_RECEIVE_SEGMENTS;
pub const TCP_TRANSMIT_BUFFER_BYTES: usize = TCP_RECEIVE_WINDOW_BYTES;
// Local divan `tcp_ack_discards_in_flight_segments` and
// `tcp_single_segment_transmit` keep this reservation tied to measured stream
// shape rather than a round capacity. The 10-segment reservation covers the
// current bulk transmit bench without growth and moved first/bulk in-flight
// allocation from 2.048 KiB to 1.28 KiB versus 16 reserved segments.
const TCP_QUEUED_INITIAL_SEGMENTS: usize = 10;
// Local divan `tcp_receive_contiguous_read` showed that 128 KiB receive
// coalescing plus a small segmented queue beats the old 64 KiB/full-window
// queue metadata path: 23/64/128 KiB medians moved from
// 11.02/10.94/12.72 us to 7.875/7.208/7.214 us, and per-iteration
// allocation dropped from 121.4 KB to about 65.76 KB.
const TCP_RECEIVE_COALESCE_BYTES: usize = 128 * 1024;
const TCP_RECEIVE_INLINE_SEGMENTS: usize = 4;
const TCP_RECEIVE_OVERFLOW_INITIAL_SEGMENTS: usize = 64;
// Throughput beats zero-copy purity here: divan showed a 32 KiB first
// allocation keeps receive coalescing faster without the old 64 KiB burst tax.
const TCP_RECEIVE_BYTES: usize = MAX_TCP_RECEIVE_SEGMENTS * TCP_RECEIVE_SEGMENT_BYTES;
const TCP_RECEIVE_BACKPRESSURE_BYTES: usize =
    TCP_RECEIVE_BACKPRESSURE_SEGMENTS * TCP_RECEIVE_SEGMENT_BYTES;
const TCP_SMALL_PAYLOAD_ACK_BYTES: usize = TCP_RECEIVE_SEGMENT_BYTES / 2;
const TCP_DELAYED_ACK_SEGMENTS: u8 = 2;
const TCP_WINDOW_UPDATE_BYTES: u16 = (TCP_RECEIVE_SEGMENT_BYTES * 4) as u16;
/// Acknowledgements a socket may owe at once.
///
/// The stack is driven in batches: a whole burst of received frames is
/// demultiplexed before the transmit side runs, so the count of ACKs a
/// socket owes has to survive the batch. RFC 5681 3.2 requires an
/// immediate duplicate ACK for every out-of-order segment, and the peer
/// needs three of them before it will fast-retransmit; a single pending
/// flag collapses the whole burst into one ACK and leaves the peer with
/// nothing to do but wait out its retransmission timeout. One cumulative
/// ACK plus four duplicates clears that threshold with a spare, and stays
/// far below the outbound frame slab so a lossy burst cannot starve data
/// transmission.
const TCP_MAX_QUEUED_ACKS: u8 = 5;
pub(crate) const TCP_LOCAL_WINDOW_SCALE: u8 = 4;
const TCP_MAX_WINDOW_SCALE: u8 = 14;
pub(crate) const TCP_RECEIVE_BACKPRESSURE_SEGMENTS: usize = MAX_TCP_RECEIVE_SEGMENTS - 4;
pub const TCP_INITIAL_RTO_NANOS: u64 = 1_000_000_000;
pub const TCP_MIN_RTO_NANOS: u64 = 200_000_000;
pub const TCP_MAX_RTO_NANOS: u64 = 60_000_000_000;
pub const TCP_MAX_RETRANSMISSIONS: u8 = 5;
pub const TCP_DELAYED_ACK_NANOS: u64 = 40_000_000;
pub const TCP_TIME_WAIT_NANOS: u64 = 60_000_000_000;

/// How much of the transmit queue a socket may put into one frame.
///
/// Both budgets are TCP header-plus-payload bytes, measured from the
/// same place: `wire_segment` is what one packet on the wire may carry,
/// and `coalesced` is what one frame handed to a segmenting interface
/// may carry before the device splits it back into wire segments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpSegmentBudget {
    wire_segment: usize,
    coalesced: usize,
}

impl TcpSegmentBudget {
    /// An interface that segments nothing: every frame is one wire
    /// segment.
    pub const fn wire_segments(wire_segment: usize) -> Self {
        Self {
            wire_segment,
            coalesced: wire_segment,
        }
    }

    /// An interface that splits an oversized frame into `wire_segment`
    /// sized packets, accepting `coalesced` bytes per frame.
    pub const fn coalesced(wire_segment: usize, coalesced: usize) -> Self {
        Self {
            wire_segment,
            coalesced,
        }
    }

    /// TCP header-plus-payload bytes of one packet on the wire.
    /// Retransmission is always framed one wire segment at a time.
    pub const fn wire_segment(self) -> usize {
        self.wire_segment
    }
}

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
    /// Payload bytes the peer sees per wire segment.
    ///
    /// Equal to `payload.len()` for everything the socket emits at MSS
    /// granularity. A transmit-time coalesced burst carries several
    /// MSS-sized segments in one buffer and reports the MSS here, so
    /// the frame encoder can hand the device the segmentation metadata
    /// it needs to split the burst back up. Retransmission and loss
    /// recovery never see a burst: the in-flight queue records one
    /// entry per MSS-sized segment regardless of how they were framed.
    pub segment_bytes: u32,
    pub retransmission: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpSegmentOutcome {
    pub recovery: Option<RecoveryAction>,
    pub readable: bool,
    pub receive_backpressure: bool,
    pub reset: Option<TcpReset>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpReset {
    pub local: TcpEndpoint,
    pub remote: TcpEndpoint,
    pub header: TcpHeader,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TcpPersistProbe {
    sequence: u32,
    sequence_len: u32,
    fin: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TcpOutOfOrderSegment {
    sequence: u32,
    payload: Bytes,
}

// Hot TCP sockets should pay for the state they actually touch, not for every
// possible TCP path up front. Keeping receive, out-of-order, transmit, and
// in-flight queues lazy moved tcp_first_listen from 5.496 KiB to 344 B.
#[derive(Clone, Debug)]
struct TcpOutOfOrderQueue {
    segments: Box<HeapVec<TcpOutOfOrderSegment, MAX_TCP_OUT_OF_ORDER_SEGMENTS>>,
}

impl TcpOutOfOrderQueue {
    fn new() -> Self {
        Self {
            segments: Box::new(HeapVec::new()),
        }
    }

    fn first(&self) -> Option<&TcpOutOfOrderSegment> {
        self.segments.first()
    }

    fn push(&mut self, segment: TcpOutOfOrderSegment) -> Result<(), TcpOutOfOrderSegment> {
        self.segments.push(segment)
    }

    fn remove(&mut self, index: usize) -> TcpOutOfOrderSegment {
        self.segments.remove(index)
    }

    fn len(&self) -> usize {
        self.segments.len()
    }

    fn swap(&mut self, left: usize, right: usize) {
        self.segments.swap(left, right);
    }

    fn iter(&self) -> impl Iterator<Item = &TcpOutOfOrderSegment> {
        self.segments.iter()
    }
}

impl core::ops::Index<usize> for TcpOutOfOrderQueue {
    type Output = TcpOutOfOrderSegment;

    fn index(&self, index: usize) -> &Self::Output {
        &self.segments[index]
    }
}

#[derive(Clone, Debug)]
struct TcpTransmitQueue {
    // Sequential writes normally hold only the unsent tail after splitting a
    // Bytes buffer by MSS. Keeping that single element inline moved local
    // divan `tcp_send_bytes_segments_without_copy` from 2 allocations / 2.56
    // KiB to 1 allocation / 2.048 KiB, with median 414.2 ns -> 409.9 ns.
    front: Option<Bytes>,
    overflow: Option<VecDeque<Bytes>>,
    len: usize,
    queued_bytes: usize,
}

impl TcpTransmitQueue {
    fn new() -> Self {
        Self {
            front: None,
            overflow: None,
            len: 0,
            queued_bytes: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn is_full(&self) -> bool {
        self.len >= MAX_TCP_QUEUED_SEGMENTS || self.queued_bytes >= TCP_TRANSMIT_BUFFER_BYTES
    }

    fn front(&self) -> Option<&Bytes> {
        self.front.as_ref()
    }

    fn remaining_bytes_capacity(&self) -> usize {
        if self.len >= MAX_TCP_QUEUED_SEGMENTS {
            return 0;
        }
        TCP_TRANSMIT_BUFFER_BYTES.saturating_sub(self.queued_bytes)
    }

    fn push_back(&mut self, bytes: Bytes) -> Result<(), Bytes> {
        assert!(
            !bytes.is_empty(),
            "TCP transmit queue cannot store empty bytes"
        );
        let len = bytes.len();
        if self.is_full() || len > self.remaining_bytes_capacity() {
            return Err(bytes);
        }
        if self.front.is_none() {
            self.front = Some(bytes);
        } else {
            self.overflow_mut().push_back(bytes);
        }
        self.len = self
            .len
            .checked_add(1)
            .expect("TCP transmit queue segment count overflowed");
        self.queued_bytes = self
            .queued_bytes
            .checked_add(len)
            .expect("TCP transmit queued byte count overflowed");
        Ok(())
    }

    fn push_front(&mut self, bytes: Bytes) -> Result<(), Bytes> {
        assert!(
            !bytes.is_empty(),
            "TCP transmit queue cannot store empty bytes"
        );
        let len = bytes.len();
        if self.is_full() || len > self.remaining_bytes_capacity() {
            return Err(bytes);
        }
        if let Some(front) = self.front.replace(bytes) {
            self.overflow_mut().push_front(front);
        }
        self.len = self
            .len
            .checked_add(1)
            .expect("TCP transmit queue segment count overflowed");
        self.queued_bytes = self
            .queued_bytes
            .checked_add(len)
            .expect("TCP transmit queued byte count overflowed");
        Ok(())
    }

    fn pop_front(&mut self) -> Option<Bytes> {
        let bytes = self.front.take()?;
        self.len -= 1;
        assert!(
            self.queued_bytes >= bytes.len(),
            "TCP transmit queued byte count is corrupt"
        );
        self.queued_bytes -= bytes.len();
        self.front = self.overflow.as_mut().and_then(VecDeque::pop_front);
        if self.overflow.as_ref().is_some_and(VecDeque::is_empty) {
            self.overflow = None;
        }
        Some(bytes)
    }

    fn discard_front_bytes(&mut self, bytes: usize) {
        assert!(bytes != 0, "TCP transmit queue cannot discard zero bytes");
        let mut remaining = bytes;
        while remaining != 0 {
            let mut front = self
                .pop_front()
                .expect("TCP transmit queue lost persist-probed bytes");
            if front.len() > remaining {
                let _ = front.split_to(remaining);
                self.push_front(front).unwrap_or_else(|_| {
                    panic!("TCP transmit queue lost capacity while discarding probe bytes")
                });
                return;
            }
            remaining -= front.len();
        }
    }

    fn overflow_mut(&mut self) -> &mut VecDeque<Bytes> {
        self.overflow
            .get_or_insert_with(|| VecDeque::with_capacity(TCP_QUEUED_INITIAL_SEGMENTS))
    }
}

/// A bounded segment queue could not accept a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpQueueFull;

#[derive(Clone, Debug)]
struct TcpInFlightQueue {
    segments: VecDeque<TcpInFlightSegment>,
}

impl TcpInFlightQueue {
    fn new() -> Self {
        Self {
            segments: VecDeque::with_capacity(TCP_QUEUED_INITIAL_SEGMENTS),
        }
    }

    fn front(&self) -> Option<&TcpInFlightSegment> {
        self.segments.front()
    }

    fn front_mut(&mut self) -> Option<&mut TcpInFlightSegment> {
        self.segments.front_mut()
    }

    /// Appends a sent segment, or reports the queue full.
    ///
    /// A rejected segment is dropped rather than handed back: every
    /// caller gates on [`Self::is_full`] before it commits to sending,
    /// so a rejection here is a bug in that gate and nothing downstream
    /// wants the bytes returned.
    fn push_back(&mut self, segment: TcpInFlightSegment) -> Result<(), TcpQueueFull> {
        if self.is_full() {
            return Err(TcpQueueFull);
        }
        self.segments.push_back(segment);
        Ok(())
    }

    fn is_full(&self) -> bool {
        self.segments.len() >= MAX_TCP_IN_FLIGHT_SEGMENTS
    }

    fn pop_front(&mut self) -> Option<TcpInFlightSegment> {
        self.segments.pop_front()
    }

    fn clear(&mut self) {
        self.segments.clear();
    }

    fn iter(&self) -> impl Iterator<Item = &TcpInFlightSegment> {
        self.segments.iter()
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut TcpInFlightSegment> {
        self.segments.iter_mut()
    }

    /// Segment records the queue can still take before it is full.
    fn remaining_capacity(&self) -> usize {
        MAX_TCP_IN_FLIGHT_SEGMENTS.saturating_sub(self.segments.len())
    }
}

#[derive(Clone, Debug)]
struct TcpReceiveQueue {
    inline: Box<Deque<BytesMut, TCP_RECEIVE_INLINE_SEGMENTS>>,
    overflow: Option<VecDeque<BytesMut>>,
    len: usize,
}

impl TcpReceiveQueue {
    fn new() -> Self {
        Self {
            inline: Box::new(Deque::new()),
            overflow: None,
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn is_full(&self) -> bool {
        self.len >= MAX_TCP_RECEIVE_SEGMENTS
    }

    fn back_mut(&mut self) -> Option<&mut BytesMut> {
        if let Some(back) = self.overflow.as_mut().and_then(VecDeque::back_mut) {
            return Some(back);
        }
        self.inline.back_mut()
    }

    fn push_back(&mut self, bytes: BytesMut) -> Result<(), BytesMut> {
        if self.is_full() {
            return Err(bytes);
        }
        if let Some(overflow) = self.overflow.as_mut() {
            overflow.push_back(bytes);
            self.len += 1;
            return Ok(());
        }
        match self.inline.push_back(bytes) {
            Ok(()) => {
                self.len += 1;
                Ok(())
            }
            Err(bytes) => {
                let mut overflow = VecDeque::with_capacity(TCP_RECEIVE_OVERFLOW_INITIAL_SEGMENTS);
                overflow.push_back(bytes);
                self.overflow = Some(overflow);
                self.len += 1;
                Ok(())
            }
        }
    }

    fn push_front(&mut self, bytes: BytesMut) -> Result<(), BytesMut> {
        if self.is_full() {
            return Err(bytes);
        }
        match self.inline.push_front(bytes) {
            Ok(()) => {
                self.len += 1;
                Ok(())
            }
            Err(bytes) => {
                let moved = self
                    .inline
                    .pop_back()
                    .expect("full TCP receive inline queue had no tail segment");
                let overflow = self.overflow.get_or_insert_with(|| {
                    VecDeque::with_capacity(TCP_RECEIVE_OVERFLOW_INITIAL_SEGMENTS)
                });
                overflow.push_front(moved);
                self.inline.push_front(bytes).unwrap_or_else(|_| {
                    panic!("TCP receive inline queue stayed full after moving tail to overflow")
                });
                self.len += 1;
                Ok(())
            }
        }
    }

    fn pop_front(&mut self) -> Option<BytesMut> {
        if let Some(bytes) = self.inline.pop_front() {
            self.len -= 1;
            return Some(bytes);
        }
        let overflow = self.overflow.as_mut()?;
        let bytes = overflow.pop_front();
        if bytes.is_some() {
            self.len -= 1;
        }
        if overflow.is_empty() {
            self.overflow = None;
        }
        bytes
    }
}

#[cfg(test)]
impl TcpReceiveQueue {
    fn len(&self) -> usize {
        self.len
    }

    fn overflow_len(&self) -> usize {
        self.overflow.as_ref().map_or(0, VecDeque::len)
    }
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
struct TcpPersistTimer {
    deadline_nanos: u64,
    probes_sent: u8,
}

impl TcpPersistTimer {
    fn new(now_nanos: u64, retransmission_timer: TcpRetransmissionTimer) -> Self {
        Self {
            deadline_nanos: now_nanos.saturating_add(retransmission_timer.timeout_nanos()),
            probes_sent: 0,
        }
    }

    const fn deadline_nanos(self) -> u64 {
        self.deadline_nanos
    }

    fn note_probe_sent(&mut self, now_nanos: u64, retransmission_timer: TcpRetransmissionTimer) {
        self.probes_sent = self.probes_sent.saturating_add(1);
        self.deadline_nanos = now_nanos
            .saturating_add(retransmission_timer.backed_off_timeout_nanos(self.probes_sent));
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
    /// Set by every transition into [`TcpState::Closed`], from the reason
    /// the transition carried, and cleared by every transition out of it.
    /// A reader holding a closed connection asks this whether the stream
    /// ended or the connection did.
    close_kind: Option<TcpCloseKind>,
    congestion: C,
    send_next: u32,
    send_unacknowledged: u32,
    receive_next: u32,
    advertised_window: u16,
    peer_max_segment_size: usize,
    /// Scale applied to our advertised window. Zero until the handshake
    /// proves the peer sent a window-scale option; RFC 7323 only permits
    /// scaling when both SYNs carried the option.
    local_window_scale: u8,
    peer_window_scale: u8,
    peer_receive_window: u32,
    peer_sack_permitted: bool,
    peer_timestamp: Option<TcpTimestampOption>,
    receive_queue: Option<TcpReceiveQueue>,
    receive_queued_bytes: usize,
    receive_fin_sequence: Option<u32>,
    out_of_order: Option<TcpOutOfOrderQueue>,
    out_of_order_queued_bytes: usize,
    transmit_queue: Option<TcpTransmitQueue>,
    in_flight: Option<TcpInFlightQueue>,
    bytes_in_flight: u32,
    delivered_bytes: u64,
    syn_queued: bool,
    syn_ack_queued: bool,
    fin_queued: bool,
    /// Acknowledgements this socket owes its peer. Counted rather than
    /// flagged so a batch of out-of-order segments produces the
    /// duplicate ACKs that drive the peer's fast retransmit.
    pending_acks: u8,
    duplicate_ack_count: u8,
    fast_retransmit_pending: bool,
    pending_pmtu_retransmit_sequence: Option<u32>,
    retransmission_timer: TcpRetransmissionTimer,
    persist_timer: Option<TcpPersistTimer>,
    persist_probe: Option<TcpPersistProbe>,
    next_pacing_send_nanos: Option<u64>,
    delayed_ack_deadline_nanos: Option<u64>,
    time_wait_deadline_nanos: Option<u64>,
    unacked_receive_segments: u8,
    pending_window_update_bytes: u32,
    /// IPv4 TTL / IPv6 hop limit stamped on every frame this socket emits.
    hop_limit: u8,
}

/// Why a [`TcpSocket`] moved from one [`TcpState`] to another.
///
/// Every transition records one of these, so a packet capture can be
/// matched against the route the state machine actually took instead of
/// the route its output shape suggests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpStateChangeReason {
    /// A listening socket was opened.
    Listen,
    /// An active open queued its SYN.
    Connect,
    /// A listener's child socket was created for an inbound SYN.
    Accept,
    /// The `TIME-WAIT` 2MSL timer elapsed.
    TimeWaitExpired,
    /// A segment exceeded [`TCP_MAX_RETRANSMISSIONS`] retransmissions.
    RetransmissionLimit,
    /// `SYN-SENT` received an acceptable RST.
    SynSentReset,
    /// `SYN-SENT` received an acceptable SYN-ACK.
    SynSentEstablished,
    /// `SYN-RECEIVED` received an in-window RST.
    SynReceivedReset,
    /// `SYN-RECEIVED` received the ACK completing the handshake.
    SynReceivedEstablished,
    /// A synchronised state received an in-window RST.
    PeerReset,
    /// A synchronised state received an in-window SYN.
    PeerSyn,
    /// The peer's FIN was consumed in sequence.
    PeerFin,
    /// The application closed its send side.
    LocalClose,
    /// The peer acknowledged our FIN.
    FinAcknowledged,
    /// The socket was aborted locally.
    Abort,
}

/// How a connection that reached [`TcpState::Closed`] ended, from the
/// point of view of an application still holding it.
///
/// [`TcpStateChangeReason`] records the route the state machine took;
/// this is the part of that route a reader has to act on, and it is the
/// only distinction the stack's callers need to tell an exhausted stream
/// from a broken connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpCloseKind {
    /// The connection ended in order: the peer's FIN arrived in sequence
    /// behind everything it sent, so the reader has the whole stream.
    Graceful,
    /// The connection was reset — an in-window RST, or the in-window SYN
    /// that RFC 9293 3.10.7.4 answers with one.
    Reset,
    /// The local side aborted the connection.
    Aborted,
    /// The peer stopped acknowledging and the retransmission limit ran
    /// out with data still unacknowledged.
    Unresponsive,
}

impl TcpStateChangeReason {
    /// How a socket that entered [`TcpState::Closed`] through this
    /// transition reads to the application holding it.
    ///
    /// `None` for the reasons that only open or advance a connection;
    /// those never name a transition into `Closed`.
    pub const fn close_kind(self) -> Option<TcpCloseKind> {
        match self {
            Self::TimeWaitExpired | Self::PeerFin | Self::LocalClose | Self::FinAcknowledged => {
                Some(TcpCloseKind::Graceful)
            }
            Self::SynSentReset | Self::SynReceivedReset | Self::PeerReset | Self::PeerSyn => {
                Some(TcpCloseKind::Reset)
            }
            Self::Abort => Some(TcpCloseKind::Aborted),
            Self::RetransmissionLimit => Some(TcpCloseKind::Unresponsive),
            Self::Listen
            | Self::Connect
            | Self::Accept
            | Self::SynSentEstablished
            | Self::SynReceivedEstablished => None,
        }
    }
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
            close_kind: None,
            congestion,
            send_next: 0,
            send_unacknowledged: 0,
            receive_next: 0,
            advertised_window: receive_window_size(0, 0),
            peer_max_segment_size: TCP_RECEIVE_SEGMENT_BYTES,
            local_window_scale: 0,
            peer_window_scale: 0,
            peer_receive_window: u32::from(u16::MAX),
            peer_sack_permitted: false,
            peer_timestamp: None,
            receive_queue: None,
            receive_queued_bytes: 0,
            receive_fin_sequence: None,
            out_of_order: None,
            out_of_order_queued_bytes: 0,
            transmit_queue: None,
            in_flight: None,
            bytes_in_flight: 0,
            delivered_bytes: 0,
            syn_queued: false,
            syn_ack_queued: false,
            fin_queued: false,
            pending_acks: 0,
            duplicate_ack_count: 0,
            fast_retransmit_pending: false,
            pending_pmtu_retransmit_sequence: None,
            retransmission_timer: TcpRetransmissionTimer::new(),
            persist_timer: None,
            persist_probe: None,
            next_pacing_send_nanos: None,
            delayed_ack_deadline_nanos: None,
            time_wait_deadline_nanos: None,
            unacked_receive_segments: 0,
            pending_window_update_bytes: 0,
            hop_limit: crate::DEFAULT_HOP_LIMIT,
        }
    }

    /// IPv4 TTL / IPv6 hop limit this socket stamps on outgoing frames.
    pub const fn hop_limit(&self) -> u8 {
        self.hop_limit
    }

    /// Sets the TTL / hop limit for frames this socket emits from now on.
    pub const fn set_hop_limit(&mut self, hop_limit: u8) {
        self.hop_limit = hop_limit;
    }

    pub fn listen(local: TcpEndpoint, congestion: C) -> Self {
        let mut socket = Self::closed(congestion);
        socket.local = Some(local);
        socket.set_state(TcpState::Listen, TcpStateChangeReason::Listen);
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
        socket.set_state(TcpState::SynSent, TcpStateChangeReason::Connect);
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
        socket.set_state(TcpState::SynReceived, TcpStateChangeReason::Accept);
        socket.receive_next = receive_next;
        socket.send_next = initial_sequence.wrapping_add(1);
        socket.send_unacknowledged = initial_sequence;
        socket
    }

    pub const fn state(&self) -> TcpState {
        self.state
    }

    /// How this connection ended, once it reached [`TcpState::Closed`]
    /// through a transition.
    ///
    /// `None` while the connection is live, and for a socket that has
    /// never left the `Closed` it was constructed in.
    pub const fn close_kind(&self) -> Option<TcpCloseKind> {
        self.close_kind
    }

    /// The single place [`TcpSocket::state`] changes.
    ///
    /// Routing every transition through here keeps the debug-level
    /// record complete: a socket that leaves the stack's endpoint index
    /// has, by construction, logged the reason it reached
    /// [`TcpState::Closed`].
    fn set_state(&mut self, next: TcpState, reason: TcpStateChangeReason) {
        let previous = self.state;
        if previous == next {
            return;
        }
        self.state = next;
        self.close_kind = if next == TcpState::Closed {
            Some(reason.close_kind().unwrap_or_else(|| {
                panic!("TCP transition into Closed carried the non-terminal reason {reason:?}")
            }))
        } else {
            None
        };
        tracing::debug!(
            ?previous,
            ?next,
            ?reason,
            local = ?self.local,
            remote = ?self.remote,
            send_unacknowledged = self.send_unacknowledged,
            send_next = self.send_next,
            receive_next = self.receive_next,
            bytes_in_flight = self.bytes_in_flight,
            "TCP state change"
        );
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
            // The window field in a SYN segment is never scaled.
            window_size: receive_window_size(self.receive_buffered_bytes(), 0),
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
            // The window field in a SYN segment is never scaled.
            window_size: receive_window_size(self.receive_buffered_bytes(), 0),
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
        self.in_flight_mut()
            .push_back(TcpInFlightSegment {
                header,
                options: syn_header_options(now_nanos, None),
                payload: Bytes::new(),
                sequence_len: 1,
                sent_at_nanos: now_nanos,
                retransmissions: 0,
                sacked: false,
            })
            .expect("TCP in-flight queue is full");
        self.congestion
            .on_packet_sent(1, self.bytes_in_flight, now_nanos);
    }

    pub fn mark_syn_ack_queued(&mut self, now_nanos: u64) {
        assert!(
            self.state == TcpState::SynReceived,
            "SYN-ACK can only be queued for a half-open accepted socket"
        );
        let header = self.pending_syn_ack().expect("SYN-ACK header disappeared");
        let options = syn_header_options(
            now_nanos,
            self.peer_timestamp.map(|timestamp| timestamp.value),
        );
        self.syn_ack_queued = true;
        self.bytes_in_flight = self.bytes_in_flight.saturating_add(1);
        self.in_flight_mut()
            .push_back(TcpInFlightSegment {
                header,
                options,
                payload: Bytes::new(),
                sequence_len: 1,
                sent_at_nanos: now_nanos,
                retransmissions: 0,
                sacked: false,
            })
            .expect("TCP in-flight queue is full");
        self.congestion
            .on_packet_sent(1, self.bytes_in_flight, now_nanos);
    }

    /// Acknowledgements this socket still owes its peer.
    ///
    /// Everything past the first is a duplicate ACK: the receive
    /// sequence has not moved, so the peer reads the repetition as the
    /// loss signal it needs.
    pub const fn pending_ack_count(&self) -> u8 {
        self.pending_acks
    }

    pub fn pending_ack(&self) -> Option<TcpHeader> {
        if self.pending_acks == 0 {
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

    /// Consumes one owed acknowledgement. A frame carries exactly one,
    /// so a socket owing duplicates stays pending until each has been
    /// put on the wire.
    pub fn mark_ack_queued(&mut self) {
        self.pending_acks = self.pending_acks.saturating_sub(1);
        self.delayed_ack_deadline_nanos = None;
    }

    pub fn pending_ack_options(&self, now_nanos: u64) -> TcpHeaderOptions {
        let mut blocks = TcpSackBlocks::empty();
        if self.peer_sack_permitted
            && let Some(out_of_order) = self.out_of_order.as_ref()
        {
            for segment in out_of_order.iter() {
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

    /// Scale applied to our advertised window: zero unless the peer's
    /// SYN negotiated window scaling.
    pub const fn local_window_scale(&self) -> u8 {
        self.local_window_scale
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

    /// Bytes `queue_send_bytes` would accept right now. Zero when the
    /// transmit queue is full, so readiness reporting can avoid claiming
    /// writability that the next write would reject.
    pub fn send_capacity_bytes(&self) -> usize {
        self.transmit_queue.as_ref().map_or(
            TCP_TRANSMIT_BUFFER_BYTES,
            TcpTransmitQueue::remaining_bytes_capacity,
        )
    }

    pub fn queue_send_bytes(&mut self, bytes: &mut Bytes) -> usize {
        if !matches!(self.state, TcpState::Established | TcpState::CloseWait) {
            return 0;
        }
        let writable = bytes.len().min(self.transmit_queue.as_ref().map_or(
            TCP_TRANSMIT_BUFFER_BYTES,
            TcpTransmitQueue::remaining_bytes_capacity,
        ));
        if writable != 0 {
            let segment = bytes.split_to(writable);
            self.transmit_queue_mut()
                .push_back(segment)
                .unwrap_or_else(|_| {
                    panic!("TCP transmit queue reported full after capacity check")
                });
        }
        writable
    }

    /// Takes the next segment this socket wants on the wire.
    ///
    /// `budget` says what one frame may carry: a
    /// [`TcpSegmentBudget::wire_segments`] budget keeps the socket
    /// emitting one MSS-sized segment per call, and a
    /// [`TcpSegmentBudget::coalesced`] one lets it hand a segmenting
    /// interface a whole run of them at once. Whatever the framing, the
    /// in-flight queue records one entry per MSS-sized segment, so
    /// retransmission, SACK, and loss recovery keep addressing
    /// individual segments.
    pub fn take_transmit_segment(
        &mut self,
        now_nanos: u64,
        budget: TcpSegmentBudget,
    ) -> Option<TcpTransmitSegment> {
        self.arm_persist_timer_if_needed(now_nanos);
        let local = self.local?;
        let remote = self.remote?;
        let persist_probe = self.pending_persist_probe(now_nanos);
        let available_window = if persist_probe.is_some() {
            1
        } else {
            self.available_send_window()
        };
        if available_window == 0 {
            return None;
        }
        // A full in-flight queue closes the effective send window until
        // ACKs drain it; emitting another segment would have nowhere to
        // record its retransmission state.
        if persist_probe.is_none()
            && self
                .in_flight
                .as_ref()
                .is_some_and(TcpInFlightQueue::is_full)
        {
            return None;
        }
        if matches!(
            self.state,
            TcpState::Established | TcpState::CloseWait | TcpState::FinWait1 | TcpState::LastAck
        ) && !self.fin_queued
            && let Some(segment) = self.take_data_segment(
                local,
                remote,
                available_window,
                budget,
                now_nanos,
                persist_probe,
            )
        {
            return Some(segment);
        }

        if !self.transmit_queue_is_empty() {
            return None;
        }

        if !matches!(self.state, TcpState::FinWait1 | TcpState::LastAck) || self.fin_queued {
            return None;
        }

        if let Some(persist_probe) = persist_probe.filter(|probe| probe.fin) {
            return self.take_fin_segment(local, remote, now_nanos, true, persist_probe.sequence);
        }

        if self.peer_receive_window == 0 {
            self.arm_persist_timer_if_needed(now_nanos);
            return None;
        }

        self.take_fin_segment(local, remote, now_nanos, false, self.send_next)
    }

    fn take_fin_segment(
        &mut self,
        local: TcpEndpoint,
        remote: TcpEndpoint,
        now_nanos: u64,
        persist_probe: bool,
        sequence: u32,
    ) -> Option<TcpTransmitSegment> {
        let header = TcpHeader {
            source_port: local.port,
            destination_port: remote.port,
            sequence,
            acknowledgement: self.receive_next,
            flags: TcpFlags::ACK.union(TcpFlags::FIN),
            window_size: self.advertised_window,
        };
        let options = self.timestamped_options(now_nanos);
        self.pending_acks = self.pending_acks.saturating_sub(1);
        if persist_probe {
            self.mark_persist_probe_queued(
                TcpPersistProbe {
                    sequence,
                    sequence_len: 1,
                    fin: true,
                },
                now_nanos,
            );
        } else {
            self.fin_queued = true;
            self.send_next = self.send_next.wrapping_add(1);
            self.bytes_in_flight = self.bytes_in_flight.saturating_add(1);
            self.persist_timer = None;
            self.congestion
                .on_packet_sent(1, self.bytes_in_flight, now_nanos);
            self.in_flight_mut()
                .push_back(TcpInFlightSegment {
                    header,
                    options,
                    payload: Bytes::new(),
                    sequence_len: 1,
                    sent_at_nanos: now_nanos,
                    retransmissions: 0,
                    sacked: false,
                })
                .expect("TCP in-flight queue is full");
        }
        Some(TcpTransmitSegment {
            local,
            remote,
            header,
            options,
            payload: Bytes::new(),
            sequence_len: 1,
            segment_bytes: 0,
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
            self.set_state(TcpState::Closed, TcpStateChangeReason::TimeWaitExpired);
            self.time_wait_deadline_nanos = None;
        }
    }

    pub fn next_deadline_nanos(&self) -> Option<u64> {
        let rto_deadline = self.in_flight.as_ref().and_then(|in_flight| {
            in_flight.front().map(|segment| {
                segment.sent_at_nanos.saturating_add(
                    self.retransmission_timer
                        .backed_off_timeout_nanos(segment.retransmissions),
                )
            })
        });
        let pacing_deadline = self
            .next_pacing_send_nanos
            .filter(|_| !self.transmit_queue_is_empty());
        let persist_deadline = self.persist_timer.map(TcpPersistTimer::deadline_nanos);
        min_deadline(
            min_deadline(
                min_deadline(
                    min_deadline(rto_deadline, pacing_deadline),
                    persist_deadline,
                ),
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
        budget: TcpSegmentBudget,
        now_nanos: u64,
        persist_probe: Option<TcpPersistProbe>,
    ) -> Option<TcpTransmitSegment> {
        if self
            .next_pacing_send_nanos
            .is_some_and(|deadline| now_nanos < deadline)
            && persist_probe.is_none()
        {
            return None;
        }
        let options = self.timestamped_options(now_nanos);
        let tcp_header_len = TcpPacket::MIN_HEADER_LEN
            .checked_add(options.encoded_len())
            .expect("TCP options length overflowed");
        let payload_mtu = budget.wire_segment.checked_sub(tcp_header_len)?;
        if payload_mtu == 0 {
            return None;
        }
        // Coalescing is on only when the interface asked for a frame
        // wider than one wire segment; otherwise the socket emits
        // exactly one segment per call, as it always has.
        let coalesced_payload = (budget.coalesced > budget.wire_segment)
            .then(|| budget.coalesced.saturating_sub(tcp_header_len));
        let persist_probe = persist_probe.filter(|probe| !probe.fin);
        let is_persist_probe = persist_probe.is_some();
        let maximum_segment = available.min(self.peer_max_segment_size).min(payload_mtu);
        // Every MSS-sized segment inside a coalesced burst still needs
        // its own in-flight record, so the burst can never be longer
        // than the queue has room for.
        let in_flight_room = self.in_flight.as_ref().map_or(
            MAX_TCP_IN_FLIGHT_SEGMENTS,
            TcpInFlightQueue::remaining_capacity,
        );
        let burst_limit = match coalesced_payload.filter(|_| !is_persist_probe) {
            Some(coalesced_payload) => coalesced_payload
                .min(available)
                .min(maximum_segment.saturating_mul(in_flight_room))
                .max(maximum_segment),
            None => maximum_segment,
        };
        if burst_limit == 0 {
            return None;
        }
        let mut payload = if is_persist_probe {
            self.transmit_queue.as_ref()?.front()?.slice(..1)
        } else {
            self.transmit_queue.as_mut()?.pop_front()?
        };
        if payload.len() > burst_limit {
            let tail = payload.split_off(burst_limit);
            if !is_persist_probe {
                self.transmit_queue_mut()
                    .push_front(tail)
                    .unwrap_or_else(|_| panic!("TCP transmit queue lost capacity while splitting"));
            }
        }
        let payload_len = payload.len();
        let sequence_len = u32::try_from(payload_len).unwrap_or(u32::MAX);
        let sequence = persist_probe.map_or(self.send_next, |probe| probe.sequence);
        let header = TcpHeader {
            source_port: local.port,
            destination_port: remote.port,
            sequence,
            acknowledgement: self.receive_next,
            flags: TcpFlags::ACK.union(TcpFlags::PSH),
            window_size: self.advertised_window,
        };
        if is_persist_probe {
            self.mark_persist_probe_queued(
                TcpPersistProbe {
                    sequence,
                    sequence_len,
                    fin: false,
                },
                now_nanos,
            );
        } else {
            // Split the burst back into MSS-sized in-flight records.
            // Congestion control sees exactly the packet train the
            // device will put on the wire.
            let mut offset = 0usize;
            while offset < payload_len {
                let chunk_len = maximum_segment.min(payload_len - offset);
                let chunk_sequence_len = chunk_len as u32;
                let chunk_header = TcpHeader {
                    sequence: sequence.wrapping_add(offset as u32),
                    ..header
                };
                self.send_next = self.send_next.wrapping_add(chunk_sequence_len);
                self.bytes_in_flight = self.bytes_in_flight.saturating_add(chunk_sequence_len);
                self.congestion
                    .on_packet_sent(chunk_sequence_len, self.bytes_in_flight, now_nanos);
                self.in_flight_mut()
                    .push_back(TcpInFlightSegment {
                        header: chunk_header,
                        options,
                        payload: payload.slice(offset..offset + chunk_len),
                        sequence_len: chunk_sequence_len,
                        sent_at_nanos: now_nanos,
                        retransmissions: 0,
                        sacked: false,
                    })
                    .expect("TCP in-flight queue is full");
                offset += chunk_len;
            }
            self.schedule_next_pacing_send(sequence_len, now_nanos);
        }
        self.pending_acks = self.pending_acks.saturating_sub(1);
        self.delayed_ack_deadline_nanos = None;
        Some(TcpTransmitSegment {
            local,
            remote,
            header,
            options,
            payload,
            sequence_len,
            segment_bytes: maximum_segment as u32,
            retransmission: false,
        })
    }

    pub fn pending_retransmission(&self, now_nanos: u64) -> Option<TcpTransmitSegment> {
        self.pending_retransmission_with_mtu(now_nanos, usize::MAX)
    }

    pub fn pending_retransmission_with_mtu(
        &self,
        now_nanos: u64,
        tcp_segment_mtu: usize,
    ) -> Option<TcpTransmitSegment> {
        let in_flight = if self.fast_retransmit_pending {
            self.first_unsacked_in_flight()?
        } else if let Some(sequence) = self.pending_pmtu_retransmit_sequence {
            self.in_flight.as_ref()?.iter().find(|segment| {
                sequence_leq(segment.header.sequence, sequence)
                    && sequence_lt(sequence, segment_end(segment))
                    && !segment.sacked
            })?
        } else {
            let in_flight = self.in_flight.as_ref()?.front()?;
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
        let payload_mtu = tcp_segment_mtu.checked_sub(
            TcpPacket::MIN_HEADER_LEN
                .checked_add(in_flight.options.encoded_len())
                .expect("TCP options length overflowed"),
        )?;
        if payload_mtu == 0 && !in_flight.payload.is_empty() {
            return None;
        }
        let payload_len = in_flight.payload.len().min(payload_mtu);
        let sequence_len = if in_flight.payload.is_empty() {
            in_flight.sequence_len
        } else {
            u32::try_from(payload_len).unwrap_or(u32::MAX)
        };
        Some(TcpTransmitSegment {
            local,
            remote,
            header: in_flight.header,
            options: in_flight.options,
            payload: in_flight.payload.slice(..payload_len),
            sequence_len,
            segment_bytes: payload_len as u32,
            retransmission: true,
        })
    }

    fn first_unsacked_in_flight(&self) -> Option<&TcpInFlightSegment> {
        self.in_flight
            .as_ref()?
            .iter()
            .find(|segment| !segment.sacked)
    }

    pub fn mark_retransmission_queued(&mut self, sequence: u32, now_nanos: u64) {
        let fast_retransmit = self.fast_retransmit_pending;
        if fast_retransmit {
            self.fast_retransmit_pending = false;
        }
        let pmtu_retransmit = self.pending_pmtu_retransmit_sequence == Some(sequence);
        if pmtu_retransmit {
            self.pending_pmtu_retransmit_sequence = None;
        }
        let Some(in_flight_queue) = self.in_flight.as_mut() else {
            return;
        };
        let Some(in_flight) = in_flight_queue
            .iter_mut()
            .find(|segment| segment.header.sequence == sequence)
        else {
            return;
        };
        in_flight.sent_at_nanos = now_nanos;
        in_flight.retransmissions = in_flight.retransmissions.saturating_add(1);
        if in_flight.retransmissions > TCP_MAX_RETRANSMISSIONS {
            self.set_state(TcpState::Closed, TcpStateChangeReason::RetransmissionLimit);
            self.bytes_in_flight = 0;
            if let Some(in_flight) = self.in_flight.as_mut() {
                in_flight.clear();
            }
            return;
        }
        if !fast_retransmit && !pmtu_retransmit {
            self.on_retransmission_timeout();
            self.congestion
                .on_congestion_event(CongestionEvent::RetransmissionTimeout { now_nanos });
        }
    }

    pub fn mark_path_mtu_reduced(&mut self, sequence: u32) {
        let Some(in_flight) = self.in_flight.as_ref() else {
            return;
        };
        if let Some(segment) = in_flight.iter().find(|segment| {
            sequence_leq(segment.header.sequence, sequence)
                && sequence_lt(sequence, segment_end(segment))
                && !segment.sacked
        }) {
            self.pending_pmtu_retransmit_sequence = Some(segment.header.sequence);
        }
    }

    pub fn receive(&mut self, max_bytes: usize, now_nanos: u64) -> Option<Bytes> {
        if max_bytes == 0 {
            return (!self.receive_queue_is_empty()).then(Bytes::new);
        }
        self.consume_pending_receive_fin(now_nanos);
        let previous_window = self.advertised_window;
        let mut bytes = self.pop_receive_segment()?;
        if bytes.len() >= max_bytes {
            if bytes.len() > max_bytes {
                let tail = bytes.split_off(max_bytes);
                self.push_front_receive_segment(tail);
            }
            self.complete_receive_drain(previous_window, now_nanos);
            return Some(bytes.freeze());
        }

        if self.receive_queue_is_empty() {
            self.complete_receive_drain(previous_window, now_nanos);
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
                bytes.unsplit(next);
                self.push_front_receive_segment(tail);
                break;
            }
            bytes.unsplit(next);
        }
        self.complete_receive_drain(previous_window, now_nanos);
        Some(bytes.freeze())
    }

    pub fn receive_into<S>(&mut self, sink: &mut S, now_nanos: u64) -> Option<usize>
    where
        S: TcpReceiveBuffer,
    {
        if sink.remaining_capacity() == 0 {
            return (!self.receive_queue_is_empty()).then_some(0);
        }
        self.consume_pending_receive_fin(now_nanos);
        let previous_window = self.advertised_window;
        let mut written_total = 0usize;

        loop {
            let Some(mut bytes) = self.pop_receive_segment() else {
                if written_total == 0 {
                    return None;
                }
                break;
            };
            let remaining_before = sink.remaining_capacity();
            let written = sink.write_from(bytes.as_ref());
            assert!(
                written <= bytes.len(),
                "TCP receive sink wrote more bytes than the source segment"
            );
            assert!(
                written != 0 || remaining_before == 0 || bytes.is_empty(),
                "TCP receive sink made no progress with available capacity"
            );
            written_total = written_total
                .checked_add(written)
                .expect("TCP receive sink byte count overflowed");
            if written < bytes.len() {
                let tail = bytes.split_off(written);
                self.push_front_receive_segment(tail);
                break;
            }
            if sink.remaining_capacity() == 0 || self.receive_queue_is_empty() {
                break;
            }
        }

        self.complete_receive_drain(previous_window, now_nanos);
        Some(written_total)
    }

    fn receive_merge_len(&self, first_len: usize, max_bytes: usize) -> usize {
        first_len
            .saturating_add(self.receive_queued_bytes)
            .min(max_bytes)
    }

    fn complete_receive_drain(&mut self, previous_window: u16, now_nanos: u64) {
        let drained_segments = self.drain_contiguous_out_of_order();
        self.consume_pending_receive_fin(now_nanos);
        self.refresh_advertised_window();
        if drained_segments != 0 || self.should_advertise_window_update(previous_window) {
            self.request_ack();
        }
    }

    pub fn on_segment(
        &mut self,
        packet: TcpPacket<'_>,
        payload_bytes: Bytes,
        now_nanos: u64,
    ) -> TcpSegmentOutcome {
        match self.state {
            TcpState::SynSent if packet.flags.contains(TcpFlags::RST) => {
                if !packet.flags.contains(TcpFlags::ACK)
                    || !self.syn_sent_ack_acceptable(packet.acknowledgement)
                {
                    return TcpSegmentOutcome::default();
                }
                self.set_state(TcpState::Closed, TcpStateChangeReason::SynSentReset);
                TcpSegmentOutcome {
                    recovery: Some(
                        self.congestion.on_congestion_event(
                            CongestionEvent::RetransmissionTimeout { now_nanos },
                        ),
                    ),
                    readable: false,
                    receive_backpressure: false,
                    reset: None,
                }
            }
            TcpState::SynSent if packet.flags.contains(TcpFlags::SYN.union(TcpFlags::ACK)) => {
                if !self.syn_sent_ack_acceptable(packet.acknowledgement) {
                    return TcpSegmentOutcome {
                        reset: self.unacceptable_ack_reset(packet.acknowledgement),
                        ..TcpSegmentOutcome::default()
                    };
                }
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
                    false,
                );
                self.set_state(
                    TcpState::Established,
                    TcpStateChangeReason::SynSentEstablished,
                );
                self.pending_acks = self.pending_acks.max(1);
                TcpSegmentOutcome {
                    recovery: action,
                    readable: false,
                    receive_backpressure: false,
                    reset: None,
                }
            }
            TcpState::SynReceived if packet.flags.contains(TcpFlags::RST) => {
                if self.receive_sequence_acceptable(packet.sequence, receive_sequence_len(packet)) {
                    self.set_state(TcpState::Closed, TcpStateChangeReason::SynReceivedReset);
                }
                TcpSegmentOutcome::default()
            }
            TcpState::SynReceived if packet.flags.contains(TcpFlags::ACK) => {
                if packet.acknowledgement != self.send_next {
                    return TcpSegmentOutcome {
                        reset: self.unacceptable_ack_reset(packet.acknowledgement),
                        ..TcpSegmentOutcome::default()
                    };
                }
                self.update_peer_receive_window(packet.window_size);
                self.record_peer_timestamp(packet.options.timestamp());
                let action = self.acknowledge_sent(
                    packet.acknowledgement,
                    now_nanos,
                    true,
                    packet.options.timestamp(),
                    false,
                );
                self.set_state(
                    TcpState::Established,
                    TcpStateChangeReason::SynReceivedEstablished,
                );
                TcpSegmentOutcome {
                    recovery: action,
                    readable: false,
                    receive_backpressure: false,
                    reset: None,
                }
            }
            TcpState::Established
            | TcpState::FinWait1
            | TcpState::FinWait2
            | TcpState::CloseWait
            | TcpState::Closing
            | TcpState::LastAck => {
                if packet.flags.contains(TcpFlags::RST) {
                    if self
                        .receive_sequence_acceptable(packet.sequence, receive_sequence_len(packet))
                    {
                        self.set_state(TcpState::Closed, TcpStateChangeReason::PeerReset);
                        return TcpSegmentOutcome {
                            recovery: Some(self.congestion.on_congestion_event(
                                CongestionEvent::RetransmissionTimeout { now_nanos },
                            )),
                            readable: false,
                            receive_backpressure: false,
                            reset: None,
                        };
                    }
                    return TcpSegmentOutcome::default();
                }
                if !packet.flags.contains(TcpFlags::ACK) {
                    return TcpSegmentOutcome::default();
                }
                if self.acknowledges_unsent_data(packet.acknowledgement) {
                    self.request_ack();
                    return TcpSegmentOutcome::default();
                }
                if !self.receive_sequence_acceptable(packet.sequence, receive_sequence_len(packet))
                {
                    // RFC 9293 3.10.7.4: every unacceptable segment is
                    // answered with an acknowledgement of its own.
                    self.request_duplicate_ack();
                    return TcpSegmentOutcome::default();
                }
                if packet.flags.contains(TcpFlags::SYN) {
                    self.set_state(TcpState::Closed, TcpStateChangeReason::PeerSyn);
                    return TcpSegmentOutcome {
                        recovery: Some(self.congestion.on_congestion_event(
                            CongestionEvent::RetransmissionTimeout { now_nanos },
                        )),
                        readable: false,
                        receive_backpressure: false,
                        reset: None,
                    };
                }
                let duplicate_ack_eligible = self.duplicate_ack_eligible(packet);
                let action = if sequence_lt(packet.acknowledgement, self.send_unacknowledged) {
                    None
                } else {
                    self.update_peer_receive_window(packet.window_size);
                    self.record_peer_timestamp(packet.options.timestamp());
                    self.apply_sack_blocks(packet.options.sack_blocks());
                    self.acknowledge_sent(
                        packet.acknowledgement,
                        now_nanos,
                        self.transmit_queue_is_empty(),
                        packet.options.timestamp(),
                        duplicate_ack_eligible,
                    )
                };
                let mut readable = false;
                if !packet.payload.is_empty()
                    && matches!(
                        self.state,
                        TcpState::Established | TcpState::FinWait1 | TcpState::FinWait2
                    )
                {
                    match self.receive_payload(packet.sequence, payload_bytes, now_nanos) {
                        Ok(payload_readable) => readable |= payload_readable,
                        Err(()) => {
                            self.refresh_advertised_window();
                            // The segment was dropped for want of queue
                            // space, so it is a hole to the peer just
                            // like a lost one.
                            self.request_duplicate_ack();
                            return TcpSegmentOutcome {
                                recovery: action,
                                readable: false,
                                receive_backpressure: true,
                                reset: None,
                            };
                        }
                    }
                }
                if packet.flags.contains(TcpFlags::FIN) {
                    readable |=
                        self.note_receive_fin(packet.sequence, packet.payload.len(), now_nanos);
                }
                TcpSegmentOutcome {
                    recovery: action,
                    readable,
                    receive_backpressure: false,
                    reset: None,
                }
            }
            TcpState::TimeWait => {
                if packet.flags.contains(TcpFlags::FIN)
                    && self
                        .receive_sequence_acceptable(packet.sequence, receive_sequence_len(packet))
                {
                    self.enter_time_wait(now_nanos);
                    self.request_ack();
                }
                TcpSegmentOutcome::default()
            }
            _ => TcpSegmentOutcome::default(),
        }
    }

    pub fn close_send(&mut self) {
        let next = match self.state {
            TcpState::Established => TcpState::FinWait1,
            TcpState::CloseWait => TcpState::LastAck,
            state => state,
        };
        self.set_state(next, TcpStateChangeReason::LocalClose);
    }

    pub fn abort(&mut self) {
        self.set_state(TcpState::Closed, TcpStateChangeReason::Abort);
        self.bytes_in_flight = 0;
        self.receive_queue = None;
        self.receive_queued_bytes = 0;
        self.receive_fin_sequence = None;
        self.out_of_order = None;
        self.out_of_order_queued_bytes = 0;
        self.transmit_queue = None;
        self.in_flight = None;
        self.syn_queued = false;
        self.syn_ack_queued = false;
        self.fin_queued = false;
        self.pending_acks = 0;
        self.duplicate_ack_count = 0;
        self.fast_retransmit_pending = false;
        self.pending_pmtu_retransmit_sequence = None;
        self.persist_timer = None;
        self.persist_probe = None;
        self.next_pacing_send_nanos = None;
        self.delayed_ack_deadline_nanos = None;
        self.time_wait_deadline_nanos = None;
        self.unacked_receive_segments = 0;
        self.pending_window_update_bytes = 0;
    }

    fn discard_acked_segments(&mut self, acknowledgement: u32) {
        loop {
            let Some(front) = self.in_flight.as_ref().and_then(TcpInFlightQueue::front) else {
                return;
            };
            if sequence_leq(segment_end(front), acknowledgement) {
                let Some(in_flight) = self.in_flight.as_mut() else {
                    return;
                };
                let _ = in_flight.pop_front();
                continue;
            }
            if sequence_lt(front.header.sequence, acknowledgement) {
                let trim = usize::try_from(acknowledgement.wrapping_sub(front.header.sequence))
                    .expect("TCP partial ACK trim does not fit usize");
                let Some(in_flight) = self.in_flight.as_mut() else {
                    return;
                };
                let front = in_flight
                    .front_mut()
                    .expect("TCP in-flight queue lost front during partial ACK trim");
                front.header.sequence = acknowledgement;
                if !front.payload.is_empty() {
                    assert!(
                        trim <= front.payload.len(),
                        "TCP partial ACK trim exceeds in-flight payload"
                    );
                    let _ = front.payload.split_to(trim);
                }
                front.sequence_len = front
                    .sequence_len
                    .saturating_sub(u32::try_from(trim).expect("TCP partial ACK trim exceeds u32"));
            }
            return;
        }
    }

    fn discard_acked_persist_probe(&mut self, acknowledgement: u32) -> u32 {
        let Some(probe) = self.persist_probe else {
            return 0;
        };
        if sequence_lt(probe.sequence, acknowledgement) {
            self.persist_probe = None;
            let acked = acknowledgement
                .wrapping_sub(probe.sequence)
                .min(probe.sequence_len);
            if !probe.fin {
                self.transmit_queue
                    .as_mut()
                    .expect("TCP persist probe payload lost transmit queue")
                    .discard_front_bytes(
                        usize::try_from(acked).expect("TCP persist probe ACK exceeds usize"),
                    );
                self.send_next = self.send_next.wrapping_add(acked);
            }
            if probe.fin && !self.fin_queued {
                self.fin_queued = true;
                self.send_next = self.send_next.wrapping_add(1);
            }
            return acked;
        }
        0
    }

    fn highest_sent_sequence(&self) -> u32 {
        self.persist_probe.map_or(self.send_next, |probe| {
            self.send_next
                .max(probe.sequence.wrapping_add(probe.sequence_len))
        })
    }

    fn refresh_persist_after_ack(&mut self, now_nanos: u64) {
        if self.peer_receive_window != 0 {
            self.persist_timer = None;
            self.persist_probe = None;
        } else {
            self.arm_persist_timer_if_needed(now_nanos);
        }
    }

    fn acknowledges_unsent_data(&self, acknowledgement: u32) -> bool {
        sequence_lt(self.highest_sent_sequence(), acknowledgement)
    }

    fn syn_sent_ack_acceptable(&self, acknowledgement: u32) -> bool {
        sequence_lt(self.send_unacknowledged, acknowledgement)
            && sequence_leq(acknowledgement, self.send_next)
    }

    fn unacceptable_ack_reset(&self, acknowledgement: u32) -> Option<TcpReset> {
        let local = self.local?;
        let remote = self.remote?;
        Some(TcpReset {
            local,
            remote,
            header: TcpHeader {
                source_port: local.port,
                destination_port: remote.port,
                sequence: acknowledgement,
                acknowledgement: 0,
                flags: TcpFlags::RST,
                window_size: 0,
            },
        })
    }

    fn receive_sequence_acceptable(&self, sequence: u32, len: u32) -> bool {
        let window = self.local_receive_window_bytes();
        if len == 0 {
            return if window == 0 {
                sequence == self.receive_next
            } else {
                self.sequence_in_receive_window(sequence, window)
            };
        }
        if window == 0 {
            return false;
        }
        self.sequence_in_receive_window(sequence, window)
            || self.sequence_in_receive_window(sequence.wrapping_add(len - 1), window)
    }

    fn sequence_in_receive_window(&self, sequence: u32, window: u32) -> bool {
        let window_end = self.receive_next.wrapping_add(window);
        sequence_leq(self.receive_next, sequence) && sequence_lt(sequence, window_end)
    }

    fn local_receive_window_bytes(&self) -> u32 {
        u32::from(self.advertised_window) << self.local_window_scale
    }

    fn acknowledge_sent(
        &mut self,
        acknowledgement: u32,
        now_nanos: u64,
        app_limited: bool,
        timestamp: Option<TcpTimestampOption>,
        duplicate_ack_eligible: bool,
    ) -> Option<RecoveryAction> {
        if sequence_leq(acknowledgement, self.send_unacknowledged) {
            return (acknowledgement == self.send_unacknowledged && duplicate_ack_eligible)
                .then(|| self.note_duplicate_ack(now_nanos))
                .flatten();
        }
        let mut acked = acknowledgement.wrapping_sub(self.send_unacknowledged);
        if let Some(probe) = self.persist_probe
            && sequence_leq(
                acknowledgement,
                probe.sequence.wrapping_add(probe.sequence_len),
            )
            && sequence_leq(self.send_next, probe.sequence)
        {
            acked = self.send_next.wrapping_sub(self.send_unacknowledged);
        }
        let timing = self.ack_sample_timing(acknowledgement, now_nanos, timestamp);
        self.retransmission_timer.note_rtt(timing.rtt_nanos);
        self.send_unacknowledged = acknowledgement;
        self.duplicate_ack_count = 0;
        self.fast_retransmit_pending = false;
        if let Some(sequence) = self.pending_pmtu_retransmit_sequence
            && sequence_lt(sequence, acknowledgement)
        {
            self.pending_pmtu_retransmit_sequence = None;
        }
        let probe_acked = self.discard_acked_persist_probe(acknowledgement);
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(acked);
        self.discard_acked_segments(acknowledgement);
        self.delivered_bytes = self
            .delivered_bytes
            .saturating_add(u64::from(acked.saturating_add(probe_acked)));
        if self.fin_queued && sequence_leq(self.send_next, acknowledgement) {
            match self.state {
                TcpState::FinWait1 => {
                    self.set_state(TcpState::FinWait2, TcpStateChangeReason::FinAcknowledged);
                }
                TcpState::Closing => self.enter_time_wait(now_nanos),
                TcpState::LastAck => {
                    self.set_state(TcpState::Closed, TcpStateChangeReason::FinAcknowledged);
                }
                _ => {}
            }
        }
        self.refresh_persist_after_ack(now_nanos);
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
        let lost_bytes = self
            .first_unsacked_in_flight()
            .map(|segment| segment.sequence_len)?;
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
            if !self.sack_block_in_send_range(*block) {
                continue;
            }
            let Some(in_flight) = self.in_flight.as_mut() else {
                return;
            };
            for segment in in_flight.iter_mut() {
                if sequence_leq(block.left_edge, segment.header.sequence)
                    && sequence_leq(segment_end(segment), block.right_edge)
                {
                    segment.sacked = true;
                }
            }
        }
    }

    fn sack_block_in_send_range(&self, block: TcpSackBlock) -> bool {
        sequence_lt(block.left_edge, block.right_edge)
            && sequence_lt(self.send_unacknowledged, block.left_edge)
            && sequence_leq(block.right_edge, self.send_next)
    }

    fn on_retransmission_timeout(&mut self) {
        self.duplicate_ack_count = 0;
        self.fast_retransmit_pending = false;
        if let Some(in_flight) = self.in_flight.as_mut() {
            for segment in in_flight.iter_mut() {
                segment.sacked = false;
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
        if let Some(in_flight) = self.in_flight.as_ref() {
            for segment in in_flight.iter() {
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
            receive_window_size(self.receive_buffered_bytes(), self.local_window_scale);
    }

    fn receive_buffered_bytes(&self) -> usize {
        self.receive_queued_bytes
            .checked_add(self.out_of_order_queued_bytes)
            .expect("TCP receive buffered byte count overflowed")
    }

    fn receive_queue_is_empty(&self) -> bool {
        self.receive_queue
            .as_ref()
            .is_none_or(TcpReceiveQueue::is_empty)
    }

    fn receive_queue_is_full(&self) -> bool {
        self.receive_queue
            .as_ref()
            .is_some_and(TcpReceiveQueue::is_full)
    }

    fn receive_queue_mut(&mut self) -> &mut TcpReceiveQueue {
        self.receive_queue.get_or_insert_with(TcpReceiveQueue::new)
    }

    fn out_of_order_mut(&mut self) -> &mut TcpOutOfOrderQueue {
        self.out_of_order
            .get_or_insert_with(TcpOutOfOrderQueue::new)
    }

    fn transmit_queue_is_empty(&self) -> bool {
        self.transmit_queue
            .as_ref()
            .is_none_or(TcpTransmitQueue::is_empty)
    }

    fn transmit_queue_mut(&mut self) -> &mut TcpTransmitQueue {
        self.transmit_queue
            .get_or_insert_with(TcpTransmitQueue::new)
    }

    fn in_flight_mut(&mut self) -> &mut TcpInFlightQueue {
        self.in_flight.get_or_insert_with(TcpInFlightQueue::new)
    }

    fn push_receive_payload(&mut self, payload: Bytes) -> Result<(), ()> {
        if payload.is_empty() {
            return Ok(());
        }

        // Coalesce small consecutive segments into the back of the
        // receive queue's BytesMut. This `extend_from_slice` is the
        // only copy in the receive path now: large segments and
        // out-of-order fragments are pushed as Bytes via
        // `push_receive_segment_owned` without re-copying.
        if let Some(queue) = self.receive_queue.as_mut()
            && let Some(back) = queue.back_mut()
            && back.len().saturating_add(payload.len()) <= TCP_RECEIVE_COALESCE_BYTES
        {
            back.extend_from_slice(payload.as_ref());
            self.receive_queued_bytes = self
                .receive_queued_bytes
                .checked_add(payload.len())
                .expect("TCP receive queued byte count overflowed");
            return Ok(());
        }

        self.push_receive_segment_owned(payload).map_err(|_| ())
    }

    /// Push an owning `Bytes` into the receive queue without
    /// re-copying when possible. The current queue type is
    /// `BytesMut`; we recover ownership via `try_into_mut` (succeeds
    /// when the payload's Arc refcount is 1 — the typical case for
    /// virtio zero-copy buffers) and fall back to a one-shot
    /// `BytesMut::from(slice)` only on shared payloads. Once the
    /// queue stores `Bytes` directly this becomes pure zero-copy.
    fn push_receive_segment_owned(&mut self, bytes: Bytes) -> Result<(), Bytes> {
        let len = bytes.len();
        let buffer = bytes
            .try_into_mut()
            .unwrap_or_else(|shared| BytesMut::from(shared.as_ref()));
        self.receive_queue_mut()
            .push_back(buffer)
            .map_err(|buffer| buffer.freeze())?;
        self.receive_queued_bytes = self
            .receive_queued_bytes
            .checked_add(len)
            .expect("TCP receive queued byte count overflowed");
        Ok(())
    }

    fn push_front_receive_segment(&mut self, bytes: BytesMut) {
        let len = bytes.len();
        self.receive_queue_mut()
            .push_front(bytes)
            .unwrap_or_else(|_| panic!("TCP receive queue lost capacity while splitting"));
        self.receive_queued_bytes = self
            .receive_queued_bytes
            .checked_add(len)
            .expect("TCP receive queued byte count overflowed");
    }

    fn pop_receive_segment(&mut self) -> Option<BytesMut> {
        let bytes = self.receive_queue.as_mut()?.pop_front()?;
        assert!(
            self.receive_queued_bytes >= bytes.len(),
            "TCP receive queued byte count is corrupt"
        );
        self.receive_queued_bytes -= bytes.len();
        Some(bytes)
    }

    fn receive_payload(
        &mut self,
        sequence: u32,
        payload: Bytes,
        now_nanos: u64,
    ) -> Result<bool, ()> {
        let payload_len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
        let payload_end = sequence.wrapping_add(payload_len);
        if sequence_leq(payload_end, self.receive_next) {
            // Data the peer already had acknowledged. It resent it
            // because it believes something was lost, so answer every
            // copy rather than one per batch.
            self.request_duplicate_ack();
            return Ok(false);
        }

        if sequence_lt(sequence, self.receive_next) {
            let trim = usize::try_from(self.receive_next.wrapping_sub(sequence))
                .expect("TCP receive overlap trim does not fit usize");
            return self.receive_payload(self.receive_next, payload.slice(trim..), now_nanos);
        }

        if sequence == self.receive_next {
            return self.push_contiguous_payload(payload, now_nanos);
        }

        self.insert_out_of_order_payload(sequence, payload)?;
        self.refresh_advertised_window();
        self.request_duplicate_ack();
        Ok(false)
    }

    fn push_contiguous_payload(&mut self, payload: Bytes, now_nanos: u64) -> Result<bool, ()> {
        let payload_len = payload.len();
        self.push_receive_payload(payload)?;
        self.receive_next = self
            .receive_next
            .wrapping_add(u32::try_from(payload_len).unwrap_or(u32::MAX));
        let drained_segments = self.drain_contiguous_out_of_order();
        self.refresh_advertised_window();
        if drained_segments == 0 {
            self.note_inbound_payload(payload_len, now_nanos);
        } else {
            self.request_ack();
        }
        Ok(payload_len != 0 || drained_segments != 0)
    }

    fn drain_contiguous_out_of_order(&mut self) -> usize {
        let mut drained = 0;
        loop {
            let remove_stale = {
                let Some(out_of_order) = self.out_of_order.as_ref() else {
                    return drained;
                };
                let Some(segment) = out_of_order.first() else {
                    return drained;
                };
                let segment_end = segment_end_from_parts(segment.sequence, segment.payload.len());
                if sequence_lt(self.receive_next, segment.sequence) {
                    return drained;
                }
                if !sequence_leq(segment_end, self.receive_next) && self.receive_queue_is_full() {
                    return drained;
                }
                sequence_leq(segment_end, self.receive_next)
            };

            if remove_stale {
                let stale = self
                    .out_of_order
                    .as_mut()
                    .expect("TCP out-of-order queue disappeared during stale removal")
                    .remove(0);
                self.out_of_order_queued_bytes = self
                    .out_of_order_queued_bytes
                    .checked_sub(stale.payload.len())
                    .expect("TCP out-of-order byte count underflowed");
                continue;
            }

            let mut segment = self
                .out_of_order
                .as_mut()
                .expect("TCP out-of-order queue disappeared during drain")
                .remove(0);
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
            self.push_receive_payload(segment.payload)
                .unwrap_or_else(|_| panic!("TCP receive queue reported full after capacity check"));
            self.receive_next = self
                .receive_next
                .wrapping_add(u32::try_from(len).unwrap_or(u32::MAX));
            drained += 1;
        }
    }

    fn note_receive_fin(&mut self, sequence: u32, payload_len: usize, now_nanos: u64) -> bool {
        let fin_sequence = segment_end_from_parts(sequence, payload_len);
        if sequence_lt(fin_sequence, self.receive_next) {
            self.request_ack();
            return false;
        }
        self.receive_fin_sequence = Some(fin_sequence);
        if self.consume_pending_receive_fin(now_nanos) {
            return true;
        }
        if self.receive_fin_sequence.is_some() {
            self.request_ack();
        }
        false
    }

    fn consume_pending_receive_fin(&mut self, now_nanos: u64) -> bool {
        if self.receive_fin_sequence != Some(self.receive_next) {
            return false;
        }
        self.receive_fin_sequence = None;
        self.receive_next = self.receive_next.wrapping_add(1);
        match self.state {
            TcpState::Established => {
                self.set_state(TcpState::CloseWait, TcpStateChangeReason::PeerFin);
            }
            TcpState::FinWait1 => {
                self.set_state(TcpState::Closing, TcpStateChangeReason::PeerFin);
            }
            TcpState::FinWait2 => self.enter_time_wait(now_nanos),
            _ => {}
        }
        self.request_ack();
        true
    }

    fn insert_out_of_order_payload(&mut self, sequence: u32, payload: Bytes) -> Result<(), ()> {
        let end = segment_end_from_parts(sequence, payload.len());
        let mut cursor_sequence = sequence;
        let mut cursor_offset = 0;
        let mut fragments = HeapVec::<(u32, usize, usize), MAX_TCP_OUT_OF_ORDER_SEGMENTS>::new();

        if let Some(out_of_order) = self.out_of_order.as_ref() {
            for existing in out_of_order.iter() {
                let existing_end =
                    segment_end_from_parts(existing.sequence, existing.payload.len());
                if sequence_leq(existing_end, cursor_sequence) {
                    continue;
                }
                if sequence_leq(end, existing.sequence) {
                    break;
                }
                if sequence_lt(cursor_sequence, existing.sequence) {
                    let fragment_len =
                        usize::try_from(existing.sequence.wrapping_sub(cursor_sequence))
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
        }

        if sequence_lt(cursor_sequence, end) {
            fragments
                .push((cursor_sequence, cursor_offset, payload.len()))
                .map_err(|_| ())?;
        }

        for (fragment_sequence, start, end) in fragments {
            self.insert_out_of_order_fragment(fragment_sequence, payload.slice(start..end))?;
        }
        Ok(())
    }

    fn insert_out_of_order_fragment(&mut self, sequence: u32, payload: Bytes) -> Result<(), ()> {
        if payload.is_empty() {
            return Ok(());
        }
        // payload is already an owning Bytes — slice or copied into
        // by the caller. No further copy at the segment boundary.
        let segment = TcpOutOfOrderSegment { sequence, payload };
        let len = segment.payload.len();
        self.out_of_order_mut().push(segment).map_err(|_| ())?;
        self.out_of_order_queued_bytes = self
            .out_of_order_queued_bytes
            .checked_add(len)
            .expect("TCP out-of-order byte count overflowed");

        let out_of_order = self
            .out_of_order
            .as_mut()
            .expect("TCP out-of-order queue was not materialized after insert");
        let mut index = out_of_order.len() - 1;
        while index != 0
            && sequence_lt(
                out_of_order[index].sequence,
                out_of_order[index - 1].sequence,
            )
        {
            out_of_order.swap(index, index - 1);
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
            self.peer_max_segment_size = usize::from(mss).clamp(1, TCP_RECEIVE_SEGMENT_BYTES);
        }
        if let Some(shift) = packet.options.window_scale() {
            self.peer_window_scale = shift.min(TCP_MAX_WINDOW_SCALE);
        }
        // RFC 7323: window scaling is in effect only when both SYNs carry
        // the option; we always offer it, so the peer's SYN decides.
        self.local_window_scale = if packet.options.window_scale().is_some() {
            TCP_LOCAL_WINDOW_SCALE
        } else {
            0
        };
        self.refresh_advertised_window();
        // RFC 7323 §2.2: the window field in a SYN or SYN-ACK is never
        // scaled, even though that segment is what carries the option.
        self.peer_receive_window = u32::from(packet.window_size);
        self.peer_sack_permitted = packet.options.sack_permitted();
        self.record_peer_timestamp(packet.options.timestamp());
    }

    fn record_peer_timestamp(&mut self, timestamp: Option<TcpTimestampOption>) {
        if let Some(timestamp) = timestamp {
            self.peer_timestamp = Some(timestamp);
        }
    }

    fn update_peer_receive_window(&mut self, window_size: u16) {
        self.peer_receive_window = self.scaled_peer_receive_window(window_size);
    }

    fn scaled_peer_receive_window(&self, window_size: u16) -> u32 {
        u32::from(window_size) << self.peer_window_scale
    }

    fn duplicate_ack_eligible(&self, packet: TcpPacket<'_>) -> bool {
        packet.acknowledgement == self.send_unacknowledged
            && packet.payload.is_empty()
            && !packet.flags.contains(TcpFlags::SYN)
            && !packet.flags.contains(TcpFlags::FIN)
            && !packet.flags.contains(TcpFlags::RST)
            && self.scaled_peer_receive_window(packet.window_size) == self.peer_receive_window
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
        self.pending_acks = self.pending_acks.max(1);
        self.delayed_ack_deadline_nanos = None;
        self.unacked_receive_segments = 0;
        self.pending_window_update_bytes = 0;
    }

    /// Queues one more acknowledgement on top of whatever is already
    /// owed, for a segment that could not advance the receive sequence.
    ///
    /// The peer counts these: three repetitions of the same
    /// acknowledgement are what tell it to retransmit the hole without
    /// waiting for its timer, and a receiver with no SACK to offer —
    /// which is every peer that did not negotiate it — has no other way
    /// to say so.
    fn request_duplicate_ack(&mut self) {
        self.pending_acks = self.pending_acks.saturating_add(1).min(TCP_MAX_QUEUED_ACKS);
        self.delayed_ack_deadline_nanos = None;
        self.unacked_receive_segments = 0;
        self.pending_window_update_bytes = 0;
    }

    fn has_persist_pending_data(&self) -> bool {
        if !matches!(
            self.state,
            TcpState::Established | TcpState::CloseWait | TcpState::FinWait1 | TcpState::LastAck
        ) {
            return false;
        }
        !self.transmit_queue_is_empty()
            || (matches!(self.state, TcpState::FinWait1 | TcpState::LastAck) && !self.fin_queued)
    }

    fn arm_persist_timer_if_needed(&mut self, now_nanos: u64) {
        if self.peer_receive_window != 0 || !self.has_persist_pending_data() {
            self.persist_timer = None;
            return;
        }
        if self.persist_timer.is_none() {
            self.persist_timer = Some(TcpPersistTimer::new(now_nanos, self.retransmission_timer));
        }
    }

    fn pending_persist_probe(&self, now_nanos: u64) -> Option<TcpPersistProbe> {
        if self.peer_receive_window != 0 {
            return None;
        }
        let timer = self.persist_timer?;
        if now_nanos < timer.deadline_nanos() {
            return None;
        }
        if !self.transmit_queue_is_empty() {
            return Some(TcpPersistProbe {
                sequence: self.send_next,
                sequence_len: 1,
                fin: false,
            });
        }
        (matches!(self.state, TcpState::FinWait1 | TcpState::LastAck) && !self.fin_queued)
            .then_some(TcpPersistProbe {
                sequence: self.send_next,
                sequence_len: 1,
                fin: true,
            })
    }

    fn mark_persist_probe_queued(&mut self, probe: TcpPersistProbe, now_nanos: u64) {
        self.persist_probe = Some(probe);
        self.persist_timer
            .get_or_insert_with(|| TcpPersistTimer::new(now_nanos, self.retransmission_timer))
            .note_probe_sent(now_nanos, self.retransmission_timer);
    }

    fn enter_time_wait(&mut self, now_nanos: u64) {
        self.set_state(TcpState::TimeWait, TcpStateChangeReason::PeerFin);
        self.time_wait_deadline_nanos = Some(now_nanos.saturating_add(TCP_TIME_WAIT_NANOS));
    }

    fn should_advertise_window_update(&mut self, previous_window: u16) -> bool {
        if self.advertised_window <= previous_window {
            return false;
        }
        if previous_window == 0 {
            return true;
        }
        // The advertised window field counts scaled units; accumulate the
        // delta in bytes so the update threshold means the same thing with
        // and without a negotiated window scale.
        let delta_bytes =
            u32::from(self.advertised_window - previous_window) << self.local_window_scale;
        self.pending_window_update_bytes =
            self.pending_window_update_bytes.saturating_add(delta_bytes);
        self.pending_window_update_bytes >= u32::from(TCP_WINDOW_UPDATE_BYTES)
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

fn receive_sequence_len(packet: TcpPacket<'_>) -> u32 {
    u32::try_from(packet.payload.len())
        .unwrap_or(u32::MAX)
        .saturating_add(u32::from(packet.flags.contains(TcpFlags::FIN)))
}

pub(crate) fn sequence_leq(lhs: u32, rhs: u32) -> bool {
    lhs == rhs || (rhs.wrapping_sub(lhs) as i32) >= 0
}

pub(crate) fn sequence_lt(lhs: u32, rhs: u32) -> bool {
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

    /// Test fixture that derives the owning `Bytes` payload from a
    /// `TcpPacket`'s borrowed `&[u8]` view via
    /// `Bytes::copy_from_slice` and forwards to `on_segment`.
    /// Production callers already hold the parent frame as `Bytes`
    /// and slice into it directly without this copy.
    fn deliver_segment<C: CongestionControl>(
        socket: &mut TcpSocket<C>,
        packet: TcpPacket<'_>,
        now_nanos: u64,
    ) -> TcpSegmentOutcome {
        let payload = Bytes::copy_from_slice(packet.payload);
        socket.on_segment(packet, payload, now_nanos)
    }

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

    fn sack_options_bytes(block: TcpSackBlock) -> [u8; 10] {
        let left = block.left_edge.to_be_bytes();
        let right = block.right_edge.to_be_bytes();
        [
            5, 10, left[0], left[1], left[2], left[3], right[0], right[1], right[2], right[3],
        ]
    }

    fn established_socket() -> TcpSocket<BbrV3> {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
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

    /// Like [`established_socket`], but the peer negotiates window
    /// scaling so our advertised window is scaled too.
    fn established_scaled_socket() -> TcpSocket<BbrV3> {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::parse(&[3, 3, TCP_LOCAL_WINDOW_SCALE, 0])
                    .expect("window scale option should parse"),
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
        let _ = deliver_segment(
            &mut socket,
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
    fn syn_sent_rejects_syn_ack_acknowledging_unsent_sequence() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 9,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );

        assert_eq!(socket.state(), TcpState::SynSent);
        assert_eq!(socket.receive_next, 0);
        let reset = outcome
            .reset
            .expect("unacceptable SYN-SENT ACK must request a reset");
        assert_eq!(reset.header.sequence, 9);
        assert_eq!(reset.header.acknowledgement, 0);
        assert_eq!(reset.header.flags, TcpFlags::RST);
    }

    #[test]
    fn syn_sent_ignores_rst_without_ack() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 0,
                flags: TcpFlags::RST,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );

        assert_eq!(socket.state(), TcpState::SynSent);
        assert_eq!(outcome, TcpSegmentOutcome::default());
    }

    #[test]
    fn syn_received_accepts_in_window_rst() {
        let mut socket = TcpSocket::accept(endpoint(8080), peer(49152), 11, 99, BbrV3::new(1460));
        socket.mark_syn_ack_queued(0);

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 49152,
                destination_port: 8080,
                sequence: 11,
                acknowledgement: 0,
                flags: TcpFlags::RST,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );

        assert_eq!(outcome, TcpSegmentOutcome::default());
        assert_eq!(socket.state(), TcpState::Closed);
    }

    #[test]
    fn syn_received_drops_out_of_window_rst() {
        let mut socket = TcpSocket::accept(endpoint(8080), peer(49152), 11, 99, BbrV3::new(1460));
        socket.mark_syn_ack_queued(0);
        let far_sequence = socket
            .receive_next
            .wrapping_add(u32::from(socket.advertised_window()) << TCP_LOCAL_WINDOW_SCALE)
            .wrapping_add(1);

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 49152,
                destination_port: 8080,
                sequence: far_sequence,
                acknowledgement: 0,
                flags: TcpFlags::RST,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );

        assert_eq!(outcome, TcpSegmentOutcome::default());
        assert_eq!(socket.state(), TcpState::SynReceived);
    }

    #[test]
    fn syn_received_unacceptable_ack_requests_reset() {
        let mut socket = TcpSocket::accept(endpoint(8080), peer(49152), 11, 99, BbrV3::new(1460));
        socket.mark_syn_ack_queued(0);

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 49152,
                destination_port: 8080,
                sequence: 11,
                acknowledgement: 101,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );

        assert_eq!(socket.state(), TcpState::SynReceived);
        let reset = outcome
            .reset
            .expect("unacceptable SYN-RECEIVED ACK must request a reset");
        assert_eq!(reset.header.sequence, 101);
        assert_eq!(reset.header.acknowledgement, 0);
        assert_eq!(reset.header.flags, TcpFlags::RST);
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
        let _ = deliver_segment(
            &mut socket,
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
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("established socket should transmit queued bytes");
        let _ = deliver_segment(
            &mut socket,
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
            .take_transmit_segment(sent_at, TcpSegmentBudget::wire_segments(usize::MAX))
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
            .take_transmit_segment(sent_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("established socket should transmit queued bytes");

        for index in 0u64..3 {
            let _ = deliver_segment(
                &mut socket,
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
    fn data_carrying_same_ack_does_not_trigger_fast_retransmit() {
        let mut socket = established_socket();
        assert_eq!(socket.queue_send(b"hello"), 5);
        let sent_at = TCP_INITIAL_RTO_NANOS + 7;
        let data = socket
            .take_transmit_segment(sent_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("established socket should transmit queued bytes");

        for index in 0u64..3 {
            let payload = [b'a' + u8::try_from(index).expect("test byte offset fits u8")];
            let _ = deliver_segment(
                &mut socket,
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101 + u32::try_from(index).expect("test sequence offset fits u32"),
                    acknowledgement: data.header.sequence,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload: &payload,
                },
                sent_at + index,
            );
        }

        assert_eq!(socket.duplicate_ack_count, 0);
        assert_eq!(socket.pending_retransmission(sent_at + 3), None);
    }

    #[test]
    fn window_update_same_ack_does_not_trigger_fast_retransmit() {
        let mut socket = established_socket();
        assert_eq!(socket.queue_send(b"hello"), 5);
        let sent_at = TCP_INITIAL_RTO_NANOS + 7;
        let data = socket
            .take_transmit_segment(sent_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("established socket should transmit queued bytes");

        for (index, window_size) in [4096u16, 8192, 16_384].into_iter().enumerate() {
            let _ = deliver_segment(
                &mut socket,
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101,
                    acknowledgement: data.header.sequence,
                    flags: TcpFlags::ACK,
                    window_size,
                    options: TcpOptions::empty(),
                    payload: &[],
                },
                sent_at + u64::try_from(index).expect("test timestamp fits u64"),
            );
        }

        assert_eq!(socket.duplicate_ack_count, 0);
        assert_eq!(socket.pending_retransmission(sent_at + 3), None);
        assert_eq!(socket.peer_receive_window(), 16_384);
    }

    #[test]
    fn sack_blocks_before_cumulative_ack_or_after_send_next_are_ignored() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
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
            .take_transmit_segment(sent_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("first segment should transmit");
        let second_send_at = socket
            .next_deadline_nanos()
            .expect("paced second segment should have a deadline");
        let second = socket
            .take_transmit_segment(second_send_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("second segment should transmit");
        let mut blocks = TcpSackBlocks::empty();
        blocks
            .push(TcpSackBlock {
                left_edge: first.header.sequence,
                right_edge: first.header.sequence + first.sequence_len,
            })
            .expect("single SACK block should fit");
        blocks
            .push(TcpSackBlock {
                left_edge: second.header.sequence,
                right_edge: second.header.sequence + second.sequence_len + 1,
            })
            .expect("second SACK block should fit");
        socket.apply_sack_blocks(blocks);

        {
            let in_flight = socket
                .in_flight
                .as_ref()
                .expect("test should have data in flight");
            let mut segments = in_flight.iter();
            assert!(
                !segments
                    .next()
                    .expect("first segment should remain in flight")
                    .sacked
            );
            assert!(
                !segments
                    .next()
                    .expect("second segment should remain in flight")
                    .sacked
            );
        }

        for index in 0..3 {
            let _ = deliver_segment(
                &mut socket,
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
            .expect("duplicate ACKs should still retransmit the first unsacked segment");
        assert_eq!(retransmit.header.sequence, first.header.sequence);
        assert_eq!(retransmit.payload, first.payload);
    }

    #[test]
    fn valid_sack_marks_only_in_range_in_flight_segments() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
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
        assert_eq!(socket.queue_send(b"abcdefghijkl"), 12);
        let sent_at = TCP_INITIAL_RTO_NANOS + 7;
        let first = socket
            .take_transmit_segment(sent_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("first segment should transmit");
        let second_send_at = socket
            .next_deadline_nanos()
            .expect("paced second segment should have a deadline");
        let second = socket
            .take_transmit_segment(second_send_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("second segment should transmit");
        assert!(sequence_lt(first.header.sequence, second.header.sequence));
        let mut blocks = TcpSackBlocks::empty();
        blocks
            .push(TcpSackBlock {
                left_edge: second.header.sequence,
                right_edge: second.header.sequence + second.sequence_len,
            })
            .expect("valid SACK block should fit");
        socket.apply_sack_blocks(blocks);

        let in_flight = socket
            .in_flight
            .as_ref()
            .expect("test should have data in flight");
        let mut segments = in_flight.iter();
        assert!(
            !segments
                .next()
                .expect("first segment should remain in flight")
                .sacked
        );
        assert!(
            segments
                .next()
                .expect("second segment should remain in flight")
                .sacked
        );
    }

    #[test]
    fn retransmission_timeout_clears_stale_sack_state() {
        let mut socket = established_socket();
        socket.peer_max_segment_size = 4;
        assert_eq!(socket.queue_send(b"abcdefgh"), 8);
        let sent_at = TCP_INITIAL_RTO_NANOS + 7;
        let first = socket
            .take_transmit_segment(sent_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("first segment should transmit");
        let second_send_at = socket
            .next_deadline_nanos()
            .expect("paced second segment should have a deadline");
        let second = socket
            .take_transmit_segment(second_send_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("second segment should transmit");
        let mut blocks = TcpSackBlocks::empty();
        blocks
            .push(TcpSackBlock {
                left_edge: second.header.sequence,
                right_edge: second.header.sequence + second.sequence_len,
            })
            .expect("single SACK block should fit");
        let sack_options = sack_options_bytes(blocks.as_slice()[0]);
        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: first.header.sequence,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::parse(&sack_options).expect("SACK option should parse"),
                payload: &[],
            },
            sent_at + 1,
        );
        assert!(
            socket
                .in_flight
                .as_ref()
                .expect("test should have in-flight data")
                .iter()
                .any(|segment| segment.header.sequence == second.header.sequence && segment.sacked)
        );
        let retransmit_at = socket
            .next_deadline_nanos()
            .expect("first in-flight segment should arm RTO");
        let retransmit = socket
            .pending_retransmission(retransmit_at)
            .expect("RTO should retransmit first unacknowledged segment");
        assert_eq!(retransmit.header.sequence, first.header.sequence);
        socket.mark_retransmission_queued(retransmit.header.sequence, retransmit_at);

        let in_flight = socket
            .in_flight
            .as_ref()
            .expect("test should keep in-flight data after RTO");
        assert!(in_flight.iter().all(|segment| !segment.sacked));
        assert_eq!(socket.duplicate_ack_count, 0);

        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: second.header.sequence,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            retransmit_at + 1,
        );
        for index in 0..3 {
            let _ = deliver_segment(
                &mut socket,
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101,
                    acknowledgement: second.header.sequence,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload: &[],
                },
                retransmit_at + 2 + index,
            );
        }
        let retransmit = socket
            .pending_retransmission(retransmit_at + 5)
            .expect("second segment should remain eligible for fast retransmit after RTO");
        assert_eq!(retransmit.header.sequence, second.header.sequence);
        assert_eq!(retransmit.payload, second.payload);
    }

    #[test]
    fn old_ack_with_sack_blocks_is_ignored_after_cumulative_ack() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
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
            .take_transmit_segment(sent_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("first segment should transmit");
        let second_send_at = socket
            .next_deadline_nanos()
            .expect("paced second segment should have a deadline");
        let second = socket
            .take_transmit_segment(second_send_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("second segment should transmit");
        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: second.header.sequence + second.sequence_len,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            second_send_at + 1,
        );
        assert_eq!(socket.bytes_in_flight, 0);
        let mut blocks = TcpSackBlocks::empty();
        blocks
            .push(TcpSackBlock {
                left_edge: second.header.sequence,
                right_edge: second.header.sequence + second.sequence_len,
            })
            .expect("single SACK block should fit");
        let sack_options = sack_options_bytes(blocks.as_slice()[0]);

        for index in 0..3 {
            let outcome = deliver_segment(
                &mut socket,
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 101,
                    acknowledgement: first.header.sequence,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::parse(&sack_options).expect("SACK option should parse"),
                    payload: &[],
                },
                second_send_at + 2 + index,
            );
            assert_eq!(outcome, TcpSegmentOutcome::default());
        }

        assert_eq!(socket.duplicate_ack_count, 0);
        assert_eq!(
            socket.pending_retransmission(second_send_at + TCP_INITIAL_RTO_NANOS),
            None
        );
        assert_eq!(socket.bytes_in_flight, 0);
        assert!(
            socket
                .in_flight
                .as_ref()
                .is_none_or(|queue| queue.front().is_none())
        );
    }

    #[test]
    fn queue_send_bytes_reuses_payload_storage() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
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
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("established socket should transmit queued bytes");
        assert_eq!(segment.payload.as_ref(), b"hello");
        assert_eq!(segment.payload.as_ptr(), payload_ptr);
    }

    #[test]
    fn full_in_flight_queue_closes_emission_instead_of_panicking() {
        let mut socket = established_scaled_socket();
        socket.update_peer_receive_window(u16::MAX);
        assert_eq!(socket.queue_send(b"pending payload"), 15);

        // Saturate the in-flight queue the way a long bulk upload does
        // once the congestion window has grown past it; before the
        // emission gate the next segment panicked on the full queue.
        let local = endpoint(49152);
        let remote = peer(80);
        for value in 0..MAX_TCP_IN_FLIGHT_SEGMENTS {
            socket
                .in_flight_mut()
                .push_back(TcpInFlightSegment {
                    header: TcpHeader {
                        source_port: local.port,
                        destination_port: remote.port,
                        sequence: value as u32,
                        acknowledgement: 0,
                        flags: TcpFlags::ACK,
                        window_size: u16::MAX,
                    },
                    options: TcpHeaderOptions::empty(),
                    payload: Bytes::from_static(b"x"),
                    sequence_len: 1,
                    sent_at_nanos: 0,
                    retransmissions: 0,
                    sacked: false,
                })
                .expect("in-flight fill within capacity");
        }

        assert!(
            socket
                .take_transmit_segment(
                    TCP_INITIAL_RTO_NANOS,
                    TcpSegmentBudget::wire_segments(usize::MAX)
                )
                .is_none(),
            "a full in-flight queue must close the send window"
        );
    }

    #[test]
    fn transmit_and_in_flight_queues_preserve_capacity() {
        let mut transmit = TcpTransmitQueue::new();
        for value in 0..MAX_TCP_QUEUED_SEGMENTS {
            transmit
                .push_back(Bytes::copy_from_slice(&[value as u8]))
                .expect("transmit queue should accept capacity segment");
        }
        assert!(transmit.push_back(Bytes::from_static(b"full")).is_err());
        for value in 0..MAX_TCP_QUEUED_SEGMENTS {
            let bytes = transmit
                .pop_front()
                .expect("transmit queue should preserve queued segment");
            assert_eq!(bytes.as_ref(), &[value as u8]);
        }
        assert!(transmit.is_empty());
        assert_eq!(
            transmit.remaining_bytes_capacity(),
            TCP_TRANSMIT_BUFFER_BYTES
        );

        let mut in_flight = TcpInFlightQueue::new();
        let local = endpoint(49152);
        let remote = peer(80);
        for value in 0..MAX_TCP_IN_FLIGHT_SEGMENTS {
            in_flight
                .push_back(TcpInFlightSegment {
                    header: TcpHeader {
                        source_port: local.port,
                        destination_port: remote.port,
                        sequence: value as u32,
                        acknowledgement: 0,
                        flags: TcpFlags::ACK,
                        window_size: u16::MAX,
                    },
                    options: TcpHeaderOptions::empty(),
                    payload: Bytes::copy_from_slice(&[value as u8]),
                    sequence_len: 1,
                    sent_at_nanos: value as u64,
                    retransmissions: 0,
                    sacked: false,
                })
                .expect("in-flight queue should accept capacity segment");
        }
        assert!(
            in_flight
                .push_back(TcpInFlightSegment {
                    header: TcpHeader {
                        source_port: local.port,
                        destination_port: remote.port,
                        sequence: MAX_TCP_IN_FLIGHT_SEGMENTS as u32,
                        acknowledgement: 0,
                        flags: TcpFlags::ACK,
                        window_size: u16::MAX,
                    },
                    options: TcpHeaderOptions::empty(),
                    payload: Bytes::from_static(b"full"),
                    sequence_len: 1,
                    sent_at_nanos: 0,
                    retransmissions: 0,
                    sacked: false,
                })
                .is_err()
        );
        for value in 0..MAX_TCP_QUEUED_SEGMENTS {
            let segment = in_flight
                .pop_front()
                .expect("in-flight queue should preserve queued segment");
            assert_eq!(segment.payload.as_ref(), &[value as u8]);
        }
    }

    #[test]
    fn peer_mss_option_caps_transmit_segment_size() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
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
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("queued bytes should produce a capped segment");

        assert_eq!(first.payload.as_ref(), b"abcd");
    }

    #[test]
    fn path_mtu_caps_transmit_segment_size() {
        let mut socket = established_socket();

        assert_eq!(socket.queue_send(b"abcdefghij"), 10);
        let first = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1,
                TcpSegmentBudget::wire_segments(TcpPacket::MIN_HEADER_LEN + 4),
            )
            .expect("path MTU should allow a capped data segment");
        let second_send_at = socket
            .next_deadline_nanos()
            .expect("PMTU split second segment should have a pacing deadline");
        let second = socket
            .take_transmit_segment(
                second_send_at,
                TcpSegmentBudget::wire_segments(TcpPacket::MIN_HEADER_LEN + 4),
            )
            .expect("remaining bytes should stay queued after PMTU split");

        assert_eq!(first.payload.as_ref(), b"abcd");
        assert_eq!(second.payload.as_ref(), b"efgh");
    }

    #[test]
    fn path_mtu_reduction_queues_eager_retransmission_without_rto() {
        let mut socket = established_socket();
        assert_eq!(socket.queue_send(b"abcdefgh"), 8);
        let sent_at = TCP_INITIAL_RTO_NANOS + 7;
        let data = socket
            .take_transmit_segment(sent_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("queued data should transmit");

        socket.mark_path_mtu_reduced(data.header.sequence);
        let retransmit = socket
            .pending_retransmission_with_mtu(sent_at + 1, TcpPacket::MIN_HEADER_LEN + 3)
            .expect("PMTU reduction should trigger retransmission before RTO");

        assert_eq!(retransmit.header.sequence, data.header.sequence);
        assert_eq!(retransmit.sequence_len, 3);
        assert_eq!(retransmit.payload.as_ref(), b"abc");
    }

    #[test]
    fn path_mtu_reduction_inside_segment_clears_after_retransmission() {
        let mut socket = established_socket();
        assert_eq!(socket.queue_send(b"abcdefgh"), 8);
        let sent_at = TCP_INITIAL_RTO_NANOS + 7;
        let data = socket
            .take_transmit_segment(sent_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("queued data should transmit");

        socket.mark_path_mtu_reduced(data.header.sequence + 3);
        let retransmit = socket
            .pending_retransmission_with_mtu(sent_at + 1, TcpPacket::MIN_HEADER_LEN + 3)
            .expect("interior PMTU quote should trigger retransmission before RTO");
        assert_eq!(retransmit.header.sequence, data.header.sequence);
        socket.mark_retransmission_queued(retransmit.header.sequence, sent_at + 1);

        assert_eq!(
            socket.pending_retransmission_with_mtu(sent_at + 2, TcpPacket::MIN_HEADER_LEN + 3),
            None
        );
    }

    #[test]
    fn partial_ack_trims_in_flight_retransmission_payload() {
        let mut socket = established_socket();
        assert_eq!(socket.queue_send(b"abcdefgh"), 8);
        let data = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("queued data should transmit");
        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: data.header.sequence + 3,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 2,
        );
        let retransmit_at = socket
            .next_deadline_nanos()
            .expect("partial ACK should leave the unacked tail on the RTO timer");

        let retransmit = socket
            .pending_retransmission_with_mtu(retransmit_at, TcpPacket::MIN_HEADER_LEN + 2)
            .expect("unacked tail should remain eligible for retransmission");

        assert_eq!(retransmit.header.sequence, data.header.sequence + 3);
        assert_eq!(retransmit.sequence_len, 2);
        assert_eq!(retransmit.payload.as_ref(), b"de");
    }

    #[test]
    fn ack_above_send_next_is_rejected_without_advancing_send_state() {
        let mut socket = established_socket();
        assert_eq!(socket.queue_send(b"abcdefgh"), 8);
        let sent_at = TCP_INITIAL_RTO_NANOS + 1;
        let data = socket
            .take_transmit_segment(sent_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("queued data should transmit");
        let delivered_before = socket.delivered_bytes;
        let send_unacknowledged_before = socket.send_unacknowledged;
        let peer_receive_window_before = socket.peer_receive_window();
        let mut blocks = TcpSackBlocks::empty();
        blocks
            .push(TcpSackBlock {
                left_edge: data.header.sequence,
                right_edge: data.header.sequence + data.sequence_len,
            })
            .expect("single SACK block should fit");
        let sack_options = sack_options_bytes(blocks.as_slice()[0]);

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: data.header.sequence + data.sequence_len + 1,
                flags: TcpFlags::ACK,
                window_size: 1,
                options: TcpOptions::parse(&sack_options).expect("SACK option should parse"),
                payload: &[],
            },
            sent_at + 1,
        );

        assert_eq!(outcome, TcpSegmentOutcome::default());
        assert_eq!(socket.send_unacknowledged, send_unacknowledged_before);
        assert_eq!(socket.bytes_in_flight, data.sequence_len);
        assert_eq!(socket.delivered_bytes, delivered_before);
        assert_eq!(socket.peer_receive_window(), peer_receive_window_before);
        let in_flight = socket
            .in_flight
            .as_ref()
            .and_then(TcpInFlightQueue::front)
            .expect("invalid ACK must leave data in flight");
        assert_eq!(in_flight.header.sequence, data.header.sequence);
        assert!(!in_flight.sacked);
        assert!(socket.pending_ack().is_some());

        let retransmit_at = socket
            .next_deadline_nanos()
            .expect("invalid ACK must leave original RTO armed");
        let retransmit = socket
            .pending_retransmission(retransmit_at)
            .expect("original in-flight data must remain retransmittable");
        assert_eq!(retransmit.header.sequence, data.header.sequence);
        assert_eq!(retransmit.payload, data.payload);
    }

    #[test]
    fn ack_above_send_next_preserves_pmtu_retransmission_marker() {
        let mut socket = established_socket();
        assert_eq!(socket.queue_send(b"abcdefgh"), 8);
        let sent_at = TCP_INITIAL_RTO_NANOS + 1;
        let data = socket
            .take_transmit_segment(sent_at, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("queued data should transmit");
        socket.mark_path_mtu_reduced(data.header.sequence);

        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: data.header.sequence + data.sequence_len + 1,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            sent_at + 1,
        );

        assert_eq!(
            socket.pending_pmtu_retransmit_sequence,
            Some(data.header.sequence)
        );
        let retransmit = socket
            .pending_retransmission_with_mtu(sent_at + 2, TcpPacket::MIN_HEADER_LEN + 3)
            .expect("invalid ACK must not clear PMTU-triggered retransmission");
        assert_eq!(retransmit.header.sequence, data.header.sequence);
        assert_eq!(retransmit.payload.as_ref(), b"abc");
    }

    #[test]
    fn synchronized_state_drops_payload_without_ack_bit() {
        let mut socket = established_socket();
        let receive_next_before = socket.receive_next;
        let peer_receive_window_before = socket.peer_receive_window();

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: receive_next_before,
                acknowledgement: 0,
                flags: TcpFlags::PSH,
                window_size: 1,
                options: TcpOptions::empty(),
                payload: b"abc",
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(outcome, TcpSegmentOutcome::default());
        assert_eq!(socket.receive_next, receive_next_before);
        assert_eq!(socket.receive(16, TCP_INITIAL_RTO_NANOS + 2), None);
        assert_eq!(socket.peer_receive_window(), peer_receive_window_before);
        assert_eq!(socket.pending_ack(), None);
    }

    #[test]
    fn synchronized_state_drops_fin_without_ack_bit() {
        let mut socket = established_socket();
        let receive_next_before = socket.receive_next;

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: receive_next_before,
                acknowledgement: 0,
                flags: TcpFlags::FIN,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(outcome, TcpSegmentOutcome::default());
        assert_eq!(socket.state(), TcpState::Established);
        assert_eq!(socket.receive_next, receive_next_before);
        assert_eq!(socket.receive(16, TCP_INITIAL_RTO_NANOS + 2), None);
        assert_eq!(socket.pending_ack(), None);
    }

    #[test]
    fn synchronized_state_drops_out_of_window_rst() {
        let mut socket = established_socket();
        let receive_next_before = socket.receive_next;
        let far_sequence = receive_next_before
            .wrapping_add(u32::from(socket.advertised_window()) << TCP_LOCAL_WINDOW_SCALE)
            .wrapping_add(1);

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: far_sequence,
                acknowledgement: 0,
                flags: TcpFlags::RST,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(outcome, TcpSegmentOutcome::default());
        assert_eq!(socket.state(), TcpState::Established);
        assert_eq!(socket.receive_next, receive_next_before);
        assert_eq!(socket.pending_ack(), None);
    }

    #[test]
    fn synchronized_state_accepts_in_window_rst() {
        let mut socket = established_socket();
        let receive_next_before = socket.receive_next;

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: receive_next_before,
                acknowledgement: 0,
                flags: TcpFlags::RST,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(socket.state(), TcpState::Closed);
        assert!(outcome.recovery.is_some());
    }

    #[test]
    fn synchronized_state_closes_on_in_window_syn() {
        let mut socket = established_socket();
        let receive_next_before = socket.receive_next;

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: receive_next_before,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(socket.state(), TcpState::Closed);
        assert!(outcome.recovery.is_some());
    }

    #[test]
    fn synchronized_state_rejects_out_of_window_syn_with_ack() {
        let mut socket = established_socket();
        let receive_next_before = socket.receive_next;
        let far_sequence = receive_next_before
            .wrapping_add(u32::from(socket.advertised_window()) << TCP_LOCAL_WINDOW_SCALE)
            .wrapping_add(1);

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: far_sequence,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(outcome, TcpSegmentOutcome::default());
        assert_eq!(socket.state(), TcpState::Established);
        let ack = socket
            .pending_ack()
            .expect("out-of-window SYN must request an ACK");
        assert_eq!(ack.acknowledgement, receive_next_before);
    }

    #[test]
    fn peer_window_scale_expands_tracked_receive_window() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
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
        // RFC 7323 §2.2: the SYN-ACK's own window field is never scaled.
        assert_eq!(socket.peer_receive_window(), 4);

        let _ = deliver_segment(
            &mut socket,
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
            TCP_INITIAL_RTO_NANOS,
        );

        assert_eq!(socket.peer_receive_window(), 32);
        assert_eq!(socket.queue_send(b"abcdefghijklmnopqrstuvwxyz"), 26);
        let segment = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("scaled peer receive window should allow data");

        assert_eq!(segment.payload.len(), 26);
    }

    #[test]
    fn advertised_window_stays_unscaled_without_peer_wscale() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
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

        assert_eq!(socket.state(), TcpState::Established);
        assert_eq!(socket.local_window_scale(), 0);
        // Large enough that the pre-fix scaled window drops below the
        // u16 cap and would fail the unscaled assertion.
        let payload = [0_u8; 1460];
        let _ = deliver_segment(
            &mut socket,
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
        assert_eq!(
            socket.advertised_window(),
            u16::MAX,
            "without negotiated scaling the raw window must not be shifted"
        );
    }

    #[test]
    fn advertised_window_scales_after_peer_wscale() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 100,
                acknowledgement: 8,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::parse(&[3, 3, 3, 0]).expect("window scale should parse"),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS,
        );

        assert_eq!(socket.state(), TcpState::Established);
        assert_eq!(socket.local_window_scale(), TCP_LOCAL_WINDOW_SCALE);
        assert_eq!(
            socket.advertised_window(),
            receive_window_size(0, TCP_LOCAL_WINDOW_SCALE),
        );
    }

    #[test]
    fn syn_ack_window_is_unscaled_even_when_negotiated() {
        let mut child = TcpSocket::accept(endpoint(80), peer(49152), 101, 700, BbrV3::new(1460));
        child.record_peer_options(TcpPacket {
            source_port: 49152,
            destination_port: 80,
            sequence: 100,
            acknowledgement: 0,
            flags: TcpFlags::SYN,
            window_size: u16::MAX,
            options: TcpOptions::parse(&[3, 3, 3, 0]).expect("window scale should parse"),
            payload: &[],
        });

        assert_eq!(child.local_window_scale(), TCP_LOCAL_WINDOW_SCALE);
        let syn_ack = child
            .pending_syn_ack()
            .expect("accepted child owes a SYN-ACK");
        assert_eq!(
            syn_ack.window_size,
            receive_window_size(0, 0),
            "the window field in a SYN segment is never scaled"
        );
    }

    #[test]
    fn peer_receive_window_caps_transmitted_send_bytes() {
        let mut socket = established_socket();
        let _ = deliver_segment(
            &mut socket,
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
        assert_eq!(socket.queue_send(b"abcdefghij"), 10);
        let first = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 2,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("peer receive window should allow a capped segment");
        assert_eq!(first.payload.as_ref(), b"abcd");
        assert_eq!(
            socket.take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 3,
                TcpSegmentBudget::wire_segments(usize::MAX)
            ),
            None
        );
    }

    #[test]
    fn zero_peer_receive_window_buffers_until_window_update() {
        let mut socket = established_socket();
        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: 0,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(socket.peer_receive_window(), 0);
        assert_eq!(socket.queue_send(b"abcdefghij"), 10);
        assert_eq!(
            socket.take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 2,
                TcpSegmentBudget::wire_segments(usize::MAX)
            ),
            None
        );

        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: 10,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 3,
        );
        let segment = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 4,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("window update should release locally buffered bytes");

        assert_eq!(segment.payload.as_ref(), b"abcdefghij");
    }

    #[test]
    fn zero_peer_receive_window_probe_sends_one_byte_after_rto() {
        let mut socket = established_socket();
        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: 0,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );
        assert_eq!(socket.queue_send(b"abc"), 3);

        let rto = socket.retransmission_timer.timeout_nanos();
        let first_deadline = TCP_INITIAL_RTO_NANOS + 2 + rto;
        assert_eq!(
            socket.take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 2,
                TcpSegmentBudget::wire_segments(usize::MAX)
            ),
            None
        );
        assert_eq!(socket.next_deadline_nanos(), Some(first_deadline));

        let probe = socket
            .take_transmit_segment(first_deadline, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("persist timer should send a one-byte probe");
        assert_eq!(probe.payload.as_ref(), b"a");
        assert_eq!(probe.header.sequence, 8);
        assert_eq!(probe.sequence_len, 1);
        assert!(!probe.retransmission);
        assert_eq!(socket.bytes_in_flight, 0);
        assert_eq!(socket.send_next, 8);

        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: 0,
                options: TcpOptions::empty(),
                payload: &[],
            },
            first_deadline + 1,
        );
        assert_eq!(socket.next_deadline_nanos(), Some(first_deadline + rto * 2));
        assert_eq!(
            socket.take_transmit_segment(
                first_deadline + rto,
                TcpSegmentBudget::wire_segments(usize::MAX)
            ),
            None
        );
        let second = socket
            .take_transmit_segment(
                first_deadline + rto * 2,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("persist probe should exponentially back off");
        assert_eq!(second.payload.as_ref(), b"a");
        assert_eq!(second.header.sequence, 8);
    }

    #[test]
    fn zero_peer_receive_window_probe_clears_when_window_opens() {
        let mut socket = established_socket();
        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: 0,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );
        assert_eq!(socket.queue_send(b"abc"), 3);
        let deadline = TCP_INITIAL_RTO_NANOS + 2 + socket.retransmission_timer.timeout_nanos();
        assert_eq!(
            socket.take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 2,
                TcpSegmentBudget::wire_segments(usize::MAX)
            ),
            None
        );
        let _probe = socket
            .take_transmit_segment(deadline, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("persist timer should send a one-byte probe");

        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 9,
                flags: TcpFlags::ACK,
                window_size: 32,
                options: TcpOptions::empty(),
                payload: &[],
            },
            deadline + 1,
        );
        assert_eq!(socket.peer_receive_window(), 32);
        assert_eq!(socket.next_deadline_nanos(), None);

        let segment = socket
            .take_transmit_segment(deadline + 2, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("opened window should transmit queued data normally");
        assert_eq!(segment.payload.as_ref(), b"bc");
        assert_eq!(segment.header.sequence, 9);
    }

    #[test]
    fn zero_peer_receive_window_probes_queued_fin() {
        let mut socket = established_socket();
        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: 0,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );
        socket.close_send();
        assert_eq!(socket.state(), TcpState::FinWait1);

        let deadline = TCP_INITIAL_RTO_NANOS + 2 + socket.retransmission_timer.timeout_nanos();
        assert_eq!(
            socket.take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 2,
                TcpSegmentBudget::wire_segments(usize::MAX)
            ),
            None
        );
        let probe = socket
            .take_transmit_segment(deadline, TcpSegmentBudget::wire_segments(usize::MAX))
            .expect("persist timer should probe queued FIN");
        assert!(probe.payload.is_empty());
        assert!(probe.header.flags.contains(TcpFlags::FIN));
        assert_eq!(probe.header.sequence, 8);
        assert_eq!(socket.send_next, 8);
        assert!(!socket.fin_queued);

        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 101,
                acknowledgement: 9,
                flags: TcpFlags::ACK,
                window_size: 0,
                options: TcpOptions::empty(),
                payload: &[],
            },
            deadline + 1,
        );
        assert_eq!(socket.state(), TcpState::FinWait2);
        assert_eq!(socket.send_next, 9);
        assert!(socket.fin_queued);
    }

    #[test]
    fn local_transmit_buffer_capacity_preserves_unqueued_tail() {
        let mut socket = established_socket();
        let mut payload = BytesMut::with_capacity(TCP_TRANSMIT_BUFFER_BYTES + 1);
        payload.resize(TCP_TRANSMIT_BUFFER_BYTES + 1, 0x5a);
        let mut payload = payload.freeze();

        assert_eq!(
            socket.queue_send_bytes(&mut payload),
            TCP_TRANSMIT_BUFFER_BYTES
        );
        assert_eq!(payload.len(), 1);
        assert_eq!(socket.queue_send(b"x"), 0);

        let segment = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("local transmit buffer should drain through normal send path");
        assert_eq!(segment.payload.len(), TCP_RECEIVE_SEGMENT_BYTES);
        assert_eq!(socket.queue_send(b"x"), 1);
    }

    #[test]
    fn pacing_rate_delays_next_data_segment() {
        let mut socket = established_fixed_pacing_socket(PacingRate::from_bytes_per_second(1_000));
        assert_eq!(socket.queue_send(b"abcdefgh"), 8);
        let first = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("first paced segment should transmit immediately");
        assert_eq!(first.payload.as_ref(), b"abcdefgh");
        assert_eq!(socket.queue_send(b"ijkl"), 4);
        assert_eq!(
            socket.next_deadline_nanos(),
            Some(TCP_INITIAL_RTO_NANOS + 1 + 8_000_000)
        );
        assert_eq!(
            socket.take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 2,
                TcpSegmentBudget::wire_segments(usize::MAX)
            ),
            None
        );

        let second = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1 + 8_000_000,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("paced deadline should release the next segment");
        assert_eq!(second.payload.as_ref(), b"ijkl");
    }

    #[test]
    fn paced_close_waits_for_queued_data_before_fin() {
        let mut socket = established_fixed_pacing_socket(PacingRate::from_bytes_per_second(1_000));
        assert_eq!(socket.queue_send(b"abcdefgh"), 8);
        let first = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("first paced segment should transmit immediately");
        assert_eq!(first.payload.as_ref(), b"abcdefgh");
        assert_eq!(socket.queue_send(b"ijkl"), 4);

        socket.close_send();
        assert_eq!(socket.state(), TcpState::FinWait1);
        assert_eq!(
            socket.take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 2,
                TcpSegmentBudget::wire_segments(usize::MAX)
            ),
            None
        );

        let second = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1 + 8_000_000,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("paced data must be transmitted before FIN");
        assert_eq!(second.payload.as_ref(), b"ijkl");

        let fin = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1 + 12_000_000,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
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
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("active close must drain queued data before FIN");
        assert_eq!(data.payload.as_ref(), b"hi");
        assert!(data.header.flags.contains(TcpFlags::PSH));
        assert!(!data.header.flags.contains(TcpFlags::FIN));

        let fin = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 2,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("active close must queue FIN after data");
        assert!(fin.payload.is_empty());
        assert_eq!(fin.sequence_len, 1);
        assert!(fin.header.flags.contains(TcpFlags::FIN));
        assert_eq!(
            fin.header.sequence,
            data.header.sequence + data.sequence_len
        );

        let _ = deliver_segment(
            &mut socket,
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
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("active close must queue FIN");
        let _ = deliver_segment(
            &mut socket,
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
        let _ = deliver_segment(
            &mut socket,
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
    fn time_wait_out_of_window_fin_does_not_restart_deadline() {
        let mut socket = established_socket();
        socket.close_send();
        let fin = socket
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 1,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("active close must queue FIN");
        let _ = deliver_segment(
            &mut socket,
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
        let fin_received_at = TCP_INITIAL_RTO_NANOS + 3;
        let _ = deliver_segment(
            &mut socket,
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
        let original_deadline = socket
            .next_deadline_nanos()
            .expect("TIME_WAIT should arm a deadline");
        socket.mark_ack_queued();
        let far_sequence = socket
            .receive_next
            .wrapping_add(u32::from(socket.advertised_window()) << TCP_LOCAL_WINDOW_SCALE)
            .wrapping_add(1);

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: far_sequence,
                acknowledgement: fin.header.sequence + 1,
                flags: TcpFlags::ACK.union(TcpFlags::FIN),
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            fin_received_at + 1,
        );

        assert_eq!(outcome, TcpSegmentOutcome::default());
        assert_eq!(socket.state(), TcpState::TimeWait);
        assert_eq!(socket.next_deadline_nanos(), Some(original_deadline));
        assert!(socket.pending_ack().is_none());
    }

    #[test]
    fn passive_close_sends_fin_and_closes_after_fin_ack() {
        let mut socket = established_socket();
        let _ = deliver_segment(
            &mut socket,
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
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 2,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("passive close must queue FIN from LAST-ACK");
        assert!(fin.header.flags.contains(TcpFlags::FIN));

        let _ = deliver_segment(
            &mut socket,
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
        let _ = deliver_segment(
            &mut socket,
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
            .take_transmit_segment(
                TCP_INITIAL_RTO_NANOS + 2,
                TcpSegmentBudget::wire_segments(usize::MAX),
            )
            .expect("queued data must transmit");
        assert!(segment.header.flags.contains(TcpFlags::ACK));
        assert_eq!(socket.pending_ack(), None);
    }

    #[test]
    fn receive_window_tracks_queued_payload_segments() {
        let mut socket = established_scaled_socket();
        let open_window = socket.advertised_window();
        let payload = [0u8; TCP_RECEIVE_SEGMENT_BYTES];
        let queued_segments = 4;
        for index in 0..queued_segments {
            let _ = deliver_segment(
                &mut socket,
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
            socket.receive(TCP_RECEIVE_SEGMENT_BYTES, 2).as_deref(),
            Some(&payload[..])
        );
        assert!(socket.advertised_window() < open_window);
        for _ in 1..queued_segments {
            assert_eq!(
                socket.receive(TCP_RECEIVE_SEGMENT_BYTES, 4).as_deref(),
                Some(&payload[..])
            );
        }
        assert_eq!(socket.advertised_window(), open_window);
    }

    #[test]
    fn full_size_receive_payloads_ack_every_two_segments() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
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
        let _ = deliver_segment(
            &mut socket,
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

        let _ = deliver_segment(
            &mut socket,
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
        let _ = deliver_segment(
            &mut socket,
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
        let _ = deliver_segment(
            &mut socket,
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

        let _ = deliver_segment(
            &mut socket,
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
        let _ = deliver_segment(
            &mut socket,
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
            let _ = deliver_segment(
                &mut socket,
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

        assert_eq!(socket.receive(7, 2).as_deref(), Some(b"abcdefg".as_slice()));
        assert_eq!(socket.receive(7, 3).as_deref(), Some(b"hij".as_slice()));
    }

    #[test]
    fn receive_single_segment_does_not_allocate_merge_buffer() {
        let mut socket = established_socket();
        let _ = deliver_segment(
            &mut socket,
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
            socket.receive(usize::MAX, 2).as_deref(),
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
            let _ = deliver_segment(
                &mut socket,
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
    fn receive_queue_spills_without_reordering_segments() {
        let mut queue = TcpReceiveQueue::new();
        for value in 0..(TCP_RECEIVE_INLINE_SEGMENTS + 4) {
            queue
                .push_back(BytesMut::from([value as u8].as_slice()))
                .expect("receive queue should accept test segment");
        }

        assert_eq!(queue.len(), TCP_RECEIVE_INLINE_SEGMENTS + 4);
        assert_eq!(queue.overflow_len(), 4);
        for value in 0..(TCP_RECEIVE_INLINE_SEGMENTS + 4) {
            let bytes = queue
                .pop_front()
                .expect("receive queue should preserve queued segment");
            assert_eq!(bytes.as_ref(), &[value as u8]);
        }
        assert!(queue.is_empty());
    }

    #[test]
    fn receive_queue_push_front_preserves_overflow_order() {
        let mut queue = TcpReceiveQueue::new();
        for value in 1..=(TCP_RECEIVE_INLINE_SEGMENTS + 2) {
            queue
                .push_back(BytesMut::from([value as u8].as_slice()))
                .expect("receive queue should accept test segment");
        }
        queue
            .push_front(BytesMut::from([0].as_slice()))
            .expect("receive queue should accept split head segment");

        for value in 0..=(TCP_RECEIVE_INLINE_SEGMENTS + 2) {
            let bytes = queue
                .pop_front()
                .expect("receive queue should preserve front insertion order");
            assert_eq!(bytes.as_ref(), &[value as u8]);
        }
        assert!(queue.is_empty());
    }

    #[test]
    fn out_of_order_receive_payload_reassembles_after_gap_arrives() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
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

        let _ = deliver_segment(
            &mut socket,
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
        assert_eq!(socket.receive(16, 2), None);

        let _ = deliver_segment(
            &mut socket,
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
        assert_eq!(socket.receive(16, 3).as_deref(), Some(b"abcdef".as_slice()));
    }

    #[test]
    fn out_of_window_receive_payload_is_rejected_with_ack() {
        let mut socket = established_socket();
        let receive_next_before = socket.receive_next;
        let far_sequence = receive_next_before
            .wrapping_add(u32::from(socket.advertised_window()) << TCP_LOCAL_WINDOW_SCALE)
            .wrapping_add(1);

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: far_sequence,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: b"abc",
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(outcome, TcpSegmentOutcome::default());
        assert_eq!(socket.receive_next, receive_next_before);
        assert_eq!(socket.out_of_order_queued_bytes, 0);
        assert_eq!(socket.receive(16, TCP_INITIAL_RTO_NANOS + 2), None);
        let ack = socket
            .pending_ack()
            .expect("out-of-window payload must request an ACK");
        assert_eq!(ack.acknowledgement, receive_next_before);
    }

    #[test]
    fn in_window_out_of_order_receive_payload_is_retained() {
        let mut socket = established_socket();
        socket.mark_ack_queued();
        let receive_next_before = socket.receive_next;

        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: receive_next_before + 3,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: b"def",
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        assert_eq!(socket.receive_next, receive_next_before);
        assert_eq!(socket.out_of_order_queued_bytes, 3);
        let ack = socket
            .pending_ack()
            .expect("out-of-order payload must request a duplicate ACK");
        assert_eq!(ack.acknowledgement, receive_next_before);
    }

    #[test]
    fn zero_receive_window_rejects_payload_but_accepts_empty_ack_at_receive_next() {
        let mut socket = established_socket();
        let payload = [0u8; TCP_RECEIVE_SEGMENT_BYTES];
        let mut sequence = socket.receive_next;
        while socket.advertised_window() != 0 {
            let _ = deliver_segment(
                &mut socket,
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence,
                    acknowledgement: 8,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload: &payload,
                },
                TCP_INITIAL_RTO_NANOS + u64::from(sequence),
            );
            sequence = sequence.wrapping_add(TCP_RECEIVE_SEGMENT_BYTES as u32);
        }
        socket.mark_ack_queued();
        let receive_next_before = socket.receive_next;
        let queued_before = socket.receive_queued_bytes;

        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: receive_next_before,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: b"x",
            },
            TCP_INITIAL_RTO_NANOS + 9,
        );

        assert_eq!(socket.receive_next, receive_next_before);
        assert_eq!(socket.receive_queued_bytes, queued_before);
        assert!(socket.pending_ack().is_some());
        socket.mark_ack_queued();

        let outcome = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: receive_next_before,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: 4,
                options: TcpOptions::empty(),
                payload: &[],
            },
            TCP_INITIAL_RTO_NANOS + 10,
        );

        assert!(!outcome.receive_backpressure);
        assert_eq!(socket.peer_receive_window(), 4);
    }

    #[test]
    fn out_of_order_receive_payload_drains_sorted_segments() {
        let mut socket = established_socket();
        socket.mark_ack_queued();

        for (sequence, payload) in [(107, b"ghi".as_slice()), (104, b"def".as_slice())] {
            let _ = deliver_segment(
                &mut socket,
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
        assert_eq!(socket.receive(16, 2), None);

        let _ = deliver_segment(
            &mut socket,
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

        assert_eq!(
            socket.receive(16, 3).as_deref(),
            Some(b"abcdefghi".as_slice())
        );
    }

    #[test]
    fn out_of_order_drain_coalesces_receive_queue_tail() {
        let mut socket = established_socket();
        socket.mark_ack_queued();

        let _ = deliver_segment(
            &mut socket,
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: 107,
                acknowledgement: 8,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: b"ghi",
            },
            TCP_INITIAL_RTO_NANOS + 1,
        );

        let _ = deliver_segment(
            &mut socket,
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

        assert_eq!(
            socket
                .receive_queue
                .as_ref()
                .expect("receive queue must be allocated")
                .len(),
            1
        );
        assert_eq!(socket.receive_queued_bytes, 3);

        let _ = deliver_segment(
            &mut socket,
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
            TCP_INITIAL_RTO_NANOS + 3,
        );

        let queue = socket
            .receive_queue
            .as_ref()
            .expect("receive queue must remain allocated");
        assert_eq!(queue.len(), 1);
        assert_eq!(socket.receive_queued_bytes, 9);
        assert_eq!(
            socket.receive(16, 4).as_deref(),
            Some(b"abcdefghi".as_slice())
        );
    }

    #[test]
    fn pending_ack_options_report_out_of_order_sack_blocks() {
        let mut socket = TcpSocket::connect(endpoint(49152), peer(80), 7, BbrV3::new(1460));
        socket.mark_syn_queued(0);
        let _ = deliver_segment(
            &mut socket,
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

        let _ = deliver_segment(
            &mut socket,
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
        let _ = deliver_segment(
            &mut socket,
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
        let mut socket = established_scaled_socket();

        let payload = [0u8; TCP_RECEIVE_SEGMENT_BYTES];
        let queued_segments =
            MAX_TCP_RECEIVE_SEGMENTS - (u16::MAX as usize / TCP_RECEIVE_SEGMENT_BYTES) + 4;
        for index in 0..queued_segments {
            let _ = deliver_segment(
                &mut socket,
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

        // Drain in multiples of the 16-byte scale unit so every drain
        // frees exactly the same number of advertised-window units and
        // the byte-threshold crossing lands on a deterministic step.
        let drain_bytes = TCP_RECEIVE_SEGMENT_BYTES & !((1 << TCP_LOCAL_WINDOW_SCALE) - 1);
        let update_segments = usize::from(TCP_WINDOW_UPDATE_BYTES).div_ceil(drain_bytes);
        for _ in 1..update_segments {
            assert_eq!(
                socket.receive(drain_bytes, 2).map(|bytes| bytes.len()),
                Some(drain_bytes)
            );
            assert_eq!(socket.pending_ack(), None);
        }
        assert_eq!(
            socket.receive(drain_bytes, 4).map(|bytes| bytes.len()),
            Some(drain_bytes)
        );
        assert!(socket.pending_ack().is_some());
    }

    /// A hole in the stream owes the peer one duplicate ACK per
    /// out-of-order segment, not one per batch.
    ///
    /// The stack is driven once per received burst, so an ACK the socket
    /// only records as a flag collapses the whole burst into a single
    /// acknowledgement. A peer without SACK — every peer that did not
    /// negotiate it, QEMU's slirp among them — then never counts the
    /// three duplicates that trigger a fast retransmit and recovers the
    /// hole on its retransmission timer instead, which cost 1.4–1.9 s
    /// per loss on the aarch64 bench lane.
    #[test]
    fn out_of_order_segments_owe_one_duplicate_ack_each() {
        let mut socket = established_socket();
        let payload = [0u8; 100];

        // In-order data first, so the peer already holds the cumulative
        // acknowledgement every later duplicate repeats.
        let _ = deliver_segment(
            &mut socket,
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
            1,
        );
        while socket.pending_ack().is_some() {
            socket.mark_ack_queued();
        }
        assert_eq!(socket.pending_ack_count(), 0);

        // The segment at 201 is the hole; everything after it arrives.
        for index in 0..4u32 {
            let _ = deliver_segment(
                &mut socket,
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 301 + index * 100,
                    acknowledgement: 8,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload: &payload,
                },
                2 + u64::from(index),
            );
        }

        assert_eq!(socket.pending_ack_count(), 4);
        let mut acknowledgements = 0;
        while let Some(header) = socket.pending_ack() {
            assert_eq!(header.acknowledgement, 201);
            socket.mark_ack_queued();
            acknowledgements += 1;
        }
        assert_eq!(acknowledgements, 4);
    }

    /// The queue of owed acknowledgements is bounded: a peer that keeps
    /// pushing segments into a hole cannot make the socket owe an
    /// unbounded burst of frames.
    #[test]
    fn owed_duplicate_acks_are_capped() {
        let mut socket = established_socket();
        let payload = [0u8; 100];
        for index in 0..(u32::from(TCP_MAX_QUEUED_ACKS) + 8) {
            let _ = deliver_segment(
                &mut socket,
                TcpPacket {
                    source_port: 80,
                    destination_port: 49152,
                    sequence: 301 + index * 100,
                    acknowledgement: 8,
                    flags: TcpFlags::ACK,
                    window_size: u16::MAX,
                    options: TcpOptions::empty(),
                    payload: &payload,
                },
                1 + u64::from(index),
            );
        }
        assert_eq!(socket.pending_ack_count(), TCP_MAX_QUEUED_ACKS);
    }
}
