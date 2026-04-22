extern crate alloc;

use acpi::platform::{AcpiPlatform, ProcessorState};
use acpi::{AcpiTables, Handler, PciAddress, PhysicalMapping};
use alloc::alloc::{Layout, alloc_zeroed};
use alloc::boxed::Box;
use alloc::sync::Arc;
use bootloader_api::BootInfo;
use core::arch::x86_64::__cpuid;
use core::ops::Range;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use helios_hal::cpu::ProcessorId;
use pci_types::ConfigRegionAccess;
use x86::apic::x2apic::X2APIC;
use x86::apic::xapic::XAPIC;
use x86::apic::{ApicControl, ApicId};
use x86::msr::{IA32_FS_BASE, rdmsr, wrmsr};
use x86_64::PhysAddr;
use x86_64::VirtAddr;
use x86_64::instructions::tlb;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageSize, PageTable, PageTableFlags, PhysFrame,
    Size2MiB, Size4KiB,
};

use crate::KERNEL_STACK_BYTES;
use crate::debug_state;
use crate::pci::LegacyPciConfigAccess;
use crate::read_tsc;
use crate::watchdog::X86Watchdog;

const WAKEUP_PAGE_BYTES: usize = 4096;
const SIPI_MAX_PHYSICAL_ADDRESS: usize = 0x10_0000;
const AP_STARTUP_INIT_ASSERT_MICROS: u64 = 10_000;
const AP_STARTUP_INTER_IPI_MICROS: u64 = 200;
const PAGE_BYTES: usize = 4096;

#[repr(C)]
pub(crate) struct ProcessorRuntime {
    logical_id: u16,
    _reserved: u16,
    pub(crate) wasmtime_tls: AtomicPtr<u8>,
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

pub(crate) fn reserve_wakeup_page(boot_info: &'static BootInfo) -> Range<usize> {
    boot_info
        .memory_regions
        .iter()
        .find_map(|region| {
            if region.kind != bootloader_api::info::MemoryRegionKind::Usable {
                return None;
            }
            let start = align_up(region.start as usize, WAKEUP_PAGE_BYTES);
            let end = (region.end as usize).min(SIPI_MAX_PHYSICAL_ADDRESS);
            (start
                .checked_add(WAKEUP_PAGE_BYTES)
                .is_some_and(|next| next <= end))
            .then_some(start..start + WAKEUP_PAGE_BYTES)
        })
        .unwrap_or_else(|| panic!("failed to reserve a low-memory wakeup page for x86 AP startup"))
}

pub(crate) fn build_boot_context(
    boot_info: &'static BootInfo,
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
    let rsdp = boot_info
        .rsdp_addr
        .into_option()
        .unwrap_or_else(|| panic!("bootloader did not provide an RSDP address"));
    let tables = unsafe { AcpiTables::from_rsdp(handler.clone(), rsdp as usize) }
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
            wasmtime_tls: AtomicPtr::new(core::ptr::null_mut()),
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
                wasmtime_tls: AtomicPtr::new(core::ptr::null_mut()),
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
    runtime.started.store(true, Ordering::Release);
}

pub(crate) fn current_runtime() -> &'static ProcessorRuntime {
    let runtime = unsafe { rdmsr(IA32_FS_BASE) as *const ProcessorRuntime };
    assert!(
        !runtime.is_null(),
        "x86 processor runtime was not installed before use"
    );
    unsafe { &*runtime }
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

    pub(crate) fn physical_memory_offset(&self) -> usize {
        self.physical_memory_offset
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

    pub(crate) fn bootstrap_processor(&self) -> ProcessorId {
        ProcessorId::new(0)
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
    let cpuid = unsafe { __cpuid(1) };
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
    let virtual_base = physical_base
        .checked_add(physical_memory_offset)
        .unwrap_or_else(|| panic!("x86 local APIC virtual address overflow"));
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
        ticks.saturating_mul(1_000_000_000) / self.tsc_hz
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

    for virtual_address in (map_start..map_start + map_bytes).step_by(PAGE_BYTES) {
        let page = Page::<Size4KiB>::from_start_address(VirtAddr::new(virtual_address as u64))
            .unwrap_or_else(|error| panic!("invalid x86 MMIO virtual page: {error:?}"));
        let entry =
            ensure_direct_mapped_leaf_entry(physical_memory_offset, page, &mut frame_allocator);
        entry.set_flags(entry.flags() | PageTableFlags::NO_CACHE | PageTableFlags::WRITE_THROUGH);
        tlb::flush(VirtAddr::new(virtual_address as u64));
    }

    virtual_start
}

fn ensure_direct_mapped_leaf_entry(
    physical_memory_offset: usize,
    page: Page<Size4KiB>,
    frame_allocator: &mut DirectMappedFrameAllocator,
) -> &mut x86_64::structures::paging::page_table::PageTableEntry {
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
    entry: &'a mut x86_64::structures::paging::page_table::PageTableEntry,
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
    entry: &'a mut x86_64::structures::paging::page_table::PageTableEntry,
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
    entry: &'a mut x86_64::structures::paging::page_table::PageTableEntry,
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
    entry: &'a mut x86_64::structures::paging::page_table::PageTableEntry,
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
    entry: &'a mut x86_64::structures::paging::page_table::PageTableEntry,
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

unsafe fn current_mapper(physical_memory_offset: usize) -> OffsetPageTable<'static> {
    let (level_4, _) = Cr3::read();
    let virtual_address = level_4
        .start_address()
        .as_u64()
        .checked_add(physical_memory_offset as u64)
        .unwrap_or_else(|| panic!("x86 level-4 page table virtual address overflow"));
    let table = unsafe { &mut *(virtual_address as *mut PageTable) };
    unsafe { OffsetPageTable::new(table, VirtAddr::new(physical_memory_offset as u64)) }
}

struct DirectMappedFrameAllocator {
    physical_memory_offset: usize,
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

use helios_hal::align_up;

fn align_down(value: usize, align: usize) -> usize {
    assert!(align.is_power_of_two(), "alignment must be a power of two");
    value & !(align - 1)
}

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
