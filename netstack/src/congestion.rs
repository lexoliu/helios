//! Congestion controllers and the contract the TCP transport drives them
//! through.
//!
//! # What the transport reports
//!
//! The transport owns the sequence space, so it owns the facts a
//! controller cannot observe for itself: what each segment carried when
//! it left ([`PacketSent`]), what an acknowledgement delivered and the
//! delivery-rate sample it produced ([`AckSample`], built by
//! `crate::delivery_rate` as draft-ietf-ccwg-bbr section 4.1 specifies),
//! which segments it declared lost, and when a loss-recovery episode
//! began and ended ([`CongestionEvent`]). Within one acknowledgement the
//! transport reports losses and recovery transitions before the
//! [`CongestionControl::on_ack`] for it, the order Linux's `tcp_ack`
//! runs `tcp_fastretrans_alert` before `tcp_cong_control`.
//!
//! # Concurrency
//!
//! A controller is the private state of one connection. It is created
//! by cloning the stack's template into a new socket and is then only
//! ever touched through that socket's `&mut`, on whichever processor
//! owns the socket's stack shard. Nothing in this module is shared,
//! locked, or atomic; the `Send + Sync` bound exists so the template can
//! live in a stack that moves between processors, not so that a
//! controller can be reached from two at once.

use core::cmp;
use core::hash::Hasher;

use num_integer::Roots;
use siphasher::sip::SipHasher13;

pub use crate::delivery_rate::{RateSample, SendSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CongestionWindow(u32);

impl CongestionWindow {
    pub const fn new(bytes: u32) -> Self {
        assert!(bytes != 0, "congestion window must be non-zero");
        Self(bytes)
    }

    pub const fn bytes(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacingRate(u64);

impl PacingRate {
    pub const fn from_bytes_per_second(bytes_per_second: u64) -> Self {
        assert!(bytes_per_second != 0, "pacing rate must be non-zero");
        Self(bytes_per_second)
    }

    pub const fn bytes_per_second(self) -> u64 {
        self.0
    }
}

/// One transmission, original or retransmitted, reported just after the
/// transport put it on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketSent {
    /// Sequence space the segment occupies.
    pub bytes: u32,
    /// Bytes outstanding with this segment included.
    pub bytes_in_flight: u32,
    /// The connection's delivered count (`C.delivered`).
    pub delivered: u64,
    /// The connection was application-limited when the segment was
    /// sent (`P.is_app_limited`).
    pub app_limited: bool,
    pub now_nanos: u64,
}

/// What one acknowledgement told the transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AckSample {
    /// Bytes this acknowledgement newly delivered, cumulatively or
    /// selectively (`RS.newly_acked`).
    pub newly_acked: u32,
    /// The connection's delivered count after it (`C.delivered`).
    pub delivered: u64,
    /// Bytes still outstanding after it (`C.inflight`).
    pub bytes_in_flight: u32,
    /// Round trip of the most recently sent segment it delivered that
    /// was never retransmitted, or the timestamp echo when every
    /// delivered segment was (`RS.rtt`).
    pub rtt_nanos: Option<u64>,
    /// The transport's smoothed round trip (RFC 6298 SRTT, `C.srtt`).
    pub smoothed_rtt_nanos: Option<u64>,
    /// The delivery-rate sample, `None` when the acknowledgement
    /// delivered nothing a segment record describes.
    pub rate: Option<RateSample>,
    /// The congestion window was fully used at some point in the last
    /// window of data (`C.is_cwnd_limited`).
    pub cwnd_limited: bool,
    /// The peer negotiated SACK (`C.has_selective_acks`).
    pub has_selective_acks: bool,
    pub now_nanos: u64,
}

/// One segment the transport has declared lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LostPacket {
    /// Sequence space the segment occupies (`P.size`).
    pub bytes: u32,
    /// The connection's state when the segment was last sent.
    pub sent: SendSnapshot,
    /// Bytes the connection has marked lost, this segment included
    /// (`C.lost`).
    pub lost: u64,
    /// The connection's delivered count (`C.delivered`).
    pub delivered: u64,
    pub has_selective_acks: bool,
    pub now_nanos: u64,
}

/// What began a loss-recovery episode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryTrigger {
    /// Duplicate acknowledgements: fast retransmit and fast recovery.
    FastRetransmit,
    /// The retransmission timer expired.
    RetransmissionTimeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CongestionEvent {
    /// The transport marked one segment lost. Every segment is reported
    /// once per loss of a transmission, before the
    /// [`CongestionEvent::RecoveryStarted`] that the loss begins, if it
    /// begins one.
    PacketLost(LostPacket),
    /// A loss-recovery episode began. The episode lasts until everything
    /// outstanding at its start has been acknowledged.
    RecoveryStarted {
        trigger: RecoveryTrigger,
        /// RFC 5681 FlightSize: bytes sent and not yet cumulatively
        /// acknowledged.
        outstanding_bytes: u32,
        /// Bytes the transport believes are still in the network:
        /// outstanding data less what it has marked lost and not yet
        /// retransmitted (the draft's `C.inflight`, RFC 6675's pipe).
        bytes_in_flight: u32,
        now_nanos: u64,
    },
    /// The episode ended: an acknowledgement covered everything that was
    /// outstanding when it began.
    RecoveryEnded { now_nanos: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryAction {
    pub cwnd: CongestionWindow,
    pub pacing_rate: Option<PacingRate>,
}

pub trait CongestionControl: Clone + Send + Sync + 'static {
    fn algorithm_name(&self) -> &'static str;

    fn on_packet_sent(&mut self, packet: PacketSent);

    fn on_ack(&mut self, sample: AckSample) -> RecoveryAction;

    fn on_congestion_event(&mut self, event: CongestionEvent) -> RecoveryAction;

    fn congestion_window(&self) -> CongestionWindow;

    fn pacing_rate(&self) -> Option<PacingRate>;

    /// Whether the controller is holding what it sends below what the
    /// path would take, on purpose, so that delivery-rate samples of the
    /// data it sends now would measure the controller rather than the
    /// path. The transport answers `true` by marking the connection
    /// application-limited after the acknowledgement (draft-ietf-ccwg-bbr
    /// `HandleProbeRTT`).
    fn limits_delivery(&self) -> bool;
}

#[derive(Clone, Debug)]
pub struct NewReno {
    cwnd: u32,
    ssthresh: u32,
    mss: u32,
    /// Inside a fast-retransmit recovery episode, where RFC 6582 holds
    /// the window at `ssthresh` (this transport does not inflate it) and
    /// RFC 5681's growth, which runs on ACKs of new data outside
    /// recovery, does not apply.
    in_recovery: bool,
}

impl NewReno {
    pub const fn new(mss: u32) -> Self {
        assert!(mss != 0, "MSS must be non-zero");
        Self {
            cwnd: mss * 10,
            ssthresh: u32::MAX,
            mss,
            in_recovery: false,
        }
    }

    fn action(&self) -> RecoveryAction {
        RecoveryAction {
            cwnd: self.congestion_window(),
            pacing_rate: None,
        }
    }
}

impl CongestionControl for NewReno {
    fn algorithm_name(&self) -> &'static str {
        "newreno"
    }

    fn on_packet_sent(&mut self, _packet: PacketSent) {}

    fn on_ack(&mut self, sample: AckSample) -> RecoveryAction {
        if self.in_recovery {
            return self.action();
        }
        if self.cwnd < self.ssthresh {
            self.cwnd = self.cwnd.saturating_add(sample.newly_acked);
        } else {
            let increment = ((u64::from(self.mss) * u64::from(sample.newly_acked))
                / u64::from(self.cwnd.max(1)))
            .max(1);
            self.cwnd = self.cwnd.saturating_add(increment as u32);
        }
        self.action()
    }

    fn on_congestion_event(&mut self, event: CongestionEvent) -> RecoveryAction {
        match event {
            CongestionEvent::RecoveryStarted {
                trigger: RecoveryTrigger::FastRetransmit,
                outstanding_bytes,
                ..
            } => {
                self.ssthresh = cmp::max(outstanding_bytes / 2, self.mss * 2);
                self.cwnd = self.ssthresh;
                self.in_recovery = true;
            }
            CongestionEvent::RecoveryStarted {
                trigger: RecoveryTrigger::RetransmissionTimeout,
                ..
            } => {
                self.ssthresh = cmp::max(self.cwnd / 2, self.mss * 2);
                self.cwnd = self.mss;
                // RFC 5681 section 3.1: slow start resends the loss
                // window.
                self.in_recovery = false;
            }
            CongestionEvent::RecoveryEnded { .. } => self.in_recovery = false,
            CongestionEvent::PacketLost(_) => {}
        }
        self.action()
    }

    fn congestion_window(&self) -> CongestionWindow {
        CongestionWindow::new(self.cwnd.max(self.mss))
    }

    fn pacing_rate(&self) -> Option<PacingRate> {
        None
    }

    fn limits_delivery(&self) -> bool {
        false
    }
}

/// A window in segments of the SMSS, in fixed point with
/// [`Segments::FRACTION_BITS`] fractional bits: RFC 9438 states every
/// window in segments, and `segments_acked` (its Figure 4) is a fraction
/// of one whenever an acknowledgement covers less than an SMSS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Segments(u64);

impl Segments {
    /// 1/65536 of a segment: far below one byte at any SMSS a link
    /// carries, so converting to bytes and back loses nothing the
    /// transport can act on.
    const FRACTION_BITS: u32 = 16;
    const ONE: Self = Self(1 << Self::FRACTION_BITS);

    const fn whole(segments: u64) -> Self {
        Self(segments << Self::FRACTION_BITS)
    }

    fn from_bytes(bytes: u64, smss: u32) -> Self {
        Self(((u128::from(bytes) << Self::FRACTION_BITS) / u128::from(smss)) as u64)
    }

    fn to_bytes(self, smss: u32) -> u64 {
        ((u128::from(self.0) * u128::from(smss)) >> Self::FRACTION_BITS) as u64
    }

    /// `self * numerator / denominator`.
    fn scaled(self, numerator: u64, denominator: u64) -> Self {
        Self((u128::from(self.0) * u128::from(numerator) / u128::from(denominator)) as u64)
    }

    /// `self * multiplier / divisor`, for a ratio of two windows.
    fn times_ratio(self, multiplier: Self, divisor: Self) -> Self {
        Self((u128::from(self.0) * u128::from(multiplier.0) / u128::from(divisor.0.max(1))) as u64)
    }

    const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

/// One congestion-avoidance stage (RFC 9438 section 4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CubicEpoch {
    /// `t_epoch`, shifted forward by every interval in which the flow
    /// was application-limited (section 5.8: `t` excludes them).
    start_nanos: u64,
    /// `K` in microseconds.
    k_micros: u64,
    /// `W_max` for this stage: the plateau the curve is centred on.
    w_max: Segments,
    /// `W_est` (section 4.3).
    w_est: Segments,
}

/// CUBIC, RFC 9438.
///
/// Windows are kept in fixed-point [`Segments`] and times in
/// microseconds, the units of the RFC's formulas, and converted to
/// bytes only at the interface. Slow start is RFC 5681's, with the
/// per-ACK increase bounded by RFC 3465's L = 2 SMSS; RFC 9438 section
/// 4.10 recommends HyStart++ (RFC 9406) and allows Reno's.
#[derive(Clone, Debug)]
pub struct Cubic {
    smss: u32,
    cwnd: Segments,
    /// `None` until the first congestion event: an unbounded
    /// threshold.
    ssthresh: Option<Segments>,
    /// `W_max`: the window just before the last reduction, or less
    /// under fast convergence.
    w_max: Segments,
    /// `cwnd_prior`: the window just before the last reduction.
    cwnd_prior: Segments,
    epoch: Option<CubicEpoch>,
    /// The next congestion-avoidance stage follows a timeout: section
    /// 4.8 starts it with `K = 0` and `W_max` at its own starting
    /// window.
    after_timeout: bool,
    /// Inside a fast-retransmit recovery episode. Section 4.6 sets the
    /// window once on entering recovery and the growth of sections 4.2
    /// to 4.4 runs on new ACKs in congestion avoidance, so nothing an
    /// ACK reports inside the episode grows it, as in Linux, which
    /// skips `cong_avoid` while it is reducing the window. A timeout's
    /// episode is not one: its window slow-starts.
    in_recovery: bool,
    last_ack_nanos: Option<u64>,
}

impl Cubic {
    /// β_cubic, section 4.6: 0.7.
    const BETA_NUMERATOR: u64 = 7;
    const BETA_DENOMINATOR: u64 = 10;
    /// C, section 5: 0.4 segments per second cubed, as 2/5.
    const C_NUMERATOR: u128 = 2;
    const C_DENOMINATOR: u128 = 5;
    /// α_cubic = 3 (1 - β) / (1 + β), section 4.3: 0.9 / 1.7 = 9/17.
    const ALPHA_NUMERATOR: u64 = 9;
    const ALPHA_DENOMINATOR: u64 = 17;
    /// RFC 6928's initial window.
    const INITIAL_WINDOW_SEGMENTS: u64 = 10;
    /// RFC 3465's L: slow start grows by at most this many SMSS per ACK.
    const SLOW_START_LIMIT_SEGMENTS: u64 = 2;
    const MICROS_PER_SECOND: u128 = 1_000_000;
    /// The largest `|t - K|` the cubic term is evaluated at, in
    /// microseconds (about 19 hours): its cube times `C` in fixed point
    /// stays inside an `i128`, and any window it yields is far past the
    /// 1.5 × cwnd bound on the target anyway.
    const MAX_CUBIC_OFFSET_MICROS: i128 = 1 << 36;

    pub const fn new(mss: u32) -> Self {
        assert!(mss != 0, "MSS must be non-zero");
        Self {
            smss: mss,
            cwnd: Segments::whole(Self::INITIAL_WINDOW_SEGMENTS),
            ssthresh: None,
            w_max: Segments(0),
            cwnd_prior: Segments(0),
            epoch: None,
            after_timeout: false,
            in_recovery: false,
            last_ack_nanos: None,
        }
    }

    fn action(&self) -> RecoveryAction {
        RecoveryAction {
            cwnd: self.congestion_window(),
            pacing_rate: None,
        }
    }

    /// `W_cubic(t)` of Figure 1, `t` and `K` in microseconds.
    fn w_cubic(epoch: &CubicEpoch, t_micros: u64) -> Segments {
        let offset = (i128::from(t_micros) - i128::from(epoch.k_micros)).clamp(
            -Self::MAX_CUBIC_OFFSET_MICROS,
            Self::MAX_CUBIC_OFFSET_MICROS,
        );
        // C (t - K)^3 segments, with t - K in microseconds: divide the
        // cube by 10^18 to bring it back to seconds cubed.
        let term =
            offset * offset * offset * (1 << Segments::FRACTION_BITS) * Self::C_NUMERATOR as i128
                / (Self::C_DENOMINATOR as i128 * Self::MICROS_PER_SECOND.pow(3) as i128);
        let window = i128::from(epoch.w_max.0) + term;
        Segments(u64::try_from(window.max(0)).unwrap_or(u64::MAX))
    }

    /// `K` of Figure 2 in microseconds: the cube root of
    /// `(W_max - cwnd_epoch) / C` seconds.
    fn k_micros(w_max: Segments, cwnd_epoch: Segments) -> u64 {
        let shortfall = u128::from(w_max.saturating_sub(cwnd_epoch).0);
        let seconds_cubed_in_micros =
            shortfall * Self::C_DENOMINATOR * Self::MICROS_PER_SECOND.pow(3)
                / (Self::C_NUMERATOR << Segments::FRACTION_BITS);
        u64::try_from(seconds_cubed_in_micros.cbrt()).unwrap_or(u64::MAX)
    }

    fn start_epoch(&mut self, now_nanos: u64) -> CubicEpoch {
        let cwnd_epoch = self.cwnd;
        let (w_max, k_micros) = if self.after_timeout || self.w_max <= cwnd_epoch {
            (cwnd_epoch, 0)
        } else {
            (self.w_max, Self::k_micros(self.w_max, cwnd_epoch))
        };
        self.after_timeout = false;
        CubicEpoch {
            start_nanos: now_nanos,
            k_micros,
            w_max,
            w_est: cwnd_epoch,
        }
    }

    fn reduce(&mut self, outstanding_bytes: u32) {
        self.cwnd_prior = self.cwnd;
        let flight = Segments::from_bytes(u64::from(outstanding_bytes), self.smss);
        let ssthresh = flight
            .scaled(Self::BETA_NUMERATOR, Self::BETA_DENOMINATOR)
            .max(Segments::whole(2));
        self.ssthresh = Some(ssthresh);
        self.epoch = None;
    }

    fn congestion_avoidance(&mut self, sample: &AckSample, acked: Segments) {
        let now = sample.now_nanos;
        let mut epoch = match self.epoch {
            Some(epoch) => epoch,
            None => self.start_epoch(now),
        };
        let t_micros = now.saturating_sub(epoch.start_nanos) / 1_000;
        let rtt_micros = sample.smoothed_rtt_nanos.or(sample.rtt_nanos).unwrap_or(0) / 1_000;
        let cwnd = self.cwnd;
        let target = Self::w_cubic(&epoch, t_micros + rtt_micros).clamp(cwnd, cwnd.scaled(3, 2));
        let alpha = if epoch.w_est >= self.cwnd_prior {
            (1, 1)
        } else {
            (Self::ALPHA_NUMERATOR, Self::ALPHA_DENOMINATOR)
        };
        epoch.w_est = epoch.w_est.saturating_add(
            acked
                .times_ratio(Segments::ONE, cwnd)
                .scaled(alpha.0, alpha.1),
        );
        self.cwnd = if Self::w_cubic(&epoch, t_micros) < epoch.w_est {
            // Section 4.3: the Reno-friendly region.
            cwnd.max(epoch.w_est)
        } else {
            // Sections 4.4 and 4.5: (target - cwnd) / cwnd per ACK of
            // one segment, scaled by the segments this ACK covered so
            // that a receiver acknowledging every other segment follows
            // the same curve.
            cwnd.saturating_add(target.saturating_sub(cwnd).times_ratio(acked, cwnd))
        };
        self.epoch = Some(epoch);
    }
}

impl CongestionControl for Cubic {
    fn algorithm_name(&self) -> &'static str {
        "cubic"
    }

    fn on_packet_sent(&mut self, _packet: PacketSent) {}

    fn on_ack(&mut self, sample: AckSample) -> RecoveryAction {
        let previous_ack = self.last_ack_nanos.replace(sample.now_nanos);
        if self.in_recovery {
            return self.action();
        }
        if !sample.cwnd_limited {
            // Section 5.8: an application-limited flow does not grow its
            // window, and its epoch clock does not run.
            if let (Some(epoch), Some(previous)) = (self.epoch.as_mut(), previous_ack) {
                epoch.start_nanos += sample.now_nanos.saturating_sub(previous);
            }
            return self.action();
        }
        let acked = Segments::from_bytes(u64::from(sample.newly_acked), self.smss);
        // RFC 9438 section 4.10 slow-starts while cwnd is "no more than"
        // ssthresh; RFC 5681 section 3.1 leaves the equal case to the
        // sender. A reduction sets cwnd to exactly ssthresh, and
        // slow-starting there would move the epoch's starting window
        // off the reduced one that K is computed from, so equality is
        // congestion avoidance, as in Linux's `tcp_in_slow_start`.
        if self.ssthresh.is_none_or(|ssthresh| self.cwnd < ssthresh) {
            let limit = Segments::whole(Self::SLOW_START_LIMIT_SEGMENTS);
            self.cwnd = self.cwnd.saturating_add(acked.min(limit));
        } else {
            self.congestion_avoidance(&sample, acked);
        }
        self.action()
    }

    fn on_congestion_event(&mut self, event: CongestionEvent) -> RecoveryAction {
        match event {
            CongestionEvent::RecoveryStarted {
                trigger: RecoveryTrigger::FastRetransmit,
                outstanding_bytes,
                ..
            } => {
                // Section 4.7, fast convergence, before section 4.6's
                // reduction.
                self.w_max = if self.cwnd < self.w_max {
                    self.cwnd.scaled(
                        Self::BETA_DENOMINATOR + Self::BETA_NUMERATOR,
                        2 * Self::BETA_DENOMINATOR,
                    )
                } else {
                    self.cwnd
                };
                self.reduce(outstanding_bytes);
                self.cwnd = self
                    .ssthresh
                    .expect("a reduction sets ssthresh")
                    .max(Segments::whole(2));
                self.in_recovery = true;
            }
            CongestionEvent::RecoveryStarted {
                trigger: RecoveryTrigger::RetransmissionTimeout,
                outstanding_bytes,
                ..
            } => {
                // Section 4.8: Reno's loss window, ssthresh from β, and
                // the next stage centred on its own starting window.
                self.reduce(outstanding_bytes);
                self.cwnd = Segments::ONE;
                self.after_timeout = true;
                // RFC 5681 section 3.1: a timeout's window grows again
                // in slow start while the lost data is resent.
                self.in_recovery = false;
            }
            CongestionEvent::RecoveryEnded { .. } => self.in_recovery = false,
            CongestionEvent::PacketLost(_) => {}
        }
        self.action()
    }

    fn congestion_window(&self) -> CongestionWindow {
        let bytes = self.cwnd.to_bytes(self.smss).max(u64::from(self.smss));
        CongestionWindow::new(u32::try_from(bytes).unwrap_or(u32::MAX))
    }

    fn pacing_rate(&self) -> Option<PacingRate> {
        None
    }

    fn limits_delivery(&self) -> bool {
        false
    }
}

#[cfg(test)]
impl Cubic {
    pub(crate) fn epoch_start_nanos(&self) -> Option<u64> {
        self.epoch.map(|epoch| epoch.start_nanos)
    }

    pub(crate) fn w_max_bytes(&self) -> u64 {
        self.w_max.to_bytes(self.smss)
    }
}

/// A gain in thousandths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Gain(u64);

impl Gain {
    const UNITY: Self = Self(1000);

    fn of(self, value: u64) -> u64 {
        let gained = u128::from(value) * u128::from(self.0) / 1000;
        u64::try_from(gained).unwrap_or(u64::MAX)
    }
}

/// Kathleen Nichols' windowed running maximum, the filter the draft
/// names for `BBR.max_bw` and `BBR.extra_acked` (section 5.5.3, after
/// Linux's `lib/win_minmax.c`): the best, second-best and third-best
/// samples from successive sub-windows, so the maximum over a sliding
/// window is kept in three entries.
///
/// A sample taken at time `t` covers `t .. t + window`: it has left the
/// window once `now - t >= window`. That is the draft's prose for both
/// filters ("the current cycle and the previous full cycle" for a
/// two-cycle window, "just remember 1 round" for a one-round one);
/// Linux's filter keeps a sample one time unit longer (`dt > win`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowedMax {
    samples: [WindowedSample; 3],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WindowedSample {
    time: u64,
    value: u64,
}

impl WindowedMax {
    const fn new() -> Self {
        Self {
            samples: [WindowedSample { time: 0, value: 0 }; 3],
        }
    }

    fn update(&mut self, value: u64, time: u64, window: u64) -> u64 {
        let sample = WindowedSample { time, value };
        let [best, second, third] = &mut self.samples;
        if value >= best.value || time.saturating_sub(third.time) >= window {
            self.samples = [sample; 3];
            return value;
        }
        if value >= second.value {
            *second = sample;
            *third = sample;
        } else if value >= third.value {
            *third = sample;
        }
        let elapsed = time.saturating_sub(self.samples[0].time);
        if elapsed >= window {
            self.samples = [self.samples[1], self.samples[2], sample];
            if time.saturating_sub(self.samples[0].time) >= window {
                self.samples = [self.samples[1], self.samples[2], sample];
            }
        } else if self.samples[1].time == self.samples[0].time && elapsed > window / 4 {
            self.samples[1] = sample;
            self.samples[2] = sample;
        } else if self.samples[2].time == self.samples[1].time && elapsed > window / 2 {
            self.samples[2] = sample;
        }
        self.samples[0].value
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BbrState {
    Startup,
    Drain,
    ProbeBwDown,
    ProbeBwCruise,
    ProbeBwRefill,
    ProbeBwUp,
    ProbeRtt,
}

impl BbrState {
    const fn is_probe_bw(self) -> bool {
        matches!(
            self,
            Self::ProbeBwDown | Self::ProbeBwCruise | Self::ProbeBwRefill | Self::ProbeBwUp
        )
    }

    /// The draft's `IsProbingBW`.
    const fn is_probing_bandwidth(self) -> bool {
        matches!(self, Self::Startup | Self::ProbeBwRefill | Self::ProbeBwUp)
    }
}

/// BBRv3, draft-ietf-ccwg-bbr-07.
///
/// The pseudocode of the draft's sections 5.2 through 5.6, with the
/// names it uses (`inflight_longterm` and `inflight_shortterm` are the
/// earlier drafts' and Linux's `inflight_hi` and `inflight_lo`). Volumes
/// are bytes, times nanoseconds, rates bytes per second. "Infinity" is
/// `None`.
///
/// Two parts of the draft are absent because the transport gives them
/// nothing to act on: the undo of a spurious recovery (section 5.5.11),
/// since the transport detects no spurious recovery, and the send
/// quantum as a control output (section 5.6.3), since the transport
/// coalesces by the interface's segmentation budget; the quantum is
/// still computed for the offload budget the inflight bound includes.
#[derive(Clone, Debug)]
pub struct BbrV3 {
    smss: u64,
    initial_cwnd: u64,
    state: BbrState,
    pacing_gain: Gain,
    cwnd_gain: Gain,
    cwnd: u64,
    pacing_rate: u64,
    has_seen_rtt: bool,
    /// Whether the clock-anchored state of `OnInit` (`min_rtt_stamp`,
    /// `extra_acked_interval_start`) has been anchored. A controller is
    /// built before its connection exists, so `Now()` at
    /// initialisation is the first event it sees.
    clock_started: bool,

    round_count: u64,
    round_start: bool,
    next_round_delivered: u64,
    idle_restart: bool,
    drain_start_round: u64,

    max_bw_filter: WindowedMax,
    cycle_count: u64,
    max_bw: u64,
    bw_shortterm: Option<u64>,
    bw: u64,
    bw_latest: u64,
    inflight_latest: u64,

    min_rtt: Option<u64>,
    min_rtt_stamp: u64,
    min_rtt_expired: bool,
    extra_acked_filter: WindowedMax,
    extra_acked: u64,
    extra_acked_interval_start: u64,
    extra_acked_delivered: u64,
    offload_budget: u64,
    max_inflight: u64,
    inflight_longterm: Option<u64>,
    inflight_shortterm: Option<u64>,

    full_bw_reached: bool,
    full_bw_now: bool,
    full_bw: u64,
    full_bw_count: u32,

    loss_round_delivered: u64,
    loss_round_start: bool,
    is_loss_in_round: bool,
    loss_events_in_round: u32,
    in_loss_recovery: bool,

    is_bw_probe_stopping: bool,
    check_probe_loss: bool,
    bw_probe_up_acked: u64,
    probe_up_acked_per_inc: Option<u64>,
    bw_probe_up_rounds: u32,
    rounds_since_probe_up: u64,
    bw_probe_wait: u64,
    cycle_stamp: u64,
    prev_probe_too_high: bool,
    prev_probe_precautionary: bool,

    probe_rtt_done_stamp: Option<u64>,
    probe_rtt_round_done: bool,
    probe_rtt_cwnd: Option<u64>,
    prior_cwnd: u64,
}

impl BbrV3 {
    /// `BBR.StartupPacingGain`, 4 ln 2 rounded as the draft prints it.
    const STARTUP_PACING_GAIN: Gain = Gain(2770);
    const DRAIN_PACING_GAIN: Gain = Gain(500);
    const DEFAULT_CWND_GAIN: Gain = Gain(2000);
    const PROBE_BW_DOWN_PACING_GAIN: Gain = Gain(900);
    const PROBE_BW_UP_PACING_GAIN: Gain = Gain(1250);
    const PROBE_BW_UP_CWND_GAIN: Gain = Gain(2250);
    const PROBE_RTT_CWND_GAIN: Gain = Gain(500);
    /// `BBR.Beta`.
    const BETA: Gain = Gain(700);
    /// `BBR.Headroom`.
    const HEADROOM: Gain = Gain(150);
    /// `BBR.LossThresh`, 2%, in thousandths.
    const LOSS_THRESH_PER_MILLE: u64 = 20;
    /// `BBR.PacingMarginPercent`.
    const PACING_MARGIN_PERCENT: u64 = 1;
    /// A full-pipe round grows bandwidth by less than 25%.
    const FULL_BW_GROWTH: Gain = Gain(1250);
    const FULL_BW_ROUNDS: u32 = 3;
    /// `BBR.StartupFullLossCnt`.
    const STARTUP_FULL_LOSS_COUNT: u32 = 6;
    /// `BBR.MinPipeCwnd`, in segments.
    const MIN_PIPE_CWND_SEGMENTS: u64 = 4;
    /// RFC 6928's initial window, `C.InitialCwnd`.
    const INITIAL_CWND_SEGMENTS: u64 = 10;
    /// `BBR.MaxBwFilterLen`, in ProbeBW cycles.
    const MAX_BW_FILTER_LEN: u64 = 2;
    /// `BBR.ExtraAckedFilterLen`, in round trips.
    const EXTRA_ACKED_FILTER_LEN: u64 = 10;
    const MIN_RTT_FILTER_LEN_NANOS: u64 = 10_000_000_000;
    const PROBE_RTT_DURATION_NANOS: u64 = 200_000_000;
    /// Drain gives up waiting for the queue after this many rounds.
    const DRAIN_MAX_ROUNDS: u64 = 3;
    /// `PickProbeWait`: two seconds plus up to one more.
    const PROBE_WAIT_BASE_NANOS: u64 = 2_000_000_000;
    const PROBE_WAIT_RANGE_NANOS: u64 = 1_000_000_000;
    /// `IsRenoCoexistenceProbeTime`'s bound on the rounds between
    /// probes.
    const RENO_PROBE_ROUNDS: u64 = 63;
    /// `RaiseInflightLongtermSlope` doubles its growth for at most this
    /// many rounds.
    const MAX_PROBE_UP_ROUNDS: u32 = 30;
    /// `SetSendQuantum`'s ceiling, 64 KBytes.
    const MAX_SEND_QUANTUM_BYTES: u64 = 64 * 1024;
    /// `SetSendQuantum` sizes the quantum to one millisecond of pacing.
    const SEND_QUANTUM_NANOS: u64 = 1_000_000;
    /// `InitPacingRate`'s round trip when no RTT is known yet.
    const DEFAULT_INITIAL_RTT_NANOS: u64 = 1_000_000;
    const NANOS_PER_SECOND: u128 = 1_000_000_000;

    pub const fn new(mss: u32) -> Self {
        assert!(mss != 0, "MSS must be non-zero");
        let smss = mss as u64;
        let initial_cwnd = smss * Self::INITIAL_CWND_SEGMENTS;
        let send_quantum = 2 * smss;
        Self {
            smss,
            initial_cwnd,
            state: BbrState::Startup,
            pacing_gain: Self::STARTUP_PACING_GAIN,
            cwnd_gain: Self::DEFAULT_CWND_GAIN,
            cwnd: initial_cwnd,
            pacing_rate: Self::initial_pacing_rate(initial_cwnd, Self::DEFAULT_INITIAL_RTT_NANOS),
            has_seen_rtt: false,
            clock_started: false,
            round_count: 0,
            round_start: false,
            next_round_delivered: 0,
            idle_restart: false,
            drain_start_round: 0,
            max_bw_filter: WindowedMax::new(),
            cycle_count: 0,
            max_bw: 0,
            bw_shortterm: None,
            bw: 0,
            bw_latest: 0,
            inflight_latest: 0,
            min_rtt: None,
            min_rtt_stamp: 0,
            min_rtt_expired: false,
            extra_acked_filter: WindowedMax::new(),
            extra_acked: 0,
            extra_acked_interval_start: 0,
            extra_acked_delivered: 0,
            offload_budget: 3 * send_quantum,
            max_inflight: initial_cwnd,
            inflight_longterm: None,
            inflight_shortterm: None,
            full_bw_reached: false,
            full_bw_now: false,
            full_bw: 0,
            full_bw_count: 0,
            loss_round_delivered: 0,
            loss_round_start: false,
            is_loss_in_round: false,
            loss_events_in_round: 0,
            in_loss_recovery: false,
            is_bw_probe_stopping: false,
            check_probe_loss: false,
            bw_probe_up_acked: 0,
            probe_up_acked_per_inc: None,
            bw_probe_up_rounds: 0,
            rounds_since_probe_up: 0,
            bw_probe_wait: 0,
            cycle_stamp: 0,
            prev_probe_too_high: false,
            prev_probe_precautionary: false,
            probe_rtt_done_stamp: None,
            probe_rtt_round_done: false,
            probe_rtt_cwnd: None,
            prior_cwnd: 0,
        }
    }

    /// `InitPacingRate`: the startup gain over the initial window per
    /// round trip.
    const fn initial_pacing_rate(initial_cwnd: u64, rtt_nanos: u64) -> u64 {
        let nominal = initial_cwnd as u128 * Self::NANOS_PER_SECOND / rtt_nanos as u128;
        let rate = nominal * Self::STARTUP_PACING_GAIN.0 as u128 / 1000;
        if rate == 0 { 1 } else { rate as u64 }
    }

    fn start_clock(&mut self, now: u64) {
        if !self.clock_started {
            self.clock_started = true;
            self.min_rtt_stamp = now;
            self.extra_acked_interval_start = now;
        }
    }

    fn min_pipe_cwnd(&self) -> u64 {
        Self::MIN_PIPE_CWND_SEGMENTS * self.smss
    }

    fn set_state(&mut self, state: BbrState, pacing_gain: Gain, cwnd_gain: Gain) {
        self.state = state;
        self.pacing_gain = pacing_gain;
        self.cwnd_gain = cwnd_gain;
    }

    // Section 5.3: the state machine.

    fn enter_startup(&mut self) {
        self.set_state(
            BbrState::Startup,
            Self::STARTUP_PACING_GAIN,
            Self::DEFAULT_CWND_GAIN,
        );
    }

    fn enter_drain(&mut self) {
        self.set_state(
            BbrState::Drain,
            Self::DRAIN_PACING_GAIN,
            Self::DEFAULT_CWND_GAIN,
        );
        self.drain_start_round = self.round_count;
    }

    fn enter_probe_bw(&mut self, now: u64, delivered: u64) {
        self.cwnd_gain = Self::DEFAULT_CWND_GAIN;
        self.start_probe_bw_down(now, delivered);
    }

    fn start_probe_bw_down(&mut self, now: u64, delivered: u64) {
        self.reset_congestion_signals();
        self.probe_up_acked_per_inc = None;
        self.pick_probe_wait(now, delivered);
        self.cycle_stamp = now;
        self.is_bw_probe_stopping = true;
        self.start_round(delivered);
        self.set_state(
            BbrState::ProbeBwDown,
            Self::PROBE_BW_DOWN_PACING_GAIN,
            Self::DEFAULT_CWND_GAIN,
        );
    }

    fn start_probe_bw_cruise(&mut self) {
        self.set_state(
            BbrState::ProbeBwCruise,
            Gain::UNITY,
            Self::DEFAULT_CWND_GAIN,
        );
    }

    fn start_probe_bw_refill(&mut self, delivered: u64) {
        self.reset_short_term_model();
        self.bw_probe_up_rounds = 0;
        self.bw_probe_up_acked = 0;
        self.prev_probe_precautionary = false;
        self.is_bw_probe_stopping = false;
        self.start_round(delivered);
        self.set_state(
            BbrState::ProbeBwRefill,
            Gain::UNITY,
            Self::DEFAULT_CWND_GAIN,
        );
    }

    fn start_probe_bw_up(&mut self, delivery_rate: u64, delivered: u64) {
        self.start_round(delivered);
        self.reset_full_bw();
        self.full_bw = delivery_rate;
        self.set_state(
            BbrState::ProbeBwUp,
            Self::PROBE_BW_UP_PACING_GAIN,
            Self::PROBE_BW_UP_CWND_GAIN,
        );
        self.raise_inflight_longterm_slope();
    }

    /// `PickProbeWait`. The draft draws both bounds at random so that
    /// flows sharing a bottleneck do not probe in lockstep. A controller
    /// has no entropy source and every connection's is a clone of one
    /// template, so the draw is a SipHash of the cycle's start time and
    /// delivered count: two flows start their cycles at different
    /// nanoseconds, which is all the decorrelation the draft asks for.
    fn pick_probe_wait(&mut self, now: u64, delivered: u64) {
        let mut hasher = SipHasher13::new();
        hasher.write_u64(now);
        hasher.write_u64(delivered);
        let draw = hasher.finish();
        self.rounds_since_probe_up = draw & 1;
        self.bw_probe_wait =
            Self::PROBE_WAIT_BASE_NANOS + (draw >> 1) % Self::PROBE_WAIT_RANGE_NANOS;
    }

    fn update_probe_bw_cycle_phase(&mut self, sample: &AckSample) {
        if !self.full_bw_reached {
            return;
        }
        if self.adapt_long_term_model(sample) {
            return;
        }
        let delivery_rate = rate_of(sample);
        match self.state {
            BbrState::ProbeBwDown => {
                if self.is_time_to_probe_bw(sample) {
                    return;
                }
                if self.is_time_to_cruise(u64::from(sample.bytes_in_flight)) {
                    self.start_probe_bw_cruise();
                }
            }
            BbrState::ProbeBwCruise => {
                let _ = self.is_time_to_probe_bw(sample);
            }
            BbrState::ProbeBwRefill => {
                if self.round_start {
                    self.check_probe_loss = true;
                    self.start_probe_bw_up(delivery_rate, sample.delivered);
                }
            }
            BbrState::ProbeBwUp => {
                if self.is_time_to_go_down(sample) {
                    self.prev_probe_too_high = false;
                    self.start_probe_bw_down(sample.now_nanos, sample.delivered);
                }
            }
            BbrState::Startup | BbrState::Drain | BbrState::ProbeRtt => {}
        }
    }

    fn is_time_to_probe_bw(&mut self, sample: &AckSample) -> bool {
        if self.has_elapsed_in_phase(self.bw_probe_wait, sample.now_nanos)
            || self.is_reno_coexistence_probe_time()
        {
            self.start_probe_bw_refill(sample.delivered);
            return true;
        }
        false
    }

    fn has_elapsed_in_phase(&self, interval: u64, now: u64) -> bool {
        now > self.cycle_stamp + interval
    }

    fn is_reno_coexistence_probe_time(&self) -> bool {
        // `TargetInflight` is a volume and the bound a round count; the
        // draft's round count is that volume in packets, as Linux's
        // `bbr_target_inflight` returns it.
        let reno_rounds = self.target_inflight() / self.smss;
        self.rounds_since_probe_up >= reno_rounds.min(Self::RENO_PROBE_ROUNDS)
    }

    /// `TargetInflight`.
    fn target_inflight(&self) -> u64 {
        self.bdp().min(self.cwnd)
    }

    fn is_time_to_cruise(&self, inflight: u64) -> bool {
        if self
            .inflight_with_headroom()
            .is_some_and(|headroom| inflight > headroom)
        {
            return false;
        }
        inflight <= self.inflight(self.max_bw, Gain::UNITY)
    }

    fn is_time_to_go_down(&mut self, sample: &AckSample) -> bool {
        let inflight = u64::from(sample.bytes_in_flight);
        if self.prev_probe_too_high
            && self
                .inflight_longterm
                .is_some_and(|longterm| inflight >= longterm)
        {
            self.prev_probe_precautionary = true;
            return true;
        }
        if sample.cwnd_limited
            && self
                .inflight_longterm
                .is_some_and(|longterm| self.cwnd >= longterm)
        {
            self.reset_full_bw();
            self.full_bw = rate_of(sample);
        } else if self.full_bw_now {
            return true;
        }
        false
    }

    /// `InflightWithHeadroom`; `None` is Infinity.
    fn inflight_with_headroom(&self) -> Option<u64> {
        let longterm = self.inflight_longterm?;
        let headroom = self.smss.max(Self::HEADROOM.of(longterm));
        Some(longterm.saturating_sub(headroom).max(self.min_pipe_cwnd()))
    }

    fn raise_inflight_longterm_slope(&mut self) {
        let growth_this_round = 1u64 << self.bw_probe_up_rounds;
        self.bw_probe_up_rounds = (self.bw_probe_up_rounds + 1).min(Self::MAX_PROBE_UP_ROUNDS);
        self.probe_up_acked_per_inc = Some((self.cwnd / growth_this_round).max(self.smss));
    }

    fn probe_inflight_longterm_upward(&mut self, sample: &AckSample) {
        let Some(longterm) = self.inflight_longterm else {
            return;
        };
        if !sample.cwnd_limited || self.cwnd < longterm {
            return;
        }
        self.bw_probe_up_acked += u64::from(sample.newly_acked);
        if let Some(per_inc) = self.probe_up_acked_per_inc
            && self.bw_probe_up_acked >= per_inc
        {
            let delta = self.bw_probe_up_acked / per_inc;
            self.bw_probe_up_acked -= delta * per_inc;
            self.inflight_longterm = Some(longterm + delta * self.smss);
        }
        if self.round_start {
            self.raise_inflight_longterm_slope();
        }
    }

    /// `AdaptLongTermModel`; `true` when it decided a state transition.
    fn adapt_long_term_model(&mut self, sample: &AckSample) -> bool {
        let rate = sample.rate;
        if self.is_bw_probe_stopping && self.round_start {
            self.check_probe_loss = false;
            self.is_bw_probe_stopping = false;
            let app_limited = rate.is_some_and(|rate| rate.is_app_limited);
            if self.state.is_probe_bw() && !app_limited {
                // `AdvanceMaxBwFilter`.
                self.cycle_count += 1;
            }
            if self.state.is_probe_bw()
                && self.prev_probe_precautionary
                && !self.prev_probe_too_high
            {
                self.start_probe_bw_refill(sample.delivered);
                return true;
            }
        }
        let Some(rate) = rate else {
            return false;
        };
        if !self.is_inflight_too_high(rate.lost, rate.tx_in_flight, sample.has_selective_acks) {
            let Some(longterm) = self.inflight_longterm else {
                return false;
            };
            let tx_in_flight = u64::from(rate.tx_in_flight);
            if tx_in_flight > longterm {
                self.inflight_longterm = Some(tx_in_flight);
            }
            if self.state == BbrState::ProbeBwUp {
                self.probe_inflight_longterm_upward(sample);
            }
        }
        false
    }

    fn update_min_rtt(&mut self, sample: &AckSample) {
        let now = sample.now_nanos;
        self.min_rtt_expired = now > self.min_rtt_stamp + Self::MIN_RTT_FILTER_LEN_NANOS;
        if self.min_rtt_expired {
            self.probe_rtt_cwnd = Some(self.bdp_multiple(Self::PROBE_RTT_CWND_GAIN));
        }
        if let Some(rtt) = sample.rtt_nanos
            && (self.min_rtt.is_none_or(|min_rtt| rtt < min_rtt) || self.min_rtt_expired)
        {
            self.min_rtt = Some(rtt);
            self.min_rtt_stamp = now;
        }
    }

    fn check_probe_rtt(&mut self, sample: &AckSample) {
        if self.state != BbrState::ProbeRtt && self.min_rtt_expired && !self.idle_restart {
            self.save_cwnd();
            self.set_state(BbrState::ProbeRtt, Gain::UNITY, Self::PROBE_RTT_CWND_GAIN);
            self.probe_rtt_done_stamp = None;
            self.is_bw_probe_stopping = true;
            self.start_round(sample.delivered);
        }
        if self.state == BbrState::ProbeRtt {
            self.handle_probe_rtt(sample);
        }
        if sample.rate.is_some_and(|rate| rate.delivered > 0) {
            self.idle_restart = false;
        }
    }

    /// `HandleProbeRTT`. Its `MarkConnectionAppLimited` is the
    /// transport's to perform: it asks [`CongestionControl::limits_delivery`]
    /// after every acknowledgement.
    fn handle_probe_rtt(&mut self, sample: &AckSample) {
        let now = sample.now_nanos;
        if self.probe_rtt_done_stamp.is_none()
            && u64::from(sample.bytes_in_flight) <= self.probe_rtt_cwnd_value()
        {
            self.probe_rtt_done_stamp = Some(now + Self::PROBE_RTT_DURATION_NANOS);
            self.probe_rtt_round_done = false;
            self.start_round(sample.delivered);
        } else if self.probe_rtt_done_stamp.is_some() {
            if self.round_start {
                self.probe_rtt_round_done = true;
            }
            if self.probe_rtt_round_done {
                self.check_probe_rtt_done(now, sample.delivered);
            }
        }
    }

    fn check_probe_rtt_done(&mut self, now: u64, delivered: u64) {
        if self.probe_rtt_done_stamp.is_some_and(|done| now > done) {
            self.min_rtt_stamp = now;
            self.restore_cwnd();
            self.exit_probe_rtt(now, delivered);
        }
    }

    fn exit_probe_rtt(&mut self, now: u64, delivered: u64) {
        self.reset_short_term_model();
        if self.full_bw_reached {
            self.start_probe_bw_down(now, delivered);
            self.start_probe_bw_cruise();
        } else {
            self.enter_startup();
        }
    }

    // Section 5.5: the path model.

    fn start_round(&mut self, delivered: u64) {
        self.next_round_delivered = delivered;
    }

    fn update_round(&mut self, sample: &AckSample) {
        if let Some(rate) = sample.rate
            && rate.prior_delivered >= self.next_round_delivered
        {
            self.start_round(sample.delivered);
            self.round_count += 1;
            self.rounds_since_probe_up += 1;
            self.round_start = true;
        } else {
            self.round_start = false;
        }
    }

    fn update_max_bw(&mut self, sample: &AckSample) {
        self.update_round(sample);
        if let Some(rate) = sample.rate
            && let Some(delivery_rate) = rate.delivery_rate
            && delivery_rate > 0
            && (delivery_rate >= self.max_bw || !rate.is_app_limited)
        {
            self.max_bw =
                self.max_bw_filter
                    .update(delivery_rate, self.cycle_count, Self::MAX_BW_FILTER_LEN);
        }
    }

    fn update_latest_delivery_signals(&mut self, sample: &AckSample) {
        self.loss_round_start = false;
        self.bw_latest = self.bw_latest.max(rate_of(sample));
        self.inflight_latest = self.inflight_latest.max(delivered_of(sample));
        if let Some(rate) = sample.rate
            && rate.prior_delivered >= self.loss_round_delivered
        {
            self.loss_round_delivered = sample.delivered;
            self.loss_round_start = true;
        }
    }

    fn advance_latest_delivery_signals(&mut self, sample: &AckSample) {
        if self.loss_round_start {
            self.bw_latest = rate_of(sample);
            self.inflight_latest = delivered_of(sample);
            self.loss_events_in_round = 0;
        }
    }

    fn reset_congestion_signals(&mut self) {
        self.is_loss_in_round = false;
        self.loss_events_in_round = 0;
        self.bw_latest = 0;
        self.inflight_latest = 0;
    }

    fn update_congestion_signals(&mut self, sample: &AckSample) {
        self.update_max_bw(sample);
        if !self.loss_round_start {
            return;
        }
        if !self.state.is_probing_bandwidth() && self.is_loss_in_round {
            // `AdaptLowerBoundsFromCongestion`: `InitLowerBounds`, then
            // `LossLowerBounds`.
            let bw_shortterm = *self.bw_shortterm.get_or_insert(self.max_bw);
            let inflight_shortterm = *self.inflight_shortterm.get_or_insert(self.cwnd);
            self.bw_shortterm = Some(self.bw_latest.max(Self::BETA.of(bw_shortterm)));
            self.inflight_shortterm =
                Some(self.inflight_latest.max(Self::BETA.of(inflight_shortterm)));
        }
        self.is_loss_in_round = false;
    }

    fn reset_short_term_model(&mut self) {
        self.bw_shortterm = None;
        self.inflight_shortterm = None;
    }

    fn bound_bw_for_model(&mut self) {
        self.bw = self
            .bw_shortterm
            .map_or(self.max_bw, |shortterm| self.max_bw.min(shortterm));
    }

    fn update_ack_aggregation(&mut self, sample: &AckSample) {
        let now = sample.now_nanos;
        let interval = now.saturating_sub(self.extra_acked_interval_start);
        let mut expected_delivered = bytes_over(self.bw, interval);
        if self.extra_acked_delivered <= expected_delivered {
            self.extra_acked_delivered = 0;
            self.extra_acked_interval_start = now;
            expected_delivered = 0;
        }
        self.extra_acked_delivered += u64::from(sample.newly_acked);
        let extra = self
            .extra_acked_delivered
            .saturating_sub(expected_delivered)
            .min(self.cwnd);
        let filter_len = if self.full_bw_reached {
            Self::EXTRA_ACKED_FILTER_LEN
        } else {
            1
        };
        self.extra_acked = self
            .extra_acked_filter
            .update(extra, self.round_count, filter_len);
    }

    fn reset_full_bw(&mut self) {
        self.full_bw = 0;
        self.full_bw_count = 0;
        self.full_bw_now = false;
    }

    fn check_full_bw_reached(&mut self, sample: &AckSample) {
        let Some(rate) = sample.rate else {
            return;
        };
        if self.full_bw_now || !self.round_start || rate.is_app_limited {
            return;
        }
        let delivery_rate = rate.delivery_rate.unwrap_or(0);
        if delivery_rate >= Self::FULL_BW_GROWTH.of(self.full_bw) {
            self.reset_full_bw();
            self.full_bw = delivery_rate;
            return;
        }
        self.full_bw_count += 1;
        self.full_bw_now = self.full_bw_count >= Self::FULL_BW_ROUNDS;
        if self.full_bw_now {
            self.full_bw_reached = true;
        }
    }

    fn check_startup_done(&mut self, sample: &AckSample) {
        self.check_startup_high_loss(sample);
        if self.state == BbrState::Startup && self.full_bw_reached {
            self.enter_drain();
        }
    }

    fn check_startup_high_loss(&mut self, sample: &AckSample) {
        if self.full_bw_reached || !self.loss_round_start || !self.in_loss_recovery {
            return;
        }
        if self.loss_events_in_round < Self::STARTUP_FULL_LOSS_COUNT && sample.has_selective_acks {
            return;
        }
        let Some(rate) = sample.rate else {
            return;
        };
        if !self.is_inflight_too_high(rate.lost, rate.tx_in_flight, sample.has_selective_acks) {
            return;
        }
        self.full_bw_reached = true;
        self.inflight_longterm = Some(self.bdp().max(self.inflight_latest));
    }

    fn check_drain_done(&mut self, sample: &AckSample) {
        if self.state == BbrState::Drain
            && (u64::from(sample.bytes_in_flight) <= self.inflight(self.bw, Gain::UNITY)
                || self.round_count > self.drain_start_round + Self::DRAIN_MAX_ROUNDS)
        {
            self.enter_probe_bw(sample.now_nanos, sample.delivered);
        }
    }

    /// `IsInflightTooHigh` over one sample's `RS.lost` and
    /// `RS.tx_in_flight`.
    fn is_inflight_too_high(&self, lost: u64, tx_in_flight: u32, has_selective_acks: bool) -> bool {
        lost * 1000 > u64::from(tx_in_flight) * Self::LOSS_THRESH_PER_MILLE
            || (lost > 0 && !has_selective_acks)
    }

    fn note_loss(&mut self, delivered: u64) {
        if !self.is_loss_in_round {
            self.loss_round_delivered = delivered;
            // `SaveStateUponLoss`, whose undo half has no spurious
            // recovery to serve.
            self.save_cwnd();
        }
        self.is_loss_in_round = true;
        self.loss_events_in_round += 1;
    }

    fn handle_lost_packet(&mut self, lost: LostPacket) {
        self.note_loss(lost.delivered);
        if !self.check_probe_loss {
            return;
        }
        let tx_in_flight = lost.sent.tx_in_flight;
        let lost_since_send = lost.lost - lost.sent.lost;
        if self.is_inflight_too_high(lost_since_send, tx_in_flight, lost.has_selective_acks) {
            let at_loss = Self::inflight_at_loss(lost.bytes, tx_in_flight, lost_since_send);
            self.handle_inflight_too_high(
                at_loss,
                lost.sent.is_app_limited,
                lost.now_nanos,
                lost.delivered,
            );
        }
    }

    /// `InflightAtLoss`: where in this segment the loss rate crossed
    /// `BBR.LossThresh`. A negative prefix (the threshold was crossed
    /// before this segment) is kept, as the draft's arithmetic keeps it;
    /// the result floors at zero.
    fn inflight_at_loss(size: u32, tx_in_flight: u32, lost: u64) -> u64 {
        let inflight_prev = i128::from(tx_in_flight) - i128::from(size);
        let lost_prev = i128::from(lost) - i128::from(size);
        let thresh = i128::from(Self::LOSS_THRESH_PER_MILLE);
        let lost_prefix = (thresh * inflight_prev - 1000 * lost_prev) / (1000 - thresh);
        u64::try_from((inflight_prev + lost_prefix).max(0)).unwrap_or(u64::MAX)
    }

    fn handle_inflight_too_high(
        &mut self,
        tx_in_flight: u64,
        app_limited: bool,
        now: u64,
        delivered: u64,
    ) {
        self.prev_probe_too_high = true;
        self.check_probe_loss = false;
        if !app_limited {
            self.inflight_longterm = Some(tx_in_flight.max(Self::BETA.of(self.target_inflight())));
        }
        if self.state == BbrState::ProbeBwUp {
            self.start_probe_bw_down(now, delivered);
        }
    }

    // Section 5.6: control parameters.

    /// `BBR.bdp` = `BBR.bw * BBR.min_rtt`; zero until a round trip has
    /// been measured.
    fn bdp(&self) -> u64 {
        self.min_rtt
            .map_or(0, |min_rtt| bytes_over(self.bw, min_rtt))
    }

    /// `BDPMultiple(gain)`.
    fn bdp_multiple(&self, gain: Gain) -> u64 {
        self.bdp_multiple_of(self.bw, gain)
    }

    fn bdp_multiple_of(&self, bw: u64, gain: Gain) -> u64 {
        match self.min_rtt {
            None => self.initial_cwnd,
            Some(min_rtt) => gain.of(bytes_over(bw, min_rtt)),
        }
    }

    fn quantization_budget(&self, inflight_cap: u64) -> u64 {
        let cap = inflight_cap
            .max(self.offload_budget)
            .max(self.min_pipe_cwnd());
        if self.state == BbrState::ProbeBwUp {
            cap + 2 * self.smss
        } else {
            cap
        }
    }

    /// `Inflight(bw, gain)`.
    fn inflight(&self, bw: u64, gain: Gain) -> u64 {
        self.quantization_budget(self.bdp_multiple_of(bw, gain))
    }

    fn update_max_inflight(&mut self) {
        let inflight_cap = self.bdp_multiple(self.cwnd_gain) + self.extra_acked;
        self.max_inflight = self.quantization_budget(inflight_cap);
    }

    fn set_pacing_rate_with_gain(&mut self, gain: Gain) {
        let rate = gain.of(self.bw) * (100 - Self::PACING_MARGIN_PERCENT) / 100;
        // A zero rate is the absence of a measurement, not a rate: the
        // draft's arithmetic would stop the flow if a pipe were declared
        // full before any sample carried a rate.
        if rate != 0 && (self.full_bw_reached || rate > self.pacing_rate) {
            self.pacing_rate = rate;
        }
    }

    fn set_send_quantum(&mut self) {
        let quantum = bytes_over(self.pacing_rate, Self::SEND_QUANTUM_NANOS)
            .min(Self::MAX_SEND_QUANTUM_BYTES)
            .max(2 * self.smss);
        // `UpdateOffloadBudget` for TCP.
        self.offload_budget = 3 * quantum;
    }

    fn set_cwnd(&mut self, sample: &AckSample) {
        self.update_max_inflight();
        let newly_acked = u64::from(sample.newly_acked);
        if self.full_bw_reached {
            self.cwnd = (self.cwnd + newly_acked).min(self.max_inflight);
        } else if self.cwnd < self.max_inflight || sample.delivered < self.initial_cwnd {
            self.cwnd += newly_acked;
        }
        self.cwnd = self.cwnd.max(self.min_pipe_cwnd());
        if self.state == BbrState::ProbeRtt {
            self.cwnd = self.cwnd.min(self.probe_rtt_cwnd_value());
        }
        self.bound_cwnd_for_model();
    }

    /// `ProbeRTTCwnd`.
    fn probe_rtt_cwnd_value(&self) -> u64 {
        let cwnd = self.bdp_multiple(Self::PROBE_RTT_CWND_GAIN);
        let cwnd = self
            .probe_rtt_cwnd
            .map_or(cwnd, |probe_rtt_cwnd| cwnd.min(probe_rtt_cwnd));
        cwnd.max(self.min_pipe_cwnd())
    }

    fn bound_cwnd_for_model(&mut self) {
        // The draft's summary table also lists `inflight_longterm` as a
        // volume cap in Drain; its normative `BoundCwndForModel`, which
        // this follows, does not.
        let cap = match self.state {
            BbrState::ProbeBwDown | BbrState::ProbeBwRefill | BbrState::ProbeBwUp => {
                self.inflight_longterm
            }
            BbrState::ProbeBwCruise | BbrState::ProbeRtt => self.inflight_with_headroom(),
            BbrState::Startup | BbrState::Drain => None,
        };
        let cap = match (cap, self.inflight_shortterm) {
            (Some(cap), Some(shortterm)) => Some(cap.min(shortterm)),
            (cap, shortterm) => cap.or(shortterm),
        };
        if let Some(cap) = cap {
            self.cwnd = self.cwnd.min(cap.max(self.min_pipe_cwnd()));
        }
    }

    /// `SaveCwnd`.
    fn save_cwnd(&mut self) {
        self.prior_cwnd = if !self.in_loss_recovery && self.state != BbrState::ProbeRtt {
            self.cwnd
        } else {
            self.prior_cwnd.max(self.cwnd)
        };
    }

    /// `RestoreCwnd`.
    fn restore_cwnd(&mut self) {
        self.cwnd = self.cwnd.max(self.prior_cwnd);
    }

    fn action(&self) -> RecoveryAction {
        RecoveryAction {
            cwnd: self.congestion_window(),
            pacing_rate: self.pacing_rate(),
        }
    }
}

impl CongestionControl for BbrV3 {
    fn algorithm_name(&self) -> &'static str {
        "bbr-v3"
    }

    fn on_packet_sent(&mut self, packet: PacketSent) {
        let now = packet.now_nanos;
        self.start_clock(now);
        // `HandleRestartFromIdle`, which the draft runs before the
        // transmission, when nothing was in flight.
        let in_flight_before = packet.bytes_in_flight.saturating_sub(packet.bytes);
        if in_flight_before == 0 && packet.app_limited {
            self.idle_restart = true;
            self.extra_acked_interval_start = now;
            if self.state.is_probe_bw() {
                self.set_pacing_rate_with_gain(Gain::UNITY);
            } else if self.state == BbrState::ProbeRtt {
                self.check_probe_rtt_done(now, packet.delivered);
            }
        }
    }

    fn on_ack(&mut self, sample: AckSample) -> RecoveryAction {
        self.start_clock(sample.now_nanos);
        if !self.has_seen_rtt
            && let Some(srtt) = sample.smoothed_rtt_nanos.filter(|srtt| *srtt != 0)
        {
            self.has_seen_rtt = true;
            self.pacing_rate = Self::initial_pacing_rate(self.initial_cwnd, srtt);
        }
        // `UpdateModelAndState`.
        self.update_latest_delivery_signals(&sample);
        self.update_congestion_signals(&sample);
        self.update_ack_aggregation(&sample);
        self.check_full_bw_reached(&sample);
        self.check_startup_done(&sample);
        self.check_drain_done(&sample);
        self.update_probe_bw_cycle_phase(&sample);
        self.update_min_rtt(&sample);
        self.check_probe_rtt(&sample);
        self.advance_latest_delivery_signals(&sample);
        self.bound_bw_for_model();
        // `UpdateControlParameters`.
        self.set_pacing_rate_with_gain(self.pacing_gain);
        self.set_send_quantum();
        self.set_cwnd(&sample);
        self.action()
    }

    fn on_congestion_event(&mut self, event: CongestionEvent) -> RecoveryAction {
        match event {
            CongestionEvent::PacketLost(lost) => self.handle_lost_packet(lost),
            CongestionEvent::RecoveryStarted {
                trigger,
                bytes_in_flight,
                now_nanos,
                ..
            } => {
                self.start_clock(now_nanos);
                // `OnEnterFastRecovery` and `OnEnterRTO`: `SaveCwnd` and
                // `SaveStateUponLoss` both save the window.
                self.save_cwnd();
                if trigger == RecoveryTrigger::RetransmissionTimeout {
                    // "A value that will allow 1 C.SMSS to be
                    // transmitted": the draft's `C.inflight + 1` is in
                    // packets.
                    self.cwnd = u64::from(bytes_in_flight) + self.smss;
                }
                self.in_loss_recovery = true;
            }
            CongestionEvent::RecoveryEnded { .. } => {
                self.restore_cwnd();
                self.in_loss_recovery = false;
            }
        }
        self.action()
    }

    fn congestion_window(&self) -> CongestionWindow {
        CongestionWindow::new(u32::try_from(self.cwnd).unwrap_or(u32::MAX))
    }

    fn pacing_rate(&self) -> Option<PacingRate> {
        Some(PacingRate::from_bytes_per_second(self.pacing_rate))
    }

    fn limits_delivery(&self) -> bool {
        self.state == BbrState::ProbeRtt
    }
}

#[cfg(test)]
impl BbrV3 {
    pub(crate) fn state(&self) -> BbrState {
        self.state
    }

    pub(crate) fn max_bw(&self) -> u64 {
        self.max_bw
    }

    pub(crate) fn min_rtt(&self) -> Option<u64> {
        self.min_rtt
    }

    pub(crate) fn round_count(&self) -> u64 {
        self.round_count
    }

    pub(crate) fn round_start(&self) -> bool {
        self.round_start
    }

    pub(crate) fn extra_acked(&self) -> u64 {
        self.extra_acked
    }

    pub(crate) fn inflight_longterm(&self) -> Option<u64> {
        self.inflight_longterm
    }

    pub(crate) fn inflight_shortterm(&self) -> Option<u64> {
        self.inflight_shortterm
    }

    pub(crate) fn cwnd_gain_per_mille(&self) -> u64 {
        self.cwnd_gain.0
    }

    pub(crate) fn in_loss_recovery(&self) -> bool {
        self.in_loss_recovery
    }
}

/// The sample's delivery rate, zero when it carries none.
fn rate_of(sample: &AckSample) -> u64 {
    sample.rate.and_then(|rate| rate.delivery_rate).unwrap_or(0)
}

/// The sample's `RS.delivered`, zero when it carries none.
fn delivered_of(sample: &AckSample) -> u64 {
    sample.rate.map_or(0, |rate| rate.delivered)
}

/// Bytes `rate` (bytes per second) moves in `nanos`.
fn bytes_over(rate: u64, nanos: u64) -> u64 {
    let bytes = u128::from(rate) * u128::from(nanos) / 1_000_000_000;
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSS: u32 = 1460;

    /// An acknowledgement of `newly_acked` bytes on a window-limited
    /// flow.
    fn ack(newly_acked: u32, now_nanos: u64) -> AckSample {
        AckSample {
            newly_acked,
            delivered: 0,
            bytes_in_flight: 0,
            rtt_nanos: Some(20_000_000),
            smoothed_rtt_nanos: Some(20_000_000),
            rate: None,
            cwnd_limited: true,
            has_selective_acks: false,
            now_nanos,
        }
    }

    fn fast_retransmit(outstanding_bytes: u32) -> CongestionEvent {
        CongestionEvent::RecoveryStarted {
            trigger: RecoveryTrigger::FastRetransmit,
            outstanding_bytes,
            bytes_in_flight: outstanding_bytes,
            now_nanos: 0,
        }
    }

    /// Whatever the ACKs inside a fast-recovery episode report, the
    /// window they leave is the one the reduction set; growth resumes
    /// on the ACK after the episode ends.
    fn assert_holds_through_fast_recovery<C: CongestionControl>(mut congestion: C) {
        let reduced = congestion
            .on_congestion_event(fast_retransmit(20 * MSS))
            .cwnd;
        for step in 1..=10 {
            assert_eq!(congestion.on_ack(ack(MSS, step)).cwnd, reduced);
        }
        let _ = congestion.on_congestion_event(CongestionEvent::RecoveryEnded { now_nanos: 11 });
        assert!(congestion.on_ack(ack(MSS, 12)).cwnd.bytes() > reduced.bytes());
    }

    #[test]
    fn newreno_holds_its_window_through_fast_recovery() {
        assert_holds_through_fast_recovery(NewReno::new(MSS));
    }

    #[test]
    fn cubic_holds_its_window_through_fast_recovery() {
        assert_holds_through_fast_recovery(Cubic::new(MSS));
    }

    #[test]
    fn the_windowed_max_forgets_a_sample_once_its_window_has_passed() {
        let mut filter = WindowedMax::new();
        assert_eq!(filter.update(100, 0, 2), 100);
        assert_eq!(
            filter.update(60, 1, 2),
            100,
            "the previous cycle still counts"
        );
        assert_eq!(
            filter.update(50, 2, 2),
            60,
            "two cycles on, cycle 0 has left and cycle 1's best remains"
        );
        assert_eq!(filter.update(40, 4, 2), 40, "nothing older is left");
    }

    #[test]
    fn a_one_round_window_remembers_only_the_current_round() {
        let mut filter = WindowedMax::new();
        assert_eq!(filter.update(9, 5, 1), 9);
        assert_eq!(filter.update(3, 5, 1), 9);
        assert_eq!(filter.update(2, 6, 1), 2);
    }

    #[test]
    fn inflight_at_loss_finds_where_the_threshold_was_crossed() {
        // 100 segments in flight before a lost 1460-byte segment with
        // nothing lost before it: the 2% threshold is crossed only once
        // the loss itself is counted, so the crossing point is the
        // inflight that makes one segment 2% of it, less the prefix.
        let mss = 1460;
        let before = 100 * mss;
        let at_loss = BbrV3::inflight_at_loss(mss, before + mss, u64::from(mss));
        // lost_prefix = (0.02 * 146000 - 0) / 0.98 = 2979.59..., floored.
        assert_eq!(at_loss, u64::from(before) + 2979);
    }

    #[test]
    fn cubic_k_is_the_cube_root_of_the_shortfall_over_c() {
        // W_max = 100 segments, cwnd_epoch = 70: K = cbrt(30 / 0.4) s =
        // cbrt(75) s = 4.2172 s.
        let k = Cubic::k_micros(Segments::whole(100), Segments::whole(70));
        assert!(k.abs_diff(4_217_163) <= 1, "K = {k} us");
    }
}
