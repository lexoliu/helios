//! Where the runtime's linear memory meets the store's view table.
//!
//! The view table in [`crate::buffer`] checks every span against the
//! memory as it stands at the call, and only the runtime can say how far
//! that is: the guest grows its memory whenever it likes. This is the
//! one place that asks, and the one place a view the pool cannot refill
//! becomes the trap that kills its instance.
//!
//! # Concurrency contract
//!
//! Everything here runs on the task that owns the store, inside a host
//! call: the memory handle and the store context it is read through are
//! that task's, and nothing is shared.

use helios_hal::vmm::VirtAddr;
use wasmtime::AsContext;

use crate::buffer::{GuestMemory, ViewError};
use crate::{ProgramOutOfMemory, user_heap_stats};

/// The calling instance's linear memory, read from the runtime now.
///
/// `memory` is the handle the store recorded when the instance was
/// built, absent when its component lowers through a shared memory or
/// through more than one: the runtime does not hand a shared memory back
/// as an unshared one, and nothing is shown in a memory another
/// processor may be running code against.
pub(crate) fn guest_memory(
    memory: Option<wasmtime::Memory>,
    store: impl AsContext,
) -> Result<GuestMemory, ViewError> {
    let memory = memory.ok_or(ViewError::NoUnsharedMemory)?;
    let base = memory.data_ptr(&store) as usize;
    let bytes = memory.data_size(&store) as u64;
    Ok(GuestMemory::new(VirtAddr::new(base), bytes))
}

/// The trap a view that could not be ended earns.
///
/// A view the pool cannot put zeroed memory back behind is a user OOM:
/// the instance is killed and its pool reclaimed, and the view is
/// withdrawn with the rest of the store. Anything else the table
/// answers here is a handle naming a view the table never made, which
/// is a kernel fault worth a message rather than a guest-visible error.
pub(crate) fn view_trap(error: ViewError) -> wasmtime::Error {
    match error {
        ViewError::OutOfMemory { bytes } => {
            let user_heap = user_heap_stats();
            ProgramOutOfMemory {
                requested_bytes: usize::try_from(bytes)
                    .expect("a view that was shown fits the address space"),
                available_bytes: user_heap.available_bytes(),
                pool_bytes: user_heap.total_bytes,
                reserved_bytes: 0,
            }
            .into()
        }
        other => wasmtime::Error::msg(alloc::format!("a view could not be ended: {other}")),
    }
}
