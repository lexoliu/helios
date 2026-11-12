//! A user-memory pool for the tests that build kernel tables out of one.
//!
//! The kernel's per-processor tables are carved from a pool's node
//! memory, so a test that configures one needs a pool to name even when
//! it never allocates from it.

#![cfg(test)]

use std::alloc::{Layout, alloc};

use helios_hal::cpu::ProcessorId;
use helios_hal::numa::{NodeId, SingleNode};
use helios_kernel::{NodeRun, UserMemoryPool};

/// Host memory the pool owns: room for a few processors' tables.
const POOL_BYTES: usize = 16 << 20;

/// A leaked one-processor pool over fresh host memory, as a test
/// process's machine is.
pub(crate) fn single_processor_pool() -> &'static UserMemoryPool {
    let layout = Layout::from_size_align(POOL_BYTES, POOL_BYTES).expect("a pool-sized layout");
    // SAFETY: the layout has a non-zero size; the block is leaked, which
    // is the life the pool's memory has.
    let start = unsafe { alloc(layout) } as usize;
    assert!(start != 0, "the host would not lend {POOL_BYTES} bytes");
    Box::leak(Box::new(UserMemoryPool::new(
        &SingleNode::new(1),
        ProcessorId::new(0),
        &[NodeRun {
            start,
            end: start + POOL_BYTES,
            node: NodeId::FIRST,
        }],
    )))
}
