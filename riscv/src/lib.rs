#![no_std]
#![no_main]

extern crate alloc;

pub mod compute;
mod debug_transport;
mod debugger_program;
mod debugger_wasi;
mod debugger_wasi_p2;
mod host_fs;
mod net;
mod virtio_mmio;

mod debug_state {
    pub(crate) type RuntimeState = helios_kernel::RuntimeState<
        crate::debugger_program::UserProgramService,
        crate::net::NetworkService,
        crate::host_fs::HostFileSystemService,
    >;
}

mod debugger_bindings {
    pub(crate) mod bindings {
        wasmtime::component::bindgen!({
            path: "../wit",
            world: "debugger",
            imports: { default: async | store | trappable },
            exports: { default: async },
            with: {
                "helios:system/net.tcp-stream": crate::debugger_program::SbiTcpStream,
                "helios:system/serial.serial-port": crate::debugger_program::SbiSerialPort,
                "helios:system/sync.raw-mutex": crate::debugger_program::SbiRawMutex,
                "helios:system/sync.raw-mutex-guard": crate::debugger_program::SbiRawMutexGuard,
                "helios:system/sync.raw-rw-lock": crate::debugger_program::SbiRawRwLock,
                "helios:system/sync.raw-rw-lock-read-guard":
                    crate::debugger_program::SbiRawRwLockReadGuard,
                "helios:system/sync.raw-rw-lock-write-guard":
                    crate::debugger_program::SbiRawRwLockWriteGuard,
            },
            require_store_data_send: true,
        });
    }
}

mod program_bindings {
    pub(crate) mod bindings {
        wasmtime::component::bindgen!({
            path: "../wit",
            world: "init",
            imports: { default: async | store | trappable },
            exports: { default: async },
            with: {
                "helios:system/net.tcp-stream": crate::debugger_program::SbiTcpStream,
                "helios:system/serial.serial-port": crate::debugger_program::SbiSerialPort,
                "helios:system/sync.raw-mutex": crate::debugger_program::SbiRawMutex,
                "helios:system/sync.raw-mutex-guard": crate::debugger_program::SbiRawMutexGuard,
                "helios:system/sync.raw-rw-lock": crate::debugger_program::SbiRawRwLock,
                "helios:system/sync.raw-rw-lock-read-guard":
                    crate::debugger_program::SbiRawRwLockReadGuard,
                "helios:system/sync.raw-rw-lock-write-guard":
                    crate::debugger_program::SbiRawRwLockWriteGuard,
            },
            require_store_data_send: true,
        });
    }
}

use core::arch::{asm, global_asm};
use core::cell::Cell;
use core::fmt::Write;
use core::ops::Range;
use core::sync::atomic::{AtomicUsize, Ordering, compiler_fence};

use arrayvec::ArrayVec;
use debug_transport::DebugTransport;
use fdt::Fdt;
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::memory::MemoryRegion;
use helios_kernel::Timer;
use riscv::interrupt::Trap;
use riscv::interrupt::supervisor::{Exception, Interrupt};
use riscv_rt::entry;
use spin::Once;
use trapframe::TrapFrame;

global_asm!(include_str!("mp_hook.S"));
global_asm!(include_str!("secondary_entry.S"));

const UNINITIALIZED_BOOT_HART: usize = usize::MAX;
const SSTATUS_SPP_BIT: usize = 1 << 8;

static BOOT_HART_ID: AtomicUsize = AtomicUsize::new(UNINITIALIZED_BOOT_HART);
static CRITICAL_SECTION_OWNER: AtomicUsize = AtomicUsize::new(0);
static CRITICAL_SECTION_DEPTH: AtomicUsize = AtomicUsize::new(0);
static DEBUG_STATE: Once<debug_state::RuntimeState> = Once::new();

struct SupervisorCriticalSection;

critical_section::set_impl!(SupervisorCriticalSection);

unsafe impl critical_section::Impl for SupervisorCriticalSection {
    unsafe fn acquire() -> usize {
        let interrupts_were_enabled = riscv::register::sstatus::read().sie();
        riscv::interrupt::supervisor::disable();
        compiler_fence(Ordering::SeqCst);

        let owner = critical_section_owner();
        loop {
            match CRITICAL_SECTION_OWNER.compare_exchange(
                0,
                owner,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    CRITICAL_SECTION_DEPTH.store(1, Ordering::Relaxed);
                    return critical_section_token(interrupts_were_enabled, true);
                }
                Err(current) if current == owner => {
                    let depth = CRITICAL_SECTION_DEPTH.fetch_add(1, Ordering::Relaxed);
                    assert!(depth != usize::MAX, "critical section nesting overflowed");
                    return critical_section_token(interrupts_were_enabled, false);
                }
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    unsafe fn release(restore_state: usize) {
        compiler_fence(Ordering::SeqCst);
        let interrupts_were_enabled = critical_section_restore_interrupts(restore_state);
        let outermost = critical_section_is_outermost(restore_state);
        let previous_depth = CRITICAL_SECTION_DEPTH.fetch_sub(1, Ordering::Relaxed);
        assert!(previous_depth != 0, "critical section depth underflowed");

        if outermost {
            assert!(
                previous_depth == 1,
                "outermost critical section release observed nested depth {previous_depth}"
            );
            CRITICAL_SECTION_OWNER.store(0, Ordering::Release);
        }

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
    wasmtime_tls: Cell<*mut u8>,
    debug_transport: Option<DebugTransport>,
    external_interrupts: Option<net::ExternalInterrupts>,
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

struct SbiConsole {
    debug_state: debug_state::RuntimeState,
    mirror_to_uart: bool,
}

impl SbiConsole {
    fn new(debug_state: debug_state::RuntimeState, mirror_to_uart: bool) -> Self {
        Self {
            debug_state,
            mirror_to_uart,
        }
    }
}

impl Write for SbiConsole {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.debug_state
            .record_console_text(riscv::register::time::read64(), s);
        if !self.mirror_to_uart {
            return Ok(());
        }
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
    debug_state: debug_state::RuntimeState,
}

impl RiscvCpu {
    pub(crate) fn new(
        current_hart: ProcessorId,
        bootstrap_hart: ProcessorId,
        hart_count: usize,
        timebase_frequency: u64,
        fdt_addr: usize,
        debug_state: debug_state::RuntimeState,
    ) -> Self {
        Self {
            current_hart,
            bootstrap_hart,
            hart_count,
            timebase_frequency,
            fdt_addr,
            debug_state,
        }
    }

    pub(crate) fn debug_state(&self) -> debug_state::RuntimeState {
        self.debug_state.clone()
    }

    pub(crate) fn instance_registry(&self) -> helios_kernel::InstanceRegistry {
        self.debug_state.instance_registry()
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
        match hart_status(hart) {
            sbi_spec::hsm::hart_state::STARTED => {
                self.wake_processor(hart);
            }
            sbi_spec::hsm::hart_state::STOPPED => {
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
            state => panic!(
                "hart {} reported unexpected SBI HSM state {}",
                hart.id(),
                state
            ),
        }
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
        halt_forever()
    }

    fn reboot(&self) -> ! {
        sbi_rt::system_reset(sbi_rt::ColdReboot, sbi_rt::NoReason);
        halt_forever()
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
#[unsafe(link_section = ".trap.rust")]
extern "C" fn trap_handler(tf: &mut TrapFrame) {
    trap_dispatch(tf);
}

#[inline(never)]
fn trap_dispatch(tf: &mut TrapFrame) {
    match riscv::register::scause::read().cause().try_into() {
        Ok(Trap::Interrupt(Interrupt::SupervisorTimer)) => {
            current_hart_runtime().timer.handle_interrupt();
        }
        Ok(Trap::Interrupt(Interrupt::SupervisorSoft)) => unsafe {
            riscv::register::sip::clear_ssoft();
        },
        Ok(Trap::Interrupt(Interrupt::SupervisorExternal)) => {
            let runtime = current_hart_runtime();
            let interrupts = runtime.external_interrupts.as_ref().unwrap_or_else(|| {
                panic!(
                    "unhandled hardware interrupt without registered dispatch: interrupt={:?}, tf={tf:#x?}",
                    Interrupt::SupervisorExternal
                )
            });
            interrupts.handle();
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
    let fdt = unsafe { Fdt::from_ptr(fdt_addr as *const u8) }
        .expect("OpenSBI did not provide a valid FDT");
    let mut cpus = fdt.cpus();
    let first_cpu = cpus.next().expect("FDT does not describe any CPU");
    let hart_count = 1 + cpus.count();
    let timebase_frequency = first_cpu.timebase_frequency() as u64;
    let allocator_window = allocator_window();
    let memory_regions = collect_memory_regions(fdt.memory(), allocator_window.clone());
    let current_hart = ProcessorId::new(hart_id as u16);
    let bootstrap_processor = remember_bootstrap_hart(hart_id);
    if current_hart == bootstrap_processor {
        release_early_boot_harts();
    }
    if current_hart == bootstrap_processor {
        helios_kernel::prime_bootstrap_allocator(memory_regions.iter().copied());
    }

    let debug_state = shared_debug_state(timebase_frequency, hart_count);
    let debug_transport = DebugTransport::discover(&fdt);
    let console = SbiConsole::new(
        debug_state.clone(),
        helios_kernel::embedded_debugger().is_none(),
    );
    let cpu = RiscvCpu::new(
        current_hart,
        bootstrap_processor,
        hart_count,
        timebase_frequency,
        fdt_addr,
        debug_state.clone(),
    );

    let kernel = helios_kernel::init(helios_kernel::Platform::new(
        console,
        memory_regions.into_iter(),
        cpu.clone(),
    ));
    let external_interrupts = if current_hart == bootstrap_processor {
        let mut interrupts = net::install_network_service(&cpu, &kernel, &fdt, &debug_state);
        if let Some(host_fs) = host_fs::install(&cpu, &kernel, &fdt, &debug_state) {
            host_fs.plic.set_priority(host_fs.interrupt.source, 1);
            host_fs
                .plic
                .enable(host_fs.interrupt.source, host_fs.context);
            host_fs.plic.set_threshold(host_fs.context, 0);
            match interrupts.as_mut() {
                Some(interrupts_ref) => interrupts_ref.attach_host_fs(host_fs.interrupt),
                None => {
                    interrupts = Some(net::ExternalInterrupts::host_fs_only(
                        host_fs.plic,
                        host_fs.context,
                        host_fs.interrupt,
                    ));
                }
            }
            tracing::info!("virtio 9p online mount_tag=hostshare");
        }
        interrupts
    } else {
        None
    };
    let hart_runtime = HartRuntime {
        hart_id: current_hart,
        timer: kernel.timer(),
        wasmtime_tls: Cell::new(core::ptr::null_mut()),
        debug_transport,
        external_interrupts,
    };
    hart_runtime.install();
    unsafe {
        configure_interrupts();
    }
    if debugger_program::should_run_on(current_hart.id(), hart_count, bootstrap_processor.id()) {
        debugger_program::run_forever(cpu.clone());
    }
    if debugger_program::program_worker_should_run_on(
        current_hart.id(),
        hart_count,
        bootstrap_processor.id(),
    ) {
        debugger_program::run_program_workers_forever(cpu.clone(), kernel);
    }
    kernel.run();
}

fn shared_debug_state(timebase_frequency: u64, hart_count: usize) -> debug_state::RuntimeState {
    DEBUG_STATE
        .call_once(|| {
            debug_state::RuntimeState::new(
                timebase_frequency,
                hart_count,
                riscv::register::time::read64(),
            )
        })
        .clone()
}

pub(crate) fn global_debug_state() -> debug_state::RuntimeState {
    DEBUG_STATE
        .get()
        .expect("global debug state was requested before bootstrap initialization")
        .clone()
}

unsafe extern "C" {
    static __ebss: u8;
    static __stack_bottom_value: usize;
    static _secondary_start: u8;
    fn _release_mp_harts();
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

fn collect_memory_regions<'fdt>(
    memory: fdt::standard_nodes::Memory<'fdt, 'fdt>,
    allocator_window: Range<usize>,
) -> ArrayVec<MemoryRegion, 8> {
    let mut regions = ArrayVec::new();

    for region in memory
        .regions()
        .map(|region| {
            let start = region.starting_address as usize;
            let end = start
                .checked_add(region.size.unwrap())
                .expect("FDT memory region overflows usize");

            start..end
        })
        .filter_map(move |region| intersect(region, allocator_window.clone()))
        .map(range_to_memory_region)
    {
        regions.try_push(region).unwrap_or_else(|_| {
            panic!("FDT described more memory regions than the kernel supports")
        });
    }

    regions
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

const CRITICAL_SECTION_RESTORE_INTERRUPTS_BIT: usize = 1;
const CRITICAL_SECTION_OUTERMOST_BIT: usize = 1 << 1;

const fn critical_section_token(interrupts_were_enabled: bool, outermost: bool) -> usize {
    (interrupts_were_enabled as usize) | ((outermost as usize) << 1)
}

const fn critical_section_restore_interrupts(token: usize) -> bool {
    token & CRITICAL_SECTION_RESTORE_INTERRUPTS_BIT != 0
}

const fn critical_section_is_outermost(token: usize) -> bool {
    token & CRITICAL_SECTION_OUTERMOST_BIT != 0
}

fn critical_section_owner() -> usize {
    let runtime = read_hart_runtime();
    if runtime != 0 {
        return runtime;
    }

    // Before hart-local runtime state is installed, the kernel has not yet
    // started using any shared async synchronization primitives. Use a stable
    // bootstrap sentinel in that narrow window so early critical sections still
    // function without requiring M-mode-only hart-id registers.
    1
}

fn release_early_boot_harts() {
    // `_mp_hook` keeps every non-winning hart in a tiny early-boot spin loop
    // until the winner has completed the runtime's RAM initialization. Once
    // Rust code is safe to execute, publish the release gate so the other harts
    // can proceed into `helios_kernel::init`, which will park them again until
    // full kernel bootstrap is complete.
    unsafe {
        _release_mp_harts();
    }
}

fn hart_status(hart: ProcessorId) -> usize {
    let ret = sbi_rt::hart_get_status(hart.id() as usize);
    if ret.is_ok() {
        return ret.value;
    }

    panic!(
        "failed to query hart {} via SBI HSM: error={} value={}",
        hart.id(),
        ret.error,
        ret.value
    );
}

fn current_hart_runtime() -> &'static HartRuntime {
    let ptr = read_hart_runtime();
    assert!(ptr != 0, "hart runtime is not installed");

    unsafe { &*(ptr as *const HartRuntime) }
}

fn current_debug_transport() -> &'static DebugTransport {
    let runtime = current_hart_runtime();
    runtime
        .debug_transport
        .as_ref()
        .expect("debug transport is missing from the current hart runtime")
}

pub(crate) fn try_read_debug_serial_byte() -> Option<u8> {
    current_debug_transport().try_read_byte()
}

pub(crate) fn write_debug_serial_bytes(bytes: &[u8]) {
    current_debug_transport().write_bytes(bytes);
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_get() -> *mut u8 {
    let runtime = read_hart_runtime();
    if runtime == 0 {
        return core::ptr::null_mut();
    }

    unsafe { (*(runtime as *const HartRuntime)).wasmtime_tls.get() }
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(ptr: *mut u8) {
    let runtime = current_hart_runtime();
    runtime.wasmtime_tls.set(ptr);
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

    struct PanicConsole;

    impl Write for PanicConsole {
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

    let mut console = PanicConsole;
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
    halt_forever()
}

fn halt_forever() -> ! {
    loop {
        riscv::asm::wfi();
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
        asm!("mv tp, {}", in(reg) runtime, options(nostack, preserves_flags));
    }
}

fn clear_hart_runtime() {
    write_hart_runtime(core::ptr::null());
}
