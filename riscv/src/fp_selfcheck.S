// usize __helios_riscv_fp_trap_selfcheck(usize seed)
//
// Proves, on the hart that calls it, that a supervisor trap taken in the
// middle of floating-point code returns with `f0`-`f31` and `fcsr`
// intact. The trap is a supervisor software interrupt this routine
// raises on itself, so the window is exact rather than a race against
// the timer.
//
// Returns 0 when every register survived; `n + 1` when `fn` came back
// wrong; 33 when `fcsr` came back wrong; 34 when the interrupt never
// fired, in which case the routine proved nothing and says so instead of
// reporting a pass.
//
// The first instruction that touches a floating-point register also
// settles the other half of the question: if this hart reached Rust with
// `sstatus.FS == Off`, it faults here at bring-up rather than somewhere
// far away.

// `frm = 001` (round toward zero) with no accrued exception flags: a
// value neither reset nor `fscsr x0` leaves behind.
.equ FP_SELFCHECK_FCSR, 0x20
// `sip.SSIP`.
.equ SIP_SSIP, 0x2
.equ FP_SELFCHECK_SAVE_BYTES, 112
.equ FP_SELFCHECK_FCSR_SLOT, 96

// `global_asm!` starts from the base ISA regardless of the target's own
// feature set, the same reason `secondary_entry.S` has to ask for
// `zmmul` before it multiplies.
.option push
.option arch, +f, +d

.section .text.helios_fp_selfcheck, "ax"
.global __helios_riscv_fp_trap_selfcheck
.balign 4
__helios_riscv_fp_trap_selfcheck:
    // `f8`, `f9` and `f18`-`f27` are callee-saved, and `fcsr` carries the
    // caller's rounding mode; this routine overwrites all of them on
    // purpose.
    addi sp, sp, -FP_SELFCHECK_SAVE_BYTES
    fsd f8, 0(sp)
    fsd f9, 8(sp)
    fsd f18, 16(sp)
    fsd f19, 24(sp)
    fsd f20, 32(sp)
    fsd f21, 40(sp)
    fsd f22, 48(sp)
    fsd f23, 56(sp)
    fsd f24, 64(sp)
    fsd f25, 72(sp)
    fsd f26, 80(sp)
    fsd f27, 88(sp)
    frcsr t0
    sd t0, FP_SELFCHECK_FCSR_SLOT(sp)

    li t0, FP_SELFCHECK_FCSR
    fscsr t0
    // `fmv.d.x` moves the raw 64-bit pattern, so `seed + n` comes back
    // out of `fn` bit-for-bit through the `fsd`/`fld` pair in the trap
    // entry.
    mv t0, a0
    .irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    fmv.d.x f\n, t0
    addi t0, t0, 1
    .endr

    // `sip.SSIP` is writable from supervisor mode, and the caller has
    // already enabled `sie.SSIE` and `sstatus.SIE`, so the hart takes the
    // trap before it retires the next instruction.
    li t1, SIP_SSIP
    csrs sip, t1
    // The dispatcher clears `SSIP`. A bit still pending here means the
    // interrupt was never delivered, so nothing was tested.
    csrr t2, sip
    and t2, t2, t1
    bnez t2, .Lselfcheck_no_trap

    mv t0, a0
    li a0, 1
    .irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    fmv.x.d t3, f\n
    bne t3, t0, .Lselfcheck_done
    addi t0, t0, 1
    addi a0, a0, 1
    .endr
    // 33 registers checked, `a0` is 33, and `fcsr` is the last of them.
    frcsr t3
    li t4, FP_SELFCHECK_FCSR
    bne t3, t4, .Lselfcheck_done
    li a0, 0

.Lselfcheck_done:
    fld f8, 0(sp)
    fld f9, 8(sp)
    fld f18, 16(sp)
    fld f19, 24(sp)
    fld f20, 32(sp)
    fld f21, 40(sp)
    fld f22, 48(sp)
    fld f23, 56(sp)
    fld f24, 64(sp)
    fld f25, 72(sp)
    fld f26, 80(sp)
    fld f27, 88(sp)
    ld t0, FP_SELFCHECK_FCSR_SLOT(sp)
    fscsr t0
    addi sp, sp, FP_SELFCHECK_SAVE_BYTES
    ret

.Lselfcheck_no_trap:
    csrc sip, t1
    li a0, 34
    j .Lselfcheck_done

.option pop
