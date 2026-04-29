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
        _ => panic!(
            "unhandled x86 interrupt vector={} rip={:#x}",
            frame.vector, frame.rip
        ),
    }
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
