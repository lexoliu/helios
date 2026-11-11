# Saves the callee-saved context of the executing processor and halts it
# until `helios_x86_resume_current` returns into the caller.
#
#   rdi: where to store the stack pointer the context was saved at
#   rsi: the processor's `smp::ProcessorRuntime`, handed to
#        `helios_x86_halt_stopped`, which never returns
#
# The context is what the SysV ABI makes callee-saved: the six
# general-purpose registers pushed here, and the control bits of MXCSR and
# the x87 control word, kept in the 8-byte slot below them (MXCSR at
# offset 0, the control word at offset 4). INIT resets both, so they are
# restored rather than assumed. Everything else is caller-saved, so the
# caller of `helios_x86_stop_current` already holds nothing live in it.
# The frames below the saved stack pointer are dead once the processor is
# restarted, which is why the restart enters on the stack just under this
# context.
.section .text.helios_x86_stop, "ax"

.global helios_x86_stop_current
helios_x86_stop_current:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    # Six pushes and the return address leave rsp 8 modulo 16; the
    # control-word slot brings it to the 16-byte alignment the call
    # below needs before the return address is pushed.
    sub rsp, 8
    stmxcsr dword ptr [rsp]
    fnstcw word ptr [rsp + 4]
    mov qword ptr [rdi], rsp
    mov rdi, rsi
    call helios_x86_halt_stopped
    ud2

# Returns from `helios_x86_stop_current` on the processor that saved the
# context, with `rdi` the stack pointer it stored.
.global helios_x86_resume_current
helios_x86_resume_current:
    mov rsp, rdi
    ldmxcsr dword ptr [rsp]
    fldcw word ptr [rsp + 4]
    add rsp, 8
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret
