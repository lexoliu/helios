.global helios_x86_secondary_wakeup_start
.global helios_x86_secondary_wakeup_end
.global helios_x86_secondary_wakeup_cr3
.global helios_x86_secondary_wakeup_stack_top
.global helios_x86_secondary_wakeup_context
.global helios_x86_secondary_wakeup_runtime
.global helios_x86_secondary_wakeup_entry

.equ HELIOS_X86_PROTECTED_CODE_SELECTOR, 0x08
.equ HELIOS_X86_DATA_SELECTOR, 0x10
.equ HELIOS_X86_LONG_MODE_CODE_SELECTOR, 0x18
.equ HELIOS_X86_CR0_PROTECTED_MODE_ENABLE, 0x00000001
.equ HELIOS_X86_CR0_ENABLE_PAGING, 0x80000000
.equ HELIOS_X86_CR4_ENABLE_PAE, 0x00000020
.equ HELIOS_X86_EFER_MSR, 0xC0000080
.equ HELIOS_X86_EFER_LONG_MODE_ENABLE, 0x00000100
.equ HELIOS_X86_EFER_NO_EXECUTE_ENABLE, 0x00000800
.equ HELIOS_X86_PROTECTED_MODE_START_OFFSET, helios_x86_secondary_wakeup_protected_mode_start - helios_x86_secondary_wakeup_start
.equ HELIOS_X86_GDT_OFFSET, helios_x86_secondary_wakeup_gdt - helios_x86_secondary_wakeup_start
.equ HELIOS_X86_GDT_DESCRIPTOR_OFFSET, helios_x86_secondary_wakeup_gdt_descriptor - helios_x86_secondary_wakeup_start
.equ HELIOS_X86_LONG_MODE_START_OFFSET, helios_x86_secondary_wakeup_long_mode_start - helios_x86_secondary_wakeup_start
.equ HELIOS_X86_LONG_MODE_JUMP_OFFSET, helios_x86_secondary_wakeup_long_mode_jump_offset - helios_x86_secondary_wakeup_start
.equ HELIOS_X86_GDT_CODE32_BASE_LOW_OFFSET, helios_x86_secondary_wakeup_gdt_code32_base_low - helios_x86_secondary_wakeup_start
.equ HELIOS_X86_GDT_CODE32_BASE_MID_OFFSET, helios_x86_secondary_wakeup_gdt_code32_base_mid - helios_x86_secondary_wakeup_start
.equ HELIOS_X86_GDT_CODE32_BASE_HIGH_OFFSET, helios_x86_secondary_wakeup_gdt_code32_base_high - helios_x86_secondary_wakeup_start
.equ HELIOS_X86_GDT_DATA_BASE_LOW_OFFSET, helios_x86_secondary_wakeup_gdt_data_base_low - helios_x86_secondary_wakeup_start
.equ HELIOS_X86_GDT_DATA_BASE_MID_OFFSET, helios_x86_secondary_wakeup_gdt_data_base_mid - helios_x86_secondary_wakeup_start
.equ HELIOS_X86_GDT_DATA_BASE_HIGH_OFFSET, helios_x86_secondary_wakeup_gdt_data_base_high - helios_x86_secondary_wakeup_start
.equ HELIOS_X86_CR3_OFFSET, helios_x86_secondary_wakeup_cr3 - helios_x86_secondary_wakeup_start

.section .text.helios_x86_secondary_wakeup, "ax"
.balign 4096
.intel_syntax noprefix

.code16
helios_x86_secondary_wakeup_start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax

    mov ax, cs
    mov bx, ax
    shl bx, 4
    shr ax, 12

    mov word ptr cs:[HELIOS_X86_GDT_CODE32_BASE_LOW_OFFSET], bx
    mov word ptr cs:[HELIOS_X86_GDT_DATA_BASE_LOW_OFFSET], bx
    mov byte ptr cs:[HELIOS_X86_GDT_CODE32_BASE_MID_OFFSET], al
    mov byte ptr cs:[HELIOS_X86_GDT_DATA_BASE_MID_OFFSET], al

    mov dx, bx
    lea si, [HELIOS_X86_GDT_OFFSET]
    add dx, si
    mov word ptr cs:[HELIOS_X86_GDT_DESCRIPTOR_OFFSET + 2], dx
    mov dl, al
    adc dl, 0
    mov byte ptr cs:[HELIOS_X86_GDT_DESCRIPTOR_OFFSET + 4], dl

    mov dx, bx
    lea si, [HELIOS_X86_LONG_MODE_START_OFFSET]
    add dx, si
    mov word ptr cs:[HELIOS_X86_LONG_MODE_JUMP_OFFSET], dx
    xor cx, cx
    mov cl, al
    adc cx, 0
    mov word ptr cs:[HELIOS_X86_LONG_MODE_JUMP_OFFSET + 2], cx

    lgdt cs:[HELIOS_X86_GDT_DESCRIPTOR_OFFSET]

    mov eax, cr0
    or eax, HELIOS_X86_CR0_PROTECTED_MODE_ENABLE
    mov cr0, eax
helios_x86_secondary_wakeup_enter_protected_mode:
    .byte 0xea
    .word HELIOS_X86_PROTECTED_MODE_START_OFFSET
    .word HELIOS_X86_PROTECTED_CODE_SELECTOR

.code32
helios_x86_secondary_wakeup_protected_mode_start:
    mov ax, HELIOS_X86_DATA_SELECTOR
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax

    mov eax, dword ptr [HELIOS_X86_CR3_OFFSET]
    mov cr3, eax

    mov eax, cr4
    or eax, HELIOS_X86_CR4_ENABLE_PAE
    mov cr4, eax

    mov ecx, HELIOS_X86_EFER_MSR
    rdmsr
    or eax, HELIOS_X86_EFER_LONG_MODE_ENABLE | HELIOS_X86_EFER_NO_EXECUTE_ENABLE
    wrmsr

    mov eax, cr0
    or eax, HELIOS_X86_CR0_ENABLE_PAGING
    mov cr0, eax
helios_x86_secondary_wakeup_long_mode_jump:
    .byte 0xea
helios_x86_secondary_wakeup_long_mode_jump_offset:
    .long 0
    .word HELIOS_X86_LONG_MODE_CODE_SELECTOR

.code64
helios_x86_secondary_wakeup_long_mode_start:
    mov ax, HELIOS_X86_DATA_SELECTOR
    mov ds, ax
    mov es, ax
    mov ss, ax
    xor eax, eax
    mov fs, ax
    mov gs, ax

    mov rsp, qword ptr [rip + helios_x86_secondary_wakeup_stack_top]
    xor rbp, rbp
    mov rdi, qword ptr [rip + helios_x86_secondary_wakeup_context]
    mov rsi, qword ptr [rip + helios_x86_secondary_wakeup_runtime]
    mov rax, qword ptr [rip + helios_x86_secondary_wakeup_entry]
    call rax
    ud2

.balign 8
helios_x86_secondary_wakeup_gdt:
    .quad 0
helios_x86_secondary_wakeup_gdt_code32:
    .word 0xffff
helios_x86_secondary_wakeup_gdt_code32_base_low:
    .word 0
helios_x86_secondary_wakeup_gdt_code32_base_mid:
    .byte 0
    .byte 0x9a
    .byte 0xcf
helios_x86_secondary_wakeup_gdt_code32_base_high:
    .byte 0
helios_x86_secondary_wakeup_gdt_data:
    .word 0xffff
helios_x86_secondary_wakeup_gdt_data_base_low:
    .word 0
helios_x86_secondary_wakeup_gdt_data_base_mid:
    .byte 0
    .byte 0x92
    .byte 0xcf
helios_x86_secondary_wakeup_gdt_data_base_high:
    .byte 0
helios_x86_secondary_wakeup_gdt_code64:
    .quad 0x00af9a000000ffff
helios_x86_secondary_wakeup_gdt_descriptor:
    .word helios_x86_secondary_wakeup_gdt_end - helios_x86_secondary_wakeup_gdt - 1
    .long 0
helios_x86_secondary_wakeup_gdt_end:

.balign 8
helios_x86_secondary_wakeup_cr3:
    .quad 0
helios_x86_secondary_wakeup_stack_top:
    .quad 0
helios_x86_secondary_wakeup_context:
    .quad 0
helios_x86_secondary_wakeup_runtime:
    .quad 0
helios_x86_secondary_wakeup_entry:
    .quad 0
helios_x86_secondary_wakeup_end:
.att_syntax
