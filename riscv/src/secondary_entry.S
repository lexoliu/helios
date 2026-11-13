// The two ways a hart enters the kernel from SBI `hart_start`:
//
// `_secondary_start` is the first start. `a0` is the hart id and `a1` the
// device tree; the hart takes the top of its own boot stack.
//
// `_hart_restart` is the restart of a hart `Cpu::stop_current` took out of
// service. `a0` is the hart id and `a1` the `SavedContext` the hart left
// on its own stack; the hart continues on that stack, below the frame
// `stop_current` suspended in, and `helios_hart_resume` (`lifecycle.rs`)
// carries on from there.
//
// Both leave `sstatus.FS` and `fcsr` in the same state before any Rust
// runs, because `sbi_hart_start` leaves them unspecified in either case.
// The byte offsets below are those of `SavedContext`, which `lifecycle.rs`
// asserts.
.equ HELIOS_CONTEXT_SP, 0x08
.equ HELIOS_CONTEXT_TP, 0x10

.section .init.secondary, "ax"
.global _secondary_start
.global _hart_restart
_secondary_start:
    .option push
    .option arch, +zmmul
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop

    mv t2, a0
    lui t1, %hi(_hart_stack_size)
    add t1, t1, %lo(_hart_stack_size)
    mul t0, t2, t1
    la t1, _stack_start
    sub t1, t1, t0
    andi sp, t1, -16

    la t3, secondary_start_rust
    j .Lhart_enter

_hart_restart:
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop

    ld sp, HELIOS_CONTEXT_SP(a1)
    ld tp, HELIOS_CONTEXT_TP(a1)
    la t3, helios_hart_resume

.Lhart_enter:
    // Enable floating point, exactly as `riscv-rt`'s `_start` does for the
    // boot hart: clear `sstatus.FS`, set it to `Initial`, and zero `fcsr`.
    // `sbi_hart_start` leaves `FS` unspecified, so without this a hart
    // runs Rust compiled for `lp64d` on whatever the firmware happened to
    // leave behind — `Off` would fault on the first double, and a
    // stale `Dirty` would inherit another hart's rounding mode.
    .option push
    .option arch, +f
    li t0, 0x4000
    csrc sstatus, t0
    li t0, 0x2000
    csrs sstatus, t0
    fscsr x0
    .option pop

    jr t3
    .option pop
