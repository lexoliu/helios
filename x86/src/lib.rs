#![no_std]
#![no_main]

extern crate alloc;

mod balloon;
mod block;
mod boot;
mod entropy;
mod exceptions;
mod host_fs;
mod iommu;
mod net;
mod pci;
mod rtc;
mod smp;
mod vsock;
mod watchdog;

mod debug_state {
    pub(crate) type RuntimeState =
        helios_kernel::HostRuntimeState<crate::X86Cpu, crate::host_fs::HostFileSystemService>;
    pub(crate) type ProgramService =
        helios_kernel::UserProgramService<crate::X86Cpu, crate::host_fs::HostFileSystemService>;
}

use alloc::sync::Arc;
use core::arch::asm;
use core::arch::global_asm;
use core::arch::x86_64::{__cpuid, __cpuid_count, _rdrand64_step, _rdtsc};
use core::ops::Range;
use core::sync::atomic::{AtomicUsize, Ordering};
use helios_hal::boot::{BootMemoryMap, BootReservedRanges, usable_region_segments};
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::critical_section::ProcessorIdentity;
use helios_hal::entropy::{EntropyQuality, EntropyUnavailable};
use helios_hal::memory::MemoryRegion;
use helios_hal::serial::ByteSerial;
use helios_hal::watchdog::Watchdog;
use helios_hal::{
    DeviceInventory, DmaModel, Platform, ProcessorStartupPolicy, ProcessorTopology, align_up,
};
use helios_kernel::DebugSerialAccess;
use x86_64::instructions::port::{Port, PortReadOnly, PortWriteOnly};
use x86_64::registers::control::{Cr0Flags, Cr4Flags};

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
pub(crate) const KERNEL_STACK_BYTES: usize = 4 * 1024 * 1024;
pub(crate) static WASMTIME_NATIVE_TRAP_HANDLER: AtomicUsize = AtomicUsize::new(0);
static CRITICAL_SECTION_STATE: helios_hal::critical_section::CriticalSectionState =
    helios_hal::critical_section::CriticalSectionState::new();

global_asm!(include_str!("secondary_wakeup.S"));

mod vmm;
pub use vmm::X86UserAddressSpace;

struct X86InterruptOps;

impl helios_hal::critical_section::InterruptOps for X86InterruptOps {
    fn interrupts_enabled() -> bool {
        x86_64::instructions::interrupts::are_enabled()
    }

    fn disable_interrupts() {
        x86_64::instructions::interrupts::disable();
    }

    unsafe fn enable_interrupts() {
        x86_64::instructions::interrupts::enable();
    }

    fn current_identity() -> ProcessorIdentity {
        smp::current_identity()
    }
}

struct X86CriticalSection;

critical_section::set_impl!(X86CriticalSection);

unsafe impl critical_section::Impl for X86CriticalSection {
    unsafe fn acquire() -> usize {
        unsafe { CRITICAL_SECTION_STATE.acquire::<X86InterruptOps>() }
    }

    unsafe fn release(restore_state: usize) {
        unsafe { CRITICAL_SECTION_STATE.release::<X86InterruptOps>(restore_state) }
    }
}

/// Kernel entry from the bootloader. The kernel is compiled with SSE
/// enabled, but the Limine x86_64 handoff does not guarantee an
/// SSE-enabled machine state (the Limine 9 EFI loader hands off with
/// CR4.OSFXSR clear), so no compiler-generated code may run before the
/// FPU/SSE control bits are set: LLVM freely emits SSE moves into early
/// boot code. This naked stub is the only pre-SSE code in the kernel.
#[unsafe(no_mangle)]
#[unsafe(naked)]
extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "mov rax, cr0",
        "and eax, {cr0_clear}",
        "or eax, {cr0_set}",
        "mov cr0, rax",
        "mov rax, cr4",
        "or eax, {cr4_set}",
        "mov cr4, rax",
        "fninit",
        "jmp {main}",
        // Clear EM and TS, set MP.
        cr0_clear = const !(Cr0Flags::EMULATE_COPROCESSOR.bits() as u32
            | Cr0Flags::TASK_SWITCHED.bits() as u32),
        cr0_set = const Cr0Flags::MONITOR_COPROCESSOR.bits() as u32,
        cr4_set = const (Cr4Flags::OSFXSR.bits() as u32
            | Cr4Flags::OSXMMEXCPT_ENABLE.bits() as u32),
        main = sym x86_kernel_main,
    )
}

fn x86_kernel_main() -> ! {
    // The one place COM1 is configured; see `serial_uart_init`.
    serial_uart_init();
    assert!(
        boot::base_revision_supported(),
        "Limine bootloader does not support the required base protocol revision"
    );
    let handoff = boot::limine_boot_handoff();
    let physical_memory_offset = boot::physical_memory_offset();
    let reserved_ranges = boot_reserved_ranges(&handoff);
    let reserved_wakeup_page = reserve_wakeup_page(&handoff, &reserved_ranges);
    let rsdp_address = handoff
        .tables
        .acpi_rsdp
        .unwrap_or_else(|| panic!("Limine handoff did not include an ACPI RSDP address"));
    let processor_count = processor_count(rsdp_address, physical_memory_offset);
    helios_kernel::prime_bootstrap_allocator(
        boot_memory_regions(
            &handoff,
            physical_memory_offset,
            &reserved_ranges,
            Some(reserved_wakeup_page.clone()),
        ),
        processor_count,
    );
    let wakeup_page = (processor_count > 1).then_some(reserved_wakeup_page);
    let memory_regions = boot_memory_regions(
        &handoff,
        physical_memory_offset,
        &reserved_ranges,
        wakeup_page.clone(),
    );
    // The memory a confined device is allowed to reach is exactly what
    // the allocator was primed with, captured here while the boot
    // handoff is still in hand.
    let dma_memory = dma_capable_ranges(
        &handoff,
        physical_memory_offset,
        &reserved_ranges,
        wakeup_page.clone(),
    );
    let tsc_hz = detect_tsc_frequency_hz();
    let tsc_base = read_tsc();
    let debug_state = debug_state::RuntimeState::new(tsc_hz, processor_count, 0);
    let boot = smp::build_boot_context(
        rsdp_address,
        physical_memory_offset,
        wakeup_page,
        tsc_base,
        tsc_hz,
        debug_state.clone(),
    );
    smp::activate_runtime(boot.bootstrap_runtime());
    exceptions::install_for_current_processor();
    let console = serial_console(debug_state.clone());
    let cpu = X86Cpu::new(boot.platform());
    vmm::install_user_address_space(physical_memory_offset, cpu.processor_count());
    let pci = pci::PciRoot::new(physical_memory_offset);
    // The translation topology is read before any device is programmed:
    // whether a function is confined decides which addresses its driver
    // may publish, and that has to be settled before the first ring is
    // allocated.
    let iommu_topology = iommu::discover(rsdp_address, physical_memory_offset);
    let network_function = net::discover(&pci);
    let host_share_function = host_fs::discover(&pci);
    let entropy_function = entropy::discover(&pci);
    let balloon_function = balloon::discover(&pci);
    let vsock_function = vsock::discover(&pci);
    let block_functions = block::discover(&pci);
    let mut devices = DeviceInventory::new().with_debug_serial();
    if network_function.is_some() {
        devices = devices.with_network();
    }
    if host_share_function.is_some() {
        devices = devices.with_host_share();
    }
    if entropy_function.is_some() {
        devices = devices.with_entropy_device();
    }
    if balloon_function.is_some() {
        devices = devices.with_memory_balloon();
    }
    if vsock_function.is_some() {
        devices = devices.with_vsock();
    }
    if !block_functions.is_empty() {
        devices = devices.with_block_devices(block_functions.len());
    }
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
            .with_devices(devices),
    );
    smp::current_runtime().install_timer(kernel.timer());
    let debug_state = cpu.debug_state();
    // The root DRBG is seeded before any component can ask for random
    // bytes. x86 has no firmware seed to read — the boot protocol here
    // is ACPI, not a device tree — so `RDSEED`/`RDRAND` is the
    // pre-executor source, and a processor without one refuses to boot
    // here rather than running with a predictable stream.
    //
    // Unlike the device-tree backends, this one cannot take the
    // bring-up read of its entropy device instead: that device is a PCI
    // function which has to be confined in its virtio-iommu domain
    // before it is programmed, and that pass also programs the block
    // device, which needs the root DRBG this call produces. It joins
    // through the reseed task once the executor runs.
    let root_entropy =
        helios_kernel::seed_root_entropy(&cpu, None, None::<&helios_kernel::NoEntropyDevice>);
    debug_state.install_root_entropy(root_entropy.clone());
    // The calendar is read once, here, before any component can ask
    // what time it is. The TSC carries wall time forward from that
    // reading; nothing re-synchronises it afterwards.
    match rtc::discover(rsdp_address, physical_memory_offset) {
        Some(rtc) => {
            debug_state.seed_wall_clock(cpu.now().ticks(), &rtc);
        }
        None => {
            tracing::warn!("the FADT points at no readable CMOS clock; wall time reads as uptime");
        }
    }
    // Devices are brought up while the bootstrap processor still runs
    // with interrupts masked, so their MSI-X routes are published before
    // the first message can be delivered.
    install_pci_devices(
        &cpu,
        &kernel,
        &pci,
        physical_memory_offset,
        iommu_topology,
        &dma_memory,
        network_function,
        host_share_function,
        entropy_function,
        balloon_function,
        vsock_function,
        &block_functions,
        &debug_state,
        root_entropy,
    );
    x86_64::instructions::interrupts::enable();
    let program_service = helios_kernel::install_component_host_program_service(
        &kernel,
        &cpu,
        &debug_state,
        read_debug_serial,
        write_debug_serial_bytes,
    );
    smp::current_runtime().install_program_service(
        program_service.unwrap_or_else(|| panic!("x86 bootstrap did not install program service")),
    );
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

/// Brings up the virtio-PCI devices the platform exposes and publishes
/// their MSI-X routes to every processor.
///
/// A device's configuration message is addressed to the bootstrap local
/// APIC, but a multi-queue network device steers each queue pair's
/// message to the processor that drains the pair, so every processor
/// has to be able to dispatch a device vector. This runs while the
/// bootstrap processor is the only one online and its interrupts are
/// still masked, so the table is in place before the first message can
/// be delivered anywhere. The routes are installed unconditionally so
/// an interrupt from a device the kernel never claimed fails loudly
/// instead of being silently acknowledged.
#[allow(clippy::too_many_arguments)]
fn install_pci_devices<WatchdogImpl>(
    cpu: &X86Cpu,
    kernel: &helios_kernel::Kernel<X86Cpu, WatchdogImpl>,
    pci: &pci::PciRoot,
    physical_memory_offset: usize,
    iommu_topology: Option<iommu::IommuTopology>,
    dma_memory: &[helios_hal::iommu::PhysicalRange],
    network_function: Option<pci_types::PciAddress>,
    host_share_function: Option<pci_types::PciAddress>,
    entropy_function: Option<pci_types::PciAddress>,
    balloon_function: Option<pci_types::PciAddress>,
    vsock_function: Option<pci_types::PciAddress>,
    block_functions: &[pci_types::PciAddress],
    debug_state: &debug_state::RuntimeState,
    root_entropy: helios_kernel::RootEntropyHandle,
) where
    WatchdogImpl: Watchdog + Clone,
{
    let destination_apic_id = cpu.bootstrap_apic_id();
    let mut routes = exceptions::DeviceInterruptRoutes::new();
    // Every virtio function the kernel is about to drive has to be in a
    // domain before it is programmed, so the whole set is confined in
    // one pass first.
    let confined: alloc::vec::Vec<pci_types::PciAddress> = host_share_function
        .into_iter()
        .chain(network_function)
        .chain(entropy_function)
        .chain(block_functions.iter().copied())
        .collect();
    let confinement = iommu_topology.map(|topology| {
        iommu::confine_devices(pci, topology, physical_memory_offset, dma_memory, &confined)
    });
    if let Some(confinement) = &confinement {
        let report = confinement.report();
        debug_state.install_iommu_report(report.clone());
        // The unit has no interrupt of its own to report a fault on, so
        // the kernel collects them on a task that sleeps between polls.
        kernel.spawn_detached(iommu::watch_faults(
            confinement.device(),
            report,
            kernel.timer(),
        ));
    } else {
        tracing::info!(
            "no virtio-iommu in the ACPI topology; device DMA reaches all of physical memory"
        );
    }
    let dma_pool = |address: pci_types::PciAddress| {
        iommu::dma_pool(confinement.as_ref(), address, physical_memory_offset)
    };
    if let Some(address) = host_share_function {
        let transport = host_fs::install(
            cpu,
            pci,
            address,
            dma_pool(address),
            exceptions::HOST_FS_INTERRUPT_VECTOR,
            destination_apic_id,
            debug_state,
        );
        routes.set_host_fs(exceptions::HOST_FS_INTERRUPT_VECTOR, transport);
    } else {
        tracing::warn!("virtio 9p device was not discovered on the PCI bus");
    }
    if let Some(address) = network_function {
        let interrupts = net::install(
            cpu,
            kernel,
            net::PciFunction {
                root: pci,
                address,
                dma: dma_pool(address),
            },
            net::MsixDelivery {
                vector: exceptions::NETWORK_INTERRUPT_VECTOR,
                destination_apic_id,
            },
            debug_state,
        );
        for (vector, handler) in interrupts.queues {
            routes.add_network(vector, handler);
        }
        routes.add_network(
            exceptions::NETWORK_INTERRUPT_VECTOR,
            interrupts.configuration,
        );
    } else {
        tracing::warn!("virtio network device was not discovered on the PCI bus");
    }
    if let Some(address) = entropy_function {
        let device = entropy::install(
            kernel,
            pci,
            address,
            dma_pool(address),
            exceptions::ENTROPY_INTERRUPT_VECTOR,
            destination_apic_id,
            root_entropy.clone(),
        );
        routes.set_entropy(exceptions::ENTROPY_INTERRUPT_VECTOR, device);
    } else {
        tracing::warn!("virtio entropy device was not discovered on the PCI bus");
    }
    if let Some(address) = vsock_function {
        let device = vsock::install(
            cpu,
            kernel,
            pci,
            address,
            physical_memory_offset,
            exceptions::VSOCK_INTERRUPT_VECTOR,
            destination_apic_id,
            debug_state,
        );
        routes.set_vsock(exceptions::VSOCK_INTERRUPT_VECTOR, device);
    } else {
        tracing::warn!("virtio vsock device was not discovered on the PCI bus");
    }
    if let Some(address) = balloon_function {
        let handle = balloon::install(kernel, pci, address, physical_memory_offset);
        debug_state.install_memory_balloon(handle);
    }
    // The x86 address space reserves and commits lazily, so a page could
    // be taken away here — but the backend has not wired the other half:
    // no `SwapVmHooks` table, so a not-present four-level entry carries no
    // swap token and the fault handler has nothing to look the page up by.
    // Extending swap to this backend is #25's follow-up to #59.
    helios_kernel::disable_swap(helios_kernel::SwapDisabled::NoSwapHooks);
    if block_functions.is_empty() {
        tracing::warn!("virtio block device was not discovered on the PCI bus");
    }
    let block_devices: alloc::vec::Vec<(pci_types::PciAddress, iommu::X86DmaPool)> =
        block_functions
            .iter()
            .map(|address| (*address, dma_pool(*address)))
            .collect();
    for block in block::install(
        cpu,
        kernel,
        pci,
        &block_devices,
        destination_apic_id,
        debug_state,
        root_entropy,
    ) {
        routes.add_block(block.vector, block.device);
    }
    cpu.platform_state().install_device_interrupts(routes);
}

// TODO(x86-avx): enable OSXSAVE, program XCR0, and preserve XSAVE state
// (in `_start` and the secondary wakeup trampoline) before advertising
// AVX/FMA/AVX512 to Wasmtime-generated code.

/// Counts usable processors from the MADT. Runs before the bootstrap
/// allocator is primed (the count sizes the allocator's per-processor
/// structures), so it must not allocate: iterate MADT entries directly
/// instead of building `AcpiPlatform` processor info.
fn processor_count(rsdp_address: usize, physical_memory_offset: usize) -> usize {
    use acpi::sdt::madt::{Madt, MadtEntry};

    const LAPIC_ENABLED: u32 = 1 << 0;
    const LAPIC_ONLINE_CAPABLE: u32 = 1 << 1;
    const LAPIC_USABLE: u32 = LAPIC_ENABLED | LAPIC_ONLINE_CAPABLE;

    let handler = smp::PhysicalOffsetAcpiHandler {
        physical_memory_offset,
        tsc_base: 0,
        tsc_hz: 1,
    };
    let tables =
        unsafe { acpi::AcpiTables::from_rsdp(handler, rsdp_address) }.unwrap_or_else(|error| {
            panic!("failed to parse ACPI tables for processor count: {error:?}")
        });
    let madt = tables
        .find_table::<Madt>()
        .unwrap_or_else(|| panic!("ACPI tables did not expose an MADT"));
    let count = madt
        .get()
        .entries()
        .filter(|entry| match entry {
            MadtEntry::LocalApic(apic) => apic.flags & LAPIC_USABLE != 0,
            MadtEntry::LocalX2Apic(apic) => apic.flags & LAPIC_USABLE != 0,
            _ => false,
        })
        .count();
    assert!(count > 0, "MADT did not list any usable processors");
    count
}

fn boot_reserved_ranges(handoff: &boot::LimineBootHandoff) -> BootReservedRanges {
    let executable_bytes = align_up(
        usize::try_from(handoff.kernel.size).unwrap_or_else(|_| {
            panic!(
                "Limine executable file size does not fit usize: {}",
                handoff.kernel.size
            )
        }),
        PAGE_BYTES,
    );
    let loaded_executable_start =
        usize::try_from(handoff.kernel.physical_base).unwrap_or_else(|_| {
            panic!(
                "Limine executable physical base does not fit usize: {:#x}",
                handoff.kernel.physical_base
            )
        });
    let loaded_executable_end = loaded_executable_start
        .checked_add(executable_bytes)
        .unwrap_or_else(|| {
            panic!(
                "Limine executable loaded range overflow: start={loaded_executable_start:#x}, len={executable_bytes:#x}"
            )
        });
    let file_start = handoff.kernel.file_address;
    let file_end = file_start.checked_add(executable_bytes).unwrap_or_else(|| {
        panic!("Limine executable file range overflow: start={file_start:#x}, len={executable_bytes:#x}")
    });

    let mut reserved = BootReservedRanges::new();
    reserved.reserve(loaded_executable_start..loaded_executable_end);
    reserved.reserve(file_start..file_end);
    reserved
}

fn reserve_wakeup_page(
    handoff: &boot::LimineBootHandoff,
    reserved_ranges: &BootReservedRanges,
) -> Range<usize> {
    handoff
        .memory_map
        .regions()
        .flat_map(|region| usable_region_segments(region, reserved_ranges))
        .flatten()
        .find_map(|segment| {
            let start = align_up(segment.start, smp::WAKEUP_PAGE_BYTES);
            let end = segment.end.min(smp::SIPI_MAX_PHYSICAL_ADDRESS);
            (start
                .checked_add(smp::WAKEUP_PAGE_BYTES)
                .is_some_and(|next| next <= end))
            .then_some(start..start + smp::WAKEUP_PAGE_BYTES)
        })
        .unwrap_or_else(|| panic!("failed to reserve a low-memory wakeup page for x86 AP startup"))
}

fn boot_memory_regions(
    handoff: &boot::LimineBootHandoff,
    physical_memory_offset: usize,
    reserved_ranges: &BootReservedRanges,
    excluded: Option<Range<usize>>,
) -> impl IntoIterator<Item = MemoryRegion> {
    let mut reserved_ranges = reserved_ranges.clone();
    if let Some(excluded) = excluded {
        reserved_ranges.reserve(excluded);
    }
    handoff.memory_map.regions().flat_map(move |region| {
        usable_region_segments(region, &reserved_ranges)
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

/// The physical memory the kernel allocates its DMA buffers from.
///
/// It is exactly what the bootstrap allocator was primed with: every
/// buffer a driver hands to a device is a kernel heap allocation, so a
/// domain that maps these runs reaches every buffer and nothing else.
fn dma_capable_ranges(
    handoff: &boot::LimineBootHandoff,
    physical_memory_offset: usize,
    reserved_ranges: &BootReservedRanges,
    excluded: Option<Range<usize>>,
) -> alloc::vec::Vec<helios_hal::iommu::PhysicalRange> {
    boot_memory_regions(handoff, physical_memory_offset, reserved_ranges, excluded)
        .into_iter()
        .map(|region| {
            let start = region.as_ptr().cast::<u8>() as usize - physical_memory_offset;
            helios_hal::iommu::PhysicalRange::new(start as u64, region.len() as u64)
        })
        .collect()
}

/// The kernel console: every record is retained for the debugger and
/// mirrored to the debug UART, as it is on aarch64.
///
/// Mirroring is not a debugging aid to switch on when something breaks.
/// Anything the kernel reports before the inspector's RPC transport
/// exists — device discovery, feature negotiation, the reason a device
/// refused to come up — reaches the outside world through this UART or
/// through nowhere at all, and a bring-up failure is exactly the case
/// where the transport never arrives. x86 used to mirror only under the
/// watchdog self-test, which is why a panic in PCI bring-up left a boot
/// log holding the panic line and nothing that explained it.
///
/// The port itself is already configured by the time any console exists:
/// `serial_uart_init` runs once, on the bootstrap processor, and a
/// console built on an application processor writes to the port that
/// processor's siblings are already using.
fn serial_console(
    debug_state: debug_state::RuntimeState,
) -> helios_kernel::RecordingConsole<
    debug_state::RuntimeState,
    impl FnMut() -> u64,
    impl FnMut(&[u8]),
> {
    helios_kernel::RecordingConsole::new(debug_state, read_tsc, Some(write_debug_serial_bytes))
}

#[derive(Clone)]
pub(crate) struct X86Cpu {
    state: Arc<smp::X86PlatformState>,
}

impl X86Cpu {
    fn new(state: Arc<smp::X86PlatformState>) -> Self {
        Self { state }
    }

    pub(crate) fn platform_state(&self) -> &smp::X86PlatformState {
        &self.state
    }

    pub(crate) fn debug_state(&self) -> debug_state::RuntimeState {
        self.state.debug_state()
    }

    /// Local-APIC id of the bootstrap processor: the destination every
    /// device MSI-X message is addressed to.
    /// The local APIC an MSI-X message for `processor` must target.
    ///
    /// Falls back to the bootstrap processor's APIC for a slot ACPI did
    /// not describe, so a vector is never programmed with a destination
    /// no processor answers.
    pub(crate) fn apic_id_of_processor(&self, processor: helios_hal::cpu::ProcessorId) -> u32 {
        self.state
            .apic_id_of(processor)
            .unwrap_or_else(|| self.bootstrap_apic_id())
    }

    pub(crate) fn bootstrap_apic_id(&self) -> u32 {
        let bootstrap = self.state.bootstrap_processor();
        self.state
            .apic_id_of(bootstrap)
            .unwrap_or_else(|| panic!("x86 bootstrap processor {bootstrap:?} has no local-APIC id"))
    }

    pub(crate) fn watchdog(&self) -> crate::watchdog::X86Watchdog {
        self.state.watchdog()
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
        // HLT halts the CPU until any unmasked interrupt fires. The
        // local APIC timer always ticks at our scheduler frequency,
        // and a remote core can drag this one out of HLT immediately
        // by sending the wake IPI through `wake_processor`. Inside a
        // critical section IRQs are masked and HLT would deadlock,
        // but `park_current` is only called from the kernel run loop
        // which never holds a critical section across the call.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }

    fn start_processor(&self, processor: ProcessorId) {
        self.state
            .start_processor(processor, secondary_start_rust as *const () as usize);
    }

    fn wake_processor(&self, processor: ProcessorId) {
        if let Some(apic_id) = self.state.apic_id_of(processor) {
            smp::send_wake_ipi(apic_id);
        }
    }

    fn now(&self) -> Instant {
        Instant::new(read_tsc().saturating_sub(self.state.tsc_base()))
    }

    fn timer_frequency(&self) -> u64 {
        self.state.tsc_hz()
    }

    fn set_deadline(&self, deadline: Instant) {
        let _ = deadline;
        smp::ensure_local_scheduler_timer(exceptions::TIMER_INTERRUPT_VECTOR);
    }

    fn publish_executable(&self, ptr: *const u8, len: usize) {
        helios_kernel::runtime_memory::publish_code_memory(ptr, len);
    }

    fn unpublish_executable(&self, ptr: *const u8, len: usize) {
        helios_kernel::runtime_memory::unpublish_code_memory(ptr, len);
    }

    fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
        Some(detect_x86_native_feature)
    }

    fn has_lazy_commit_virtual_memory(&self) -> bool {
        // `X86UserAddressSpace` reserves four-level-paging ranges without
        // touching a frame and commits them page by page on request, so the
        // runtime can pre-reserve a 4 GiB slot per linear memory out of the
        // 32 TiB user window and pay physical memory only for what a guest
        // actually touches.
        true
    }

    fn fill_entropy(&self, buffer: &mut [u8]) -> Result<EntropyQuality, EntropyUnavailable> {
        if !x86_has_rdrand() {
            return Err(EntropyUnavailable);
        }
        fill_with_rdrand(buffer);
        Ok(EntropyQuality::Cryptographic)
    }

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

fn x86_has_rdrand() -> bool {
    __cpuid(1).ecx & (1 << 30) != 0
}

fn fill_with_rdrand(buffer: &mut [u8]) {
    let mut chunks = buffer.chunks_exact_mut(core::mem::size_of::<u64>());
    for chunk in chunks.by_ref() {
        chunk.copy_from_slice(&rdrand64().to_le_bytes());
    }

    let remainder = chunks.into_remainder();
    if !remainder.is_empty() {
        let word = rdrand64().to_le_bytes();
        remainder.copy_from_slice(&word[..remainder.len()]);
    }
}

#[target_feature(enable = "rdrand")]
unsafe fn rdrand64_enabled() -> Option<u64> {
    let mut value = 0_u64;
    for _ in 0..10 {
        if _rdrand64_step(&mut value) == 1 {
            return Some(value);
        }
    }
    None
}

fn rdrand64() -> u64 {
    unsafe { rdrand64_enabled() }.expect("x86 RDRAND failed after retries")
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
    // FPU/SSE control bits are set by the secondary wakeup trampoline
    // before any compiler-generated code runs on this processor.
    //
    // COM1 is deliberately not configured here: the bootstrap processor
    // configured it before it woke anyone, and configuring it again is
    // destructive rather than idempotent. See `serial_uart_init`.
    let boot = unsafe { &*boot };
    let runtime = unsafe { &*runtime };
    smp::activate_runtime(runtime);
    exceptions::install_for_current_processor();

    let debug_state = boot.platform().debug_state();
    let console = serial_console(debug_state.clone());
    let cpu = X86Cpu::new(boot.platform());
    let kernel = helios_kernel::init_with_watchdog(Platform::with_watchdog(
        console,
        core::iter::empty::<MemoryRegion>(),
        cpu.clone(),
        cpu.watchdog(),
    ));
    smp::current_runtime().install_timer(kernel.timer());
    x86_64::instructions::interrupts::enable();
    let program_service = debug_state
        .program_service()
        .unwrap_or_else(|| panic!("x86 secondary started before program service installation"));
    smp::current_runtime().install_program_service(program_service);
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

    let max_basic = __cpuid(0).eax;
    let max_extended = __cpuid(0x8000_0000).eax;
    let leaf1 = __cpuid(1);
    let leaf7 = (max_basic >= 7).then(|| __cpuid_count(7, 0));
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
        "lzcnt" => Some(max_extended >= 0x8000_0001 && __cpuid(0x8000_0001).ecx & (1 << 5) != 0),
        _ => None,
    }
}

fn read_tsc() -> u64 {
    unsafe { _rdtsc() }
}

fn detect_tsc_frequency_hz() -> u64 {
    let max_basic = __cpuid(0).eax;
    if max_basic >= 0x15 {
        let leaf_15 = __cpuid(0x15);
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
        let base_mhz = u64::from(__cpuid(0x16).eax);
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

/// Configures COM1, exactly once, on the bootstrap processor before it
/// wakes any application processor.
///
/// This is a reset, not an idempotent setup. Writing `0xc7` to the FIFO
/// control register clears both FIFOs, throwing away every transmit byte
/// the device has accepted but not yet handed to the host; and raising
/// DLAB in the line control register turns the data port into the baud
/// divisor latch, so a byte another processor writes during that window
/// lands in the divisor instead of on the wire.
///
/// Every processor used to run this from `secondary_start_rust` and again
/// from `serial_console`, which was harmless only while x86 mirrored the
/// kernel log to this UART under the watchdog self-test alone. Once the
/// console mirrored unconditionally the bootstrap processor was streaming
/// the boot log through the FIFO while the application processors came
/// up, and each of them silently ate part of it: issue #98 is a stage
/// marker that reached the inspector as `"[KDBG engine:o"` because the
/// `k]\n` behind it was cleared out of the FIFO.
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
            serial_write_byte(byte);
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

/// The port a panicking processor writes its report to.
///
/// COM1, the same port everything else on this machine writes to, and
/// reached the same way: `x86_kernel_main` configures it before
/// anything can panic, and reconfiguring it here would clear the
/// transmit FIFO — discarding the tail of the very log that explains
/// the panic.
struct PanicConsolePort;

impl helios_kernel::PanicSerial for PanicConsolePort {
    fn write_bytes(bytes: &[u8]) {
        DebugSerial.write_bytes(bytes);
    }
}

fn serial_tx_ready() -> bool {
    unsafe {
        let mut status: PortReadOnly<u8> = PortReadOnly::new(COM1_LINE_STATUS);
        status.read() & LSR_TX_EMPTY != 0
    }
}

impl DebugSerialAccess for DebugSerial {
    type Port = Self;

    fn port() -> Self {
        // COM1 is configured once, by `serial_uart_init` on the
        // bootstrap processor, before anything can write to it.
        Self
    }
}

fn read_debug_serial(buffer: &mut alloc::vec::Vec<u8>, max_bytes: u32) {
    helios_kernel::read_debug_serial::<DebugSerial>(buffer, max_bytes);
}

pub(crate) fn write_debug_serial_bytes(bytes: &[u8]) {
    helios_kernel::write_debug_serial_bytes::<DebugSerial>(bytes);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    helios_kernel::emit_panic_report::<PanicConsolePort>(info);
    helios_kernel::panic_log(info);
    loop {
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_get(slot: usize) -> *mut u8 {
    smp::current_runtime().wasmtime_tls.get(slot)
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(slot: usize, ptr: *mut u8) {
    smp::current_runtime().wasmtime_tls.set(slot, ptr);
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_init_traps(handler: helios_kernel::KernelNativeTrapHandler) -> i32 {
    WASMTIME_NATIVE_TRAP_HANDLER.store(handler as usize, Ordering::Release);
    smp::current_runtime()
        .native_trap_handler
        .store(handler as usize, Ordering::Release);
    0
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_parking_wait(_timeout_nanos: u64) {
    core::hint::spin_loop();
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_parking_unpark() {}
