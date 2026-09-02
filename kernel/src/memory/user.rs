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
use crate::memory::frame_slab::FrameSlabCache;

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
        let (ptr, size) = self.allocate_uninit_with_processor(layout, None)?;
        unsafe {
            core::ptr::write_bytes(ptr.as_ptr(), 0, size);
        }
        Ok(ptr)
    }

    pub fn allocate_zeroed_on(
        &self,
        processor: ProcessorId,
        layout: Layout,
    ) -> Result<NonNull<u8>, ProgramOutOfMemory> {
        let (ptr, size) = self.allocate_uninit_with_processor(layout, Some(processor))?;
        unsafe {
            core::ptr::write_bytes(ptr.as_ptr(), 0, size);
        }
        Ok(ptr)
    }

    /// Allocates `layout` bytes of user memory without zero-filling.
    ///
    /// Returns the allocation pointer and the actual byte count
    /// produced by the buddy allocator (rounded up to its block size).
    /// The caller must zero or fully initialise the returned region
    /// before exposing it to user code; the architecture-specific zero
    /// path (for example AArch64 `dc zva` via `Cpu::zero_memory`)
    /// applies here.
    pub fn allocate_uninit_on(
        &self,
        processor: ProcessorId,
        layout: Layout,
    ) -> Result<(NonNull<u8>, usize), ProgramOutOfMemory> {
        self.allocate_uninit_with_processor(layout, Some(processor))
    }

    fn allocate_uninit_with_processor(
        &self,
        layout: Layout,
        processor: Option<ProcessorId>,
    ) -> Result<(NonNull<u8>, usize), ProgramOutOfMemory> {
        let allocation_size = buddy_allocation_size(layout);
        if is_single_frame_layout(layout) {
            let ptr = match processor {
                Some(processor) => self.frame_slab.allocate_on(processor),
                None => self.frame_slab.allocate(),
            };
            if let Some(ptr) = ptr {
                return Ok((ptr, PhysFrame::SIZE));
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
        Ok((ptr, allocation_size))
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

    /// The largest single allocation the pool can still be asked for.
    ///
    /// [`buddy_allocation_size`] rounds every request up to a power of
    /// two, so a request for exactly [`Self::available_bytes`] is refused
    /// whenever the free byte count is not itself a power of two: the
    /// allocator asks its buddy heap for the next power of two above it,
    /// which by definition exceeds what is free. Callers that size an
    /// allocation from whatever is left of the pool must clamp to this
    /// value; clamping to `available_bytes` computes a budget the
    /// allocator can never hand out.
    ///
    /// This is the allocator's granularity bound, not a promise: a buddy
    /// heap can still refuse a block this large when its free space is
    /// fragmented across smaller buddies, so callers keep handling
    /// [`ProgramOutOfMemory`].
    pub fn largest_allocatable_bytes(self) -> usize {
        let available = self.available_bytes();
        if available == 0 {
            return 0;
        }
        1_usize << (usize::BITS - 1 - available.leading_zeros())
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

/// Allocates a single user-mode physical frame without zero-filling.
///
/// The returned pointer is suitable for the caller's own zero path —
/// in particular AArch64 backends use `dc zva` via
/// [`helios_hal::cpu::Cpu::zero_memory`] which is several times faster
/// than the generic memset that [`allocate_user_frame_zeroed_on`]
/// performs internally. Callers must zero or otherwise initialise the
/// frame before exposing it to user code.
pub fn allocate_user_frame_uninit_on(
    processor: ProcessorId,
) -> Result<NonNull<u8>, ProgramOutOfMemory> {
    let layout = Layout::from_size_align(PhysFrame::SIZE, PhysFrame::SIZE)
        .unwrap_or_else(|_| panic!("invalid user-frame layout"));
    user_memory_pool()
        .allocate_uninit_on(processor, layout)
        .map(|(ptr, _)| ptr)
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

/// Kernel-internal uninit allocator counterpart used by the wasmtime
/// adapter. Returns the buddy-rounded byte size alongside the pointer
/// so the caller can drive an architecture-specific zero
/// (`Cpu::zero_memory`) over the actual allocation rather than the
/// requested layout size.
pub(crate) fn allocate_user_uninit_on(
    processor: ProcessorId,
    layout: Layout,
) -> Result<(NonNull<u8>, usize), ProgramOutOfMemory> {
    user_memory_pool().allocate_uninit_on(processor, layout)
}

pub(crate) fn deallocate_user_on(processor: ProcessorId, ptr: NonNull<u8>, layout: Layout) {
    user_memory_pool().deallocate_on(processor, ptr, layout);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(total: usize, allocated: usize) -> UserHeapStats {
        UserHeapStats {
            total_bytes: total,
            allocated_bytes: allocated,
        }
    }

    #[test]
    fn largest_allocatable_rounds_free_space_down_to_a_buddy_block() {
        // 488 MiB free: the buddy allocator would round a request for all
        // of it up to 512 MiB and refuse, so the usable request is 256 MiB.
        let free = 511_705_088;
        let stats = stats(1 << 30, (1 << 30) - free);
        assert_eq!(stats.available_bytes(), free);
        assert_eq!(stats.largest_allocatable_bytes(), 256 * 1024 * 1024);
        assert_eq!(
            buddy_allocation_size(
                Layout::from_size_align(stats.largest_allocatable_bytes(), 64 * 1024)
                    .expect("power-of-two layout is valid")
            ),
            stats.largest_allocatable_bytes(),
        );
    }

    #[test]
    fn largest_allocatable_keeps_an_exact_power_of_two() {
        let stats = stats(1 << 30, 1 << 29);
        assert_eq!(stats.largest_allocatable_bytes(), 1 << 29);
    }

    #[test]
    fn largest_allocatable_is_zero_for_an_exhausted_pool() {
        let stats = stats(1 << 30, 1 << 30);
        assert_eq!(stats.largest_allocatable_bytes(), 0);
    }
}
