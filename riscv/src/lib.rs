#![no_std]
#![no_main]

extern crate alloc;
mod balloon;
mod block;
mod entropy;
mod host_fs;
mod net;
mod pci;
mod rtc;
mod vsock;
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

/// RISC-V MMIO is identity-mapped, so candidates are probed at their
/// physical base directly.
pub(crate) fn matches_virtio_mmio_device(base: usize, expected: DeviceType) -> bool {
    unsafe { helios_virtio::mmio_device_matches(base, expected) }
}

pub(crate) fn count_virtio_mmio_devices(fdt: &Fdt<'_>, expected: DeviceType) -> usize {
    helios_virtio::mmio_candidates(fdt)
        .filter(|candidate| matches_virtio_mmio_device(candidate.base, expected))
        .count()
}

use core::arch::{asm, global_asm};
use core::cell::Cell;
use core::fmt::Write;
use core::mem;
use core::num::NonZeroUsize;
use core::ops::Range;
use core::sync::atomic::{AtomicUsize, Ordering, compiler_fence};

use arrayvec::ArrayVec;
use fdt::Fdt;
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::critical_section::ProcessorIdentity;
use helios_hal::memory::MemoryRegion;
use helios_hal::serial::ByteSerial;
use helios_hal::{DeviceInventory, DmaModel, ProcessorStartupPolicy, ProcessorTopology};
use helios_kernel::{
    KernelException, KernelExceptionCause, KernelExceptionDispatch, KernelNativeTrapHandler, Timer,
    WasmtimeTlsSlots,
};
use riscv::interrupt::Trap;
use riscv::interrupt::supervisor::{Exception, Interrupt};
use riscv_rt::entry;
use spin::Once;

mod trap;

pub use trap::{GeneralRegs, TrapFrame};

global_asm!(include_str!("mp_hook.S"));
global_asm!(include_str!("secondary_entry.S"));

const UNINITIALIZED_BOOT_HART: usize = usize::MAX;
/// One bit of [`ONLINE_HARTS`] per hart id.
const MAX_TRACKED_HARTS: usize = usize::BITS as usize;
const SSTATUS_SPP_BIT: usize = 1 << 8;

static BOOT_HART_ID: AtomicUsize = AtomicUsize::new(UNINITIALIZED_BOOT_HART);
/// Harts that have entered [`run_hart`] and can therefore be parked in `wfi`.
///
/// SBI only accepts `sbi_send_ipi` for harts the firmware currently reports as
/// started, suspended, or resume-pending: OpenSBI's `sbi_ipi_send_many`
/// intersects the requested mask with the interruptible harts and rejects the
/// whole call with `SBI_ERR_INVALID_PARAM` when any requested hart drops out.
/// The kernel scheduler legitimately nudges secondaries while the bootstrap
/// hart is still bringing up devices, long before it hands them to
/// `sbi_hart_start`, so the backend tracks which harts are actually running
/// kernel code. A hart that has not entered the kernel is not parked and needs
/// no nudge: it drains the run queues as soon as it reaches the scheduler.
static ONLINE_HARTS: AtomicUsize = AtomicUsize::new(0);
static CRITICAL_SECTION_STATE: helios_hal::critical_section::CriticalSectionState =
    helios_hal::critical_section::CriticalSectionState::new();
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

struct SupervisorInterruptOps;

impl helios_hal::critical_section::InterruptOps for SupervisorInterruptOps {
    fn interrupts_enabled() -> bool {
        riscv::register::sstatus::read().sie()
    }

    fn disable_interrupts() {
        riscv::interrupt::supervisor::disable();
    }

    unsafe fn enable_interrupts() {
        unsafe { riscv::interrupt::supervisor::enable() };
    }

    fn current_identity() -> ProcessorIdentity {
        read_hart_identity()
    }
}

struct SupervisorCriticalSection;

critical_section::set_impl!(SupervisorCriticalSection);

unsafe impl critical_section::Impl for SupervisorCriticalSection {
    unsafe fn acquire() -> usize {
        unsafe { CRITICAL_SECTION_STATE.acquire::<SupervisorInterruptOps>() }
    }

    unsafe fn release(restore_state: usize) {
        unsafe { CRITICAL_SECTION_STATE.release::<SupervisorInterruptOps>(restore_state) }
    }
}

struct HartRuntime {
    hart_id: ProcessorId,
    timer: Timer<RiscvCpu>,
    wasmtime_tls: WasmtimeTlsSlots,
    native_trap_handler: Cell<Option<KernelNativeTrapHandler>>,
    debug_transport: Option<DebugTransport>,
    external_interrupts: Option<net::ExternalInterrupts>,
    program_service: Option<debug_state::ProgramService>,
}

impl HartRuntime {
    /// Publishes this runtime as the hart's identity.
    ///
    /// Hart-local runtime state lives in `tp`, which the trap entry saves
    /// and restores with the rest of the integer file.
    /// This keeps trap entry correct while still giving the kernel a cheap
    /// per-hart lookup path. The hart already carried a bootstrapping identity
    /// in `tp`; this replaces it, and both forms are distinguishable, so the
    /// critical section never confuses two harts for one.
    ///
    /// The runtime must outlive the hart: `run_hart` never returns, so the
    /// stack frame holding it lives as long as the hart does.
    fn install(&self) {
        write_hart_identity(ProcessorIdentity::installed(self));
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
    helios_kernel::RecordingConsole::new(debug_state, riscv::register::time::read64, write_fn)
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
        installed_hart_runtime().map_or(self.current_hart, |runtime| runtime.hart_id)
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

        if !hart_is_online(hart) {
            // The hart has not entered the kernel yet, so it cannot be parked
            // in `wfi`. Sending it an IPI would be rejected by SBI with
            // `SBI_ERR_INVALID_PARAM`, and there is nothing to wake: the hart
            // observes every queued task once it reaches the scheduler.
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

    fn publish_executable(&self, ptr: *const u8, len: usize) {
        helios_kernel::runtime_memory::publish_code_memory(ptr, len);
        // `fence.i` is the only ordering RISC-V defines between a store to an
        // instruction's memory and a fetch of it, and it is per-hart. The
        // range is published before any other hart can be steered into it, so
        // fencing the publishing hart is what the hart that executes next
        // needs; every other hart reaches this code through a scheduler
        // handoff that fences on its own side.
        unsafe {
            core::arch::asm!("fence.i", options(nostack, preserves_flags));
        }
    }

    fn unpublish_executable(&self, ptr: *const u8, len: usize) {
        helios_kernel::runtime_memory::unpublish_code_memory(ptr, len);
    }

    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
        None
    }

    fn has_lazy_commit_virtual_memory(&self) -> bool {
        // `RiscvUserAddressSpace` reserves Sv48 virtual ranges without
        // touching a frame and commits them page by page on request, so the
        // runtime can pre-reserve a 4 GiB slot per linear memory out of the
        // 32 TiB user window and pay physical memory only for what a guest
        // actually touches.
        true
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

mod vmm;
pub use vmm::RiscvUserAddressSpace;

#[unsafe(no_mangle)]
extern "C" fn secondary_start_rust(hart_id: usize, fdt_addr: usize) -> ! {
    run_hart(hart_id, fdt_addr)
}

/// The Rust half of the supervisor trap entry in `trap.S`.
#[unsafe(no_mangle)]
extern "C" fn __helios_riscv_trap_dispatch(tf: &mut TrapFrame) {
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
    // Every hart takes critical sections long before it builds its runtime,
    // and several harts run this prologue concurrently. Seeding `tp` with the
    // hart's own bootstrapping identity here is what keeps those acquires
    // distinguishable; sharing one sentinel makes a second hart's acquire look
    // like the first hart's nested re-acquire and voids mutual exclusion.
    write_hart_identity(ProcessorIdentity::bootstrapping(hart_id));
    let fdt = unsafe { Fdt::from_ptr(fdt_addr as *const u8) }
        .expect("OpenSBI did not provide a valid FDT");
    let mut cpus = fdt.cpus();
    let first_cpu = cpus.next().expect("FDT does not describe any CPU");
    let hart_count = 1 + cpus.count();
    assert!(
        hart_count <= MAX_TRACKED_HARTS,
        "FDT describes {hart_count} harts but this backend tracks at most {MAX_TRACKED_HARTS}"
    );
    let timebase_frequency = first_cpu.timebase_frequency() as u64;
    let allocator_window = allocator_window();
    let memory_regions = collect_memory_regions(fdt.memory(), allocator_window.clone());
    let current_hart = ProcessorId::new(hart_id as u16);
    let bootstrap_processor = remember_bootstrap_hart(hart_id);
    mark_hart_online(current_hart);
    // Bring up Sv48 paging on every hart before any allocator or
    // driver work. The bootstrap hart populates the root table once
    // (identity-mapping the kernel's 512 GiB physical window with a
    // single root-level leaf), then every hart writes its own `satp` to
    // switch into paged execution. Identity mapping makes the transition
    // transparent for kernel addresses; the 32 TiB user-VA window above
    // the identity map is owned by `RiscvUserAddressSpace`, which is
    // where the runtime's linear-memory reservations live.
    for region in &memory_regions {
        let start = region.as_ptr().cast::<u8>() as usize;
        let end = start + region.len();
        assert!(
            end <= vmm::KERNEL_IDENTITY_LIMIT,
            "firmware reports physical memory at {start:#x}..{end:#x}, past the {:#x} the Sv48 \
             kernel identity map reaches",
            vmm::KERNEL_IDENTITY_LIMIT
        );
    }
    if current_hart == bootstrap_processor {
        unsafe {
            vmm::install_kernel_paging();
        }
        vmm::install_runtime_memory_hooks();
    }
    unsafe {
        vmm::activate_paging();
    }
    if current_hart == bootstrap_processor {
        release_early_boot_harts();
    }
    if current_hart == bootstrap_processor {
        helios_kernel::prime_bootstrap_allocator(memory_regions.iter().copied(), hart_count);
    }

    let debug_state = shared_debug_state(timebase_frequency, hart_count);
    let debug_transport = DebugTransport::discover(&fdt);
    let watchdog = shared_watchdog(&fdt);
    let has_vsock = vsock::has_vsock_device(&fdt);
    // The boot UART carries kernel tracing unless the embedded system
    // component needs the line to itself for its RPC framing. It needs
    // the line only when the machine has no vsock device: with one, the
    // component serves its RPC there and the UART stays a console, which
    // is the whole reason the vsock transport exists.
    let console = sbi_console(
        debug_state.clone(),
        !helios_kernel::has_embedded_system_component() || has_vsock,
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
    let block_device_count = block::count_block_devices(&fdt);
    if block_device_count != 0 {
        devices = devices.with_block_devices(block_device_count);
    }
    if host_fs::has_9p_device(&fdt) {
        devices = devices.with_host_share();
    }
    if entropy::has_entropy_device(&fdt) {
        devices = devices.with_entropy_device();
    }
    if balloon::has_balloon_device(&fdt) {
        devices = devices.with_memory_balloon();
    }
    if has_vsock {
        devices = devices.with_vsock();
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
    // The root DRBG is seeded once, on the bootstrap hart, before
    // anything can ask for random bytes. RISC-V has no unprivileged
    // entropy instruction the kernel can rely on, so the seed OpenSBI
    // leaves in `/chosen/rng-seed` is the platform's pre-boot source;
    // the entropy device joins it as soon as the executor runs. This
    // follows `init` so the source line reaches the log the kernel just
    // installed.
    // The entropy device comes up before the root DRBG is seeded,
    // because riscv64 has no entropy instruction and a platform whose
    // firmware leaves no `/chosen/rng-seed` has this device and nothing
    // else. Its PLIC source is attached later, with every other
    // device's; the bring-up read polls the used ring and needs none.
    let entropy_device = (current_hart == bootstrap_processor)
        .then(|| entropy::bring_up(&fdt))
        .flatten();
    let root_entropy = (current_hart == bootstrap_processor).then(|| {
        let root = helios_kernel::seed_root_entropy(
            &cpu,
            helios_hal::entropy::device_tree_seed(&fdt),
            entropy_device.as_ref().map(|entropy| &entropy.device),
        );
        debug_state.install_root_entropy(root.clone());
        // The calendar is read once, on the bootstrap hart, before any
        // component can ask what time it is. The hart timer carries wall
        // time forward from that reading; nothing re-synchronises it.
        match rtc::discover(&fdt) {
            Some(rtc) => {
                debug_state.seed_wall_clock(cpu.now().ticks(), &rtc);
            }
            None => {
                tracing::warn!(
                    "no goldfish RTC in the device tree; the wall clock reads as uptime"
                );
            }
        }
        root
    });
    // Only the bootstrap hart brings devices up, and it is the hart
    // that holds the root DRBG handle.
    let external_interrupts = root_entropy.and_then(|root_entropy| {
        // Every platform device shares one PLIC context, so the context
        // is opened once and each device attaches its own source to it.
        net::discover_plic_context(&fdt, bootstrap_processor.id()).map(|(plic, context)| {
            let mut interrupts = net::ExternalInterrupts::new(plic, context);
            if let Some(network) = net::install_network_service(&cpu, &kernel, &fdt, &debug_state) {
                interrupts.attach_network(network);
            }
            if let Some(host_fs) = host_fs::install(&cpu, &fdt, &debug_state) {
                interrupts.attach_host_fs(host_fs);
            }
            if let Some(entropy) = entropy_device {
                entropy::install(&kernel, &entropy, root_entropy.clone());
                interrupts.attach_entropy(entropy);
            }
            if let Some(balloon) = balloon::install(&kernel, &fdt) {
                debug_state.install_memory_balloon(balloon.handle.clone());
                interrupts.attach_balloon(balloon);
            }
            if let Some(vsock) = vsock::install(&kernel, &cpu, &fdt, &debug_state) {
                interrupts.attach_vsock(vsock);
            }
            for block in block::install(&cpu, &kernel, &fdt, &debug_state, root_entropy) {
                interrupts.attach_block(block);
            }
            // The Sv48 address space reserves and commits lazily, so a page
            // could be taken away here — but the backend has not wired the
            // other half: no `SwapVmHooks` table, so a not-present PTE
            // carries no swap token and the fault handler has nothing to
            // look the page up by. Extending swap to this backend is #25's
            // follow-up to #59.
            helios_kernel::disable_swap(helios_kernel::SwapDisabled::NoSwapHooks);
            interrupts
        })
    });
    let mut hart_runtime = HartRuntime {
        hart_id: current_hart,
        timer: kernel.timer(),
        wasmtime_tls: WasmtimeTlsSlots::new(),
        native_trap_handler: Cell::new(None),
        debug_transport,
        external_interrupts,
        program_service: None,
    };
    hart_runtime.install();
    let program_service = helios_kernel::install_component_host_program_service(
        &kernel,
        &cpu,
        &debug_state,
        read_debug_serial,
        write_debug_serial_bytes,
    );
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
    static _stack_start: u8;
    static _secondary_start: u8;
    fn _release_mp_harts();
}

fn allocator_window() -> Range<usize> {
    let start = align_up(
        core::ptr::addr_of!(_stack_start).addr(),
        core::mem::align_of::<usize>(),
    );
    assert!(
        core::ptr::addr_of!(__ebss).addr() < start,
        "riscv stack must be linked after kernel bss"
    );
    start..usize::MAX
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

fn hart_bit(hart: ProcessorId) -> usize {
    let index = usize::from(hart.id());
    assert!(
        index < MAX_TRACKED_HARTS,
        "hart {index} is outside the {MAX_TRACKED_HARTS} harts this backend tracks"
    );

    1 << index
}

/// Publishes the current hart as a valid IPI target.
///
/// Called before the hart can reach any parking point, so a waker that misses
/// the publication still cannot lose a wakeup: the hart drains the run queues
/// after this point.
fn mark_hart_online(hart: ProcessorId) {
    ONLINE_HARTS.fetch_or(hart_bit(hart), Ordering::Release);
}

fn hart_is_online(hart: ProcessorId) -> bool {
    ONLINE_HARTS.load(Ordering::Acquire) & hart_bit(hart) != 0
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
    installed_hart_runtime().expect("hart runtime is not installed")
}

pub(crate) fn current_hart_id() -> ProcessorId {
    current_hart_runtime().hart_id
}

fn current_debug_transport() -> &'static DebugTransport {
    let runtime = current_hart_runtime();
    runtime
        .debug_transport
        .as_ref()
        .expect("debug transport is missing from the current hart runtime")
}

fn read_debug_serial(buffer: &mut alloc::vec::Vec<u8>, max_bytes: u32) {
    helios_kernel::try_read_serial(current_debug_transport(), buffer, max_bytes);
}

pub(crate) fn write_debug_serial_bytes(bytes: &[u8]) {
    current_debug_transport().write_bytes(bytes);
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_get(slot: usize) -> *mut u8 {
    installed_hart_runtime().map_or(core::ptr::null_mut(), |runtime| {
        runtime.wasmtime_tls.get(slot)
    })
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(slot: usize, ptr: *mut u8) {
    current_hart_runtime().wasmtime_tls.set(slot, ptr);
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
        trap::install_trap_vector();
        riscv::register::sie::set_ssoft();
        riscv::register::sie::set_stimer();
        riscv::register::sie::set_sext();
        riscv::register::sstatus::set_sie();
    }
    // The entry is live from here, so this is the first moment the hart can
    // prove it round-trips the floating-point file. §3.4: every hart proves
    // it for itself, because `FS` and the trap vector are per-hart state.
    trap::verify_trap_preserves_fp_state();
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
        // SAFETY: the only writer of this slot stores a
        // `KernelNativeTrapHandler` cast to `usize`, and a zero means
        // "unset", which the check above has already ruled out.
        unsafe { mem::transmute::<usize, KernelNativeTrapHandler>(raw_handler) }
    };
    let Some(cause) = kernel_exception_cause(exception) else {
        return KernelExceptionDispatch::Unhandled;
    };
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

    // The panic report shares the UART with kernel tracing and the debugger's
    // stage markers, so it goes out as one indivisible message like every
    // other console producer. Nothing follows it: the hart shuts down next.
    helios_kernel::emit_console_line(|| {
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
    });

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

/// The identity this hart carries in `tp`: its bootstrapping form until
/// [`HartRuntime::install`] replaces it with the runtime address.
fn read_hart_identity() -> ProcessorIdentity {
    let word = NonZeroUsize::new(current_hart_runtime_ptr())
        .expect("tp lost the hart identity `run_hart` seeded");
    ProcessorIdentity::from_raw(word)
}

fn write_hart_identity(identity: ProcessorIdentity) {
    unsafe {
        asm!("mv tp, {}", in(reg) identity.raw(), options(nostack, preserves_flags));
    }
}

/// The installed runtime, or `None` while the hart still carries only its
/// bootstrapping identity.
fn installed_hart_runtime() -> Option<&'static HartRuntime> {
    let address = read_hart_identity().runtime_address()?;
    // SAFETY: only `HartRuntime::install` publishes a runtime address, and it
    // does so from a `&'static HartRuntime`.
    Some(unsafe { &*(address.get() as *const HartRuntime) })
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_parking_wait(_timeout_nanos: u64) {
    riscv::asm::wfi();
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_parking_unpark() {}
