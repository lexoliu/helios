//! Which instance the pages committed on a processor belong to.
//!
//! The runtime never tells its virtual-memory hooks who is asking:
//! Wasmtime's pooling allocator reserves one large range for every slot
//! at engine construction and then `mprotect`s a slot's used part when
//! an instance starts, so the reservation says nothing about ownership
//! and the commit call carries no instance either.
//!
//! The component host closes that gap by naming the owner for the
//! stretch of work it is about to do on this processor — instantiating
//! a component, or running a guest call — and every commit recorded
//! while that scope is open is attributed to it. That is enough for the
//! swap policy, which needs to answer "which pages belong to the
//! instance that has been idle longest" and "how much is this instance
//! resident".
//!
//! Concurrency contract: one cell per processor, written only by the
//! processor it belongs to and read by the swap policy from wherever it
//! runs, so the cells are relaxed atomics and never a lock. The table is
//! sized once at boot, like the frame slab's shards; before it is sized,
//! and on a processor that never entered a scope, the owner reads as
//! [`MemoryOwner::NONE`].

extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU64, Ordering};

use helios_hal::cpu::ProcessorId;
use spin::Once;

/// Which instance a committed page belongs to.
///
/// [`MemoryOwner::NONE`] means nothing claimed it: kernel-side
/// reservations, the runtime's own scratch mappings, and anything
/// committed outside a scope. The swap policy never evicts unowned
/// pages — it cannot tell who would take the fault.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemoryOwner(u64);

impl MemoryOwner {
    pub const NONE: Self = Self(0);

    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// Per-processor "who is committing right now" cells.
pub struct UserMemoryOwners {
    processors: Once<Box<[AtomicU64]>>,
}

impl UserMemoryOwners {
    pub const fn new() -> Self {
        Self {
            processors: Once::new(),
        }
    }

    /// Sizes the table. Called once, from the same place the user pool
    /// learns its processor count.
    pub fn configure_processors(&self, processor_count: usize) {
        self.processors.call_once(|| {
            (0..processor_count)
                .map(|_| AtomicU64::new(MemoryOwner::NONE.raw()))
                .collect()
        });
    }

    fn cell(&self, processor: ProcessorId) -> Option<&AtomicU64> {
        self.processors.get()?.get(processor.id() as usize)
    }

    /// The owner pages committed on `processor` are attributed to.
    pub fn current(&self, processor: ProcessorId) -> MemoryOwner {
        match self.cell(processor) {
            Some(cell) => MemoryOwner::new(cell.load(Ordering::Relaxed)),
            None => MemoryOwner::NONE,
        }
    }

    /// Names `owner` for the work running on `processor` from now until
    /// something else names a different one, and reports who was named
    /// before.
    ///
    /// This is the form the runtime's call hooks use: they fire on
    /// entering and leaving guest code, which is not a scope any value
    /// can own.
    pub fn set(&self, processor: ProcessorId, owner: MemoryOwner) -> MemoryOwner {
        match self.cell(processor) {
            Some(cell) => MemoryOwner::new(cell.swap(owner.raw(), Ordering::Relaxed)),
            None => MemoryOwner::NONE,
        }
    }

    /// Names `owner` for the work about to run on `processor`, restoring
    /// the previous owner when the returned scope drops. Scopes nest:
    /// a guest call inside an instantiation restores the instantiating
    /// owner rather than clearing it.
    pub fn enter(&self, processor: ProcessorId, owner: MemoryOwner) -> UserMemoryOwnerScope<'_> {
        let previous = match self.cell(processor) {
            Some(cell) => MemoryOwner::new(cell.swap(owner.raw(), Ordering::Relaxed)),
            None => MemoryOwner::NONE,
        };
        UserMemoryOwnerScope {
            owners: self,
            processor,
            previous,
        }
    }
}

impl Default for UserMemoryOwners {
    fn default() -> Self {
        Self::new()
    }
}

/// Restores the previous owner of a processor when it drops.
pub struct UserMemoryOwnerScope<'a> {
    owners: &'a UserMemoryOwners,
    processor: ProcessorId,
    previous: MemoryOwner,
}

impl Drop for UserMemoryOwnerScope<'_> {
    fn drop(&mut self) {
        if let Some(cell) = self.owners.cell(self.processor) {
            cell.store(self.previous.raw(), Ordering::Relaxed);
        }
    }
}

/// The kernel's owner table. Installed once at boot next to the user
/// memory pool; backends read it from their `commit` path and the
/// component host writes it around the work it runs.
static OWNERS: UserMemoryOwners = UserMemoryOwners::new();

pub fn configure_user_memory_owner_processors(processor_count: usize) {
    OWNERS.configure_processors(processor_count);
}

/// The owner a page committed on `processor` right now belongs to.
pub fn current_user_memory_owner(processor: ProcessorId) -> MemoryOwner {
    OWNERS.current(processor)
}

/// Names `owner` for the work running on `processor` from now on, and
/// reports who was named before.
pub fn set_user_memory_owner(processor: ProcessorId, owner: MemoryOwner) -> MemoryOwner {
    OWNERS.set(processor, owner)
}

/// Names `owner` for the work about to run on `processor`.
pub fn enter_user_memory_owner(
    processor: ProcessorId,
    owner: MemoryOwner,
) -> UserMemoryOwnerScope<'static> {
    OWNERS.enter(processor, owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_nest_and_restore() {
        let owners = UserMemoryOwners::new();
        owners.configure_processors(2);
        let processor = ProcessorId::new(0);
        assert_eq!(owners.current(processor), MemoryOwner::NONE);
        {
            let _outer = owners.enter(processor, MemoryOwner::new(7));
            assert_eq!(owners.current(processor), MemoryOwner::new(7));
            {
                let _inner = owners.enter(processor, MemoryOwner::new(9));
                assert_eq!(owners.current(processor), MemoryOwner::new(9));
            }
            assert_eq!(owners.current(processor), MemoryOwner::new(7));
        }
        assert_eq!(owners.current(processor), MemoryOwner::NONE);
    }

    #[test]
    fn processors_do_not_share_an_owner() {
        let owners = UserMemoryOwners::new();
        owners.configure_processors(2);
        let _first = owners.enter(ProcessorId::new(0), MemoryOwner::new(1));
        assert_eq!(owners.current(ProcessorId::new(1)), MemoryOwner::NONE);
    }

    #[test]
    fn an_unsized_table_reports_no_owner() {
        let owners = UserMemoryOwners::new();
        let _scope = owners.enter(ProcessorId::new(0), MemoryOwner::new(3));
        assert_eq!(owners.current(ProcessorId::new(0)), MemoryOwner::NONE);
    }
}
