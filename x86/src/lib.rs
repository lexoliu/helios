#![no_std]
#![no_main]

extern crate alloc;

mod host_fs;
mod pci;
mod smp;
mod watchdog;

mod debug_state {
    pub(crate) type RuntimeState =
        helios_kernel::HostRuntimeState<crate::X86Cpu, crate::host_fs::HostFileSystemService>;
}

use alloc::sync::Arc;
use bootloader_api::config::Mapping;
use bootloader_api::info::MemoryRegionKind;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use core::arch::asm;
use core::arch::global_asm;
use core::arch::x86_64::{__cpuid, __cpuid_count, _rdtsc};
use core::fmt::{self, Write};
use core::ops::Range;
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::memory::MemoryRegion;
use helios_hal::serial::ByteSerial;
use helios_hal::watchdog::Watchdog;
use helios_hal::{DeviceInventory, DmaModel, Platform, ProcessorStartupPolicy, ProcessorTopology};
use x86_64::instructions::port::{Port, PortReadOnly, PortWriteOnly};
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr4, Cr4Flags};

const COM1_BASE: u16 = 0x3f8;
const COM1_DATA: u16 = COM1_BASE;
const COM1_INTERRUPT_ENABLE: u16 = COM1_BASE + 1;
const COM1_FIFO_CONTROL: u16 = COM1_BASE + 2;
const COM1_LINE_CONTROL: u16 = COM1_BASE + 3;
const COM1_MODEM_CONTROL: u16 = COM1_BASE + 4;
const COM1_LINE_STATUS: u16 = COM1_BASE + 5;
const LSR_DATA_READY: u8 = 0x01;
const LSR_TX_EMPTY: u8 = 0x20;
const PIT_COMMAND: u16 = 0x43;
const PIT_CHANNEL2_DATA: u16 = 0x42;
const PIT_SPEAKER_GATE: u16 = 0x61;
const PIT_BASE_HZ: u64 = 1_193_182;
const PIT_CALIBRATION_HZ: u64 = 100;
const PAGE_BYTES: usize = 4096;
const PAGE_MASK: usize = PAGE_BYTES - 1;
const PAGE_PRESENT: u64 = 1 << 0;
const PAGE_HUGE: u64 = 1 << 7;
const PAGE_NO_EXECUTE: u64 = 1 << 63;
pub(crate) const KERNEL_STACK_BYTES: usize = 4 * 1024 * 1024;
const WATCHDOG_SELF_TEST_ENABLED: bool = option_env!("HELIOS_WATCHDOG_SELF_TEST").is_some();

global_asm!(include_str!("secondary_wakeup.S"));

static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config.kernel_stack_size = KERNEL_STACK_BYTES as u64;
    config
};

struct X86CriticalSection;

critical_section::set_impl!(X86CriticalSection);

unsafe impl critical_section::Impl for X86CriticalSection {
    unsafe fn acquire() -> bool {
        let interrupts_were_enabled = x86_64::instructions::interrupts::are_enabled();
        x86_64::instructions::interrupts::disable();
        interrupts_were_enabled
    }

    unsafe fn release(restore_state: bool) {
        if restore_state {
            x86_64::instructions::interrupts::enable();
        }
    }
}

entry_point!(x86_kernel_main, config = &BOOTLOADER_CONFIG);

fn x86_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    serial_uart_init();
    enable_fpu_simd();
    let physical_memory_offset = boot_physical_memory_offset(boot_info);
    let processor_count = processor_count(boot_info);
    let wakeup_page = (processor_count > 1).then(|| smp::reserve_wakeup_page(boot_info));
    helios_kernel::prime_bootstrap_allocator(boot_memory_regions(
        boot_info,
        physical_memory_offset,
        wakeup_page.clone(),
    ));
    let memory_regions =
        boot_memory_regions(boot_info, physical_memory_offset, wakeup_page.clone());
    let tsc_hz = detect_tsc_frequency_hz();
    let tsc_base = read_tsc();
    let debug_state = debug_state::RuntimeState::new(tsc_hz, processor_count, 0);
    let boot = smp::build_boot_context(
        boot_info,
        physical_memory_offset,
        wakeup_page,
        tsc_base,
        tsc_hz,
        debug_state.clone(),
    );
    smp::activate_runtime(boot.bootstrap_runtime());
    let mirror_to_uart = WATCHDOG_SELF_TEST_ENABLED;
    let console = serial_console(debug_state.clone(), mirror_to_uart);
    let cpu = X86Cpu::new(boot.platform());
    let kernel = helios_kernel::init_with_watchdog(
        Platform::with_watchdog(console, memory_regions, cpu.clone(), cpu.watchdog())
            .with_topology(
                ProcessorTopology::start_all_secondaries(
                    cpu.bootstrap_processor(),
                    cpu.processor_count(),
                )
                .with_startup_policy(ProcessorStartupPolicy::BootstrapOnly),
            )
            .with_dma_model(DmaModel::Translated)
            .with_devices(DeviceInventory::new().with_debug_serial()),
    );
    let debug_state = cpu.debug_state();
    let _program_service =
        helios_kernel::install_component_host_program_service(&kernel, &cpu, &debug_state);
    if cpu.current_processor() == cpu.bootstrap_processor() {
        for processor in helios_kernel::component_host_processors_to_start(
            cpu.processor_count(),
            cpu.bootstrap_processor(),
        ) {
            cpu.start_processor(processor);
        }
    }
    run_current_processor(cpu, kernel, debug_state)
}

fn enable_fpu_simd() {
    unsafe {
        let mut cr0 = Cr0::read();
        cr0.remove(Cr0Flags::EMULATE_COPROCESSOR | Cr0Flags::TASK_SWITCHED);
        cr0.insert(Cr0Flags::MONITOR_COPROCESSOR);
        Cr0::write(cr0);

        let mut cr4 = Cr4::read();
        cr4.insert(Cr4Flags::OSFXSR | Cr4Flags::OSXMMEXCPT_ENABLE);
        Cr4::write(cr4);

        asm!("fninit", options(nostack, preserves_flags));
    }
}

fn boot_physical_memory_offset(boot_info: &'static BootInfo) -> usize {
    match boot_info.physical_memory_offset.into_option() {
        Some(offset) => offset as usize,
        None => panic!("bootloader did not provide a physical memory mapping"),
    }
}

fn processor_count(boot_info: &'static BootInfo) -> usize {
    let rsdp = boot_info
        .rsdp_addr
        .into_option()
        .unwrap_or_else(|| panic!("bootloader did not provide an RSDP address"));
    let handler = smp::PhysicalOffsetAcpiHandler {
        physical_memory_offset: boot_physical_memory_offset(boot_info),
        tsc_base: 0,
        tsc_hz: 1,
    };
    let tables = unsafe { acpi::AcpiTables::from_rsdp(handler.clone(), rsdp as usize) }
        .unwrap_or_else(|error| {
            panic!("failed to parse ACPI tables for processor count: {error:?}")
        });
    let platform = acpi::platform::AcpiPlatform::new(tables, handler)
        .unwrap_or_else(|error| panic!("failed to construct ACPI platform info: {error:?}"));
    let processor_info = platform
        .processor_info
        .as_ref()
        .unwrap_or_else(|| panic!("ACPI platform info did not expose processor topology"));
    1 + processor_info.application_processors.len()
}

fn boot_memory_regions(
    boot_info: &'static BootInfo,
    physical_memory_offset: usize,
    excluded: Option<Range<usize>>,
) -> impl IntoIterator<Item = MemoryRegion> {
    boot_info.memory_regions.iter().flat_map(move |region| {
        usable_region_segments(region, excluded.as_ref())
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
    region: &bootloader_api::info::MemoryRegion,
    excluded: Option<&Range<usize>>,
) -> [Option<Range<usize>>; 2] {
    if region.kind != MemoryRegionKind::Usable || region.end <= region.start {
        return [None, None];
    }

    let start = region.start as usize;
    let end = region.end as usize;
    let Some(excluded) = excluded else {
        return [Some(start..end), None];
    };

    if excluded.end <= start || excluded.start >= end {
        return [Some(start..end), None];
    }

    [
        (excluded.start > start).then_some(start..excluded.start),
        (excluded.end < end).then_some(excluded.end..end),
    ]
}

#[cfg(test)]
mod tests {
    use super::usable_region_segments;
    use bootloader_api::info::{MemoryRegion, MemoryRegionKind};
    use core::ops::Range;

    #[test]
    fn usable_region_without_overlap_stays_single_segment() {
        assert_eq!(
            collect_segments(region(0x1000..0x4000), Some(&(0x5000..0x6000))),
            alloc::vec![0x1000..0x4000]
        );
    }

    #[test]
    fn excluded_wakeup_page_splits_region_without_heap_growth_path() {
        assert_eq!(
            collect_segments(region(0x1000..0x5000), Some(&(0x2000..0x3000))),
            alloc::vec![0x1000..0x2000, 0x3000..0x5000]
        );
    }

    #[test]
    fn non_usable_region_yields_no_segments() {
        assert!(
            collect_segments(
                MemoryRegion {
                    start: 0x1000,
                    end: 0x4000,
                    kind: MemoryRegionKind::Bootloader,
                },
                None,
            )
            .is_empty()
        );
    }

    fn collect_segments(
        region: MemoryRegion,
        excluded: Option<&Range<usize>>,
    ) -> alloc::vec::Vec<Range<usize>> {
        usable_region_segments(&region, excluded)
            .into_iter()
            .flatten()
            .collect()
    }

    fn region(range: Range<usize>) -> MemoryRegion {
        MemoryRegion {
            start: range.start as u64,
            end: range.end as u64,
            kind: MemoryRegionKind::Usable,
        }
    }
}

fn serial_console(
    debug_state: debug_state::RuntimeState,
    mirror_to_uart: bool,
) -> helios_kernel::RecordingConsole<
    debug_state::RuntimeState,
    impl FnMut() -> u64,
    impl FnMut(&[u8]),
> {
    serial_uart_init();
    let write_fn: Option<fn(&[u8])> = if mirror_to_uart {
        Some(|bytes: &[u8]| {
            for &byte in bytes {
                serial_write_byte(byte);
            }
        })
    } else {
        None
    };
    helios_kernel::RecordingConsole::new(debug_state, || read_tsc(), write_fn)
}

#[derive(Clone)]
pub(crate) struct X86Cpu {
    state: Arc<smp::X86PlatformState>,
}

impl X86Cpu {
    fn new(state: Arc<smp::X86PlatformState>) -> Self {
        Self { state }
    }

    pub(crate) fn debug_state(&self) -> debug_state::RuntimeState {
        self.state.debug_state()
    }

    pub(crate) fn watchdog(&self) -> crate::watchdog::X86Watchdog {
        self.state.watchdog()
    }
}

impl helios_kernel::CodegenPlatform for X86Cpu {
    fn publish_executable(&self, ptr: *const u8, len: usize) {
        if len == 0 {
            return;
        }

        let start = ptr as usize;
        let end = start
            .checked_add(len - 1)
            .unwrap_or_else(|| panic!("code publish range overflow: start={start:#x}, len={len}"));
        let first_page = start & !PAGE_MASK;
        let last_page = end & !PAGE_MASK;
        let mut page = first_page;
        loop {
            unsafe {
                clear_no_execute_bit(page, self.state.physical_memory_offset());
                asm!(
                    "invlpg [{addr}]",
                    addr = in(reg) page,
                    options(nostack, preserves_flags),
                );
            }
            if page == last_page {
                break;
            }
            page = page
                .checked_add(PAGE_BYTES)
                .unwrap_or_else(|| panic!("code publish page iteration overflow at {page:#x}"));
        }
    }

    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
        Some(detect_x86_native_feature)
    }
}

impl Cpu for X86Cpu {
    fn current_processor(&self) -> ProcessorId {
        self.state.current_processor()
    }

    fn processor_count(&self) -> usize {
        self.state.processor_count()
    }

    fn bootstrap_processor(&self) -> ProcessorId {
        self.state.bootstrap_processor()
    }

    fn park_current(&self) {
        core::hint::spin_loop();
    }

    fn start_processor(&self, processor: ProcessorId) {
        self.state
            .start_processor(processor, secondary_start_rust as *const () as usize);
    }

    fn wake_processor(&self, _processor: ProcessorId) {}

    fn now(&self) -> Instant {
        Instant::new(read_tsc().saturating_sub(self.state.tsc_base()))
    }

    fn timer_frequency(&self) -> u64 {
        self.state.tsc_hz()
    }

    fn set_deadline(&self, _deadline: Instant) {}

    fn shutdown(&self) -> ! {
        loop {
            core::hint::spin_loop();
        }
    }

    fn reboot(&self) -> ! {
        unsafe {
            PortWriteOnly::new(0x64).write(0xfe_u8);
        }
        loop {
            core::hint::spin_loop();
        }
    }
}

fn run_current_processor<WatchdogImpl>(
    cpu: X86Cpu,
    kernel: helios_kernel::Kernel<X86Cpu, WatchdogImpl>,
    debug_state: debug_state::RuntimeState,
) -> !
where
    WatchdogImpl: Watchdog + Clone,
{
    helios_kernel::run_component_host_processor_forever(
        cpu,
        kernel,
        debug_state,
        read_debug_serial,
        write_debug_serial_bytes,
    );
}

#[unsafe(no_mangle)]
extern "C" fn secondary_start_rust(
    boot: *const smp::BootContext,
    runtime: *const smp::ProcessorRuntime,
) -> ! {
    serial_uart_init();
    enable_fpu_simd();
    let boot = unsafe { &*boot };
    let runtime = unsafe { &*runtime };
    smp::activate_runtime(runtime);

    let debug_state = boot.platform().debug_state();
    let console = serial_console(debug_state.clone(), WATCHDOG_SELF_TEST_ENABLED);
    let cpu = X86Cpu::new(boot.platform());
    let kernel = helios_kernel::init_with_watchdog(Platform::with_watchdog(
        console,
        core::iter::empty::<MemoryRegion>(),
        cpu.clone(),
        cpu.watchdog(),
    ));
    run_current_processor(cpu, kernel, debug_state)
}

fn detect_x86_native_feature(feature: &str) -> Option<bool> {
    fn xgetbv0() -> u64 {
        let eax: u32;
        let edx: u32;
        unsafe {
            asm!(
                "xgetbv",
                in("ecx") 0_u32,
                lateout("eax") eax,
                lateout("edx") edx,
                options(nomem, nostack, preserves_flags),
            );
        }
        ((edx as u64) << 32) | u64::from(eax)
    }

    let max_basic = unsafe { __cpuid(0) }.eax;
    let max_extended = unsafe { __cpuid(0x8000_0000) }.eax;
    let leaf1 = unsafe { __cpuid(1) };
    let leaf7 = (max_basic >= 7).then(|| unsafe { __cpuid_count(7, 0) });
    let osxsave = (leaf1.ecx & (1 << 27)) != 0;
    let xcr0 = osxsave.then(xgetbv0).unwrap_or(0);
    let avx_os_enabled = (leaf1.ecx & (1 << 28)) != 0 && (xcr0 & 0b110) == 0b110;
    let avx512_os_enabled = avx_os_enabled && (xcr0 & 0b1110_0000) == 0b1110_0000;
    match feature {
        "cmpxchg16b" => Some(leaf1.ecx & (1 << 13) != 0),
        "sse3" => Some(leaf1.ecx & (1 << 0) != 0),
        "ssse3" => Some(leaf1.ecx & (1 << 9) != 0),
        "sse4.1" => Some(leaf1.ecx & (1 << 19) != 0),
        "sse4.2" => Some(leaf1.ecx & (1 << 20) != 0),
        "popcnt" => Some(leaf1.ecx & (1 << 23) != 0),
        "avx" => Some(avx_os_enabled),
        "fma" => Some(leaf1.ecx & (1 << 12) != 0 && avx_os_enabled),
        "bmi1" => Some(leaf7.is_some_and(|leaf| leaf.ebx & (1 << 3) != 0)),
        "bmi2" => Some(leaf7.is_some_and(|leaf| leaf.ebx & (1 << 8) != 0)),
        "avx512bitalg" => {
            Some(leaf7.is_some_and(|leaf| leaf.ecx & (1 << 12) != 0) && avx512_os_enabled)
        }
        "avx512dq" => {
            Some(leaf7.is_some_and(|leaf| leaf.ebx & (1 << 17) != 0) && avx512_os_enabled)
        }
        "avx512f" => Some(leaf7.is_some_and(|leaf| leaf.ebx & (1 << 16) != 0) && avx512_os_enabled),
        "avx512vl" => {
            Some(leaf7.is_some_and(|leaf| leaf.ebx & (1 << 31) != 0) && avx512_os_enabled)
        }
        "avx512vbmi" => {
            Some(leaf7.is_some_and(|leaf| leaf.ecx & (1 << 1) != 0) && avx512_os_enabled)
        }
        "lzcnt" => {
            Some(max_extended >= 0x8000_0001 && unsafe { __cpuid(0x8000_0001) }.ecx & (1 << 5) != 0)
        }
        _ => None,
    }
}

unsafe fn clear_no_execute_bit(virtual_addr: usize, physical_memory_offset: usize) {
    let (level_4_frame, _) = Cr3::read();
    let level_4 = unsafe {
        page_table_from_physical(
            level_4_frame.start_address().as_u64() as usize,
            physical_memory_offset,
        )
    };

    let level_4_index = (virtual_addr >> 39) & 0x1ff;
    let level_3_index = (virtual_addr >> 30) & 0x1ff;
    let level_2_index = (virtual_addr >> 21) & 0x1ff;
    let level_1_index = (virtual_addr >> 12) & 0x1ff;

    let level_4_entry = level_4[level_4_index];
    assert!(
        level_4_entry & PAGE_PRESENT != 0,
        "missing level-4 page-table entry for virtual address {virtual_addr:#x}"
    );
    let level_3 = unsafe {
        page_table_from_physical(entry_frame_address(level_4_entry), physical_memory_offset)
    };
    let level_3_entry = level_3[level_3_index];
    assert!(
        level_3_entry & PAGE_PRESENT != 0,
        "missing level-3 page-table entry for virtual address {virtual_addr:#x}"
    );
    if level_3_entry & PAGE_HUGE != 0 {
        level_3[level_3_index] = level_3_entry & !PAGE_NO_EXECUTE;
        return;
    }

    let level_2 = unsafe {
        page_table_from_physical(entry_frame_address(level_3_entry), physical_memory_offset)
    };
    let level_2_entry = level_2[level_2_index];
    assert!(
        level_2_entry & PAGE_PRESENT != 0,
        "missing level-2 page-table entry for virtual address {virtual_addr:#x}"
    );
    if level_2_entry & PAGE_HUGE != 0 {
        level_2[level_2_index] = level_2_entry & !PAGE_NO_EXECUTE;
        return;
    }

    let level_1 = unsafe {
        page_table_from_physical(entry_frame_address(level_2_entry), physical_memory_offset)
    };
    let level_1_entry = level_1[level_1_index];
    assert!(
        level_1_entry & PAGE_PRESENT != 0,
        "missing level-1 page-table entry for virtual address {virtual_addr:#x}"
    );
    level_1[level_1_index] = level_1_entry & !PAGE_NO_EXECUTE;
}

unsafe fn page_table_from_physical(
    table_physical_addr: usize,
    physical_memory_offset: usize,
) -> &'static mut [u64; 512] {
    let table_virtual_addr = table_physical_addr
        .checked_add(physical_memory_offset)
        .unwrap_or_else(|| {
            panic!(
                "physical memory offset overflow: phys={table_physical_addr:#x}, offset={physical_memory_offset:#x}"
            )
        });
    let table_ptr = table_virtual_addr as *mut [u64; 512];
    unsafe { &mut *table_ptr }
}

fn entry_frame_address(entry: u64) -> usize {
    (entry as usize) & 0x000f_ffff_ffff_f000
}

fn read_tsc() -> u64 {
    unsafe { _rdtsc() }
}

fn detect_tsc_frequency_hz() -> u64 {
    let max_basic = unsafe { __cpuid(0) }.eax;
    if max_basic >= 0x15 {
        let leaf_15 = unsafe { __cpuid(0x15) };
        let denominator = u64::from(leaf_15.eax);
        let numerator = u64::from(leaf_15.ebx);
        let crystal_hz = u64::from(leaf_15.ecx);
        if denominator != 0 && numerator != 0 && crystal_hz != 0 {
            let tsc_hz = (u128::from(crystal_hz) * u128::from(numerator)) / u128::from(denominator);
            let tsc_hz =
                u64::try_from(tsc_hz).expect("computed TSC frequency does not fit into u64");
            assert!(tsc_hz != 0, "computed TSC frequency is zero");
            return tsc_hz;
        }
    }

    if max_basic >= 0x16 {
        let base_mhz = u64::from(unsafe { __cpuid(0x16) }.eax);
        if base_mhz != 0 {
            let tsc_hz = base_mhz.saturating_mul(1_000_000);
            assert!(tsc_hz != 0, "computed TSC frequency is zero");
            return tsc_hz;
        }
    }

    calibrate_tsc_via_pit()
}

fn calibrate_tsc_via_pit() -> u64 {
    let pit_count = u16::try_from(PIT_BASE_HZ / PIT_CALIBRATION_HZ)
        .expect("PIT calibration divisor overflowed u16");

    unsafe {
        let mut speaker: Port<u8> = Port::new(PIT_SPEAKER_GATE);
        let speaker_state = speaker.read();

        // Route PIT channel 2 output to the gate without enabling the speaker.
        speaker.write((speaker_state | 0x01) & !0x02);

        let mut command: PortWriteOnly<u8> = PortWriteOnly::new(PIT_COMMAND);
        let mut channel2: PortWriteOnly<u8> = PortWriteOnly::new(PIT_CHANNEL2_DATA);
        command.write(0xB0);
        channel2.write((pit_count & 0x00ff) as u8);
        channel2.write((pit_count >> 8) as u8);

        let start = read_tsc();
        loop {
            let state = speaker.read();
            if state & 0x20 != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        let elapsed = read_tsc().saturating_sub(start);

        speaker.write(speaker_state);

        let tsc_hz = elapsed.saturating_mul(PIT_CALIBRATION_HZ);
        assert!(
            tsc_hz != 0,
            "PIT-based TSC calibration produced zero frequency"
        );
        tsc_hz
    }
}

fn serial_uart_init() {
    unsafe {
        PortWriteOnly::new(COM1_INTERRUPT_ENABLE).write(0x00_u8);
        PortWriteOnly::new(COM1_LINE_CONTROL).write(0x80_u8);
        PortWriteOnly::new(COM1_DATA).write(0x01_u8);
        PortWriteOnly::new(COM1_INTERRUPT_ENABLE).write(0x00_u8);
        PortWriteOnly::new(COM1_LINE_CONTROL).write(0x03_u8);
        PortWriteOnly::new(COM1_FIFO_CONTROL).write(0xc7_u8);
        PortWriteOnly::new(COM1_MODEM_CONTROL).write(0x0b_u8);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DebugSerial;

impl ByteSerial for DebugSerial {
    fn try_read_byte(&self) -> Option<u8> {
        unsafe {
            let mut status: PortReadOnly<u8> = PortReadOnly::new(COM1_LINE_STATUS);
            if status.read() & LSR_DATA_READY == 0 {
                return None;
            }
            Some(Port::new(COM1_DATA).read())
        }
    }

    fn write_bytes(&self, bytes: &[u8]) {
        for &byte in bytes {
            while !serial_tx_ready() {
                core::hint::spin_loop();
            }
            unsafe {
                PortWriteOnly::new(COM1_DATA).write(byte);
            }
        }
    }
}

fn serial_write_byte(byte: u8) {
    while !serial_tx_ready() {
        core::hint::spin_loop();
    }
    unsafe {
        PortWriteOnly::new(COM1_DATA).write(byte);
    }
}

struct PanicSerialWriter;

impl Write for PanicSerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_debug_serial_bytes(s.as_bytes());
        Ok(())
    }
}

fn serial_tx_ready() -> bool {
    unsafe {
        let mut status: PortReadOnly<u8> = PortReadOnly::new(COM1_LINE_STATUS);
        status.read() & LSR_TX_EMPTY != 0
    }
}

fn read_debug_serial(max_bytes: u32) -> alloc::vec::Vec<u8> {
    helios_kernel::try_read_serial(&DebugSerial, max_bytes)
}

pub(crate) fn write_debug_serial_bytes(bytes: &[u8]) {
    DebugSerial.write_bytes(bytes);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    serial_uart_init();
    let _ = writeln!(PanicSerialWriter, "{info}");
    helios_kernel::panic_log(info);
    loop {
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_get() -> *mut u8 {
    smp::current_runtime()
        .wasmtime_tls
        .load(core::sync::atomic::Ordering::Acquire)
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(ptr: *mut u8) {
    smp::current_runtime()
        .wasmtime_tls
        .store(ptr, core::sync::atomic::Ordering::Release);
}
