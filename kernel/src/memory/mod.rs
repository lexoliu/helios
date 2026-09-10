//! Kernel-side memory management.
//!
//! `irq_safe` carries the lock every allocator here is built on: an
//! interrupt handler allocates, so the lock that guards an allocator
//! has to mask interrupts while it is held.
//! `pmm` exposes the kernel's physical-frame allocator wrapper.
//! `fiber_stack` owns the arena the runtime's async stacks are committed
//! out of one page-fault at a time, and `frame_reserve` holds the frames
//! that fault path is allowed to allocate.
//! `user` carries the per-program user-memory pool used by Wasmtime
//! linear memories. `frame_slab` is the per-processor frame cache
//! that backs both. `entropy` owns the boot-seeded root DRBG and the
//! per-instance pools derived from it. `reservations` is the AddressSpace
//! reservation/committed-region bookkeeping shared by every backend,
//! `mapping_cost` says what mapping user address space costs the kernel
//! heap that describes it, and `policy` states — once, for every
//! backend — how the boot memory map is divided between the two
//! domains.

mod balloon;
mod entropy;
mod fiber_stack;
mod frame_pool;
mod frame_reserve;
mod frame_slab;
mod irq_safe;
mod mapping_cost;
mod owner;
mod pmm;
mod policy;
mod reported;
mod reservations;
mod swap;
mod user;

pub use balloon::{BalloonHandle, BalloonStats, FREE_PAGE_REPORT_INTERVAL, install_memory_balloon};
pub use entropy::{
    ENTROPY_RESEED_INTERVAL, EntropyPool, EntropySources, HardwareEntropySource,
    NoCryptographicEntropy, NoEntropyDevice, ROOT_ENTROPY_MATERIAL_BYTES, RootEntropy,
    RootEntropyHandle, install_entropy_device, seed_root_entropy,
};
pub use fiber_stack::{
    FIBER_STACK_GUARD_BYTES, FiberStack, FiberStackArenaStats, FiberStackError, FiberStackVmHooks,
    StackFault, claim_fiber_stack, fiber_stack_arena_stats, fiber_stack_demand_commits_on,
    install_fiber_stack_arena, install_fiber_stack_hooks, resolve_stack_fault,
};
pub use frame_reserve::reserved_bytes as page_fault_frame_reserve_bytes;
pub(crate) use frame_reserve::{
    configure_processors as configure_frame_reserve_processors, top_up as top_up_frame_reserve,
};
pub(crate) use irq_safe::IrqSafeMutex;
pub use mapping_cost::user_mapping_kernel_heap_bytes;
pub use owner::{
    MemoryOwner, UserMemoryOwnerScope, UserMemoryOwners, configure_user_memory_owner_processors,
    current_user_memory_owner, enter_user_memory_owner, set_user_memory_owner,
};
pub use pmm::KernelPhysFrameAllocator;
pub use policy::{
    BootMemoryPlan, BootRegionSplitter, KERNEL_HEAP_BOOTSTRAP_BYTES,
    KERNEL_HEAP_GROWTH_CHUNK_BYTES, KERNEL_HEAP_MAX_BOOT_FRACTION, KERNEL_HEAP_MIN_RESERVE_BYTES,
    KERNEL_HEAP_RESERVE_FRACTION, RegionShares, TASK_ARENA_FRACTION, TASK_ARENA_MIN_BYTES,
    USER_POOL_MIN_REGION_BYTES, kernel_reserve_for, task_arena_bytes_for,
};
pub use reservations::{
    AccessibilityPlan, CommittedRegion, ReleasedReservation, ReservationLookup, ReservationTracker,
    SwapEntry, VaCursor, validate_range,
};
pub use swap::{
    IDLE_SWAP_AFTER, SWAP_BATCH_BYTES, SWAP_TICK, SwapDisabled, SwapFaultError, SwapHandle,
    SwapStats, SwapVmHooks, disable_swap, install_swap, install_swap_hooks, installed_swap_handle,
    installed_swap_hooks, swapped_token,
};
pub use user::{
    UserHeapStats, UserMemoryPool, allocate_user_frame_uninit_on, allocate_user_frame_zeroed,
    allocate_user_frame_zeroed_on, allocate_user_run_zeroed_on, deallocate_user_frame,
    deallocate_user_frame_on, deallocate_user_run_on, largest_servable_user_bytes, user_heap_stats,
};
pub(crate) use user::{
    allocate_user_memory_pool, install_user_memory_pool, lend_user_memory_to_kernel_heap,
    user_pool_available_bytes,
};
