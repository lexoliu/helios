//! Kernel-side memory management.
//!
//! `pmm` exposes the kernel's physical-frame allocator wrapper.
//! `user` carries the per-program user-memory pool used by Wasmtime
//! linear memories. `frame_slab` is the per-processor frame cache
//! that backs both. `entropy` owns the boot-seeded root DRBG and the
//! per-instance pools derived from it. `reservations` is the AddressSpace
//! reservation/committed-region bookkeeping shared by every backend, and
//! `mapping_cost` says what mapping user address space costs the kernel
//! heap that describes it.

mod balloon;
mod entropy;
mod frame_slab;
mod mapping_cost;
mod owner;
mod pmm;
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
pub use mapping_cost::user_mapping_kernel_heap_bytes;
pub use owner::{
    MemoryOwner, UserMemoryOwnerScope, UserMemoryOwners, configure_user_memory_owner_processors,
    current_user_memory_owner, enter_user_memory_owner, set_user_memory_owner,
};
pub use pmm::KernelPhysFrameAllocator;
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
    allocate_user_frame_zeroed_on, deallocate_user_frame, deallocate_user_frame_on,
    largest_servable_user_bytes, user_heap_stats,
};
pub(crate) use user::{allocate_user_memory_pool, install_user_memory_pool};
