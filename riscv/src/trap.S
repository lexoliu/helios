// Supervisor trap entry for the RISC-V backend.
//
// The kernel never leaves supervisor mode: `run_user` is never called,
// nothing writes `sscratch`, and `handle_user_exception` exists only to
// panic if `sstatus.SPP` ever comes back clear. So this entry assumes a
// valid supervisor stack and builds the frame on it, the same way the
// aarch64 backend's exception entries do.
//
// It saves the floating-point file as well as the integer file.
// `riscv64gc-unknown-none-elf` is `+f,+d` with the `lp64d` ABI and
// `riscv-rt` leaves `sstatus.FS` enabled, so compiled kernel code holds
// live values in `f0`-`f31` and passes doubles in them. The handler this
// entry calls is ordinary Rust and clobbers every one of them; an entry
// that restored only the integer file would hand the interrupted code
// silently wrong numbers, which is the aarch64 defect from #93 with the
// registers renamed.
//
// The layout below is the layout of `TrapFrame` in `trap.rs`, and the
// `offset_of!` assertions there fail the build if the two drift apart.

.equ HELIOS_TRAP_SP,          0x010
.equ HELIOS_TRAP_SSTATUS,     0x100
.equ HELIOS_TRAP_SEPC,        0x108
.equ HELIOS_TRAP_FCSR,        0x110
.equ HELIOS_TRAP_F0,          0x118
.equ HELIOS_TRAP_FRAME_BYTES, 0x220

// `sstatus.FS`, bits 14:13. `Off` means this hart has no floating-point
// state to preserve — and also that touching `f0`-`f31` here would fault.
.equ SSTATUS_FS_MASK, 0x6000

// `global_asm!` starts from the base ISA regardless of the target's own
// feature set, the same reason `secondary_entry.S` has to ask for
// `zmmul` before it multiplies.
.option push
.option arch, +f, +d

.section .text.helios_trap_entry, "ax"
.global __helios_riscv_trap_entry
.balign 4
__helios_riscv_trap_entry:
    addi sp, sp, -HELIOS_TRAP_FRAME_BYTES
    // `x0` is stored so the frame's `zero` field reads as it should in a
    // panic dump rather than as whatever the stack held.
    sd x0, 0(sp)
    .irp n, 1,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    sd x\n, (\n * 8)(sp)
    .endr
    // `x2` is the interrupted stack pointer, which is where this frame
    // ends.
    addi t0, sp, HELIOS_TRAP_FRAME_BYTES
    sd t0, HELIOS_TRAP_SP(sp)
    csrr t1, sstatus
    csrr t2, sepc
    sd t1, HELIOS_TRAP_SSTATUS(sp)
    sd t2, HELIOS_TRAP_SEPC(sp)

    li t3, SSTATUS_FS_MASK
    and t3, t1, t3
    beqz t3, 1f
    frcsr t4
    sd t4, HELIOS_TRAP_FCSR(sp)
    .irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    fsd f\n, (HELIOS_TRAP_F0 + \n * 8)(sp)
    .endr
1:
    mv a0, sp
    call __helios_riscv_trap_dispatch

    // The dispatcher may have replaced the whole frame — that is how a
    // preempted compute task is switched — so every value below comes
    // back out of the frame rather than out of a register held across
    // the call.
    ld t1, HELIOS_TRAP_SSTATUS(sp)
    li t3, SSTATUS_FS_MASK
    and t3, t1, t3
    // Adopt the frame's `FS` before touching `f0`-`f31`: the frame being
    // restored need not be the one that was saved, and its `FS` is the
    // one that says whether these loads are legal.
    csrw sstatus, t1
    beqz t3, 2f
    ld t4, HELIOS_TRAP_FCSR(sp)
    fscsr t4
    .irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    fld f\n, (HELIOS_TRAP_F0 + \n * 8)(sp)
    .endr
    // Writing the floating-point file set `FS` to `Dirty`. Put the value
    // the frame recorded back, so `sret` resumes with the interrupted
    // context's own `FS` rather than a coarser one.
    csrw sstatus, t1
2:
    ld t2, HELIOS_TRAP_SEPC(sp)
    csrw sepc, t2
    .irp n, 1,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    ld x\n, (\n * 8)(sp)
    .endr
    // The stack pointer last: everything above is addressed through it.
    ld x2, HELIOS_TRAP_SP(sp)
    sret

.option pop
