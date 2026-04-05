.section .data.mp_hook, "aw"
.balign 4
.global __helios_mp_gate
__helios_mp_gate:
    .word 0

.section .init.mp_hook, "ax"
.global _mp_hook
_mp_hook:
    la t0, __helios_mp_gate
    lw t1, 0(t0)
    beqz t1, 2f
1:
    li t2, 2
    beq t1, t2, 3f
    lw t1, 0(t0)
    j 1b
2:
    li t1, 1
    amoswap.w.aq t2, t1, (t0)
    beqz t2, 4f
    mv t1, t2
    j 1b
3:
    fence r, rw
    li a0, 0
    ret
4:
    li a0, 1
    ret

.section .text.mp_hook, "ax"
.balign 4
.global _release_mp_harts
_release_mp_harts:
    la t0, __helios_mp_gate
    fence rw, w
    li t1, 2
    sw t1, 0(t0)
    ret
