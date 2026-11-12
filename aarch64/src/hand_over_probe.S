// The two traps of the boot-time trap hand-over self-check
// (`hand_over::verify_trap_hand_over`). Both write into the
// `HandOverProbe` whose address arrives in x0; the field offsets are
// operands of the `global_asm!` that includes this file.

.section .text.helios_hand_over_probe, "ax"

// void __helios_aarch64_hand_over_probe_claimed(HandOverProbe *probe)
//
// Raises a `brk` that the hand-over claims the way the runtime claims a
// wasm trap: it writes `sp` and `fp` and branches to the landing below,
// with x0 = x1 = 0 and no other register restored. The routine
// therefore saves every callee-saved register itself before the trap
// and takes them back after it, and the landing reads the processor
// state the claim left behind into the probe.
.global __helios_aarch64_hand_over_probe_claimed
.type __helios_aarch64_hand_over_probe_claimed, %function
.balign 4
__helios_aarch64_hand_over_probe_claimed:
    sub sp, sp, #{save_bytes}
    stp x19, x20, [sp, #0x00]
    stp x21, x22, [sp, #0x10]
    stp x23, x24, [sp, #0x20]
    stp x25, x26, [sp, #0x30]
    stp x27, x28, [sp, #0x40]
    stp x29, x30, [sp, #0x50]
    stp d8, d9, [sp, #0x60]
    stp d10, d11, [sp, #0x70]
    stp d12, d13, [sp, #0x80]
    stp d14, d15, [sp, #0x90]
    // The probe pointer survives the claim only in memory.
    str x0, [sp, #0xa0]
    mov x29, sp
    adr x9, 2f
    mov x10, sp
    str x9, [x0, #{resume_pc}]
    str x10, [x0, #{resume_sp}]
    str x29, [x0, #{resume_fp}]
    // Arm last: the hand-over claims only the `brk` this names.
    adr x9, 1f
    str x9, [x0, #{claim_pc}]
1:
    brk #{claim_brk}
2:
    ldr x10, [sp, #0xa0]
    mrs x9, spsel
    str x9, [x10, #{resumed_spsel}]
    // SP_EL0 is readable at EL1 only while SP_EL1 is selected; the
    // check on the Rust side fails on SPSel first when it is not.
    cbz x9, 3f
    mrs x11, sp_el0
    str x11, [x10, #{resumed_sp_el0}]
3:
    mrs x12, daif
    str x12, [x10, #{resumed_daif}]
    ldp x19, x20, [sp, #0x00]
    ldp x21, x22, [sp, #0x10]
    ldp x23, x24, [sp, #0x20]
    ldp x25, x26, [sp, #0x30]
    ldp x27, x28, [sp, #0x40]
    ldp x29, x30, [sp, #0x50]
    ldp d8, d9, [sp, #0x60]
    ldp d10, d11, [sp, #0x70]
    ldp d12, d13, [sp, #0x80]
    ldp d14, d15, [sp, #0x90]
    add sp, sp, #{save_bytes}
    ret
.size __helios_aarch64_hand_over_probe_claimed, . - __helios_aarch64_hand_over_probe_claimed

// void __helios_aarch64_hand_over_probe_returning(HandOverProbe *probe)
//
// Raises a `brk` the synchronous handler resolves in place: it records
// where the entry built the frame and the SP it saved, steps ELR past
// the `brk`, and the epilogue returns here with every register intact.
.global __helios_aarch64_hand_over_probe_returning
.type __helios_aarch64_hand_over_probe_returning, %function
.balign 4
__helios_aarch64_hand_over_probe_returning:
    mov x10, sp
    str x10, [x0, #{return_sp}]
    adr x9, 1f
    str x9, [x0, #{return_pc}]
1:
    brk #{return_brk}
    ret
.size __helios_aarch64_hand_over_probe_returning, . - __helios_aarch64_hand_over_probe_returning
