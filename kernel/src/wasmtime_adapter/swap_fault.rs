//! The bridge from a hardware page fault to the runtime's fiber.
//!
//! A backend's fault entry redirects a fault on a swapped-out page onto
//! a trampoline running on the faulting fiber's own stack, and the
//! trampoline calls this. Blocking here suspends that fiber exactly the
//! way an `async` host function does: the executor keeps running, the
//! swap task performs the read on whatever processor it lives on, and
//! the fiber is resumed in place.
//!
//! The invariant this rests on: **only guest code may fault on user
//! memory.** Kernel code has no fiber to suspend, so every kernel path
//! that touches guest memory directly pre-faults its range with
//! [`SwapHandle::ensure_present`](crate::SwapHandle::ensure_present)
//! first. A fault that arrives with no fiber under it is that invariant
//! being broken, and it is reported rather than papered over.

use core::convert::Infallible;

use helios_hal::vmm::VirtAddr;
use wasmtime::BlockOnCurrentFiberError;

use crate::kernel_exception::{KernelException, dispatch_native_trap};
use crate::memory::{
    InterruptedContext, StackFault, SwapFaultError, installed_swap_handle, resolve_stack_fault,
    swapped_token,
};

/// Reads the page at `addr` back in, blocking the fiber this is called
/// on until the swap device has it.
///
/// Returns `Ok` once the page is present, whether this request read it
/// back or another fault on the same page already had: the faulting
/// instruction is retried either way.
///
/// Returns `Err` when the page could not be reinstated. The caller must
/// then let the fault reach the runtime's trap handler, which unwinds
/// the guest — returning to the faulting instruction would fault again
/// forever.
pub fn resolve_swap_fault_blocking(addr: VirtAddr) -> Result<(), SwapFaultError> {
    let Some(handle) = installed_swap_handle() else {
        return Err(SwapFaultError::NotConfigured);
    };
    // SAFETY: the caller is the backend's swap trampoline, which the
    // fault entry entered on the faulting fiber's own stack with no
    // store borrow live and nothing held that the executor needs.
    match unsafe { wasmtime::block_on_current_fiber(handle.fault_in(addr)) } {
        Ok(outcome) => outcome,
        Err(BlockOnCurrentFiberError::NoFiber) => {
            tracing::error!(
                target: "helios_kernel::swap",
                addr = addr.raw(),
                "user memory faulted outside a fiber: a kernel path touched guest memory \
                 without calling ensure_present() on it first"
            );
            Err(SwapFaultError::NotConfigured)
        }
        Err(BlockOnCurrentFiberError::Unavailable) => {
            tracing::error!(
                target: "helios_kernel::swap",
                addr = addr.raw(),
                "user memory faulted inside a host call that already holds the fiber: \
                 that path must call ensure_present() before touching guest memory"
            );
            Err(SwapFaultError::NotConfigured)
        }
        Err(BlockOnCurrentFiberError::Cancelled(_)) => {
            // The instance is being torn down; the fiber was resumed
            // only so it can unwind. Reporting the failure sends the
            // fault to the runtime's trap handler, which is exactly the
            // unwinding the runtime is waiting for.
            Err(SwapFaultError::Backend)
        }
    }
}

/// What a backend's page-fault entry makes of a fault on a user address,
/// once the steps that belong to the page-table format (an access-flag
/// fault on aarch64, a spurious-fault fence on riscv) have declined it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserPageFault {
    /// A reserved page inside a live fiber stack, committed in place. The
    /// faulting instruction runs again.
    Resolved,
    /// A swapped-out page. The entry redirects the faulting context onto
    /// a trampoline on its own stack, which calls
    /// [`run_swap_fault_trampoline`].
    Swapped,
    /// Neither: the fault belongs to the runtime, which unwinds a guest
    /// that overflowed its stack or touched memory it was never given.
    /// The stack arena's verdict rides along for the entry's fatal report.
    Unclaimed(StackFault),
}

/// Classifies a fault at `addr` in the order every backend uses: the fiber
/// stack arena first, because it resolves from the user pool without
/// touching the swap tables, then the swap entry.
///
/// `interrupted` is what the backend's entry saved of the context the
/// fault interrupted; see [`InterruptedContext`]. Safe to call from trap
/// context: it waits only as [`resolve_stack_fault`] does.
pub fn classify_user_page_fault(addr: VirtAddr, interrupted: InterruptedContext) -> UserPageFault {
    let stack_fault = resolve_stack_fault(addr, interrupted);
    if stack_fault == StackFault::Committed {
        return UserPageFault::Resolved;
    }
    if swapped_token(addr).is_some() {
        return UserPageFault::Swapped;
    }
    UserPageFault::Unclaimed(stack_fault)
}

/// The body of the swap-fault trampoline, run on the faulting fiber's own
/// stack with the interrupted context's interrupt mask.
///
/// Reads the page back, blocking the fiber, and on success calls `resume`,
/// which restores the saved context and retries the faulting instruction.
/// A fault the kernel cannot resolve (no fiber to block, the fiber
/// cancelled mid-wait, the swap device refusing the read) is handed to the
/// runtime's trap handler, which unwinds the guest and does not return
/// when it claims the fault; reaching the panic is the runtime declining
/// it too.
///
/// `interrupted` describes the context the fault interrupted, as the
/// backend's own entry would have reported it. `resume` returns
/// [`Infallible`] because it never comes back: it is the backend's resume
/// path, which ends in the instruction that returns to the interrupted
/// context.
pub fn run_swap_fault_trampoline<Resume>(
    addr: VirtAddr,
    interrupted: KernelException,
    resume: Resume,
) -> !
where
    Resume: FnOnce() -> Infallible,
{
    let outcome = resolve_swap_fault_blocking(addr);
    if outcome.is_ok() {
        match resume() {}
    }
    tracing::error!(
        target: "helios_kernel::swap",
        addr = addr.raw(),
        error = ?outcome.err(),
        "page fault on a swapped-out page could not be resolved"
    );
    let _ = dispatch_native_trap(interrupted);
    panic!(
        "unresolvable swap fault at {:#x} that the runtime did not claim either",
        addr.raw()
    )
}
