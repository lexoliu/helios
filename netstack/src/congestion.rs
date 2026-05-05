use core::cmp;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AckSample {
    pub acked_bytes: u32,
    pub delivered_bytes: u64,
    pub bytes_in_flight: u32,
    pub rtt_nanos: u64,
    pub interval_nanos: u64,
    pub now_nanos: u64,
    pub app_limited: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CongestionEvent {
    PacketLoss {
        lost_bytes: u32,
        bytes_in_flight: u32,
        now_nanos: u64,
    },
    ExplicitCongestion {
        bytes_in_flight: u32,
        now_nanos: u64,
    },
    RetransmissionTimeout {
        now_nanos: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryAction {
    pub cwnd: CongestionWindow,
    pub pacing_rate: Option<PacingRate>,
}

pub trait CongestionControl: Clone + Send + Sync + 'static {
    fn algorithm_name(&self) -> &'static str;

    fn on_packet_sent(&mut self, bytes: u32, bytes_in_flight: u32, now_nanos: u64);

    fn on_ack(&mut self, sample: AckSample) -> RecoveryAction;

    fn on_congestion_event(&mut self, event: CongestionEvent) -> RecoveryAction;

    fn congestion_window(&self) -> CongestionWindow;

    fn pacing_rate(&self) -> Option<PacingRate>;
}

#[derive(Clone, Debug)]
pub struct NewReno {
    cwnd: u32,
    ssthresh: u32,
    mss: u32,
}

impl NewReno {
    pub const fn new(mss: u32) -> Self {
        assert!(mss != 0, "MSS must be non-zero");
        Self {
            cwnd: mss * 10,
            ssthresh: u32::MAX,
            mss,
        }
    }
}

impl CongestionControl for NewReno {
    fn algorithm_name(&self) -> &'static str {
        "newreno"
    }

    fn on_packet_sent(&mut self, _bytes: u32, _bytes_in_flight: u32, _now_nanos: u64) {}

    fn on_ack(&mut self, sample: AckSample) -> RecoveryAction {
        if self.cwnd < self.ssthresh {
            self.cwnd = self.cwnd.saturating_add(sample.acked_bytes);
        } else {
            let increment = ((u64::from(self.mss) * u64::from(sample.acked_bytes))
                / u64::from(self.cwnd.max(1)))
            .max(1);
            self.cwnd = self.cwnd.saturating_add(increment as u32);
        }
        RecoveryAction {
            cwnd: self.congestion_window(),
            pacing_rate: None,
        }
    }

    fn on_congestion_event(&mut self, event: CongestionEvent) -> RecoveryAction {
        match event {
            CongestionEvent::PacketLoss {
                bytes_in_flight, ..
            }
            | CongestionEvent::ExplicitCongestion {
                bytes_in_flight, ..
            } => {
                self.ssthresh = cmp::max(bytes_in_flight / 2, self.mss * 2);
                self.cwnd = self.ssthresh;
            }
            CongestionEvent::RetransmissionTimeout { .. } => {
                self.ssthresh = cmp::max(self.cwnd / 2, self.mss * 2);
                self.cwnd = self.mss;
            }
        }
        RecoveryAction {
            cwnd: self.congestion_window(),
            pacing_rate: None,
        }
    }

    fn congestion_window(&self) -> CongestionWindow {
        CongestionWindow::new(self.cwnd.max(self.mss))
    }

    fn pacing_rate(&self) -> Option<PacingRate> {
        None
    }
}

#[derive(Clone, Debug)]
pub struct Cubic {
    reno_floor: NewReno,
    cwnd: u32,
    mss: u32,
    epoch_start_nanos: Option<u64>,
    last_max_cwnd: u32,
    origin_point_cwnd: u32,
    k_scaled: u64,
}

impl Cubic {
    const BETA_NUMERATOR: u32 = 717;
    const BETA_DENOMINATOR: u32 = 1024;
    const CUBIC_SCALE: u64 = 410;

    pub const fn new(mss: u32) -> Self {
        assert!(mss != 0, "MSS must be non-zero");
        Self {
            reno_floor: NewReno::new(mss),
            cwnd: mss * 10,
            mss,
            epoch_start_nanos: None,
            last_max_cwnd: 0,
            origin_point_cwnd: 0,
            k_scaled: 0,
        }
    }

    fn reset_epoch(&mut self) {
        self.epoch_start_nanos = None;
    }

    fn update_cubic_window(&mut self, now_nanos: u64, acked_bytes: u32) {
        let epoch_start = *self.epoch_start_nanos.get_or_insert(now_nanos);
        if self.origin_point_cwnd == 0 {
            if self.cwnd < self.last_max_cwnd {
                self.k_scaled = cubic_root_scaled(
                    (u64::from(self.last_max_cwnd - self.cwnd) << 10) / Self::CUBIC_SCALE,
                );
                self.origin_point_cwnd = self.last_max_cwnd;
            } else {
                self.k_scaled = 0;
                self.origin_point_cwnd = self.cwnd;
            }
        }

        let elapsed_scaled = now_nanos.saturating_sub(epoch_start) / 1_000_000;
        let distance = elapsed_scaled.abs_diff(self.k_scaled);
        let delta =
            ((Self::CUBIC_SCALE * distance * distance * distance) >> 10).min(u64::from(u32::MAX));
        let target = if elapsed_scaled < self.k_scaled {
            self.origin_point_cwnd.saturating_sub(delta as u32)
        } else {
            self.origin_point_cwnd.saturating_add(delta as u32)
        };
        if target > self.cwnd {
            let cnt =
                (u64::from(self.cwnd) * u64::from(self.mss)) / u64::from(target - self.cwnd).max(1);
            let increment = ((u64::from(self.mss) * u64::from(acked_bytes)) / cnt.max(1)).max(1);
            self.cwnd = self.cwnd.saturating_add(increment as u32);
        }
        let reno = self.reno_floor.congestion_window().bytes();
        if self.cwnd < reno {
            self.cwnd = reno;
        }
    }
}

impl CongestionControl for Cubic {
    fn algorithm_name(&self) -> &'static str {
        "cubic"
    }

    fn on_packet_sent(&mut self, bytes: u32, bytes_in_flight: u32, now_nanos: u64) {
        self.reno_floor
            .on_packet_sent(bytes, bytes_in_flight, now_nanos);
    }

    fn on_ack(&mut self, sample: AckSample) -> RecoveryAction {
        self.reno_floor.on_ack(sample);
        self.update_cubic_window(sample.now_nanos, sample.acked_bytes);
        RecoveryAction {
            cwnd: self.congestion_window(),
            pacing_rate: None,
        }
    }

    fn on_congestion_event(&mut self, event: CongestionEvent) -> RecoveryAction {
        let bytes_in_flight = match event {
            CongestionEvent::PacketLoss {
                bytes_in_flight, ..
            }
            | CongestionEvent::ExplicitCongestion {
                bytes_in_flight, ..
            } => bytes_in_flight,
            CongestionEvent::RetransmissionTimeout { .. } => self.mss,
        };
        if self.cwnd < self.last_max_cwnd {
            self.last_max_cwnd = (u64::from(self.cwnd) * u64::from(1 + Self::BETA_NUMERATOR)
                / 2
                / u64::from(Self::BETA_DENOMINATOR)) as u32;
        } else {
            self.last_max_cwnd = self.cwnd;
        }
        self.cwnd = cmp::max(
            (u64::from(bytes_in_flight) * u64::from(Self::BETA_NUMERATOR)
                / u64::from(Self::BETA_DENOMINATOR)) as u32,
            self.mss * 2,
        );
        self.origin_point_cwnd = 0;
        self.reset_epoch();
        self.reno_floor.on_congestion_event(event);
        RecoveryAction {
            cwnd: self.congestion_window(),
            pacing_rate: None,
        }
    }

    fn congestion_window(&self) -> CongestionWindow {
        CongestionWindow::new(self.cwnd.max(self.mss * 2))
    }

    fn pacing_rate(&self) -> Option<PacingRate> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BbrMode {
    Startup,
    Drain,
    ProbeBw,
    ProbeRtt,
}

#[derive(Clone, Debug)]
pub struct BbrV3 {
    mode: BbrMode,
    mss: u32,
    cwnd: u32,
    pacing_rate: Option<PacingRate>,
    max_bandwidth_bytes_per_second: u64,
    min_rtt_nanos: u64,
    min_rtt_stamp_nanos: u64,
    full_bandwidth: u64,
    full_bandwidth_rounds: u8,
    delivered_at_round_start: u64,
    pacing_gain_index: usize,
    inflight_hi: u32,
    inflight_lo: u32,
    probe_rtt_done_stamp_nanos: Option<u64>,
}

impl BbrV3 {
    const STARTUP_PACING_GAIN_NUMERATOR: u64 = 2885;
    const STARTUP_PACING_GAIN_DENOMINATOR: u64 = 1000;
    const DRAIN_GAIN_NUMERATOR: u64 = 1000;
    const DRAIN_GAIN_DENOMINATOR: u64 = 2885;
    const PROBE_BW_GAINS: [(u64, u64); 8] = [
        (1250, 1000),
        (750, 1000),
        (1000, 1000),
        (1000, 1000),
        (1000, 1000),
        (1000, 1000),
        (1000, 1000),
        (1000, 1000),
    ];
    const MIN_RTT_WINDOW_NANOS: u64 = 10_000_000_000;
    const PROBE_RTT_INTERVAL_NANOS: u64 = 5_000_000_000;
    const PROBE_RTT_DURATION_NANOS: u64 = 200_000_000;
    const FULL_BW_THRESHOLD_NUMERATOR: u64 = 125;
    const FULL_BW_THRESHOLD_DENOMINATOR: u64 = 100;

    pub const fn new(mss: u32) -> Self {
        assert!(mss != 0, "MSS must be non-zero");
        Self {
            mode: BbrMode::Startup,
            mss,
            cwnd: mss * 10,
            pacing_rate: None,
            max_bandwidth_bytes_per_second: 0,
            min_rtt_nanos: u64::MAX,
            min_rtt_stamp_nanos: 0,
            full_bandwidth: 0,
            full_bandwidth_rounds: 0,
            delivered_at_round_start: 0,
            pacing_gain_index: 0,
            inflight_hi: u32::MAX,
            inflight_lo: u32::MAX,
            probe_rtt_done_stamp_nanos: None,
        }
    }

    fn bandwidth_sample(sample: AckSample) -> u64 {
        if sample.interval_nanos == 0 {
            return 0;
        }
        (u64::from(sample.acked_bytes) * 1_000_000_000) / sample.interval_nanos
    }

    fn update_model(&mut self, sample: AckSample) {
        let bandwidth = Self::bandwidth_sample(sample);
        if bandwidth > self.max_bandwidth_bytes_per_second && !sample.app_limited {
            self.max_bandwidth_bytes_per_second = bandwidth;
        }
        if sample.rtt_nanos != 0
            && (sample.rtt_nanos <= self.min_rtt_nanos
                || sample.now_nanos.saturating_sub(self.min_rtt_stamp_nanos)
                    > Self::MIN_RTT_WINDOW_NANOS)
        {
            self.min_rtt_nanos = sample.rtt_nanos;
            self.min_rtt_stamp_nanos = sample.now_nanos;
        }
        if sample.delivered_bytes >= self.delivered_at_round_start {
            self.delivered_at_round_start = sample.delivered_bytes;
            self.update_full_bandwidth();
        }
    }

    fn update_full_bandwidth(&mut self) {
        if self.max_bandwidth_bytes_per_second
            >= (self.full_bandwidth * Self::FULL_BW_THRESHOLD_NUMERATOR)
                / Self::FULL_BW_THRESHOLD_DENOMINATOR
        {
            self.full_bandwidth = self.max_bandwidth_bytes_per_second;
            self.full_bandwidth_rounds = 0;
        } else {
            self.full_bandwidth_rounds = self.full_bandwidth_rounds.saturating_add(1);
        }
    }

    fn bdp_bytes(&self, gain_num: u64, gain_den: u64) -> u32 {
        if self.max_bandwidth_bytes_per_second == 0 || self.min_rtt_nanos == u64::MAX {
            return self.mss * 10;
        }
        let bdp = self
            .max_bandwidth_bytes_per_second
            .saturating_mul(self.min_rtt_nanos)
            / 1_000_000_000;
        let gained = bdp.saturating_mul(gain_num) / gain_den;
        u32::try_from(gained).unwrap_or(u32::MAX).max(self.mss * 4)
    }

    fn update_control_parameters(&mut self, sample: AckSample) {
        match self.mode {
            BbrMode::Startup => {
                self.cwnd = self
                    .cwnd
                    .saturating_add(sample.acked_bytes)
                    .min(self.inflight_hi);
                self.pacing_rate = self.rate_with_gain(
                    Self::STARTUP_PACING_GAIN_NUMERATOR,
                    Self::STARTUP_PACING_GAIN_DENOMINATOR,
                );
                if self.full_bandwidth_rounds >= 3 {
                    self.mode = BbrMode::Drain;
                }
            }
            BbrMode::Drain => {
                self.cwnd = self.bdp_bytes(1, 1).min(self.inflight_hi);
                self.pacing_rate =
                    self.rate_with_gain(Self::DRAIN_GAIN_NUMERATOR, Self::DRAIN_GAIN_DENOMINATOR);
                if sample.bytes_in_flight <= self.cwnd {
                    self.mode = BbrMode::ProbeBw;
                }
            }
            BbrMode::ProbeBw => {
                let (gain_num, gain_den) = Self::PROBE_BW_GAINS[self.pacing_gain_index];
                self.cwnd = self
                    .bdp_bytes(2, 1)
                    .min(self.inflight_hi)
                    .min(self.inflight_lo)
                    .max(self.mss * 4);
                self.pacing_rate = self.rate_with_gain(gain_num, gain_den);
                if sample.delivered_bytes > self.delivered_at_round_start {
                    self.pacing_gain_index =
                        (self.pacing_gain_index + 1) % Self::PROBE_BW_GAINS.len();
                }
                if sample.now_nanos.saturating_sub(self.min_rtt_stamp_nanos)
                    > Self::PROBE_RTT_INTERVAL_NANOS
                {
                    self.mode = BbrMode::ProbeRtt;
                    self.probe_rtt_done_stamp_nanos = None;
                }
            }
            BbrMode::ProbeRtt => {
                self.cwnd = self.bdp_bytes(1, 2).max(self.mss * 4);
                self.pacing_rate = self.rate_with_gain(1, 1);
                let done = *self
                    .probe_rtt_done_stamp_nanos
                    .get_or_insert(sample.now_nanos + Self::PROBE_RTT_DURATION_NANOS);
                if sample.now_nanos >= done {
                    self.mode = BbrMode::ProbeBw;
                    self.pacing_gain_index = 0;
                    self.probe_rtt_done_stamp_nanos = None;
                }
            }
        }
    }

    fn rate_with_gain(&self, gain_num: u64, gain_den: u64) -> Option<PacingRate> {
        if self.max_bandwidth_bytes_per_second == 0 {
            return None;
        }
        Some(PacingRate::from_bytes_per_second(
            (self.max_bandwidth_bytes_per_second * gain_num / gain_den).max(1),
        ))
    }
}

impl CongestionControl for BbrV3 {
    fn algorithm_name(&self) -> &'static str {
        "bbr-v3"
    }

    fn on_packet_sent(&mut self, _bytes: u32, _bytes_in_flight: u32, _now_nanos: u64) {}

    fn on_ack(&mut self, sample: AckSample) -> RecoveryAction {
        self.update_model(sample);
        self.update_control_parameters(sample);
        RecoveryAction {
            cwnd: self.congestion_window(),
            pacing_rate: self.pacing_rate(),
        }
    }

    fn on_congestion_event(&mut self, event: CongestionEvent) -> RecoveryAction {
        match event {
            CongestionEvent::PacketLoss {
                bytes_in_flight, ..
            }
            | CongestionEvent::ExplicitCongestion {
                bytes_in_flight, ..
            } => {
                self.inflight_hi = cmp::max(bytes_in_flight.saturating_sub(self.mss), self.mss * 4);
                self.inflight_lo = self.inflight_hi;
                self.cwnd = self.cwnd.min(self.inflight_hi);
            }
            CongestionEvent::RetransmissionTimeout { .. } => {
                self.inflight_hi = self.mss * 4;
                self.inflight_lo = self.mss * 4;
                self.cwnd = self.mss * 4;
                self.mode = BbrMode::Startup;
            }
        }
        RecoveryAction {
            cwnd: self.congestion_window(),
            pacing_rate: self.pacing_rate(),
        }
    }

    fn congestion_window(&self) -> CongestionWindow {
        CongestionWindow::new(self.cwnd.max(self.mss * 4))
    }

    fn pacing_rate(&self) -> Option<PacingRate> {
        self.pacing_rate
    }
}

fn cubic_root_scaled(value: u64) -> u64 {
    let mut low = 0u64;
    let mut high = 1u64 << 22;
    while low < high {
        let mid = (low + high).div_ceil(2);
        if mid.saturating_mul(mid).saturating_mul(mid) <= value {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}
