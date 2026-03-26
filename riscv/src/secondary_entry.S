.section .init.secondary, "ax"
.global _secondary_start
_secondary_start:
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

    tail secondary_start_rust
