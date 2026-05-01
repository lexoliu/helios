extern crate alloc;

use alloc::boxed::Box;
use core::alloc::Layout;
use core::mem::size_of;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

use buddy_system_allocator::LockedHeap;
use helios_hal::pmm::PhysFrame;

use crate::ProgramOutOfMemory;

const USER_HEAP_ORDER: usize = 32;

pub struct UserMemoryPool {
    heap: LockedHeap<USER_HEAP_ORDER>,
}

impl UserMemoryPool {
    pub const fn empty() -> Self {
        Self {
            heap: LockedHeap::empty(),
        }
    }

    pub fn add_region(&self, start: usize, end: usize) {
        if end <= start {
            return;
        }
        unsafe {
            self.heap.lock().add_to_heap(start, end);
        }
    }

    pub fn stats(&self) -> UserHeapStats {
        let allocator = self.heap.lock();
        UserHeapStats {
            total_bytes: allocator.stats_total_bytes(),
            allocated_bytes: allocator.stats_alloc_actual(),
        }
    }

    pub fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<u8>, ProgramOutOfMemory> {
        let allocation_size = buddy_allocation_size(layout);
        let mut allocator = self.heap.lock();
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

    pub fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        unsafe {
            self.heap.lock().dealloc(ptr, layout);
        }
    }
}

static USER_MEMORY_POOL: AtomicPtr<UserMemoryPool> = AtomicPtr::new(core::ptr::null_mut());

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

pub fn install_user_memory_pool(pool: &'static UserMemoryPool) -> &'static UserMemoryPool {
    USER_MEMORY_POOL
        .compare_exchange(
            core::ptr::null_mut(),
            pool as *const _ as *mut _,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .unwrap_or_else(|_| panic!("user memory pool was installed more than once"));
    pool
}

pub fn allocate_user_memory_pool() -> &'static UserMemoryPool {
    Box::leak(Box::new(UserMemoryPool::empty()))
}

pub fn installed_user_memory_pool() -> Option<&'static UserMemoryPool> {
    let ptr = USER_MEMORY_POOL.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &*ptr })
    }
}

fn user_memory_pool() -> &'static UserMemoryPool {
    installed_user_memory_pool()
        .unwrap_or_else(|| panic!("user memory pool accessed before bootstrap installation"))
}

pub fn user_heap_stats() -> UserHeapStats {
    user_memory_pool().stats()
}

pub fn allocate_user_frame_zeroed() -> Result<NonNull<u8>, ProgramOutOfMemory> {
    let layout = Layout::from_size_align(PhysFrame::SIZE, PhysFrame::SIZE)
        .unwrap_or_else(|_| panic!("invalid user-frame layout"));
    user_memory_pool().allocate_zeroed(layout)
}

pub fn deallocate_user_frame(ptr: NonNull<u8>) {
    let layout = Layout::from_size_align(PhysFrame::SIZE, PhysFrame::SIZE)
        .unwrap_or_else(|_| panic!("invalid user-frame layout"));
    user_memory_pool().deallocate(ptr, layout);
}

pub(crate) fn allocate_user_zeroed(layout: Layout) -> Result<NonNull<u8>, ProgramOutOfMemory> {
    user_memory_pool().allocate_zeroed(layout)
}

pub(crate) fn deallocate_user(ptr: NonNull<u8>, layout: Layout) {
    user_memory_pool().deallocate(ptr, layout);
}

pub(crate) fn user_memory_allocation_size(layout: Layout) -> usize {
    buddy_allocation_size(layout)
}

fn buddy_allocation_size(layout: Layout) -> usize {
    layout
        .size()
        .next_power_of_two()
        .max(layout.align())
        .max(size_of::<usize>())
}
