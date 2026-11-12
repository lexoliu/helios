//! Return-address signing on every processor (Arm FEAT_PAuth, key A).
//!
//! The image is compiled with `-Zbranch-protection=pac-ret`
//! (`.cargo/config.toml`, checked by `build.rs`), so every function that saves
//! `LR` signs it with `paciasp` and authenticates it with `autiasp`. Both are
//! HINT encodings that do nothing until `SCTLR_EL1.EnIA` is set. This module
//! gives the machine one instruction key, installs it in each processor's
//! `APIAKey` registers and sets `EnIA` on that processor.
//!
//! # Concurrency contract
//!
//! * The capability ([`ReturnAddressSigning`]) is read from the bootstrap
//!   processor's ID registers before any other processor starts, stored in the
//!   platform state and never written again. Any processor may read it, in
//!   any context.
//! * The key is drawn once, on the bootstrap processor, and published to a
//!   [`SharedInstructionKey`] before the first secondary starts. Secondaries
//!   start one at a time (`Aarch64PlatformState::start_processor` returns when
//!   the started processor has set its `started` flag, which follows its read
//!   of the key), so there is at most one reader at any time. The bootstrap
//!   processor wipes the words after the last one returned.
//! * `APIAKey` and `SCTLR_EL1` are per-processor registers. Each processor
//!   writes its own, once, with interrupts masked, and never again: suspended
//!   fibers and every live kernel stack hold return addresses signed with the
//!   key, so it cannot change for the life of the machine.
//! * Nothing here takes a lock, sends an IPI or is reachable from an interrupt
//!   handler. Fault classification runs on the faulting processor and reads
//!   `ESR_EL1`, `FAR_EL1`, `ELR_EL1` and this processor's ID registers.
//!
//! # What runs before the enable
//!
//! A frame that is live across [`enable_instruction_key_a`] has `LR` saved
//! raw (its `paciasp` ran with `EnIA` clear) and would fail `autiasp` after
//! it. So the enable is called only from the two entry functions,
//! `aarch64_kernel_main` and `aarch64_secondary_main`, which never return,
//! and every function either calls before the enable has returned.

use core::sync::atomic::{AtomicU64, Ordering};

use arm_sysregs::el1::accessors::{
    read_id_aa64isar1_el1, read_id_aa64isar2_el1, write_apiakeyhi_el1, write_apiakeylo_el1,
};
use arm_sysregs::el1::registers::{
    ApiakeyhiEl1, ApiakeyloEl1, IdAa64isar1El1, IdAa64isar2El1, SctlrEl1,
};
use helios_hal::cpu::ReturnAddressSigning;
use helios_kernel::RootEntropy;
use zeroize::Zeroize;

/// `ID_AA64ISAR1_EL1.{APA,API}` and `ID_AA64ISAR2_EL1.APA3` read `0b0100`
/// when the algorithm is implemented with FEAT_FPAC, and more with
/// FEAT_FPACCOMBINE (`0b0101`) and FEAT_PAuth_LR (`0b0110`)
/// (Linux `arch/arm64/tools/sysreg`, enums `APA`, `API`, `APA3`).
const FIELD_FPAC: u8 = 0b0100;

/// `ESR_ELx.EC` is bits `[31:26]`.
const ESR_EC_SHIFT: usize = 26;
const ESR_EC_MASK: usize = 0x3f;
/// `ESR_ELx.EC` of a failed FEAT_FPAC authentication (`ESR_ELx_EC_FPAC` in
/// Linux `arch/arm64/include/asm/esr.h`).
const ESR_EC_FPAC: usize = 0x1c;
/// `ESR_ELx.EC` of an instruction abort from a lower and from the current
/// exception level.
const ESR_EC_INSTRUCTION_ABORT_LOWER: usize = 0x20;
const ESR_EC_INSTRUCTION_ABORT_SAME: usize = 0x21;

/// Bytes of the 128-bit `APIAKey`.
const INSTRUCTION_KEY_BYTES: usize = 16;

/// This processor's address-authentication ID fields.
struct IdRegisters {
    isar1: IdAa64isar1El1,
    isar2: IdAa64isar2El1,
}

impl IdRegisters {
    fn read() -> Self {
        Self {
            isar1: read_id_aa64isar1_el1(),
            isar2: read_id_aa64isar2_el1(),
        }
    }

    /// What this processor offers, or `None` without address authentication
    /// (APA, API and APA3 all zero).
    ///
    /// When more than one algorithm is implemented the weakest decides
    /// whether failure faults at authentication, so the claim holds whichever
    /// the processor uses.
    fn signing(&self) -> Option<ReturnAddressSigning> {
        let weakest = [self.isar1.apa(), self.isar1.api(), self.isar2.apa3()]
            .into_iter()
            .filter(|&field| field != 0)
            .min()?;
        Some(ReturnAddressSigning {
            faults_at_authentication: weakest >= FIELD_FPAC,
        })
    }
}

/// The signing capability of the bootstrap processor.
///
/// An image compiled with `-Zbranch-protection=pac-ret` and run without
/// address authentication would sign nothing, so the machine does not boot.
pub(crate) fn bootstrap_capability() -> ReturnAddressSigning {
    let ids = IdRegisters::read();
    ids.signing().unwrap_or_else(|| {
        panic!(
            "AArch64 bootstrap processor lacks FEAT_PAuth, which this image's return-address \
             signing requires (ID_AA64ISAR1_EL1={:#x}, ID_AA64ISAR2_EL1={:#x})",
            ids.isar1.bits(),
            ids.isar2.bits(),
        )
    })
}

/// The machine's 128-bit instruction key, as the two `APIAKey` registers.
struct InstructionKey {
    lo: u64,
    hi: u64,
}

impl Zeroize for InstructionKey {
    fn zeroize(&mut self) {
        self.lo.zeroize();
        self.hi.zeroize();
    }
}

impl InstructionKey {
    /// Draws a key from the root DRBG; the bytes it came from are wiped.
    fn draw(root_entropy: &RootEntropy) -> Self {
        let mut bytes = [0_u8; INSTRUCTION_KEY_BYTES];
        root_entropy.fill(&mut bytes);
        let [lo, hi] = [0, 8]
            .map(|offset| u64::from_le_bytes(core::array::from_fn(|index| bytes[offset + index])));
        bytes.zeroize();
        Self { lo, hi }
    }

    /// Writes the key into this processor's `APIAKey` registers.
    ///
    /// # Safety
    ///
    /// `SCTLR_EL1.EnIA` is still clear on this processor: nothing has
    /// authenticated against another key, and no return address signed with
    /// one is live.
    unsafe fn install(&self) {
        // SAFETY: `APIAKey_EL1` takes any value, and the caller guarantees
        // that nothing is signed under another one.
        unsafe {
            write_apiakeylo_el1(ApiakeyloEl1::from_bits_retain(self.lo));
            write_apiakeyhi_el1(ApiakeyhiEl1::from_bits_retain(self.hi));
        }
    }
}

/// The key between the bootstrap processor, which publishes it, and the
/// secondaries, which each read it once during bring-up.
///
/// The bootstrap processor writes both words before the first secondary
/// starts (`Release`) and zeroes them after the last one has started. Each
/// secondary reads them (`Acquire`) before it sets its own `started` flag, so
/// the bootstrap processor's wipe cannot overlap a read. The words are off
/// every hot path and are not padded.
pub(crate) struct SharedInstructionKey {
    lo: AtomicU64,
    hi: AtomicU64,
}

impl SharedInstructionKey {
    pub(crate) const fn new() -> Self {
        Self {
            lo: AtomicU64::new(0),
            hi: AtomicU64::new(0),
        }
    }

    fn publish(&self, key: &InstructionKey) {
        self.lo.store(key.lo, Ordering::Release);
        self.hi.store(key.hi, Ordering::Release);
    }

    fn load(&self) -> InstructionKey {
        InstructionKey {
            lo: self.lo.load(Ordering::Acquire),
            hi: self.hi.load(Ordering::Acquire),
        }
    }

    /// Zeroes the shared copy. After this the key exists only in each
    /// processor's `APIAKey` registers; copies the compiler spilled into
    /// temporaries are not covered.
    pub(crate) fn wipe(&self) {
        self.lo.store(0, Ordering::Release);
        self.hi.store(0, Ordering::Release);
    }
}

/// Draws the machine's key on the bootstrap processor, publishes it for the
/// secondaries and installs it in this processor's registers.
///
/// # Safety
///
/// `SCTLR_EL1.EnIA` is still clear on this processor, and no secondary has
/// started.
pub(crate) unsafe fn install_bootstrap_key(shared: &SharedInstructionKey, root: &RootEntropy) {
    let mut key = InstructionKey::draw(root);
    shared.publish(&key);
    // SAFETY: the caller guarantees `EnIA` is clear.
    unsafe { key.install() };
    key.zeroize();
}

/// Checks that a secondary processor offers what the bootstrap processor
/// enabled and installs the machine's key in its registers.
///
/// A processor that lacks it must not join: its returns would not
/// authenticate, or would not fault where the bootstrap processor's do.
///
/// # Safety
///
/// `SCTLR_EL1.EnIA` is still clear on this processor, and the bootstrap
/// processor published the key before it started this one.
pub(crate) unsafe fn install_secondary_key(
    required: ReturnAddressSigning,
    shared: &SharedInstructionKey,
    mpidr: u64,
) {
    let ids = IdRegisters::read();
    let own = ids.signing();
    assert!(
        own.is_some(),
        "AArch64 processor MPIDR {mpidr:#x} lacks FEAT_PAuth that the bootstrap processor enabled \
         (ID_AA64ISAR1_EL1={:#x}, ID_AA64ISAR2_EL1={:#x})",
        ids.isar1.bits(),
        ids.isar2.bits(),
    );
    assert!(
        own.is_some_and(|own| own.faults_at_authentication || !required.faults_at_authentication),
        "AArch64 processor MPIDR {mpidr:#x} lacks FEAT_FPAC that the bootstrap processor reported \
         (ID_AA64ISAR1_EL1={:#x}, ID_AA64ISAR2_EL1={:#x})",
        ids.isar1.bits(),
        ids.isar2.bits(),
    );
    let mut key = shared.load();
    // SAFETY: the caller guarantees `EnIA` is clear.
    unsafe { key.install() };
    key.zeroize();
}

/// Turns on pointer authentication with key A for this processor's
/// instructions, and turns the other three enables off.
///
/// Limine's `SCTLR_EL1` is not established, and the claim depends on exactly
/// one key being enabled, so `EnIB`, `EnDA` and `EnDB` are cleared. `BT0` and
/// `BT1` are left alone.
///
/// This function is naked, so it has no prologue: a normal function would run
/// `paciasp` with `EnIA` clear and `autiasp` with it set, and fault. It
/// clobbers `x0`.
///
/// # Safety
///
/// The caller never returns to a frame that was entered before this call
/// (those saved `LR` unsigned), so it is one of the two `-> !` entry
/// functions, and the key is installed in this processor's registers.
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn enable_instruction_key_a() {
    core::arch::naked_asm!(
        "mrs x0, sctlr_el1",
        "orr x0, x0, #{enia}",
        "bic x0, x0, #{enib}",
        "bic x0, x0, #{enda}",
        "bic x0, x0, #{endb}",
        "msr sctlr_el1, x0",
        "isb",
        "ret",
        enia = const SctlrEl1::ENIA.bits(),
        enib = const SctlrEl1::ENIB.bits(),
        enda = const SctlrEl1::ENDA.bits(),
        endb = const SctlrEl1::ENDB.bits(),
    )
}

/// Whether this processor's failed authentication raises an exception at the
/// authenticating instruction (FEAT_FPAC).
fn faults_at_authentication() -> bool {
    IdRegisters::read()
        .signing()
        .is_some_and(|signing| signing.faults_at_authentication)
}

/// `pointer` with its pointer authentication code removed.
///
/// `XPACLRI` is a HINT, a no-op without FEAT_PAuth, and takes and returns
/// `LR`, which the Wasmtime fork's unwinder uses the same way
/// (`crates/unwinder/src/arch/aarch64.rs`, `get_next_older_pc_from_fp`).
fn strip_pointer_authentication_code(pointer: usize) -> usize {
    let mut pointer = pointer as u64;
    // SAFETY: the sequence only moves `pointer` through `LR`, which it
    // declares as clobbered.
    unsafe {
        core::arch::asm!(
            "mov lr, {pointer}",
            "xpaclri",
            "mov {pointer}, lr",
            pointer = inout(reg) pointer,
            out("lr") _,
            options(nomem, nostack, preserves_flags),
        );
    }
    pointer as usize
}

/// Panics when a synchronous exception is a failed return-address
/// authentication; returns otherwise.
///
/// A guest cannot form a native return address, so a failure means a
/// corrupted host stack or a compiler bug, and the exception is never handed
/// to the runtime.
///
/// * With FEAT_FPAC the authenticating instruction itself raises the
///   exception (`ESR_ELx.EC` `0x1c`).
/// * Without it `autiasp` leaves a poisoned `LR`, and the `ret` through it
///   takes an instruction abort at an address whose authentication code does
///   not strip away.
pub(crate) fn refuse_authentication_failure(esr_el1: usize, far_el1: usize, elr_el1: u64) {
    match (esr_el1 >> ESR_EC_SHIFT) & ESR_EC_MASK {
        ESR_EC_FPAC => panic!(
            "control-flow violation: pointer authentication failed at {elr_el1:#x} \
             esr={esr_el1:#x}"
        ),
        ESR_EC_INSTRUCTION_ABORT_LOWER | ESR_EC_INSTRUCTION_ABORT_SAME
            if !faults_at_authentication() =>
        {
            let stripped = strip_pointer_authentication_code(far_el1);
            if stripped != far_el1 {
                panic!(
                    "control-flow violation: return to {far_el1:#x} (stripped {stripped:#x}) \
                     failed pointer authentication"
                );
            }
        }
        _ => {}
    }
}
