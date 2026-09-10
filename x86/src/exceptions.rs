use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::ops::Range;
use core::sync::atomic::Ordering;

use helios_hal::vmm::VirtAddr as UserVirtAddr;
use helios_kernel::{KernelException, KernelExceptionCause, KernelExceptionDispatch, StackFault};
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::registers::control::Cr2;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable};
use x86_64::structures::idt::InterruptDescriptorTable;
use x86_64::structures::tss::TaskStateSegment;

use crate::smp;

const PAGE_FAULT_INSTRUCTION_FETCH: u64 = 1 << 4;
const DOUBLE_FAULT_VECTOR: u64 = 8;
const PAGE_FAULT_VECTOR: u64 = 14;
/// Interrupt-stack-table slots, as `set_stack_index` counts them.
const PAGE_FAULT_IST_INDEX: u16 = 0;
const DOUBLE_FAULT_IST_INDEX: u16 = 1;
/// One exception stack. The runtime's trap handler and a panic's
/// formatting both run on it; AArch64 sizes its exception stack the same.
pub(crate) const EXCEPTION_STACK_BYTES: usize = 64 * 1024;
/// The value `ProcessorRuntime::probe_fault` takes once the boot-time
/// page-fault probe has been resolved, distinct from every page address.
const PROBE_RESOLVED: usize = usize::MAX;
pub(crate) const TIMER_INTERRUPT_VECTOR: u8 = 0x20;
pub(crate) const WAKE_INTERRUPT_VECTOR: u8 = 0x21;
pub(crate) const TLB_SHOOTDOWN_INTERRUPT_VECTOR: u8 = 0x22;
/// MSI-X vectors for the PCI devices the kernel drives. One vector per
/// device keeps dispatch a direct lookup in [`DeviceInterruptRoutes`]
/// without a shared interrupt-status scan.
/// The network device's configuration-change message. Its queue pairs
/// have vectors of their own, one per processor that drains one.
pub(crate) const NETWORK_INTERRUPT_VECTOR: u8 = 0x30;
/// Queue pairs the backend hands a vector of their own, each delivered
/// to the local APIC of the processor whose shard drains that pair.
/// A machine with more processors than this shares the last vector,
/// which costs a cross-core hand-off for the tail pairs but never drops
/// their completions.
pub(crate) const MAX_NETWORK_QUEUE_VECTORS: usize = 8;
/// One IDT vector per steered queue pair, contiguous so the dispatch is
/// a subtraction rather than a table search.
pub(crate) const NETWORK_QUEUE_INTERRUPT_VECTORS: [u8; MAX_NETWORK_QUEUE_VECTORS] =
    [0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47];
pub(crate) const HOST_FS_INTERRUPT_VECTOR: u8 = 0x31;
pub(crate) const ENTROPY_INTERRUPT_VECTOR: u8 = 0x32;
pub(crate) const VSOCK_INTERRUPT_VECTOR: u8 = 0x37;
pub(crate) const DISPLAY_INTERRUPT_VECTOR: u8 = 0x38;
/// One vector per block device the routing table can hold: the platform
/// exposes the boot image and the kernel's own disk as separate
/// functions, and each of them delivers its completions on its own
/// message.
pub(crate) const BLOCK_INTERRUPT_VECTORS: [u8; helios_kernel::MAX_BLOCK_DEVICES] =
    [0x33, 0x34, 0x35, 0x36];

/// Device interrupt routing table for this backend, keyed by IDT vector.
///
/// The memory balloon has no slot to fill: its PCI function carries no
/// MSI-X capability, and `balloon` reads its interrupt status on the
/// kernel timer instead.
pub(crate) type DeviceInterruptRoutes = helios_kernel::ExternalInterruptRoutes<
    u8,
    crate::net::VirtioNetworkDevice,
    crate::host_fs::HostFsTransportService,
    crate::entropy::VirtioEntropyDevice,
    core::convert::Infallible,
    crate::vsock::VirtioVsockFunction,
    crate::gpu::VirtioDisplayDevice,
    crate::block::VirtioBlockDevice,
>;

global_asm!(include_str!("exceptions.S"));

unsafe extern "C" {
    fn helios_x86_exception_divide_error();
    fn helios_x86_exception_breakpoint();
    fn helios_x86_exception_invalid_opcode();
    fn helios_x86_exception_double_fault();
    fn helios_x86_exception_general_protection();
    fn helios_x86_exception_page_fault();
    fn helios_x86_exception_x87_floating_point();
    fn helios_x86_exception_simd_floating_point();
    fn helios_x86_interrupt_timer();
    fn helios_x86_interrupt_wake();
    fn helios_x86_interrupt_tlb_shootdown();
    fn helios_x86_interrupt_network();
    fn helios_x86_interrupt_network_queue_0();
    fn helios_x86_interrupt_network_queue_1();
    fn helios_x86_interrupt_network_queue_2();
    fn helios_x86_interrupt_network_queue_3();
    fn helios_x86_interrupt_network_queue_4();
    fn helios_x86_interrupt_network_queue_5();
    fn helios_x86_interrupt_network_queue_6();
    fn helios_x86_interrupt_network_queue_7();
    fn helios_x86_interrupt_host_fs();
    fn helios_x86_interrupt_entropy();
    fn helios_x86_interrupt_vsock();
    fn helios_x86_interrupt_display();
    fn helios_x86_interrupt_block_0();
    fn helios_x86_interrupt_block_1();
    fn helios_x86_interrupt_block_2();
    fn helios_x86_interrupt_block_3();
}

pub(crate) struct ProcessorIdt {
    table: UnsafeCell<InterruptDescriptorTable>,
}

unsafe impl Sync for ProcessorIdt {}

impl ProcessorIdt {
    pub(crate) const fn new() -> Self {
        Self {
            table: UnsafeCell::new(InterruptDescriptorTable::new()),
        }
    }

    pub(crate) fn install(&self) {
        let table = unsafe { &mut *self.table.get() };
        *table = InterruptDescriptorTable::new();
        unsafe {
            table
                .divide_error
                .set_handler_addr(handler_address(helios_x86_exception_divide_error));
            table
                .breakpoint
                .set_handler_addr(handler_address(helios_x86_exception_breakpoint));
            table
                .invalid_opcode
                .set_handler_addr(handler_address(helios_x86_exception_invalid_opcode));
            table
                .general_protection_fault
                .set_handler_addr(handler_address(helios_x86_exception_general_protection));
            // A page fault raised by a push below `rsp` cannot be taken
            // on the interrupted stack: the processor's own push of the
            // exception frame lands in the same unmapped page and faults
            // during delivery, which is a double fault, and a double
            // fault without a stack of its own is a triple fault and a
            // reset. Both take a stack the TSS names.
            table
                .page_fault
                .set_handler_addr(handler_address(helios_x86_exception_page_fault))
                .set_stack_index(PAGE_FAULT_IST_INDEX);
            table
                .double_fault
                .set_handler_addr(handler_address(helios_x86_exception_double_fault))
                .set_stack_index(DOUBLE_FAULT_IST_INDEX);
            table
                .x87_floating_point
                .set_handler_addr(handler_address(helios_x86_exception_x87_floating_point));
            table
                .simd_floating_point
                .set_handler_addr(handler_address(helios_x86_exception_simd_floating_point));
            table[TIMER_INTERRUPT_VECTOR]
                .set_handler_addr(handler_address(helios_x86_interrupt_timer));
            table[WAKE_INTERRUPT_VECTOR]
                .set_handler_addr(handler_address(helios_x86_interrupt_wake));
            table[TLB_SHOOTDOWN_INTERRUPT_VECTOR]
                .set_handler_addr(handler_address(helios_x86_interrupt_tlb_shootdown));
            table[NETWORK_INTERRUPT_VECTOR]
                .set_handler_addr(handler_address(helios_x86_interrupt_network));
            let network_queue_stubs: [unsafe extern "C" fn(); MAX_NETWORK_QUEUE_VECTORS] = [
                helios_x86_interrupt_network_queue_0,
                helios_x86_interrupt_network_queue_1,
                helios_x86_interrupt_network_queue_2,
                helios_x86_interrupt_network_queue_3,
                helios_x86_interrupt_network_queue_4,
                helios_x86_interrupt_network_queue_5,
                helios_x86_interrupt_network_queue_6,
                helios_x86_interrupt_network_queue_7,
            ];
            for (vector, stub) in NETWORK_QUEUE_INTERRUPT_VECTORS
                .iter()
                .zip(network_queue_stubs)
            {
                table[*vector].set_handler_addr(handler_address(stub));
            }
            table[HOST_FS_INTERRUPT_VECTOR]
                .set_handler_addr(handler_address(helios_x86_interrupt_host_fs));
            table[ENTROPY_INTERRUPT_VECTOR]
                .set_handler_addr(handler_address(helios_x86_interrupt_entropy));
            table[VSOCK_INTERRUPT_VECTOR]
                .set_handler_addr(handler_address(helios_x86_interrupt_vsock));
            table[DISPLAY_INTERRUPT_VECTOR]
                .set_handler_addr(handler_address(helios_x86_interrupt_display));
            let block_stubs: [unsafe extern "C" fn(); helios_kernel::MAX_BLOCK_DEVICES] = [
                helios_x86_interrupt_block_0,
                helios_x86_interrupt_block_1,
                helios_x86_interrupt_block_2,
                helios_x86_interrupt_block_3,
            ];
            for (vector, stub) in BLOCK_INTERRUPT_VECTORS.iter().zip(block_stubs) {
                table[*vector].set_handler_addr(handler_address(stub));
            }
            table.load_unsafe();
        }
    }
}

/// The segment state a processor loads beside its IDT: a GDT of its own
/// carrying a TSS whose interrupt stack table names the two exception
/// stacks. Limine hands the kernel a GDT with no TSS, so until this is
/// loaded no IDT entry can ask for a stack switch.
///
/// Owned by one [`smp::ProcessorRuntime`] and touched only by the
/// processor it belongs to, during `install_for_current_processor`.
pub(crate) struct ProcessorSegments {
    gdt: UnsafeCell<GlobalDescriptorTable>,
    tss: UnsafeCell<TaskStateSegment>,
    page_fault_stack: Range<usize>,
    double_fault_stack: Range<usize>,
}

// SAFETY: the tables are written and loaded by the owning processor only,
// before that processor enables interrupts; the stack ranges are immutable.
unsafe impl Sync for ProcessorSegments {}

impl ProcessorSegments {
    /// `page_fault_stack` and `double_fault_stack` are the byte ranges of
    /// two stacks allocated for this processor alone; the TSS names their
    /// upper ends.
    pub(crate) const fn new(
        page_fault_stack: Range<usize>,
        double_fault_stack: Range<usize>,
    ) -> Self {
        Self {
            gdt: UnsafeCell::new(GlobalDescriptorTable::new()),
            tss: UnsafeCell::new(TaskStateSegment::new()),
            page_fault_stack,
            double_fault_stack,
        }
    }

    /// The stack every page fault on this processor is taken on.
    pub(crate) fn page_fault_stack(&self) -> Range<usize> {
        self.page_fault_stack.clone()
    }

    /// Builds the TSS and GDT and makes them the processor's own.
    ///
    /// `FS` and `GS` are deliberately left alone: `IA32_FS_BASE` carries
    /// the processor anchor, and loading a selector into `FS` would reset
    /// that base to the descriptor's.
    fn install(&self) {
        assert!(
            self.page_fault_stack.end.is_multiple_of(16)
                && self.double_fault_stack.end.is_multiple_of(16),
            "x86 exception stack tops must be 16-byte aligned"
        );
        // SAFETY: this runs once per processor, on that processor, with
        // interrupts disabled, and nothing else reaches these cells.
        let tss = unsafe { &mut *self.tss.get() };
        let gdt = unsafe { &mut *self.gdt.get() };
        *tss = TaskStateSegment::new();
        tss.interrupt_stack_table[usize::from(PAGE_FAULT_IST_INDEX)] =
            VirtAddr::new(self.page_fault_stack.end as u64);
        tss.interrupt_stack_table[usize::from(DOUBLE_FAULT_IST_INDEX)] =
            VirtAddr::new(self.double_fault_stack.end as u64);
        *gdt = GlobalDescriptorTable::new();
        let code = gdt.append(Descriptor::kernel_code_segment());
        let data = gdt.append(Descriptor::kernel_data_segment());
        // SAFETY: the TSS lives in a `ProcessorRuntime` that
        // `publish_anchor_identities` pinned at its final address before
        // any processor was activated, so the descriptor's base stays
        // valid for as long as the processor runs.
        let tss_selector = gdt.append(unsafe { Descriptor::tss_segment_unchecked(tss) });
        // SAFETY: the table outlives the processor for the reason above;
        // the selectors loaded are the ones this table just produced.
        unsafe {
            gdt.load_unsafe();
            CS::set_reg(code);
            SS::set_reg(data);
            DS::set_reg(data);
            ES::set_reg(data);
            load_tss(tss_selector);
        }
    }
}

pub(crate) fn install_for_current_processor() {
    let runtime = smp::current_runtime();
    runtime.segments.install();
    runtime.exception_idt.install();
}

/// Proves, on the calling processor, that a page fault is taken on the
/// exception stack and that a fault the kernel resolves in place returns
/// to the faulting instruction.
///
/// The probe reserves one page of user address space without committing
/// it, announces the address in `probe_fault`, and reads the page. The
/// read faults; the dispatcher recognises the announced address, commits
/// the page, marks the probe resolved and returns; the read then
/// completes and sees the fresh frame's zero. Every step that could
/// silently fail is asserted: a probe that did not fault, a fault that
/// was not resolved, or a read that saw anything but zero is a boot
/// failure with a message, because a kernel whose fault path cannot
/// return would otherwise discover it at the first stack overflow.
pub(crate) fn verify_page_fault_returns() {
    let runtime = smp::current_runtime();
    let page = crate::vmm::reserve_probe_page();
    let start = page.start.raw();
    let previous = runtime.probe_fault.swap(start, Ordering::AcqRel);
    assert!(
        previous == 0,
        "x86 page-fault probe re-entered with {previous:#x} outstanding"
    );
    // SAFETY: `page` is a reserved user page this processor owns for the
    // duration of the probe; reading it is the fault under test, and the
    // dispatcher commits it before the read completes.
    let value = unsafe { core::ptr::read_volatile(start as *const u64) };
    let outcome = runtime.probe_fault.swap(0, Ordering::AcqRel);
    assert!(
        outcome == PROBE_RESOLVED,
        "x86 page-fault probe at {start:#x} did not fault: the read completed with \
         the reservation uncommitted (probe word {outcome:#x})"
    );
    assert!(
        value == 0,
        "x86 page-fault probe at {start:#x} read {value:#x} from a page committed \
         from the exception stack; the frame was not zeroed"
    );
    crate::vmm::release_probe_page(page);
    tracing::info!(
        target: "helios_x86::exceptions",
        processor = runtime.logical_id(),
        page = start,
        "page fault taken on the exception stack and resolved in place"
    );
}

/// Resolves the boot-time probe's fault, if `faulting_address` is the
/// page it announced.
fn resolve_probe_fault(faulting_address: usize) -> bool {
    let runtime = smp::current_runtime();
    let expected = runtime.probe_fault.load(Ordering::Acquire);
    if expected == 0 || expected == PROBE_RESOLVED || faulting_address & !0xfff != expected {
        return false;
    }
    crate::vmm::commit_probe_page(expected);
    runtime.probe_fault.store(PROBE_RESOLVED, Ordering::Release);
    true
}

/// A page fault whose frame is not on this processor's exception stack
/// means the TSS is not in effect, and the next fault below `rsp` will
/// reset the machine instead of being reported. Caught here, on the
/// first fault of any kind, rather than there.
fn assert_frame_on_exception_stack(frame: &ExceptionFrame) {
    let stack = smp::current_runtime().segments.page_fault_stack();
    let address = core::ptr::from_ref(frame) as usize;
    assert!(
        stack.contains(&address),
        "x86 page fault frame at {address:#x} is not on this processor's exception stack \
         {:#x}..{:#x}: the IST is not in effect",
        stack.start,
        stack.end
    );
}

fn handler_address(handler: unsafe extern "C" fn()) -> VirtAddr {
    VirtAddr::new(handler as usize as u64)
}

#[repr(C)]
pub(crate) struct ExceptionFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rdi: u64,
    rsi: u64,
    rbp: u64,
    rbx: u64,
    rdx: u64,
    rcx: u64,
    rax: u64,
    vector: u64,
    error_code: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
}

/// The exception entry's dispatcher. Returning means the fault was
/// resolved in place and the stub restores the interrupted context;
/// everything unresolved diverges here, either into the runtime's trap
/// handler (which unwinds the guest and never comes back) or into a
/// panic.
#[unsafe(no_mangle)]
extern "C" fn helios_x86_exception_dispatch(frame: &mut ExceptionFrame) {
    let mut stack_fault = StackFault::Elsewhere;
    if frame.vector == PAGE_FAULT_VECTOR {
        assert_frame_on_exception_stack(frame);
        let faulting_address = Cr2::read_raw() as usize;
        if resolve_probe_fault(faulting_address) {
            return;
        }
        // A reserved page inside a live fiber stack is a demand commit
        // the kernel resolves here, with no lock and no allocation; the
        // guard page below one is a stack overflow and stays a fault.
        stack_fault = helios_kernel::resolve_stack_fault(UserVirtAddr::new(faulting_address));
        if stack_fault == StackFault::Committed {
            return;
        }
    }
    if frame.vector == DOUBLE_FAULT_VECTOR {
        panic!(
            "x86 double fault rip={:#x} rsp-at-fault-frame={:#x}: an exception could not be \
             delivered on the interrupted stack",
            frame.rip,
            core::ptr::from_ref(frame) as usize
        );
    }
    if let Some(exception) = exception_from_frame(frame) {
        match helios_kernel::dispatch_native_trap(exception) {
            KernelExceptionDispatch::Resolved => return,
            KernelExceptionDispatch::Unhandled => {
                panic!(
                    "unhandled x86 kernel exception after Wasmtime dispatch: \
                     {exception:?}{stack_fault}"
                )
            }
        }
    }
    panic!(
        "unhandled x86 kernel exception vector={} rip={:#x} error_code={:#x}",
        frame.vector, frame.rip, frame.error_code
    );
}

#[unsafe(no_mangle)]
extern "C" fn helios_x86_interrupt_dispatch(frame: &mut ExceptionFrame) {
    match u8::try_from(frame.vector) {
        Ok(TIMER_INTERRUPT_VECTOR) => {
            smp::handle_local_timer_interrupt();
        }
        Ok(WAKE_INTERRUPT_VECTOR) => {
            // The wake IPI exists solely to drag a HLT-ed processor
            // back into the kernel run loop; receiving it is enough,
            // no work to do beyond ack.
            smp::handle_wake_interrupt();
        }
        Ok(TLB_SHOOTDOWN_INTERRUPT_VECTOR) => {
            smp::handle_tlb_shootdown_interrupt();
        }
        Ok(vector) if is_device_interrupt(vector) => {
            smp::handle_device_interrupt(vector);
        }
        _ => panic!(
            "unhandled x86 interrupt vector={:#x} rip={:#x}; device vectors are \
             network={NETWORK_INTERRUPT_VECTOR:#x} host-fs={HOST_FS_INTERRUPT_VECTOR:#x} \
             entropy={ENTROPY_INTERRUPT_VECTOR:#x} vsock={VSOCK_INTERRUPT_VECTOR:#x} \
             display={DISPLAY_INTERRUPT_VECTOR:#x} block={BLOCK_INTERRUPT_VECTORS:#x?}",
            frame.vector, frame.rip
        ),
    }
}

/// Whether `vector` belongs to a device route.
///
/// The IDT stub for a device vector pushes nothing but the vector
/// number, so this predicate is what decides between the routing table
/// and a fatal spurious interrupt. Every vector [`ProcessorIdt::install`]
/// points at a device stub has to be listed here, which is why the block
/// devices are tested against the same array the IDT is built from
/// rather than against a second copy of those numbers.
fn is_device_interrupt(vector: u8) -> bool {
    matches!(
        vector,
        NETWORK_INTERRUPT_VECTOR
            | HOST_FS_INTERRUPT_VECTOR
            | ENTROPY_INTERRUPT_VECTOR
            | VSOCK_INTERRUPT_VECTOR
            | DISPLAY_INTERRUPT_VECTOR
    ) || BLOCK_INTERRUPT_VECTORS.contains(&vector)
        || NETWORK_QUEUE_INTERRUPT_VECTORS.contains(&vector)
}

fn exception_from_frame(frame: &ExceptionFrame) -> Option<KernelException> {
    let cause = match frame.vector {
        0 | 16 | 19 => KernelExceptionCause::Arithmetic,
        3 => KernelExceptionCause::Breakpoint,
        6 => KernelExceptionCause::IllegalInstruction,
        13 => KernelExceptionCause::DataFault,
        14 if frame.error_code & PAGE_FAULT_INSTRUCTION_FETCH != 0 => {
            KernelExceptionCause::InstructionFault
        }
        14 => KernelExceptionCause::DataFault,
        _ => return None,
    };
    Some(KernelException {
        cause,
        instruction_pointer: frame.rip as usize,
        frame_pointer: frame.rbp as usize,
        faulting_address: (frame.vector == 14).then(|| Cr2::read_raw() as usize),
    })
}
