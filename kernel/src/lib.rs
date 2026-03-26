#![no_std]
extern crate alloc;
mod log;
pub use helios_hal::Platform;

use linked_list_allocator::LockedHeap;

static ALLOCATOR: LockedHeap = LockedHeap::empty();

pub fn init<Console: core::fmt::Write + Send>(platform: Platform<Console>) {
    let Platform { console, heap } = platform;
    unsafe {
        ALLOCATOR.lock().init(heap.as_mut_ptr(), heap.len());
    }
    log::init_logger(console);
    tracing::info!("Kernel initialized");
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
