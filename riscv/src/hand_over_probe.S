// The traps of the boot-time trap hand-over self-check
// (`trap::verify_trap_hand_over`). Both routines write into the
// `HandOverProbe` whose address arrives in a0; the field offsets are
// operands of the `global_asm!` that includes this file.

// `sip.SSIP`.
.equ HAND_OVER_PROBE_SIP_SSIP, 0x2

// `global_asm!` starts from the base ISA regardless of the target's own
// feature set, the same reason `trap.S` asks for `f` and `d` before it
// saves the floating-point file. `norvc` keeps each `ebreak` four bytes
// long, which is what the dispatcher steps past.
.option push
.option arch, +f, +d
.option norvc

.section .text.helios_hand_over_probe, "ax"

// void __helios_riscv_hand_over_probe_claimed(HandOverProbe *probe)
//
// Raises an `ebreak` that the hand-over claims the way the runtime
// claims a wasm trap: it writes `sp` and `fp` and jumps to the landing
// below, with a0 = a1 = 0 and no other register restored. The routine
// therefore saves every callee-saved register itself before the trap and
// takes them back after it, and the landing reads the hart state the
// claim left behind into the probe.
.global __helios_riscv_hand_over_probe_claimed
.balign 4
__helios_riscv_hand_over_probe_claimed:
    addi sp, sp, -208
    sd ra, 0(sp)
    sd s0, 8(sp)
    sd s1, 16(sp)
    sd s2, 24(sp)
    sd s3, 32(sp)
    sd s4, 40(sp)
    sd s5, 48(sp)
    sd s6, 56(sp)
    sd s7, 64(sp)
    sd s8, 72(sp)
    sd s9, 80(sp)
    sd s10, 88(sp)
    sd s11, 96(sp)
    fsd fs0, 104(sp)
    fsd fs1, 112(sp)
    fsd fs2, 120(sp)
    fsd fs3, 128(sp)
    fsd fs4, 136(sp)
    fsd fs5, 144(sp)
    fsd fs6, 152(sp)
    fsd fs7, 160(sp)
    fsd fs8, 168(sp)
    fsd fs9, 176(sp)
    fsd fs10, 184(sp)
    fsd fs11, 192(sp)
    // The probe pointer survives the claim only in memory.
    sd a0, 200(sp)
    mv s0, sp
    lla t0, 2f
    sd t0, {resume_pc}(a0)
    sd sp, {resume_sp}(a0)
    sd s0, {resume_fp}(a0)
    // Arm last: the hand-over claims only the `ebreak` this names.
    lla t0, 1f
    sd t0, {claim_pc}(a0)
1:
    ebreak
2:
    ld t1, 200(sp)
    csrr t0, sscratch
    sd t0, {resumed_sscratch}(t1)
    csrr t0, sstatus
    sd t0, {resumed_sstatus}(t1)
    ld ra, 0(sp)
    ld s0, 8(sp)
    ld s1, 16(sp)
    ld s2, 24(sp)
    ld s3, 32(sp)
    ld s4, 40(sp)
    ld s5, 48(sp)
    ld s6, 56(sp)
    ld s7, 64(sp)
    ld s8, 72(sp)
    ld s9, 80(sp)
    ld s10, 88(sp)
    ld s11, 96(sp)
    fld fs0, 104(sp)
    fld fs1, 112(sp)
    fld fs2, 120(sp)
    fld fs3, 128(sp)
    fld fs4, 136(sp)
    fld fs5, 144(sp)
    fld fs6, 152(sp)
    fld fs7, 160(sp)
    fld fs8, 168(sp)
    fld fs9, 176(sp)
    fld fs10, 184(sp)
    fld fs11, 192(sp)
    addi sp, sp, 208
    ret

// void __helios_riscv_hand_over_probe_returning(HandOverProbe *probe)
//
// Raises a supervisor software interrupt on this hart, then an `ebreak`
// the dispatcher resolves in place. The dispatcher records, for each,
// where the entry built the frame and the `sp` it saved, and the
// routine records the `sp` it really ran on.
.global __helios_riscv_hand_over_probe_returning
.balign 4
__helios_riscv_hand_over_probe_returning:
    sd sp, {interrupt_sp}(a0)
    // Arm after `sp` is recorded: any interrupt from here on, the timer's
    // included, interrupts code running on that `sp`.
    li t0, -1
    sd t0, {interrupt_frame}(a0)
    // `sie.SSIE` and `sstatus.SIE` are set, so the hart takes the
    // interrupt before it retires the next instruction.
    li t0, HAND_OVER_PROBE_SIP_SSIP
    csrs sip, t0
    sd sp, {return_sp}(a0)
    lla t0, 1f
    sd t0, {return_pc}(a0)
1:
    ebreak
    ret

.option pop
