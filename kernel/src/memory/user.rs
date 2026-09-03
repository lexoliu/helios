extern crate alloc;

use alloc::boxed::Box;
use core::alloc::Layout;
use core::future::Future;
use core::mem::size_of;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use buddy_system_allocator::LockedHeap;
use helios_hal::cpu::ProcessorId;
use helios_hal::pmm::{
    FrameAllocError, FrameAllocStats, PhysFrame, PhysFrameAllocator, PhysFrameRange,
};

use crate::ProgramOutOfMemory;
use crate::memory::frame_slab::FrameSlabCache;
use crate::memory::reported::{ReportedFrames, visit_free_runs};

const USER_HEAP_ORDER: usize = 32;

pub struct UserMemoryPool {
    heap: LockedHeap<USER_HEAP_ORDER>,
    frame_slab: FrameSlabCache,
    total_bytes: AtomicUsize,
    /// Frames shown to a free-page consumer. The memory is still the
    /// pool's to hand out; its contents are not.
    reported: ReportedFrames,
}

impl UserMemoryPool {
    pub const fn empty() -> Self {
        Self {
            heap: LockedHeap::empty(),
            frame_slab: FrameSlabCache::new(),
            total_bytes: AtomicUsize::new(0),
            reported: ReportedFrames::new(),
        }
    }

    pub fn add_region(&self, start: usize, end: usize) {
        if end <= start {
            return;
        }
        unsafe {
            self.heap.lock().add_to_heap(start, end);
        }
        self.reported.cover(start, end);
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
        let (ptr, size) = self.allocate_raw(layout, processor)?;
        // Memory the pool showed to a free-page consumer may have been
        // discarded by it. The uninit contract lets a caller skip
        // zeroing what it is about to overwrite; it does not let the
        // pool hand back a hole.
        if self.reported.take_bytes(ptr.as_ptr() as usize, size) {
            unsafe {
                core::ptr::write_bytes(ptr.as_ptr(), 0, size);
            }
        }
        Ok((ptr, size))
    }

    /// The largest block the pool will actually hand out right now, at most
    /// `limit` bytes.
    ///
    /// [`UserHeapStats::largest_allocatable_bytes`] is a granularity bound
    /// derived from the free byte count alone: it answers what the allocator
    /// could serve if its free space were one intact buddy. It usually is not.
    /// A pool with 1000 MiB free reports 512 MiB and then refuses the 512 MiB
    /// request, because one earlier allocation landing inside that buddy split
    /// it for good — issue #62, where a spawning program sized its shared
    /// memory from the bound and died on the allocation that followed.
    ///
    /// The buddy heap does not publish its free lists, so the only honest
    /// answer is the one it gives when asked. This takes a candidate block and
    /// releases it immediately, halving until one is served. What it returns is
    /// a size, not a reservation: the caller still races every other processor
    /// for it, so callers keep handling [`ProgramOutOfMemory`].
    pub fn largest_servable_bytes(&self, limit: usize) -> usize {
        let mut candidate = self.stats().largest_allocatable_bytes().min(limit);
        // The buddy heap rounds every request up to a power of two, so only
        // powers of two are worth probing.
        candidate = previous_power_of_two(candidate);
        while candidate >= PhysFrame::SIZE {
            let layout = Layout::from_size_align(candidate, PhysFrame::SIZE)
                .unwrap_or_else(|_| panic!("invalid probe layout for {candidate} bytes"));
            // The probe deliberately skips the reported-frame check and the
            // zeroing it can trigger: nothing reads this block, and paying to
            // zero half a gigabyte to answer a question would be absurd.
            if let Ok((ptr, size)) = self.allocate_raw(layout, None) {
                self.deallocate_with_processor(ptr, layout, None);
                debug_assert_eq!(size, candidate);
                return candidate;
            }
            candidate /= 2;
        }
        0
    }

    /// The pool's allocation path without the reported-frame check.
    ///
    /// Free-page reporting takes runs out of the pool only to hand them
    /// straight back, and must not pay to zero them on the way through.
    fn allocate_raw(
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

    /// Returns a byte allocation to the pool.
    ///
    /// Named apart from the frame-allocator contract's `deallocate`,
    /// which takes a frame run: one pool answers both, and a reader
    /// should not have to work out which is which from the argument
    /// list.
    pub fn deallocate_bytes(&self, ptr: NonNull<u8>, layout: Layout) {
        self.deallocate_with_processor(ptr, layout, None);
    }

    pub fn deallocate_bytes_on(&self, processor: ProcessorId, ptr: NonNull<u8>, layout: Layout) {
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

/// The pool as a frame allocator.
///
/// User memory is the guest's bulk memory: it is where wasm linear
/// memories live, it is page-granular, and running it dry costs an
/// instance rather than the kernel. That makes it the pool a memory
/// balloon draws from — the kernel heap's exhaustion is fatal, so it is
/// not something a host may ask the guest to give away.
impl PhysFrameAllocator for UserMemoryPool {
    fn allocate(
        &self,
        count: usize,
        zero_first_use: bool,
    ) -> Result<PhysFrameRange, FrameAllocError> {
        if count == 0 {
            return Err(FrameAllocError::OutOfFrames {
                requested: 0,
                available: 0,
            });
        }
        let (ptr, size) = self
            .allocate_uninit_with_processor(frame_layout(count), None)
            .map_err(|error| FrameAllocError::OutOfFrames {
                requested: count,
                available: error.available_bytes / PhysFrame::SIZE,
            })?;
        if zero_first_use {
            unsafe {
                core::ptr::write_bytes(ptr.as_ptr(), 0, size);
            }
        }
        Ok(PhysFrameRange::from_phys_addr(
            ptr.as_ptr() as usize,
            count * PhysFrame::SIZE,
        ))
    }

    fn free_runs<Visit, Visited>(
        &self,
        min_frames: usize,
        max_runs: usize,
        visit: Visit,
    ) -> impl Future<Output = usize> + Send
    where
        Visit: FnMut(PhysFrameRange) -> Visited + Send,
        Visited: Future<Output = ()> + Send,
    {
        visit_free_runs(
            &self.reported,
            min_frames,
            max_runs,
            |frames| {
                self.allocate_raw(frame_layout(frames), None)
                    .ok()
                    .map(|(ptr, _)| {
                        PhysFrameRange::from_phys_addr(
                            ptr.as_ptr() as usize,
                            frames * PhysFrame::SIZE,
                        )
                    })
            },
            |range| PhysFrameAllocator::deallocate(self, range),
            visit,
        )
    }

    fn deallocate(&self, range: PhysFrameRange) {
        if range.is_empty() {
            return;
        }
        let ptr = NonNull::new(range.start.phys_addr() as *mut u8)
            .unwrap_or_else(|| panic!("user frame deallocator received a null range"));
        self.deallocate_with_processor(ptr, frame_layout(range.frame_count), None);
    }

    fn stats(&self) -> FrameAllocStats {
        let heap = UserMemoryPool::stats(self);
        FrameAllocStats {
            total_frames: heap.total_bytes / PhysFrame::SIZE,
            allocated_frames: heap.allocated_bytes / PhysFrame::SIZE,
            // The buddy heap surfaces no true largest-free-run; total
            // free is the upper bound, which is what the pressure policy
            // treats it as.
            largest_free_run: heap.available_bytes() / PhysFrame::SIZE,
            reported_frames: self.reported.count(),
        }
    }
}

fn frame_layout(frames: usize) -> Layout {
    let bytes = frames * PhysFrame::SIZE;
    Layout::from_size_align(bytes, PhysFrame::SIZE)
        .unwrap_or_else(|_| panic!("user frame layout overflow for {bytes} bytes"))
}

static USER_MEMORY_POOL: AtomicPtr<UserMemoryPool> = AtomicPtr::new(core::ptr::null_mut());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserHeapStats {
    pub total_bytes: usize,
    pub allocated_bytes: usize,
}

/// The largest power of two not greater than `value`, or `0` for `0`.
fn previous_power_of_two(value: usize) -> usize {
    if value == 0 {
        return 0;
    }

    1_usize << (usize::BITS - 1 - value.leading_zeros())
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
        previous_power_of_two(self.available_bytes())
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

pub(crate) fn installed_user_memory_pool() -> Option<&'static UserMemoryPool> {
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

/// See [`UserMemoryPool::largest_servable_bytes`].
pub fn largest_servable_user_bytes(limit: usize) -> usize {
    user_memory_pool().largest_servable_bytes(limit)
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
    user_memory_pool().deallocate_bytes(ptr, layout);
}

pub fn deallocate_user_frame_on(processor: ProcessorId, ptr: NonNull<u8>) {
    let layout = Layout::from_size_align(PhysFrame::SIZE, PhysFrame::SIZE)
        .unwrap_or_else(|_| panic!("invalid user-frame layout"));
    user_memory_pool().deallocate_bytes_on(processor, ptr, layout);
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
    user_memory_pool().deallocate_bytes_on(processor, ptr, layout);
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
    use futures_lite::future::block_on;

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

    /// A pool over a leaked, page-aligned block of host memory.
    fn pool(bytes: usize) -> UserMemoryPool {
        let layout = Layout::from_size_align(bytes, PhysFrame::SIZE).expect("pool layout");
        let start = unsafe { alloc::alloc::alloc(layout) } as usize;
        assert!(start != 0, "host allocation for the user pool failed");
        let pool = UserMemoryPool::empty();
        pool.add_region(start, start + bytes);
        pool
    }

    /// Asserts the pool hands out exactly what it just advertised.
    fn assert_advertises_what_it_serves(pool: &UserMemoryPool) -> usize {
        let advertised = pool.largest_servable_bytes(usize::MAX);
        assert!(advertised > 0, "a pool with free space advertises a block");
        let layout =
            Layout::from_size_align(advertised, PhysFrame::SIZE).expect("advertised layout");
        let (served, _) = pool
            .allocate_raw(layout, None)
            .expect("the pool serves the block it advertised");
        pool.deallocate_with_processor(served, layout, None);
        advertised
    }

    /// Regression for #62.
    ///
    /// A spawning program sized its shared memory from
    /// `largest_allocatable_bytes`, which reads the free byte count and
    /// answers with the power of two below it. A buddy heap serves that only
    /// while the matching buddy is intact -- which it is not over a region
    /// that is not naturally aligned, and stops being the moment anything is
    /// allocated inside it. The pool then refuses a request it had just
    /// advertised, which is what the guest reported verbatim: "program memory
    /// request of 536870912 bytes exceeds its memory budget:
    /// available=1048576000 reserved=0".
    #[test]
    fn the_granularity_bound_overstates_what_the_pool_serves() {
        let pool = pool(8 * 1024 * 1024);

        let bound = pool.stats().largest_allocatable_bytes();
        let servable = assert_advertises_what_it_serves(&pool);
        assert!(
            servable < bound,
            "the bound claims {bound} bytes but the pool serves {servable}"
        );

        // Fragmenting it further must not break the promise either.
        let frame = Layout::from_size_align(PhysFrame::SIZE, PhysFrame::SIZE).expect("frame");
        let (held, _) = pool
            .allocate_raw(frame, None)
            .expect("a fresh pool has a frame");
        let fragmented = assert_advertises_what_it_serves(&pool);
        assert!(fragmented <= servable);

        pool.deallocate_with_processor(held, frame, None);
    }

    #[test]
    fn an_exhausted_pool_advertises_nothing() {
        let pool = pool(8 * 1024 * 1024);
        let mut held = alloc::vec::Vec::new();

        // Taking the largest servable block repeatedly is the only way to
        // actually drain a buddy heap: asking for `available_bytes` in one go
        // is refused even on an untouched pool, which is the bug this whole
        // family of tests is about.
        loop {
            let servable = pool.largest_servable_bytes(usize::MAX);
            if servable == 0 {
                break;
            }
            let layout =
                Layout::from_size_align(servable, PhysFrame::SIZE).expect("servable layout");
            let (ptr, _) = pool
                .allocate_raw(layout, None)
                .expect("the pool serves the block it advertised");
            held.push((ptr, layout));
        }

        assert_eq!(pool.largest_servable_bytes(usize::MAX), 0);
        assert!(
            pool.stats().available_bytes() < PhysFrame::SIZE,
            "draining by servable blocks leaves less than a frame"
        );

        for (ptr, layout) in held {
            pool.deallocate_with_processor(ptr, layout, None);
        }
    }

    #[test]
    fn a_limit_caps_what_the_pool_advertises() {
        let pool = pool(8 * 1024 * 1024);

        assert_eq!(
            pool.largest_servable_bytes(1024 * 1024),
            1024 * 1024,
            "a caller's cap is applied before the pool is asked"
        );
        assert_eq!(
            pool.largest_servable_bytes(3 * 1024 * 1024),
            2 * 1024 * 1024,
            "a cap that is not a power of two rounds down to a buddy block"
        );
    }

    fn tail_byte(address: usize, bytes: usize) -> u8 {
        unsafe { core::ptr::read_volatile((address + bytes - 1) as *const u8) }
    }

    /// The balloon draws from user memory, so the pool has to answer the
    /// frame-allocator contract as well as the wasm allocation paths.
    #[test]
    fn the_pool_hands_out_and_takes_back_contiguous_frames() {
        let pool = pool(8 * 1024 * 1024);
        let free_before = PhysFrameAllocator::stats(&pool).free_frames();

        let range = pool.allocate(64, false).expect("64 frames");
        assert_eq!(range.frame_count, 64);
        assert!(PhysFrameAllocator::stats(&pool).free_frames() < free_before);

        PhysFrameAllocator::deallocate(&pool, range);
        assert_eq!(
            PhysFrameAllocator::stats(&pool).free_frames(),
            free_before,
            "a returned run is free again"
        );
    }

    /// A reporting pass names as much of the pool as the caller lets it,
    /// in runs of the size the caller asked for. A pass that could only
    /// ever name one run would leave a mostly idle guest holding memory
    /// its host could have back.
    #[test]
    fn a_pass_names_as_many_runs_as_the_pool_can_spare() {
        let pool = pool(64 * 1024 * 1024);
        let free_before = PhysFrameAllocator::stats(&pool).free_frames();

        let mut runs = 0usize;
        let named = block_on(pool.free_runs(512, 256, |run| {
            assert_eq!(run.frame_count, 512);
            runs += 1;
            core::future::ready(())
        }));

        assert_eq!(named, runs);
        assert!(
            runs >= 24,
            "a 64 MiB pool has more than {runs} spare 2 MiB runs"
        );
        assert_eq!(
            PhysFrameAllocator::stats(&pool).free_frames(),
            free_before,
            "a pass leaves the pool as it found it"
        );
        assert_eq!(PhysFrameAllocator::stats(&pool).reported_frames, runs * 512);
    }

    /// Reporting user memory to a host that may discard it means the
    /// next wasm instance to be handed that memory must not see a hole,
    /// even on the uninit path that normally skips zeroing.
    #[test]
    fn reported_user_memory_is_zeroed_before_it_is_handed_out_again() {
        let pool = pool(4 * 1024 * 1024);

        let mut reported = None;
        assert_eq!(
            block_on(pool.free_runs(16, 1, |run| {
                unsafe {
                    core::ptr::write_bytes(run.start.phys_addr() as *mut u8, 0xc3, run.byte_size());
                }
                reported = Some(run);
                core::future::ready(())
            })),
            1
        );
        let reported = reported.expect("one run was reported");
        assert_eq!(
            PhysFrameAllocator::stats(&pool).reported_frames,
            reported.frame_count
        );

        let layout =
            Layout::from_size_align(reported.byte_size(), PhysFrame::SIZE).expect("frame layout");
        let (ptr, size) = pool
            .allocate_uninit_with_processor(layout, None)
            .expect("the reported run is free again");
        assert_eq!(ptr.as_ptr() as usize, reported.start.phys_addr());
        assert_eq!(
            tail_byte(ptr.as_ptr() as usize, size),
            0,
            "user memory the host may have discarded is zeroed on reuse"
        );
        assert_eq!(PhysFrameAllocator::stats(&pool).reported_frames, 0);
    }
}
