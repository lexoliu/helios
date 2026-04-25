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

.section .bss.stack, "aw", %nobits
.align 12
.global __aarch64_boot_stack
__aarch64_boot_stack:
    .skip {boot_stack_bytes}
.global __aarch64_boot_stack_top
__aarch64_boot_stack_top:
