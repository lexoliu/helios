.section .init.secondary, "ax"
.global _secondary_start
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

    // Enable floating point, exactly as `riscv-rt`'s `_start` does for the
    // boot hart: clear `sstatus.FS`, set it to `Initial`, and zero `fcsr`.
    // `sbi_hart_start` leaves `FS` unspecified, so without this a secondary
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

    tail secondary_start_rust
    .option pop
