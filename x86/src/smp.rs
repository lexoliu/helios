extern crate alloc;

use acpi::platform::{AcpiPlatform, ProcessorState};
use acpi::{AcpiTables, Handler, PciAddress, PhysicalMapping};
use alloc::alloc::{Layout, alloc_zeroed};
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::arch::x86_64::__cpuid;
use core::num::NonZeroUsize;

use core::ops::Range;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use helios_hal::cpu::{ProcessorId, ticks_to_nanos};
use helios_hal::critical_section::ProcessorIdentity;
use helios_hal::watchdog::Watchdog;
use helios_kernel::{Timer, WasmtimeTlsSlots};
use pci_types::ConfigRegionAccess;
use spin::Once;
use x86::apic::x2apic::X2APIC;
use x86::apic::xapic::{
    XAPIC, XAPIC_LVT_TIMER, XAPIC_SVR, XAPIC_TIMER_CURRENT_COUNT, XAPIC_TIMER_DIV_CONF,
    XAPIC_TIMER_INIT_COUNT,
};
use x86::apic::{ApicControl, ApicId};
use x86::msr::{
    IA32_APIC_BASE, IA32_FS_BASE, IA32_X2APIC_CUR_COUNT, IA32_X2APIC_DIV_CONF,
    IA32_X2APIC_INIT_COUNT, IA32_X2APIC_LVT_TIMER, rdmsr, wrmsr,
};
use x86_64::PhysAddr;
use x86_64::VirtAddr;
use x86_64::instructions::tlb;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::page_table::PageTableEntry;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageSize, PageTable, PageTableFlags, PhysFrame,
    Size2MiB, Size4KiB,
};

use crate::KERNEL_STACK_BYTES;
use crate::debug_state;
use crate::exceptions::{DeviceInterruptRoutes, ProcessorIdt};
use crate::pci::LegacyPciConfigAccess;
use crate::read_tsc;
use crate::watchdog::X86Watchdog;

pub(crate) const WAKEUP_PAGE_BYTES: usize = 4096;
pub(crate) const SIPI_MAX_PHYSICAL_ADDRESS: usize = 0x10_0000;
const AP_STARTUP_INIT_ASSERT_MICROS: u64 = 10_000;
const AP_STARTUP_INTER_IPI_MICROS: u64 = 200;
const APIC_TIMER_CALIBRATION_MICROS: u64 = 10_000;
const APIC_TIMER_DIVIDE_BY_ONE: u32 = 0b1011;
const APIC_TIMER_MASKED: u64 = 1 << 16;
const APIC_TIMER_PERIODIC: u64 = 1 << 17;
const APIC_TIMER_MODE_MASK: u64 = 0b11 << 17;
const APIC_TIMER_VECTOR_MASK: u64 = 0xff;
const PAGE_BYTES: usize = 4096;

static ONLINE_PROCESSOR_MASK: AtomicUsize = AtomicUsize::new(0);
static TLB_SHOOTDOWN_START: AtomicUsize = AtomicUsize::new(0);
static TLB_SHOOTDOWN_LEN: AtomicUsize = AtomicUsize::new(0);
static TLB_SHOOTDOWN_ACK_MASK: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
pub(crate) struct ProcessorRuntime {
    logical_id: u16,
    _reserved: u16,
    physical_memory_offset: usize,
    tsc_hz: u64,
    pub(crate) wasmtime_tls: WasmtimeTlsSlots,
    pub(crate) native_trap_handler: AtomicUsize,
    pub(crate) exception_idt: ProcessorIdt,
    watchdog: X86Watchdog,
    timer: Once<Timer<crate::X86Cpu>>,
    program_service: Once<debug_state::ProgramService>,
    device_interrupts: Once<&'static DeviceInterruptRoutes>,
    local_timer_ready: AtomicBool,
    started: AtomicBool,
}

pub(crate) struct BootContext {
    platform: Arc<X86PlatformState>,
}

pub(crate) struct X86PlatformState {
    tsc_base: u64,
    tsc_hz: u64,
    physical_memory_offset: usize,
    debug_state: debug_state::RuntimeState,
    watchdog: X86Watchdog,
    processors: Box<[ProcessorSlot]>,
    wakeup_page: Option<WakeupPage>,
    boot_context: AtomicPtr<BootContext>,
}

struct ProcessorSlot {
    apic_id: u32,
    runtime: ProcessorRuntime,
    stack_top: usize,
}

struct WakeupPage {
    physical_start: usize,
    virtual_start: usize,
}

#[derive(Clone)]
pub(crate) struct PhysicalOffsetAcpiHandler {
    pub(crate) physical_memory_offset: usize,
    pub(crate) tsc_base: u64,
    pub(crate) tsc_hz: u64,
}

pub(crate) fn build_boot_context(
    rsdp_address: usize,
    physical_memory_offset: usize,
    wakeup_page: Option<Range<usize>>,
    tsc_base: u64,
    tsc_hz: u64,
    debug_state: debug_state::RuntimeState,
) -> &'static BootContext {
    let handler = PhysicalOffsetAcpiHandler {
        physical_memory_offset,
        tsc_base,
        tsc_hz,
    };
    let tables = unsafe { AcpiTables::from_rsdp(handler.clone(), rsdp_address) }
        .unwrap_or_else(|error| panic!("failed to parse ACPI tables: {error:?}"));
    let watchdog = crate::watchdog::discover(&tables, physical_memory_offset);
    let platform = AcpiPlatform::new(tables, handler.clone())
        .unwrap_or_else(|error| panic!("failed to construct ACPI platform info: {error:?}"));
    let processor_info = platform
        .processor_info
        .as_ref()
        .unwrap_or_else(|| panic!("ACPI platform info did not expose processor topology"));

    let mut processors =
        alloc::vec::Vec::with_capacity(1 + processor_info.application_processors.len());
    processors.push(ProcessorSlot {
        apic_id: processor_info.boot_processor.local_apic_id,
        runtime: ProcessorRuntime {
            logical_id: 0,
            _reserved: 0,
            physical_memory_offset,
            tsc_hz,
            wasmtime_tls: WasmtimeTlsSlots::new(),
            native_trap_handler: AtomicUsize::new(0),
            exception_idt: ProcessorIdt::new(),
            watchdog: watchdog.clone(),
            timer: Once::new(),
            program_service: Once::new(),
            device_interrupts: Once::new(),
            local_timer_ready: AtomicBool::new(false),
            started: AtomicBool::new(false),
        },
        stack_top: 0,
    });
    for (index, processor) in processor_info.application_processors.iter().enumerate() {
        assert!(
            processor.state != ProcessorState::Disabled,
            "ACPI exposed disabled application processor apic_id={}",
            processor.local_apic_id
        );
        let stack = allocate_aligned_zeroed(KERNEL_STACK_BYTES, 16);
        processors.push(ProcessorSlot {
            apic_id: processor.local_apic_id,
            runtime: ProcessorRuntime {
                logical_id: (index + 1) as u16,
                _reserved: 0,
                physical_memory_offset,
                tsc_hz,
                wasmtime_tls: WasmtimeTlsSlots::new(),
                native_trap_handler: AtomicUsize::new(0),
                exception_idt: ProcessorIdt::new(),
                watchdog: watchdog.clone(),
                timer: Once::new(),
                program_service: Once::new(),
                device_interrupts: Once::new(),
                local_timer_ready: AtomicBool::new(false),
                started: AtomicBool::new(false),
            },
            stack_top: stack + KERNEL_STACK_BYTES,
        });
    }
    let wakeup_page = if processors.len() > 1 {
        let wakeup_page =
            wakeup_page.unwrap_or_else(|| panic!("x86 SMP startup requires a wakeup page"));
        Some(WakeupPage {
            physical_start: wakeup_page.start,
            virtual_start: wakeup_page
                .start
                .checked_add(physical_memory_offset)
                .unwrap_or_else(|| panic!("x86 wakeup page virtual address overflow")),
        })
    } else {
        None
    };
    let platform = Arc::new(X86PlatformState {
        tsc_base,
        tsc_hz,
        physical_memory_offset,
        debug_state,
        watchdog,
        processors: processors.into_boxed_slice(),
        wakeup_page,
        boot_context: AtomicPtr::new(core::ptr::null_mut()),
    });
    if let Some(wakeup_page) = platform.wakeup_page.as_ref() {
        prepare_wakeup_page(&platform, wakeup_page);
    }
    let context = Box::leak(Box::new(BootContext { platform }));
    context
        .platform
        .boot_context
        .store(context as *const _ as *mut _, Ordering::Release);
    context
}

pub(crate) fn activate_runtime(runtime: &ProcessorRuntime) {
    unsafe {
        wrmsr(IA32_FS_BASE, runtime as *const _ as u64);
    }
    let bit = processor_bit(usize::from(runtime.logical_id));
    ONLINE_PROCESSOR_MASK.fetch_or(bit, Ordering::AcqRel);
    runtime.started.store(true, Ordering::Release);
}

pub(crate) fn current_runtime() -> &'static ProcessorRuntime {
    let runtime = current_runtime_address() as *const ProcessorRuntime;
    assert!(
        !runtime.is_null(),
        "x86 processor runtime was not installed before use"
    );
    unsafe { &*runtime }
}

pub(crate) fn current_processor() -> ProcessorId {
    ProcessorId::new(current_runtime().logical_id)
}

pub(crate) fn current_runtime_address() -> usize {
    unsafe { rdmsr(IA32_FS_BASE) as usize }
}

/// The identity this processor answers critical-section acquires with.
///
/// A processor takes critical sections from its first instruction, well before
/// [`activate_runtime`] publishes its runtime address, and several application
/// processors run that prologue concurrently. The local APIC id is unique and
/// readable throughout, so it stands in until the runtime address exists;
/// sharing one value across processors would make a second processor's acquire
/// look like the first processor's nested re-acquire.
pub(crate) fn current_identity() -> ProcessorIdentity {
    match NonZeroUsize::new(current_runtime_address()) {
        Some(runtime) => ProcessorIdentity::from_raw(runtime),
        None => ProcessorIdentity::bootstrapping(current_apic_id() as usize),
    }
}

/// The local APIC id of the executing processor, from the topology leaf when
/// the CPU publishes one and the legacy 8-bit initial APIC id otherwise.
fn current_apic_id() -> u32 {
    let max_basic_leaf = __cpuid(0).eax;
    if max_basic_leaf >= 0x0b {
        let topology = __cpuid(0x0b);
        return topology.edx;
    }

    __cpuid(1).ebx >> 24
}

impl ProcessorRuntime {
    pub(crate) fn install_timer(&self, timer: Timer<crate::X86Cpu>) {
        assert!(
            self.timer.get().is_none(),
            "x86 processor timer was installed more than once"
        );
        self.timer.call_once(|| timer);
    }

    pub(crate) fn install_program_service(&self, service: debug_state::ProgramService) {
        assert!(
            self.program_service.get().is_none(),
            "x86 processor program service was installed more than once"
        );
        self.program_service.call_once(|| service);
    }

    pub(crate) fn ensure_local_timer(&self, vector: u8) {
        if self
            .local_timer_ready
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            enable_local_scheduler_timer(vector);
        }
    }

    fn platform_tsc_hz(&self) -> u64 {
        self.tsc_hz
    }
}

impl BootContext {
    pub(crate) fn platform(&self) -> Arc<X86PlatformState> {
        self.platform.clone()
    }

    pub(crate) fn bootstrap_runtime(&self) -> &ProcessorRuntime {
        &self.platform.processors[0].runtime
    }
}

impl X86PlatformState {
    pub(crate) fn tsc_base(&self) -> u64 {
        self.tsc_base
    }

    pub(crate) fn tsc_hz(&self) -> u64 {
        self.tsc_hz
    }

    pub(crate) fn debug_state(&self) -> debug_state::RuntimeState {
        self.debug_state.clone()
    }

    pub(crate) fn watchdog(&self) -> X86Watchdog {
        self.watchdog.clone()
    }

    pub(crate) fn processor_count(&self) -> usize {
        self.processors.len()
    }

    /// Publishes the device interrupt routes every processor dispatches
    /// MSI-X vectors through.
    ///
    /// One table, shared: a device's configuration message is addressed
    /// to the bootstrap processor, but a multi-queue network device
    /// steers each queue pair's message to the processor whose shard
    /// drains that pair, so any processor can be the one an MSI-X
    /// vector lands on. Which queue a vector belongs to does not depend
    /// on who fields it, so every processor reads the same table.
    ///
    /// # SMP contract
    ///
    /// The table is built once, while the bootstrap processor is the
    /// only one online and its own interrupts are still masked, and it
    /// is borrowed for the life of the machine. Every processor slot
    /// the ACPI topology described already exists by then, so a
    /// processor started later reads the table its slot was handed
    /// before it ran its first instruction; the interrupt path only
    /// loads a pointer, taking no lock and no allocation.
    pub(crate) fn install_device_interrupts(&self, routes: DeviceInterruptRoutes) {
        assert_eq!(
            ONLINE_PROCESSOR_MASK.load(Ordering::Acquire),
            processor_bit(usize::from(self.bootstrap_processor().id())),
            "x86 device interrupt routes must be published while the bootstrap \
             processor is the only one online"
        );
        let routes: &'static DeviceInterruptRoutes = Box::leak(Box::new(routes));
        for slot in self.processors.iter() {
            assert!(
                slot.runtime.device_interrupts.get().is_none(),
                "x86 device interrupt routes were installed more than once"
            );
            slot.runtime.device_interrupts.call_once(|| routes);
        }
    }

    pub(crate) fn bootstrap_processor(&self) -> ProcessorId {
        ProcessorId::new(0)
    }

    /// Look up the local-APIC id of the processor with the given
    /// logical id. Used by the wake / TLB-shootdown IPI dispatch path
    /// to address a remote core through the local APIC ICR.
    pub(crate) fn apic_id_of(&self, processor: ProcessorId) -> Option<u32> {
        self.processors
            .get(processor.id() as usize)
            .map(|slot| slot.apic_id)
    }

    pub(crate) fn current_processor(&self) -> ProcessorId {
        ProcessorId::new(current_runtime().logical_id)
    }

    pub(crate) fn start_processor(&self, processor: ProcessorId, entry: usize) {
        let slot = self.processor_slot(processor);
        let wakeup_page = self
            .wakeup_page
            .as_ref()
            .unwrap_or_else(|| panic!("x86 startup state is missing the AP wakeup page"));
        assert!(
            processor != self.bootstrap_processor(),
            "bootstrap processor cannot be started twice"
        );
        assert!(
            !slot.runtime.started.load(Ordering::Acquire),
            "processor {} was started more than once",
            processor.id()
        );

        patch_wakeup_page(
            wakeup_page.virtual_start,
            current_cr3(),
            slot.stack_top,
            self.boot_context_ptr(),
            &slot.runtime as *const _ as usize,
            entry,
        );

        unsafe {
            wake_application_processor(
                slot.apic_id,
                wakeup_page.physical_start,
                self.physical_memory_offset,
                self.tsc_hz,
            )
            .unwrap_or_else(|message| {
                panic!(
                    "failed to wake x86 processor {} apic_id={}: {message}",
                    processor.id(),
                    slot.apic_id
                )
            });
        }

        let deadline = read_tsc()
            .checked_add(self.tsc_hz / 2)
            .unwrap_or_else(|| panic!("x86 AP startup deadline overflow"));
        while !slot.runtime.started.load(Ordering::Acquire) {
            assert!(
                read_tsc() <= deadline,
                "x86 processor {} did not reach Rust startup",
                processor.id()
            );
            core::hint::spin_loop();
        }
    }

    fn processor_slot(&self, processor: ProcessorId) -> &ProcessorSlot {
        self.processors
            .get(processor.id() as usize)
            .unwrap_or_else(|| panic!("x86 processor {} is out of range", processor.id()))
    }

    fn boot_context_ptr(&self) -> usize {
        let pointer = self.boot_context.load(Ordering::Acquire);
        assert!(
            !pointer.is_null(),
            "x86 boot context pointer was not installed before SMP startup"
        );
        pointer as usize
    }
}

enum LocalApicMode {
    XApic { physical_base: usize },
    X2Apic,
}

pub(crate) fn ensure_local_scheduler_timer(vector: u8) {
    let runtime = current_runtime();
    runtime.ensure_local_timer(vector);
}

pub(crate) fn handle_local_timer_interrupt() {
    let runtime = current_runtime();
    runtime.watchdog.pet();
    if let Some(service) = runtime.program_service.get() {
        service.increment_epoch();
    }
    runtime
        .timer
        .get()
        .unwrap_or_else(|| panic!("x86 local timer interrupted before kernel timer installation"))
        .handle_interrupt();
    local_apic_eoi();
}

/// Dispatches an MSI-X device interrupt to the driver bound to `vector`.
///
/// Runs on whichever processor the message was steered to; the routes
/// its runtime holds are the one table every processor was handed
/// during device bring-up.
pub(crate) fn handle_device_interrupt(vector: u8) {
    let runtime = current_runtime();
    let routes = runtime.device_interrupts.get().unwrap_or_else(|| {
        panic!("x86 device interrupt vector {vector:#x} fired before routes were installed")
    });
    assert!(
        routes.route(vector),
        "x86 device interrupt vector {vector:#x} has no registered handler"
    );
    local_apic_eoi();
}

pub(crate) fn handle_wake_interrupt() {
    // Wake IPI carries no payload; receiving it is sufficient to
    // bring the processor out of HLT and back into the kernel
    // run loop. Just ack and return.
    local_apic_eoi();
}

pub(crate) fn shootdown_tlb_range(start: usize, byte_len: usize) {
    if byte_len == 0 {
        return;
    }
    let online = ONLINE_PROCESSOR_MASK.load(Ordering::Acquire);
    let current = usize::from(current_runtime().logical_id);
    let current_bit = processor_bit(current);
    let targets = online & !current_bit;
    if targets == 0 {
        return;
    }

    TLB_SHOOTDOWN_START.store(start, Ordering::Release);
    TLB_SHOOTDOWN_LEN.store(byte_len, Ordering::Release);
    TLB_SHOOTDOWN_ACK_MASK.store(current_bit, Ordering::Release);
    send_tlb_shootdown_ipi_all_excluding_self();
    let expected = online;
    while TLB_SHOOTDOWN_ACK_MASK.load(Ordering::Acquire) & expected != expected {
        core::hint::spin_loop();
    }
}

pub(crate) fn handle_tlb_shootdown_interrupt() {
    let start = TLB_SHOOTDOWN_START.load(Ordering::Acquire);
    let byte_len = TLB_SHOOTDOWN_LEN.load(Ordering::Acquire);
    for offset in (0..byte_len).step_by(PAGE_BYTES) {
        tlb::flush(VirtAddr::new((start + offset) as u64));
    }
    let bit = processor_bit(usize::from(current_runtime().logical_id));
    TLB_SHOOTDOWN_ACK_MASK.fetch_or(bit, Ordering::AcqRel);
    local_apic_eoi();
}

/// Send a wake IPI to the processor with the given local-APIC id.
///
/// The receiver runs the wake interrupt handler (`local_apic_eoi`-only)
/// and falls out of HLT in `Cpu::park_current`, ready to pick up
/// newly runnable kernel tasks. The function picks the appropriate
/// APIC mode (X2APIC / XAPIC) and constructs the corresponding ICR
/// internally.
pub(crate) fn send_wake_ipi(target_apic_id: u32) {
    use x86::apic::{
        ApicControl, ApicId, DeliveryMode, DeliveryStatus, DestinationMode, DestinationShorthand,
        Icr, Level, TriggerMode,
    };
    let runtime = current_runtime();
    match local_apic_mode(target_apic_id) {
        LocalApicMode::X2Apic => {
            let mut apic = X2APIC::new();
            apic.attach();
            let icr = Icr::for_x2apic(
                crate::exceptions::WAKE_INTERRUPT_VECTOR,
                ApicId::X2Apic(target_apic_id),
                DestinationShorthand::NoShorthand,
                DeliveryMode::Fixed,
                DestinationMode::Physical,
                DeliveryStatus::Idle,
                Level::Assert,
                TriggerMode::Edge,
            );
            unsafe { apic.send_ipi(icr) };
        }
        LocalApicMode::XApic { physical_base } => {
            let apic_region = xapic_mmio_region(physical_base, runtime.physical_memory_offset);
            let mut apic = XAPIC::new(apic_region);
            apic.attach();
            let target = u8::try_from(target_apic_id)
                .unwrap_or_else(|_| panic!("xAPIC wake target id {target_apic_id} exceeds 8 bits"));
            let icr = Icr::for_xapic(
                crate::exceptions::WAKE_INTERRUPT_VECTOR,
                ApicId::XApic(target),
                DestinationShorthand::NoShorthand,
                DeliveryMode::Fixed,
                DestinationMode::Physical,
                DeliveryStatus::Idle,
                Level::Assert,
                TriggerMode::Edge,
            );
            unsafe { apic.send_ipi(icr) };
        }
    }
}

fn send_tlb_shootdown_ipi_all_excluding_self() {
    use x86::apic::{
        ApicControl, ApicId, DeliveryMode, DeliveryStatus, DestinationMode, DestinationShorthand,
        Icr, Level, TriggerMode,
    };
    let runtime = current_runtime();
    match local_apic_mode(0) {
        LocalApicMode::X2Apic => {
            let mut apic = X2APIC::new();
            apic.attach();
            let icr = Icr::for_x2apic(
                crate::exceptions::TLB_SHOOTDOWN_INTERRUPT_VECTOR,
                ApicId::X2Apic(0),
                DestinationShorthand::AllExcludingSelf,
                DeliveryMode::Fixed,
                DestinationMode::Physical,
                DeliveryStatus::Idle,
                Level::Assert,
                TriggerMode::Edge,
            );
            unsafe { apic.send_ipi(icr) };
        }
        LocalApicMode::XApic { physical_base } => {
            let apic_region = xapic_mmio_region(physical_base, runtime.physical_memory_offset);
            let mut apic = XAPIC::new(apic_region);
            apic.attach();
            let icr = Icr::for_xapic(
                crate::exceptions::TLB_SHOOTDOWN_INTERRUPT_VECTOR,
                ApicId::XApic(0),
                DestinationShorthand::AllExcludingSelf,
                DeliveryMode::Fixed,
                DestinationMode::Physical,
                DeliveryStatus::Idle,
                Level::Assert,
                TriggerMode::Edge,
            );
            unsafe { apic.send_ipi(icr) };
        }
    }
}

fn processor_bit(processor: usize) -> usize {
    assert!(
        processor < usize::BITS as usize,
        "x86 TLB shootdown supports at most {} online processors; got processor {}",
        usize::BITS,
        processor
    );
    1usize << processor
}

fn enable_local_scheduler_timer(vector: u8) {
    let runtime = current_runtime();
    match local_apic_mode(0) {
        LocalApicMode::X2Apic => {
            let mut apic = X2APIC::new();
            apic.attach();
            let initial_count = calibrate_x2apic_timer(runtime.platform_tsc_hz());
            program_x2apic_periodic_timer(vector, initial_count);
        }
        LocalApicMode::XApic { physical_base } => {
            let apic_region = xapic_mmio_region(physical_base, runtime.physical_memory_offset);
            attach_xapic(apic_region);
            let initial_count = calibrate_xapic_timer(apic_region, runtime.platform_tsc_hz());
            program_xapic_periodic_timer(apic_region, vector, initial_count);
        }
    }
}

fn attach_xapic(apic_region: &[u32]) {
    unsafe {
        let mut base = rdmsr(IA32_APIC_BASE);
        base |= 1 << 11;
        wrmsr(IA32_APIC_BASE, base);
    }
    write_xapic_register(apic_region, XAPIC_SVR, (1 << 8) | 15);
}

fn calibrate_x2apic_timer(tsc_hz: u64) -> u32 {
    unsafe {
        wrmsr(IA32_X2APIC_DIV_CONF, u64::from(APIC_TIMER_DIVIDE_BY_ONE));
        wrmsr(
            IA32_X2APIC_LVT_TIMER,
            APIC_TIMER_MASKED | u64::from(crate::exceptions::TIMER_INTERRUPT_VECTOR),
        );
        wrmsr(IA32_X2APIC_INIT_COUNT, u64::from(u32::MAX));
        stall_microseconds(tsc_hz, APIC_TIMER_CALIBRATION_MICROS);
        let current = rdmsr(IA32_X2APIC_CUR_COUNT) as u32;
        wrmsr(IA32_X2APIC_INIT_COUNT, 0);
        timer_elapsed_count(current)
    }
}

fn calibrate_xapic_timer(apic_region: &[u32], tsc_hz: u64) -> u32 {
    write_xapic_register(apic_region, XAPIC_TIMER_DIV_CONF, APIC_TIMER_DIVIDE_BY_ONE);
    write_xapic_register(
        apic_region,
        XAPIC_LVT_TIMER,
        (APIC_TIMER_MASKED | u64::from(crate::exceptions::TIMER_INTERRUPT_VECTOR)) as u32,
    );
    write_xapic_register(apic_region, XAPIC_TIMER_INIT_COUNT, u32::MAX);
    stall_microseconds(tsc_hz, APIC_TIMER_CALIBRATION_MICROS);
    let current = read_xapic_register(apic_region, XAPIC_TIMER_CURRENT_COUNT);
    write_xapic_register(apic_region, XAPIC_TIMER_INIT_COUNT, 0);
    timer_elapsed_count(current)
}

fn timer_elapsed_count(current: u32) -> u32 {
    let elapsed = u32::MAX.saturating_sub(current);
    assert!(
        elapsed != 0,
        "x86 local APIC timer did not tick during calibration"
    );
    elapsed
}

fn program_x2apic_periodic_timer(vector: u8, initial_count: u32) {
    unsafe {
        let lvt = (rdmsr(IA32_X2APIC_LVT_TIMER)
            & !(APIC_TIMER_VECTOR_MASK | APIC_TIMER_MASKED | APIC_TIMER_MODE_MASK))
            | u64::from(vector)
            | APIC_TIMER_PERIODIC;
        wrmsr(IA32_X2APIC_LVT_TIMER, lvt);
        wrmsr(IA32_X2APIC_INIT_COUNT, u64::from(initial_count));
    }
}

fn program_xapic_periodic_timer(apic_region: &[u32], vector: u8, initial_count: u32) {
    let lvt = (u64::from(read_xapic_register(apic_region, XAPIC_LVT_TIMER))
        & !(APIC_TIMER_VECTOR_MASK | APIC_TIMER_MASKED | APIC_TIMER_MODE_MASK))
        | u64::from(vector)
        | APIC_TIMER_PERIODIC;
    write_xapic_register(apic_region, XAPIC_LVT_TIMER, lvt as u32);
    write_xapic_register(apic_region, XAPIC_TIMER_INIT_COUNT, initial_count);
}

fn read_xapic_register(apic_region: &[u32], offset: u32) -> u32 {
    let index = (offset / 4) as usize;
    unsafe { core::ptr::read_volatile(&apic_region[index]) }
}

fn write_xapic_register(apic_region: &[u32], offset: u32, value: u32) {
    let index = (offset / 4) as usize;
    unsafe {
        core::ptr::write_volatile(apic_region.as_ptr().add(index) as *mut u32, value);
    }
}

fn local_apic_eoi() {
    let runtime = current_runtime();
    match local_apic_mode(0) {
        LocalApicMode::X2Apic => {
            let mut apic = X2APIC::new();
            apic.eoi();
        }
        LocalApicMode::XApic { physical_base } => {
            let apic_region = xapic_mmio_region(physical_base, runtime.physical_memory_offset);
            let mut apic = XAPIC::new(apic_region);
            apic.eoi();
        }
    }
}

unsafe fn wake_application_processor(
    apic_id: u32,
    wakeup_page_physical: usize,
    physical_memory_offset: usize,
    tsc_hz: u64,
) -> Result<(), &'static str> {
    assert_eq!(
        wakeup_page_physical & (WAKEUP_PAGE_BYTES - 1),
        0,
        "x86 wakeup page must stay 4KiB aligned"
    );
    assert!(
        wakeup_page_physical < SIPI_MAX_PHYSICAL_ADDRESS,
        "x86 wakeup page must stay below 1MiB for SIPI"
    );
    let startup_vector = u8::try_from(wakeup_page_physical >> 12)
        .unwrap_or_else(|_| panic!("x86 SIPI vector exceeded 8 bits"));

    match local_apic_mode(apic_id) {
        LocalApicMode::X2Apic => {
            let mut apic = X2APIC::new();
            apic.attach();
            send_startup_ipis(&mut apic, ApicId::X2Apic(apic_id), startup_vector, tsc_hz);
        }
        LocalApicMode::XApic { physical_base } => {
            let apic_region = xapic_mmio_region(physical_base, physical_memory_offset);
            let mut apic = XAPIC::new(apic_region);
            apic.attach();
            let apic_id = u8::try_from(apic_id).map_err(|_| "xAPIC target id exceeded 8 bits")?;
            send_startup_ipis(&mut apic, ApicId::XApic(apic_id), startup_vector, tsc_hz);
        }
    }

    Ok(())
}

fn local_apic_mode(target_apic_id: u32) -> LocalApicMode {
    let cpuid = __cpuid(1);
    if cpuid.ecx & (1 << 21) != 0 {
        return LocalApicMode::X2Apic;
    }
    let base = unsafe { rdmsr(x86::msr::IA32_APIC_BASE) };
    if base & (1 << 10) != 0 {
        return LocalApicMode::X2Apic;
    }
    if target_apic_id <= u8::MAX.into() {
        return LocalApicMode::XApic {
            physical_base: (base as usize) & 0xffff_f000,
        };
    }
    panic!("x86 target apic id {target_apic_id} requires x2APIC support")
}

fn send_startup_ipis(
    apic: &mut impl ApicControl,
    apic_id: ApicId,
    startup_vector: u8,
    tsc_hz: u64,
) {
    unsafe {
        apic.ipi_init(apic_id);
    }
    stall_microseconds(tsc_hz, AP_STARTUP_INIT_ASSERT_MICROS);
    unsafe {
        apic.ipi_init_deassert();
    }
    stall_microseconds(tsc_hz, AP_STARTUP_INTER_IPI_MICROS);
    unsafe {
        apic.ipi_startup(apic_id, startup_vector);
    }
    stall_microseconds(tsc_hz, AP_STARTUP_INTER_IPI_MICROS);
    unsafe {
        apic.ipi_startup(apic_id, startup_vector);
    }
}

fn stall_microseconds(tsc_hz: u64, microseconds: u64) {
    let ticks = tsc_hz.saturating_mul(microseconds) / 1_000_000;
    let deadline = read_tsc().saturating_add(ticks);
    while read_tsc() < deadline {
        core::hint::spin_loop();
    }
}

fn xapic_mmio_region(physical_base: usize, physical_memory_offset: usize) -> &'static mut [u32] {
    // The local APIC page is MMIO, not RAM: it is absent from the boot
    // memory map, so the HHDM does not cover it until it is mapped here.
    let virtual_base = map_mmio_window(physical_memory_offset, physical_base, WAKEUP_PAGE_BYTES);
    unsafe { core::slice::from_raw_parts_mut(virtual_base as *mut u32, WAKEUP_PAGE_BYTES / 4) }
}

impl PhysicalOffsetAcpiHandler {
    fn physical_to_virtual<T>(&self, physical_address: usize) -> NonNull<T> {
        let virtual_address = physical_address
            .checked_add(self.physical_memory_offset)
            .unwrap_or_else(|| panic!("physical mapping overflow at {physical_address:#x}"));
        NonNull::new(virtual_address as *mut T)
            .unwrap_or_else(|| panic!("physical mapping produced a null virtual pointer"))
    }
}

impl Handler for PhysicalOffsetAcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        ensure_hhdm_range_mapped(self.physical_memory_offset, physical_address, size);
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
        unsafe { x86_64::instructions::port::PortReadOnly::<u8>::new(port).read() }
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        unsafe { x86_64::instructions::port::PortReadOnly::<u16>::new(port).read() }
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        unsafe { x86_64::instructions::port::PortReadOnly::<u32>::new(port).read() }
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        unsafe { x86_64::instructions::port::PortWriteOnly::<u8>::new(port).write(value) }
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        unsafe { x86_64::instructions::port::PortWriteOnly::<u16>::new(port).write(value) }
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        unsafe { x86_64::instructions::port::PortWriteOnly::<u32>::new(port).write(value) }
    }

    fn read_pci_u8(&self, address: PciAddress, offset: u16) -> u8 {
        LegacyPciConfigAccess::new().read_u8(address, offset)
    }

    fn read_pci_u16(&self, address: PciAddress, offset: u16) -> u16 {
        LegacyPciConfigAccess::new().read_u16(address, offset)
    }

    fn read_pci_u32(&self, address: PciAddress, offset: u16) -> u32 {
        unsafe { LegacyPciConfigAccess::new().read(address, offset) }
    }

    fn write_pci_u8(&self, address: PciAddress, offset: u16, value: u8) {
        LegacyPciConfigAccess::new().write_u8(address, offset, value)
    }

    fn write_pci_u16(&self, address: PciAddress, offset: u16, value: u16) {
        LegacyPciConfigAccess::new().write_u16(address, offset, value)
    }

    fn write_pci_u32(&self, address: PciAddress, offset: u16, value: u32) {
        unsafe { LegacyPciConfigAccess::new().write(address, offset, value) }
    }

    fn nanos_since_boot(&self) -> u64 {
        let ticks = read_tsc().saturating_sub(self.tsc_base);
        ticks_to_nanos(ticks, self.tsc_hz)
    }

    fn stall(&self, microseconds: u64) {
        let ticks = self.tsc_hz.saturating_mul(microseconds) / 1_000_000;
        let deadline = read_tsc().saturating_add(ticks);
        while read_tsc() < deadline {
            core::hint::spin_loop();
        }
    }

    fn sleep(&self, milliseconds: u64) {
        self.stall(milliseconds.saturating_mul(1_000));
    }

    fn create_mutex(&self) -> acpi::Handle {
        panic!("AML mutex creation is unsupported in the x86 ACPI handler")
    }

    fn acquire(&self, _mutex: acpi::Handle, _timeout: u16) -> Result<(), acpi::aml::AmlError> {
        panic!("AML mutex acquisition is unsupported in the x86 ACPI handler")
    }

    fn release(&self, _mutex: acpi::Handle) {
        panic!("AML mutex release is unsupported in the x86 ACPI handler")
    }
}

fn prepare_wakeup_page(platform: &X86PlatformState, wakeup_page: &WakeupPage) {
    identity_map_wakeup_page(platform.physical_memory_offset, wakeup_page.physical_start);
    let bytes = wakeup_image();
    assert!(
        bytes.len() <= WAKEUP_PAGE_BYTES,
        "x86 wakeup image exceeded one page: {} bytes",
        bytes.len()
    );
    unsafe {
        core::ptr::write_bytes(wakeup_page.virtual_start as *mut u8, 0, WAKEUP_PAGE_BYTES);
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            wakeup_page.virtual_start as *mut u8,
            bytes.len(),
        );
    }
}

fn patch_wakeup_page(
    wakeup_virtual_start: usize,
    cr3: u64,
    stack_top: usize,
    boot_context: usize,
    runtime: usize,
    entry: usize,
) {
    write_u64_patch(wakeup_virtual_start, wakeup_cr3_offset(), cr3);
    write_u64_patch(
        wakeup_virtual_start,
        wakeup_stack_top_offset(),
        stack_top as u64,
    );
    write_u64_patch(
        wakeup_virtual_start,
        wakeup_context_offset(),
        boot_context as u64,
    );
    write_u64_patch(
        wakeup_virtual_start,
        wakeup_runtime_offset(),
        runtime as u64,
    );
    write_u64_patch(wakeup_virtual_start, wakeup_entry_offset(), entry as u64);
}

fn write_u64_patch(image_start: usize, offset: usize, value: u64) {
    let address = image_start
        .checked_add(offset)
        .unwrap_or_else(|| panic!("x86 wakeup image patch overflow at offset {offset:#x}"));
    unsafe {
        (address as *mut u64).write_volatile(value);
    }
}

fn current_cr3() -> u64 {
    let (frame, _) = Cr3::read();
    frame.start_address().as_u64()
}

fn identity_map_wakeup_page(physical_memory_offset: usize, wakeup_physical: usize) {
    let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(wakeup_physical as u64))
        .unwrap_or_else(|error| panic!("invalid x86 wakeup page address: {error:?}"));
    let frame = PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(wakeup_physical as u64))
        .unwrap_or_else(|error| panic!("invalid x86 wakeup frame address: {error:?}"));
    let mut mapper = unsafe { current_mapper(physical_memory_offset) };
    if mapper.translate_page(page).is_ok() {
        return;
    }
    let mut frame_allocator = DirectMappedFrameAllocator {
        physical_memory_offset,
    };
    unsafe {
        mapper
            .map_to(
                page,
                frame,
                PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                &mut frame_allocator,
            )
            .unwrap_or_else(|error| panic!("failed to identity-map x86 wakeup page: {error:?}"))
            .flush();
    }
}

pub(crate) fn map_mmio_window(
    physical_memory_offset: usize,
    physical_start: usize,
    bytes: usize,
) -> usize {
    let virtual_start = physical_memory_offset
        .checked_add(physical_start)
        .unwrap_or_else(|| panic!("x86 MMIO virtual address overflow at {physical_start:#x}"));
    let page_offset = virtual_start & (PAGE_BYTES - 1);
    let map_start = align_down(virtual_start, PAGE_BYTES);
    let map_bytes = align_up(
        page_offset
            .checked_add(bytes)
            .unwrap_or_else(|| panic!("x86 MMIO mapping size overflow")),
        PAGE_BYTES,
    );
    let mut frame_allocator = DirectMappedFrameAllocator {
        physical_memory_offset,
    };
    let mut mapper = unsafe { current_mapper(physical_memory_offset) };

    for virtual_address in (map_start..map_start + map_bytes).step_by(PAGE_BYTES) {
        let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(virtual_address as u64))
            .unwrap_or_else(|error| panic!("invalid x86 MMIO virtual page: {error:?}"));
        if mapper.translate_page(page).is_ok() {
            let entry =
                ensure_direct_mapped_leaf_entry(physical_memory_offset, page, &mut frame_allocator);
            entry.set_flags(
                entry.flags() | PageTableFlags::NO_CACHE | PageTableFlags::WRITE_THROUGH,
            );
            tlb::flush(VirtAddr::new(virtual_address as u64));
            continue;
        }

        let physical_address = virtual_address
            .checked_sub(physical_memory_offset)
            .unwrap_or_else(|| {
                panic!("x86 MMIO virtual address {virtual_address:#x} was outside the HHDM")
            });
        let frame =
            PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(physical_address as u64))
                .unwrap_or_else(|error| panic!("invalid x86 MMIO physical frame: {error:?}"));
        unsafe {
            mapper
                .map_to(
                    page,
                    frame,
                    PageTableFlags::PRESENT
                        | PageTableFlags::WRITABLE
                        | PageTableFlags::NO_CACHE
                        | PageTableFlags::WRITE_THROUGH,
                    &mut frame_allocator,
                )
                .unwrap_or_else(|error| panic!("failed to map x86 MMIO page: {error:?}"))
                .flush();
        }
    }

    virtual_start
}

pub(crate) fn ensure_hhdm_range_mapped(
    physical_memory_offset: usize,
    physical_start: usize,
    bytes: usize,
) {
    let page_offset = physical_start & (PAGE_BYTES - 1);
    let map_start = align_down(physical_start, PAGE_BYTES);
    let map_bytes = align_up(
        page_offset
            .checked_add(bytes.max(1))
            .unwrap_or_else(|| panic!("x86 HHDM mapping size overflow")),
        PAGE_BYTES,
    );
    let mut mapper = unsafe { current_mapper(physical_memory_offset) };
    let mut frame_allocator = DirectMappedFrameAllocator {
        physical_memory_offset,
    };

    for physical_address in (map_start..map_start + map_bytes).step_by(PAGE_BYTES) {
        let virtual_address = physical_memory_offset
            .checked_add(physical_address)
            .unwrap_or_else(|| {
                panic!("x86 HHDM virtual address overflow at {physical_address:#x}")
            });
        let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(virtual_address as u64))
            .unwrap_or_else(|error| panic!("invalid x86 HHDM virtual page: {error:?}"));
        if mapper.translate_page(page).is_ok() {
            continue;
        }
        let frame =
            PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(physical_address as u64))
                .unwrap_or_else(|error| panic!("invalid x86 HHDM physical frame: {error:?}"));
        unsafe {
            mapper
                .map_to(
                    page,
                    frame,
                    PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                    &mut frame_allocator,
                )
                .unwrap_or_else(|error| panic!("failed to map x86 HHDM page: {error:?}"))
                .flush();
        }
    }
}

fn ensure_direct_mapped_leaf_entry(
    physical_memory_offset: usize,
    page: Page<Size4KiB>,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> &mut PageTableEntry {
    let p4 = current_level_4_table(physical_memory_offset);
    let p3 = next_table_mut(
        physical_memory_offset,
        &mut p4[page.p4_index()],
        "x86 MMIO direct-map P4 entry",
    );
    let p2 = next_or_split_p3_table(
        physical_memory_offset,
        &mut p3[page.p3_index()],
        frame_allocator,
    );
    let p1 = next_or_split_p2_table(
        physical_memory_offset,
        &mut p2[page.p2_index()],
        frame_allocator,
    );
    let entry = &mut p1[page.p1_index()];
    assert!(
        !entry.is_unused(),
        "x86 MMIO direct-map leaf entry was unexpectedly unmapped"
    );
    entry
}

fn current_level_4_table(physical_memory_offset: usize) -> &'static mut PageTable {
    let (level_4, _) = Cr3::read();
    let virtual_address = level_4
        .start_address()
        .as_u64()
        .checked_add(physical_memory_offset as u64)
        .unwrap_or_else(|| panic!("x86 level-4 page table virtual address overflow"));
    unsafe { &mut *(virtual_address as *mut PageTable) }
}

fn next_table_mut<'a>(
    physical_memory_offset: usize,
    entry: &'a mut PageTableEntry,
    context: &str,
) -> &'a mut PageTable {
    assert!(!entry.is_unused(), "{context} was unexpectedly unmapped");
    assert!(
        !entry.flags().contains(PageTableFlags::HUGE_PAGE),
        "{context} unexpectedly mapped a huge page"
    );
    let virtual_address = entry
        .addr()
        .as_u64()
        .checked_add(physical_memory_offset as u64)
        .unwrap_or_else(|| panic!("{context} virtual address overflow"));
    unsafe { &mut *(virtual_address as *mut PageTable) }
}

fn next_or_split_p3_table<'a>(
    physical_memory_offset: usize,
    entry: &'a mut PageTableEntry,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> &'a mut PageTable {
    if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return split_p3_huge_page(physical_memory_offset, entry, frame_allocator);
    }
    next_table_mut(
        physical_memory_offset,
        entry,
        "x86 MMIO direct-map P3 entry",
    )
}

fn next_or_split_p2_table<'a>(
    physical_memory_offset: usize,
    entry: &'a mut PageTableEntry,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> &'a mut PageTable {
    if entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return split_p2_huge_page(physical_memory_offset, entry, frame_allocator);
    }
    next_table_mut(
        physical_memory_offset,
        entry,
        "x86 MMIO direct-map P2 entry",
    )
}

fn split_p3_huge_page<'a>(
    physical_memory_offset: usize,
    entry: &'a mut PageTableEntry,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> &'a mut PageTable {
    let original_flags = entry.flags();
    let base = entry.addr().as_u64();
    let (frame, table) = allocate_page_table(physical_memory_offset, frame_allocator);
    for (index, child_entry) in table.iter_mut().enumerate() {
        let address = base
            .checked_add((index as u64) * Size2MiB::SIZE)
            .unwrap_or_else(|| panic!("x86 split 1GiB direct-map overflow"));
        child_entry.set_addr(
            PhysAddr::new(address),
            original_flags | PageTableFlags::HUGE_PAGE,
        );
    }
    entry.set_frame(frame, parent_table_flags(original_flags));
    table
}

fn split_p2_huge_page<'a>(
    physical_memory_offset: usize,
    entry: &'a mut PageTableEntry,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> &'a mut PageTable {
    let original_flags = entry.flags();
    let leaf_flags = original_flags & !PageTableFlags::HUGE_PAGE;
    let base = entry.addr().as_u64();
    let (frame, table) = allocate_page_table(physical_memory_offset, frame_allocator);
    for (index, child_entry) in table.iter_mut().enumerate() {
        let address = base
            .checked_add((index as u64) * Size4KiB::SIZE)
            .unwrap_or_else(|| panic!("x86 split 2MiB direct-map overflow"));
        let frame = PhysFrame::<Size4KiB>::from_start_address(PhysAddr::new(address))
            .unwrap_or_else(|error| panic!("invalid x86 split 2MiB frame address: {error:?}"));
        child_entry.set_frame(frame, leaf_flags);
    }
    entry.set_frame(frame, parent_table_flags(original_flags));
    table
}

fn allocate_page_table(
    physical_memory_offset: usize,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> (PhysFrame<Size4KiB>, &'static mut PageTable) {
    let frame = frame_allocator
        .allocate_frame()
        .unwrap_or_else(|| panic!("failed to allocate x86 page-table frame for MMIO split"));
    let virtual_address = frame
        .start_address()
        .as_u64()
        .checked_add(physical_memory_offset as u64)
        .unwrap_or_else(|| panic!("x86 page-table frame virtual address overflow"));
    let table = unsafe { &mut *(virtual_address as *mut PageTable) };
    table.zero();
    (frame, table)
}

fn parent_table_flags(flags: PageTableFlags) -> PageTableFlags {
    let mut parent = PageTableFlags::PRESENT;
    if flags.contains(PageTableFlags::WRITABLE) {
        parent |= PageTableFlags::WRITABLE;
    }
    if flags.contains(PageTableFlags::USER_ACCESSIBLE) {
        parent |= PageTableFlags::USER_ACCESSIBLE;
    }
    if flags.contains(PageTableFlags::WRITE_THROUGH) {
        parent |= PageTableFlags::WRITE_THROUGH;
    }
    if flags.contains(PageTableFlags::NO_CACHE) {
        parent |= PageTableFlags::NO_CACHE;
    }
    parent
}

pub(crate) unsafe fn current_mapper(physical_memory_offset: usize) -> OffsetPageTable<'static> {
    let (level_4, _) = Cr3::read();
    let virtual_address = level_4
        .start_address()
        .as_u64()
        .checked_add(physical_memory_offset as u64)
        .unwrap_or_else(|| panic!("x86 level-4 page table virtual address overflow"));
    let table = unsafe { &mut *(virtual_address as *mut PageTable) };
    unsafe { OffsetPageTable::new(table, VirtAddr::new(physical_memory_offset as u64)) }
}

pub(crate) struct DirectMappedFrameAllocator {
    pub(crate) physical_memory_offset: usize,
}

unsafe impl FrameAllocator<Size4KiB> for DirectMappedFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let layout = Layout::from_size_align(Size4KiB::SIZE as usize, Size4KiB::SIZE as usize)
            .unwrap_or_else(|_| panic!("failed to build x86 page-table frame layout"));
        let pointer = unsafe { alloc_zeroed(layout) };
        let pointer = NonNull::new(pointer)
            .unwrap_or_else(|| panic!("failed to allocate x86 page-table frame"));
        let virtual_address = pointer.as_ptr() as usize;
        assert!(
            virtual_address >= self.physical_memory_offset,
            "page-table frame was allocated outside the physical-memory direct map"
        );
        let physical_address = virtual_address - self.physical_memory_offset;
        Some(
            PhysFrame::from_start_address(PhysAddr::new(physical_address as u64)).unwrap_or_else(
                |error| {
                    panic!("allocated x86 page-table frame had invalid physical address: {error:?}")
                },
            ),
        )
    }
}

fn allocate_aligned_zeroed(size: usize, align: usize) -> usize {
    let layout = Layout::from_size_align(size, align).unwrap_or_else(|_| {
        panic!("failed to build aligned allocation layout size={size} align={align}")
    });
    let pointer = unsafe { alloc_zeroed(layout) };
    NonNull::new(pointer)
        .unwrap_or_else(|| panic!("aligned allocation of {size} bytes failed"))
        .as_ptr() as usize
}

use helios_hal::{align_down, align_up};

fn wakeup_image() -> &'static [u8] {
    unsafe {
        let start = &helios_x86_secondary_wakeup_start as *const u8 as usize;
        let end = &helios_x86_secondary_wakeup_end as *const u8 as usize;
        core::slice::from_raw_parts(start as *const u8, end - start)
    }
}

fn wakeup_cr3_offset() -> usize {
    unsafe { wakeup_symbol_offset(&helios_x86_secondary_wakeup_cr3) }
}

fn wakeup_stack_top_offset() -> usize {
    unsafe { wakeup_symbol_offset(&helios_x86_secondary_wakeup_stack_top) }
}

fn wakeup_context_offset() -> usize {
    unsafe { wakeup_symbol_offset(&helios_x86_secondary_wakeup_context) }
}

fn wakeup_runtime_offset() -> usize {
    unsafe { wakeup_symbol_offset(&helios_x86_secondary_wakeup_runtime) }
}

fn wakeup_entry_offset() -> usize {
    unsafe { wakeup_symbol_offset(&helios_x86_secondary_wakeup_entry) }
}

unsafe fn wakeup_symbol_offset(symbol: &'static u8) -> usize {
    (symbol as *const u8 as usize)
        .checked_sub(unsafe { &helios_x86_secondary_wakeup_start as *const u8 as usize })
        .unwrap_or_else(|| panic!("x86 wakeup symbol precedes image start"))
}

unsafe extern "C" {
    static helios_x86_secondary_wakeup_start: u8;
    static helios_x86_secondary_wakeup_end: u8;
    static helios_x86_secondary_wakeup_cr3: u8;
    static helios_x86_secondary_wakeup_stack_top: u8;
    static helios_x86_secondary_wakeup_context: u8;
    static helios_x86_secondary_wakeup_runtime: u8;
    static helios_x86_secondary_wakeup_entry: u8;
}
