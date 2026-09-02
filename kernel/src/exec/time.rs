use core::time::Duration;
use helios_hal::cpu::Cpu;
pub use helios_hal::cpu::ticks_to_nanos;
use helios_hal::rtc::UnixSeconds;

use crate::{ComponentRuntimeState, SetWallClockCap};

pub struct KernelClock<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    cpu: CpuImpl,
    runtime_state: RuntimeStateImpl,
    wall_clock_offset_nanos: i128,
}

impl<CpuImpl, RuntimeStateImpl> KernelClock<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    /// Opens a clock on the wall time the platform's real-time clock
    /// set at boot.
    ///
    /// The offset is taken from the runtime state rather than recomputed
    /// here, so every store the kernel opens agrees on what the calendar
    /// reads. A machine whose kernel found no real-time clock carries a
    /// zero offset, and wall time there is uptime.
    pub fn new(cpu: CpuImpl, runtime_state: RuntimeStateImpl) -> Self {
        let wall_clock_offset_nanos = runtime_state.wall_clock_offset_nanos();
        Self {
            cpu,
            runtime_state,
            wall_clock_offset_nanos,
        }
    }

    pub fn monotonic_nanos(&self) -> u64 {
        self.runtime_state.uptime_nanos(self.cpu.now().ticks())
    }

    pub fn system_time_nanos(&self) -> u64 {
        let value = i128::from(self.monotonic_nanos()) + self.wall_clock_offset_nanos;
        if value <= 0 {
            0
        } else {
            u64::try_from(value).unwrap_or(u64::MAX)
        }
    }

    pub fn set_system_time_nanos(&mut self, _: &SetWallClockCap, nanos: u64) {
        self.wall_clock_offset_nanos = i128::from(nanos) - i128::from(self.monotonic_nanos());
    }
}

/// The offset that carries the monotonic clock onto the wall.
///
/// Adding it to a monotonic reading yields nanoseconds since the Unix
/// epoch. It is signed because the kernel's monotonic clock starts at
/// zero on a machine whose real-time clock was never set, which puts
/// wall time behind uptime.
pub fn wall_clock_offset_nanos(wall: UnixSeconds, monotonic_nanos: u64) -> i128 {
    let wall_nanos = i128::try_from(wall.as_nanos()).unwrap_or_else(|_| {
        panic!(
            "real-time clock reported {} seconds, which no calendar can express in nanoseconds",
            wall.get()
        )
    });
    wall_nanos - i128::from(monotonic_nanos)
}

pub fn monotonic_nanos<CpuImpl: Cpu>(cpu: &CpuImpl) -> u64 {
    ticks_to_nanos(cpu.now().ticks(), cpu.timer_frequency())
}

pub fn elapsed_millis(since_nanos: u64, now_nanos: u64) -> u64 {
    now_nanos.saturating_sub(since_nanos) / 1_000_000
}

/// Guests legitimately pass `u64::MAX` nanoseconds as an "infinite"
/// timeout (WASIX futex waits, epoll). On timebases at or above 1 GHz
/// that product exceeds `u64` ticks; a deadline centuries out is
/// semantically "never", so the conversion saturates instead of
/// treating guest input as a kernel invariant.
pub fn duration_to_ticks(duration: Duration, frequency: u64) -> u64 {
    let seconds = (duration.as_secs() as u128) * (frequency as u128);
    let subsec = ((duration.subsec_nanos() as u128) * (frequency as u128)) / 1_000_000_000u128;
    u64::try_from(seconds.saturating_add(subsec)).unwrap_or(u64::MAX)
}

pub fn nanos_to_ticks_ceil_saturating(nanos: u64, frequency: u64) -> u64 {
    let numerator = (nanos as u128) * (frequency as u128);
    let ticks = numerator.saturating_add(999_999_999) / 1_000_000_000u128;
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use core::sync::atomic::{AtomicU64, Ordering};

    use helios_hal::cpu::{Instant, ProcessorId};

    use super::*;
    use crate::{ClockAuthorityRights, ProfileScope};

    #[derive(Clone)]
    struct TestCpu {
        ticks: &'static AtomicU64,
    }

    impl TestCpu {
        fn set_ticks(&self, ticks: u64) {
            self.ticks.store(ticks, Ordering::Release);
        }
    }

    impl Cpu for TestCpu {
        fn current_processor(&self) -> ProcessorId {
            ProcessorId::new(0)
        }

        fn processor_count(&self) -> usize {
            1
        }

        fn bootstrap_processor(&self) -> ProcessorId {
            ProcessorId::new(0)
        }

        fn park_current(&self) {}

        fn start_processor(&self, _: ProcessorId) {}

        fn wake_processor(&self, _: ProcessorId) {}

        fn now(&self) -> Instant {
            Instant::new(self.ticks.load(Ordering::Acquire))
        }

        fn timer_frequency(&self) -> u64 {
            1_000_000_000
        }

        fn set_deadline(&self, _: Instant) {}

        fn publish_executable(&self, _: *const u8, _: usize) {}

        fn unpublish_executable(&self, _: *const u8, _: usize) {}

        fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
            None
        }

        fn shutdown(&self) -> ! {
            panic!("test CPU should not shut down")
        }

        fn reboot(&self) -> ! {
            panic!("test CPU should not reboot")
        }
    }

    /// A runtime state whose wall clock is already seeded, the way a
    /// backend leaves it once it has read the platform's real-time
    /// clock.
    #[derive(Clone)]
    struct TestRuntimeState {
        wall_clock_offset_nanos: i128,
    }

    impl TestRuntimeState {
        const fn unseeded() -> Self {
            Self {
                wall_clock_offset_nanos: 0,
            }
        }

        const fn seeded(wall_clock_offset_nanos: i128) -> Self {
            Self {
                wall_clock_offset_nanos,
            }
        }
    }

    impl ComponentRuntimeState for TestRuntimeState {
        fn uptime_nanos(&self, current_ticks: u64) -> u64 {
            current_ticks
        }

        fn wall_clock_offset_nanos(&self) -> i128 {
            self.wall_clock_offset_nanos
        }

        fn record_console_text(&self, _: u64, _: &str) {}

        fn root_entropy(&self) -> &crate::RootEntropy {
            panic!("the clock test state has no root entropy")
        }

        fn memory_balloon(&self) -> Option<crate::memory::BalloonHandle> {
            None
        }

        fn profiling_enabled(&self) -> bool {
            false
        }

        fn record_profile_stack_nanos(&self, _: ProfileScope, _: String, _: u64) {}

        fn record_profile_stack_parts_nanos(&self, _: ProfileScope, _: &str, _: &str, _: u64) {}

        fn record_perf_metric_parts_nanos(
            &self,
            _: ProfileScope,
            _: &str,
            _: &str,
            _: u64,
            _: helios_hal::cpu::HardwarePerfCounterDelta,
            _: u64,
        ) {
        }

        fn record_perf_metric_parts_events_nanos(
            &self,
            _: ProfileScope,
            _: &str,
            _: &str,
            _: u64,
            _: u64,
            _: helios_hal::cpu::HardwarePerfCounterDelta,
            _: u64,
        ) {
        }
    }

    #[test]
    fn kernel_clock_reads_monotonic_time() {
        static TICKS: AtomicU64 = AtomicU64::new(5);
        TICKS.store(5, Ordering::Release);
        let cpu = TestCpu { ticks: &TICKS };
        let clock = KernelClock::new(cpu.clone(), TestRuntimeState::unseeded());
        assert_eq!(clock.monotonic_nanos(), 5);
        cpu.set_ticks(9);
        assert_eq!(clock.monotonic_nanos(), 9);
    }

    #[test]
    fn system_clock_mutation_requires_typed_capability() {
        static TICKS: AtomicU64 = AtomicU64::new(5);
        TICKS.store(5, Ordering::Release);
        let cpu = TestCpu { ticks: &TICKS };
        let mut clock = KernelClock::new(cpu.clone(), TestRuntimeState::unseeded());
        let mut authority = crate::ProcessAuthority::empty();
        authority.grant_clock_rights(ClockAuthorityRights::SET_WALL_CLOCK);
        let cap = authority
            .derive_set_wall_clock_cap()
            .expect("root clock authority should derive set-wall-clock cap");
        clock.set_system_time_nanos(&cap, 1_000);
        assert_eq!(clock.system_time_nanos(), 1_000);
        cpu.set_ticks(9);
        assert_eq!(clock.system_time_nanos(), 1_004);
    }

    #[test]
    fn a_seeded_offset_puts_the_monotonic_clock_on_the_calendar() {
        // The offset is what the backend computes at boot: the
        // real-time clock's reading minus how long the kernel had
        // already been up when it read it.
        const BOOT_MONOTONIC_NANOS: u64 = 2_000_000_000;
        let wall = UnixSeconds::new(1_788_266_096);
        let offset = wall_clock_offset_nanos(wall, BOOT_MONOTONIC_NANOS);
        assert_eq!(offset, 1_788_266_094_000_000_000);

        static TICKS: AtomicU64 = AtomicU64::new(BOOT_MONOTONIC_NANOS);
        TICKS.store(BOOT_MONOTONIC_NANOS, Ordering::Release);
        let cpu = TestCpu { ticks: &TICKS };
        let clock = KernelClock::new(cpu.clone(), TestRuntimeState::seeded(offset));
        assert_eq!(clock.system_time_nanos(), wall.as_nanos() as u64);

        // Wall time then advances exactly as the monotonic clock does.
        cpu.set_ticks(BOOT_MONOTONIC_NANOS + 5_000_000_000);
        assert_eq!(
            clock.system_time_nanos(),
            wall.as_nanos() as u64 + 5_000_000_000
        );
    }

    #[test]
    fn a_machine_without_a_real_time_clock_reads_wall_time_as_uptime() {
        static TICKS: AtomicU64 = AtomicU64::new(7);
        TICKS.store(7, Ordering::Release);
        let cpu = TestCpu { ticks: &TICKS };
        let clock = KernelClock::new(cpu, TestRuntimeState::unseeded());
        assert_eq!(wall_clock_offset_nanos(UnixSeconds::new(0), 0), 0);
        assert_eq!(clock.system_time_nanos(), 7);
    }

    #[test]
    fn a_clock_read_before_the_kernel_booted_yields_a_negative_offset() {
        // A machine whose real-time clock was never set reports a time
        // behind the kernel's own uptime. The offset has to stay signed
        // for that, and wall time then floors at the epoch rather than
        // wrapping.
        let offset = wall_clock_offset_nanos(UnixSeconds::new(0), 3_000_000_000);
        assert_eq!(offset, -3_000_000_000);

        static TICKS: AtomicU64 = AtomicU64::new(1_000_000_000);
        TICKS.store(1_000_000_000, Ordering::Release);
        let cpu = TestCpu { ticks: &TICKS };
        let clock = KernelClock::new(cpu.clone(), TestRuntimeState::seeded(offset));
        assert_eq!(clock.system_time_nanos(), 0);
        cpu.set_ticks(5_000_000_000);
        assert_eq!(clock.system_time_nanos(), 2_000_000_000);
    }

    #[test]
    fn infinite_guest_timeout_saturates_instead_of_panicking() {
        // WASIX guests pass u64::MAX nanoseconds as an "infinite"
        // futex/epoll timeout. On a 1 GHz timebase the tick product
        // exceeds u64; the conversion must saturate to a
        // never-expiring deadline rather than panic the kernel on
        // guest-controlled input. Below 1 GHz the product still fits
        // and must stay exact.
        const GIGAHERTZ: u64 = 1_000_000_000;
        let infinite = Duration::from_nanos(u64::MAX);
        assert_eq!(duration_to_ticks(infinite, GIGAHERTZ), u64::MAX);
        assert_eq!(duration_to_ticks(infinite, 3 * GIGAHERTZ), u64::MAX);
        let arm_frequency = 62_500_000;
        assert_eq!(
            duration_to_ticks(infinite, arm_frequency),
            ((u64::MAX as u128 / 1_000_000_000) * arm_frequency as u128) as u64
                + (u64::MAX as u128 % 1_000_000_000 * arm_frequency as u128 / 1_000_000_000) as u64
        );
        assert_eq!(
            duration_to_ticks(Duration::from_secs(1), GIGAHERTZ),
            GIGAHERTZ
        );
    }

    #[test]
    fn tick_conversion_keeps_advancing_past_the_u64_nanosecond_product() {
        // A 1 GHz TSC timebase crosses `u64::MAX / 1e9` ticks after
        // roughly 18.4 s of uptime. The naive `ticks * 1e9` product
        // saturates there and pins every derived deadline to a
        // constant, so the conversion has to stay monotonic well past
        // that point.
        const GIGAHERTZ: u64 = 1_000_000_000;
        let cliff_ticks = u64::MAX / GIGAHERTZ;

        assert_eq!(ticks_to_nanos(cliff_ticks, GIGAHERTZ), cliff_ticks);
        assert_eq!(
            ticks_to_nanos(cliff_ticks + 1, GIGAHERTZ),
            cliff_ticks + 1,
            "conversion must not saturate one tick past the u64 product limit"
        );

        let hour_ticks = 3_600 * GIGAHERTZ;
        assert_eq!(ticks_to_nanos(hour_ticks, GIGAHERTZ), hour_ticks);
        assert!(
            ticks_to_nanos(hour_ticks + GIGAHERTZ, GIGAHERTZ)
                > ticks_to_nanos(hour_ticks, GIGAHERTZ),
            "conversion must stay strictly monotonic across long uptimes"
        );

        // A slower timebase reaches the same product limit later, which
        // is why only the TSC-backed backend hit this in practice.
        assert_eq!(ticks_to_nanos(cliff_ticks, 62_500_000), 295_147_905_168);
    }
}
