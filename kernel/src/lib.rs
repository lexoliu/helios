#![no_std]
extern crate alloc;
mod log;
pub use helios_hal::Platform;

use linked_list_allocator::LockedHeap;

#[cfg_attr(target_os = "none", global_allocator)]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

#[cfg(not(target_os = "none"))]
compile_error!(
    "This crate is only for the kernel. Please use `helios_hosted` for hosted applications."
);

pub fn init<Console: core::fmt::Write + Send>(platform: Platform<Console>) {
    let Platform { console, heap } = platform;
    unsafe {
        ALLOCATOR.lock().init(heap.as_mut_ptr(), heap.len());
    }
    log::init_logger(console);
    tracing::info!("Kernel initialized");
}
