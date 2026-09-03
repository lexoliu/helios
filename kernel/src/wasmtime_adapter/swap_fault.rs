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

use helios_hal::vmm::VirtAddr;
use wasmtime::BlockOnCurrentFiberError;

use crate::memory::{SwapFaultError, installed_swap_handle};

/// Reads the page at `addr` back in, blocking the fiber this is called
/// on until the swap device has it.
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
