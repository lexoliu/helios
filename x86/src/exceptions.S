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

.global helios_x86_interrupt_tlb_shootdown
helios_x86_interrupt_tlb_shootdown:
    push 0
    push 0x22
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_network
helios_x86_interrupt_network:
    push 0
    push 0x30
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_network_queue_0
helios_x86_interrupt_network_queue_0:
    push 0
    push 0x40
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_network_queue_1
helios_x86_interrupt_network_queue_1:
    push 0
    push 0x41
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_network_queue_2
helios_x86_interrupt_network_queue_2:
    push 0
    push 0x42
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_network_queue_3
helios_x86_interrupt_network_queue_3:
    push 0
    push 0x43
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_network_queue_4
helios_x86_interrupt_network_queue_4:
    push 0
    push 0x44
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_network_queue_5
helios_x86_interrupt_network_queue_5:
    push 0
    push 0x45
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_network_queue_6
helios_x86_interrupt_network_queue_6:
    push 0
    push 0x46
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_network_queue_7
helios_x86_interrupt_network_queue_7:
    push 0
    push 0x47
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_host_fs
helios_x86_interrupt_host_fs:
    push 0
    push 0x31
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_entropy
helios_x86_interrupt_entropy:
    push 0
    push 0x32
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_vsock
helios_x86_interrupt_vsock:
    push 0
    push 0x37
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_block_0
helios_x86_interrupt_block_0:
    push 0
    push 0x33
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_block_1
helios_x86_interrupt_block_1:
    push 0
    push 0x34
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_block_2
helios_x86_interrupt_block_2:
    push 0
    push 0x35
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_block_3
helios_x86_interrupt_block_3:
    push 0
    push 0x36
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
