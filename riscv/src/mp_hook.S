.section .init.mp_hook, "ax"
.global _mp_hook
_mp_hook:
    li a0, 1
    ret
