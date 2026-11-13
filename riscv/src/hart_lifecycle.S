// The register halves of `Cpu::stop_current`, around the SBI `hart_stop`
// that `lifecycle.rs` issues.
//
// A hart that stops loses every register, and a hart that restarts
// resumes mid-function, so the only state that must survive is what the
// calling convention says a callee preserves: `ra`, `sp`, `s0`-`s11`,
// `fs0`-`fs11`, and `fcsr`, plus `tp`, which carries the hart identity.
// `gp` is link-time constant and re-derived by the entry. Everything else
// is the caller's to lose across a call.
//
// The byte offsets are those of `SavedContext` in `lifecycle.rs`, which
// asserts them.
.equ HELIOS_CONTEXT_RA,    0x00
.equ HELIOS_CONTEXT_SP,    0x08
.equ HELIOS_CONTEXT_TP,    0x10
.equ HELIOS_CONTEXT_S,     0x18
.equ HELIOS_CONTEXT_FS,    0x78
.equ HELIOS_CONTEXT_FCSR,  0xd8

.option push
.option arch, +f, +d

.section .text.helios_hart_suspend, "ax"
.global __helios_hart_suspend
.balign 4
// fn __helios_hart_suspend(context: *mut SavedContext)
//
// Saves the callee-saved state into `context` and continues into
// `helios_hart_stop`, which never returns. The hart comes back to the
// caller's return address through `__helios_hart_resume_return`.
__helios_hart_suspend:
    sd ra, HELIOS_CONTEXT_RA(a0)
    sd sp, HELIOS_CONTEXT_SP(a0)
    sd tp, HELIOS_CONTEXT_TP(a0)
    sd s0,  HELIOS_CONTEXT_S + 0x00(a0)
    sd s1,  HELIOS_CONTEXT_S + 0x08(a0)
    sd s2,  HELIOS_CONTEXT_S + 0x10(a0)
    sd s3,  HELIOS_CONTEXT_S + 0x18(a0)
    sd s4,  HELIOS_CONTEXT_S + 0x20(a0)
    sd s5,  HELIOS_CONTEXT_S + 0x28(a0)
    sd s6,  HELIOS_CONTEXT_S + 0x30(a0)
    sd s7,  HELIOS_CONTEXT_S + 0x38(a0)
    sd s8,  HELIOS_CONTEXT_S + 0x40(a0)
    sd s9,  HELIOS_CONTEXT_S + 0x48(a0)
    sd s10, HELIOS_CONTEXT_S + 0x50(a0)
    sd s11, HELIOS_CONTEXT_S + 0x58(a0)
    fsd fs0,  HELIOS_CONTEXT_FS + 0x00(a0)
    fsd fs1,  HELIOS_CONTEXT_FS + 0x08(a0)
    fsd fs2,  HELIOS_CONTEXT_FS + 0x10(a0)
    fsd fs3,  HELIOS_CONTEXT_FS + 0x18(a0)
    fsd fs4,  HELIOS_CONTEXT_FS + 0x20(a0)
    fsd fs5,  HELIOS_CONTEXT_FS + 0x28(a0)
    fsd fs6,  HELIOS_CONTEXT_FS + 0x30(a0)
    fsd fs7,  HELIOS_CONTEXT_FS + 0x38(a0)
    fsd fs8,  HELIOS_CONTEXT_FS + 0x40(a0)
    fsd fs9,  HELIOS_CONTEXT_FS + 0x48(a0)
    fsd fs10, HELIOS_CONTEXT_FS + 0x50(a0)
    fsd fs11, HELIOS_CONTEXT_FS + 0x58(a0)
    frcsr t0
    sd t0, HELIOS_CONTEXT_FCSR(a0)
    tail helios_hart_stop

.section .text.helios_hart_resume_return, "ax"
.global __helios_hart_resume_return
.balign 4
// fn __helios_hart_resume_return(context: *const SavedContext) -> !
//
// Restores what `__helios_hart_suspend` saved and returns to its caller.
__helios_hart_resume_return:
    ld t0, HELIOS_CONTEXT_FCSR(a0)
    fscsr t0
    fld fs0,  HELIOS_CONTEXT_FS + 0x00(a0)
    fld fs1,  HELIOS_CONTEXT_FS + 0x08(a0)
    fld fs2,  HELIOS_CONTEXT_FS + 0x10(a0)
    fld fs3,  HELIOS_CONTEXT_FS + 0x18(a0)
    fld fs4,  HELIOS_CONTEXT_FS + 0x20(a0)
    fld fs5,  HELIOS_CONTEXT_FS + 0x28(a0)
    fld fs6,  HELIOS_CONTEXT_FS + 0x30(a0)
    fld fs7,  HELIOS_CONTEXT_FS + 0x38(a0)
    fld fs8,  HELIOS_CONTEXT_FS + 0x40(a0)
    fld fs9,  HELIOS_CONTEXT_FS + 0x48(a0)
    fld fs10, HELIOS_CONTEXT_FS + 0x50(a0)
    fld fs11, HELIOS_CONTEXT_FS + 0x58(a0)
    ld s0,  HELIOS_CONTEXT_S + 0x00(a0)
    ld s1,  HELIOS_CONTEXT_S + 0x08(a0)
    ld s2,  HELIOS_CONTEXT_S + 0x10(a0)
    ld s3,  HELIOS_CONTEXT_S + 0x18(a0)
    ld s4,  HELIOS_CONTEXT_S + 0x20(a0)
    ld s5,  HELIOS_CONTEXT_S + 0x28(a0)
    ld s6,  HELIOS_CONTEXT_S + 0x30(a0)
    ld s7,  HELIOS_CONTEXT_S + 0x38(a0)
    ld s8,  HELIOS_CONTEXT_S + 0x40(a0)
    ld s9,  HELIOS_CONTEXT_S + 0x48(a0)
    ld s10, HELIOS_CONTEXT_S + 0x50(a0)
    ld s11, HELIOS_CONTEXT_S + 0x58(a0)
    ld tp, HELIOS_CONTEXT_TP(a0)
    ld sp, HELIOS_CONTEXT_SP(a0)
    ld ra, HELIOS_CONTEXT_RA(a0)
    ret

.option pop
