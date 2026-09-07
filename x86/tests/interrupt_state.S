.intel_syntax noprefix
#include "entry.S"
.text

.macro interrupt target
    mov rax, rsp
    xor ecx, ecx
    mov cx, ss
    push rcx
    push rax
    pushfq
    xor ecx, ecx
    mov cx, cs
    push rcx
    lea rax, [rip + 1f]
    push rax
    jmp \target
1:
.endm

.global main
.global _main
main:
_main:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    sub rsp, 1560
    mov rbx, rsp
    fxsave64 [rbx]
    lea rdi, [rbx + 512]
    xor eax, eax
    mov ecx, 128
    rep stosq
    fninit
    fld1
    fldpi
    mov word ptr [rbx + 1536], 0x077f
    fldcw [rbx + 1536]
    mov dword ptr [rbx + 1540], 0x3f80
    ldmxcsr [rbx + 1540]
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
        mov eax, \n + 1
        movd xmm\n, eax
        pshufd xmm\n, xmm\n, 0
    .endr
    fxsave64 [rbx + 512]
    mov r13d, 1
    mov r14d, 0
    interrupt helios_x86_interrupt_timer
    fxsave64 [rbx + 1024]
    mov r15d, 1
    lea rsi, [rbx + 512]
    lea rdi, [rbx + 1024]
    mov ecx, 5
    repe cmpsb
    jne failed
    mov r15d, 2
    mov eax, dword ptr [rbx + 536]
    cmp eax, dword ptr [rbx + 1048]
    jne failed
    mov r15d, 3
    lea rsi, [rbx + 544]
    lea rdi, [rbx + 1056]
    mov edx, 8
compare_x87:
    mov ecx, 10
    repe cmpsb
    jne failed
    add rsi, 6
    add rdi, 6
    dec edx
    jne compare_x87
    mov r15d, 4
    mov ecx, 256
    repe cmpsb
    jne failed
    mov r15d, 5
    cmp r13d, 1
    jne failed
    test r14, r14
    jne failed
    xor r15d, r15d
failed:
    fxrstor64 [rbx]
    mov eax, r15d
    add rsp, 1560
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

.global helios_x86_interrupt_dispatch
helios_x86_interrupt_dispatch:
    push r13
    push r14
    sub rsp, 8
    stmxcsr [rsp]
    cmp dword ptr [rsp], 0x1f80
    jne invalid_environment
    fnstcw [rsp + 4]
    cmp word ptr [rsp + 4], 0x037f
    je environment_checked
invalid_environment:
    mov qword ptr [rdi + 8], 1
environment_checked:
    fninit
    mov dword ptr [rsp], 0x5f80
    ldmxcsr [rsp]
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
        pxor xmm\n, xmm\n
    .endr
    test r13, r13
    je nested
    xor r13d, r13d
    interrupt helios_x86_interrupt_network
nested:
    add rsp, 8
    pop r14
    pop r13
    ret

.global helios_x86_exception_dispatch
helios_x86_exception_dispatch:
    ud2

#ifndef __APPLE__
.section .note.GNU-stack,"",@progbits
#endif
