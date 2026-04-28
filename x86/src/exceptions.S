.section .text.helios_x86_exceptions, "ax"

.macro HELIOS_X86_EXCEPTION_NO_ERROR name, vector
.global \name
\name:
    push 0
    push \vector
    jmp helios_x86_exception_common
.endm

.macro HELIOS_X86_EXCEPTION_WITH_ERROR name, vector
.global \name
\name:
    push \vector
    jmp helios_x86_exception_common
.endm

HELIOS_X86_EXCEPTION_NO_ERROR helios_x86_exception_divide_error, 0
HELIOS_X86_EXCEPTION_NO_ERROR helios_x86_exception_breakpoint, 3
HELIOS_X86_EXCEPTION_NO_ERROR helios_x86_exception_invalid_opcode, 6
HELIOS_X86_EXCEPTION_NO_ERROR helios_x86_exception_x87_floating_point, 16
HELIOS_X86_EXCEPTION_NO_ERROR helios_x86_exception_simd_floating_point, 19
HELIOS_X86_EXCEPTION_WITH_ERROR helios_x86_exception_general_protection, 13
HELIOS_X86_EXCEPTION_WITH_ERROR helios_x86_exception_page_fault, 14

.global helios_x86_interrupt_timer
helios_x86_interrupt_timer:
    push 0
    push 0x20
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_wake
helios_x86_interrupt_wake:
    push 0
    push 0x21
    jmp helios_x86_interrupt_common

helios_x86_exception_common:
    push rax
    push rcx
    push rdx
    push rbx
    push rbp
    push rsi
    push rdi
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15

    mov rdi, rsp
    mov r12, rsp
    and rsp, -16
    call helios_x86_exception_dispatch
    ud2

helios_x86_interrupt_common:
    push rax
    push rcx
    push rdx
    push rbx
    push rbp
    push rsi
    push rdi
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15

    mov rdi, rsp
    mov r12, rsp
    and rsp, -16
    call helios_x86_interrupt_dispatch
    mov rsp, r12

    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rdi
    pop rsi
    pop rbp
    pop rbx
    pop rdx
    pop rcx
    pop rax
    add rsp, 16
    iretq
