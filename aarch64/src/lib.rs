#![no_std]
#![no_main]

extern crate alloc;

use acpi::address::AddressSpace;
use acpi::sdt::spcr::{Spcr, SpcrInterfaceType};
use acpi::{AcpiTables, Handler, PciAddress, PhysicalMapping};
use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;
use core::fmt::{self, Write};
use core::ops::Range;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use alloc::boxed::Box;
use alloc::vec::Vec;
use fdt::Fdt;
use helios_hal::boot::{
    BootFirmwareTables, BootHandoff, BootKernelImage, BootMemoryKind, BootMemoryMap,
    BootMemoryRegion, BootModule, BootModules, FirmwareKind,
};
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::entropy::{EntropyQuality, EntropyUnavailable};
use helios_hal::memory::MemoryRegion;
use helios_hal::serial::ByteSerial;
use helios_hal::{DeviceInventory, DmaModel, Platform};
use helios_kernel::{KernelException, KernelExceptionCause, KernelNativeTrapHandler};
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
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use spin::{Mutex, Once};

const KERNEL_STACK_BYTES: usize = 4 * 1024 * 1024;
const EXCEPTION_STACK_BYTES: usize = 64 * 1024;
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
const QEMU_FW_CFG_MMIO_BASE: usize = 0x0902_0000;
const QEMU_FW_CFG_MMIO_SIZE: usize = 0x18;
const FW_CFG_DATA: usize = 0x000;
const FW_CFG_SELECTOR: usize = 0x008;
const FW_CFG_SIGNATURE: u16 = 0x0000;
const FW_CFG_FILE_DIR: u16 = 0x0019;
const FW_CFG_FILE_ENTRY_BYTES: usize = 64;
const FW_CFG_FILE_NAME_BYTES: usize = 56;
const FW_CFG_HELIOS_RNG_SEED: &[u8] = b"opt/org.helios/rng-seed";

#[cfg(target_os = "none")]
global_asm!(
    include_str!("entry.S"),
    boot_stack_bytes = const KERNEL_STACK_BYTES,
    exception_stack_bytes = const EXCEPTION_STACK_BYTES,
);

unsafe extern "C" {
    static __helios_aarch64_exception_vectors: u8;
}

mod vmm;
pub use vmm::Aarch64UserAddressSpace;

mod debug_state {
    pub(crate) type RuntimeState = helios_kernel::HostRuntimeState<
        crate::Aarch64Cpu,
        helios_kernel::UnsupportedHostFileSystem,
    >;
}

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
static RSDP_REQUEST: RsdpRequest = RsdpRequest::new();
#[used]
static DEVICE_TREE_BLOB_REQUEST: DtbRequest = DtbRequest::new();
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
    wasmtime_tls: AtomicPtr<u8>,
    native_trap_handler: AtomicUsize,
    started: AtomicBool,
}

impl ProcessorRuntime {
    fn new(logical_id: u16) -> Self {
        Self {
            exception_stack: ExceptionStack::new(),
            logical_id,
            _reserved: 0,
            wasmtime_tls: AtomicPtr::new(core::ptr::null_mut()),
            native_trap_handler: AtomicUsize::new(0),
            started: AtomicBool::new(false),
        }
    }
}

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
    boot_entropy: Option<Aarch64BootEntropy>,
    debug_state: Once<debug_state::RuntimeState>,
}

impl Aarch64PlatformState {
    fn from_limine_mp(
        timer_frequency: u64,
        boot_entropy: Option<Aarch64BootEntropy>,
    ) -> &'static Self {
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
            boot_entropy,
            debug_state: Once::new(),
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
            .bootstrap(aarch64_secondary_main, self as *const _ as u64);
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

    fn fill_entropy(&self, buffer: &mut [u8]) -> Result<(), EntropyUnavailable> {
        if let Some(boot_entropy) = &self.boot_entropy {
            boot_entropy.fill(buffer);
            return Ok(());
        }
        fill_with_rndr(buffer)
    }
}

struct Aarch64BootEntropy {
    rng: Mutex<ChaCha20Rng>,
}

impl Aarch64BootEntropy {
    fn new(seed: [u8; 32]) -> Self {
        Self {
            rng: Mutex::new(ChaCha20Rng::from_seed(seed)),
        }
    }

    fn fill(&self, buffer: &mut [u8]) {
        self.rng.lock().fill_bytes(buffer);
    }
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
        unsafe {
            asm!(
                "msr daifset, #0xf",
                options(nomem, nostack, preserves_flags)
            );
        }
    }

    unsafe fn enable_interrupts() {
        unsafe {
            asm!(
                "msr daifclr, #0xf",
                options(nomem, nostack, preserves_flags)
            );
        }
    }

    fn current_owner() -> usize {
        let runtime = read_processor_runtime();
        if runtime != 0 {
            return runtime;
        }

        1
    }
}

#[derive(Clone)]
struct Aarch64AcpiHandler {
    physical_memory_offset: usize,
    timer_frequency: u64,
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
    let debug_serial = DebugSerial::discover(&handoff, physical_memory_offset);
    debug_serial.init();
    DEBUG_SERIAL_BASE.store(debug_serial.base, Ordering::Release);
    let reserved_ranges = boot_reserved_ranges(&handoff);
    let memory_regions = boot_memory_regions(&handoff, physical_memory_offset, &reserved_ranges);
    helios_kernel::prime_bootstrap_allocator(memory_regions);
    vmm::install_user_address_space(physical_memory_offset);
    let boot_entropy = discover_boot_entropy(&handoff, physical_memory_offset);
    let platform_state = Aarch64PlatformState::from_limine_mp(timer_frequency(), boot_entropy);
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
        || read_counter(),
        Some(write_debug_serial_bytes),
    );
    let kernel = helios_kernel::init(
        Platform::new(console, core::iter::empty::<MemoryRegion>(), cpu.clone())
            .with_timer_frequency_hz(cpu.timer_frequency())
            .with_dma_model(DmaModel::Translated)
            .with_devices(DeviceInventory::new().with_debug_serial()),
    );
    let _program_service =
        helios_kernel::install_component_host_program_service(&kernel, &cpu, &debug_state);
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
        unsafe {
            asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }

    fn start_processor(&self, processor: ProcessorId) {
        self.state.start_processor(processor);
    }

    fn wake_processor(&self, _processor: ProcessorId) {
        unsafe {
            asm!("sev", options(nomem, nostack, preserves_flags));
        }
    }

    fn now(&self) -> Instant {
        Instant::new(read_counter())
    }

    fn timer_frequency(&self) -> u64 {
        self.state.timer_frequency
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

    fn fill_entropy(&self, buffer: &mut [u8]) -> Result<EntropyQuality, EntropyUnavailable> {
        self.state.fill_entropy(buffer)?;
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
}

fn activate_processor_runtime(runtime: &'static ProcessorRuntime) {
    let ptr = runtime as *const _ as usize;
    unsafe {
        asm!("msr tpidr_el1, {ptr}", ptr = in(reg) ptr, options(nomem, nostack, preserves_flags));
    }
    runtime.started.store(true, Ordering::Release);
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

    let cpu = Aarch64Cpu { state };
    let debug_state = state.debug_state();
    let console = helios_kernel::RecordingConsole::new(
        debug_state.clone(),
        || read_counter(),
        Some(write_debug_serial_bytes),
    );
    let kernel = helios_kernel::init(
        Platform::new(console, core::iter::empty::<MemoryRegion>(), cpu.clone())
            .with_timer_frequency_hz(cpu.timer_frequency())
            .with_dma_model(DmaModel::Translated)
            .with_devices(DeviceInventory::new().with_debug_serial()),
    );
    let _program_service =
        helios_kernel::install_component_host_program_service(&kernel, &cpu, &debug_state);
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
        "neon" => Some(true),
        _ => None,
    }
}

fn read_id_aa64isar0_el1() -> u64 {
    let value: u64;
    unsafe {
        asm!("mrs {value}, id_aa64isar0_el1", value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn fill_with_rndr(buffer: &mut [u8]) -> Result<(), EntropyUnavailable> {
    if (read_id_aa64isar0_el1() >> 60) & 0xf == 0 {
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

impl Aarch64AcpiHandler {
    fn new(physical_memory_offset: usize) -> Self {
        Self {
            physical_memory_offset,
            timer_frequency: timer_frequency(),
        }
    }

    fn physical_to_virtual<T>(&self, physical_address: usize) -> NonNull<T> {
        let virtual_address = physical_address
            .checked_add(self.physical_memory_offset)
            .unwrap_or_else(|| panic!("AArch64 ACPI physical mapping overflow"));
        NonNull::new(virtual_address as *mut T)
            .unwrap_or_else(|| panic!("AArch64 ACPI physical mapping produced a null pointer"))
    }
}

impl Handler for Aarch64AcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        PhysicalMapping {
            physical_start: physical_address,
            virtual_start: self.physical_to_virtual(physical_address),
            region_length: size,
            mapped_length: size,
            handler: self.clone(),
        }
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {}

    fn read_u8(&self, address: usize) -> u8 {
        unsafe { (address as *const u8).read_volatile() }
    }

    fn read_u16(&self, address: usize) -> u16 {
        unsafe { (address as *const u16).read_volatile() }
    }

    fn read_u32(&self, address: usize) -> u32 {
        unsafe { (address as *const u32).read_volatile() }
    }

    fn read_u64(&self, address: usize) -> u64 {
        unsafe { (address as *const u64).read_volatile() }
    }

    fn write_u8(&self, address: usize, value: u8) {
        unsafe { (address as *mut u8).write_volatile(value) }
    }

    fn write_u16(&self, address: usize, value: u16) {
        unsafe { (address as *mut u16).write_volatile(value) }
    }

    fn write_u32(&self, address: usize, value: u32) {
        unsafe { (address as *mut u32).write_volatile(value) }
    }

    fn write_u64(&self, address: usize, value: u64) {
        unsafe { (address as *mut u64).write_volatile(value) }
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        panic!("AArch64 ACPI system I/O read is unsupported for port {port:#x}")
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        panic!("AArch64 ACPI system I/O read is unsupported for port {port:#x}")
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        panic!("AArch64 ACPI system I/O read is unsupported for port {port:#x}")
    }

    fn write_io_u8(&self, port: u16, _value: u8) {
        panic!("AArch64 ACPI system I/O write is unsupported for port {port:#x}")
    }

    fn write_io_u16(&self, port: u16, _value: u16) {
        panic!("AArch64 ACPI system I/O write is unsupported for port {port:#x}")
    }

    fn write_io_u32(&self, port: u16, _value: u32) {
        panic!("AArch64 ACPI system I/O write is unsupported for port {port:#x}")
    }

    fn read_pci_u8(&self, address: PciAddress, offset: u16) -> u8 {
        panic!("AArch64 ACPI PCI config read is unsupported for {address:?} offset {offset:#x}")
    }

    fn read_pci_u16(&self, address: PciAddress, offset: u16) -> u16 {
        panic!("AArch64 ACPI PCI config read is unsupported for {address:?} offset {offset:#x}")
    }

    fn read_pci_u32(&self, address: PciAddress, offset: u16) -> u32 {
        panic!("AArch64 ACPI PCI config read is unsupported for {address:?} offset {offset:#x}")
    }

    fn write_pci_u8(&self, address: PciAddress, offset: u16, _value: u8) {
        panic!("AArch64 ACPI PCI config write is unsupported for {address:?} offset {offset:#x}")
    }

    fn write_pci_u16(&self, address: PciAddress, offset: u16, _value: u16) {
        panic!("AArch64 ACPI PCI config write is unsupported for {address:?} offset {offset:#x}")
    }

    fn write_pci_u32(&self, address: PciAddress, offset: u16, _value: u32) {
        panic!("AArch64 ACPI PCI config write is unsupported for {address:?} offset {offset:#x}")
    }

    fn nanos_since_boot(&self) -> u64 {
        read_counter().saturating_mul(1_000_000_000) / self.timer_frequency
    }

    fn stall(&self, microseconds: u64) {
        let ticks = self.timer_frequency.saturating_mul(microseconds) / 1_000_000;
        let deadline = read_counter().saturating_add(ticks);
        while read_counter() < deadline {
            core::hint::spin_loop();
        }
    }

    fn sleep(&self, milliseconds: u64) {
        self.stall(milliseconds.saturating_mul(1_000));
    }

    fn create_mutex(&self) -> acpi::Handle {
        panic!("AML mutex creation is unsupported in the AArch64 ACPI handler")
    }

    fn acquire(&self, _mutex: acpi::Handle, _timeout: u16) -> Result<(), acpi::aml::AmlError> {
        panic!("AML mutex acquisition is unsupported in the AArch64 ACPI handler")
    }

    fn release(&self, _mutex: acpi::Handle) {
        panic!("AML mutex release is unsupported in the AArch64 ACPI handler")
    }
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

fn discover_boot_entropy(
    handoff: &LimineBootHandoff,
    physical_memory_offset: usize,
) -> Option<Aarch64BootEntropy> {
    if let Some(fdt) = boot_fdt(handoff) {
        if let Some(chosen) = fdt.find_node("/chosen") {
            if let Some(seed) = chosen.property("rng-seed") {
                return Some(Aarch64BootEntropy::new(seed_from_slice(
                    seed.value,
                    "AArch64 FDT /chosen/rng-seed",
                )));
            }
        }
    }
    if handoff.tables.acpi_rsdp.is_some() {
        return discover_fw_cfg_boot_entropy(physical_memory_offset, handoff);
    }
    None
}

fn discover_fw_cfg_boot_entropy(
    physical_memory_offset: usize,
    handoff: &LimineBootHandoff,
) -> Option<Aarch64BootEntropy> {
    map_mmio_range(
        QEMU_FW_CFG_MMIO_BASE,
        QEMU_FW_CFG_MMIO_SIZE,
        physical_memory_offset,
        handoff,
    );
    let base = mmio_virtual_base(QEMU_FW_CFG_MMIO_BASE, physical_memory_offset);
    let signature = fw_cfg_read_signature(base);
    if signature != *b"QEMU" {
        return None;
    }
    fw_cfg_read_seed_file(base).map(Aarch64BootEntropy::new)
}

fn fw_cfg_read_signature(base: usize) -> [u8; 4] {
    fw_cfg_select(base, FW_CFG_SIGNATURE);
    let mut signature = [0_u8; 4];
    fw_cfg_read_exact(base, &mut signature);
    signature
}

fn fw_cfg_read_seed_file(base: usize) -> Option<[u8; 32]> {
    fw_cfg_select(base, FW_CFG_FILE_DIR);
    let count = fw_cfg_read_be_u32(base);
    assert!(
        count <= 1024,
        "AArch64 fw_cfg file directory has an implausible entry count"
    );
    let mut entry = [0_u8; FW_CFG_FILE_ENTRY_BYTES];
    for _ in 0..count {
        fw_cfg_read_exact(base, &mut entry);
        let size = u32::from_be_bytes(
            entry[0..4]
                .try_into()
                .unwrap_or_else(|_| panic!("fw_cfg size field had invalid width")),
        );
        let selector = u16::from_be_bytes(
            entry[4..6]
                .try_into()
                .unwrap_or_else(|_| panic!("fw_cfg selector field had invalid width")),
        );
        let name = &entry[8..8 + FW_CFG_FILE_NAME_BYTES];
        if fw_cfg_name_matches(name, FW_CFG_HELIOS_RNG_SEED) {
            assert!(
                size >= 32,
                "AArch64 fw_cfg rng seed is shorter than 32 bytes"
            );
            fw_cfg_select(base, selector);
            let mut seed = [0_u8; 32];
            fw_cfg_read_exact(base, &mut seed);
            return Some(seed);
        }
    }
    None
}

fn seed_from_slice(seed: &[u8], source: &str) -> [u8; 32] {
    assert!(seed.len() >= 32, "{source} must contain at least 32 bytes");
    let mut key = [0_u8; 32];
    key.copy_from_slice(&seed[..32]);
    key
}

fn fw_cfg_name_matches(name_field: &[u8], expected: &[u8]) -> bool {
    if name_field.len() < expected.len() {
        return false;
    }
    name_field[..expected.len()] == *expected
        && name_field[expected.len()..]
            .iter()
            .copied()
            .all(|byte| byte == 0)
}

fn fw_cfg_select(base: usize, selector: u16) {
    unsafe {
        ((base + FW_CFG_SELECTOR) as *mut u16).write_volatile(selector.to_be());
    }
}

fn fw_cfg_read_be_u32(base: usize) -> u32 {
    let mut bytes = [0_u8; 4];
    fw_cfg_read_exact(base, &mut bytes);
    u32::from_be_bytes(bytes)
}

fn fw_cfg_read_exact(base: usize, buffer: &mut [u8]) {
    for byte in buffer {
        *byte = unsafe { ((base + FW_CFG_DATA) as *const u8).read_volatile() };
    }
}

#[derive(Clone, Copy)]
struct DebugSerial {
    base: usize,
}

impl DebugSerial {
    fn discover(handoff: &LimineBootHandoff, physical_memory_offset: usize) -> Self {
        if let Some(fdt) = boot_fdt(handoff) {
            return Self::discover_fdt(&fdt, physical_memory_offset, handoff);
        }
        Self::discover_acpi(required_acpi_rsdp(handoff), physical_memory_offset, handoff)
    }

    fn discover_fdt(
        fdt: &Fdt<'_>,
        physical_memory_offset: usize,
        handoff: &LimineBootHandoff,
    ) -> Self {
        let node = fdt
            .all_nodes()
            .find(|node| {
                node.compatible()
                    .is_some_and(|compatible| compatible.all().any(|entry| entry == "arm,pl011"))
            })
            .unwrap_or_else(|| panic!("AArch64 virt platform did not expose a PL011 UART in FDT"));
        let region = node
            .raw_reg()
            .and_then(|mut regions| regions.next())
            .unwrap_or_else(|| panic!("AArch64 PL011 UART node has no usable reg property"));
        let size = fdt_cells_to_usize(region.size, "AArch64 PL011 reg size");
        assert!(size != 0, "AArch64 PL011 UART reg property has zero size");
        let physical_base = fdt_cells_to_usize(region.address, "AArch64 PL011 reg address");
        map_mmio_page(physical_base, physical_memory_offset, handoff);
        Self {
            base: mmio_virtual_base(physical_base, physical_memory_offset),
        }
    }

    fn discover_acpi(
        rsdp_address: usize,
        physical_memory_offset: usize,
        handoff: &LimineBootHandoff,
    ) -> Self {
        let tables = acpi_tables(rsdp_address, physical_memory_offset);
        let spcr = tables
            .find_table::<Spcr>()
            .unwrap_or_else(|| panic!("AArch64 ACPI platform did not expose an SPCR table"));
        assert!(
            matches!(spcr.interface_type(), SpcrInterfaceType::ArmPL011),
            "AArch64 virt platform requires an SPCR Arm PL011 console, got {:?}",
            spcr.interface_type()
        );
        let base = spcr
            .base_address()
            .unwrap_or_else(|| panic!("AArch64 ACPI SPCR did not provide a serial base address"))
            .unwrap_or_else(|error| panic!("AArch64 ACPI SPCR serial base was invalid: {error:?}"));
        assert!(
            base.address_space == AddressSpace::SystemMemory,
            "AArch64 ACPI SPCR serial base must be system memory, got {:?}",
            base.address_space
        );
        let physical_base = usize::try_from(base.address)
            .unwrap_or_else(|_| panic!("AArch64 ACPI SPCR serial base does not fit usize"));
        map_mmio_page(physical_base, physical_memory_offset, handoff);
        Self {
            base: mmio_virtual_base(physical_base, physical_memory_offset),
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

fn read_debug_serial(max_bytes: u32) -> alloc::vec::Vec<u8> {
    helios_kernel::try_read_serial(&active_debug_serial(), max_bytes)
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

fn boot_fdt(handoff: &LimineBootHandoff) -> Option<Fdt<'static>> {
    handoff.tables.device_tree_blob.map(|dtb| {
        unsafe { Fdt::from_ptr(dtb as *const u8) }
            .unwrap_or_else(|error| panic!("Limine provided an invalid AArch64 FDT: {error:?}"))
    })
}

fn required_acpi_rsdp(handoff: &LimineBootHandoff) -> usize {
    handoff
        .tables
        .acpi_rsdp
        .unwrap_or_else(|| panic!("Limine provided neither DTB nor ACPI RSDP for AArch64 virt"))
}

fn acpi_tables(
    rsdp_address: usize,
    physical_memory_offset: usize,
) -> AcpiTables<Aarch64AcpiHandler> {
    let handler = Aarch64AcpiHandler::new(physical_memory_offset);
    unsafe { AcpiTables::from_rsdp(handler, rsdp_address) }
        .unwrap_or_else(|error| panic!("AArch64 ACPI table discovery failed: {error:?}"))
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
    let physical_memory_offset = physical_memory_offset();
    let acpi_rsdp = RSDP_REQUEST.response().map(|response| {
        (response.address as usize)
            .checked_sub(physical_memory_offset)
            .unwrap_or_else(|| panic!("Limine RSDP address was outside the HHDM"))
    });
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
            acpi_rsdp,
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
extern "C" fn wasmtime_tls_get() -> *mut u8 {
    let runtime = read_processor_runtime();
    if runtime == 0 {
        return core::ptr::null_mut();
    }
    unsafe {
        (*(runtime as *const ProcessorRuntime))
            .wasmtime_tls
            .load(Ordering::Acquire)
    }
}

#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(ptr: *mut u8) {
    current_processor_runtime()
        .wasmtime_tls
        .store(ptr, Ordering::Release);
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
