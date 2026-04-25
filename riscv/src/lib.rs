#![no_std]
#![no_main]

extern crate alloc;
mod host_fs;
mod net;
mod pci;
mod watchdog;

mod debug_state {
    pub(crate) type RuntimeState =
        helios_kernel::HostRuntimeState<crate::RiscvCpu, crate::host_fs::HostFileSystemService>;
    pub(crate) type ProgramService =
        helios_kernel::UserProgramService<crate::RiscvCpu, crate::host_fs::HostFileSystemService>;
}

use ns16550a::Uart;

/// Debugger byte transport backed by the machine's boot UART. Kernel tracing
/// stays in memory so the line remains reserved for RPC traffic after boot.
#[derive(Clone, Copy)]
pub(crate) struct DebugTransport {
    uart_base: usize,
}

impl DebugTransport {
    pub(crate) fn discover(fdt: &Fdt<'_>) -> Option<Self> {
        let chosen = fdt.find_node("/chosen")?;
        let stdout_path = chosen
            .properties()
            .find(|property| property.name == "stdin-path")
            .or_else(|| {
                chosen
                    .properties()
                    .find(|property| property.name == "stdout-path")
            })?;
        let path = core::str::from_utf8(stdout_path.value)
            .ok()?
            .trim_end_matches('\0')
            .split(':')
            .next()?;
        let node = fdt
            .find_node(path)
            .or_else(|| fdt.aliases().and_then(|aliases| aliases.resolve_node(path)))?;
        let region = node.reg()?.next()?;
        Some(Self {
            uart_base: region.starting_address as usize,
        })
    }

    pub(crate) fn try_read_byte(&self) -> Option<u8> {
        Uart::new(self.uart_base).get()
    }

    pub(crate) fn write_bytes(&self, bytes: &[u8]) {
        let uart = Uart::new(self.uart_base);
        for &byte in bytes {
            while uart.put(byte).is_none() {
                core::hint::spin_loop();
            }
        }
    }
}

impl ByteSerial for DebugTransport {
    fn try_read_byte(&self) -> Option<u8> {
        DebugTransport::try_read_byte(self)
    }

    fn write_bytes(&self, bytes: &[u8]) {
        DebugTransport::write_bytes(self, bytes);
    }
}

use helios_virtio::DeviceType;

const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
const VIRTIO_MMIO_MODERN_VERSION: u32 = 2;
const VIRTIO_MMIO_MAGIC_OFFSET: usize = 0x000;
const VIRTIO_MMIO_VERSION_OFFSET: usize = 0x004;
const VIRTIO_MMIO_DEVICE_ID_OFFSET: usize = 0x008;

pub(crate) fn matches_virtio_mmio_device(base: usize, expected: DeviceType) -> bool {
    unsafe {
        read_u32(base + VIRTIO_MMIO_MAGIC_OFFSET) == VIRTIO_MMIO_MAGIC
            && read_u32(base + VIRTIO_MMIO_VERSION_OFFSET) == VIRTIO_MMIO_MODERN_VERSION
            && read_u32(base + VIRTIO_MMIO_DEVICE_ID_OFFSET) == expected as u32
    }
}

pub(crate) fn count_virtio_mmio_devices(fdt: &Fdt<'_>, expected: DeviceType) -> usize {
    fdt.all_nodes()
        .filter(|node| {
            node.compatible()
                .is_some_and(|compatible| compatible.all().any(|entry| entry == "virtio,mmio"))
        })
        .filter_map(|node| node.reg().and_then(|mut regs| regs.next()))
        .filter(|region| matches_virtio_mmio_device(region.starting_address as usize, expected))
        .count()
}

unsafe fn read_u32(addr: usize) -> u32 {
    unsafe { (addr as *const u32).read_volatile() }
}

use core::arch::{asm, global_asm};
use core::cell::Cell;
use core::fmt::Write;
use core::mem;
use core::ops::Range;
use core::sync::atomic::{AtomicUsize, Ordering, compiler_fence};

use arrayvec::ArrayVec;
use fdt::Fdt;
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::memory::MemoryRegion;
use helios_hal::serial::ByteSerial;
use helios_hal::{DeviceInventory, DmaModel, ProcessorStartupPolicy, ProcessorTopology};
use helios_kernel::{
    KernelException, KernelExceptionCause, KernelExceptionDispatch, KernelNativeTrapHandler, Timer,
};
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
static WASMTIME_NATIVE_TRAP_HANDLER: AtomicUsize = AtomicUsize::new(0);
static DEBUG_STATE: Once<debug_state::RuntimeState> = Once::new();
static WATCHDOG_STATE: Once<watchdog::RiscvWatchdog> = Once::new();

use core::ptr::NonNull;

const SSTATUS_SIE_BIT: usize = 1 << 1;
const SSTATUS_SPIE_BIT: usize = 1 << 5;

pub type ComputeTaskEntry = extern "C" fn(usize) -> !;

/// Register state that must be inherited by every supervisor-mode compute task.
///
/// `gp` and `tp` are not task-local in this kernel:
/// - `gp` must keep pointing at the kernel's global pointer window
/// - `tp` carries the current hart runtime pointer
///
/// The scheduler captures this template once per hart and stamps it into every
/// newly created compute task context.
#[derive(Clone, Copy)]
pub struct ComputeContextTemplate {
    gp: usize,
    tp: usize,
    sstatus: usize,
}

impl ComputeContextTemplate {
    pub fn capture_current() -> Self {
        let gp: usize;
        let tp: usize;
        unsafe {
            asm!("mv {}, gp", out(reg) gp, options(nomem, nostack, preserves_flags));
            asm!("mv {}, tp", out(reg) tp, options(nomem, nostack, preserves_flags));
        }

        Self {
            gp,
            tp,
            sstatus: riscv::register::sstatus::read().bits(),
        }
    }

    fn task_sstatus(self) -> usize {
        // Compute threads always resume in supervisor mode with interrupts
        // enabled after `sret`, but not while the trap handler is still
        // mutating the frame.
        (self.sstatus | SSTATUS_SPP_BIT | SSTATUS_SPIE_BIT) & !SSTATUS_SIE_BIT
    }
}

/// Saved execution context for one preemptible compute task.
///
/// The timer interrupt handler will save the interrupted trap frame here, and
/// the scheduler will later restore it into the live trap frame of a compute
/// hart before returning from the interrupt.
#[derive(Clone, Copy)]
pub struct ComputeTaskContext {
    trap_frame: TrapFrame,
}

impl ComputeTaskContext {
    /// Builds the initial supervisor context for a fresh compute task.
    ///
    /// The returned frame is ready to be copied into the live trap frame of a
    /// compute hart. After `sret`, execution will begin at `entry(arg)` on the
    /// provided stack.
    pub fn start(
        stack: NonNull<[u8]>,
        entry: ComputeTaskEntry,
        arg: usize,
        template: ComputeContextTemplate,
    ) -> Self {
        let mut trap_frame = TrapFrame::default();
        trap_frame.general.sp = aligned_stack_top(stack);
        trap_frame.general.gp = template.gp;
        trap_frame.general.tp = template.tp;
        trap_frame.general.a0 = arg;
        trap_frame.general.ra = compute_task_returned as *const () as usize;
        trap_frame.sstatus = template.task_sstatus();
        trap_frame.sepc = entry as usize;
        Self { trap_frame }
    }

    pub fn save_from_trap(&mut self, trap_frame: &TrapFrame) {
        self.trap_frame = *trap_frame;
    }

    pub fn restore_into_trap(self, trap_frame: &mut TrapFrame) {
        *trap_frame = self.trap_frame;
    }

    pub fn trap_frame(&self) -> &TrapFrame {
        &self.trap_frame
    }
}

fn aligned_stack_top(stack: NonNull<[u8]>) -> usize {
    let stack = stack.as_ptr();
    let len = unsafe { (&*stack).len() };
    let top = stack.cast::<u8>() as usize + len;
    top & !0xF
}

extern "C" fn compute_task_returned() -> ! {
    panic!("compute task returned unexpectedly");
}

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
    native_trap_handler: Cell<Option<KernelNativeTrapHandler>>,
    debug_transport: Option<DebugTransport>,
    external_interrupts: Option<net::ExternalInterrupts>,
    program_service: Option<debug_state::ProgramService>,
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

fn sbi_console(
    debug_state: debug_state::RuntimeState,
    mirror_to_uart: bool,
) -> helios_kernel::RecordingConsole<
    debug_state::RuntimeState,
    impl FnMut() -> u64,
    impl FnMut(&[u8]),
> {
    let write_fn: Option<fn(&[u8])> = if mirror_to_uart {
        Some(|bytes: &[u8]| {
            for &byte in bytes {
                let _ = sbi_rt::console_write_byte(byte);
            }
        })
    } else {
        None
    };
    helios_kernel::RecordingConsole::new(debug_state, || riscv::register::time::read64(), write_fn)
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
    pub(crate) fn new(
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

    fn publish_executable(&self, _ptr: *const u8, _len: usize) {
        unsafe {
            core::arch::asm!("fence.i", options(nostack, preserves_flags));
        }
    }

    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
        None
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
            if let Some(program_service) = current_hart_runtime().program_service.as_ref() {
                program_service.increment_epoch();
            }
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
    let watchdog = shared_watchdog(&fdt);
    let console = sbi_console(
        debug_state.clone(),
        !helios_kernel::has_embedded_system_component(),
    );
    let cpu = RiscvCpu::new(
        current_hart,
        bootstrap_processor,
        hart_count,
        timebase_frequency,
        fdt_addr,
    );
    let mut devices = DeviceInventory::new();
    if debug_transport.is_some() {
        devices = devices.with_debug_serial();
    }
    if net::has_network_device(&fdt) {
        devices = devices.with_network();
    }
    let block_device_count = count_virtio_mmio_devices(&fdt, DeviceType::Block);
    if block_device_count != 0 {
        devices = devices.with_block_devices(block_device_count);
    }
    if host_fs::has_9p_device(&fdt) {
        devices = devices.with_host_share();
    }
    let kernel = helios_kernel::init_with_watchdog(
        helios_kernel::Platform::with_watchdog(
            console,
            memory_regions.into_iter(),
            cpu.clone(),
            watchdog,
        )
        .with_topology(
            ProcessorTopology::start_all_secondaries(bootstrap_processor, hart_count)
                .with_startup_policy(ProcessorStartupPolicy::BootstrapOnly),
        )
        .with_timer_frequency_hz(timebase_frequency)
        .with_dma_model(DmaModel::Identity)
        .with_devices(devices),
    );
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
            tracing::info!(
                "virtio 9p online mount_tag={}",
                crate::host_fs::HOST_MOUNT_TAG
            );
        }
        interrupts
    } else {
        None
    };
    let mut hart_runtime = HartRuntime {
        hart_id: current_hart,
        timer: kernel.timer(),
        wasmtime_tls: Cell::new(core::ptr::null_mut()),
        native_trap_handler: Cell::new(None),
        debug_transport,
        external_interrupts,
        program_service: None,
    };
    hart_runtime.install();
    let program_service =
        helios_kernel::install_component_host_program_service(&kernel, &cpu, &debug_state);
    hart_runtime.program_service = program_service;
    unsafe {
        configure_interrupts();
    }
    if current_hart == bootstrap_processor {
        for processor in
            helios_kernel::component_host_processors_to_start(hart_count, bootstrap_processor)
        {
            cpu.start_processor(processor);
        }
    }
    helios_kernel::run_component_host_processor_forever(
        cpu,
        kernel,
        debug_state,
        read_debug_serial,
        write_debug_serial_bytes,
    );
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

fn shared_watchdog(fdt: &Fdt<'_>) -> watchdog::RiscvWatchdog {
    WATCHDOG_STATE.call_once(|| watchdog::discover(fdt)).clone()
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

use helios_hal::align_up;

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

fn read_debug_serial(max_bytes: u32) -> alloc::vec::Vec<u8> {
    helios_kernel::try_read_serial(current_debug_transport(), max_bytes)
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

#[unsafe(no_mangle)]
extern "C" fn wasmtime_init_traps(handler: KernelNativeTrapHandler) -> i32 {
    WASMTIME_NATIVE_TRAP_HANDLER.store(handler as usize, Ordering::Release);
    let runtime = current_hart_runtime();
    runtime.native_trap_handler.set(Some(handler));
    0
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
        TrapOrigin::Kernel => {
            if dispatch_kernel_exception(exception, stval, tf) == KernelExceptionDispatch::Unhandled
            {
                panic!(
                    "kernel exception: {exception:?}, sepc={:#x}, stval={:#x}, tf={tf:#x?}",
                    tf.sepc, stval,
                );
            }
            unreachable!("handled kernel exception returned to the RISC-V dispatcher")
        }
        TrapOrigin::User => handle_user_exception(exception, stval, tf),
    }
}

fn dispatch_kernel_exception(
    exception: Exception,
    stval: usize,
    tf: &TrapFrame,
) -> KernelExceptionDispatch {
    let handler = if let Some(handler) = current_hart_runtime().native_trap_handler.get() {
        handler
    } else {
        let raw_handler = WASMTIME_NATIVE_TRAP_HANDLER.load(Ordering::Acquire);
        if raw_handler == 0 {
            return KernelExceptionDispatch::Unhandled;
        }
        unsafe { mem::transmute(raw_handler) }
    };
    let Some(cause) = kernel_exception_cause(exception) else {
        return KernelExceptionDispatch::Unhandled;
    };
    write_debug_serial_bytes(
        alloc::format!(
            "\n[KDBG kernel-exception-dispatch cause={cause:?} pc={:#x} fp={:#x} tls={:#x}]\n",
            tf.sepc,
            tf.general.s0,
            wasmtime_tls_get() as usize,
        )
        .as_bytes(),
    );
    KernelException {
        cause,
        instruction_pointer: tf.sepc,
        frame_pointer: tf.general.s0,
        faulting_address: kernel_exception_faulting_address(exception, stval),
    }
    .dispatch_to(handler)
}

fn kernel_exception_cause(exception: Exception) -> Option<KernelExceptionCause> {
    match exception {
        Exception::InstructionMisaligned
        | Exception::InstructionFault
        | Exception::InstructionPageFault => Some(KernelExceptionCause::InstructionFault),
        Exception::IllegalInstruction => Some(KernelExceptionCause::IllegalInstruction),
        Exception::Breakpoint => Some(KernelExceptionCause::Breakpoint),
        Exception::LoadMisaligned
        | Exception::LoadFault
        | Exception::StoreMisaligned
        | Exception::StoreFault
        | Exception::LoadPageFault
        | Exception::StorePageFault => Some(KernelExceptionCause::DataFault),
        Exception::UserEnvCall | Exception::SupervisorEnvCall => None,
    }
}

fn kernel_exception_faulting_address(exception: Exception, stval: usize) -> Option<usize> {
    match exception {
        Exception::InstructionMisaligned
        | Exception::InstructionFault
        | Exception::InstructionPageFault
        | Exception::LoadMisaligned
        | Exception::LoadFault
        | Exception::StoreMisaligned
        | Exception::StoreFault
        | Exception::LoadPageFault
        | Exception::StorePageFault => Some(stval),
        Exception::IllegalInstruction
        | Exception::Breakpoint
        | Exception::UserEnvCall
        | Exception::SupervisorEnvCall => None,
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
