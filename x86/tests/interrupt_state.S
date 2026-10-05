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
    sub rsp, 17416
    mov rbx, rsp
    mov r11d, edi
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
    mov r12d, 0
    mov r15, rbx
    mov eax, 1
    cpuid
    mov rbx, r15
    test ecx, 1 << 27
    jz no_avx
    test ecx, 1 << 28
    jz no_avx
    xor ecx, ecx
    xgetbv
    mov r8d, eax
    mov r9d, edx
    and eax, 0b110
    cmp eax, 0b110
    jne no_avx
    mov r12d, 1
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
        .irp q,0,4,8,12,16,20,24,28
            mov dword ptr [rbx + 8192 + \n * 32 + \q], \n + \q + 1
        .endr
        vmovdqu ymm\n, [rbx + 8192 + \n * 32]
    .endr
    mov r15, rbx
    xor eax, eax
    cpuid
    mov rbx, r15
    cmp eax, 7
    jb avx_ready
    mov eax, 7
    xor ecx, ecx
    cpuid
    mov r10d, ebx
    mov rbx, r15
    test r10d, 1 << 16
    jz avx_ready
    mov eax, r8d
    and eax, 0xe0
    cmp eax, 0xe0
    jne avx_ready
    mov r12d, 3
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        .irp q,0,4,8,12,16,20,24,28,32,36,40,44,48,52,56,60
            mov dword ptr [rbx + 12288 + \n * 64 + \q], \n + \q + 1
        .endr
        vmovdqu64 zmm\n, [rbx + 12288 + \n * 64]
    .endr
    test r10d, 1 << 30
    jz avx512_opmask_w
    .irp n,0,1,2,3,4,5,6,7
        mov rax, 0x0101010101010101 + \n
        mov qword ptr [rbx + 16384 + \n * 8], rax
        kmovq k\n, rax
    .endr
    jmp avx_ready
avx512_opmask_w:
    .irp n,0,1,2,3,4,5,6,7
        mov rax, 0x0000000000001001 + \n
        mov qword ptr [rbx + 16384 + \n * 8], rax
        kmovw k\n, eax
    .endr
avx_ready:
    cmp r12d, 0
    je no_avx
    interrupt helios_x86_interrupt_timer
no_avx:
    cmp r12d, 0
    je extended_state_checked
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
        vmovdqu [rbx + 10240 + \n * 32], ymm\n
    .endr
    mov rsi, rbx
    add rsi, 8192
    mov rdi, rbx
    add rdi, 10240
    mov ecx, 64
    repe cmpsq
    jne ymm_failed
    cmp r12d, 3
    jne extended_state_checked
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        vmovdqu64 [rbx + 14336 + \n * 64], zmm\n
    .endr
    mov rsi, rbx
    add rsi, 12288
    mov rdi, rbx
    add rdi, 14336
    mov ecx, 256
    repe cmpsq
    jne zmm_failed
    test r10d, 1 << 30
    jz check_opmask_w
    .irp n,0,1,2,3,4,5,6,7
        kmovq rax, k\n
        mov qword ptr [rbx + 16448 + \n * 8], rax
    .endr
    jmp compare_opmask
check_opmask_w:
    .irp n,0,1,2,3,4,5,6,7
        kmovw eax, k\n
        mov qword ptr [rbx + 16448 + \n * 8], rax
    .endr
compare_opmask:
    mov rsi, rbx
    add rsi, 16384
    mov rdi, rbx
    add rdi, 16448
    mov ecx, 8
    repe cmpsq
    jne opmask_failed
extended_state_checked:
    mov r15d, 5
    cmp r13d, 1
    jne failed
    test r14, r14
    jne failed
    xor r15d, r15d
failed:
    fxrstor64 [rbx]
    mov eax, r15d
    test eax, eax
    jne return
    cmp r11d, 1
    jle return
    mov eax, r12d
return:
    add rsp, 17416
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

ymm_failed:
    mov r15d, 6
    jmp failed
zmm_failed:
    mov r15d, 7
    jmp failed
opmask_failed:
    mov r15d, 8
    jmp failed

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
