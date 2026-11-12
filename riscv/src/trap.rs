//! Supervisor trap vectoring for the RISC-V backend.
//!
//! The entry itself is `trap.S`; this module owns the frame type it
//! builds, installs it in `stvec`, and proves at bring-up that the
//! floating-point half of the frame actually round-trips.

use alloc::alloc::{Layout, alloc_zeroed};
use core::arch::{asm, global_asm};
use core::fmt;
use core::mem::offset_of;
use core::ops::Range;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use helios_kernel::{KernelException, KernelExceptionCause, StackFault};
use riscv::interrupt::supervisor::Exception;

global_asm!(include_str!("trap.S"));
global_asm!(include_str!("fp_selfcheck.S"));
global_asm!(
    include_str!("hand_over_probe.S"),
    claim_pc = const offset_of!(HandOverProbe, claim_pc),
    resume_pc = const offset_of!(HandOverProbe, resume_pc),
    resume_sp = const offset_of!(HandOverProbe, resume_sp),
    resume_fp = const offset_of!(HandOverProbe, resume_fp),
    resumed_sscratch = const offset_of!(HandOverProbe, resumed_sscratch),
    resumed_sstatus = const offset_of!(HandOverProbe, resumed_sstatus),
    interrupt_sp = const offset_of!(HandOverProbe, interrupt_sp),
    interrupt_frame = const offset_of!(HandOverProbe, interrupt_frame),
    return_pc = const offset_of!(HandOverProbe, return_pc),
    return_sp = const offset_of!(HandOverProbe, return_sp),
);

/// Bytes `trap.S` reserves for one [`TrapFrame`].
const TRAP_FRAME_BYTES: usize = 0x220;
/// What the entry carves per trap: the frame plus the `sscratch` slot
/// above it, kept outside [`TrapFrame`] so a whole-frame replacement
/// cannot touch it.
const TRAP_REGION_BYTES: usize = TRAP_FRAME_BYTES + 16;
/// One hart's trap stack. Every trap on the hart, nested ones included,
/// builds its region here; the runtime's trap handler and a panic's
/// formatting run on it as well, so it is sized like the aarch64 and x86
/// exception stacks.
pub(crate) const TRAP_STACK_BYTES: usize = 64 * 1024;

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
    assert!(TRAP_REGION_BYTES == 0x230);
    assert!(TRAP_STACK_BYTES.is_multiple_of(16));
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
    /// Switches to the hand-over stack, hands the trap stack back and
    /// calls `__helios_riscv_trap_hand_over_main` with `record`.
    ///
    /// # Safety
    ///
    /// Must be called from the dispatcher of an outermost frame, with
    /// `sscratch` zero and `SIE` clear. `record` must be a
    /// [`TrapHandOver`] written 16-byte aligned at the top of this hart's
    /// hand-over stack, and `trap_stack_top` this hart's trap stack top.
    fn __helios_riscv_trap_hand_over(record: usize, trap_stack_top: usize, set_sie: usize) -> !;
    /// Raises the probe's claimed trap; returns from the landing the
    /// claim resumes into. `probe` is a [`HandOverProbe`]'s address.
    fn __helios_riscv_hand_over_probe_claimed(probe: usize);
    /// Raises the probe's interrupt and returning trap. `probe` is a
    /// [`HandOverProbe`]'s address.
    fn __helios_riscv_hand_over_probe_returning(probe: usize);
    /// Restores the context `frame` holds, except `tp`, and returns into
    /// it with the `SIE` the frame's `SPIE` records.
    ///
    /// # Safety
    ///
    /// `frame` must be a [`TrapFrame`] copy [`enter_swap_fault_trampoline`]
    /// put on the stack this code is running on.
    fn __helios_riscv_swap_fault_resume(frame: *mut TrapFrame) -> !;
}

/// Rewrites a saved context so the trap exit returns into
/// [`swap_fault_trampoline`] on the faulting stack instead of retrying the
/// faulting instruction.
///
/// A copy of the context goes one frame below the interrupted `sp`, which
/// is free because the riscv psABI has no red zone, and the trampoline gets
/// its address. The exit restores the frame, `sstatus` included, so the
/// `sret` lands in the trampoline in supervisor mode with `SIE` as the
/// faulting context had it, and hands `sscratch` back to the trap stack
/// top: the trampoline runs outside a handler, and may block the fiber it
/// runs on.
///
/// # Safety
///
/// `frame` must be the outermost frame the trap entry built (its exit
/// hands `sscratch` back), and the `sp` it records must be a live stack
/// with a frame's worth of room below it.
pub(crate) unsafe fn enter_swap_fault_trampoline(
    frame: &mut TrapFrame,
    faulting_address: usize,
    access: crate::vmm::FaultAccess,
) {
    let landing = (frame.general.sp - size_of::<TrapFrame>()) & !15;
    // SAFETY: the caller's contract. A fault on this write is a stack
    // commit on the trap stack, which the nested entry resolves and
    // returns from.
    unsafe {
        (landing as *mut TrapFrame).write(*frame);
    }
    frame.general.a0 = landing;
    frame.general.a1 = faulting_address;
    frame.general.a2 = usize::from(access == crate::vmm::FaultAccess::Fetch);
    frame.general.sp = landing;
    frame.sepc = swap_fault_trampoline as *const () as usize;
}

/// Resolves a swapped-out page from the faulting context's own stack.
///
/// This runs in the fiber the fault interrupted, so it may block that
/// fiber exactly as an async host call does: the executor keeps running,
/// the swap task performs the read, and the fiber is resumed here, on
/// whichever hart polls it next. On success the saved context is restored
/// and the faulting instruction runs again, having observed nothing.
///
/// A fault the kernel cannot resolve (no fiber to block, the fiber
/// cancelled mid-wait, the swap device refusing the read) is handed to
/// the runtime's trap handler, which unwinds the guest. Only a fault the
/// runtime does not claim either is fatal.
///
/// `fetch` says whether the fault was an instruction fetch: the saved
/// registers do not record `scause`.
extern "C" fn swap_fault_trampoline(
    frame: *mut TrapFrame,
    faulting_address: usize,
    fetch: bool,
) -> ! {
    // SAFETY: the frame is intact; only its saved registers are read. The
    // trampoline runs with the interrupt mask the faulting context had
    // (`SPIE` was left alone), so what the runtime's unwind lands in runs
    // on that context's terms without a mask to restore.
    let (sepc, frame_pointer) = unsafe { ((*frame).sepc, (*frame).general.s0) };
    helios_kernel::run_swap_fault_trampoline(
        helios_hal::vmm::VirtAddr::new(faulting_address),
        KernelException {
            cause: if fetch {
                KernelExceptionCause::InstructionFault
            } else {
                KernelExceptionCause::DataFault
            },
            instruction_pointer: sepc,
            frame_pointer,
            faulting_address: Some(faulting_address),
        },
        // SAFETY: `frame` is the copy `enter_swap_fault_trampoline` put
        // one frame below this stack's interrupted `sp`, and this code is
        // running on that stack.
        || unsafe { __helios_riscv_swap_fault_resume(frame) },
    )
}

/// Allocates one hart's trap stack and returns its byte range.
pub(crate) fn allocate_trap_stack() -> Range<usize> {
    let layout = Layout::from_size_align(TRAP_STACK_BYTES, 16)
        .expect("trap stack layout is a power-of-two size at 16-byte alignment");
    // SAFETY: the layout has non-zero size.
    let base = unsafe { alloc_zeroed(layout) };
    let base = NonNull::new(base)
        .unwrap_or_else(|| panic!("failed to allocate a {TRAP_STACK_BYTES}-byte trap stack"))
        .as_ptr() as usize;
    base..base + TRAP_STACK_BYTES
}

/// Publishes `stack` as this hart's trap stack: from here on every trap
/// the hart takes is built on it rather than on the interrupted stack.
///
/// # Safety
///
/// `stack` must be the range [`allocate_trap_stack`] returned for this
/// hart and nothing else may ever write `sscratch`; the entry and exit
/// own that register from this point.
pub(crate) unsafe fn seed_trap_stack(stack: &Range<usize>) {
    unsafe {
        asm!("csrw sscratch, {top}", top = in(reg) stack.end, options(nomem, nostack));
    }
}

/// A frame outside the hart's trap stack means the swap in the entry is
/// not in effect, and the next trap raised below `sp` recurses instead of
/// being reported. Caught on the first trap of any kind rather than there,
/// and reported with that trap's cause and program counter: the trap
/// that finds the swap off is never the one that turned it off, and the
/// two addresses say what kind of context was running.
pub(crate) fn assert_frame_on_trap_stack(frame: &TrapFrame, stack: &Range<usize>) {
    let address = core::ptr::from_ref(frame) as usize;
    assert!(
        stack.contains(&address),
        "riscv trap frame at {address:#x} is not on this hart's trap stack {:#x}..{:#x} \
         (scause={:#x}, sepc={:#x}, interrupted sp={:#x})",
        stack.start,
        stack.end,
        riscv::register::scause::read().bits(),
        frame.sepc,
        frame.general.sp,
    );
}

/// The trap stack top the exit would hand back to `sscratch` for
/// `frame`, read from the slot the entry keeps above the frame; zero
/// for a nested frame, whose exit leaves the hart inside the outer
/// handler.
fn trap_stack_top_for(frame: &TrapFrame) -> usize {
    // SAFETY: the entry carves `TRAP_REGION_BYTES` per trap and writes
    // the slot at `TRAP_FRAME_BYTES` above the frame; `frame` is one the
    // entry built, which is what every caller holds.
    unsafe {
        core::ptr::from_ref(frame)
            .cast::<u8>()
            .add(TRAP_FRAME_BYTES)
            .cast::<usize>()
            .read()
    }
}

/// Whether `frame` is the outermost the trap entry built: the interrupted
/// context was outside every handler, so its `sp` is a stack a fiber can
/// block on and the exit hands `sscratch` back to the trap stack top.
pub(crate) fn is_outermost_frame(frame: &TrapFrame) -> bool {
    trap_stack_top_for(frame) != 0
}

/// What the hand-over needs once the trap stack is gone: the exception
/// for the runtime, and what the fatal report says when the runtime
/// declines it, the interrupted context included.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub(crate) struct TrapHandOver {
    pub(crate) exception: KernelException,
    pub(crate) cause: Exception,
    pub(crate) stval: usize,
    pub(crate) stack_fault: StackFault,
    /// Where the entry built the frame, and a copy of it.
    pub(crate) frame_address: usize,
    pub(crate) frame: TrapFrame,
}

/// Why a kernel exception the kernel did not resolve stays with the
/// backend and is reported where it was taken.
#[derive(Clone, Copy, Debug)]
pub(crate) enum HandOverRefusal {
    /// The hart has no runtime yet, so it has no hand-over stack and the
    /// runtime has not run anything on it.
    NoHartRuntime,
    /// The trap interrupted code on the trap stack (a nested frame): a
    /// fault inside a trap handler, never a guest trap, and handing it
    /// over would abandon the handler it interrupted.
    InsideTrapHandler,
    /// The trap interrupted the hand-over itself: the runtime's handler,
    /// or the report of a trap it declined.
    InsideHandOver,
}

impl fmt::Display for HandOverRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoHartRuntime => "this hart has no runtime yet",
            Self::InsideTrapHandler => "it interrupted a handler running on the trap stack",
            Self::InsideHandOver => "it interrupted a trap hand-over in progress",
        })
    }
}

/// Hands `record` to the runtime from this hart's hand-over stack.
///
/// The runtime ends a trap it claims by writing `sp` and `fp` and
/// jumping to its resume point (`resume_to_exception_handler` in the
/// Wasmtime fork's unwinder); nothing of the backend runs after that
/// jump. What the exit in `trap.S` would have restored for the outermost
/// frame therefore has to be in place before the handler starts:
/// `sscratch` back at the trap stack top, so the next trap carves its
/// region there instead of under whatever `sp` the continuation has,
/// and `SIE` as the interrupted context had it, because everything the
/// resume lands in — the store's teardown, its fiber stack's release,
/// the TLB shootdown that release broadcasts — is that context's
/// continuation. With `sscratch` handed back, the trap stack belongs to
/// the next trap, so the handler cannot run on it either: the record is
/// copied to the hand-over stack and `__helios_riscv_trap_hand_over`
/// switches to it, hands the trap stack back and sets `SIE`, in that
/// order, while `SIE` is still clear.
///
/// Diverges for every exception it hands over; returns the reason only
/// when the exception must not leave the trap stack.
///
/// # Concurrency contract
///
/// The hand-over stack belongs to one hart and has one user at a time:
/// a hand-over ends in the runtime's jump or in a panic, nothing
/// switches away from it while it runs, and a trap taken during it is
/// never handed over again ([`HandOverRefusal::InsideHandOver`]).
pub(crate) fn hand_over_trap(record: TrapHandOver, frame: &TrapFrame) -> HandOverRefusal {
    let Some(runtime) = crate::installed_hart_runtime() else {
        return HandOverRefusal::NoHartRuntime;
    };
    let trap_stack_top = trap_stack_top_for(frame);
    if trap_stack_top == 0 {
        return HandOverRefusal::InsideTrapHandler;
    }
    if runtime.hand_over_stack.contains(&frame.general.sp) {
        return HandOverRefusal::InsideHandOver;
    }
    // `TrapHandOver` is 16-byte aligned, so its size is a multiple of 16
    // and the slot below the 16-byte aligned top keeps `sp` aligned.
    let slot = runtime.hand_over_stack.end - core::mem::size_of::<TrapHandOver>();
    // SAFETY: the hand-over stack belongs to this hart and nothing runs on
    // it: the refusals above rule out a trap taken from a hand-over in
    // progress, and none is suspended. The switch is entered from the
    // dispatcher of an outermost frame, so `sscratch` is zero and `SIE`
    // clear, with the slot the record was just written to and the top
    // this frame's exit would have handed back.
    unsafe {
        (slot as *mut TrapHandOver).write(record);
        __helios_riscv_trap_hand_over(
            slot,
            trap_stack_top,
            usize::from(frame.sstatus & crate::SSTATUS_SPIE_BIT != 0),
        )
    }
}

/// The hand-over's Rust half, on the hand-over stack in the state the
/// trap exit would have restored.
#[unsafe(no_mangle)]
extern "C" fn __helios_riscv_trap_hand_over_main(record: usize) -> ! {
    // SAFETY: `hand_over_trap` wrote a `TrapHandOver` at this address,
    // above this frame, and nothing else writes the hand-over stack.
    let record = unsafe { (record as *const TrapHandOver).read() };
    crate::current_hart_runtime().hand_over_probe.claim(&record);
    let _ = helios_kernel::dispatch_native_trap(record.exception);
    panic!(
        "kernel exception: {:?}, sepc={:#x}, stval={:#x}{}, tf={:#x?}",
        record.cause, record.frame.sepc, record.stval, record.stack_fault, record.frame,
    )
}

/// The record of the boot-time self-check that proves the hand-over on
/// this hart: a trap claimed the way the runtime claims one, then an
/// interrupt and a second trap, each checked for where its frame landed
/// and what interrupted `sp` it recorded.
///
/// Layout is pinned by `hand_over_probe.S` through the `offset_of!`
/// operands below. Every word is written by this hart's own code and its
/// own trap dispatcher; the atomics exist for `Sync`, not for ordering
/// between harts, and every access is `Relaxed`.
#[repr(C)]
pub(crate) struct HandOverProbe {
    /// The address of the claimed `ebreak` while the probe waits for it,
    /// zero otherwise.
    claim_pc: AtomicUsize,
    /// Where the claim resumes, as the runtime's handler would.
    resume_pc: AtomicUsize,
    resume_sp: AtomicUsize,
    resume_fp: AtomicUsize,
    /// What the resumed code found in `sscratch` and `sstatus`.
    resumed_sscratch: AtomicUsize,
    resumed_sstatus: AtomicUsize,
    /// The claimed trap's frame address and the `sp` it recorded.
    claimed_frame: AtomicUsize,
    claimed_frame_sp: AtomicUsize,
    /// The `sp` the hand-over ran its Rust half on.
    hand_over_sp: AtomicUsize,
    /// The `sp` the interrupt was raised on.
    interrupt_sp: AtomicUsize,
    /// [`PROBE_INTERRUPT_ARMED`] while the probe waits for its
    /// interrupt, then the frame address the entry built.
    interrupt_frame: AtomicUsize,
    interrupt_frame_sp: AtomicUsize,
    /// The address of the returning `ebreak` while the probe waits for
    /// it, zero otherwise.
    return_pc: AtomicUsize,
    /// The `sp` the returning `ebreak` ran on.
    return_sp: AtomicUsize,
    /// The returning trap's frame address and the `sp` it recorded.
    returned_frame: AtomicUsize,
    returned_frame_sp: AtomicUsize,
}

/// The value [`HandOverProbe::interrupt_frame`] holds while the
/// self-check waits for its interrupt (`hand_over_probe.S` stores it as
/// `-1`); no frame lives at the top of the address space.
const PROBE_INTERRUPT_ARMED: usize = usize::MAX;
/// Bytes of the probe's `ebreak`, which `hand_over_probe.S` assembles
/// without the compressed extension.
const EBREAK_BYTES: usize = 4;

impl HandOverProbe {
    pub(crate) const fn new() -> Self {
        Self {
            claim_pc: AtomicUsize::new(0),
            resume_pc: AtomicUsize::new(0),
            resume_sp: AtomicUsize::new(0),
            resume_fp: AtomicUsize::new(0),
            resumed_sscratch: AtomicUsize::new(0),
            resumed_sstatus: AtomicUsize::new(0),
            claimed_frame: AtomicUsize::new(0),
            claimed_frame_sp: AtomicUsize::new(0),
            hand_over_sp: AtomicUsize::new(0),
            interrupt_sp: AtomicUsize::new(0),
            interrupt_frame: AtomicUsize::new(0),
            interrupt_frame_sp: AtomicUsize::new(0),
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
        self.claimed_frame
            .store(record.frame_address, Ordering::Relaxed);
        self.claimed_frame_sp
            .store(record.frame.general.sp, Ordering::Relaxed);
        self.hand_over_sp
            .store(current_stack_pointer(), Ordering::Relaxed);
        // SAFETY: the probe recorded a landing in its own routine, with the
        // `sp` and `fp` of that routine's frame, which is live: the routine
        // is waiting on the `ebreak` this claims.
        unsafe {
            resume_as_the_runtime_does(
                self.resume_pc.load(Ordering::Relaxed),
                self.resume_sp.load(Ordering::Relaxed),
                self.resume_fp.load(Ordering::Relaxed),
            )
        }
    }

    /// Records the frame of the interrupt the probe is waiting for.
    pub(crate) fn record_interrupt(&self, frame: &TrapFrame) {
        if self
            .interrupt_frame
            .compare_exchange(
                PROBE_INTERRUPT_ARMED,
                core::ptr::from_ref(frame) as usize,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            self.interrupt_frame_sp
                .store(frame.general.sp, Ordering::Relaxed);
        }
    }

    /// Resolves the probe's returning `ebreak` in place: records the
    /// frame, steps past the `ebreak`, and tells the dispatcher to
    /// return. `false` for every other `ebreak`.
    pub(crate) fn resolve_returning_trap(&self, frame: &mut TrapFrame) -> bool {
        let armed = self.return_pc.load(Ordering::Relaxed);
        if armed == 0 || armed != frame.sepc {
            return false;
        }
        self.return_pc.store(0, Ordering::Relaxed);
        self.returned_frame
            .store(core::ptr::from_ref(frame) as usize, Ordering::Relaxed);
        self.returned_frame_sp
            .store(frame.general.sp, Ordering::Relaxed);
        frame.sepc += EBREAK_BYTES;
        true
    }
}

/// The runtime's resume, instruction for instruction: the RISC-V
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
            "mv sp, a2",
            "mv fp, a3",
            "jr a4",
            in("a0") 0usize,
            in("a1") 0usize,
            in("a2") sp,
            in("a3") fp,
            in("a4") pc,
            options(nostack, nomem, noreturn),
        );
    }
}

fn current_stack_pointer() -> usize {
    let sp: usize;
    // SAFETY: reads the stack pointer and nothing else.
    unsafe {
        asm!("mv {sp}, sp", sp = out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    sp
}

/// Proves on the calling hart that a trap the runtime claims leaves it
/// as the trap exit would.
///
/// The probe raises an `ebreak` and claims it from the hand-over exactly
/// as the runtime claims a wasm trap, then takes a supervisor software
/// interrupt and a second `ebreak` on the same hart. Each must carve its
/// region at the top of this hart's trap stack and record the `sp` its
/// code was really running on; after the claim, `sscratch` must hold the
/// trap stack top again and `SIE` must be set as it was. A hart that
/// fails any of these corrupts the trap stack at its next trap, so the
/// verdict is fatal, like the floating-point self-check before it.
///
/// Runs once per hart, after the trap vector is installed and with
/// `sie.SSIE` and `sstatus.SIE` set.
pub(crate) fn verify_trap_hand_over() {
    let runtime = crate::current_hart_runtime();
    let probe = &runtime.hand_over_probe;
    let probe_address = core::ptr::from_ref(probe) as usize;
    let trap_stack = &runtime.trap_stack;
    let outermost_frame = trap_stack.end - TRAP_REGION_BYTES;
    let hart = runtime.hart_id;
    assert!(
        riscv::register::sstatus::read().sie(),
        "riscv hart {hart:?} ran the trap hand-over self-check with SIE clear; it cannot take \
         the interrupt it checks"
    );

    // SAFETY: the routine writes only its own frame and `probe`, and saves
    // the callee-saved registers the claim does not restore.
    unsafe { __helios_riscv_hand_over_probe_claimed(probe_address) };
    let claimed_frame = probe.claimed_frame.load(Ordering::Relaxed);
    let claimed_frame_sp = probe.claimed_frame_sp.load(Ordering::Relaxed);
    let resume_sp = probe.resume_sp.load(Ordering::Relaxed);
    assert!(
        claimed_frame == outermost_frame && claimed_frame_sp == resume_sp,
        "riscv hart {hart:?} built the claimed trap's frame at {claimed_frame:#x} recording sp \
         {claimed_frame_sp:#x}; expected {outermost_frame:#x} recording {resume_sp:#x}"
    );
    let hand_over_sp = probe.hand_over_sp.load(Ordering::Relaxed);
    assert!(
        runtime.hand_over_stack.contains(&hand_over_sp),
        "riscv hart {hart:?} ran the runtime's trap handler at sp {hand_over_sp:#x}, outside its \
         hand-over stack {:#x}..{:#x}",
        runtime.hand_over_stack.start,
        runtime.hand_over_stack.end
    );
    let sscratch = probe.resumed_sscratch.load(Ordering::Relaxed);
    assert!(
        sscratch == trap_stack.end,
        "riscv hart {hart:?} resumed a claimed trap with sscratch {sscratch:#x}, not the trap \
         stack top {:#x}",
        trap_stack.end
    );
    let sstatus = probe.resumed_sstatus.load(Ordering::Relaxed);
    assert!(
        sstatus & crate::SSTATUS_SIE_BIT != 0,
        "riscv hart {hart:?} resumed a claimed trap with sstatus {sstatus:#x}: the interrupted \
         context's SIE was not restored"
    );

    // SAFETY: the routine writes only `probe`; the interrupt and the trap
    // it raises are both handled in place and return with every register
    // restored.
    unsafe { __helios_riscv_hand_over_probe_returning(probe_address) };
    let interrupt_frame = probe.interrupt_frame.load(Ordering::Relaxed);
    let interrupt_frame_sp = probe.interrupt_frame_sp.load(Ordering::Relaxed);
    let interrupt_sp = probe.interrupt_sp.load(Ordering::Relaxed);
    assert!(
        interrupt_frame == outermost_frame && interrupt_frame_sp == interrupt_sp,
        "riscv hart {hart:?} built an interrupt frame at {interrupt_frame:#x} recording sp \
         {interrupt_frame_sp:#x} after a claimed trap; expected {outermost_frame:#x} recording \
         {interrupt_sp:#x}"
    );
    let returned_frame = probe.returned_frame.load(Ordering::Relaxed);
    let returned_frame_sp = probe.returned_frame_sp.load(Ordering::Relaxed);
    let return_sp = probe.return_sp.load(Ordering::Relaxed);
    assert!(
        returned_frame == outermost_frame && returned_frame_sp == return_sp,
        "riscv hart {hart:?} built the second trap's frame at {returned_frame:#x} recording sp \
         {returned_frame_sp:#x}; expected {outermost_frame:#x} recording {return_sp:#x}"
    );
    tracing::info!(
        target: "helios_riscv::trap",
        hart = ?hart,
        "claimed trap resumed off the trap stack with sscratch handed back"
    );
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
