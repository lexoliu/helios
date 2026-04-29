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
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
    .balign 128
    b __helios_aarch64_sync_exception
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
