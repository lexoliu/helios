.section .text._start, "ax"
.global _start
.type _start, %function
_start:
    msr daifset, #0xf
    msr spsel, #1
    mrs x0, cpacr_el1
    orr x0, x0, #(0x3 << 20)
    msr cpacr_el1, x0
    isb
    adrp x0, __aarch64_boot_stack_top
    add x0, x0, :lo12:__aarch64_boot_stack_top
    mov sp, x0
    bl aarch64_kernel_main
1:
    wfi
    b 1b
.size _start, . - _start

.section .text.exceptions, "ax"
.align 11
.global __helios_aarch64_exception_vectors
__helios_aarch64_exception_vectors:
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_irq
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_irq
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_irq
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_irq
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception

// Synchronous exception entry. Like the IRQ entry this runs on the
// per-processor stack in SP_EL0, leaving the interrupted SP_EL1 intact,
// and it saves the complete interrupted context: x0-x30, ELR, SPSR, the
// interrupted SP, and the whole FP/SIMD file. Wasm traps still end in a
// panic inside the handler, but a fault on a swapped-out page is
// redirected: the handler rewrites the frame to enter the kernel's swap
// trampoline on the faulting stack, and the epilogue below erets into
// it. Nothing here can assume it never returns.
__helios_aarch64_sync_exception:
    msr spsel, #0
    sub sp, sp, #{sync_frame_bytes}
    stp x0, x1, [sp, #0x00]
    stp x2, x3, [sp, #0x10]
    stp x4, x5, [sp, #0x20]
    stp x6, x7, [sp, #0x30]
    stp x8, x9, [sp, #0x40]
    stp x10, x11, [sp, #0x50]
    stp x12, x13, [sp, #0x60]
    stp x14, x15, [sp, #0x70]
    stp x16, x17, [sp, #0x80]
    stp x18, x19, [sp, #0x90]
    stp x20, x21, [sp, #0xa0]
    stp x22, x23, [sp, #0xb0]
    stp x24, x25, [sp, #0xc0]
    stp x26, x27, [sp, #0xd0]
    stp x28, x29, [sp, #0xe0]
    mrs x0, elr_el1
    mrs x1, spsr_el1
    stp x30, x0, [sp, #0xf0]
    str x1, [sp, #0x100]
    msr spsel, #1
    mov x0, sp
    msr spsel, #0
    str x0, [sp, #0x108]
    stp q0, q1, [sp, #0x110]
    stp q2, q3, [sp, #0x130]
    stp q4, q5, [sp, #0x150]
    stp q6, q7, [sp, #0x170]
    stp q8, q9, [sp, #0x190]
    stp q10, q11, [sp, #0x1b0]
    stp q12, q13, [sp, #0x1d0]
    stp q14, q15, [sp, #0x1f0]
    stp q16, q17, [sp, #0x210]
    stp q18, q19, [sp, #0x230]
    stp q20, q21, [sp, #0x250]
    stp q22, q23, [sp, #0x270]
    stp q24, q25, [sp, #0x290]
    stp q26, q27, [sp, #0x2b0]
    stp q28, q29, [sp, #0x2d0]
    stp q30, q31, [sp, #0x2f0]
    mrs x0, fpsr
    mrs x1, fpcr
    stp x0, x1, [sp, #0x310]
    mrs x0, esr_el1
    mrs x1, far_el1
    mov x2, sp
    bl aarch64_handle_sync_exception
    ldp x0, x1, [sp, #0x310]
    msr fpsr, x0
    msr fpcr, x1
    ldp q0, q1, [sp, #0x110]
    ldp q2, q3, [sp, #0x130]
    ldp q4, q5, [sp, #0x150]
    ldp q6, q7, [sp, #0x170]
    ldp q8, q9, [sp, #0x190]
    ldp q10, q11, [sp, #0x1b0]
    ldp q12, q13, [sp, #0x1d0]
    ldp q14, q15, [sp, #0x1f0]
    ldp q16, q17, [sp, #0x210]
    ldp q18, q19, [sp, #0x230]
    ldp q20, q21, [sp, #0x250]
    ldp q22, q23, [sp, #0x270]
    ldp q24, q25, [sp, #0x290]
    ldp q26, q27, [sp, #0x2b0]
    ldp q28, q29, [sp, #0x2d0]
    ldp q30, q31, [sp, #0x2f0]
    ldr x0, [sp, #0x108]
    msr spsel, #1
    mov sp, x0
    msr spsel, #0
    ldr x1, [sp, #0x100]
    ldp x30, x0, [sp, #0xf0]
    msr elr_el1, x0
    msr spsr_el1, x1
    ldp x28, x29, [sp, #0xe0]
    ldp x26, x27, [sp, #0xd0]
    ldp x24, x25, [sp, #0xc0]
    ldp x22, x23, [sp, #0xb0]
    ldp x20, x21, [sp, #0xa0]
    ldp x18, x19, [sp, #0x90]
    ldp x16, x17, [sp, #0x80]
    ldp x14, x15, [sp, #0x70]
    ldp x12, x13, [sp, #0x60]
    ldp x10, x11, [sp, #0x50]
    ldp x8, x9, [sp, #0x40]
    ldp x6, x7, [sp, #0x30]
    ldp x4, x5, [sp, #0x20]
    ldp x2, x3, [sp, #0x10]
    ldp x0, x1, [sp, #0x00]
    add sp, sp, #{sync_frame_bytes}
    msr spsel, #1
    eret

// Returns to a context saved by the entry above, from kernel code
// running on the stack that context was interrupted on. The frame sits
// exactly one frame below the interrupted SP, so restoring it is the
// same epilogue as above with SP already selected.
.global __helios_aarch64_swap_fault_resume
.type __helios_aarch64_swap_fault_resume, %function
__helios_aarch64_swap_fault_resume:
    msr daifset, #0x2
    mov sp, x0
    ldp x0, x1, [sp, #0x310]
    msr fpsr, x0
    msr fpcr, x1
    ldp q0, q1, [sp, #0x110]
    ldp q2, q3, [sp, #0x130]
    ldp q4, q5, [sp, #0x150]
    ldp q6, q7, [sp, #0x170]
    ldp q8, q9, [sp, #0x190]
    ldp q10, q11, [sp, #0x1b0]
    ldp q12, q13, [sp, #0x1d0]
    ldp q14, q15, [sp, #0x1f0]
    ldp q16, q17, [sp, #0x210]
    ldp q18, q19, [sp, #0x230]
    ldp q20, q21, [sp, #0x250]
    ldp q22, q23, [sp, #0x270]
    ldp q24, q25, [sp, #0x290]
    ldp q26, q27, [sp, #0x2b0]
    ldp q28, q29, [sp, #0x2d0]
    ldp q30, q31, [sp, #0x2f0]
    ldr x1, [sp, #0x100]
    ldp x30, x0, [sp, #0xf0]
    msr elr_el1, x0
    msr spsr_el1, x1
    ldp x28, x29, [sp, #0xe0]
    ldp x26, x27, [sp, #0xd0]
    ldp x24, x25, [sp, #0xc0]
    ldp x22, x23, [sp, #0xb0]
    ldp x20, x21, [sp, #0xa0]
    ldp x18, x19, [sp, #0x90]
    ldp x16, x17, [sp, #0x80]
    ldp x14, x15, [sp, #0x70]
    ldp x12, x13, [sp, #0x60]
    ldp x10, x11, [sp, #0x50]
    ldp x8, x9, [sp, #0x40]
    ldp x6, x7, [sp, #0x30]
    ldp x4, x5, [sp, #0x20]
    ldp x2, x3, [sp, #0x10]
    ldp x0, x1, [sp, #0x00]
    add sp, sp, #{sync_frame_bytes}
    eret
.size __helios_aarch64_swap_fault_resume, . - __helios_aarch64_swap_fault_resume

// IRQ entry. The interrupted context keeps SP_EL1 untouched: selecting
// SP_EL0 switches to the per-processor exception stack that
// `activate_processor_runtime` installed there, so an interrupt taken on
// a small runtime stack cannot overflow it, and returning is a plain
// `msr spsel, #1`. PSTATE.I stays set for the whole handler, so the
// frame below is never re-entered.
__helios_aarch64_irq:
    msr spsel, #0
    sub sp, sp, #{irq_frame_bytes}
    stp x0, x1, [sp, #0x00]
    stp x2, x3, [sp, #0x10]
    stp x4, x5, [sp, #0x20]
    stp x6, x7, [sp, #0x30]
    stp x8, x9, [sp, #0x40]
    stp x10, x11, [sp, #0x50]
    stp x12, x13, [sp, #0x60]
    stp x14, x15, [sp, #0x70]
    stp x16, x17, [sp, #0x80]
    stp x18, x19, [sp, #0x90]
    stp x20, x21, [sp, #0xa0]
    stp x22, x23, [sp, #0xb0]
    stp x24, x25, [sp, #0xc0]
    stp x26, x27, [sp, #0xd0]
    stp x28, x29, [sp, #0xe0]
    mrs x0, elr_el1
    mrs x1, spsr_el1
    stp x30, x0, [sp, #0xf0]
    str x1, [sp, #0x100]
    bl aarch64_handle_irq
    ldr x1, [sp, #0x100]
    ldp x30, x0, [sp, #0xf0]
    msr elr_el1, x0
    msr spsr_el1, x1
    ldp x28, x29, [sp, #0xe0]
    ldp x26, x27, [sp, #0xd0]
    ldp x24, x25, [sp, #0xc0]
    ldp x22, x23, [sp, #0xb0]
    ldp x20, x21, [sp, #0xa0]
    ldp x18, x19, [sp, #0x90]
    ldp x16, x17, [sp, #0x80]
    ldp x14, x15, [sp, #0x70]
    ldp x12, x13, [sp, #0x60]
    ldp x10, x11, [sp, #0x50]
    ldp x8, x9, [sp, #0x40]
    ldp x6, x7, [sp, #0x30]
    ldp x4, x5, [sp, #0x20]
    ldp x2, x3, [sp, #0x10]
    ldp x0, x1, [sp, #0x00]
    add sp, sp, #{irq_frame_bytes}
    msr spsel, #1
    eret

.section .bss.stack, "aw", %nobits
.align 12
.global __aarch64_boot_stack
__aarch64_boot_stack:
    .skip {boot_stack_bytes}
.global __aarch64_boot_stack_top
__aarch64_boot_stack_top:

.section .bss.exception_stack, "aw", %nobits
.align 12
.global __helios_aarch64_boot_exception_stack
__helios_aarch64_boot_exception_stack:
    .skip {exception_stack_bytes}
.global __helios_aarch64_boot_exception_stack_top
__helios_aarch64_boot_exception_stack_top:
