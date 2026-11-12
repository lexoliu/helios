//! The hand-over of a synchronous exception the kernel did not resolve
//! to the runtime's trap handler.
//!
//! The runtime ends a trap it claims by writing `sp` and `fp` and
//! branching to its resume point (`resume_to_exception_handler` in the
//! Wasmtime fork's unwinder); nothing of the backend runs after that
//! branch. Whatever the synchronous epilogue in `entry.S` would have
//! restored therefore has to be in place before the handler starts:
//! `SP_EL1` selected, so the `sp` write lands where the continuation's
//! stack lives, `SP_EL0` back at the exception-stack top, so the next
//! exception builds its frame there, and the interrupted context's IRQ
//! mask, because everything the runtime's resume lands in — the store's
//! teardown, its fiber stack's release, the TLB shootdown that release
//! broadcasts — is the interrupted code's continuation and runs on that
//! code's terms. With `SP_EL0` back at the top the exception stack
//! belongs to the next exception, so the handler cannot run on it
//! either: the hand-over copies what it needs to this processor's
//! hand-over stack, switches to it, puts the entry state back and calls
//! the handler from there, as one step taken with IRQs masked
//! (`__helios_aarch64_trap_hand_over`).
//!
//! # Concurrency contract
//!
//! Everything here is per processor and touched only by the processor
//! it belongs to. One hand-over runs on a processor at a time: it ends
//! in the runtime's branch or in a panic, nothing switches away from it
//! while it runs, and an exception taken during it is never handed over
//! again ([`HandOverRefusal::InsideHandOver`]), so the hand-over stack
//! has one user. The probe's words are written by the owning
//! processor's own code and by its own exception and IRQ entries; the
//! atomics exist for `Sync`, not for ordering between processors, and
//! every access is `Relaxed`.

use core::arch::{asm, global_asm};
use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

use helios_kernel::{KernelException, KernelExceptionCause, StackFault};

use crate::{
    IRQ_FRAME_BYTES, SPSR_IRQ_MASK, SYNC_FRAME_BYTES, SyncTrapFrame, current_processor_runtime,
    gic, installed_processor_runtime, read_counter, read_daif, timebase,
};

/// `SPSR_EL1.M[3:0]`, the mode the exception was taken from.
const SPSR_MODE_MASK: u64 = 0xf;
/// EL1 on `SP_EL1`: kernel code on its own stack, the only mode a trap
/// the runtime can claim is taken from.
const SPSR_MODE_EL1H: u64 = 0b0101;
/// EL1 on `SP_EL0`: an exception or IRQ handler on the exception stack.
const SPSR_MODE_EL1T: u64 = 0b0100;
/// Bytes of one A64 instruction, which is what a resolved `brk` steps
/// past.
const A64_INSTRUCTION_BYTES: u64 = 4;
/// `brk` immediates of the self-check's two traps. Arbitrary: the probe
/// recognises its traps by address, and the immediates only make a
/// disassembly or an `ESR_EL1` dump readable.
const PROBE_CLAIM_BRK: u16 = 0x0674;
const PROBE_RETURN_BRK: u16 = 0x0675;
/// Bytes `__helios_aarch64_hand_over_probe_claimed` reserves: `x19`-`x30`,
/// `d8`-`d15` and the probe pointer, rounded up to the 16 bytes SP keeps.
const PROBE_SAVE_BYTES: usize = 0xb0;
/// The value [`HandOverProbe::irq_frame`] holds while the self-check
/// waits for its interrupt; no frame lives at the top of the address
/// space.
const PROBE_IRQ_ARMED: usize = usize::MAX;

unsafe extern "C" {
    /// Switches to the hand-over stack, restores the entry state and calls
    /// [`aarch64_trap_hand_over_main`] with `record`.
    ///
    /// # Safety
    ///
    /// Must be called from the synchronous handler, on `SP_EL0` with IRQs
    /// masked. `record` must be a [`TrapHandOver`] written 16-byte aligned
    /// at the top of this processor's hand-over stack, and
    /// `exception_stack_top` this processor's exception-stack top.
    fn __helios_aarch64_trap_hand_over(
        record: usize,
        exception_stack_top: usize,
        unmask_irq: usize,
    ) -> !;
    /// Raises the probe's claimed trap; returns from the landing the
    /// claim resumes into. `probe` is a [`HandOverProbe`]'s address.
    fn __helios_aarch64_hand_over_probe_claimed(probe: usize);
    /// Raises the probe's returning trap. `probe` is a
    /// [`HandOverProbe`]'s address.
    fn __helios_aarch64_hand_over_probe_returning(probe: usize);
}

#[cfg(target_os = "none")]
global_asm!(
    include_str!("hand_over_probe.S"),
    save_bytes = const PROBE_SAVE_BYTES,
    claim_brk = const PROBE_CLAIM_BRK,
    return_brk = const PROBE_RETURN_BRK,
    claim_pc = const core::mem::offset_of!(HandOverProbe, claim_pc),
    resume_pc = const core::mem::offset_of!(HandOverProbe, resume_pc),
    resume_sp = const core::mem::offset_of!(HandOverProbe, resume_sp),
    resume_fp = const core::mem::offset_of!(HandOverProbe, resume_fp),
    resumed_spsel = const core::mem::offset_of!(HandOverProbe, resumed_spsel),
    resumed_sp_el0 = const core::mem::offset_of!(HandOverProbe, resumed_sp_el0),
    resumed_daif = const core::mem::offset_of!(HandOverProbe, resumed_daif),
    return_pc = const core::mem::offset_of!(HandOverProbe, return_pc),
    return_sp = const core::mem::offset_of!(HandOverProbe, return_sp),
);

/// What the hand-over needs once the exception stack is gone: the
/// exception for the runtime, and what the fatal report says when the
/// runtime declines it.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub(crate) struct TrapHandOver {
    exception: KernelException,
    esr: usize,
    far: usize,
    stack_fault: StackFault,
    /// Where the entry built the frame, and the `SP_EL1` it recorded.
    frame: usize,
    interrupted_sp: usize,
}

impl TrapHandOver {
    pub(crate) fn new(
        exception: KernelException,
        esr: usize,
        far: usize,
        stack_fault: StackFault,
        frame: &SyncTrapFrame,
    ) -> Self {
        Self {
            exception,
            esr,
            far,
            stack_fault,
            frame: core::ptr::from_ref(frame) as usize,
            interrupted_sp: frame.sp as usize,
        }
    }
}

/// Why an exception the kernel did not resolve stays with the backend
/// and is reported where it was taken.
#[derive(Clone, Copy, Debug)]
pub(crate) enum HandOverRefusal {
    /// The processor has no runtime yet, so it has no hand-over stack and
    /// the runtime has not run anything on it.
    NoProcessorRuntime,
    /// The exception interrupted code on the exception stack: a fault
    /// inside an exception or IRQ handler, never a guest trap, and
    /// handing it over would abandon the handler it interrupted.
    InsideExceptionHandler,
    /// The exception interrupted the hand-over itself: the runtime's
    /// handler, or the report of a trap it declined.
    InsideHandOver,
    /// `SPSR_EL1.M` names a mode the kernel never runs in.
    UnexpectedMode(u64),
}

impl fmt::Display for HandOverRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProcessorRuntime => f.write_str("this processor has no runtime yet"),
            Self::InsideExceptionHandler => {
                f.write_str("it interrupted a handler running on the exception stack")
            }
            Self::InsideHandOver => f.write_str("it interrupted a trap hand-over in progress"),
            Self::UnexpectedMode(mode) => {
                write!(f, "it was taken from SPSR_EL1.M {mode:#06b}, not EL1h")
            }
        }
    }
}

/// Hands `record` to the runtime from this processor's hand-over stack.
///
/// Diverges for every exception it hands over; returns the reason only
/// when the exception must not leave the exception stack.
pub(crate) fn hand_over_trap(record: TrapHandOver, spsr: u64) -> HandOverRefusal {
    let Some(runtime) = installed_processor_runtime() else {
        return HandOverRefusal::NoProcessorRuntime;
    };
    match spsr & SPSR_MODE_MASK {
        SPSR_MODE_EL1H => {}
        SPSR_MODE_EL1T => return HandOverRefusal::InsideExceptionHandler,
        mode => return HandOverRefusal::UnexpectedMode(mode),
    }
    let hand_over_stack = runtime.hand_over_stack.range();
    if hand_over_stack.contains(&record.interrupted_sp) {
        return HandOverRefusal::InsideHandOver;
    }
    // `TrapHandOver` is 16-byte aligned, so its size is a multiple of 16
    // and the slot below the 16-byte aligned top keeps SP aligned.
    let slot = hand_over_stack.end - core::mem::size_of::<TrapHandOver>();
    // SAFETY: the hand-over stack belongs to this processor and nothing
    // runs on it: the refusals above rule out an exception taken from a
    // hand-over in progress, and none is suspended. The switch is entered
    // from this handler, on `SP_EL0` with IRQs masked, with the slot the
    // record was just written to and this processor's exception stack.
    unsafe {
        (slot as *mut TrapHandOver).write(record);
        __helios_aarch64_trap_hand_over(
            slot,
            runtime.exception_stack.range().end,
            usize::from(spsr & SPSR_IRQ_MASK == 0),
        )
    }
}

/// The hand-over's Rust half, on the hand-over stack in the state the
/// synchronous epilogue would have restored.
#[unsafe(no_mangle)]
extern "C" fn aarch64_trap_hand_over_main(record: usize) -> ! {
    // SAFETY: `hand_over_trap` wrote a `TrapHandOver` at this address,
    // above this frame, and nothing else writes the hand-over stack.
    let record = unsafe { (record as *const TrapHandOver).read() };
    current_processor_runtime().hand_over_probe.claim(&record);
    let _ = helios_kernel::dispatch_native_trap(record.exception);
    panic!(
        "unhandled AArch64 synchronous exception ec={:#x} esr={:#x} elr={:#x} far={:#x}{}",
        (record.esr >> 26) & 0x3f,
        record.esr,
        record.exception.instruction_pointer,
        record.far,
        record.stack_fault,
    )
}

/// The record of the boot-time self-check that proves the hand-over on
/// this processor: a trap claimed the way the runtime claims one, then an
/// interrupt and a second trap, each checked for where its frame landed
/// and what interrupted SP it recorded.
///
/// Layout is pinned by `hand_over_probe.S` through the `offset_of!`
/// operands above.
#[repr(C)]
pub(crate) struct HandOverProbe {
    /// The address of the claimed `brk` while the probe waits for it,
    /// zero otherwise.
    claim_pc: AtomicUsize,
    /// Where the claim resumes, as the runtime's handler would.
    resume_pc: AtomicUsize,
    resume_sp: AtomicUsize,
    resume_fp: AtomicUsize,
    /// What the resumed code found: `SPSel`, `SP_EL0` and `DAIF`.
    resumed_spsel: AtomicUsize,
    resumed_sp_el0: AtomicUsize,
    resumed_daif: AtomicUsize,
    /// The claimed trap's frame address and the SP it recorded.
    claimed_frame: AtomicUsize,
    claimed_frame_sp: AtomicUsize,
    /// The SP the hand-over ran its Rust half on.
    hand_over_sp: AtomicUsize,
    /// [`PROBE_IRQ_ARMED`] while the probe waits for an interrupt, then
    /// the frame address the IRQ entry built.
    irq_frame: AtomicUsize,
    /// The address of the returning `brk` while the probe waits for it,
    /// zero otherwise.
    return_pc: AtomicUsize,
    /// The SP the returning `brk` ran on.
    return_sp: AtomicUsize,
    /// The returning trap's frame address and the SP it recorded.
    returned_frame: AtomicUsize,
    returned_frame_sp: AtomicUsize,
}

impl HandOverProbe {
    pub(crate) const fn new() -> Self {
        Self {
            claim_pc: AtomicUsize::new(0),
            resume_pc: AtomicUsize::new(0),
            resume_sp: AtomicUsize::new(0),
            resume_fp: AtomicUsize::new(0),
            resumed_spsel: AtomicUsize::new(0),
            resumed_sp_el0: AtomicUsize::new(0),
            resumed_daif: AtomicUsize::new(0),
            claimed_frame: AtomicUsize::new(0),
            claimed_frame_sp: AtomicUsize::new(0),
            hand_over_sp: AtomicUsize::new(0),
            irq_frame: AtomicUsize::new(0),
            return_pc: AtomicUsize::new(0),
            return_sp: AtomicUsize::new(0),
            returned_frame: AtomicUsize::new(0),
            returned_frame_sp: AtomicUsize::new(0),
        }
    }

    /// Claims `record` when it is the probe's trap, resuming exactly as
    /// the runtime's handler does; returns for every other exception.
    fn claim(&self, record: &TrapHandOver) {
        let armed = self.claim_pc.load(Ordering::Relaxed);
        if armed == 0
            || armed != record.exception.instruction_pointer
            || record.exception.cause != KernelExceptionCause::Breakpoint
        {
            return;
        }
        self.claim_pc.store(0, Ordering::Relaxed);
        self.claimed_frame.store(record.frame, Ordering::Relaxed);
        self.claimed_frame_sp
            .store(record.interrupted_sp, Ordering::Relaxed);
        self.hand_over_sp
            .store(current_stack_pointer(), Ordering::Relaxed);
        // SAFETY: the probe recorded a landing in its own routine, with the
        // SP and FP of that routine's frame, which is live: the routine is
        // waiting on the `brk` this claims.
        unsafe {
            resume_as_the_runtime_does(
                self.resume_pc.load(Ordering::Relaxed),
                self.resume_sp.load(Ordering::Relaxed),
                self.resume_fp.load(Ordering::Relaxed),
            )
        }
    }

    /// Records the frame of the interrupt the probe is waiting for.
    pub(crate) fn record_interrupt(&self, frame: usize) {
        let _ = self.irq_frame.compare_exchange(
            PROBE_IRQ_ARMED,
            frame,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// Resolves the probe's returning `brk` in place: records the frame,
    /// steps past the `brk`, and tells the handler to return. `false` for
    /// every other `brk`.
    pub(crate) fn resolve_returning_trap(&self, frame: &mut SyncTrapFrame) -> bool {
        let armed = self.return_pc.load(Ordering::Relaxed);
        if armed == 0 || armed != frame.elr as usize {
            return false;
        }
        self.return_pc.store(0, Ordering::Relaxed);
        self.returned_frame
            .store(core::ptr::from_ref(frame) as usize, Ordering::Relaxed);
        self.returned_frame_sp
            .store(frame.sp as usize, Ordering::Relaxed);
        frame.elr += A64_INSTRUCTION_BYTES;
        true
    }
}

/// The runtime's resume, instruction for instruction: the AArch64
/// `resume_to_exception_handler` of the Wasmtime fork's unwinder, with
/// both payloads zero as the fork's trap handler passes them.
///
/// # Safety
///
/// `sp` and `fp` must belong to a live frame that `pc` expects to resume
/// into with no other register restored.
unsafe fn resume_as_the_runtime_does(pc: usize, sp: usize, fp: usize) -> ! {
    unsafe {
        asm!(
            "mov sp, x2",
            "mov fp, x3",
            "br x4",
            in("x0") 0usize,
            in("x1") 0usize,
            in("x2") sp,
            in("x3") fp,
            in("x4") pc,
            options(nostack, nomem, noreturn),
        );
    }
}

fn current_stack_pointer() -> usize {
    let sp: usize;
    // SAFETY: reads the stack pointer and nothing else.
    unsafe {
        asm!("mov {sp}, sp", sp = out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    sp
}

/// Proves on the calling processor that a trap the runtime claims leaves
/// it as an exception return would.
///
/// The probe raises a `brk` and claims it from the hand-over exactly as
/// the runtime claims a wasm trap, then takes an interrupt and a second
/// trap on the same processor. Each must build its frame at the top of
/// this processor's exception stack, and each trap must record the SP its
/// code was really running on; after the claim the processor must have
/// `SP_EL1` selected, `SP_EL0` at the exception-stack top and IRQs
/// unmasked as they were. A processor that fails any of these corrupts
/// whatever stack its next exception lands on, so the verdict is fatal,
/// like the other bring-up self-checks.
///
/// Runs once per processor, with IRQs unmasked and the wake SGI enabled
/// on its GIC CPU interface; `mpidr` is the calling processor's.
pub(crate) fn verify_trap_hand_over(mpidr: u64) {
    let runtime = current_processor_runtime();
    let probe = &runtime.hand_over_probe;
    let exception_stack = runtime.exception_stack.range();
    let hand_over_stack = runtime.hand_over_stack.range();
    let sync_frame = exception_stack.end - SYNC_FRAME_BYTES;
    let processor = runtime.logical_id;
    let probe_address = core::ptr::from_ref(probe) as usize;
    assert!(
        read_daif() & SPSR_IRQ_MASK == 0,
        "AArch64 processor {processor} ran the trap hand-over self-check with IRQs masked; \
         it cannot take the interrupt it checks"
    );

    // SAFETY: the routine writes only its own frame and `probe`, and
    // saves the callee-saved registers the claim does not restore.
    unsafe { __helios_aarch64_hand_over_probe_claimed(probe_address) };
    let claimed_frame = probe.claimed_frame.load(Ordering::Relaxed);
    let claimed_frame_sp = probe.claimed_frame_sp.load(Ordering::Relaxed);
    let resume_sp = probe.resume_sp.load(Ordering::Relaxed);
    assert!(
        claimed_frame == sync_frame && claimed_frame_sp == resume_sp,
        "AArch64 processor {processor} built the claimed trap's frame at {claimed_frame:#x} \
         recording SP {claimed_frame_sp:#x}; expected {sync_frame:#x} recording {resume_sp:#x}"
    );
    let hand_over_sp = probe.hand_over_sp.load(Ordering::Relaxed);
    assert!(
        hand_over_stack.contains(&hand_over_sp),
        "AArch64 processor {processor} ran the runtime's trap handler at SP {hand_over_sp:#x}, \
         outside its hand-over stack {:#x}..{:#x}",
        hand_over_stack.start,
        hand_over_stack.end
    );
    let spsel = probe.resumed_spsel.load(Ordering::Relaxed);
    assert!(
        spsel == 1,
        "AArch64 processor {processor} resumed a claimed trap with SPSel {spsel}: the runtime's \
         SP write landed in SP_EL0 and the processor now runs in EL1t"
    );
    let sp_el0 = probe.resumed_sp_el0.load(Ordering::Relaxed);
    assert!(
        sp_el0 == exception_stack.end,
        "AArch64 processor {processor} resumed a claimed trap with SP_EL0 {sp_el0:#x}, not the \
         exception-stack top {:#x}",
        exception_stack.end
    );
    let daif = probe.resumed_daif.load(Ordering::Relaxed) as u64;
    assert!(
        daif & SPSR_IRQ_MASK == 0,
        "AArch64 processor {processor} resumed a claimed trap with DAIF {daif:#x}: the \
         interrupted context's IRQ mask was not restored"
    );

    probe.irq_frame.store(PROBE_IRQ_ARMED, Ordering::Relaxed);
    gic::send_wake(mpidr);
    // An SGI a processor sends itself is pending within the GIC's
    // propagation delay, microseconds even under emulation; a second of
    // the generic timer only bounds a lost one.
    let deadline = read_counter().saturating_add(timebase().frequency());
    while probe.irq_frame.load(Ordering::Relaxed) == PROBE_IRQ_ARMED {
        assert!(
            read_counter() < deadline,
            "AArch64 processor {processor} never took the wake SGI it sent itself for the trap \
             hand-over self-check"
        );
        core::hint::spin_loop();
    }
    let irq_frame = probe.irq_frame.load(Ordering::Relaxed);
    let expected_irq_frame = exception_stack.end - IRQ_FRAME_BYTES;
    assert!(
        irq_frame == expected_irq_frame,
        "AArch64 processor {processor} built an interrupt frame at {irq_frame:#x} after a \
         claimed trap; expected {expected_irq_frame:#x} at the exception-stack top"
    );

    // SAFETY: the routine writes only `probe`; the trap it raises is
    // resolved in place and returns with every register restored.
    unsafe { __helios_aarch64_hand_over_probe_returning(probe_address) };
    let returned_frame = probe.returned_frame.load(Ordering::Relaxed);
    let returned_frame_sp = probe.returned_frame_sp.load(Ordering::Relaxed);
    let return_sp = probe.return_sp.load(Ordering::Relaxed);
    assert!(
        returned_frame == sync_frame && returned_frame_sp == return_sp,
        "AArch64 processor {processor} built the second trap's frame at {returned_frame:#x} \
         recording SP {returned_frame_sp:#x}; expected {sync_frame:#x} recording {return_sp:#x}"
    );
    tracing::info!(
        target: "helios_aarch64::hand_over",
        processor,
        "claimed trap resumed on SP_EL1 with the exception stack free"
    );
}
