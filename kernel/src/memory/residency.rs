//! How much of the user pool each instance holds right now, as the
//! address space records it.
//!
//! An instance's linear-memory size says how much memory it asked for,
//! not how much of the pool it is holding: a page the swap policy wrote
//! out holds no frame, and neither does a span a view shows a buffer or
//! a device region at. What the OOM killer needs to know is what a kill
//! gives back, so it reads the address space's own account instead:
//! every committed, unsubstituted page the runtime committed while the
//! instance's owner scope was open: its linear memories as they grew,
//! and anything else the runtime made accessible for it in its turns.
//! A linear-memory slot is reset out of the pool when the store is torn
//! down (the runtime erases the mapping and the address space frees its
//! frames), so this is what the kill returns. It is not the instance's
//! fiber stack, which is committed with no owner and which the arena may
//! keep warm for the next stack; nor a pinned buffer, which goes back
//! only when its last holder lets go (a device or a second instance may
//! outlive the first) and which the bare-metal address spaces charge to
//! nobody; nor its kernel heap, which is a separate pool that funds
//! itself out of this one one way only.
//!
//! # Concurrency contract
//!
//! - Taking a [`UserResidency`] takes the address space's reservation
//!   lock once, for one walk of its committed regions. The visitor the
//!   walk calls only adds to the vector this module owns: it takes no
//!   other lock and never calls back into the address space. The vector
//!   grows on the kernel heap, by one entry per owner.
//! - It is taken in task context, and never while the instance
//!   registry's `entries` lock is held: the registry only ever reads a
//!   finished snapshot. The two locks are therefore never nested, in
//!   either order, and no lock order between them exists to keep.
//! - A [`UserResidency`] is a plain value. Reading it takes no lock, and
//!   it may be read on any processor. It is exact as of its walk; the
//!   decision it feeds is made against it moments later, while the swap
//!   policy and other processors' grows keep moving the real figure.

extern crate alloc;

use alloc::vec::Vec;

use helios_hal::vmm::AddressSpace;
use spin::Once;

use super::owner::MemoryOwner;
use super::swap::InstalledAddressSpace;

/// The user-pool bytes each owner holds, as one address-space walk saw
/// them.
///
/// An owner the walk did not see holds nothing: it has no page the pool
/// would get back by killing it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserResidency {
    /// One entry per owner, ascending by owner.
    owners: Vec<(MemoryOwner, u64)>,
}

impl UserResidency {
    /// Walk `space` once and total what it holds for each owner.
    pub fn of_address_space<Space: AddressSpace>(space: &Space) -> Self {
        let mut residency = Self::default();
        space.visit_owned_resident_bytes(|owner, bytes| {
            residency.add(MemoryOwner::new(owner), bytes);
        });
        residency
    }

    /// Walk the address space the backend installed with
    /// [`install_residency_hooks`].
    ///
    /// Panics when no backend has installed one: the runtime's linear
    /// memories then come from an address space nothing can ask, and
    /// any answer would be a guess.
    pub fn of_installed_address_space() -> Self {
        let hooks = HOOKS.get().unwrap_or_else(|| {
            panic!("user residency asked for before the backend installed its residency hooks")
        });
        (hooks.user_residency)()
    }

    /// What `owner` holds of the user pool.
    pub fn bytes_of(&self, owner: MemoryOwner) -> u64 {
        match self.owners.binary_search_by_key(&owner, |(held, _)| *held) {
            Ok(index) => self.owners[index].1,
            Err(_) => 0,
        }
    }

    fn add(&mut self, owner: MemoryOwner, bytes: u64) {
        match self.owners.binary_search_by_key(&owner, |(held, _)| *held) {
            Ok(index) => self.owners[index].1 += bytes,
            Err(index) => self.owners.insert(index, (owner, bytes)),
        }
    }
}

/// A residency built from `(owner, bytes)` parts, summed per owner: what
/// a caller that already knows the figures, a test among them, states
/// directly.
impl FromIterator<(MemoryOwner, u64)> for UserResidency {
    fn from_iter<Parts: IntoIterator<Item = (MemoryOwner, u64)>>(parts: Parts) -> Self {
        let mut residency = Self::default();
        for (owner, bytes) in parts {
            residency.add(owner, bytes);
        }
        residency
    }
}

/// The platform address space's residency surface, as a plain function
/// pointer.
///
/// Same shape and reason as `SwapVmHooks`: there is exactly one address
/// space per machine, chosen at link time, and the kernel holds no value
/// of its type. Unlike the swap table every bare-metal backend installs
/// this one, swap or no swap: the OOM killer ranks by it everywhere.
pub struct ResidencyVmHooks {
    /// One walk of the installed address space.
    pub user_residency: fn() -> UserResidency,
}

/// The residency table for `S`, generated once for every backend.
pub const fn residency_vm_hooks<S: InstalledAddressSpace>() -> ResidencyVmHooks {
    ResidencyVmHooks {
        user_residency: installed_user_residency::<S>,
    }
}

fn installed_user_residency<S: InstalledAddressSpace>() -> UserResidency {
    UserResidency::of_address_space(S::installed())
}

/// The residency table, installed once by the active backend.
static HOOKS: Once<&'static ResidencyVmHooks> = Once::new();

/// Publishes the platform address space's residency surface. Called once
/// by the backend that owns the address space, before any component
/// runs.
pub fn install_residency_hooks(hooks: &'static ResidencyVmHooks) {
    let mut installed = false;
    HOOKS.call_once(|| {
        installed = true;
        hooks
    });
    assert!(installed, "residency hooks installed more than once");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_sum_per_owner_and_an_unseen_owner_holds_nothing() {
        let residency: UserResidency = [
            (MemoryOwner::new(9), 4096),
            (MemoryOwner::new(3), 8192),
            (MemoryOwner::new(9), 4096),
        ]
        .into_iter()
        .collect();
        assert_eq!(residency.bytes_of(MemoryOwner::new(9)), 8192);
        assert_eq!(residency.bytes_of(MemoryOwner::new(3)), 8192);
        assert_eq!(residency.bytes_of(MemoryOwner::new(4)), 0);
    }
}
