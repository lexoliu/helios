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

__helios_aarch64_sync_exception:
    mrs x4, tpidr_el1
    cbz x4, 2f
    add sp, x4, #{exception_stack_bytes}
    b 3f
2:
    adrp x4, __helios_aarch64_boot_exception_stack_top
    add x4, x4, :lo12:__helios_aarch64_boot_exception_stack_top
    mov sp, x4
3:
    mrs x0, esr_el1
    mrs x1, elr_el1
    mov x2, x29
    mrs x3, far_el1
    bl aarch64_handle_sync_exception
1:
    wfi
    b 1b

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
