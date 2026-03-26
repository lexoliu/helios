#![no_std]
#![no_main]

use core::fmt::Write;
use core::ops::Range;
use core::result::Result::Ok;

use fdt::Fdt;
use helios_hal::cpu::{Cpu, HartId, Instant};
use helios_hal::memory::MemoryRegion;
use riscv_rt::entry;

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
    bootstrap_hart: HartId,
    hart_count: usize,
}

impl RiscvCpu {
    pub const fn new(bootstrap_hart: HartId, hart_count: usize) -> Self {
        Self {
            bootstrap_hart,
            hart_count,
        }
    }
}

impl Cpu for RiscvCpu {
    fn current_hart(&self) -> HartId {
        HartId::new(riscv::register::mhartid::read() as u16)
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

    fn unpark(&self, hart: HartId) {
        let _ = sbi_rt::send_ipi(sbi_rt::HartMask::from_mask_base(1, hart.id() as usize));
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

#[unsafe(export_name = "_mp_hook")]
#[unsafe(link_section = ".text.mp_hook")]
pub extern "Rust" fn mp_hook(_hartid: usize) -> bool {
    true
}

#[entry]
fn main(hart_id: usize, fdt_addr: usize, opaque: usize) -> ! {
    let _ = opaque;

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
    let cpu = RiscvCpu::new(HartId::new(hart_id as u16), hart_count);

    helios_kernel::init(helios_kernel::Platform::new(console, memory_regions, cpu));

    loop {
        core::hint::spin_loop();
    }
}

unsafe extern "C" {
    static __ebss: u8;
    static __stack_bottom_value: usize;
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
