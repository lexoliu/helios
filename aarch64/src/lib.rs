#![no_std]
#![no_main]

extern crate alloc;

use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;
use core::fmt::{self, Write};
use core::num::NonZeroUsize;
use core::ops::Range;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use alloc::boxed::Box;
use alloc::vec::Vec;
use arm_gic::IntId;
use helios_hal::boot::{
    BootFirmwareTables, BootHandoff, BootKernelImage, BootMemoryKind, BootMemoryMap,
    BootMemoryRegion, BootModule, BootModules, FirmwareKind,
};
use helios_hal::cpu::{Cpu, HardwarePerfCounters, Instant, ProcessorId};
use helios_hal::critical_section::ProcessorIdentity;
use helios_hal::entropy::{EntropyQuality, EntropyUnavailable};
use helios_hal::memory::MemoryRegion;
use helios_hal::serial::ByteSerial;
use helios_hal::{DeviceInventory, DmaModel, Platform, ProcessorStartupPolicy, ProcessorTopology};
use helios_kernel::{
    KernelException, KernelExceptionCause, KernelNativeTrapHandler, Timer, WasmtimeTlsSlots,
};
use limine::BaseRevision;
use limine::file::File;
use limine::firmware::{
    FIRMWARE_TYPE_EFI32, FIRMWARE_TYPE_EFI64, FIRMWARE_TYPE_SBI, FIRMWARE_TYPE_X86BIOS,
};
use limine::memmap::{Entry, MEMMAP_USABLE};
use limine::mp::MpInfo;
use limine::request::{
    DtbRequest, ExecutableAddressRequest, ExecutableCmdlineRequest, ExecutableFileRequest,
    FirmwareTypeRequest, HhdmRequest, MemmapRequest, ModulesRequest, MpRequest, RsdpRequest,
    StackSizeRequest,
};
use spin::Once;

const KERNEL_STACK_BYTES: usize = 4 * 1024 * 1024;
const EXCEPTION_STACK_BYTES: usize = 64 * 1024;
/// Bytes the IRQ entry reserves on the per-processor exception stack for
/// `x0`-`x30`, `elr_el1` and `spsr_el1`, rounded up to the 16-byte stack
/// alignment the architecture requires.
const IRQ_FRAME_BYTES: usize = 272;
const PAGE_BYTES: usize = 4096;
const MMIO_BLOCK_BYTES: usize = 2 * 1024 * 1024;
const MAX_USABLE_REGION_SEGMENTS: usize = 6;
const PAGE_TABLE_ENTRIES: usize = 512;
const PAGE_TABLE_INDEX_MASK: usize = PAGE_TABLE_ENTRIES - 1;
const PAGE_TABLE_DESCRIPTOR: u64 = 0b11;
const BLOCK_DESCRIPTOR: u64 = 0b01;
const PAGE_VALID: u64 = 1;
const PAGE_ADDRESS_MASK: u64 = 0x0000_ffff_ffff_f000;
const PAGE_AF: u64 = 1 << 10;
const PAGE_ATTR_INDEX_SHIFT: u64 = 2;
const PAGE_ATTR_DEVICE_INDEX: u64 = 7;
const PAGE_PXN: u64 = 1 << 53;
const PAGE_UXN: u64 = 1 << 54;
const MAIR_DEVICE_ATTR_INDEX_SHIFT: u64 = PAGE_ATTR_DEVICE_INDEX * 8;
const MAIR_DEVICE_NGNRNE: u64 = 0x00;
const PL011_DATA: usize = 0x000;
const PL011_FLAG: usize = 0x018;
const PL011_FLAG_RXFE: u32 = 1 << 4;
const PL011_FLAG_TXFF: u32 = 1 << 5;

#[cfg(target_os = "none")]
global_asm!(
    include_str!("entry.S"),
    boot_stack_bytes = const KERNEL_STACK_BYTES,
    exception_stack_bytes = const EXCEPTION_STACK_BYTES,
    irq_frame_bytes = const IRQ_FRAME_BYTES,
);

unsafe extern "C" {
    static __helios_aarch64_exception_vectors: u8;
}

mod vmm;
pub use vmm::Aarch64UserAddressSpace;
mod balloon;
mod block;
mod entropy;
mod gic;
mod host_fs;
mod net;
mod platform;
mod rtc;
mod vsock;

mod debug_state {
    pub(crate) type RuntimeState =
        helios_kernel::HostRuntimeState<crate::Aarch64Cpu, crate::host_fs::HostFileSystemService>;
    pub(crate) type ProgramService =
        helios_kernel::UserProgramService<crate::Aarch64Cpu, crate::host_fs::HostFileSystemService>;
}

/// Interrupt routes the bootstrap processor installs for the virtio
/// devices it brought up. Every device SPI is delivered to that
/// processor, so the routes live in its per-processor runtime.
pub(crate) type DeviceInterruptRoutes = helios_kernel::ExternalInterruptRoutes<
    IntId,
    net::VirtioNetworkDevice,
    host_fs::HostFsTransportService,
    entropy::VirtioEntropyDevice,
    balloon::VirtioBalloonInterrupt,
    vsock::VirtioVsockDevice,
    block::VirtioBlockDevice,
>;

#[used]
static BASE_REVISION: BaseRevision = BaseRevision::with_revision(6);
#[used]
static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();
#[used]
static MEMORY_MAP_REQUEST: MemmapRequest = MemmapRequest::new();
#[used]
static EXECUTABLE_ADDRESS_REQUEST: ExecutableAddressRequest = ExecutableAddressRequest::new();
#[used]
static EXECUTABLE_FILE_REQUEST: ExecutableFileRequest = ExecutableFileRequest::new();
#[used]
static EXECUTABLE_CMDLINE_REQUEST: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();
#[used]
static MODULE_REQUEST: ModulesRequest = ModulesRequest::new();
#[used]
static MP_REQUEST: MpRequest = MpRequest::new(0);
#[used]
static FIRMWARE_TYPE_REQUEST: FirmwareTypeRequest = FirmwareTypeRequest::new();
#[used]
static DEVICE_TREE_BLOB_REQUEST: DtbRequest = DtbRequest::new();
#[used]
static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();
#[used]
static STACK_SIZE_REQUEST: StackSizeRequest = StackSizeRequest::new(KERNEL_STACK_BYTES as u64);
static DEBUG_SERIAL_BASE: AtomicUsize = AtomicUsize::new(0);
static DEBUG_SERIAL_WRITER_HELD: AtomicBool = AtomicBool::new(false);
static CRITICAL_SECTION_STATE: helios_hal::critical_section::CriticalSectionState =
    helios_hal::critical_section::CriticalSectionState::new();

#[repr(align(4096))]
struct PageTable(UnsafeCell<[u64; PAGE_TABLE_ENTRIES]>);

unsafe impl Sync for PageTable {}

impl PageTable {
    const fn new() -> Self {
        Self(UnsafeCell::new([0; PAGE_TABLE_ENTRIES]))
    }

    fn as_mut_ptr(&self) -> *mut u64 {
        self.0.get().cast()
    }

    fn clear(&self) {
        unsafe {
            core::ptr::write_bytes(self.as_mut_ptr(), 0, PAGE_TABLE_ENTRIES);
        }
    }

    fn physical_address(&self, handoff: &LimineBootHandoff) -> usize {
        kernel_virtual_to_physical(self.as_mut_ptr() as usize, handoff)
    }
}

static MMIO_PAGE_TABLE_0: PageTable = PageTable::new();
static MMIO_PAGE_TABLE_1: PageTable = PageTable::new();
static MMIO_PAGE_TABLE_2: PageTable = PageTable::new();

#[repr(C)]
struct ProcessorRuntime {
    exception_stack: ExceptionStack,
    logical_id: u16,
    _reserved: u16,
    wasmtime_tls: WasmtimeTlsSlots,
    native_trap_handler: AtomicUsize,
    started: AtomicBool,
    /// This processor's kernel timer, published once its own
    /// `helios_kernel::init` has run so the timer PPI can advance it.
    timer: Once<Timer<Aarch64Cpu>>,
    /// The component-host program service, published on the processor
    /// that owns it so the timer PPI can drive epoch interruption.
    program_service: Once<debug_state::ProgramService>,
    /// Device interrupt routes, installed on the bootstrap processor
    /// only: every device SPI is routed to it.
    device_interrupts: Once<DeviceInterruptRoutes>,
    /// Set by `wake_processor` before it sends the wake SGI, cleared by
    /// `park_current` under masked interrupts. It closes the window
    /// between the run loop finding its queue empty and the `wfi` that
    /// parks: a wake published in that window is observed instead of
    /// slept through.
    wake_pending: AtomicBool,
    /// Block size in bytes for the `DC ZVA` cache-line zero
    /// instruction, cached from `DCZID_EL0` at processor bring-up.
    /// Zero means DC ZVA is prohibited on this PE (`DCZID_EL0.DZP`
    /// is set) and callers must fall back to a generic memset.
    dc_zva_block_bytes: AtomicU32,
}

impl ProcessorRuntime {
    fn new(logical_id: u16) -> Self {
        Self {
            exception_stack: ExceptionStack::new(),
            logical_id,
            _reserved: 0,
            wasmtime_tls: WasmtimeTlsSlots::new(),
            native_trap_handler: AtomicUsize::new(0),
            started: AtomicBool::new(false),
            timer: Once::new(),
            program_service: Once::new(),
            device_interrupts: Once::new(),
            wake_pending: AtomicBool::new(false),
            dc_zva_block_bytes: AtomicU32::new(0),
        }
    }
}

/// The exception entries in `entry.S` derive the per-processor
/// exception stack from `tpidr_el1`, which only holds if the stack is
/// the first field of the runtime.
const _: () = assert!(core::mem::offset_of!(ProcessorRuntime, exception_stack) == 0);

#[repr(C, align(16))]
struct ExceptionStack([u8; EXCEPTION_STACK_BYTES]);

impl ExceptionStack {
    const fn new() -> Self {
        Self([0; EXCEPTION_STACK_BYTES])
    }
}

struct Aarch64ProcessorSlot {
    mp_info: &'static MpInfo,
    runtime: ProcessorRuntime,
}

struct Aarch64PlatformState {
    processors: Box<[Aarch64ProcessorSlot]>,
    timer_frequency: u64,
    debug_state: Once<debug_state::RuntimeState>,
    gic: Once<gic::Gic>,
}

impl Aarch64PlatformState {
    fn from_limine_mp(timer_frequency: u64) -> &'static Self {
        let response = MP_REQUEST
            .response()
            .unwrap_or_else(|| panic!("Limine did not provide an AArch64 MP response"));
        let cpus = response.cpus();
        assert!(
            !cpus.is_empty(),
            "Limine AArch64 MP response did not describe any CPU"
        );
        let bootstrap_mpidr = response.bsp_mpidr;
        let bootstrap = cpus
            .iter()
            .copied()
            .find(|cpu| cpu.mpidr == bootstrap_mpidr)
            .unwrap_or_else(|| {
                panic!("Limine AArch64 MP response omitted bootstrap MPIDR {bootstrap_mpidr:#x}")
            });
        let mut ordered = Vec::with_capacity(cpus.len());
        ordered.push(bootstrap);
        for &cpu in cpus {
            if cpu.mpidr != bootstrap_mpidr {
                ordered.push(cpu);
            }
        }
        let processors = ordered
            .into_iter()
            .enumerate()
            .map(|(index, mp_info)| {
                let logical_id = u16::try_from(index)
                    .unwrap_or_else(|_| panic!("AArch64 processor index {index} exceeds u16"));
                Aarch64ProcessorSlot {
                    mp_info,
                    runtime: ProcessorRuntime::new(logical_id),
                }
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Box::leak(Box::new(Self {
            processors,
            timer_frequency,
            debug_state: Once::new(),
            gic: Once::new(),
        }))
    }

    fn install_debug_state(&self, debug_state: debug_state::RuntimeState) {
        assert!(
            self.debug_state.get().is_none(),
            "AArch64 debug state was installed more than once"
        );
        self.debug_state.call_once(|| debug_state);
    }

    fn debug_state(&self) -> debug_state::RuntimeState {
        self.debug_state
            .get()
            .unwrap_or_else(|| panic!("AArch64 secondary processor started before debug state"))
            .clone()
    }

    fn install_gic(&self, gic: gic::Gic) -> &gic::Gic {
        assert!(
            self.gic.get().is_none(),
            "AArch64 interrupt controller was installed more than once"
        );
        self.gic.call_once(|| gic)
    }

    fn gic(&self) -> &gic::Gic {
        self.gic
            .get()
            .unwrap_or_else(|| panic!("AArch64 processor ran before the interrupt controller"))
    }

    fn bootstrap_mpidr(&self) -> u64 {
        self.processors[0].mp_info.mpidr
    }

    /// The MPIDR of every processor Limine started, bootstrap first.
    fn processor_mpidrs(&self) -> impl Iterator<Item = u64> + Clone + '_ {
        self.processors.iter().map(|slot| slot.mp_info.mpidr)
    }

    fn bootstrap_runtime(&'static self) -> &'static ProcessorRuntime {
        &self.processors[0].runtime
    }

    fn processor_count(&self) -> usize {
        self.processors.len()
    }

    fn processor_slot(&self, processor: ProcessorId) -> &Aarch64ProcessorSlot {
        self.processors
            .get(processor.id() as usize)
            .unwrap_or_else(|| panic!("AArch64 processor {} is out of range", processor.id()))
    }

    fn processor_slot_by_mpidr(&self, mpidr: u64) -> &Aarch64ProcessorSlot {
        self.processors
            .iter()
            .find(|slot| slot.mp_info.mpidr == mpidr)
            .unwrap_or_else(|| panic!("AArch64 processor MPIDR {mpidr:#x} is unknown"))
    }

    fn start_processor(&'static self, processor: ProcessorId) {
        assert!(
            processor.id() != 0,
            "AArch64 bootstrap processor cannot be started twice"
        );
        let slot = self.processor_slot(processor);
        assert!(
            !slot.runtime.started.load(Ordering::Acquire),
            "AArch64 processor {} was started more than once",
            processor.id()
        );
        slot.mp_info
            .bootstrap(aarch64_secondary_entry, self as *const _ as u64);
        let deadline = read_counter()
            .checked_add(self.timer_frequency / 2)
            .unwrap_or_else(|| panic!("AArch64 secondary startup deadline overflow"));
        while !slot.runtime.started.load(Ordering::Acquire) {
            assert!(
                read_counter() <= deadline,
                "AArch64 processor {} MPIDR {:#x} did not reach Rust startup",
                processor.id(),
                slot.mp_info.mpidr
            );
            core::hint::spin_loop();
        }
    }
}

fn aarch64_processor_count() -> usize {
    let response = MP_REQUEST
        .response()
        .unwrap_or_else(|| panic!("Limine did not provide an AArch64 MP response"));
    let processor_count = response.cpus().len();
    assert!(
        processor_count != 0,
        "Limine AArch64 MP response did not describe any CPU"
    );
    processor_count
}

struct Aarch64InterruptOps;

impl helios_hal::critical_section::InterruptOps for Aarch64InterruptOps {
    fn interrupts_enabled() -> bool {
        let daif: u64;
        unsafe {
            asm!("mrs {daif}, daif", daif = out(reg) daif, options(nomem, nostack, preserves_flags));
        }
        daif & (1 << 7) == 0
    }

    fn disable_interrupts() {
        mask_irq();
    }

    unsafe fn enable_interrupts() {
        // SAFETY: the caller owns the critical section this restores.
        unsafe { unmask_irq() };
    }

    fn current_identity() -> ProcessorIdentity {
        match NonZeroUsize::new(read_processor_runtime()) {
            Some(runtime) => ProcessorIdentity::from_raw(runtime),
            // A processor takes critical sections from its first instruction,
            // well before it writes `tpidr_el1`, and several secondaries run
            // that prologue concurrently. `MPIDR_EL1` is unique and readable
            // throughout, so it stands in until the runtime address exists;
            // one shared value would make a second processor's acquire look
            // like the first processor's nested re-acquire.
            None => ProcessorIdentity::bootstrapping(read_mpidr_affinity()),
        }
    }
}

struct Aarch64CriticalSection;

critical_section::set_impl!(Aarch64CriticalSection);

unsafe impl critical_section::Impl for Aarch64CriticalSection {
    unsafe fn acquire() -> usize {
        unsafe { CRITICAL_SECTION_STATE.acquire::<Aarch64InterruptOps>() }
    }

    unsafe fn release(restore_state: usize) {
        unsafe { CRITICAL_SECTION_STATE.release::<Aarch64InterruptOps>(restore_state) }
    }
}

#[unsafe(no_mangle)]
extern "C" fn aarch64_kernel_main() -> ! {
    assert!(
        BASE_REVISION.is_supported() || BASE_REVISION.actual_revision() == Some(6),
        "Limine bootloader does not support the required base protocol revision"
    );
    install_exception_vectors();
    let handoff = limine_boot_handoff();
    let physical_memory_offset = physical_memory_offset();
    // The firmware description comes up before anything else, because
    // the console is part of it and a panic from here on should reach a
    // serial port. Reading the console out of it allocates nothing, in
    // either description.
    let tables = platform::PlatformTables::discover(&handoff)
        .unwrap_or_else(|error| panic!("AArch64 platform discovery failed: {error}"));
    let console = tables
        .console()
        .unwrap_or_else(|error| panic!("AArch64 console discovery failed: {error}"));
    let debug_serial = DebugSerial::map(console, physical_memory_offset, &handoff);
    debug_serial.init();
    DEBUG_SERIAL_BASE.store(debug_serial.base, Ordering::Release);
    let processor_count = aarch64_processor_count();
    let reserved_ranges = boot_reserved_ranges(&handoff);
    let memory_regions = boot_memory_regions(&handoff, physical_memory_offset, &reserved_ranges);
    helios_kernel::prime_bootstrap_allocator(memory_regions, processor_count);
    // The rest of the description needs the heap: the ACPI path has to
    // interpret AML to reach a device's `_CRS`.
    let platform = tables
        .describe(console)
        .unwrap_or_else(|error| panic!("AArch64 platform description failed: {error}"));
    vmm::install_user_address_space(physical_memory_offset);
    let platform_state = Aarch64PlatformState::from_limine_mp(timer_frequency());
    activate_processor_runtime(platform_state.bootstrap_runtime());

    let cpu = Aarch64Cpu {
        state: platform_state,
    };
    let debug_state = debug_state::RuntimeState::new(
        cpu.timer_frequency(),
        cpu.processor_count(),
        cpu.now().ticks(),
    );
    platform_state.install_debug_state(debug_state.clone());
    let console = helios_kernel::RecordingConsole::new(
        debug_state.clone(),
        read_counter,
        Some(write_debug_serial_bytes),
    );
    let mut devices = DeviceInventory::new().with_debug_serial();
    if host_fs::has_9p_device(&platform) {
        devices = devices.with_host_share();
    }
    if net::has_network_device(&platform) {
        devices = devices.with_network();
    }
    if entropy::has_entropy_device(&platform) {
        devices = devices.with_entropy_device();
    }
    if balloon::has_balloon_device(&platform) {
        devices = devices.with_memory_balloon();
    }
    if vsock::has_vsock_device(&platform) {
        devices = devices.with_vsock();
    }
    let block_device_count = block::count_block_devices(&platform);
    if block_device_count != 0 {
        devices = devices.with_block_devices(block_device_count);
    }
    let kernel = helios_kernel::init(
        Platform::new(console, core::iter::empty::<MemoryRegion>(), cpu)
            .with_topology(
                ProcessorTopology::start_all_secondaries(
                    cpu.bootstrap_processor(),
                    cpu.processor_count(),
                )
                .with_startup_policy(ProcessorStartupPolicy::BootstrapOnly),
            )
            .with_timer_frequency_hz(cpu.timer_frequency())
            .with_dma_model(DmaModel::Translated)
            .with_devices(devices),
    );
    tracing::info!(
        "platform described by {} console={:#x} gicd={:#x} gicr={:#x} virtio-slots={} rtc={}",
        platform.source,
        platform.console.region.base,
        platform.gic.distributor.base,
        platform.gic.redistributor.base,
        platform.virtio.len(),
        platform.rtc.is_some(),
    );
    // The entropy device comes up before the root DRBG is seeded,
    // because on this platform it can be the only source there is: an
    // ACPI-described machine has no `/chosen/rng-seed`, and a processor
    // without `FEAT_RNG` has no `RNDR`. Its interrupt is routed later,
    // with every other device's; the bring-up read polls the used ring
    // and needs none.
    let entropy_device = entropy::bring_up(&platform, physical_memory_offset, &handoff);
    // The root DRBG is seeded before any component can ask for random
    // bytes: `RNDR` where the processor implements it, whatever seed
    // the bootloader left behind, and a read of the entropy device.
    // None is a fallback for another. This follows `init` so the source
    // line reaches the log the kernel just installed.
    let root_entropy = helios_kernel::seed_root_entropy(
        &cpu,
        platform.boot_entropy_seed,
        entropy_device.as_ref().map(|entropy| &entropy.device),
    );
    debug_state.install_root_entropy(root_entropy.clone());
    // The calendar is read once, here, before any component can ask
    // what time it is. The processor's timer carries wall time forward
    // from that reading; nothing re-synchronises it afterwards.
    match platform.rtc {
        Some(region) => {
            let rtc = rtc::map(region, physical_memory_offset, &handoff);
            debug_state.seed_wall_clock(cpu.now().ticks(), &rtc);
        }
        None => {
            tracing::warn!("the platform describes no PL031; the wall clock reads as uptime");
        }
    }
    let gic = platform_state.install_gic(gic::Gic::new(
        &platform.gic,
        platform_state.processor_mpidrs(),
        platform_state.bootstrap_mpidr(),
        physical_memory_offset,
        &handoff,
    ));
    gic.attach_current_processor(platform_state.bootstrap_mpidr());

    let mut routes = DeviceInterruptRoutes::new();
    if let Some(host_fs) = host_fs::install(
        &cpu,
        &platform,
        physical_memory_offset,
        &handoff,
        &debug_state,
    ) {
        gic.enable_device_interrupt(
            host_fs.interrupt,
            host_fs.trigger,
            platform_state.bootstrap_mpidr(),
        );
        routes.set_host_fs(host_fs.interrupt, host_fs.transport);
    }
    if let Some(network) = net::install(
        &cpu,
        &kernel,
        &platform,
        physical_memory_offset,
        &handoff,
        &debug_state,
    ) {
        gic.enable_device_interrupt(
            network.interrupt,
            network.trigger,
            platform_state.bootstrap_mpidr(),
        );
        routes.add_network(network.interrupt, network.device);
    }
    if let Some(entropy) = entropy_device {
        entropy::install(&kernel, &entropy, root_entropy.clone());
        gic.enable_device_interrupt(
            entropy.interrupt,
            entropy.trigger,
            platform_state.bootstrap_mpidr(),
        );
        routes.set_entropy(entropy.interrupt, entropy.device);
    }
    if let Some(balloon) = balloon::install(&kernel, &platform, physical_memory_offset, &handoff) {
        gic.enable_device_interrupt(
            balloon.interrupt,
            balloon.trigger,
            platform_state.bootstrap_mpidr(),
        );
        debug_state.install_memory_balloon(balloon.handle);
        routes.set_balloon(balloon.interrupt, balloon.handler);
    }
    if let Some(vsock) = vsock::install(
        &kernel,
        &cpu,
        &platform,
        physical_memory_offset,
        &handoff,
        &debug_state,
    ) {
        gic.enable_device_interrupt(
            vsock.interrupt,
            vsock.trigger,
            platform_state.bootstrap_mpidr(),
        );
        routes.set_vsock(vsock.interrupt, vsock.device);
    }
    for block in block::install(
        &cpu,
        &kernel,
        &platform,
        physical_memory_offset,
        &handoff,
        &debug_state,
        root_entropy,
    ) {
        gic.enable_device_interrupt(
            block.interrupt,
            block.trigger,
            platform_state.bootstrap_mpidr(),
        );
        routes.add_block(block.interrupt, block.device);
    }

    let runtime = current_processor_runtime();
    runtime.install_device_interrupts(routes);
    runtime.install_timer(kernel.timer());
    if let Some(program_service) = helios_kernel::install_component_host_program_service(
        &kernel,
        &cpu,
        &debug_state,
        read_debug_serial,
        write_debug_serial_bytes,
    ) {
        runtime.install_program_service(program_service);
    }
    // SAFETY: this processor's CPU interface is initialised, its timer
    // and device routes are published, and every device SPI is routed
    // here, so an arriving interrupt now finds a handler.
    unsafe { unmask_irq() };

    for processor in helios_kernel::component_host_processors_to_start(
        cpu.processor_count(),
        cpu.bootstrap_processor(),
    ) {
        cpu.start_processor(processor);
    }

    helios_kernel::run_component_host_processor_forever(
        cpu,
        kernel,
        debug_state,
        read_debug_serial,
        write_debug_serial_bytes,
    );
}

#[derive(Clone, Copy)]
pub struct Aarch64Cpu {
    state: &'static Aarch64PlatformState,
}

impl Cpu for Aarch64Cpu {
    fn current_processor(&self) -> ProcessorId {
        current_processor_runtime().logical_id()
    }

    fn processor_count(&self) -> usize {
        self.state.processor_count()
    }

    fn bootstrap_processor(&self) -> ProcessorId {
        ProcessorId::new(0)
    }

    fn park_current(&self) {
        let runtime = current_processor_runtime();
        let daif = read_daif();
        // Masking IRQ around the check and the wait is what makes the
        // pair race-free: a wake SGI that arrives after the flag test
        // stays pending rather than being taken and forgotten, and WFI
        // completes on a pending interrupt even while PSTATE.I masks
        // it. Restoring DAIF afterwards then delivers it. WFE is not an
        // option here: under HVF it returns immediately and the park
        // becomes a spin that burns a whole host core per processor.
        mask_irq();
        if !runtime.wake_pending.swap(false, Ordering::AcqRel) {
            unsafe {
                asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }
        // SAFETY: restores exactly the mask state the caller had.
        unsafe { write_daif(daif) };
    }

    fn start_processor(&self, processor: ProcessorId) {
        self.state.start_processor(processor);
    }

    fn wake_processor(&self, processor: ProcessorId) {
        let slot = self.state.processor_slot(processor);
        // Publish before signalling: `park_current` masks interrupts
        // around its own test, so a target that is on its way into
        // `wfi` either sees this store or takes the pending SGI.
        slot.runtime.wake_pending.store(true, Ordering::Release);
        gic::send_wake(slot.mp_info.mpidr);
    }

    fn now(&self) -> Instant {
        Instant::new(read_counter())
    }

    fn timer_frequency(&self) -> u64 {
        self.state.timer_frequency
    }

    fn hardware_perf_counters(&self) -> HardwarePerfCounters {
        HardwarePerfCounters {
            reference_cycles: Some(read_counter()),
            cpu_cycles: aarch64_pmu_supported().then(read_cycle_counter),
            instructions_retired: None,
        }
    }

    fn set_deadline(&self, deadline: Instant) {
        unsafe {
            asm!("msr cntv_cval_el0, {deadline}", deadline = in(reg) deadline.ticks(), options(nomem, nostack, preserves_flags));
            asm!("msr cntv_ctl_el0, {ctl}", ctl = in(reg) 1_u64, options(nomem, nostack, preserves_flags));
        }
    }

    fn publish_executable(&self, ptr: *const u8, len: usize) {
        if len == 0 {
            return;
        }
        let start = ptr as usize;
        let end = start
            .checked_add(len)
            .unwrap_or_else(|| panic!("AArch64 executable publish range overflow"));
        let mut address = start & !(cache_line_bytes() - 1);
        while address < end {
            unsafe {
                asm!("dc cvau, {addr}", addr = in(reg) address, options(nostack, preserves_flags));
            }
            address = address
                .checked_add(cache_line_bytes())
                .unwrap_or_else(|| panic!("AArch64 data-cache clean iteration overflow"));
        }
        unsafe {
            asm!("dsb ish", options(nostack, preserves_flags));
        }
        let mut address = start & !(cache_line_bytes() - 1);
        while address < end {
            unsafe {
                asm!("ic ivau, {addr}", addr = in(reg) address, options(nostack, preserves_flags));
            }
            address = address.checked_add(cache_line_bytes()).unwrap_or_else(|| {
                panic!("AArch64 instruction-cache invalidate iteration overflow")
            });
        }
        unsafe {
            asm!("dsb ish", options(nostack, preserves_flags));
            asm!("isb", options(nostack, preserves_flags));
        }
        vmm::publish_code_memory(ptr, len);
    }

    fn unpublish_executable(&self, ptr: *const u8, len: usize) {
        vmm::unpublish_code_memory(ptr, len);
    }

    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
        Some(detect_aarch64_native_feature)
    }

    fn has_lazy_commit_virtual_memory(&self) -> bool {
        true
    }

    unsafe fn zero_memory(&self, ptr: NonNull<u8>, size: usize) {
        // SAFETY: caller has guaranteed the buffer is writable; the
        // helper uses `dc zva` for cache-line aligned blocks and
        // `write_bytes` for the unaligned remainder.
        unsafe {
            aarch64_zero_memory(ptr.as_ptr(), size);
        }
    }

    /// The processor's own entropy source. The seed firmware leaves in
    /// the device tree is a separate source that the kernel mixes into
    /// its root DRBG; it is deliberately not laundered through here,
    /// which would make one a fallback for the other.
    fn fill_entropy(&self, buffer: &mut [u8]) -> Result<EntropyQuality, EntropyUnavailable> {
        fill_with_rndr(buffer)?;
        Ok(EntropyQuality::Cryptographic)
    }

    fn shutdown(&self) -> ! {
        loop {
            self.park_current();
        }
    }

    fn reboot(&self) -> ! {
        panic!("AArch64 reboot requires PSCI system_reset")
    }
}

impl ProcessorRuntime {
    fn logical_id(&self) -> ProcessorId {
        ProcessorId::new(self.logical_id)
    }

    fn install_timer(&self, timer: Timer<Aarch64Cpu>) {
        assert!(
            self.timer.get().is_none(),
            "AArch64 processor timer was installed more than once"
        );
        self.timer.call_once(|| timer);
    }

    fn install_program_service(&self, program_service: debug_state::ProgramService) {
        assert!(
            self.program_service.get().is_none(),
            "AArch64 program service was installed more than once"
        );
        self.program_service.call_once(|| program_service);
    }

    fn install_device_interrupts(&self, routes: DeviceInterruptRoutes) {
        assert!(
            self.device_interrupts.get().is_none(),
            "AArch64 device interrupt routes were installed more than once"
        );
        self.device_interrupts.call_once(|| routes);
    }
}

fn activate_processor_runtime(runtime: &'static ProcessorRuntime) {
    let ptr = runtime as *const _ as usize;
    let exception_stack_top = ptr
        .checked_add(EXCEPTION_STACK_BYTES)
        .unwrap_or_else(|| panic!("AArch64 exception stack top overflow"));
    unsafe {
        asm!("msr tpidr_el1, {ptr}", ptr = in(reg) ptr, options(nomem, nostack, preserves_flags));
        // The kernel never leaves EL1 and always runs on SP_EL1, so
        // SP_EL0 is free to hold the stack the IRQ entry switches to.
        asm!("msr sp_el0, {top}", top = in(reg) exception_stack_top, options(nomem, nostack, preserves_flags));
    }
    cache_dc_zva_block_bytes(runtime);
    runtime.started.store(true, Ordering::Release);
}

/// Reads `DCZID_EL0` once for this PE and stores the byte block size
/// for `DC ZVA` in `runtime.dc_zva_block_bytes`. A zero value means
/// the architecture prohibits DC ZVA on this PE (`DCZID_EL0.DZP == 1`)
/// and callers must fall through to a generic memset.
fn cache_dc_zva_block_bytes(runtime: &ProcessorRuntime) {
    let dczid: u64;
    unsafe {
        asm!(
            "mrs {v}, dczid_el0",
            v = out(reg) dczid,
            options(nomem, nostack, preserves_flags),
        );
    }
    let dzp_prohibited = (dczid >> 4) & 1 != 0;
    let block_bytes = if dzp_prohibited {
        0
    } else {
        let bs = (dczid & 0xf) as u32;
        4u32 << bs
    };
    runtime
        .dc_zva_block_bytes
        .store(block_bytes, Ordering::Relaxed);
}

/// Zero `size` bytes starting at `ptr` using `DC ZVA` for the
/// largest aligned middle region. The unaligned head and tail use a
/// scalar `write_bytes` that the compiler lowers to `stp`/`str`
/// pairs.
///
/// # Safety
///
/// `ptr` must point to `size` writable bytes that no other thread
/// observes for the duration of the call. `block_bytes` must be the
/// active processor's `DCZID_EL0` block size in bytes and a power of
/// two.
unsafe fn dc_zva_zero(mut ptr: *mut u8, mut size: usize, block_bytes: usize) {
    debug_assert!(block_bytes.is_power_of_two());
    let block_mask = block_bytes - 1;
    let misalignment = (ptr as usize) & block_mask;
    if misalignment != 0 {
        let prefix = (block_bytes - misalignment).min(size);
        unsafe {
            core::ptr::write_bytes(ptr, 0, prefix);
            ptr = ptr.add(prefix);
        }
        size -= prefix;
    }
    while size >= block_bytes {
        unsafe {
            asm!("dc zva, {p}", p = in(reg) ptr, options(nostack, preserves_flags));
            ptr = ptr.add(block_bytes);
        }
        size -= block_bytes;
    }
    if size > 0 {
        unsafe {
            core::ptr::write_bytes(ptr, 0, size);
        }
    }
}

/// Aarch64-specific zero-fill that consults the per-PE
/// `dc_zva_block_bytes` cache and falls back to generic memset when
/// DC ZVA is not available on the current PE.
///
/// # Safety
///
/// `ptr` must point to `size` writable bytes; the active processor
/// must have completed `activate_processor_runtime` so the cached
/// block size is published.
pub(crate) unsafe fn aarch64_zero_memory(ptr: *mut u8, size: usize) {
    let runtime = current_processor_runtime();
    let block_bytes = runtime.dc_zva_block_bytes.load(Ordering::Relaxed) as usize;
    if block_bytes == 0 {
        unsafe {
            core::ptr::write_bytes(ptr, 0, size);
        }
        return;
    }
    unsafe {
        dc_zva_zero(ptr, size, block_bytes);
    }
}

/// Entry point Limine jumps to on a secondary processor.
///
/// Limine hands the processor over running on `SP_EL0`, while `_start`
/// put the bootstrap processor on `SP_EL1`. The kernel needs one stack
/// selection on every processor: exception entry always switches to
/// `SP_EL1`, and the IRQ path claims `SP_EL0` for its own stack, which
/// is `UNDEFINED` to access at EL1 while it is the selected one. Move
/// Limine's stack under `SP_EL1` and select it before any Rust code
/// runs.
#[unsafe(naked)]
unsafe extern "C" fn aarch64_secondary_entry(mp_info: &MpInfo) -> ! {
    core::arch::naked_asm!(
        "mov x9, sp",
        "msr spsel, #1",
        "mov sp, x9",
        "b {main}",
        main = sym aarch64_secondary_main,
    )
}

unsafe extern "C" fn aarch64_secondary_main(mp_info: &MpInfo) -> ! {
    prepare_current_processor();
    let state = mp_info.extra_argument() as *const Aarch64PlatformState;
    assert!(
        !state.is_null(),
        "AArch64 secondary processor started without a platform state pointer"
    );
    let state = unsafe { &*state };
    let slot = state.processor_slot_by_mpidr(mp_info.mpidr);
    activate_processor_runtime(&slot.runtime);
    state.gic().attach_current_processor(mp_info.mpidr);

    let cpu = Aarch64Cpu { state };
    let debug_state = state.debug_state();
    let console = helios_kernel::RecordingConsole::new(
        debug_state.clone(),
        read_counter,
        Some(write_debug_serial_bytes),
    );
    let kernel = helios_kernel::init(
        Platform::new(console, core::iter::empty::<MemoryRegion>(), cpu)
            .with_timer_frequency_hz(cpu.timer_frequency())
            .with_dma_model(DmaModel::Translated)
            .with_devices(DeviceInventory::new().with_debug_serial()),
    );
    let runtime = current_processor_runtime();
    runtime.install_timer(kernel.timer());
    if let Some(program_service) = helios_kernel::install_component_host_program_service(
        &kernel,
        &cpu,
        &debug_state,
        read_debug_serial,
        write_debug_serial_bytes,
    ) {
        runtime.install_program_service(program_service);
    }
    // SAFETY: this processor's CPU interface is initialised and its
    // timer is published; device interrupts are routed to the
    // bootstrap processor, so only private interrupts arrive here.
    unsafe { unmask_irq() };
    helios_kernel::run_component_host_processor_forever(
        cpu,
        kernel,
        debug_state,
        read_debug_serial,
        write_debug_serial_bytes,
    );
}

fn prepare_current_processor() {
    unsafe {
        asm!(
            "msr daifset, #0xf",
            options(nomem, nostack, preserves_flags)
        );
        let cpacr: u64;
        asm!("mrs {cpacr}, cpacr_el1", cpacr = out(reg) cpacr, options(nomem, nostack, preserves_flags));
        let cpacr = cpacr | (0x3 << 20);
        asm!("msr cpacr_el1, {cpacr}", cpacr = in(reg) cpacr, options(nomem, nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
    enable_pmu_for_current_processor();
    install_exception_vectors();
}

fn install_exception_vectors() {
    let vectors = &raw const __helios_aarch64_exception_vectors as usize;
    unsafe {
        asm!("msr vbar_el1, {vectors}", vectors = in(reg) vectors, options(nomem, nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

#[unsafe(no_mangle)]
extern "C" fn aarch64_handle_sync_exception(
    esr_el1: usize,
    elr_el1: usize,
    frame_pointer: usize,
    far_el1: usize,
) -> ! {
    let exception_class = (esr_el1 >> 26) & 0x3f;
    let (cause, faulting_address) = match exception_class {
        0b100000 | 0b100001 => (KernelExceptionCause::InstructionFault, Some(far_el1)),
        0b100100 | 0b100101 => (KernelExceptionCause::DataFault, Some(far_el1)),
        0b111100 => (KernelExceptionCause::Breakpoint, None),
        0b001110 => (KernelExceptionCause::IllegalInstruction, None),
        _ => (KernelExceptionCause::IllegalInstruction, None),
    };

    let runtime = read_processor_runtime();
    if runtime != 0 {
        let handler = unsafe {
            (*(runtime as *const ProcessorRuntime))
                .native_trap_handler
                .load(Ordering::Acquire)
        };
        if handler != 0 {
            let handler: KernelNativeTrapHandler = unsafe { core::mem::transmute(handler) };
            let exception = KernelException {
                cause,
                instruction_pointer: elr_el1,
                frame_pointer,
                faulting_address,
            };
            let _ = exception.dispatch_to(handler);
        }
    }

    panic!(
        "unhandled AArch64 synchronous exception ec={exception_class:#x} esr={esr_el1:#x} elr={elr_el1:#x} far={far_el1:#x}"
    )
}

/// IRQ entry point for every exception level slot, called from
/// `entry.S` on the per-processor exception stack with `PSTATE.I` still
/// set, so the handler never nests.
///
/// The CPU interface signals one interrupt at a time; the loop drains
/// every pending one before returning so a level-triggered device that
/// re-asserts while its handler runs is picked up in the same entry.
#[unsafe(no_mangle)]
extern "C" fn aarch64_handle_irq() {
    let runtime = current_processor_runtime();
    while let Some(intid) = gic::acknowledge_interrupt() {
        if intid == gic::WAKE_SGI {
            // The wake carries no payload: returning from `wfi` is the
            // whole message, and `park_current` owns the flag.
        } else if intid == gic::VIRTUAL_TIMER_PPI {
            if let Some(program_service) = runtime.program_service.get() {
                program_service.increment_epoch();
            }
            runtime
                .timer
                .get()
                .unwrap_or_else(|| {
                    panic!("AArch64 timer interrupt fired before the kernel timer was installed")
                })
                .handle_interrupt();
        } else {
            let routes = runtime.device_interrupts.get().unwrap_or_else(|| {
                panic!("AArch64 device interrupt {intid:?} fired before routes were installed")
            });
            assert!(
                routes.route(intid),
                "AArch64 device interrupt {intid:?} has no registered handler"
            );
        }
        gic::end_interrupt(intid);
    }
}

/// Masks IRQ delivery on the calling processor. Debug, SError and FIQ
/// keep the mask the boot path gave them: they are fatal here, and a
/// critical section that unmasked them on exit would enable more than
/// it disabled.
fn mask_irq() {
    unsafe {
        asm!(
            "msr daifset, #0x2",
            options(nomem, nostack, preserves_flags)
        );
    }
}

fn read_daif() -> u64 {
    let daif: u64;
    unsafe {
        asm!("mrs {daif}, daif", daif = out(reg) daif, options(nomem, nostack, preserves_flags));
    }
    daif
}

/// Restores a previously read `DAIF`.
///
/// # Safety
///
/// `daif` must come from [`read_daif`] on this processor, so the call
/// restores a mask state the caller established rather than inventing
/// one.
unsafe fn write_daif(daif: u64) {
    unsafe {
        asm!("msr daif, {daif}", daif = in(reg) daif, options(nomem, nostack, preserves_flags));
    }
}

/// Unmasks IRQ delivery on the calling processor.
///
/// # Safety
///
/// The calling processor's GIC CPU interface must be initialised and
/// every interrupt routed to it must have a handler installed.
unsafe fn unmask_irq() {
    unsafe {
        asm!(
            "msr daifclr, #0x2",
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// The affinity fields of `MPIDR_EL1`, which name this processor uniquely on
/// every AArch64 implementation.
fn read_mpidr_affinity() -> usize {
    let mpidr: u64;
    unsafe {
        asm!("mrs {mpidr}, mpidr_el1", mpidr = out(reg) mpidr, options(nomem, nostack, preserves_flags));
    }
    const AFFINITY_0_TO_2: u64 = 0x00ff_ffff;
    const AFFINITY_3: u64 = 0xff << 32;
    let affinity = (mpidr & AFFINITY_0_TO_2) | ((mpidr & AFFINITY_3) >> 8);
    usize::try_from(affinity).expect("AArch64 MPIDR affinity does not fit usize")
}

fn read_processor_runtime() -> usize {
    let ptr: usize;
    unsafe {
        asm!("mrs {ptr}, tpidr_el1", ptr = out(reg) ptr, options(nomem, nostack, preserves_flags));
    }
    ptr
}

fn current_processor_runtime() -> &'static ProcessorRuntime {
    let ptr = read_processor_runtime() as *const ProcessorRuntime;
    assert!(
        !ptr.is_null(),
        "AArch64 processor runtime was not installed before use"
    );
    unsafe { &*ptr }
}

fn detect_aarch64_native_feature(feature: &str) -> Option<bool> {
    match feature {
        "lse" => Some(read_id_aa64isar0_el1() & 0xf00_000 != 0),
        "fp16" => Some(aarch64_id_field(read_id_aa64pfr0_el1(), 16) == 1),
        "neon" => Some(true),
        _ => None,
    }
}

fn aarch64_id_field(register: u64, shift: u8) -> u64 {
    (register >> shift) & 0xf
}

fn read_id_aa64isar0_el1() -> u64 {
    let value: u64;
    unsafe {
        asm!("mrs {value}, id_aa64isar0_el1", value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn read_id_aa64pfr0_el1() -> u64 {
    let value: u64;
    unsafe {
        asm!("mrs {value}, id_aa64pfr0_el1", value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn read_id_aa64dfr0_el1() -> u64 {
    let value: u64;
    unsafe {
        asm!("mrs {value}, id_aa64dfr0_el1", value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn aarch64_pmu_supported() -> bool {
    let pmuver = (read_id_aa64dfr0_el1() >> 8) & 0xf;
    pmuver != 0 && pmuver != 0xf
}

fn enable_pmu_for_current_processor() {
    if !aarch64_pmu_supported() {
        return;
    }
    unsafe {
        let enable_reset_and_64bit_cycle_counter = (1_u64 << 0) | (1_u64 << 2) | (1_u64 << 6);
        asm!("msr pmcr_el0, {value}", value = in(reg) enable_reset_and_64bit_cycle_counter, options(nomem, nostack, preserves_flags));
        asm!("msr pmcntenset_el0, {value}", value = in(reg) 1_u64 << 31, options(nomem, nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

fn read_cycle_counter() -> u64 {
    let value: u64;
    unsafe {
        asm!("mrs {value}, pmccntr_el0", value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn rndr_supported() -> bool {
    (read_id_aa64isar0_el1() >> 60) & 0xf != 0
}

fn fill_with_rndr(buffer: &mut [u8]) -> Result<(), EntropyUnavailable> {
    if !rndr_supported() {
        return Err(EntropyUnavailable);
    }

    for chunk in buffer.chunks_mut(core::mem::size_of::<u64>()) {
        let value = read_rndr()?;
        let bytes = value.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    Ok(())
}

fn read_rndr() -> Result<u64, EntropyUnavailable> {
    let value: u64;
    let failed: u64;
    unsafe {
        asm!(
            "mrs {value}, S3_3_C2_C4_0",
            "cset {failed}, eq",
            value = out(reg) value,
            failed = out(reg) failed,
            options(nomem, nostack)
        );
    }
    if failed == 0 {
        Ok(value)
    } else {
        Err(EntropyUnavailable)
    }
}

fn read_counter() -> u64 {
    let value: u64;
    unsafe {
        asm!("mrs {value}, cntvct_el0", value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn timer_frequency() -> u64 {
    let value: u64;
    unsafe {
        asm!("mrs {value}, cntfrq_el0", value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    assert!(value != 0, "AArch64 CNTFRQ_EL0 reported zero");
    value
}

fn cache_line_bytes() -> usize {
    let ctr_el0: u64;
    unsafe {
        asm!("mrs {ctr_el0}, ctr_el0", ctr_el0 = out(reg) ctr_el0, options(nomem, nostack, preserves_flags));
    }
    let log2_words = ((ctr_el0 >> 16) & 0xf) as usize;
    4 << log2_words
}

fn kernel_virtual_to_physical(virtual_address: usize, handoff: &LimineBootHandoff) -> usize {
    let virtual_base = usize::try_from(handoff.kernel.virtual_base)
        .unwrap_or_else(|_| panic!("AArch64 kernel virtual base does not fit usize"));
    let physical_base = usize::try_from(handoff.kernel.physical_base)
        .unwrap_or_else(|_| panic!("AArch64 kernel physical base does not fit usize"));
    assert!(
        virtual_address >= virtual_base,
        "AArch64 kernel virtual address precedes the kernel virtual base"
    );
    physical_base
        .checked_add(virtual_address - virtual_base)
        .unwrap_or_else(|| panic!("AArch64 kernel virtual to physical translation overflow"))
}

fn read_ttbr1_el1() -> usize {
    let value: usize;
    unsafe {
        asm!("mrs {value}, ttbr1_el1", value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value & PAGE_ADDRESS_MASK as usize
}

fn install_device_memory_attribute() {
    let mair: u64;
    unsafe {
        asm!("mrs {mair}, mair_el1", mair = out(reg) mair, options(nomem, nostack, preserves_flags));
    }
    let mask = 0xff << MAIR_DEVICE_ATTR_INDEX_SHIFT;
    let mair = (mair & !mask) | (MAIR_DEVICE_NGNRNE << MAIR_DEVICE_ATTR_INDEX_SHIFT);
    unsafe {
        asm!("msr mair_el1, {mair}", mair = in(reg) mair, options(nomem, nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

fn map_mmio_page(
    physical_address: usize,
    physical_memory_offset: usize,
    handoff: &LimineBootHandoff,
) {
    install_device_memory_attribute();
    let virtual_address = mmio_virtual_base(physical_address, physical_memory_offset);
    let block_virtual = virtual_address & !(MMIO_BLOCK_BYTES - 1);
    let block_physical = physical_address & !(MMIO_BLOCK_BYTES - 1);
    let root = table_from_physical(read_ttbr1_el1(), physical_memory_offset);
    let backing_tables = [&MMIO_PAGE_TABLE_0, &MMIO_PAGE_TABLE_1, &MMIO_PAGE_TABLE_2];
    let mut next_backing_table = 0usize;
    let mut table = root;
    let indexes = [
        (block_virtual >> 39) & PAGE_TABLE_INDEX_MASK,
        (block_virtual >> 30) & PAGE_TABLE_INDEX_MASK,
        (block_virtual >> 21) & PAGE_TABLE_INDEX_MASK,
    ];
    for &index in &indexes[..2] {
        table = ensure_next_page_table(
            table,
            index,
            &backing_tables,
            &mut next_backing_table,
            handoff,
            physical_memory_offset,
        );
    }
    let entry = unsafe { table.add(indexes[2]).read_volatile() };
    if entry & PAGE_VALID != 0 {
        assert!(
            entry & PAGE_TABLE_DESCRIPTOR != PAGE_TABLE_DESCRIPTOR,
            "AArch64 early MMIO mapper found a page table where a block mapping was required"
        );
        assert!(
            (entry & PAGE_ADDRESS_MASK) as usize == block_physical,
            "AArch64 early MMIO mapper found a conflicting block mapping"
        );
        return;
    }
    let descriptor = block_physical as u64
        | (PAGE_ATTR_DEVICE_INDEX << PAGE_ATTR_INDEX_SHIFT)
        | PAGE_AF
        | PAGE_PXN
        | PAGE_UXN
        | BLOCK_DESCRIPTOR;
    unsafe {
        table.add(indexes[2]).write_volatile(descriptor);
        asm!("dsb ishst", options(nostack, preserves_flags));
        let va_page = block_virtual >> 12;
        asm!("tlbi vae1is, {va_page}", va_page = in(reg) va_page, options(nostack, preserves_flags));
        asm!("dsb ish", options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

fn map_mmio_range(
    physical_base: usize,
    size: usize,
    physical_memory_offset: usize,
    handoff: &LimineBootHandoff,
) {
    assert!(size != 0, "AArch64 MMIO range has zero size");
    let end = physical_base
        .checked_add(size)
        .unwrap_or_else(|| panic!("AArch64 MMIO range overflow"));
    let mut block = physical_base & !(MMIO_BLOCK_BYTES - 1);
    while block < end {
        map_mmio_page(block, physical_memory_offset, handoff);
        block = block
            .checked_add(MMIO_BLOCK_BYTES)
            .unwrap_or_else(|| panic!("AArch64 MMIO mapping iteration overflow"));
    }
}

fn ensure_next_page_table(
    table: *mut u64,
    index: usize,
    backing_tables: &[&PageTable],
    next_backing_table: &mut usize,
    handoff: &LimineBootHandoff,
    physical_memory_offset: usize,
) -> *mut u64 {
    let entry = unsafe { table.add(index).read_volatile() };
    if entry & 0b11 == PAGE_TABLE_DESCRIPTOR {
        return table_from_physical((entry & PAGE_ADDRESS_MASK) as usize, physical_memory_offset);
    }
    assert!(
        entry & PAGE_VALID == 0,
        "AArch64 early MMIO mapper cannot split an existing block descriptor"
    );
    assert!(
        *next_backing_table < backing_tables.len(),
        "AArch64 early MMIO mapper ran out of page-table pages"
    );
    let backing_table = backing_tables[*next_backing_table];
    *next_backing_table += 1;
    backing_table.clear();
    let descriptor = backing_table.physical_address(handoff) as u64 | PAGE_TABLE_DESCRIPTOR;
    unsafe {
        table.add(index).write_volatile(descriptor);
        asm!("dsb ishst", options(nostack, preserves_flags));
    }
    backing_table.as_mut_ptr()
}

fn table_from_physical(physical_address: usize, physical_memory_offset: usize) -> *mut u64 {
    physical_address
        .checked_add(physical_memory_offset)
        .unwrap_or_else(|| panic!("AArch64 page-table virtual address overflow")) as *mut u64
}

fn fdt_cells_to_usize(bytes: &[u8], name: &str) -> usize {
    assert!(
        bytes.len() == 4 || bytes.len() == 8,
        "{name} must contain one or two 32-bit cells, got {} bytes",
        bytes.len()
    );
    let mut value = 0usize;
    for cell in bytes.chunks_exact(4) {
        value = value
            .checked_shl(32)
            .unwrap_or_else(|| panic!("{name} cell shift overflow"))
            | u32::from_be_bytes(
                cell.try_into()
                    .unwrap_or_else(|_| panic!("{name} cell had invalid width")),
            ) as usize;
    }
    value
}

fn mmio_virtual_base(physical_base: usize, physical_memory_offset: usize) -> usize {
    physical_base
        .checked_add(physical_memory_offset)
        .unwrap_or_else(|| panic!("AArch64 MMIO virtual address overflow"))
}

fn matches_virtio_mmio_device(
    physical_base: usize,
    physical_memory_offset: usize,
    handoff: &LimineBootHandoff,
    expected: helios_virtio::DeviceType,
) -> bool {
    map_mmio_page(physical_base, physical_memory_offset, handoff);
    let virtual_base = mmio_virtual_base(physical_base, physical_memory_offset);
    unsafe { helios_virtio::mmio_device_matches(virtual_base, expected) }
}

/// Every transport slot the platform describes that answers as a
/// device of the expected type.
///
/// The platform describes where transports may be; only the register
/// window itself says what is actually plugged into one, so each slot
/// is mapped and probed here.
fn virtio_slots<'a>(
    platform: &'a platform::PlatformDescription,
    physical_memory_offset: usize,
    handoff: &'a LimineBootHandoff,
    expected: helios_virtio::DeviceType,
) -> impl Iterator<Item = platform::VirtioMmioSlot> + 'a {
    platform.virtio.iter().filter(move |slot| {
        matches_virtio_mmio_device(slot.region.base, physical_memory_offset, handoff, expected)
    })
}

fn count_virtio_mmio_devices(
    platform: &platform::PlatformDescription,
    expected: helios_virtio::DeviceType,
) -> usize {
    let handoff = limine_boot_handoff();
    let physical_memory_offset = physical_memory_offset();
    virtio_slots(platform, physical_memory_offset, &handoff, expected).count()
}

#[derive(Clone, Copy)]
struct DebugSerial {
    base: usize,
}

impl DebugSerial {
    /// Maps the console the platform describes.
    fn map(
        console: platform::ConsoleDescription,
        physical_memory_offset: usize,
        handoff: &LimineBootHandoff,
    ) -> Self {
        assert!(
            console.region.size != 0,
            "AArch64 console UART window has zero size"
        );
        map_mmio_page(console.region.base, physical_memory_offset, handoff);
        Self {
            base: mmio_virtual_base(console.region.base, physical_memory_offset),
        }
    }

    fn init(self) {}

    fn read_flag(self) -> u32 {
        unsafe { ((self.base + PL011_FLAG) as *const u32).read_volatile() }
    }

    fn try_read_byte(self) -> Option<u8> {
        if self.read_flag() & PL011_FLAG_RXFE != 0 {
            return None;
        }
        Some(unsafe { ((self.base + PL011_DATA) as *const u32).read_volatile() as u8 })
    }

    fn write_byte(self, byte: u8) {
        while self.read_flag() & PL011_FLAG_TXFF != 0 {
            core::hint::spin_loop();
        }
        unsafe {
            ((self.base + PL011_DATA) as *mut u32).write_volatile(u32::from(byte));
        }
    }
}

impl ByteSerial for DebugSerial {
    fn try_read_byte(&self) -> Option<u8> {
        (*self).try_read_byte()
    }

    fn write_bytes(&self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_byte(byte);
        }
    }
}

fn active_debug_serial() -> DebugSerial {
    let base = DEBUG_SERIAL_BASE.load(Ordering::Acquire);
    assert!(
        base != 0,
        "AArch64 debug serial was used before firmware discovery completed"
    );
    DebugSerial { base }
}

fn read_debug_serial(buffer: &mut alloc::vec::Vec<u8>, max_bytes: u32) {
    helios_kernel::try_read_serial(&active_debug_serial(), buffer, max_bytes);
}

fn write_debug_serial_bytes(bytes: &[u8]) {
    while DEBUG_SERIAL_WRITER_HELD
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    active_debug_serial().write_bytes(bytes);
    DEBUG_SERIAL_WRITER_HELD.store(false, Ordering::Release);
}

fn try_write_panic_serial_bytes(bytes: &[u8]) {
    let base = DEBUG_SERIAL_BASE.load(Ordering::Acquire);
    if base != 0 {
        DebugSerial { base }.write_bytes(bytes);
    }
}

fn physical_memory_offset() -> usize {
    HHDM_REQUEST
        .response()
        .unwrap_or_else(|| panic!("Limine did not provide an HHDM response"))
        .offset as usize
}

#[derive(Clone, Copy)]
struct LimineMemoryMap {
    entries: &'static [&'static Entry],
}

impl BootMemoryMap for LimineMemoryMap {
    type Iter = core::iter::Map<
        core::iter::Copied<core::slice::Iter<'static, &'static Entry>>,
        fn(&'static Entry) -> BootMemoryRegion,
    >;

    fn regions(&self) -> Self::Iter {
        self.entries.iter().copied().map(convert_memory_region)
    }
}

#[derive(Clone, Copy)]
struct LimineModules {
    files: &'static [&'static File],
}

impl BootModules<'static> for LimineModules {
    type Iter = core::iter::Map<
        core::iter::Copied<core::slice::Iter<'static, &'static File>>,
        fn(&'static File) -> BootModule<'static>,
    >;

    fn modules(&self) -> Self::Iter {
        self.files.iter().copied().map(convert_module)
    }
}

type LimineBootHandoff = BootHandoff<'static, LimineMemoryMap, LimineModules>;

fn limine_boot_handoff() -> LimineBootHandoff {
    let memory_map = LimineMemoryMap {
        entries: MEMORY_MAP_REQUEST
            .response()
            .unwrap_or_else(|| panic!("Limine did not provide a memory map response"))
            .entries(),
    };
    let executable_address = EXECUTABLE_ADDRESS_REQUEST
        .response()
        .unwrap_or_else(|| panic!("Limine did not provide an executable address response"));
    let executable_file = EXECUTABLE_FILE_REQUEST
        .response()
        .unwrap_or_else(|| panic!("Limine did not provide an executable file response"))
        .executable_file();
    let firmware = FIRMWARE_TYPE_REQUEST
        .response()
        .unwrap_or_else(|| panic!("Limine did not provide a firmware type response"))
        .firmware_type;
    let firmware = convert_firmware_kind(firmware);
    assert!(
        firmware == FirmwareKind::Uefi64,
        "AArch64 backend requires Limine UEFI64 boot, got {firmware:?}"
    );
    BootHandoff {
        memory_map,
        kernel: BootKernelImage {
            physical_base: executable_address.physical_base,
            virtual_base: executable_address.virtual_base,
            file_address: executable_file.data().as_ptr() as usize,
            size: executable_file.data().len() as u64,
        },
        command_line: EXECUTABLE_CMDLINE_REQUEST
            .response()
            .unwrap_or_else(|| panic!("Limine did not provide an executable command-line response"))
            .cmdline()
            .as_bytes(),
        modules: LimineModules {
            files: MODULE_REQUEST
                .response()
                .map(|response| response.modules())
                .unwrap_or(&[]),
        },
        firmware,
        tables: BootFirmwareTables {
            // Both descriptions are published when the firmware offers
            // them. `platform::PlatformTables` decides which one is
            // used; the handoff only reports what exists.
            //
            // Limine hands the RSDP over through its direct map at
            // every base revision this kernel accepts, and the handoff
            // carries the physical address, so the offset comes back
            // off here rather than being subtracted again by every
            // reader.
            acpi_rsdp: RSDP_REQUEST.response().map(|response| {
                (response.address as usize)
                    .checked_sub(physical_memory_offset())
                    .unwrap_or_else(|| {
                        panic!("Limine's RSDP address is below the direct map it was mapped into")
                    })
            }),
            device_tree_blob: DEVICE_TREE_BLOB_REQUEST
                .response()
                .map(|response| response.dtb_ptr as usize),
        },
    }
}

fn convert_memory_region(entry: &'static Entry) -> BootMemoryRegion {
    BootMemoryRegion {
        start: entry.base,
        end: entry.base + entry.length,
        kind: if entry.type_ == MEMMAP_USABLE {
            BootMemoryKind::Usable
        } else {
            BootMemoryKind::Reserved
        },
    }
}

fn convert_module(file: &'static File) -> BootModule<'static> {
    BootModule {
        address: file.data().as_ptr() as usize,
        size: file.data().len(),
        path: file.path().as_bytes(),
        command_line: file.cmdline().as_bytes(),
    }
}

fn convert_firmware_kind(firmware: u64) -> FirmwareKind {
    if firmware == FIRMWARE_TYPE_EFI32 {
        FirmwareKind::Uefi32
    } else if firmware == FIRMWARE_TYPE_EFI64 {
        FirmwareKind::Uefi64
    } else if firmware == FIRMWARE_TYPE_SBI {
        FirmwareKind::Sbi
    } else if firmware == FIRMWARE_TYPE_X86BIOS {
        panic!("Limine BIOS boot is not supported on AArch64")
    } else {
        panic!("Limine reported an unknown firmware type")
    }
}

#[derive(Clone, Debug)]
struct BootReservedRanges {
    ranges: [Option<Range<usize>>; 2],
}

impl BootReservedRanges {
    fn iter(&self) -> impl Iterator<Item = &Range<usize>> {
        self.ranges.iter().flatten()
    }
}

fn boot_reserved_ranges(handoff: &LimineBootHandoff) -> BootReservedRanges {
    let executable_bytes = align_up_usize(
        usize::try_from(handoff.kernel.size)
            .unwrap_or_else(|_| panic!("Limine executable file size does not fit usize")),
        PAGE_BYTES,
    );
    let loaded_executable_start = usize::try_from(handoff.kernel.physical_base)
        .unwrap_or_else(|_| panic!("Limine executable physical base does not fit usize"));
    let loaded_executable_end = loaded_executable_start
        .checked_add(executable_bytes)
        .unwrap_or_else(|| panic!("Limine executable loaded range overflow"));
    let file_start = handoff.kernel.file_address;
    let file_end = file_start
        .checked_add(executable_bytes)
        .unwrap_or_else(|| panic!("Limine executable file range overflow"));
    BootReservedRanges {
        ranges: [
            Some(loaded_executable_start..loaded_executable_end),
            Some(file_start..file_end),
        ],
    }
}

fn boot_memory_regions(
    handoff: &LimineBootHandoff,
    physical_memory_offset: usize,
    reserved_ranges: &BootReservedRanges,
) -> impl IntoIterator<Item = MemoryRegion> {
    handoff.memory_map.regions().flat_map(move |region| {
        usable_region_segments(region, reserved_ranges)
            .into_iter()
            .flatten()
            .map(move |segment| {
                let start = segment.start + physical_memory_offset;
                let len = segment.end - segment.start;
                let slice = core::ptr::slice_from_raw_parts_mut(start as *mut u8, len);
                core::ptr::NonNull::new(slice).unwrap_or_else(|| {
                    panic!("usable memory segment had a null start: {segment:?}")
                })
            })
    })
}

fn usable_region_segments(
    region: BootMemoryRegion,
    reserved_ranges: &BootReservedRanges,
) -> [Option<Range<usize>>; MAX_USABLE_REGION_SEGMENTS] {
    if !region.usable() || region.end <= region.start {
        return [const { None }; MAX_USABLE_REGION_SEGMENTS];
    }
    let mut segments = [const { None }; MAX_USABLE_REGION_SEGMENTS];
    segments[0] = Some(region.start as usize..region.end as usize);
    for reserved in reserved_ranges.iter() {
        segments = subtract_reserved_range(segments, reserved);
    }
    segments
}

fn subtract_reserved_range(
    segments: [Option<Range<usize>>; MAX_USABLE_REGION_SEGMENTS],
    reserved: &Range<usize>,
) -> [Option<Range<usize>>; MAX_USABLE_REGION_SEGMENTS] {
    let mut out = [const { None }; MAX_USABLE_REGION_SEGMENTS];
    let mut next = 0;
    for segment in segments.into_iter().flatten() {
        for piece in split_segment(segment, reserved).into_iter().flatten() {
            assert!(
                next < out.len(),
                "AArch64 boot memory segmentation exceeded fixed capacity"
            );
            out[next] = Some(piece);
            next += 1;
        }
    }
    out
}

fn split_segment(segment: Range<usize>, reserved: &Range<usize>) -> [Option<Range<usize>>; 2] {
    if reserved.end <= segment.start || reserved.start >= segment.end {
        return [Some(segment), None];
    }
    let left = (reserved.start > segment.start).then_some(segment.start..reserved.start);
    let right = (reserved.end < segment.end).then_some(reserved.end..segment.end);
    [left, right]
}

fn align_up_usize(value: usize, align: usize) -> usize {
    assert!(
        align.is_power_of_two(),
        "alignment must be a power of two, got {align}"
    );
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
        .unwrap_or_else(|| panic!("alignment overflow for value={value:#x}, align={align:#x}"))
}

struct PanicSerialWriter;

impl Write for PanicSerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        try_write_panic_serial_bytes(s.as_bytes());
        Ok(())
    }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let _ = writeln!(PanicSerialWriter, "{info}");
    helios_kernel::panic_log(info);
    loop {
        unsafe {
            asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_get(slot: usize) -> *mut u8 {
    let runtime = read_processor_runtime();
    if runtime == 0 {
        return core::ptr::null_mut();
    }
    unsafe {
        (*(runtime as *const ProcessorRuntime))
            .wasmtime_tls
            .get(slot)
    }
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(slot: usize, ptr: *mut u8) {
    current_processor_runtime().wasmtime_tls.set(slot, ptr);
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn wasmtime_init_traps(handler: helios_kernel::KernelNativeTrapHandler) -> i32 {
    current_processor_runtime()
        .native_trap_handler
        .store(handler as usize, Ordering::Release);
    0
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn wasmtime_parking_wait(timeout_nanos: u64) {
    if timeout_nanos == 0 {
        return;
    }
    if timeout_nanos == u64::MAX {
        unsafe {
            asm!("wfe", options(nomem, nostack, preserves_flags));
        }
        return;
    }

    let ticks = ((u128::from(timeout_nanos) * u128::from(timer_frequency())) / 1_000_000_000)
        .clamp(1, u128::from(u64::MAX)) as u64;
    let deadline = read_counter().saturating_add(ticks);
    while read_counter() < deadline {
        unsafe {
            asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

#[inline]
fn signal_waiting_processors() {
    unsafe {
        asm!("sev", options(nomem, nostack, preserves_flags));
    }
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn wasmtime_parking_unpark() {
    signal_waiting_processors();
}
