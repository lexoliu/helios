#![no_std]
#![no_main]

extern crate alloc;

use bootloader_api::config::Mapping;
use bootloader_api::info::MemoryRegionKind;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicPtr, Ordering};
use helios_hal::Platform;
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::memory::MemoryRegion;
use spin::Mutex;
use x86_64::instructions::hlt;
use x86_64::instructions::port::Port;

static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config
};

entry_point!(x86_kernel_main, config = &BOOTLOADER_CONFIG);

fn x86_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    let memory_regions = boot_memory_regions(boot_info);
    let console = SerialConsole::new();
    let cpu = X86Cpu;
    let kernel = helios_kernel::init(Platform::new(console, memory_regions, cpu));
    kernel.run();
}

fn boot_memory_regions(boot_info: &'static BootInfo) -> impl IntoIterator<Item = MemoryRegion> {
    let physical_memory_offset = match boot_info.physical_memory_offset.into_option() {
        Some(offset) => offset,
        None => panic!("bootloader did not provide a physical memory mapping"),
    };

    boot_info
        .memory_regions
        .iter()
        .filter(|region| region.kind == MemoryRegionKind::Usable && region.end > region.start)
        .map(move |region| {
            let start = region.start as usize + physical_memory_offset as usize;
            let len = (region.end - region.start) as usize;
            let slice = core::ptr::slice_from_raw_parts_mut(start as *mut u8, len);
            core::ptr::NonNull::new(slice)
                .unwrap_or_else(|| panic!("usable memory region had a null start: {region:?}"))
        })
}

struct SerialConsole {
    port: Mutex<Port<u8>>,
}

impl SerialConsole {
    fn new() -> Self {
        let mut port = Port::new(0x3f8);
        unsafe {
            port.write(0x00u8);
        }
        Self {
            port: Mutex::new(port),
        }
    }
}

impl Write for SerialConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut port = self.port.lock();
        for byte in s.bytes() {
            unsafe {
                port.write(byte);
            }
        }
        Ok(())
    }
}

static WASMTIME_TLS: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());

#[derive(Clone, Copy)]
struct X86Cpu;

impl Cpu for X86Cpu {
    fn current_processor(&self) -> ProcessorId {
        ProcessorId::new(0)
    }

    fn processor_count(&self) -> usize {
        1
    }

    fn bootstrap_processor(&self) -> ProcessorId {
        ProcessorId::new(0)
    }

    fn park_current(&self) {
        hlt();
    }

    fn start_processor(&self, processor: ProcessorId) {
        panic!(
            "x86 SMP bring-up is not implemented yet for processor {}",
            processor.id()
        );
    }

    fn wake_processor(&self, _processor: ProcessorId) {}

    fn now(&self) -> Instant {
        Instant::new(0)
    }

    fn timer_frequency(&self) -> u64 {
        1_000_000_000
    }

    fn set_deadline(&self, _deadline: Instant) {}

    fn shutdown(&self) -> ! {
        loop {
            hlt();
        }
    }

    fn reboot(&self) -> ! {
        panic!("x86 reboot is not implemented yet");
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        hlt();
    }
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_get() -> *mut u8 {
    WASMTIME_TLS.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(ptr: *mut u8) {
    WASMTIME_TLS.store(ptr, Ordering::Release);
}
