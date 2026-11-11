//! What swap needs from one page-table format.
//!
//! [`SwapSpace`](super::SwapSpace) owns everything about swapping a page
//! that is the same on every architecture: the order of the steps, the
//! frame, the reservation bookkeeping and its rollback, the orphan queue
//! and the ageing walk. A backend supplies the part that is not, the
//! encoding of a leaf entry and the TLB work around it, by implementing
//! [`SwapLeafFormat`] on its address space.

use core::ptr::NonNull;

use helios_hal::vmm::{AddressSpaceError, PageAge, PageFlags, SwapToken, VirtAddr};

use crate::memory::ReservationTracker;

/// Pages whose accessed flag one scan clears before it calls
/// [`SwapLeafFormat::accessed_cleared`], so a format that has to
/// invalidate translations does it once per run rather than per page.
pub const AGE_BATCH_PAGES: usize = 128;

/// The leaf-entry operations swap needs from one page-table format.
///
/// # Concurrency contract
///
/// [`Self::swapped_token`] takes no lock, allocates nothing and touches no
/// memory that can fault, because a backend's page-fault entry calls it
/// from trap context. Every other method runs with the owning address
/// space's reservation lock held, which serialises every software write
/// to a user leaf. Hardware, and the fault entry's access-flag setter, may
/// still change the accessed (and dirty) flag of a present leaf
/// concurrently, so a method that rewrites a present leaf does it with an
/// operation that cannot lose such an update, or replaces the leaf
/// outright because nothing the concurrent writer did survives the
/// replacement.
///
/// The `tracker` the two mutating methods take is the reservation
/// bookkeeping the caller already holds the lock for, shown read-only. A
/// format whose hardware state is derived from the bookkeeping (the
/// hosted backend's host-page protection is the union of the regions
/// that share a host page) reads it there; a format that encodes
/// everything in the leaf ignores it.
pub trait SwapLeafFormat {
    /// The token the leaf at `addr` carries. `None` for every address
    /// outside this format's user window, answered before any walk.
    fn swapped_token(&self, addr: VirtAddr) -> Option<SwapToken>;

    /// Turns the present leaf at `addr` into a swap entry carrying
    /// `token`.
    ///
    /// On return every processor has dropped its translation of the page:
    /// the local invalidate plus whatever the format needs to reach the
    /// others, completed. Returns the frame's kernel alias, still
    /// allocated and now unreachable by any translation; the caller reads
    /// it and frees it.
    ///
    /// `tracker` already records the page as swapped out.
    fn detach(
        &self,
        tracker: &ReservationTracker,
        addr: VirtAddr,
        token: SwapToken,
    ) -> Result<NonNull<u8>, AddressSpaceError>;

    /// Turns the swap entry carrying `expected` at `addr` into a present
    /// leaf for `frame` with `flags`, accessed flag set.
    ///
    /// A not-present leaf becoming present needs no shootdown, because no
    /// processor caches a not-present translation. Returns
    /// [`AddressSpaceError::NotSwapped`] if the leaf does not carry
    /// `expected`. On `Ok` the frame belongs to the leaf (a format that
    /// copies it somewhere else returns it to the user pool itself); on
    /// `Err` it is still the caller's.
    ///
    /// `tracker` already records the page as committed again.
    fn attach(
        &self,
        tracker: &ReservationTracker,
        addr: VirtAddr,
        expected: SwapToken,
        frame: NonNull<u8>,
        flags: PageFlags,
    ) -> Result<(), AddressSpaceError>;

    /// Turns a swap entry back into an empty leaf. A leaf that is not a
    /// swap entry is left as it is.
    ///
    /// The reservation bookkeeping no longer holds the entry by the time
    /// this is called: it came back from the tracker's release, decommit
    /// or commit, which is why a format whose leaf is the bookkeeping
    /// itself has nothing left to clear.
    fn clear_swapped(&self, addr: VirtAddr);

    /// Clears the accessed flag of a present leaf and reports what it
    /// was. `None` for a leaf that is not present.
    fn test_and_clear_accessed(&self, addr: VirtAddr) -> Option<PageAge>;

    /// Called after `pages` consecutive pages from `start` (at most
    /// [`AGE_BATCH_PAGES`]) had their accessed flag cleared by
    /// [`Self::test_and_clear_accessed`]: whatever this format needs for
    /// the cleared flag to be observed.
    fn accessed_cleared(&self, start: VirtAddr, pages: usize);
}
