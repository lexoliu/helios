use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::mem;
use core::sync::atomic::Ordering;

use helios_kernel::{
    KernelException, KernelExceptionCause, KernelExceptionDispatch, KernelNativeTrapHandler,
};
use x86_64::VirtAddr;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::InterruptDescriptorTable;

use crate::smp;

const PAGE_FAULT_INSTRUCTION_FETCH: u64 = 1 << 4;
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
/// The memory balloon's vector sits after the block devices' rather
/// than filling the first free slot: the vsock transport claims `0x37`,
/// and two devices sharing a vector would each be told about the
/// other's completions.
pub(crate) const BALLOON_INTERRUPT_VECTOR: u8 = 0x38;
/// One vector per block device the routing table can hold: the platform
/// exposes the boot image and the kernel's own disk as separate
/// functions, and each of them delivers its completions on its own
/// message.
pub(crate) const BLOCK_INTERRUPT_VECTORS: [u8; helios_kernel::MAX_BLOCK_DEVICES] =
    [0x33, 0x34, 0x35, 0x36];

/// Device interrupt routing table for this backend, keyed by IDT vector.
pub(crate) type DeviceInterruptRoutes = helios_kernel::ExternalInterruptRoutes<
    u8,
    crate::net::VirtioNetworkDevice,
    crate::host_fs::HostFsTransportService,
    crate::entropy::VirtioEntropyDevice,
    crate::balloon::VirtioBalloonInterrupt,
    crate::vsock::VirtioVsockFunction,
    crate::block::VirtioBlockDevice,
>;

global_asm!(include_str!("exceptions.S"));

unsafe extern "C" {
    fn helios_x86_exception_divide_error();
    fn helios_x86_exception_breakpoint();
    fn helios_x86_exception_invalid_opcode();
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
    fn helios_x86_interrupt_balloon();
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
            table
                .page_fault
                .set_handler_addr(handler_address(helios_x86_exception_page_fault));
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
            table[BALLOON_INTERRUPT_VECTOR]
                .set_handler_addr(handler_address(helios_x86_interrupt_balloon));
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

pub(crate) fn install_for_current_processor() {
    smp::current_runtime().exception_idt.install();
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

#[unsafe(no_mangle)]
extern "C" fn helios_x86_exception_dispatch(frame: &mut ExceptionFrame) -> ! {
    if let Some(exception) = exception_from_frame(frame) {
        if dispatch_to_wasmtime(exception) == KernelExceptionDispatch::Unhandled {
            panic!("unhandled x86 kernel exception after Wasmtime dispatch: {exception:?}");
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
             entropy={ENTROPY_INTERRUPT_VECTOR:#x} balloon={BALLOON_INTERRUPT_VECTOR:#x} \
             block={BLOCK_INTERRUPT_VECTORS:#x?}",
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
            | BALLOON_INTERRUPT_VECTOR
    ) || BLOCK_INTERRUPT_VECTORS.contains(&vector)
        || NETWORK_QUEUE_INTERRUPT_VECTORS.contains(&vector)
}

fn dispatch_to_wasmtime(exception: KernelException) -> KernelExceptionDispatch {
    let per_processor_handler = smp::current_runtime()
        .native_trap_handler
        .load(Ordering::Acquire);
    let raw_handler = if per_processor_handler != 0 {
        per_processor_handler
    } else {
        crate::WASMTIME_NATIVE_TRAP_HANDLER.load(Ordering::Acquire)
    };
    if raw_handler == 0 {
        return KernelExceptionDispatch::Unhandled;
    }
    let handler: KernelNativeTrapHandler = unsafe { mem::transmute(raw_handler) };
    exception.dispatch_to(handler)
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
