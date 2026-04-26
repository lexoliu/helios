extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use core::alloc::Layout;
use core::mem::size_of;
use core::ptr::NonNull;

use buddy_system_allocator::LockedHeap;
use wasmtime::{LinearMemory, MemoryCreator, MemoryType};

use crate::ProgramOutOfMemory;

const USER_HEAP_ORDER: usize = 32;
const LINEAR_MEMORY_ALIGNMENT: usize = 64 * 1024;

static USER_ALLOCATOR: LockedHeap<USER_HEAP_ORDER> = LockedHeap::empty();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserHeapStats {
    pub total_bytes: usize,
    pub allocated_bytes: usize,
}

impl UserHeapStats {
    pub fn available_bytes(self) -> usize {
        self.total_bytes.saturating_sub(self.allocated_bytes)
    }
}

#[derive(Clone)]
pub struct UserMemoryCreator;

pub fn add_user_heap_region(start: usize, end: usize) {
    if end <= start {
        return;
    }
    unsafe {
        USER_ALLOCATOR.lock().add_to_heap(start, end);
    }
}

pub fn user_heap_stats() -> UserHeapStats {
    let allocator = USER_ALLOCATOR.lock();
    UserHeapStats {
        total_bytes: allocator.stats_total_bytes(),
        allocated_bytes: allocator.stats_alloc_actual(),
    }
}

unsafe impl MemoryCreator for UserMemoryCreator {
    fn new_memory(
        &self,
        ty: MemoryType,
        minimum: usize,
        maximum: Option<usize>,
        reserved_size_in_bytes: Option<usize>,
        guard_size_in_bytes: usize,
    ) -> Result<Box<dyn LinearMemory>, String> {
        if guard_size_in_bytes != 0 {
            return Err(format!(
                "user linear memory guard pages require a virtual-memory backed allocator, got guard_size={guard_size_in_bytes}"
            ));
        }

        if reserved_size_in_bytes.is_some_and(|reserved| reserved != 0) {
            return Err(format!(
                "user linear memory reservation requires a virtual-memory backed allocator, got reservation={}",
                reserved_size_in_bytes.unwrap_or(0)
            ));
        }

        let (capacity, relocatable) = if ty.is_shared() {
            let maximum = maximum.ok_or_else(|| {
                "shared user linear memory must declare a maximum size".to_string()
            })?;
            (maximum, false)
        } else {
            (minimum, true)
        };

        UserLinearMemory::new(minimum, capacity, relocatable)
            .map(|memory| Box::new(memory) as Box<dyn LinearMemory>)
    }
}

struct UserLinearMemory {
    ptr: NonNull<u8>,
    byte_len: usize,
    byte_capacity: usize,
    relocatable: bool,
}

unsafe impl Send for UserLinearMemory {}
unsafe impl Sync for UserLinearMemory {}

impl UserLinearMemory {
    fn new(minimum: usize, capacity: usize, relocatable: bool) -> Result<Self, String> {
        if capacity < minimum {
            return Err(format!(
                "user linear memory capacity {capacity} is smaller than minimum {minimum}"
            ));
        }

        if capacity == 0 {
            return Ok(Self {
                ptr: NonNull::dangling(),
                byte_len: 0,
                byte_capacity: 0,
                relocatable,
            });
        }

        let layout = linear_memory_layout(capacity)?;
        let ptr = allocate_user_zeroed(layout).map_err(|error| error.to_string())?;
        let byte_capacity = buddy_allocation_size(layout);
        Ok(Self {
            ptr,
            byte_len: minimum,
            byte_capacity,
            relocatable,
        })
    }
}

unsafe impl LinearMemory for UserLinearMemory {
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

        let new_layout =
            linear_memory_layout(new_size).map_err(|error| wasmtime::format_err!("{error}"))?;
        let new_ptr = allocate_user_zeroed(new_layout).map_err(wasmtime::Error::from)?;
        let new_capacity = buddy_allocation_size(new_layout);
        if self.byte_len != 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(self.ptr.as_ptr(), new_ptr.as_ptr(), self.byte_len);
            }
            deallocate_user(self.ptr, self.byte_capacity);
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

impl Drop for UserLinearMemory {
    fn drop(&mut self) {
        if self.byte_capacity != 0 {
            deallocate_user(self.ptr, self.byte_capacity);
        }
    }
}

fn allocate_user_zeroed(layout: Layout) -> Result<NonNull<u8>, ProgramOutOfMemory> {
    let allocation_size = buddy_allocation_size(layout);
    let mut allocator = USER_ALLOCATOR.lock();
    let ptr = allocator.alloc(layout).map_err(|_| {
        let stats = UserHeapStats {
            total_bytes: allocator.stats_total_bytes(),
            allocated_bytes: allocator.stats_alloc_actual(),
        };
        ProgramOutOfMemory {
            requested_bytes: allocation_size,
            available_bytes: stats.available_bytes(),
            reserved_bytes: 0,
        }
    })?;
    unsafe {
        core::ptr::write_bytes(ptr.as_ptr(), 0, allocation_size);
    }
    Ok(ptr)
}

fn deallocate_user(ptr: NonNull<u8>, size: usize) {
    let layout = linear_memory_layout(size).unwrap_or_else(|error| {
        panic!("invalid user linear memory layout during dealloc: {error}")
    });
    unsafe {
        USER_ALLOCATOR.lock().dealloc(ptr, layout);
    }
}

fn linear_memory_layout(size: usize) -> Result<Layout, String> {
    Layout::from_size_align(size, LINEAR_MEMORY_ALIGNMENT).map_err(|error| {
        format!(
            "invalid user linear memory layout size={size} align={LINEAR_MEMORY_ALIGNMENT}: {error}"
        )
    })
}

fn buddy_allocation_size(layout: Layout) -> usize {
    layout
        .size()
        .next_power_of_two()
        .max(layout.align())
        .max(size_of::<usize>())
}
