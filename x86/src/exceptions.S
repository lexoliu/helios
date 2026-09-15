.section .text.helios_x86_exceptions, "ax"

// Bytes reserved on the stack for one XSAVE area. Large enough for every
// component `extended_state::enable_for_current_processor` may enable
// (x87, SSE, AVX, AVX-512 opmask/ZMM), which it asserts against the
// size CPUID reports for the XCR0 it programmed.
.equ HELIOS_X86_XSAVE_AREA_BYTES, 4096

// Saves the interrupted context's extended register state below the
// pushed general registers and gives the handler a clean x87/SSE
// control state. Every general register is already on the stack, so
// rax, rcx, rdx and rdi are free to zero the XSAVE header: XRSTOR
// refuses an area whose header (bytes 512..575) carries anything but
// what XSAVE wrote, and uninitialised stack memory carries anything.
// The mask in edx:eax selects every component XCR0 enables.
.macro HELIOS_X86_XSAVE_ENTER
    sub rsp, HELIOS_X86_XSAVE_AREA_BYTES
    and rsp, -64
    lea rdi, [rsp + 512]
    xor eax, eax
    mov ecx, 8
    cld
    rep stosq
    mov eax, 0xffffffff
    mov edx, 0xffffffff
    xsave64 [rsp]
    sub rsp, 16
    mov dword ptr [rsp], 0x1f80
    fninit
    ldmxcsr [rsp]
.endm

// Restores what HELIOS_X86_XSAVE_ENTER saved; the caller then drops the
// area by reloading the stack pointer it kept.
.macro HELIOS_X86_XSAVE_LEAVE
    add rsp, 16
    mov eax, 0xffffffff
    mov edx, 0xffffffff
    xrstor64 [rsp]
.endm

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
HELIOS_X86_EXCEPTION_WITH_ERROR helios_x86_exception_double_fault, 8
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

.global helios_x86_interrupt_display
helios_x86_interrupt_display:
    push 0
    push 0x38
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_input_0
helios_x86_interrupt_input_0:
    push 0
    push 0x39
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_input_1
helios_x86_interrupt_input_1:
    push 0
    push 0x3a
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_input_2
helios_x86_interrupt_input_2:
    push 0
    push 0x3b
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_input_3
helios_x86_interrupt_input_3:
    push 0
    push 0x3c
    jmp helios_x86_interrupt_common

.global helios_x86_interrupt_sound
helios_x86_interrupt_sound:
    push 0
    push 0x3d
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

    mov r12, rsp
    // The dispatcher returns when the kernel resolved the fault in
    // place, so the interrupted context's whole extended register file
    // (x87, SSE, AVX and AVX-512 state, everything XCR0 enables) has
    // to survive it exactly as it survives an interrupt handler.
    HELIOS_X86_XSAVE_ENTER
    mov rdi, r12
    call helios_x86_exception_dispatch
    HELIOS_X86_XSAVE_LEAVE
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

    mov r12, rsp
    HELIOS_X86_XSAVE_ENTER
    mov rdi, r12
    call helios_x86_interrupt_dispatch
    HELIOS_X86_XSAVE_LEAVE
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
