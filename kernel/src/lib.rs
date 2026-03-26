#![no_std]
extern crate alloc;
mod log;
mod program;

pub use helios_hal::Platform;

use core::sync::atomic::{AtomicU8, Ordering};

use buddy_system_allocator::LockedHeap;
use helios_hal::cpu::{Cpu, HartId};
use helios_hal::memory::MemoryRegion;

const HEAP_ORDER: usize = 32;
const BOOT_UNINITIALIZED: u8 = 0;
const BOOT_INITIALIZING: u8 = 1;
const BOOT_READY: u8 = 2;

#[cfg_attr(target_os = "none", global_allocator)]
static ALLOCATOR: LockedHeap<HEAP_ORDER> = LockedHeap::empty();
static BOOT_STATE: AtomicU8 = AtomicU8::new(BOOT_UNINITIALIZED);

pub fn init<Console, CpuImpl, Regions>(platform: Platform<Console, CpuImpl, Regions>)
where
    Console: core::fmt::Write + Send + 'static,
    CpuImpl: Cpu,
    Regions: IntoIterator<Item = MemoryRegion>,
{
    let Platform {
        console,
        cpu,
        memory_regions,
    } = platform;
    let current_hart = cpu.current_hart();

    if current_hart == cpu.bootstrap_hart() {
        bootstrap_init(console, memory_regions, &cpu);
    } else {
        wait_for_bootstrap(&cpu);
    }

    tracing::info!("Hart online hart={}", current_hart.id());
}

fn init_allocator<Regions>(memory_regions: Regions)
where
    Regions: IntoIterator<Item = MemoryRegion>,
{
    for mut region in memory_regions {
        let region = unsafe { region.as_mut() };
        let start = region.as_mut_ptr() as usize;
        let end = start + region.len();
        unsafe {
            ALLOCATOR.lock().add_to_heap(start, end);
        }
    }
}

fn bootstrap_init<Console, CpuImpl, Regions>(
    console: Console,
    memory_regions: Regions,
    cpu: &CpuImpl,
) where
    Console: core::fmt::Write + Send + 'static,
    CpuImpl: Cpu,
    Regions: IntoIterator<Item = MemoryRegion>,
{
    match BOOT_STATE.compare_exchange(
        BOOT_UNINITIALIZED,
        BOOT_INITIALIZING,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(state) => panic!("bootstrap hart observed invalid boot state {state}"),
    }

    init_allocator(memory_regions);
    log::init_logger(console);
    tracing::info!(
        "Kernel initialized on bootstrap hart={}",
        cpu.bootstrap_hart().id()
    );
    tracing::info!("{}", include_str!("welcome.txt"));

    BOOT_STATE.store(BOOT_READY, Ordering::Release);

    for hart in 0..cpu.hart_count() {
        let hart = HartId::new(hart as u16);
        if hart != cpu.bootstrap_hart() {
            cpu.start_hart(hart);
        }
    }
}

fn wait_for_bootstrap<CpuImpl: Cpu>(cpu: &CpuImpl) {
    loop {
        if BOOT_STATE.load(Ordering::Acquire) == BOOT_READY {
            return;
        }
        cpu.park_current();
    }
}

pub fn panic_log(info: &core::panic::PanicInfo) {
    panic_log_message(info.message(), info.location());
}

pub fn panic_log_message(
    message: impl core::fmt::Display,
    location: Option<&core::panic::Location<'_>>,
) {
    if let Some(location) = location {
        tracing::error!(
            "Kernel panic: {} ({}:{}:{})",
            message,
            location.file(),
            location.line(),
            location.column()
        );
        return;
    }

    tracing::error!("Kernel panic: {}", message);
}
