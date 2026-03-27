#![no_std]
#![no_main]

pub mod compute;

use core::arch::{asm, global_asm};
use core::fmt::Write;
use core::ops::Range;
use core::sync::atomic::{AtomicUsize, Ordering, compiler_fence};

use fdt::Fdt;
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::memory::MemoryRegion;
use helios_kernel::Timer;
use riscv::interrupt::Trap;
use riscv::interrupt::supervisor::{Exception, Interrupt};
use riscv_rt::entry;
use trapframe::TrapFrame;

global_asm!(include_str!("mp_hook.S"));
global_asm!(include_str!("secondary_entry.S"));

const UNINITIALIZED_BOOT_HART: usize = usize::MAX;
const SSTATUS_SPP_BIT: usize = 1 << 8;

static BOOT_HART_ID: AtomicUsize = AtomicUsize::new(UNINITIALIZED_BOOT_HART);

struct SupervisorCriticalSection;

critical_section::set_impl!(SupervisorCriticalSection);

unsafe impl critical_section::Impl for SupervisorCriticalSection {
    unsafe fn acquire() -> bool {
        let interrupts_were_enabled = riscv::register::sstatus::read().sie();
        riscv::interrupt::supervisor::disable();
        compiler_fence(Ordering::SeqCst);
        interrupts_were_enabled
    }

    unsafe fn release(interrupts_were_enabled: bool) {
        compiler_fence(Ordering::SeqCst);
        if interrupts_were_enabled {
            unsafe {
                riscv::interrupt::supervisor::enable();
            }
        }
    }
}

struct HartRuntime {
    hart_id: ProcessorId,
    timer: Timer<RiscvCpu>,
}

impl HartRuntime {
    fn install(&self) {
        // `trapframe` owns `sscratch`, so hart-local runtime state lives in `tp`.
        // This keeps trap entry correct while still giving the kernel a cheap
        // per-hart lookup path.
        write_hart_runtime(self as *const Self);
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    fatal_panic(info)
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

#[derive(Clone)]
pub struct RiscvCpu {
    current_hart: ProcessorId,
    bootstrap_hart: ProcessorId,
    hart_count: usize,
    timebase_frequency: u64,
    fdt_addr: usize,
}

impl RiscvCpu {
    pub const fn new(
        current_hart: ProcessorId,
        bootstrap_hart: ProcessorId,
        hart_count: usize,
        timebase_frequency: u64,
        fdt_addr: usize,
    ) -> Self {
        Self {
            current_hart,
            bootstrap_hart,
            hart_count,
            timebase_frequency,
            fdt_addr,
        }
    }
}

impl Cpu for RiscvCpu {
    fn current_processor(&self) -> ProcessorId {
        let runtime_ptr = read_hart_runtime();
        if runtime_ptr == 0 {
            return self.current_hart;
        }

        unsafe { (*(runtime_ptr as *const HartRuntime)).hart_id }
    }

    fn processor_count(&self) -> usize {
        self.hart_count
    }

    fn bootstrap_processor(&self) -> ProcessorId {
        self.bootstrap_hart
    }

    fn park_current(&self) {
        riscv::asm::wfi();
    }

    fn start_processor(&self, hart: ProcessorId) {
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

    fn wake_processor(&self, hart: ProcessorId) {
        if self.current_processor() == hart {
            return;
        }

        let ret = sbi_rt::send_ipi(sbi_rt::HartMask::from_mask_base(1, hart.id() as usize));
        if ret.is_ok() {
            return;
        }

        panic!(
            "failed to wake hart {} via SBI IPI: error={} value={}",
            hart.id(),
            ret.error,
            ret.value
        );
    }

    fn now(&self) -> Instant {
        Instant::new(riscv::register::time::read64())
    }

    fn timer_frequency(&self) -> u64 {
        self.timebase_frequency
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

#[unsafe(no_mangle)]
extern "C" fn trap_handler(tf: &mut TrapFrame) {
    match riscv::register::scause::read().cause().try_into() {
        Ok(Trap::Interrupt(Interrupt::SupervisorTimer)) => {
            current_hart_runtime().timer.handle_interrupt();
        }
        Ok(Trap::Interrupt(Interrupt::SupervisorSoft)) => unsafe {
            riscv::register::sip::clear_ssoft();
        },
        Ok(Trap::Interrupt(Interrupt::SupervisorExternal)) => {
            panic!(
                "unhandled hardware interrupt: interrupt={:?}, tf={tf:#x?}",
                Interrupt::SupervisorExternal
            );
        }
        Ok(Trap::Exception(exception)) => {
            handle_exception(exception, tf);
        }
        Err(err) => {
            panic!("invalid supervisor trap cause: {err:?}, tf={tf:#x?}");
        }
    }
}

fn run_hart(hart_id: usize, fdt_addr: usize) -> ! {
    clear_hart_runtime();
    let console = SbiConsole;
    let fdt = unsafe { Fdt::from_ptr(fdt_addr as *const u8) }
        .expect("OpenSBI did not provide a valid FDT");
    let mut cpus = fdt.cpus();
    let first_cpu = cpus.next().expect("FDT does not describe any CPU");
    let hart_count = 1 + cpus.count();
    let timebase_frequency = first_cpu.timebase_frequency() as u64;
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
    let current_hart = ProcessorId::new(hart_id as u16);
    let bootstrap_processor = remember_bootstrap_hart(hart_id);
    let cpu = RiscvCpu::new(
        current_hart,
        bootstrap_processor,
        hart_count,
        timebase_frequency,
        fdt_addr,
    );

    let kernel = helios_kernel::init(helios_kernel::Platform::new(console, memory_regions, cpu));
    let hart_runtime = HartRuntime {
        hart_id: current_hart,
        timer: kernel.timer(),
    };
    hart_runtime.install();
    unsafe {
        configure_interrupts();
    }
    kernel.run();
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

fn remember_bootstrap_hart(current_hart: usize) -> ProcessorId {
    match BOOT_HART_ID.compare_exchange(
        UNINITIALIZED_BOOT_HART,
        current_hart,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => ProcessorId::new(current_hart as u16),
        Err(bootstrap_hart) => ProcessorId::new(bootstrap_hart as u16),
    }
}

fn current_hart_runtime() -> &'static HartRuntime {
    let ptr = read_hart_runtime();
    assert!(ptr != 0, "hart runtime is not installed");

    unsafe { &*(ptr as *const HartRuntime) }
}

unsafe fn configure_interrupts() {
    unsafe {
        trapframe::init();
        riscv::register::sie::set_ssoft();
        riscv::register::sie::set_stimer();
        riscv::register::sie::set_sext();
        riscv::register::sstatus::set_sie();
    }
}

fn handle_exception(exception: Exception, tf: &TrapFrame) -> ! {
    let stval = riscv::register::stval::read();
    match trap_origin(tf) {
        TrapOrigin::Kernel => panic!(
            "kernel exception: {exception:?}, sepc={:#x}, stval={:#x}, tf={tf:#x?}",
            tf.sepc, stval,
        ),
        TrapOrigin::User => handle_user_exception(exception, stval, tf),
    }
}

fn handle_user_exception(exception: Exception, stval: usize, tf: &TrapFrame) -> ! {
    panic!(
        "unhandled user trap: exception={exception:?}, sepc={:#x}, stval={:#x}, tf={tf:#x?}",
        tf.sepc, stval,
    )
}

fn fatal_panic(info: &core::panic::PanicInfo<'_>) -> ! {
    mask_interrupts();

    let mut console = SbiConsole;
    let _ = console.write_str("Kernel panic");
    if let Some(location) = info.location() {
        let _ = write!(
            console,
            " at {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
    }
    let _ = console.write_str("\n");
    let _ = writeln!(console, "{}", info.message());

    shutdown_machine()
}

fn mask_interrupts() {
    unsafe {
        riscv::interrupt::supervisor::disable();
        riscv::register::sie::clear_ssoft();
        riscv::register::sie::clear_stimer();
        riscv::register::sie::clear_sext();
    }
    compiler_fence(Ordering::SeqCst);
}

fn shutdown_machine() -> ! {
    sbi_rt::system_reset(sbi_rt::Shutdown, sbi_rt::NoReason);
    loop {
        core::hint::spin_loop();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrapOrigin {
    Kernel,
    User,
}

fn trap_origin(tf: &TrapFrame) -> TrapOrigin {
    if tf.sstatus & SSTATUS_SPP_BIT != 0 {
        TrapOrigin::Kernel
    } else {
        TrapOrigin::User
    }
}

fn current_hart_runtime_ptr() -> usize {
    let runtime_ptr: usize;
    unsafe {
        asm!("mv {}, tp", out(reg) runtime_ptr, options(nomem, nostack, preserves_flags));
    }
    runtime_ptr
}

fn read_hart_runtime() -> usize {
    current_hart_runtime_ptr()
}

fn write_hart_runtime(runtime: *const HartRuntime) {
    unsafe {
        asm!("mv tp, {}", in(reg) runtime, options(nomem, nostack, preserves_flags));
    }
}

fn clear_hart_runtime() {
    write_hart_runtime(core::ptr::null());
}
