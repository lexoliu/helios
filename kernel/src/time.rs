use core::time::Duration;
use helios_hal::cpu::Cpu;

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
    pub fn new(cpu: CpuImpl, runtime_state: RuntimeStateImpl) -> Self {
        Self {
            cpu,
            runtime_state,
            wall_clock_offset_nanos: 0,
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

pub fn monotonic_nanos<CpuImpl: Cpu>(cpu: &CpuImpl) -> u64 {
    let ticks = cpu.now().ticks();
    ticks.saturating_mul(1_000_000_000) / cpu.timer_frequency()
}

pub fn elapsed_millis(since_nanos: u64, now_nanos: u64) -> u64 {
    now_nanos.saturating_sub(since_nanos) / 1_000_000
}

pub fn duration_to_ticks(duration: Duration, frequency: u64) -> u64 {
    let seconds = (duration.as_secs() as u128) * (frequency as u128);
    let subsec = ((duration.subsec_nanos() as u128) * (frequency as u128)) / 1_000_000_000u128;
    let ticks = seconds
        .checked_add(subsec)
        .expect("timer duration overflows tick conversion");

    u64::try_from(ticks).expect("timer duration does not fit into u64 ticks")
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

    #[derive(Clone)]
    struct TestRuntimeState;

    impl ComponentRuntimeState for TestRuntimeState {
        fn uptime_nanos(&self, current_ticks: u64) -> u64 {
            current_ticks
        }

        fn record_console_text(&self, _: u64, _: &str) {}

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
    }

    #[test]
    fn kernel_clock_reads_monotonic_time() {
        static TICKS: AtomicU64 = AtomicU64::new(5);
        TICKS.store(5, Ordering::Release);
        let cpu = TestCpu { ticks: &TICKS };
        let clock = KernelClock::new(cpu.clone(), TestRuntimeState);
        assert_eq!(clock.monotonic_nanos(), 5);
        cpu.set_ticks(9);
        assert_eq!(clock.monotonic_nanos(), 9);
    }

    #[test]
    fn system_clock_mutation_requires_typed_capability() {
        static TICKS: AtomicU64 = AtomicU64::new(5);
        TICKS.store(5, Ordering::Release);
        let cpu = TestCpu { ticks: &TICKS };
        let mut clock = KernelClock::new(cpu.clone(), TestRuntimeState);
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
}
