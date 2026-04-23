use helios_hal::cpu::Cpu;
use core::time::Duration;

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
