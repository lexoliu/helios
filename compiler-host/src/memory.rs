//! Byte copies in and out of the plugin's shared linear memory.
//!
//! The host only touches regions the plugin handed it (an allocation it
//! returned, a response it wrote) while no plugin code is using them, which
//! is the same discipline the kernel's `read_shared_memory` and
//! `write_shared_memory` keep.

use thiserror::Error;
use wasmtime::SharedMemory;

/// A host access to guest memory that falls outside the memory's current
/// size.
#[derive(Debug, Error)]
#[error("guest memory access {start:#x}+{len} is outside the plugin's {size}-byte memory")]
pub struct GuestMemoryAccess {
    pub start: u32,
    pub len: usize,
    pub size: usize,
}

fn checked_range(
    memory: &SharedMemory,
    start: u32,
    len: usize,
) -> Result<*mut u8, GuestMemoryAccess> {
    let data = memory.data();
    let size = data.len();
    let in_bounds = (start as usize)
        .checked_add(len)
        .is_some_and(|end| end <= size);
    if !in_bounds {
        return Err(GuestMemoryAccess { start, len, size });
    }
    // SAFETY: `start` is within the slice, checked above.
    Ok(unsafe { data.as_ptr().cast::<u8>().cast_mut().add(start as usize) })
}

pub(crate) fn read(
    memory: &SharedMemory,
    start: u32,
    len: usize,
) -> Result<Vec<u8>, GuestMemoryAccess> {
    let source = checked_range(memory, start, len)?;
    // SAFETY: `source..source + len` is inside the memory, and no plugin
    // code writes the region while the host reads it.
    Ok(unsafe { core::slice::from_raw_parts(source, len) }.to_vec())
}

pub(crate) fn write(
    memory: &SharedMemory,
    start: u32,
    bytes: &[u8],
) -> Result<(), GuestMemoryAccess> {
    let destination = checked_range(memory, start, bytes.len())?;
    // SAFETY: the destination is inside the memory, cannot overlap a host
    // buffer, and no plugin code touches the region while the host writes it.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len()) };
    Ok(())
}
