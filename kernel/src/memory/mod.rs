//! Kernel-side memory management.
//!
//! `pmm` exposes the kernel's physical-frame allocator wrapper.
//! `user` carries the per-program user-memory pool used by Wasmtime
//! linear memories. `frame_slab` is the per-processor frame cache
//! that backs both. `entropy` keeps the entropy pool used to seed
//! ASLR / TCP ISN, etc.

mod entropy;
mod frame_slab;
mod pmm;
mod user;

pub use entropy::{EntropyError, EntropyPool};
pub use pmm::KernelPhysFrameAllocator;
pub use user::{
    UserHeapStats, UserMemoryPool, allocate_user_frame_uninit_on, allocate_user_frame_zeroed,
    allocate_user_frame_zeroed_on, deallocate_user_frame, deallocate_user_frame_on,
    user_heap_stats,
};
pub(crate) use user::{
    allocate_user_memory_pool, allocate_user_uninit_on, deallocate_user_on,
    install_user_memory_pool,
};
