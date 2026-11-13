//! Congestion controllers driven through a simulated path.
//!
//! A [`TcpSocket`] sends a bulk transfer through a first-in first-out
//! bottleneck of fixed rate, with an unbounded or a tail-drop queue, a
//! fixed propagation delay each way, and a receiver model that
//! acknowledges by one of three policies. Time is simulated: every event
//! carries the nanosecond it happens at, and the socket sees exactly the
//! timestamps a real stack would. The receiver negotiates neither SACK
//! nor timestamps, so loss recovery runs on duplicate ACKs alone.
//!
//! The sender side follows the stack's own order: a due retransmission
//! first, then new data, until the socket has nothing it may send.

extern crate std;

use alloc::collections::{BTreeMap, BinaryHeap};
use alloc::vec::Vec;
use core::cell::Cell;
use core::cmp::Reverse;

use bytes::Bytes;

use crate::congestion::BbrState;
use crate::tcp::TCP_MIN_RTO_NANOS;
use crate::{
    BbrV3, CongestionControl, Cubic, IpAddress, Ipv4Address, TcpEndpoint, TcpFlags, TcpOptions,
    TcpPacket, TcpSegmentBudget, TcpSocket, TcpState, TcpTransmitSegment,
};

const MSS: u32 = 1460;
const NANOS_PER_SECOND: u64 = 1_000_000_000;
const MILLISECOND: u64 = 1_000_000;
/// The receiver's delayed-ACK timer, RFC 1122's common 40 ms.
const DELAYED_ACK_NANOS: u64 = 40 * MILLISECOND;
/// How long a batching receive path holds acknowledgements before it
/// hands them up whatever their count: one poll interval.
const BATCH_FLUSH_NANOS: u64 = MILLISECOND;
/// The peer's sequence number after its SYN.
const PEER_SEQUENCE: u32 = 101;
/// The socket's initial sequence number.
const INITIAL_SEQUENCE: u32 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AckPolicy {
    /// One acknowledgement per segment, sent the moment it arrives.
    EverySegment,
    /// RFC 1122 delayed ACKs: every second full segment, else after
    /// [`DELAYED_ACK_NANOS`].
    EverySecondSegment,
    /// One acknowledgement per segment, handed to the sender in groups
    /// of this many that arrive at the same instant: a batching receive
    /// path on either end.
    Batched(usize),
}

#[derive(Clone, Copy, Debug)]
struct Path {
    bottleneck_bytes_per_second: u64,
    /// Two-way propagation delay, half each way.
    rtt_nanos: u64,
    ack_policy: AckPolicy,
    budget: TcpSegmentBudget,
    /// The bottleneck's tail-drop queue limit; `None` queues without
    /// bound.
    buffer_bytes: Option<u64>,
}

impl Path {
    /// 50 Mbit/s over 20 ms: a BDP of 125 kB, about 86 segments.
    const fn wan(ack_policy: AckPolicy) -> Self {
        Self {
            bottleneck_bytes_per_second: 6_250_000,
            rtt_nanos: 20 * MILLISECOND,
            ack_policy,
            budget: TcpSegmentBudget::wire_segments(TCP_WIRE_SEGMENT),
            buffer_bytes: None,
        }
    }

    const fn with_buffer(self, buffer_bytes: u64) -> Self {
        Self {
            buffer_bytes: Some(buffer_bytes),
            ..self
        }
    }

    /// The sender coalesces a whole window per transmit pass into one
    /// segmentation-offload burst, every segment of it under one send
    /// timestamp.
    const fn coalescing(self) -> Self {
        Self {
            budget: TcpSegmentBudget::coalesced(TCP_WIRE_SEGMENT, 64 * 1024),
            ..self
        }
    }

    fn serialization_nanos(&self, bytes: u32) -> u64 {
        u64::from(bytes) * NANOS_PER_SECOND / self.bottleneck_bytes_per_second
    }

    /// The bandwidth-delay product of the empty path: the bottleneck
    /// rate over the shortest round trip a segment can see, which
    /// includes its own serialization.
    fn bdp_bytes(&self) -> u64 {
        let rtt = self.rtt_nanos + self.serialization_nanos(MSS);
        self.bottleneck_bytes_per_second * rtt / NANOS_PER_SECOND
    }
}

/// TCP header plus one MSS of payload: what one wire segment carries.
const TCP_WIRE_SEGMENT: usize = 20 + MSS as usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    /// Data reaches the receiver.
    Arrival { sequence: u32, len: u32 },
    /// An acknowledgement reaches the sender.
    Ack { acknowledgement: u32 },
}

#[derive(Debug, Default)]
struct Receiver {
    next_expected: u32,
    /// Out-of-order data, start to end. The transfers here stay far
    /// below 4 GiB, so sequence numbers compare as plain integers.
    out_of_order: BTreeMap<u32, u32>,
    unacknowledged_segments: u32,
    held: Vec<u32>,
    timer: Option<u64>,
    /// In-order bytes received, with the time of each advance.
    delivered: Vec<(u64, u64)>,
    delivered_total: u64,
}

struct Sim<C, D>
where
    C: CongestionControl,
    D: FnMut(&TcpSocket<C>, u64, u32, bool) -> bool,
{
    socket: TcpSocket<C>,
    path: Path,
    /// Whether the bottleneck drops the segment starting at `sequence`
    /// the socket is putting on the wire now.
    drop: D,
    now: u64,
    link_free_at: u64,
    events: BinaryHeap<Reverse<(u64, u64, Event)>>,
    next_event_id: u64,
    receiver: Receiver,
    payload: Bytes,
    /// Segments the bottleneck's full buffer dropped.
    tail_drops: u64,
}

impl<C, D> Sim<C, D>
where
    C: CongestionControl,
    D: FnMut(&TcpSocket<C>, u64, u32, bool) -> bool,
{
    fn new(congestion: C, path: Path, drop: D) -> Self {
        let local = TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::new([192, 0, 2, 1])),
            port: 49152,
        };
        let remote = TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::new([192, 0, 2, 2])),
            port: 80,
        };
        let mut socket = TcpSocket::connect(local, remote, INITIAL_SEQUENCE, congestion);
        socket.mark_syn_queued(0);
        // The SYN-ACK comes back a round trip later with a window scale
        // of 7: an 8 MiB window, so the receiver never limits the
        // sender.
        let now = path.rtt_nanos;
        let _ = socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: PEER_SEQUENCE - 1,
                acknowledgement: INITIAL_SEQUENCE + 1,
                flags: TcpFlags::SYN.union(TcpFlags::ACK),
                window_size: u16::MAX,
                options: TcpOptions::parse(&[3, 3, 7, 0]).expect("window scale option parses"),
                payload: &[],
            },
            Bytes::new(),
            now,
        );
        socket.mark_ack_queued();
        assert_eq!(socket.state(), TcpState::Established);
        Self {
            socket,
            path,
            drop,
            now,
            link_free_at: now,
            events: BinaryHeap::new(),
            next_event_id: 0,
            receiver: Receiver {
                next_expected: INITIAL_SEQUENCE + 1,
                ..Receiver::default()
            },
            payload: Bytes::from(alloc::vec![0x5a; 64 * 1024]),
            tail_drops: 0,
        }
    }

    fn schedule(&mut self, at: u64, event: Event) {
        self.events.push(Reverse((at, self.next_event_id, event)));
        self.next_event_id += 1;
    }

    /// Keeps the application's data queued, so the flow is never
    /// application-limited.
    fn fill_application(&mut self) {
        while self.socket.send_capacity_bytes() != 0 {
            let mut chunk = self.payload.clone();
            chunk.truncate(self.socket.send_capacity_bytes());
            if self.socket.queue_send_bytes(&mut chunk) == 0 {
                break;
            }
        }
    }

    fn run_sender(&mut self) {
        loop {
            if let Some(segment) = self
                .socket
                .pending_retransmission_with_mtu(self.now, self.path.budget.wire_segment())
            {
                self.socket
                    .mark_retransmission_queued(segment.header.sequence, self.now);
                assert_ne!(
                    self.socket.state(),
                    TcpState::Closed,
                    "the retransmission limit closed the connection"
                );
                self.transmit(&segment);
                continue;
            }
            self.fill_application();
            let Some(segment) = self
                .socket
                .take_transmit_segment(self.now, self.path.budget)
            else {
                return;
            };
            self.transmit(&segment);
        }
    }

    fn transmit(&mut self, segment: &TcpTransmitSegment) {
        let piece = if segment.segment_bytes == 0 {
            segment.sequence_len
        } else {
            segment.segment_bytes
        };
        let mut offset = 0;
        while offset < segment.sequence_len {
            let len = piece.min(segment.sequence_len - offset);
            let sequence = segment.header.sequence + offset;
            offset += len;
            let queued_bytes = self.link_free_at.saturating_sub(self.now)
                * self.path.bottleneck_bytes_per_second
                / NANOS_PER_SECOND;
            if self
                .path
                .buffer_bytes
                .is_some_and(|buffer| queued_bytes + u64::from(len) > buffer)
            {
                self.tail_drops += 1;
                continue;
            }
            let start = self.link_free_at.max(self.now);
            self.link_free_at = start + self.path.serialization_nanos(len);
            if !(self.drop)(&self.socket, self.now, sequence, segment.retransmission) {
                let arrival = self.link_free_at + self.path.rtt_nanos / 2;
                self.schedule(arrival, Event::Arrival { sequence, len });
            }
        }
    }

    fn acknowledge(&mut self) {
        let acknowledgement = self.receiver.next_expected;
        self.receiver.unacknowledged_segments = 0;
        self.receiver.timer = None;
        self.schedule(
            self.now + self.path.rtt_nanos / 2,
            Event::Ack { acknowledgement },
        );
    }

    fn flush_held(&mut self) {
        let at = self.now + self.path.rtt_nanos / 2;
        for acknowledgement in core::mem::take(&mut self.receiver.held) {
            self.schedule(at, Event::Ack { acknowledgement });
        }
        self.receiver.timer = None;
    }

    fn on_arrival(&mut self, sequence: u32, len: u32) {
        let receiver = &mut self.receiver;
        let had_hole = !receiver.out_of_order.is_empty();
        let in_order = sequence == receiver.next_expected;
        if in_order {
            receiver.next_expected += len;
            while let Some((&start, &end)) = receiver.out_of_order.first_key_value()
                && start <= receiver.next_expected
            {
                receiver.out_of_order.pop_first();
                receiver.next_expected = receiver.next_expected.max(end);
            }
            let total = u64::from(receiver.next_expected - (INITIAL_SEQUENCE + 1));
            receiver.delivered_total = total;
            receiver.delivered.push((self.now, total));
        } else if sequence > receiver.next_expected {
            receiver.out_of_order.insert(sequence, sequence + len);
        }
        // RFC 5681 section 4.2: an out-of-order segment, a duplicate,
        // or one that fills a hole is acknowledged at once.
        let immediate = !in_order || had_hole;
        match self.path.ack_policy {
            AckPolicy::EverySegment => self.acknowledge(),
            AckPolicy::EverySecondSegment => {
                self.receiver.unacknowledged_segments += 1;
                if immediate || self.receiver.unacknowledged_segments >= 2 {
                    self.acknowledge();
                } else if self.receiver.timer.is_none() {
                    self.receiver.timer = Some(self.now + DELAYED_ACK_NANOS);
                }
            }
            AckPolicy::Batched(count) => {
                let acknowledgement = self.receiver.next_expected;
                self.receiver.held.push(acknowledgement);
                if immediate || self.receiver.held.len() >= count {
                    self.flush_held();
                } else if self.receiver.timer.is_none() {
                    self.receiver.timer = Some(self.now + BATCH_FLUSH_NANOS);
                }
            }
        }
    }

    fn on_ack(&mut self, acknowledgement: u32) {
        let _ = self.socket.on_segment(
            TcpPacket {
                source_port: 80,
                destination_port: 49152,
                sequence: PEER_SEQUENCE,
                acknowledgement,
                flags: TcpFlags::ACK,
                window_size: u16::MAX,
                options: TcpOptions::empty(),
                payload: &[],
            },
            Bytes::new(),
            self.now,
        );
    }

    /// Runs the path until `until`, calling `observe` after every
    /// acknowledgement the socket processes.
    fn run_until(&mut self, until: u64, mut observe: impl FnMut(&TcpSocket<C>, u64)) {
        while self.now < until {
            self.run_sender();
            let next_event = self.events.peek().map(|Reverse((at, _, _))| *at);
            // The sender has sent everything it may, so a deadline that is
            // already due would be a timer the stack fires for nothing,
            // over and over.
            let next_deadline = self.socket.next_deadline_nanos();
            assert!(
                next_deadline.is_none_or(|deadline| deadline > self.now),
                "the socket reports a deadline of {next_deadline:?} at {} with nothing to send",
                self.now
            );
            let next = [next_event, next_deadline, self.receiver.timer]
                .into_iter()
                .flatten()
                .min()
                .expect("a bulk transfer always has something scheduled");
            self.now = next;
            while let Some(Reverse((at, _, event))) = self.events.peek().copied()
                && at == self.now
            {
                self.events.pop();
                match event {
                    Event::Arrival { sequence, len } => self.on_arrival(sequence, len),
                    Event::Ack { acknowledgement } => {
                        self.on_ack(acknowledgement);
                        observe(&self.socket, self.now);
                    }
                }
            }
            if self.receiver.timer == Some(self.now) {
                match self.path.ack_policy {
                    AckPolicy::Batched(_) => self.flush_held(),
                    AckPolicy::EverySegment | AckPolicy::EverySecondSegment => self.acknowledge(),
                }
            }
        }
    }

    /// The receiver's in-order goodput between `from` and `to`, in
    /// bytes per second.
    fn goodput(&self, from: u64, to: u64) -> u64 {
        let at = |time: u64| {
            self.receiver
                .delivered
                .iter()
                .take_while(|(when, _)| *when <= time)
                .last()
                .map_or(0, |(_, total)| *total)
        };
        (at(to) - at(from)) * NANOS_PER_SECOND / (to - from)
    }
}

fn no_drops<C: CongestionControl>(_: &TcpSocket<C>, _: u64, _: u32, _: bool) -> bool {
    false
}

fn seconds(value: u64) -> u64 {
    value * NANOS_PER_SECOND
}

/// What one acknowledgement left the BBR model at.
#[derive(Clone, Copy, Debug)]
struct BbrObservation {
    now: u64,
    state: BbrState,
    round_count: u64,
    round_start: bool,
    cwnd: u64,
    pacing_rate: u64,
    max_bw: u64,
    min_rtt: Option<u64>,
    extra_acked: u64,
    cwnd_gain: u64,
    inflight_longterm: Option<u64>,
    inflight_shortterm: Option<u64>,
    in_loss_recovery: bool,
}

fn observe_bbr(socket: &TcpSocket<BbrV3>, now: u64) -> BbrObservation {
    let bbr = socket.congestion();
    BbrObservation {
        now,
        state: bbr.state(),
        round_count: bbr.round_count(),
        round_start: bbr.round_start(),
        cwnd: u64::from(bbr.congestion_window().bytes()),
        pacing_rate: bbr
            .pacing_rate()
            .expect("BBR always paces")
            .bytes_per_second(),
        max_bw: bbr.max_bw(),
        min_rtt: bbr.min_rtt(),
        extra_acked: bbr.extra_acked(),
        cwnd_gain: bbr.cwnd_gain_per_mille(),
        inflight_longterm: bbr.inflight_longterm(),
        inflight_shortterm: bbr.inflight_shortterm(),
        in_loss_recovery: bbr.in_loss_recovery(),
    }
}

fn run_bbr<D>(path: Path, until: u64, drop: D) -> (Vec<BbrObservation>, Sim<BbrV3, D>)
where
    D: FnMut(&TcpSocket<BbrV3>, u64, u32, bool) -> bool,
{
    let mut sim = Sim::new(BbrV3::new(MSS), path, drop);
    let mut observations = Vec::new();
    sim.run_until(until, |socket, now| {
        observations.push(observe_bbr(socket, now));
    });
    (observations, sim)
}

/// The model BBR converged to on `path`: its bandwidth estimate is the
/// bottleneck rate, its BDP the path's, its window the gain times that
/// BDP plus the aggregation it measured, and the flow moves the path's
/// rate.
fn assert_bbr_converged<D>(path: Path, observations: &[BbrObservation], sim: &Sim<BbrV3, D>)
where
    D: FnMut(&TcpSocket<BbrV3>, u64, u32, bool) -> bool,
{
    let rate = path.bottleneck_bytes_per_second;
    let last = observations.last().expect("the flow was acknowledged");
    assert!(
        matches!(
            last.state,
            BbrState::ProbeBwDown
                | BbrState::ProbeBwCruise
                | BbrState::ProbeBwRefill
                | BbrState::ProbeBwUp
        ),
        "a bulk flow settles in ProbeBW, not {:?}",
        last.state
    );
    // The estimate can sit a little above the bottleneck rate: a sample
    // whose acknowledgements were bunched by the receiver ends its ACK
    // phase early. The send phase bounds that (draft section
    // 4.1.1.2.4), and these paths stay within 5%.
    assert!(
        last.max_bw.abs_diff(rate) * 100 <= rate * 5,
        "max_bw {} B/s against a {rate} B/s bottleneck",
        last.max_bw
    );
    let min_rtt = last.min_rtt.expect("the flow measured a round trip");
    let bdp = last.max_bw * min_rtt / NANOS_PER_SECOND;
    let path_bdp = path.bdp_bytes();
    // The delayed-ACK receiver holds the first segment of each pair for
    // the second one, which the newest-segment round trip does not see;
    // the 40 ms timer never fires in a bulk flow.
    assert!(
        bdp.abs_diff(path_bdp) * 100 <= path_bdp * 10,
        "estimated BDP {bdp} B against the path's {path_bdp} B"
    );
    // Outside ProbeBW_UP the window is `BBR.DefaultCwndGain` (2) times
    // the BDP plus `BBR.extra_acked` (draft section 5.6.4.2), reached
    // by growing at most `RS.newly_acked` per ACK.
    let settled: Vec<_> = observations
        .iter()
        .filter(|observation| {
            observation.now + seconds(1) >= last.now
                && matches!(
                    observation.state,
                    BbrState::ProbeBwDown | BbrState::ProbeBwCruise | BbrState::ProbeBwRefill
                )
        })
        .collect();
    assert!(
        !settled.is_empty(),
        "the last second spent time outside ProbeBW_UP"
    );
    for observation in settled {
        let min_rtt = observation.min_rtt.expect("measured");
        let target = 2 * observation.max_bw * min_rtt / NANOS_PER_SECOND + observation.extra_acked;
        assert_eq!(observation.cwnd_gain, 2000);
        assert!(
            observation.cwnd.abs_diff(target) <= 2 * u64::from(MSS),
            "cwnd {} B against 2 x BDP + extra_acked = {target} B at {} ns",
            observation.cwnd,
            observation.now
        );
        assert!(
            observation.cwnd >= 2 * path_bdp * 9 / 10,
            "cwnd {} B below twice the path's {path_bdp} B BDP",
            observation.cwnd
        );
    }
    let goodput = sim.goodput(last.now - seconds(2), last.now);
    assert!(
        goodput * 100 >= rate * 95,
        "goodput {goodput} B/s against a {rate} B/s bottleneck"
    );
}

#[test]
fn bbr_converges_to_the_path_when_every_segment_is_acknowledged() {
    let path = Path::wan(AckPolicy::EverySegment);
    let (observations, sim) = run_bbr(path, seconds(6), no_drops);
    assert_bbr_converged(path, &observations, &sim);
}

#[test]
fn bbr_converges_to_the_path_when_acknowledgements_are_delayed() {
    let path = Path::wan(AckPolicy::EverySecondSegment);
    let (observations, sim) = run_bbr(path, seconds(6), no_drops);
    assert_bbr_converged(path, &observations, &sim);
}

#[test]
fn bbr_converges_to_the_path_when_acknowledgements_arrive_in_batches() {
    // Eight ACKs at once, every one of them processed under the same
    // `now_nanos`.
    let path = Path::wan(AckPolicy::Batched(8));
    let (observations, sim) = run_bbr(path, seconds(6), no_drops);
    assert_bbr_converged(path, &observations, &sim);
}

#[test]
fn bbr_converges_to_the_path_when_a_window_shares_one_send_time() {
    // The sender coalesces each transmit pass, so a burst of segments
    // leaves under one `sent_at_nanos`.
    let path = Path::wan(AckPolicy::EverySegment).coalescing();
    let (observations, sim) = run_bbr(path, seconds(6), no_drops);
    assert_bbr_converged(path, &observations, &sim);
}

#[test]
fn bbr_converges_with_shared_send_times_and_batched_acknowledgements() {
    let path = Path::wan(AckPolicy::Batched(8)).coalescing();
    let (observations, sim) = run_bbr(path, seconds(6), no_drops);
    assert_bbr_converged(path, &observations, &sim);
}

/// The state changes an observation sequence shows, with the round
/// count and round-start flag of the acknowledgement that made each.
fn transitions(observations: &[BbrObservation]) -> Vec<(BbrState, BbrObservation)> {
    observations
        .windows(2)
        .filter(|pair| pair[0].state != pair[1].state)
        .map(|pair| (pair[0].state, pair[1]))
        .collect()
}

#[test]
fn probe_bw_cycles_its_gains_on_round_boundaries() {
    let path = Path::wan(AckPolicy::EverySegment);
    // Short of the 10 s after which an unrefreshed minimum round trip
    // sends the flow to ProbeRTT.
    let (observations, _) = run_bbr(path, seconds(9), no_drops);
    let changes = transitions(&observations);
    let probe_bw: Vec<_> = changes
        .iter()
        .skip_while(|(_, to)| to.state != BbrState::ProbeBwDown)
        .collect();
    let mut cycles = 0;
    for pair in probe_bw.windows(2) {
        let (from, to) = (pair[0].1, pair[1].1);
        let expected_next = match from.state {
            BbrState::ProbeBwDown => BbrState::ProbeBwCruise,
            BbrState::ProbeBwCruise => BbrState::ProbeBwRefill,
            BbrState::ProbeBwRefill => BbrState::ProbeBwUp,
            BbrState::ProbeBwUp => BbrState::ProbeBwDown,
            other => panic!("left ProbeBW for {other:?} on a lossless path"),
        };
        assert_eq!(to.state, expected_next, "{from:?} -> {to:?}");
        match to.state {
            BbrState::ProbeBwRefill => {
                // The Reno-coexistence bound, min(BDP in segments, 63)
                // rounds, comes before the 2-3 s wall-clock one on this
                // path, and it is counted in rounds.
                assert!(to.round_start, "REFILL began off a round boundary: {to:?}");
            }
            BbrState::ProbeBwUp => {
                // REFILL lasts exactly one packet-timed round.
                assert!(to.round_start, "UP began off a round boundary: {to:?}");
                assert_eq!(
                    to.round_count,
                    from.round_count + 1,
                    "REFILL lasted one round"
                );
            }
            BbrState::ProbeBwDown => {
                // UP ends when three rounds have not grown the
                // bandwidth by 25%, which is judged at a round start.
                assert!(to.round_start, "UP ended off a round boundary: {to:?}");
                assert!(to.round_count >= from.round_count + 3);
                cycles += 1;
            }
            _ => {}
        }
    }
    assert!(
        cycles >= 2,
        "only {cycles} ProbeBW cycles in 9 s: {changes:?}"
    );
    // Each phase paces at its gain over the bandwidth estimate, less
    // the 1% margin.
    for observation in observations
        .iter()
        .filter(|observation| observation.state != BbrState::Startup)
    {
        let gain = match observation.state {
            BbrState::ProbeBwDown => 900,
            BbrState::ProbeBwCruise | BbrState::ProbeBwRefill => 1000,
            BbrState::ProbeBwUp => 1250,
            BbrState::Drain => 500,
            BbrState::Startup | BbrState::ProbeRtt => continue,
        };
        assert_eq!(
            observation.pacing_rate,
            observation.max_bw * gain / 1000 * 99 / 100,
            "{observation:?}"
        );
    }
}

#[test]
fn the_short_term_bound_a_loss_sets_is_reset_by_the_next_refill() {
    let path = Path::wan(AckPolicy::EverySegment);
    let mut dropped = false;
    let (observations, sim) = run_bbr(path, seconds(8), |socket, now, _, retransmission| {
        let drop = !dropped
            && !retransmission
            && now >= seconds(3)
            && socket.congestion().state() == BbrState::ProbeBwCruise;
        dropped |= drop;
        drop
    });
    let bounded = observations
        .iter()
        .position(|observation| observation.inflight_shortterm.is_some())
        .expect("a loss outside a bandwidth probe sets inflight_shortterm");
    let bound = observations[bounded];
    assert_eq!(bound.state, BbrState::ProbeBwCruise);
    let refill = observations[bounded..]
        .iter()
        .position(|observation| observation.state == BbrState::ProbeBwRefill)
        .map(|offset| observations[bounded + offset])
        .expect("the cycle reaches REFILL after the loss");
    assert_eq!(
        refill.inflight_shortterm, None,
        "REFILL resets the short-term model"
    );
    assert!(
        observations[bounded..]
            .iter()
            .any(|observation| !observation.in_loss_recovery),
        "recovery ended"
    );
    assert_bbr_converged(path, &observations, &sim);
}

#[test]
fn the_long_term_bound_rises_in_probes_after_the_bottleneck_buffer_overflows() {
    // A tail-drop bottleneck buffer of one BDP: a probe that queues more
    // than that loses packets, and the loss sets the long-term bound.
    // Each later ProbeBW_UP round that fills the window without loss
    // raises it again (draft section 5.3.3.9,
    // `ProbeInflightLongtermUpward`).
    let wan = Path::wan(AckPolicy::EverySegment);
    let path = wan.with_buffer(wan.bdp_bytes());
    let (observations, sim) = run_bbr(path, seconds(9), no_drops);
    assert_ne!(sim.tail_drops, 0, "the probes never overflowed the buffer");
    let set = observations
        .iter()
        .position(|observation| observation.inflight_longterm.is_some())
        .expect("a loss sets inflight_longterm");
    let raised_in_up = observations[set..]
        .windows(2)
        .filter(|pair| {
            pair[0].state == BbrState::ProbeBwUp
                && pair[1].state == BbrState::ProbeBwUp
                && !pair[0].in_loss_recovery
                && !pair[1].in_loss_recovery
                && pair[1].inflight_longterm > pair[0].inflight_longterm
        })
        .count();
    assert!(
        raised_in_up >= 2,
        "inflight_longterm rose {raised_in_up} times in ProbeBW_UP"
    );
    let last = observations.last().expect("observed");
    let rate = path.bottleneck_bytes_per_second;
    let goodput = sim.goodput(last.now - seconds(2), last.now);
    assert!(
        goodput * 100 >= rate * 90,
        "goodput {goodput} B/s against a {rate} B/s bottleneck"
    );
}

#[test]
fn the_window_recovers_after_a_retransmission_timeout() {
    // One segment is lost in CRUISE, and so is its fast retransmission:
    // nothing then advances the cumulative acknowledgement, and the
    // segment waits out the retransmission timer. Everything else in
    // the window arrived, so the timeout's resend is acknowledged by one
    // cumulative ACK that ends the episode.
    let path = Path::wan(AckPolicy::EverySegment);
    let lost = Cell::new(None);
    let fast_retransmitted_at = Cell::new(None);
    let at_timeout = Cell::new(None);
    let (observations, sim) =
        run_bbr(
            path,
            seconds(8),
            |socket, now, sequence, retransmission| match lost.get() {
                None if !retransmission
                    && now >= seconds(2)
                    && socket.congestion().state() == BbrState::ProbeBwCruise =>
                {
                    lost.set(Some((sequence, now)));
                    true
                }
                Some((lost_sequence, _)) if retransmission && sequence == lost_sequence => {
                    if fast_retransmitted_at.get().is_none() {
                        fast_retransmitted_at.set(Some(now));
                        return true;
                    }
                    if at_timeout.get().is_none() {
                        at_timeout.set(Some(observe_bbr(socket, now)));
                    }
                    false
                }
                _ => false,
            },
        );
    let (_, lost_at) = lost.get().expect("a segment was dropped in CRUISE");
    let fast_retransmitted_at = fast_retransmitted_at
        .get()
        .expect("three duplicate ACKs resent the segment");
    let timeout: BbrObservation = at_timeout
        .get()
        .expect("the retransmission timer resent the segment");
    assert!(
        timeout.now - fast_retransmitted_at >= TCP_MIN_RTO_NANOS,
        "the second resend came {} ns after the first, sooner than any timeout",
        timeout.now - fast_retransmitted_at
    );
    let before = observations
        .iter()
        .rposition(|observation| observation.now < lost_at)
        .map(|index| observations[index])
        .expect("observed before the loss");
    // Draft section 5.6.4.4: on a timeout the window falls to what is
    // still in the network plus one segment. The timeout declared every
    // other outstanding segment lost, so that is the resend alone.
    assert!(timeout.in_loss_recovery);
    assert!(
        timeout.cwnd <= 2 * u64::from(MSS),
        "cwnd {} B after the timeout",
        timeout.cwnd
    );
    let recovered = observations
        .iter()
        .position(|observation| observation.now > timeout.now)
        .map(|index| observations[index])
        .expect("the resend is acknowledged");
    assert!(
        !recovered.in_loss_recovery,
        "the acknowledgement of the resend covers the recovery point"
    );
    // `RestoreCwnd` brings back the window the flow had before the
    // episode, within the short-term bound the loss in CRUISE set
    // (draft section 5.5.10.3), which still covers the path's BDP.
    let bound = recovered
        .inflight_shortterm
        .map_or(before.cwnd, |shortterm| shortterm.min(before.cwnd));
    assert!(
        recovered.cwnd >= bound,
        "cwnd {} B after recovery against {} B before, bounded at {bound} B",
        recovered.cwnd,
        before.cwnd
    );
    assert!(
        recovered.cwnd >= path.bdp_bytes(),
        "cwnd {} B after recovery is below the path's BDP",
        recovered.cwnd
    );
    // The next REFILL resets that bound, and the window returns to twice
    // the BDP.
    assert_bbr_converged(path, &observations, &sim);
}

/// What one acknowledgement left the CUBIC window at.
#[derive(Clone, Copy, Debug)]
struct CubicObservation {
    now: u64,
    cwnd: u64,
    w_max: u64,
    epoch_start: Option<u64>,
}

fn observe_cubic(socket: &TcpSocket<Cubic>, now: u64) -> CubicObservation {
    let cubic = socket.congestion();
    CubicObservation {
        now,
        cwnd: u64::from(cubic.congestion_window().bytes()),
        w_max: cubic.w_max_bytes(),
        epoch_start: cubic.epoch_start_nanos(),
    }
}

/// RFC 9438 Figure 1 with Figure 2's K, in segments and seconds.
fn rfc9438_w_cubic(w_max: f64, cwnd_epoch: f64, t: f64) -> f64 {
    const C: f64 = 0.4;
    let k = ((w_max - cwnd_epoch) / C).cbrt();
    C * (t - k).powi(3) + w_max
}

/// A path with room for far more than the windows below, so the
/// queue stays empty and the round trip is the propagation delay.
fn cubic_path(ack_policy: AckPolicy) -> Path {
    Path {
        bottleneck_bytes_per_second: 12_500_000,
        rtt_nanos: 100 * MILLISECOND,
        ack_policy,
        budget: TcpSegmentBudget::wire_segments(TCP_WIRE_SEGMENT),
        buffer_bytes: None,
    }
}

/// Grows CUBIC to `loss_window` segments in slow start, drops one
/// segment, and checks the congestion-avoidance stage that follows
/// against RFC 9438's curve; then drops another before the window is
/// back at `W_max` and checks fast convergence.
fn assert_cubic_follows_rfc9438(ack_policy: AckPolicy) {
    const LOSS_WINDOW_SEGMENTS: u64 = 150;
    const SECOND_LOSS_AFTER_NANOS: u64 = 2 * NANOS_PER_SECOND;
    let path = cubic_path(ack_policy);
    let mut first_loss_at = None;
    let mut second_loss_at = None;
    let mut sim = Sim::new(
        Cubic::new(MSS),
        path,
        |socket: &TcpSocket<Cubic>, now: u64, _: u32, retransmission: bool| {
            if retransmission {
                return false;
            }
            let cwnd = u64::from(socket.congestion().congestion_window().bytes());
            if first_loss_at.is_none() && cwnd >= LOSS_WINDOW_SEGMENTS * u64::from(MSS) {
                first_loss_at = Some(now);
                return true;
            }
            if second_loss_at.is_none()
                && let Some(epoch) = socket.congestion().epoch_start_nanos()
                && now >= epoch + SECOND_LOSS_AFTER_NANOS
            {
                second_loss_at = Some(now);
                return true;
            }
            false
        },
    );
    let mut observations = Vec::new();
    sim.run_until(seconds(12), |socket, now| {
        observations.push(observe_cubic(socket, now));
    });
    let mss = f64::from(MSS);

    // The first congestion event: W_max is the window before it, and
    // the stage starts at β = 0.7 of the flight.
    let reduced = observations
        .windows(2)
        .position(|pair| pair[1].cwnd < pair[0].cwnd)
        .expect("the loss reduces the window");
    let before = observations[reduced];
    let after = observations[reduced + 1];
    assert!(before.cwnd >= LOSS_WINDOW_SEGMENTS * u64::from(MSS));
    assert_eq!(
        after.w_max, before.cwnd,
        "W_max is the window before the loss"
    );
    let flight_ratio = after.cwnd as f64 / before.cwnd as f64;
    assert!(
        (flight_ratio - 0.7).abs() < 0.02,
        "the window fell to {flight_ratio:.3} of itself, not β = 0.7"
    );

    let epoch_start = observations[reduced + 1..]
        .iter()
        .find_map(|observation| observation.epoch_start)
        .expect("congestion avoidance starts an epoch");
    let w_max = after.w_max as f64 / mss;
    let cwnd_epoch = after.cwnd as f64 / mss;
    let window_at = |time: u64| {
        observations
            .iter()
            .take_while(|observation| observation.now <= time)
            .last()
            .expect("observed")
            .cwnd as f64
            / mss
    };
    // Several points of the concave stage, up to the second loss.
    for t in [0.25, 0.5, 1.0, 1.5, 1.9] {
        let expected = rfc9438_w_cubic(w_max, cwnd_epoch, t);
        let observed = window_at(epoch_start + (t * 1e9) as u64);
        // The window moves toward W_cubic(t + RTT) once per segment
        // acknowledged, so it trails the curve by at most a round trip
        // of its slope, plus a segment of rounding.
        let slope = 3.0 * 0.4 * (t - ((w_max - cwnd_epoch) / 0.4).cbrt()).powi(2);
        let tolerance = slope * 0.1 + 1.0;
        assert!(
            (observed - expected).abs() <= tolerance,
            "at t = {t} s the window is {observed:.2} segments, RFC 9438 says {expected:.2} \
             (W_max {w_max:.2}, cwnd_epoch {cwnd_epoch:.2})"
        );
    }

    // The second loss comes while the window is still below W_max:
    // fast convergence remembers (1 + β) / 2 = 0.85 of it.
    let second = observations[reduced + 1..]
        .windows(2)
        .position(|pair| pair[1].cwnd < pair[0].cwnd)
        .map(|index| reduced + 1 + index)
        .expect("the second loss reduces the window");
    let before_second = observations[second];
    let after_second = observations[second + 1];
    assert!(before_second.cwnd < after.w_max);
    let expected_w_max = before_second.cwnd as f64 * 0.85;
    assert!(
        (after_second.w_max as f64 - expected_w_max).abs() <= 1.0,
        "W_max {} B after fast convergence, (1 + β) / 2 of {} B is {expected_w_max:.1} B",
        after_second.w_max,
        before_second.cwnd
    );
}

#[test]
fn cubic_follows_rfc9438_when_every_segment_is_acknowledged() {
    assert_cubic_follows_rfc9438(AckPolicy::EverySegment);
}

#[test]
fn cubic_follows_rfc9438_when_acknowledgements_are_delayed() {
    assert_cubic_follows_rfc9438(AckPolicy::EverySecondSegment);
}

#[test]
fn cubic_follows_rfc9438_into_the_convex_region() {
    // One loss, then the curve past K: the window crosses W_max and
    // accelerates away from it.
    let path = cubic_path(AckPolicy::EverySegment);
    let mut dropped = false;
    let mut sim = Sim::new(
        Cubic::new(MSS),
        path,
        |socket: &TcpSocket<Cubic>, _: u64, _: u32, retransmission: bool| {
            let cwnd = u64::from(socket.congestion().congestion_window().bytes());
            let drop = !dropped && !retransmission && cwnd >= 150 * u64::from(MSS);
            dropped |= drop;
            drop
        },
    );
    let mut observations = Vec::new();
    sim.run_until(seconds(11), |socket, now| {
        observations.push(observe_cubic(socket, now));
    });
    let mss = f64::from(MSS);
    let reduced = observations
        .windows(2)
        .position(|pair| pair[1].cwnd < pair[0].cwnd)
        .expect("the loss reduces the window");
    let after = observations[reduced + 1];
    let epoch_start = observations[reduced + 1..]
        .iter()
        .find_map(|observation| observation.epoch_start)
        .expect("congestion avoidance starts an epoch");
    let w_max = after.w_max as f64 / mss;
    let cwnd_epoch = after.cwnd as f64 / mss;
    let k = ((w_max - cwnd_epoch) / 0.4).cbrt();
    let window_at = |time: u64| {
        observations
            .iter()
            .take_while(|observation| observation.now <= time)
            .last()
            .expect("observed")
            .cwnd as f64
            / mss
    };
    assert!(
        observations.last().expect("observed").now >= epoch_start + ((k + 3.0) * 1e9) as u64,
        "the run ends before the last point checked"
    );
    for t in [k - 1.0, k, k + 1.0, k + 2.0, k + 3.0] {
        let expected = rfc9438_w_cubic(w_max, cwnd_epoch, t);
        let observed = window_at(epoch_start + (t * 1e9) as u64);
        let slope = 3.0 * 0.4 * (t - k).powi(2);
        let tolerance = slope * 0.1 + 1.0;
        assert!(
            (observed - expected).abs() <= tolerance,
            "at t = {t:.2} s (K = {k:.2} s) the window is {observed:.2} segments, \
             RFC 9438 says {expected:.2}"
        );
    }
}
