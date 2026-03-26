#![no_std]
extern crate alloc;
mod log;
mod program;
pub use helios_hal::Platform;

use buddy_system_allocator::LockedHeap;
use helios_hal::cpu::Cpu;
use helios_hal::memory::MemoryRegion;

const HEAP_ORDER: usize = 32;

#[cfg_attr(target_os = "none", global_allocator)]
static ALLOCATOR: LockedHeap<HEAP_ORDER> = LockedHeap::empty();

pub fn init<Console, CpuImpl, Regions>(platform: Platform<Console, CpuImpl, Regions>)
where
    Console: core::fmt::Write + Send,
    CpuImpl: Cpu,
    Regions: IntoIterator<Item = MemoryRegion>,
{
    let Platform {
        console,
        memory_regions,
        ..
    } = platform;
    init_allocator(memory_regions);
    log::init_logger(console);
    tracing::info!("Kernel initialized\n {}", include_str!("welcome.txt"));
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
