extern crate alloc;

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use core::alloc::Layout;
use core::ptr::NonNull;

use thiserror::Error;
use wasmtime::{LinearMemory, MemoryCreator, MemoryType};

use crate::ProgramOutOfMemory;
use crate::memory::{allocate_user_uninit_on, deallocate_user_on};
use helios_hal::cpu::{Cpu, ProcessorId};

const LINEAR_MEMORY_ALIGNMENT: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct UserMemoryCreator<C: Cpu + Clone> {
    processor: ProcessorId,
    cpu: C,
}

impl<C: Cpu + Clone> UserMemoryCreator<C> {
    pub(crate) fn new(cpu: C) -> Self {
        let processor = cpu.current_processor();
        Self { processor, cpu }
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
enum UserLinearMemoryError {
    #[error("user linear memory guard pages require a virtual-memory backed allocator")]
    GuardPagesUnsupported,
    #[error("user linear memory reservation requires a virtual-memory backed allocator")]
    ReservationUnsupported,
    #[error("shared user linear memory must declare a maximum size")]
    SharedMaximumMissing,
    #[error("user linear memory capacity is smaller than its minimum size")]
    CapacityBelowMinimum,
    #[error("invalid user linear memory layout")]
    InvalidLayout,
    #[error("{0}")]
    OutOfMemory(ProgramOutOfMemory),
}

impl From<ProgramOutOfMemory> for UserLinearMemoryError {
    fn from(error: ProgramOutOfMemory) -> Self {
        Self::OutOfMemory(error)
    }
}

unsafe impl<C: Cpu + Clone> MemoryCreator for UserMemoryCreator<C> {
    fn new_memory(
        &self,
        ty: MemoryType,
        minimum: usize,
        maximum: Option<usize>,
        reserved_size_in_bytes: Option<usize>,
        guard_size_in_bytes: usize,
    ) -> Result<Box<dyn LinearMemory>, String> {
        plan_user_linear_memory(
            ty,
            minimum,
            maximum,
            reserved_size_in_bytes,
            guard_size_in_bytes,
        )
        .and_then(|plan| plan.allocate(self.processor, self.cpu.clone()))
        .map(|memory| Box::new(memory) as Box<dyn LinearMemory>)
        .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UserLinearMemoryPlan {
    minimum: usize,
    capacity: usize,
    relocatable: bool,
}

impl UserLinearMemoryPlan {
    fn allocate<C: Cpu + Clone>(
        self,
        processor: ProcessorId,
        cpu: C,
    ) -> Result<UserLinearMemory<C>, UserLinearMemoryError> {
        UserLinearMemory::new(
            self.minimum,
            self.capacity,
            self.relocatable,
            processor,
            cpu,
        )
    }
}

fn plan_user_linear_memory(
    ty: MemoryType,
    minimum: usize,
    maximum: Option<usize>,
    reserved_size_in_bytes: Option<usize>,
    guard_size_in_bytes: usize,
) -> Result<UserLinearMemoryPlan, UserLinearMemoryError> {
    if guard_size_in_bytes != 0 {
        return Err(UserLinearMemoryError::GuardPagesUnsupported);
    }

    if reserved_size_in_bytes.is_some_and(|reserved| reserved != 0) {
        return Err(UserLinearMemoryError::ReservationUnsupported);
    }

    let (capacity, relocatable) = if ty.is_shared() {
        (
            maximum.ok_or(UserLinearMemoryError::SharedMaximumMissing)?,
            false,
        )
    } else {
        (minimum, true)
    };

    Ok(UserLinearMemoryPlan {
        minimum,
        capacity,
        relocatable,
    })
}

struct UserLinearMemory<C: Cpu + Clone> {
    ptr: NonNull<u8>,
    byte_len: usize,
    byte_capacity: usize,
    relocatable: bool,
    processor: ProcessorId,
    cpu: C,
}

// SAFETY: each `UserLinearMemory` owns a non-overlapping page-aligned
// buffer; concurrent access is gated by Wasmtime's own LinearMemory
// contract and not by this struct's internals.
unsafe impl<C: Cpu + Clone> Send for UserLinearMemory<C> {}
unsafe impl<C: Cpu + Clone> Sync for UserLinearMemory<C> {}

impl<C: Cpu + Clone> UserLinearMemory<C> {
    fn new(
        minimum: usize,
        capacity: usize,
        relocatable: bool,
        processor: ProcessorId,
        cpu: C,
    ) -> Result<Self, UserLinearMemoryError> {
        if capacity < minimum {
            return Err(UserLinearMemoryError::CapacityBelowMinimum);
        }

        if capacity == 0 {
            return Ok(Self {
                ptr: NonNull::dangling(),
                byte_len: 0,
                byte_capacity: 0,
                relocatable,
                processor,
                cpu,
            });
        }

        let layout = linear_memory_layout(capacity)?;
        let (ptr, byte_capacity) = allocate_user_uninit_on(processor, layout)?;
        // SAFETY: `ptr` is a fresh exclusive allocation of
        // `byte_capacity` writable bytes; the chosen `Cpu::zero_memory`
        // implementation (potentially `dc zva` on aarch64) clears the
        // whole region before any user code observes it.
        unsafe {
            cpu.zero_memory(ptr, byte_capacity);
        }
        Ok(Self {
            ptr,
            byte_len: minimum,
            byte_capacity,
            relocatable,
            processor,
            cpu,
        })
    }
}

unsafe impl<C: Cpu + Clone> LinearMemory for UserLinearMemory<C> {
    fn byte_size(&self) -> usize {
        self.byte_len
    }

    fn byte_capacity(&self) -> usize {
        self.byte_capacity
    }

    fn grow_to(&mut self, new_size: usize) -> wasmtime::Result<()> {
        if new_size <= self.byte_capacity {
            self.byte_len = new_size;
            return Ok(());
        }
        if !self.relocatable {
            return Err(wasmtime::Error::from(ProgramOutOfMemory {
                requested_bytes: new_size,
                available_bytes: 0,
                reserved_bytes: self.byte_capacity,
            }));
        }

        let new_layout = linear_memory_layout(new_size).map_err(wasmtime::Error::new)?;
        let (new_ptr, new_capacity) =
            allocate_user_uninit_on(self.processor, new_layout).map_err(wasmtime::Error::from)?;
        // SAFETY: `new_ptr` is a fresh exclusive allocation of
        // `new_capacity` writable bytes; we will overwrite the prefix
        // with the previous contents below, so only the tail truly
        // needs zeroing — but driving `Cpu::zero_memory` over the
        // whole region keeps the contract symmetric with `new_memory`
        // and preserves the user-OOM invariant that fresh wasm memory
        // is observed as zeros.
        unsafe {
            self.cpu.zero_memory(new_ptr, new_capacity);
        }
        if self.byte_len != 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(self.ptr.as_ptr(), new_ptr.as_ptr(), self.byte_len);
            }
            deallocate_user_on(
                self.processor,
                self.ptr,
                linear_memory_layout(self.byte_capacity).unwrap_or_else(|error| {
                    panic!("invalid user linear memory layout during grow: {error}")
                }),
            );
        }
        self.ptr = new_ptr;
        self.byte_len = new_size;
        self.byte_capacity = new_capacity;
        Ok(())
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl<C: Cpu + Clone> Drop for UserLinearMemory<C> {
    fn drop(&mut self) {
        if self.byte_capacity != 0 {
            deallocate_user_on(
                self.processor,
                self.ptr,
                linear_memory_layout(self.byte_capacity).unwrap_or_else(|error| {
                    panic!("invalid user linear memory layout during drop: {error}")
                }),
            );
        }
    }
}

fn linear_memory_layout(size: usize) -> Result<Layout, UserLinearMemoryError> {
    Layout::from_size_align(size, LINEAR_MEMORY_ALIGNMENT)
        .map_err(|_| UserLinearMemoryError::InvalidLayout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_memory_planning_preserves_structured_errors() {
        let memory = MemoryType::new(1, None);

        assert_eq!(
            plan_user_linear_memory(memory.clone(), 1, None, None, LINEAR_MEMORY_ALIGNMENT),
            Err(UserLinearMemoryError::GuardPagesUnsupported)
        );
        assert_eq!(
            plan_user_linear_memory(memory, 1, None, Some(LINEAR_MEMORY_ALIGNMENT), 0),
            Err(UserLinearMemoryError::ReservationUnsupported)
        );
        assert_eq!(
            plan_user_linear_memory(MemoryType::shared(1, 1), 1, None, None, 0),
            Err(UserLinearMemoryError::SharedMaximumMissing)
        );
    }

    #[test]
    fn user_memory_plan_keeps_shared_memory_fixed_capacity() {
        assert_eq!(
            plan_user_linear_memory(
                MemoryType::shared(1, 2),
                LINEAR_MEMORY_ALIGNMENT,
                Some(LINEAR_MEMORY_ALIGNMENT * 2),
                None,
                0,
            )
            .expect("shared memory plan must be valid"),
            UserLinearMemoryPlan {
                minimum: LINEAR_MEMORY_ALIGNMENT,
                capacity: LINEAR_MEMORY_ALIGNMENT * 2,
                relocatable: false,
            }
        );
    }
}
