//! Swap's answer to the runtime's residency hook.
//!
//! Wasmtime copies guest bytes for streams and futures from the store's
//! event loop, which runs inside the instance's turn but on no fiber, so
//! a swapped-out page there cannot be faulted in the way a guest access
//! is. Before each such copy, in the same poll, Wasmtime names the bytes
//! it is about to touch; this hook answers from swap's page-table entries
//! and, for a page that is out, hands back the fault-in to await. The
//! copy happens only after a later check finds every page present, in
//! the poll that makes it, which is the contract of the swap module
//! (`crate::memory::swap`).
//!
//! # Concurrency
//!
//! The hook is owned by its store and called with the store exclusively
//! borrowed, from the turn that polls it. A check is lock-free (see
//! [`GuestResidency`]); a fault-in it returns queues one request on the
//! swap service, which settles it from whatever processor that service
//! runs on.

use core::ptr::NonNull;

use helios_hal::vmm::{VirtAddr, VirtRange};
use wasmtime::component::GuestMemoryResidency;

use crate::memory::{GuestResidency, SwapFaultError, SwapFaultIn};

/// The residency hook a component store whose guest memory swap can take
/// is given.
pub(crate) struct SwapResidencyHook {
    residency: GuestResidency,
}

impl SwapResidencyHook {
    /// The hook for this machine, or `None` when it cannot swap at all:
    /// then no store registers one, and the runtime's copies pay nothing.
    pub(crate) fn installed() -> Option<Self> {
        GuestResidency::installed().map(Self::new)
    }

    /// The hook answering from `residency`.
    pub(crate) const fn new(residency: GuestResidency) -> Self {
        Self { residency }
    }
}

impl GuestMemoryResidency for SwapResidencyHook {
    type Error = SwapFaultError;
    type Wait = SwapFaultIn;

    fn wait_until_resident(&mut self, bytes: NonNull<[u8]>) -> Option<SwapFaultIn> {
        let range = VirtRange::new(VirtAddr::new(bytes.cast::<u8>().addr().get()), bytes.len());
        self.residency.fault_in_first_swapped(range)
    }
}
