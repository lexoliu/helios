//! The runtime's fiber stacks, served out of the kernel's arena.
//!
//! Wasmtime's generic stack pool has no `mmap` behind it: without a
//! creator it takes every async fiber stack from the global allocator,
//! fully committed, with no guard page. `Config::with_host_stack`
//! installs the creator below instead, and the pool then asks the kernel
//! for each stack while keeping its own live-stack accounting.
//!
//! This is Wasmtime's own `dyn` boundary — the same one
//! `Arc<dyn CustomCodeMemory>` crosses in
//! [`engine`](super::engine) — and it is the only reason a trait object
//! appears here. Nothing kernel-facing acquires one: the creator's whole
//! body is a call into [`crate::memory`], which is generic-free and
//! concrete.

use alloc::boxed::Box;
use core::ops::Range;

use wasmtime::{StackCreator, StackMemory};

use crate::memory::{FiberStack, claim_fiber_stack};

/// Hands the runtime one slot of the kernel's fiber-stack arena per
/// stack it asks for.
///
/// The creator itself is stateless: the arena is machine-wide, so the
/// engines the kernel builds share one, and their stack pools each carry
/// the same live-stack limit the arena has slots for.
pub(crate) struct ArenaStackCreator;

// SAFETY: every stack this hands out is one slot of the arena, owned by
// the returned value alone until it is dropped, and unreachable from
// anything else while it lives — the slot's live flag is what makes that
// true against a second claim. The guard range below it is never mapped.
unsafe impl StackCreator for ArenaStackCreator {
    fn new_stack(&self, size: usize, _zeroed: bool) -> wasmtime::Result<Box<dyn StackMemory>> {
        // `zeroed` is not a request this can decline: every page of a
        // fresh slot arrives zeroed from the user pool, and a slot is
        // fully decommitted when its stack is dropped, so a stack the
        // arena hands out has never held anything.
        let stack = claim_fiber_stack(size).map_err(wasmtime::Error::new)?;
        Ok(Box::new(ArenaStackMemory(stack)))
    }
}

/// One arena slot, as the runtime sees it.
struct ArenaStackMemory(FiberStack);

// SAFETY: `top` and `range` describe the slot's body, which is committed
// on demand and stays mapped for as long as this value lives;
// `guard_range` describes the unmapped span below it, which is what
// turns an overflow into a fault rather than into the neighbouring
// slot's memory.
unsafe impl StackMemory for ArenaStackMemory {
    fn top(&self) -> *mut u8 {
        self.0.top()
    }

    fn range(&self) -> Range<usize> {
        self.0.range()
    }

    fn guard_range(&self) -> Range<*mut u8> {
        self.0.guard_range()
    }
}
