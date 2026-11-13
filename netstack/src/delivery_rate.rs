//! Delivery-rate estimation, as draft-ietf-ccwg-bbr section 4.1
//! specifies it.
//!
//! Every transmission snapshots the connection's delivery state into a
//! [`SendSnapshot`] that travels with the segment's retransmission
//! record. When an acknowledgement (cumulative or selective) covers
//! segments, the sampler takes the snapshot of the most recently sent of
//! them and measures what the network delivered between that segment's
//! departure and now: the bytes delivered since its snapshot, over the
//! longer of the time the flight took to send and the time its
//! acknowledgements took to arrive. That is a rate of the path, not of
//! one ACK: it is the same whether a receiver acknowledges every segment
//! or every tenth, and whether a sender puts a window on the wire with
//! one timestamp or paces it out.
//!
//! # Concurrency
//!
//! A sampler is one TCP connection's state and is touched only by the
//! code that owns that connection's `TcpSocket`, the shard running its
//! stack. Nothing here is shared between processors, locked, or atomic;
//! the snapshots are plain values stored inline in the in-flight records
//! the retransmission queue already holds, so sampling allocates
//! nothing.

/// The connection's delivery state when a segment was last transmitted
/// (the draft's per-packet `P` state, section 4.1.2.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendSnapshot {
    /// `P.delivered`: bytes the connection had delivered when this
    /// segment was sent.
    pub delivered: u64,
    /// `P.delivered_time`: when that delivered count was last advanced.
    pub delivered_time_nanos: u64,
    /// `P.first_send_time`: the send time of the segment most recently
    /// delivered when this one was sent, the start of its flight.
    pub first_send_time_nanos: u64,
    /// `P.send_time`: when this segment was handed to the wire.
    pub send_time_nanos: u64,
    /// `P.lost`: bytes the connection had marked lost when this segment
    /// was sent.
    pub lost: u64,
    /// `P.tx_in_flight`: bytes outstanding immediately after this
    /// segment was sent, this segment included.
    pub tx_in_flight: u32,
    /// `P.is_app_limited`: the connection was application-limited when
    /// this segment was sent.
    pub is_app_limited: bool,
}

/// One acknowledgement's delivery-rate sample (the draft's `RS`,
/// section 4.1.2.1.3), taken from the most recently sent segment the
/// acknowledgement covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateSample {
    /// `RS.prior_delivered`: the connection's delivered count when that
    /// segment was sent. A packet-timed round trip ends when this
    /// reaches the delivered count at the round's start.
    pub prior_delivered: u64,
    /// `RS.delivered`: bytes delivered over the sampling interval.
    pub delivered: u64,
    /// `RS.interval`: the longer of the send and acknowledgement phases
    /// of the sampled flight.
    pub interval_nanos: u64,
    /// `RS.delivery_rate` in bytes per second, `None` when the interval
    /// is shorter than the connection's minimum round trip (a sample
    /// the draft calls unreliable: a retransmission acknowledged for its
    /// original) or zero.
    pub delivery_rate: Option<u64>,
    /// `RS.is_app_limited`: the sampled segment was sent while the
    /// connection was application-limited.
    pub is_app_limited: bool,
    /// `RS.tx_in_flight`: bytes outstanding when the sampled segment was
    /// sent.
    pub tx_in_flight: u32,
    /// `RS.lost`: bytes marked lost between the sampled segment's send
    /// and this acknowledgement.
    pub lost: u64,
}

/// The delivery accounting of one acknowledgement while the transport
/// walks the segments it covers (the draft's `InitRateSample` state).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AckDelivery {
    /// Snapshot of the most recently sent segment delivered so far.
    newest: Option<SendSnapshot>,
    /// Bytes this acknowledgement delivered (`RS.newly_acked`).
    newly_acked: u32,
}

impl AckDelivery {
    pub(crate) const fn newly_acked(&self) -> u32 {
        self.newly_acked
    }
}

/// Per-connection delivery-rate state (the draft's `C` state, section
/// 4.1.2.1.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeliveryRateSampler {
    /// `C.delivered`.
    delivered: u64,
    /// `C.delivered_time`.
    delivered_time_nanos: u64,
    /// `C.first_send_time`.
    first_send_time_nanos: u64,
    /// `C.app_limited`: the delivered count past which the bubble an
    /// application-limited moment put in the pipe has drained. `None`
    /// is the draft's 0, "not application-limited"; an explicit option
    /// keeps a mark of zero (a connection that has delivered nothing)
    /// distinct from no mark.
    app_limited_until: Option<u64>,
    /// `C.lost`.
    lost: u64,
    /// `C.min_rtt`: the shortest round trip the connection has seen,
    /// which bounds a believable sampling interval from below.
    min_rtt_nanos: Option<u64>,
}

impl DeliveryRateSampler {
    pub(crate) const fn new() -> Self {
        Self {
            delivered: 0,
            delivered_time_nanos: 0,
            first_send_time_nanos: 0,
            app_limited_until: None,
            lost: 0,
            min_rtt_nanos: None,
        }
    }

    /// `C.delivered`.
    pub(crate) const fn delivered(&self) -> u64 {
        self.delivered
    }

    /// The draft's `OnPacketSent`: snapshot the delivery state into the
    /// segment just transmitted. `in_flight_before` is what was
    /// outstanding (sent and unacknowledged) before this transmission,
    /// and `in_flight_after` is `C.inflight`, the pipe, with it.
    pub(crate) fn on_packet_sent(
        &mut self,
        in_flight_before: u32,
        in_flight_after: u32,
        now_nanos: u64,
    ) -> SendSnapshot {
        // The draft tests `C.inflight == 0` after the transmission, which
        // can never hold for a packet that counts itself; what it means,
        // and what Linux's `tcp_rate_skb_sent` tests, is that nothing was
        // outstanding before it. A flight that starts from idle is timed
        // from its own first send, not from an acknowledgement that
        // arrived before the idle period.
        if in_flight_before == 0 {
            self.first_send_time_nanos = now_nanos;
            self.delivered_time_nanos = now_nanos;
        }
        SendSnapshot {
            delivered: self.delivered,
            delivered_time_nanos: self.delivered_time_nanos,
            first_send_time_nanos: self.first_send_time_nanos,
            send_time_nanos: now_nanos,
            lost: self.lost,
            tx_in_flight: in_flight_after,
            is_app_limited: self.app_limited_until.is_some(),
        }
    }

    /// The draft's `MarkConnectionAppLimited`: everything sent until
    /// what is in flight now has been delivered measures the
    /// application, not the path.
    pub(crate) fn mark_app_limited(&mut self, bytes_in_flight: u32) {
        self.app_limited_until = Some(self.delivered + u64::from(bytes_in_flight));
    }

    /// Counts `bytes` newly marked lost into `C.lost` and returns the
    /// new total.
    pub(crate) fn mark_lost(&mut self, bytes: u32) -> u64 {
        self.lost += u64::from(bytes);
        self.lost
    }

    /// The draft's `UpdateRateSample` for one segment, or the
    /// acknowledged prefix of one, with `bytes` newly delivered.
    ///
    /// Within one acknowledgement the caller visits segments in
    /// ascending sequence order. That order is what makes the draft's
    /// `IsNewestPacket` tie-break (equal send times, higher packet id
    /// wins) the plain `>=` below: a segment visited later has the
    /// higher id, which is exactly the case a burst sent under one
    /// timestamp produces.
    pub(crate) fn on_packet_delivered(
        &mut self,
        ack: &mut AckDelivery,
        sent: &SendSnapshot,
        bytes: u32,
        now_nanos: u64,
    ) {
        self.note_delivered(ack, bytes, now_nanos);
        if ack.newest.is_none() || sent.send_time_nanos >= self.first_send_time_nanos {
            ack.newest = Some(*sent);
            self.first_send_time_nanos = sent.send_time_nanos;
        }
    }

    /// Delivered bytes no segment record describes (a zero-window
    /// probe's): they count toward `C.delivered` but time nothing.
    pub(crate) fn on_unrecorded_delivered(
        &mut self,
        ack: &mut AckDelivery,
        bytes: u32,
        now_nanos: u64,
    ) {
        self.note_delivered(ack, bytes, now_nanos);
    }

    fn note_delivered(&mut self, ack: &mut AckDelivery, bytes: u32, now_nanos: u64) {
        self.delivered += u64::from(bytes);
        self.delivered_time_nanos = now_nanos;
        ack.newly_acked += bytes;
    }

    /// The draft's `GenerateRateSample`, after every segment the
    /// acknowledgement covers has been visited. `rtt_nanos` is the
    /// acknowledgement's round-trip sample; it joins the connection's
    /// minimum before that minimum judges this sample's interval, as
    /// Linux orders `tcp_ack_update_rtt` before `tcp_rate_gen`, so the
    /// first acknowledgement of a connection is not judged against a
    /// minimum it has not measured yet.
    ///
    /// `None` when the acknowledgement delivered nothing a segment
    /// record describes.
    pub(crate) fn generate(
        &mut self,
        ack: &AckDelivery,
        rtt_nanos: Option<u64>,
    ) -> Option<RateSample> {
        if self
            .app_limited_until
            .is_some_and(|until| self.delivered > until)
        {
            self.app_limited_until = None;
        }
        if let Some(rtt_nanos) = rtt_nanos {
            self.min_rtt_nanos = Some(
                self.min_rtt_nanos
                    .map_or(rtt_nanos, |min| min.min(rtt_nanos)),
            );
        }
        let newest = ack.newest?;
        let send_elapsed = newest
            .send_time_nanos
            .saturating_sub(newest.first_send_time_nanos);
        let ack_elapsed = self
            .delivered_time_nanos
            .saturating_sub(newest.delivered_time_nanos);
        let interval_nanos = send_elapsed.max(ack_elapsed);
        let delivered = self.delivered - newest.delivered;
        let reliable = interval_nanos != 0
            && self
                .min_rtt_nanos
                .is_none_or(|min_rtt| interval_nanos >= min_rtt);
        Some(RateSample {
            prior_delivered: newest.delivered,
            delivered,
            interval_nanos,
            delivery_rate: reliable.then(|| bytes_per_second(delivered, interval_nanos)),
            is_app_limited: newest.is_app_limited,
            tx_in_flight: newest.tx_in_flight,
            lost: self.lost - newest.lost,
        })
    }
}

/// `bytes` over `nanos`, in bytes per second.
pub(crate) fn bytes_per_second(bytes: u64, nanos: u64) -> u64 {
    let rate = u128::from(bytes) * NANOS_PER_SECOND / u128::from(nanos);
    u64::try_from(rate).unwrap_or(u64::MAX)
}

const NANOS_PER_SECOND: u128 = 1_000_000_000;

#[cfg(test)]
mod tests {
    use super::*;

    const MSS: u32 = 1460;

    /// Sends `count` segments back to back at one timestamp, the shape a
    /// sender that coalesces a window into one transmit pass produces.
    fn send_burst(
        sampler: &mut DeliveryRateSampler,
        in_flight: &mut u32,
        count: usize,
        now: u64,
    ) -> alloc::vec::Vec<SendSnapshot> {
        (0..count)
            .map(|_| {
                let before = *in_flight;
                *in_flight += MSS;
                sampler.on_packet_sent(before, *in_flight, now)
            })
            .collect()
    }

    #[test]
    fn a_burst_sent_under_one_timestamp_samples_the_ack_rate() {
        // Ten segments leave at t = 0 under one timestamp; the bottleneck
        // spaces them 1 ms apart and the path adds 20 ms. Each ACK covers
        // one segment.
        let mut sampler = DeliveryRateSampler::new();
        let mut in_flight = 0;
        let sent = send_burst(&mut sampler, &mut in_flight, 10, 0);
        let mut last = None;
        for (index, segment) in sent.iter().enumerate() {
            let now = 20_000_000 + index as u64 * 1_000_000;
            let mut ack = AckDelivery::default();
            sampler.on_packet_delivered(&mut ack, segment, MSS, now);
            last = sampler.generate(&ack, Some(now - segment.send_time_nanos));
        }
        let sample = last.expect("the last ACK delivers a recorded segment");
        // All ten segments over the 29 ms from their shared departure
        // (the flight started from idle) to the last ACK.
        assert_eq!(sample.delivered, 10 * u64::from(MSS));
        assert_eq!(sample.interval_nanos, 29_000_000);
        assert_eq!(
            sample.delivery_rate,
            Some(10 * u64::from(MSS) * 1_000_000_000 / 29_000_000)
        );
    }

    #[test]
    fn acks_processed_under_one_timestamp_take_the_newest_segment() {
        // A segment leaves idle at t = 0 and ten more follow it 1 ms
        // apart while it is outstanding, so their flight starts where
        // the first one's did. The first is acknowledged at 20 ms; the
        // other ten are acknowledged by ten ACKs that a batched receive
        // path hands up together at 40 ms.
        let mut sampler = DeliveryRateSampler::new();
        let mut in_flight = 0;
        let first = sampler.on_packet_sent(in_flight, MSS, 0);
        in_flight += MSS;
        let flight: alloc::vec::Vec<_> = (1..=10u64)
            .map(|index| {
                let before = in_flight;
                in_flight += MSS;
                sampler.on_packet_sent(before, in_flight, index * 1_000_000)
            })
            .collect();
        let mut ack = AckDelivery::default();
        sampler.on_packet_delivered(&mut ack, &first, MSS, 20_000_000);
        let _ = sampler.generate(&ack, Some(20_000_000));

        let now = 40_000_000;
        let samples: alloc::vec::Vec<_> = flight
            .iter()
            .map(|segment| {
                let mut ack = AckDelivery::default();
                sampler.on_packet_delivered(&mut ack, segment, MSS, now);
                sampler
                    .generate(&ack, Some(now - segment.send_time_nanos))
                    .expect("each ACK delivers a recorded segment")
            })
            .collect();
        // Every segment of the flight was sent before the 20 ms ACK, so
        // each one's interval runs from t = 0: the ACK phase (40 ms) is
        // longer than its send phase (up to 10 ms).
        for (index, sample) in samples.iter().enumerate() {
            assert_eq!(sample.interval_nanos, 40_000_000);
            assert_eq!(sample.delivered, (index as u64 + 2) * u64::from(MSS));
        }
        // An ACK-compressed batch claims no rate faster than the flight
        // was delivered over: the last sample is all eleven segments
        // over the 40 ms the flight took.
        assert_eq!(
            samples[9].delivery_rate,
            Some(11 * u64::from(MSS) * 1_000_000_000 / 40_000_000)
        );
    }

    #[test]
    fn a_stretch_ack_samples_its_most_recently_sent_segment() {
        let mut sampler = DeliveryRateSampler::new();
        let mut in_flight = 0;
        let sent: alloc::vec::Vec<_> = (0..4u64)
            .map(|index| {
                let before = in_flight;
                in_flight += MSS;
                sampler.on_packet_sent(before, in_flight, index * 1_000_000)
            })
            .collect();
        let mut ack = AckDelivery::default();
        for segment in &sent {
            sampler.on_packet_delivered(&mut ack, segment, MSS, 30_000_000);
        }
        let sample = sampler
            .generate(&ack, Some(27_000_000))
            .expect("a stretch ACK delivers recorded segments");
        assert_eq!(ack.newly_acked(), 4 * MSS);
        assert_eq!(sample.tx_in_flight, 4 * MSS);
        assert_eq!(sample.interval_nanos, 30_000_000);
    }

    #[test]
    fn an_application_limited_mark_clears_once_its_bubble_is_delivered() {
        let mut sampler = DeliveryRateSampler::new();
        sampler.mark_app_limited(0);
        let first = sampler.on_packet_sent(0, MSS, 0);
        assert!(first.is_app_limited);
        let mut ack = AckDelivery::default();
        sampler.on_packet_delivered(&mut ack, &first, MSS, 10_000_000);
        let sample = sampler
            .generate(&ack, Some(10_000_000))
            .expect("the ACK delivers a recorded segment");
        assert!(
            sample.is_app_limited,
            "the sample is from an app-limited send"
        );
        let next = sampler.on_packet_sent(0, MSS, 10_000_000);
        assert!(!next.is_app_limited, "the bubble has left the pipe");
    }

    #[test]
    fn a_sample_shorter_than_the_minimum_round_trip_carries_no_rate() {
        let mut sampler = DeliveryRateSampler::new();
        let first = sampler.on_packet_sent(0, MSS, 0);
        let mut ack = AckDelivery::default();
        sampler.on_packet_delivered(&mut ack, &first, MSS, 10_000_000);
        assert!(
            sampler
                .generate(&ack, Some(10_000_000))
                .and_then(|sample| sample.delivery_rate)
                .is_some()
        );
        // A retransmission sent at 12 ms is acknowledged at 13 ms by an
        // ACK that was really for its original: the 1 ms interval is
        // shorter than any round trip the path has shown.
        let retransmission = sampler.on_packet_sent(0, MSS, 12_000_000);
        let mut ack = AckDelivery::default();
        sampler.on_packet_delivered(&mut ack, &retransmission, MSS, 13_000_000);
        let sample = sampler
            .generate(&ack, None)
            .expect("the ACK delivers a recorded segment");
        assert_eq!(sample.interval_nanos, 1_000_000);
        assert_eq!(sample.delivery_rate, None);
    }
}
