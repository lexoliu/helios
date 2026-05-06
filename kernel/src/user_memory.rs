extern crate alloc;

use alloc::boxed::Box;
use core::alloc::Layout;
use core::mem::size_of;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use buddy_system_allocator::LockedHeap;
use helios_hal::cpu::ProcessorId;
use helios_hal::pmm::PhysFrame;

use crate::ProgramOutOfMemory;
use crate::frame_slab::FrameSlabCache;

const USER_HEAP_ORDER: usize = 32;

pub struct UserMemoryPool {
    heap: LockedHeap<USER_HEAP_ORDER>,
    frame_slab: FrameSlabCache,
    total_bytes: AtomicUsize,
}

impl UserMemoryPool {
    pub const fn empty() -> Self {
        Self {
            heap: LockedHeap::empty(),
            frame_slab: FrameSlabCache::new(),
            total_bytes: AtomicUsize::new(0),
        }
    }

    pub fn add_region(&self, start: usize, end: usize) {
        if end <= start {
            return;
        }
        unsafe {
            self.heap.lock().add_to_heap(start, end);
        }
        self.total_bytes.fetch_add(end - start, Ordering::Release);
    }

    pub fn configure_processors(&self, processor_count: usize) {
        self.frame_slab.configure_processors(processor_count);
    }

    pub fn stats(&self) -> UserHeapStats {
        let allocator = self.heap.lock();
        let cached = self.frame_slab.cached_bytes();
        UserHeapStats {
            total_bytes: allocator.stats_total_bytes(),
            allocated_bytes: allocator.stats_alloc_actual().saturating_sub(cached),
        }
    }

    pub fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<u8>, ProgramOutOfMemory> {
        self.allocate_zeroed_with_processor(layout, None)
    }

    pub fn allocate_zeroed_on(
        &self,
        processor: ProcessorId,
        layout: Layout,
    ) -> Result<NonNull<u8>, ProgramOutOfMemory> {
        self.allocate_zeroed_with_processor(layout, Some(processor))
    }

    fn allocate_zeroed_with_processor(
        &self,
        layout: Layout,
        processor: Option<ProcessorId>,
    ) -> Result<NonNull<u8>, ProgramOutOfMemory> {
        let allocation_size = buddy_allocation_size(layout);
        if is_single_frame_layout(layout) {
            let ptr = match processor {
                Some(processor) => self.frame_slab.allocate_on(processor),
                None => self.frame_slab.allocate(),
            };
            if let Some(ptr) = ptr {
                unsafe {
                    core::ptr::write_bytes(ptr.as_ptr(), 0, PhysFrame::SIZE);
                }
                return Ok(ptr);
            }
        }

        let mut allocator = self.heap.lock();
        let ptr = allocator
            .alloc(layout)
            .or_else(|_| {
                self.frame_slab.drain(|ptr| unsafe {
                    allocator.dealloc(ptr, single_frame_layout());
                });
                allocator.alloc(layout)
            })
            .map_err(|_| {
                let stats = UserHeapStats {
                    total_bytes: allocator.stats_total_bytes(),
                    allocated_bytes: allocator
                        .stats_alloc_actual()
                        .saturating_sub(self.frame_slab.cached_bytes()),
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
        self.deallocate_with_processor(ptr, layout, None);
    }

    pub fn deallocate_on(&self, processor: ProcessorId, ptr: NonNull<u8>, layout: Layout) {
        self.deallocate_with_processor(ptr, layout, Some(processor));
    }

    fn deallocate_with_processor(
        &self,
        ptr: NonNull<u8>,
        layout: Layout,
        processor: Option<ProcessorId>,
    ) {
        if is_single_frame_layout(layout) {
            let total_frames = self.total_bytes.load(Ordering::Acquire) / PhysFrame::SIZE;
            let cached = match processor {
                Some(processor) => self.frame_slab.deallocate_on(processor, ptr, total_frames),
                None => self.frame_slab.deallocate(ptr, total_frames),
            };
            if cached {
                return;
            }
        }
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

pub fn allocate_user_frame_zeroed_on(
    processor: ProcessorId,
) -> Result<NonNull<u8>, ProgramOutOfMemory> {
    let layout = Layout::from_size_align(PhysFrame::SIZE, PhysFrame::SIZE)
        .unwrap_or_else(|_| panic!("invalid user-frame layout"));
    user_memory_pool().allocate_zeroed_on(processor, layout)
}

pub fn deallocate_user_frame(ptr: NonNull<u8>) {
    let layout = Layout::from_size_align(PhysFrame::SIZE, PhysFrame::SIZE)
        .unwrap_or_else(|_| panic!("invalid user-frame layout"));
    user_memory_pool().deallocate(ptr, layout);
}

pub fn deallocate_user_frame_on(processor: ProcessorId, ptr: NonNull<u8>) {
    let layout = Layout::from_size_align(PhysFrame::SIZE, PhysFrame::SIZE)
        .unwrap_or_else(|_| panic!("invalid user-frame layout"));
    user_memory_pool().deallocate_on(processor, ptr, layout);
}

pub(crate) fn allocate_user_zeroed_on(
    processor: ProcessorId,
    layout: Layout,
) -> Result<NonNull<u8>, ProgramOutOfMemory> {
    user_memory_pool().allocate_zeroed_on(processor, layout)
}

pub(crate) fn deallocate_user_on(processor: ProcessorId, ptr: NonNull<u8>, layout: Layout) {
    user_memory_pool().deallocate_on(processor, ptr, layout);
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

fn is_single_frame_layout(layout: Layout) -> bool {
    layout.size() == PhysFrame::SIZE && layout.align() == PhysFrame::SIZE
}

fn single_frame_layout() -> Layout {
    Layout::from_size_align(PhysFrame::SIZE, PhysFrame::SIZE)
        .unwrap_or_else(|_| panic!("invalid user-frame layout"))
}
