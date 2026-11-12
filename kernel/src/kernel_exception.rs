use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelExceptionCause {
    InstructionFault,
    DataFault,
    IllegalInstruction,
    Breakpoint,
    Arithmetic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelException {
    pub cause: KernelExceptionCause,
    pub instruction_pointer: usize,
    pub frame_pointer: usize,
    pub faulting_address: Option<usize>,
}

pub type KernelNativeTrapHandler = extern "C" fn(
    instruction_pointer: usize,
    frame_pointer: usize,
    has_faulting_address: bool,
    faulting_address: usize,
);

/// What a backend's exception entry does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelExceptionDispatch {
    /// The fault was resolved in place. The entry restores the
    /// interrupted context and the faulting instruction runs again.
    Resolved,
    /// Nobody claimed the exception; the entry reports it as fatal.
    Unhandled,
}

impl KernelException {
    fn dispatch_to(self, handler: KernelNativeTrapHandler) -> KernelExceptionDispatch {
        handler(
            self.instruction_pointer,
            self.frame_pointer,
            self.faulting_address.is_some(),
            self.faulting_address.unwrap_or(0),
        );
        KernelExceptionDispatch::Unhandled
    }
}

/// The runtime's native trap handler, published for the whole machine.
///
/// Wasmtime initialises its trap handling once per machine, on
/// whichever processor first builds an engine. A slot on that
/// processor's runtime therefore leaves every other processor unable to
/// claim a wasm trap: a guest's out-of-bounds load taken there reaches
/// the backend's "unhandled exception" panic and takes the machine down
/// instead of trapping the one instance. The handler is the same
/// function on every processor, so the machine publishes it once and
/// every fault entry reads it here.
///
/// # Concurrency contract
///
/// [`install_native_trap_handler`] writes the word once, from the
/// processor that initialised the runtime, before any guest code has
/// run. [`dispatch_native_trap`] reads it from fault context on every
/// processor, where nothing may lock or allocate — which is why this is
/// a plain atomic word and not a structure behind a guard.
static NATIVE_TRAP_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Publishes the runtime's native trap handler for every processor.
///
/// Backends call this from the `wasmtime_init_traps` linkage symbol.
///
/// # Panics
///
/// When a second, different handler is published. One machine has one
/// runtime trap handler; two would mean the fault entries were sending
/// traps to whichever was installed last, and there is no correct way
/// to pick between them.
pub fn install_native_trap_handler(handler: KernelNativeTrapHandler) {
    let handler = handler as usize;
    let previous = NATIVE_TRAP_HANDLER.swap(handler, Ordering::Release);
    assert!(
        previous == 0 || previous == handler,
        "the runtime published a second native trap handler {handler:#x} over {previous:#x}"
    );
}

/// Hands `exception` to the runtime's native trap handler.
///
/// Returns [`KernelExceptionDispatch::Unhandled`] when no handler is
/// installed yet or when the runtime did not claim the exception. A
/// claimed exception never returns here: the handler ends it by writing
/// the stack and frame pointers of the runtime's resume point and
/// branching there, and that write is where execution leaves the stack
/// this call was made on. No backend code runs after it.
///
/// So the backend calls this with the processor already in the state
/// its exception epilogue would have restored: the interrupted context's
/// interrupt mask and stack-pointer selection, and every piece of
/// per-processor entry state the epilogue hands back (the AArch64
/// exception-stack top in `SP_EL0` with `SP_EL1` selected, the RISC-V
/// trap-stack top in `sscratch`). It calls from a stack that no
/// exception taken during the call builds its frame on: the interrupted
/// stack itself, after the epilogue has returned onto it, or a
/// per-processor stack set aside for the call, which the backend
/// switches to off the exception stack. The switch and the hand-back
/// are one step, taken with interrupts still masked and in that order,
/// so the entry state is handed back at the moment execution leaves the
/// exception stack. Handing it back while still running there would let
/// an interrupt build its frame over the live one; handing it back after
/// the call is impossible. A trap the runtime declines is reported from
/// the same state.
pub fn dispatch_native_trap(exception: KernelException) -> KernelExceptionDispatch {
    let raw_handler = NATIVE_TRAP_HANDLER.load(Ordering::Acquire);
    if raw_handler == 0 {
        return KernelExceptionDispatch::Unhandled;
    }
    // SAFETY: the only writer of this word is
    // `install_native_trap_handler`, which stores a
    // `KernelNativeTrapHandler` cast to `usize`; zero means "unset" and
    // the check above has already ruled it out.
    let handler = unsafe { core::mem::transmute::<usize, KernelNativeTrapHandler>(raw_handler) };
    exception.dispatch_to(handler)
}
