//! Supervisor trap vectoring for the RISC-V backend.
//!
//! The entry itself is `trap.S`; this module owns the frame type it
//! builds, installs it in `stvec`, and proves at bring-up that the
//! floating-point half of the frame actually round-trips.

use core::arch::{asm, global_asm};

global_asm!(include_str!("trap.S"));
global_asm!(include_str!("fp_selfcheck.S"));

/// Bytes `trap.S` reserves for one [`TrapFrame`].
const TRAP_FRAME_BYTES: usize = 0x220;

/// The integer file as the trap entry lays it out: `x0` through `x31` in
/// architectural order, so the offset of `xN` is `8 * N`.
#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct GeneralRegs {
    pub zero: usize,
    pub ra: usize,
    pub sp: usize,
    pub gp: usize,
    pub tp: usize,
    pub t0: usize,
    pub t1: usize,
    pub t2: usize,
    pub s0: usize,
    pub s1: usize,
    pub a0: usize,
    pub a1: usize,
    pub a2: usize,
    pub a3: usize,
    pub a4: usize,
    pub a5: usize,
    pub a6: usize,
    pub a7: usize,
    pub s2: usize,
    pub s3: usize,
    pub s4: usize,
    pub s5: usize,
    pub s6: usize,
    pub s7: usize,
    pub s8: usize,
    pub s9: usize,
    pub s10: usize,
    pub s11: usize,
    pub t3: usize,
    pub t4: usize,
    pub t5: usize,
    pub t6: usize,
}

/// The complete interrupted supervisor context.
///
/// The floating-point file is part of it because the target is `+f,+d`
/// with the `lp64d` ABI and `sstatus.FS` is enabled: compiled kernel code
/// keeps live values in `f0`-`f31`, and the dispatcher is ordinary Rust
/// that clobbers them. `fcsr` travels with them so a handler that raises
/// an exception flag or changes the rounding mode cannot leak it into the
/// interrupted computation.
///
/// The scheduler copies this whole frame in and out of
/// [`ComputeTaskContext`](crate::ComputeTaskContext), so the floating-point
/// registers are per-task state as well as per-trap state.
///
/// Layout is pinned by `trap.S`; the assertions below keep the two from
/// drifting apart.
#[derive(Debug, Default, Clone, Copy)]
#[repr(C, align(16))]
pub struct TrapFrame {
    /// General registers.
    pub general: GeneralRegs,
    /// Supervisor status.
    pub sstatus: usize,
    /// Supervisor exception program counter.
    pub sepc: usize,
    /// Floating-point control and status.
    ///
    /// Only meaningful when `sstatus.FS` was not `Off`; the entry skips
    /// the floating-point half of the frame entirely when it was.
    pub fcsr: usize,
    /// `f0`-`f31`, as raw bit patterns.
    pub f: [u64; 32],
}

const _: () = {
    assert!(core::mem::size_of::<TrapFrame>() == TRAP_FRAME_BYTES);
    assert!(core::mem::size_of::<GeneralRegs>() == 0x100);
    assert!(core::mem::offset_of!(GeneralRegs, sp) == 0x010);
    assert!(core::mem::offset_of!(TrapFrame, sstatus) == 0x100);
    assert!(core::mem::offset_of!(TrapFrame, sepc) == 0x108);
    assert!(core::mem::offset_of!(TrapFrame, fcsr) == 0x110);
    assert!(core::mem::offset_of!(TrapFrame, f) == 0x118);
};

unsafe extern "C" {
    fn __helios_riscv_trap_entry();
    fn __helios_riscv_fp_trap_selfcheck(seed: usize) -> usize;
}

/// Points `stvec` at this backend's trap entry, in direct mode.
///
/// # Safety
///
/// The caller must not point `stvec` anywhere else afterwards: the entry
/// and [`TrapFrame`] are one contract.
pub(crate) unsafe fn install_trap_vector() {
    // Direct mode is bit 0 of `stvec` clear, which the entry's four-byte
    // alignment guarantees.
    unsafe {
        asm!(
            "csrw stvec, {vector}",
            vector = in(reg) __helios_riscv_trap_entry as *const () as usize,
            options(nomem, nostack),
        );
    }
}

/// Proves on the calling hart that a trap taken inside floating-point code
/// returns with `f0`-`f31` and `fcsr` intact.
///
/// The probe raises a supervisor software interrupt on itself between
/// filling the floating-point file and reading it back, so the window is
/// exact rather than a race against the timer. It has to run after
/// interrupts are enabled and after the hart runtime is installed, because
/// the interrupt it raises goes through the ordinary dispatcher.
///
/// This is the boot-time gate on the defect this entry exists to fix, and
/// its verdict is fatal: a hart whose traps eat floating-point state
/// computes wrong answers quietly, which is far worse than not booting.
pub(crate) fn verify_trap_preserves_fp_state() {
    // A seed with bits set across the whole word, so a register that came
    // back zeroed, sign-extended or NaN-boxed is as visible as one that
    // came back holding a neighbour's value.
    const SEED: usize = 0x5eed_f00d_0bad_c0de;

    // SAFETY: the probe touches only caller-saved integer registers, the
    // floating-point file it saves and restores around itself, and
    // `sip.SSIP`, which the dispatcher already owns.
    let outcome = unsafe { __helios_riscv_fp_trap_selfcheck(SEED) };
    match outcome {
        SELFCHECK_PASS => {}
        SELFCHECK_NO_TRAP => panic!(
            "floating-point trap self-check could not run: the supervisor \
             software interrupt it raised on itself was never delivered"
        ),
        SELFCHECK_FCSR => panic!("supervisor trap entry did not preserve fcsr"),
        register => panic!(
            "supervisor trap entry did not preserve f{}",
            register.wrapping_sub(1)
        ),
    }
}

/// Every floating-point register and `fcsr` came back as it went in.
const SELFCHECK_PASS: usize = 0;
/// `fcsr` came back wrong. `1..=32` name the first `f` register that did,
/// offset by one so that zero can mean "pass".
const SELFCHECK_FCSR: usize = 33;
/// The probe's own interrupt was never delivered, so it tested nothing.
const SELFCHECK_NO_TRAP: usize = 34;
