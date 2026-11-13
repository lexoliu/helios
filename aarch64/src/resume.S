// Re-entry of a processor that PSCI `CPU_OFF` powered down.
//
// `CPU_ON` starts the target at `__helios_aarch64_cpu_resume` with the MMU
// and data caches off, at the highest non-secure exception level, with
// `x0` holding the physical address of the `ResumeContext` the processor
// saved before it left. Everything here is position independent and
// touches only that context until the MMU is on. The offsets are pinned
// by assertions in `lifecycle.rs`.
.section .text.cpu_resume, "ax"
.global __helios_aarch64_cpu_resume
.type __helios_aarch64_cpu_resume, %function
__helios_aarch64_cpu_resume:
    msr daifset, #0xf
    mrs x1, CurrentEL
    lsr x1, x1, #2
    cmp x1, #2
    b.ne 2f
    // Entered at EL2: configure it to run EL1 as AArch64 with the
    // generic timer, FP/SIMD and the identification registers visible,
    // then drop to EL1 at the next instruction.
    mov x1, #(1 << 31)
    msr hcr_el2, x1
    mrs x1, cnthctl_el2
    orr x1, x1, #3
    msr cnthctl_el2, x1
    msr cntvoff_el2, xzr
    mrs x1, midr_el1
    msr vpidr_el2, x1
    mrs x1, mpidr_el1
    msr vmpidr_el2, x1
    mov x1, #0x33ff
    msr cptr_el2, x1
    msr hstr_el2, xzr
    mov x1, #0x0800
    movk x1, #0x30d0, lsl #16
    msr sctlr_el1, x1
    mov x1, #0x3c5
    msr spsr_el2, x1
    adr x1, 2f
    msr elr_el2, x1
    isb
    eret
2:
    msr spsel, #1
    // Install the translation regime. TTBR0 is the context's identity
    // map of this very code, so the instruction after the SCTLR write
    // still fetches; TTBR1 is the kernel's own mapping.
    ldr x1, [x0, #168]
    msr mair_el1, x1
    ldr x1, [x0, #264]
    msr tcr_el1, x1
    ldr x1, [x0, #272]
    msr ttbr0_el1, x1
    ldr x1, [x0, #192]
    msr ttbr1_el1, x1
    ldr x2, [x0, #280]
    ldr x3, [x0, #288]
    ldr x4, [x0, #200]
    isb
    tlbi vmalle1
    ic iallu
    dsb nsh
    isb
    msr sctlr_el1, x4
    isb
    mov x0, x2
    br x3
.size __helios_aarch64_cpu_resume, . - __helios_aarch64_cpu_resume
.global __helios_aarch64_cpu_resume_end
__helios_aarch64_cpu_resume_end:

// Second half, running at the kernel's virtual address with the MMU on
// and `x0` holding the context's virtual address. It restores the
// processor's own translation registers, exception state and callee-saved
// registers, then returns 0 to the caller of `save_and_power_off`.
.section .text.cpu_resume_virtual, "ax"
.global __helios_aarch64_cpu_resume_virtual
.type __helios_aarch64_cpu_resume_virtual, %function
__helios_aarch64_cpu_resume_virtual:
    ldr x1, [x0, #176]
    ldr x2, [x0, #184]
    msr ttbr0_el1, x2
    msr tcr_el1, x1
    isb
    tlbi vmalle1
    dsb nsh
    isb
    ldr x1, [x0, #208]
    msr cpacr_el1, x1
    isb
    ldr x1, [x0, #216]
    msr vbar_el1, x1
    ldr x1, [x0, #224]
    msr tpidr_el1, x1
    ldr x1, [x0, #232]
    msr sp_el0, x1
    ldr x1, [x0, #240]
    msr fpcr, x1
    isb
    ldp x19, x20, [x0, #0]
    ldp x21, x22, [x0, #16]
    ldp x23, x24, [x0, #32]
    ldp x25, x26, [x0, #48]
    ldp x27, x28, [x0, #64]
    ldp x29, x30, [x0, #80]
    ldr x1, [x0, #96]
    mov sp, x1
    ldp d8, d9, [x0, #104]
    ldp d10, d11, [x0, #120]
    ldp d12, d13, [x0, #136]
    ldp d14, d15, [x0, #152]
    mov x0, #0
    ret
.size __helios_aarch64_cpu_resume_virtual, . - __helios_aarch64_cpu_resume_virtual
