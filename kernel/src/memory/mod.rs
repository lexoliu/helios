//! Kernel-side memory management.
//!
//! `pmm` exposes the kernel's physical-frame allocator wrapper.
//! `user` carries the per-program user-memory pool used by Wasmtime
//! linear memories. `frame_slab` is the per-processor frame cache
//! that backs both. `entropy` keeps the entropy pool used to seed
//! ASLR / TCP ISN, etc. `reservations` is the AddressSpace
//! reservation/committed-region bookkeeping shared by every backend.

mod entropy;
mod frame_slab;
mod pmm;
mod reservations;
mod user;

pub use entropy::{EntropyError, EntropyPool};
pub use pmm::KernelPhysFrameAllocator;
pub use reservations::{
    AccessibilityPlan, CommittedRegion, ReservationLookup, ReservationTracker, VaCursor,
    validate_range,
};
pub use user::{
    UserHeapStats, UserMemoryPool, allocate_user_frame_uninit_on, allocate_user_frame_zeroed,
    allocate_user_frame_zeroed_on, deallocate_user_frame, deallocate_user_frame_on,
    user_heap_stats,
};
pub(crate) use user::{
    allocate_user_memory_pool, allocate_user_uninit_on, deallocate_user_on,
    install_user_memory_pool,
};
