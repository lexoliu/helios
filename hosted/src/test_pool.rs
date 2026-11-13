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

/// A one-node pool for `processors` processors over fresh host memory,
/// primed by processor 0, as a test process's machine is. The memory is
/// leaked, which is the life a pool's memory has; the pool itself is the
/// caller's to leak or install.
pub(crate) fn single_node_machine(processors: usize) -> UserMemoryPool {
    let layout = Layout::from_size_align(POOL_BYTES, POOL_BYTES).expect("a pool-sized layout");
    // SAFETY: the layout has a non-zero size; the block is leaked.
    let start = unsafe { alloc(layout) } as usize;
    assert!(start != 0, "the host would not lend {POOL_BYTES} bytes");
    UserMemoryPool::new(
        &SingleNode::new(processors),
        ProcessorId::new(0),
        &[NodeRun {
            start,
            end: start + POOL_BYTES,
            node: NodeId::FIRST,
        }],
    )
}

/// A leaked [`single_node_machine`], for a test that names a pool
/// without installing it as the machine's.
pub(crate) fn single_node_pool(processors: usize) -> &'static UserMemoryPool {
    Box::leak(Box::new(single_node_machine(processors)))
}
