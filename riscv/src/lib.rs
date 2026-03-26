#![no_std]
#![no_main]

use core::arch::global_asm;
use core::fmt::Write;
use core::ops::Range;
use core::result::Result::Ok;
use core::sync::atomic::{AtomicUsize, Ordering};

use fdt::Fdt;
use helios_hal::cpu::{Cpu, HartId, Instant};
use helios_hal::memory::MemoryRegion;
use riscv_rt::entry;

global_asm!(include_str!("mp_hook.S"));
global_asm!(include_str!("secondary_entry.S"));

const UNINITIALIZED_BOOT_HART: usize = usize::MAX;

static BOOT_HART_ID: AtomicUsize = AtomicUsize::new(UNINITIALIZED_BOOT_HART);

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let _ = writeln!(SbiConsole, "panic: {info}");
    helios_kernel::panic_log(info);
    sbi_rt::system_reset(sbi_rt::Shutdown, sbi_rt::NoReason);
    loop {
        core::hint::spin_loop();
    }
}

pub struct SbiConsole;

impl Write for SbiConsole {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            let ret = sbi_rt::console_write_byte(byte);
            if ret.is_err() {
                return Err(core::fmt::Error);
            }
        }
        Ok(())
    }
}

pub struct RiscvCpu {
    current_hart: HartId,
    bootstrap_hart: HartId,
    hart_count: usize,
    fdt_addr: usize,
}

impl RiscvCpu {
    pub const fn new(
        current_hart: HartId,
        bootstrap_hart: HartId,
        hart_count: usize,
        fdt_addr: usize,
    ) -> Self {
        Self {
            current_hart,
            bootstrap_hart,
            hart_count,
            fdt_addr,
        }
    }
}

impl Cpu for RiscvCpu {
    fn current_hart(&self) -> HartId {
        self.current_hart
    }

    fn hart_count(&self) -> usize {
        self.hart_count
    }

    fn bootstrap_hart(&self) -> HartId {
        self.bootstrap_hart
    }

    fn park_current(&self) {
        riscv::asm::wfi();
    }

    fn start_hart(&self, hart: HartId) {
        let ret = sbi_rt::hart_start(
            hart.id() as usize,
            core::ptr::addr_of!(_secondary_start).addr(),
            self.fdt_addr,
        );
        if ret.is_ok() {
            return;
        }

        panic!(
            "failed to start hart {} via SBI HSM: error={} value={}",
            hart.id(),
            ret.error,
            ret.value
        );
    }

    fn now(&self) -> Instant {
        Instant::new(riscv::register::time::read64())
    }

    fn set_deadline(&self, deadline: Instant) {
        sbi_rt::set_timer(deadline.ticks());
    }

    fn shutdown(&self) -> ! {
        sbi_rt::system_reset(sbi_rt::Shutdown, sbi_rt::NoReason);
        loop {
            core::hint::spin_loop();
        }
    }

    fn reboot(&self) -> ! {
        sbi_rt::system_reset(sbi_rt::ColdReboot, sbi_rt::NoReason);
        loop {
            core::hint::spin_loop();
        }
    }
}

#[entry]
fn main(hart_id: usize, fdt_addr: usize, opaque: usize) -> ! {
    let _ = opaque;
    run_hart(hart_id, fdt_addr)
}

#[unsafe(no_mangle)]
extern "C" fn secondary_start_rust(hart_id: usize, fdt_addr: usize) -> ! {
    run_hart(hart_id, fdt_addr)
}

fn run_hart(hart_id: usize, fdt_addr: usize) -> ! {
    let console = SbiConsole;
    let fdt = unsafe { Fdt::from_ptr(fdt_addr as *const u8) }
        .expect("OpenSBI did not provide a valid FDT");
    let hart_count = fdt.cpus().count();
    let allocator_window = allocator_window();
    let memory = fdt.memory();
    let memory_regions = memory
        .regions()
        .map(|region| {
            let start = region.starting_address as usize;
            let end = start
                .checked_add(region.size.unwrap())
                .expect("FDT memory region overflows usize");

            start..end
        })
        .filter_map(move |region| intersect(region, allocator_window.clone()))
        .map(range_to_memory_region);
    let current_hart = HartId::new(hart_id as u16);
    let bootstrap_hart = remember_bootstrap_hart(hart_id);
    let cpu = RiscvCpu::new(current_hart, bootstrap_hart, hart_count, fdt_addr);

    helios_kernel::init(helios_kernel::Platform::new(console, memory_regions, cpu));

    loop {
        core::hint::spin_loop();
    }
}

unsafe extern "C" {
    static __ebss: u8;
    static __stack_bottom_value: usize;
    static _secondary_start: u8;
}

fn allocator_window() -> Range<usize> {
    let start = align_up(
        core::ptr::addr_of!(__ebss).addr(),
        core::mem::align_of::<usize>(),
    );
    let stack_bottom =
        unsafe { core::ptr::read_volatile(core::ptr::addr_of!(__stack_bottom_value)) };

    assert!(start < stack_bottom, "allocator window is empty");
    start..stack_bottom
}

fn intersect(left: Range<usize>, right: Range<usize>) -> Option<Range<usize>> {
    let start = left.start.max(right.start);
    let end = left.end.min(right.end);

    (start < end).then_some(start..end)
}

fn range_to_memory_region(range: Range<usize>) -> MemoryRegion {
    let len = range.end - range.start;
    let slice = core::ptr::slice_from_raw_parts_mut(range.start as *mut u8, len);

    unsafe { MemoryRegion::new_unchecked(slice) }
}

const fn align_up(value: usize, align: usize) -> usize {
    (value + (align - 1)) & !(align - 1)
}

fn remember_bootstrap_hart(current_hart: usize) -> HartId {
    match BOOT_HART_ID.compare_exchange(
        UNINITIALIZED_BOOT_HART,
        current_hart,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => HartId::new(current_hart as u16),
        Err(bootstrap_hart) => HartId::new(bootstrap_hart as u16),
    }
}
