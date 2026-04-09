use helios_hal::cpu::Cpu;

pub fn monotonic_nanos<CpuImpl: Cpu>(cpu: &CpuImpl) -> u64 {
    let ticks = cpu.now().ticks();
    ticks.saturating_mul(1_000_000_000) / cpu.timer_frequency()
}

pub fn elapsed_millis(since_nanos: u64, now_nanos: u64) -> u64 {
    now_nanos.saturating_sub(since_nanos) / 1_000_000
}
