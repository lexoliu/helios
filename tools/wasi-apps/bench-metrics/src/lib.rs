//! Measurement helpers shared by the benchmark workload programs.
//!
//! Every workload reports its primary result as `<workload>:<value>` and any
//! secondary measurement as a `bench.<name>=<number>` line. The inspector's
//! `workload-bench` and the Linux runner parse those lines into the same
//! metric names, so a workload built for Helios and its Linux counterpart
//! produce comparable reports without either side knowing about the other.

use std::fmt::Display;
use std::time::Duration;

/// Prefix of a secondary-measurement line.
pub const METRIC_LINE_PREFIX: &str = "bench.";

/// Prints one secondary measurement.
pub fn report_metric(name: &str, value: impl Display) {
    println!("{METRIC_LINE_PREFIX}{name}={value}");
}

/// Latency samples collected by one workload.
#[derive(Debug, Default, Clone)]
pub struct LatencySamples {
    nanos: Vec<u64>,
}

impl LatencySamples {
    /// Reserves room for `capacity` samples up front so recording a sample
    /// never allocates inside the timed region.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            nanos: Vec::with_capacity(capacity),
        }
    }

    pub fn record(&mut self, elapsed: Duration) {
        let nanos =
            u64::try_from(elapsed.as_nanos()).expect("latency sample exceeds u64 nanoseconds");
        self.nanos.push(nanos);
    }

    pub fn extend(&mut self, other: &Self) {
        self.nanos.extend_from_slice(&other.nanos);
    }

    pub fn len(&self) -> usize {
        self.nanos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nanos.is_empty()
    }

    /// Reports `<prefix>_p50_us`, `<prefix>_p99_us`, `<prefix>_max_us` and
    /// `<prefix>_mean_us` for the samples collected so far.
    pub fn report(&mut self, prefix: &str) {
        assert!(!self.nanos.is_empty(), "no {prefix} samples were collected");
        self.nanos.sort_unstable();
        report_metric(&format!("{prefix}_p50_us"), micros(self.percentile(50)));
        report_metric(&format!("{prefix}_p99_us"), micros(self.percentile(99)));
        report_metric(&format!("{prefix}_max_us"), micros(self.percentile(100)));
        let total: u128 = self.nanos.iter().map(|&nanos| u128::from(nanos)).sum();
        let mean = total / self.nanos.len() as u128;
        report_metric(
            &format!("{prefix}_mean_us"),
            micros(u64::try_from(mean).expect("mean fits u64")),
        );
    }

    /// Nearest-rank percentile over the sorted samples.
    fn percentile(&self, percent: u32) -> u64 {
        let rank = (self.nanos.len() as u64 * u64::from(percent)).div_ceil(100);
        let index = usize::try_from(rank.max(1) - 1).expect("rank fits usize");
        self.nanos[index]
    }
}

fn micros(nanos: u64) -> String {
    format!("{}.{:03}", nanos / 1_000, nanos % 1_000)
}

/// Bytes per second expressed in MiB/s with three decimals.
pub fn mib_per_second(bytes: u64, elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    assert!(seconds > 0.0, "throughput needs a non-zero elapsed time");
    format!("{:.3}", bytes as f64 / (1024.0 * 1024.0) / seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let mut samples = LatencySamples::default();
        for nanos in [5_000, 1_000, 3_000, 2_000, 4_000] {
            samples.record(Duration::from_nanos(nanos));
        }
        samples.nanos.sort_unstable();
        assert_eq!(samples.percentile(50), 3_000);
        assert_eq!(samples.percentile(99), 5_000);
        assert_eq!(samples.percentile(100), 5_000);
        assert_eq!(samples.percentile(1), 1_000);
    }

    #[test]
    fn micros_keeps_sub_microsecond_digits() {
        assert_eq!(micros(1_234_567), "1234.567");
        assert_eq!(micros(42), "0.042");
    }
}
