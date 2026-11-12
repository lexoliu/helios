//! Reservation and committed-region bookkeeping shared by every
//! [`AddressSpace`](helios_hal::vmm::AddressSpace) backend.
//!
//! Concurrency contract: [`ReservationTracker`] is a plain value with no
//! interior mutability — the owning backend wraps it in its own lock and
//! must not hold that lock across `.await` points or hardware mutation
//! that can fault. [`VaCursor`] is lock-free (a single atomic bump with
//! CAS retry) so fresh carving never contends with the tracker lock.
//!
//! The tracker records intent, not hardware state: backends validate and
//! update bookkeeping through it, and perform page-table or `mprotect`
//! mutation themselves between the precheck and the record step.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpaceError, PageFlags, SwapToken, VirtAddr, VirtRange};

use super::owner::MemoryOwner;

/// A committed sub-range of a reservation, the flags it was granted,
/// and the instance it was committed for.
///
/// A region can be *substituted*: its leaf entries were rewritten in
/// place to show a pinned run or a device region instead of the pool
/// frames the runtime committed. It stays part of the committed extent,
/// so a protection change applies to it like any committed page and a
/// commit never lands on it, but its frames are not the reservation's:
/// they belong to the run's owner or to a device, so nothing that frees
/// a reservation's frames may free them, swap and aging skip them, and
/// they are not resident memory of [`Self::owner`]. The owner and flags
/// stay recorded because ending the substitution puts memory back with
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommittedRegion {
    pub range: VirtRange,
    pub flags: PageFlags,
    pub owner: MemoryOwner,
    pub substituted: bool,
}

/// What [`ReservationTracker::record_decommit`] took out of the
/// tracker.
#[derive(Debug, Default)]
pub struct Decommitted {
    /// Swapped pages inside the range, whose tokens the caller must
    /// release.
    pub swapped: Vec<SwapEntry>,
    /// The parts of the range that were substituted, coalesced and
    /// ascending. Their frames are not the pool's: the caller drops
    /// those mappings and shoots them down without freeing a frame,
    /// before it frees the rest of the range.
    pub substituted: Vec<VirtRange>,
}

/// What [`ReservationTracker::take_committed_intersections`] took out of
/// the tracker.
#[derive(Debug, Default)]
pub struct CommittedRemoval {
    /// Every committed sub-range that was removed, substituted ones
    /// included.
    pub ranges: Vec<VirtRange>,
    /// The substituted parts of those, coalesced and ascending: see
    /// [`Decommitted::substituted`].
    pub substituted: Vec<VirtRange>,
}

/// One page whose contents live in a swap backend instead of memory.
///
/// The page-table entry carries the same token on architectures that
/// have spare bits in a not-present descriptor; this record is what
/// teardown and enumeration walk, because a page table cannot be
/// searched by owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapEntry {
    pub addr: VirtAddr,
    pub token: SwapToken,
    /// Flags the page had before it was swapped out, so restoring it
    /// puts it back exactly as the runtime left it.
    pub flags: PageFlags,
    pub owner: MemoryOwner,
}

/// What a released reservation was still holding.
///
/// Committed regions have frames to hand back; swapped pages have swap
/// extents the backend is still holding, and the caller must release
/// every one of those tokens or the swap device leaks one dead instance
/// at a time.
#[derive(Debug, Default)]
pub struct ReleasedReservation {
    pub committed: Vec<CommittedRegion>,
    pub swapped: Vec<SwapEntry>,
}

struct Reservation {
    range: VirtRange,
    committed: Vec<CommittedRegion>,
    /// Pages inside this reservation that are swapped out, ascending.
    swapped: Vec<SwapEntry>,
}

/// Result of looking up a single address in the tracker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservationLookup {
    Unreserved,
    Reserved,
    Committed(CommittedRegion),
}

/// Sub-ranges an accessibility request splits into: already-committed
/// parts that only need a protection change, and uncommitted gaps that
/// need a fresh commit.
pub struct AccessibilityPlan {
    pub protect: Vec<VirtRange>,
    pub commit: Vec<VirtRange>,
}

/// Reservation table plus the free-list of released ranges available for
/// reuse by [`ReservationTracker::reuse_free_range`].
#[derive(Default)]
pub struct ReservationTracker {
    reservations: Vec<Reservation>,
    free_list: Vec<VirtRange>,
}

impl ReservationTracker {
    pub const fn new() -> Self {
        Self {
            reservations: Vec::new(),
            free_list: Vec::new(),
        }
    }

    /// Records a fresh reservation. The caller owns address selection.
    pub fn reserve(&mut self, range: VirtRange) {
        self.reservations.push(Reservation {
            range,
            committed: Vec::new(),
            swapped: Vec::new(),
        });
    }

    /// Removes the reservation exactly matching `range` and returns
    /// everything it still held: committed regions to unmap, and
    /// swapped pages whose tokens the caller must release.
    pub fn release(&mut self, range: VirtRange) -> Result<ReleasedReservation, AddressSpaceError> {
        let index = self
            .reservations
            .iter()
            .position(|reservation| reservation.range == range)
            .ok_or(AddressSpaceError::NotReserved)?;
        let reservation = self.reservations.swap_remove(index);
        Ok(ReleasedReservation {
            committed: reservation.committed,
            swapped: reservation.swapped,
        })
    }

    /// Validates that `virt` lies inside a reservation and does not
    /// overlap an already-committed region.
    pub fn precheck_commit(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        let reservation = self.find(virt)?;
        if reservation
            .committed
            .iter()
            .any(|region| ranges_overlap(region.range, virt))
        {
            return Err(AddressSpaceError::Overlap);
        }
        Ok(())
    }

    /// Records a committed region after the backend mapped it,
    /// attributing it to whoever the processor was committing for.
    ///
    /// Committing over a swapped-out page discards that page's swap
    /// entry — the runtime has decided the old contents are gone — and
    /// the orphaned token is returned so the caller can release its
    /// backing store.
    pub fn record_commit(
        &mut self,
        virt: VirtRange,
        flags: PageFlags,
        owner: MemoryOwner,
    ) -> Result<Vec<SwapEntry>, AddressSpaceError> {
        let reservation = self.find_mut(virt)?;
        let orphaned = take_swapped_range(&mut reservation.swapped, virt);
        reservation.committed.push(CommittedRegion {
            range: virt,
            flags,
            owner,
            substituted: false,
        });
        Ok(orphaned)
    }

    /// Validates that `virt` lies inside a reservation and is fully
    /// covered by committed regions or swapped-out pages.
    ///
    /// A swapped-out page is committed memory whose frame is on loan to
    /// the swap backend, so it counts here exactly as it does in
    /// [`Self::accessibility_plan`] and [`Self::check_substitutable`]:
    /// a protection change or a decommit reaches it, and the entry keeps
    /// the page's flags and token.
    pub fn ensure_committed(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        let reservation = self.find(virt)?;
        if !committed_or_swapped_cover(&reservation.committed, &reservation.swapped, virt) {
            return Err(AddressSpaceError::NotCommitted);
        }
        Ok(())
    }

    /// Removes `virt` from the committed set after validating coverage,
    /// returning any swapped pages inside it whose tokens the caller
    /// must release and any substituted parts whose frames are not the
    /// pool's to take back.
    pub fn record_decommit(&mut self, virt: VirtRange) -> Result<Decommitted, AddressSpaceError> {
        self.ensure_committed(virt)?;
        let reservation = self.find_mut(virt)?;
        let substituted = substituted_parts(&reservation.committed, virt);
        remove_committed_range(&mut reservation.committed, virt);
        Ok(Decommitted {
            swapped: take_swapped_range(&mut reservation.swapped, virt),
            substituted,
        })
    }

    /// Replaces the committed flags for `virt` after the backend changed
    /// the hardware protection. Coverage must have been prechecked with
    /// [`Self::ensure_committed`].
    ///
    /// Each region the range touches keeps its own owner and its
    /// substituted mark: a protection change is not a substitution
    /// change.
    pub fn record_protect(
        &mut self,
        virt: VirtRange,
        flags: PageFlags,
    ) -> Result<(), AddressSpaceError> {
        let reservation = self.find_mut(virt)?;
        let pieces = clipped_regions(&reservation.committed, virt);
        remove_committed_range(&mut reservation.committed, virt);
        for piece in pieces {
            reservation
                .committed
                .push(CommittedRegion { flags, ..piece });
        }
        // A protection change applies to the swapped pages too: they go
        // back with the flags the runtime last asked for.
        for entry in &mut reservation.swapped {
            if virt.contains(entry.addr) {
                entry.flags = flags;
            }
        }
        Ok(())
    }

    /// Whether `virt` can be substituted: inside a reservation, every
    /// page committed or swapped out, and none of it substituted
    /// already. Changes nothing.
    ///
    /// A swapped-out page counts because a program's buffer is ordinary
    /// memory the swap policy may have evicted; the backend finds such a
    /// page's leaf holding a swap descriptor instead of a frame.
    ///
    /// [`AddressSpaceError::NotCommitted`] when any page is neither,
    /// [`AddressSpaceError::Substituted`] when any page already shows
    /// something else.
    pub fn check_substitutable(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        let reservation = self.find(virt)?;
        if !committed_or_swapped_cover(&reservation.committed, &reservation.swapped, virt) {
            return Err(AddressSpaceError::NotCommitted);
        }
        self.ensure_unsubstituted(virt)
    }

    /// Refuses with [`AddressSpaceError::Substituted`] when any page of
    /// `virt` is substituted: for an operation that moves or copies the
    /// pool frames behind a range, which a substitution does not have.
    pub fn ensure_unsubstituted(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        let reservation = self.find(virt)?;
        if reservation
            .committed
            .iter()
            .any(|region| region.substituted && ranges_overlap(region.range, virt))
        {
            return Err(AddressSpaceError::Substituted);
        }
        Ok(())
    }

    /// Marks `virt` substituted after the backend rewrote its leaves.
    /// The range stays committed, with the flags and owner each of its
    /// regions already carried.
    ///
    /// A swapped-out page inside `virt` leaves the swapped set and joins
    /// the substituted region with the flags and owner its swap entry
    /// carried, so ending the substitution gives it back as it was. Its
    /// entry is returned: the caller releases the token like one a
    /// commit orphaned, and has no pool frame to free for that page.
    pub fn record_substitute(
        &mut self,
        virt: VirtRange,
    ) -> Result<Vec<SwapEntry>, AddressSpaceError> {
        self.check_substitutable(virt)?;
        let reservation = self.find_mut(virt)?;
        let mut pieces = clipped_regions(&reservation.committed, virt);
        remove_committed_range(&mut reservation.committed, virt);
        let swapped = take_swapped_range(&mut reservation.swapped, virt);
        pieces.extend(swapped.iter().map(|entry| CommittedRegion {
            range: VirtRange::new(entry.addr, PhysFrame::SIZE),
            flags: entry.flags,
            owner: entry.owner,
            substituted: false,
        }));
        for piece in pieces {
            reservation.committed.push(CommittedRegion {
                substituted: true,
                ..piece
            });
        }
        Ok(swapped)
    }

    /// The regions that make up `virt`, clipped to it, when every page
    /// of it is substituted: what a backend needs to put memory back
    /// with the flags and owner each page had.
    ///
    /// [`AddressSpaceError::NotSubstituted`] when any page of `virt` is
    /// not.
    pub fn substituted_regions(
        &self,
        virt: VirtRange,
    ) -> Result<Vec<CommittedRegion>, AddressSpaceError> {
        let reservation = self.find(virt)?;
        if !substituted_regions_cover(&reservation.committed, virt) {
            return Err(AddressSpaceError::NotSubstituted);
        }
        Ok(clipped_regions(&reservation.committed, virt))
    }

    /// Clears the substituted mark on `virt` after the backend put
    /// memory back. The range stays committed with its flags and owner.
    pub fn record_restore(&mut self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        let pieces = self.substituted_regions(virt)?;
        let reservation = self.find_mut(virt)?;
        remove_committed_range(&mut reservation.committed, virt);
        for piece in pieces {
            reservation.committed.push(CommittedRegion {
                substituted: false,
                ..piece
            });
        }
        Ok(())
    }

    /// Removes the substituted `virt` from the committed set after the
    /// backend dropped its mapping, leaving it reserved and
    /// uncommitted.
    pub fn record_withdraw(&mut self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        self.substituted_regions(virt)?;
        let reservation = self.find_mut(virt)?;
        remove_committed_range(&mut reservation.committed, virt);
        Ok(())
    }

    /// Puts `regions` back verbatim as the committed set of `virt`,
    /// undoing a change a backend could not finish.
    pub fn replace_committed(
        &mut self,
        virt: VirtRange,
        regions: &[CommittedRegion],
    ) -> Result<(), AddressSpaceError> {
        let reservation = self.find_mut(virt)?;
        remove_committed_range(&mut reservation.committed, virt);
        reservation.committed.extend_from_slice(regions);
        Ok(())
    }

    /// Range of the reservation containing `virt`, for callers that need
    /// to clip hardware operations to the reservation hull.
    pub fn reservation_range(&self, virt: VirtRange) -> Result<VirtRange, AddressSpaceError> {
        Ok(self.find(virt)?.range)
    }

    /// Classifies a single address as unreserved, reserved, or committed.
    pub fn lookup(&self, addr: VirtAddr) -> ReservationLookup {
        let Some(reservation) = self
            .reservations
            .iter()
            .find(|reservation| reservation.range.contains(addr))
        else {
            return ReservationLookup::Unreserved;
        };
        match reservation
            .committed
            .iter()
            .find(|region| region.range.contains(addr))
        {
            Some(region) => ReservationLookup::Committed(*region),
            None => ReservationLookup::Reserved,
        }
    }

    /// Snapshots every committed region intersecting `virt`, clipped to
    /// the intersection, without modifying the tracker.
    pub fn committed_intersections(
        &self,
        virt: VirtRange,
    ) -> Result<Vec<CommittedRegion>, AddressSpaceError> {
        let reservation = self.find(virt)?;
        Ok(clipped_regions(&reservation.committed, virt))
    }

    /// Removes and returns the swapped pages inside `virt`, for callers
    /// that are about to drop the range's contents. The tokens are the
    /// caller's to release.
    pub fn take_swap_entries_in(&mut self, virt: VirtRange) -> Vec<SwapEntry> {
        match self.find_mut(virt) {
            Ok(reservation) => take_swapped_range(&mut reservation.swapped, virt),
            Err(_) => Vec::new(),
        }
    }

    /// Removes and returns the committed sub-ranges intersecting `virt`,
    /// for callers that decommit hardware state afterwards, and which of
    /// them were substituted.
    pub fn take_committed_intersections(
        &mut self,
        virt: VirtRange,
    ) -> Result<CommittedRemoval, AddressSpaceError> {
        let reservation = self.find_mut(virt)?;
        let ranges: Vec<VirtRange> = reservation
            .committed
            .iter()
            .filter_map(|region| range_intersection(region.range, virt))
            .collect();
        let substituted = substituted_parts(&reservation.committed, virt);
        for subrange in &ranges {
            remove_committed_range(&mut reservation.committed, *subrange);
        }
        Ok(CommittedRemoval {
            ranges,
            substituted,
        })
    }

    /// Splits `virt` into already-committed sub-ranges (to re-protect)
    /// and uncommitted gaps (to commit).
    ///
    /// A swapped-out page is committed: it is planned for a protection
    /// change, never for a commit, which would replace its contents with
    /// a zero page and orphan its token.
    pub fn accessibility_plan(
        &self,
        virt: VirtRange,
    ) -> Result<AccessibilityPlan, AddressSpaceError> {
        let reservation = self.find(virt)?;
        let mut plan = AccessibilityPlan {
            protect: Vec::new(),
            commit: Vec::new(),
        };
        let mut cursor = virt.start.raw();
        let end = virt.end().raw();
        while cursor < end {
            if let Some(next) =
                committed_extent_end(&reservation.committed, &reservation.swapped, cursor)
            {
                let next = next.min(end);
                plan.protect
                    .push(VirtRange::new(VirtAddr::new(cursor), next - cursor));
                cursor = next;
                continue;
            }
            let next_committed_start =
                next_committed_extent_start(&reservation.committed, &reservation.swapped, cursor)
                    .filter(|start| *start < end)
                    .unwrap_or(end);
            plan.commit.push(VirtRange::new(
                VirtAddr::new(cursor),
                next_committed_start - cursor,
            ));
            cursor = next_committed_start;
        }
        Ok(plan)
    }

    /// Reuses a released range of at least `byte_len` bytes, splitting
    /// off any leftover back into the free-list.
    pub fn reuse_free_range(&mut self, byte_len: usize) -> Option<VirtRange> {
        let index = self
            .free_list
            .iter()
            .position(|range| range.byte_len >= byte_len)?;
        let mut reused = self.free_list.swap_remove(index);
        if reused.byte_len > byte_len {
            let leftover = VirtRange::new(
                VirtAddr::new(reused.start.raw() + byte_len),
                reused.byte_len - byte_len,
            );
            self.free_list.push(leftover);
            reused.byte_len = byte_len;
        }
        Some(reused)
    }

    /// Returns a released reservation range to the reuse pool.
    pub fn push_free_range(&mut self, range: VirtRange) {
        self.free_list.push(range);
    }

    /// Moves the committed page at `addr` into the swapped set.
    ///
    /// The page must be committed; its flags are captured so
    /// [`Self::take_swap_entry`] can put it back unchanged.
    pub fn record_swap_out(
        &mut self,
        addr: VirtAddr,
        token: SwapToken,
    ) -> Result<PageFlags, AddressSpaceError> {
        let page = VirtRange::new(addr, PhysFrame::SIZE);
        let reservation = self.find_mut(page)?;
        let region = reservation
            .committed
            .iter()
            .find(|region| region.range.contains(addr))
            .copied()
            .ok_or(AddressSpaceError::NotCommitted)?;
        if region.substituted {
            return Err(AddressSpaceError::Substituted);
        }
        remove_committed_range(&mut reservation.committed, page);
        let entry = SwapEntry {
            addr,
            token,
            flags: region.flags,
            owner: region.owner,
        };
        let position = reservation
            .swapped
            .partition_point(|existing| existing.addr < addr);
        reservation.swapped.insert(position, entry);
        Ok(region.flags)
    }

    /// Moves the page at `addr` back out of the swapped set and into the
    /// committed set, returning the entry it had.
    pub fn take_swap_entry(&mut self, addr: VirtAddr) -> Result<SwapEntry, AddressSpaceError> {
        let page = VirtRange::new(addr, PhysFrame::SIZE);
        let reservation = self.find_mut(page)?;
        let position = reservation
            .swapped
            .iter()
            .position(|entry| entry.addr == addr)
            .ok_or(AddressSpaceError::NotSwapped)?;
        let entry = reservation.swapped.remove(position);
        reservation.committed.push(CommittedRegion {
            range: page,
            flags: entry.flags,
            owner: entry.owner,
            substituted: false,
        });
        Ok(entry)
    }

    /// The swap entry for `addr`, without disturbing it.
    pub fn swap_entry(&self, addr: VirtAddr) -> Option<SwapEntry> {
        let page = VirtRange::new(addr, PhysFrame::SIZE);
        self.find(page)
            .ok()?
            .swapped
            .iter()
            .find(|entry| entry.addr == addr)
            .copied()
    }

    /// Bytes committed for `owner` across every reservation. A
    /// substituted region is not counted: its frames are not `owner`'s
    /// resident memory.
    pub fn owned_resident_bytes(&self, owner: MemoryOwner) -> u64 {
        self.reservations
            .iter()
            .flat_map(|reservation| reservation.committed.iter())
            .filter(|region| region.owner == owner && !region.substituted)
            .map(|region| region.range.byte_len as u64)
            .sum()
    }

    /// Pages `owner` has swapped out across every reservation.
    pub fn owned_swapped_pages(&self, owner: MemoryOwner) -> usize {
        self.reservations
            .iter()
            .flat_map(|reservation| reservation.swapped.iter())
            .filter(|entry| entry.owner == owner)
            .count()
    }

    /// Pages swapped out across every reservation and owner.
    pub fn swapped_pages(&self) -> usize {
        self.reservations
            .iter()
            .map(|reservation| reservation.swapped.len())
            .sum()
    }

    /// Visits `owner`'s committed regions, largest first, until `visit`
    /// returns `false`. Largest first because a swap-out pass wants the
    /// fewest scans for the most reclaimed pages. Substituted regions
    /// are skipped: their frames are not the pool's to age or evict.
    pub fn owned_committed_regions<Visit>(&self, owner: MemoryOwner, mut visit: Visit)
    where
        Visit: FnMut(CommittedRegion) -> bool,
    {
        let mut regions: Vec<CommittedRegion> = self
            .reservations
            .iter()
            .flat_map(|reservation| reservation.committed.iter())
            .filter(|region| region.owner == owner && !region.substituted)
            .copied()
            .collect();
        regions.sort_by_key(|region| core::cmp::Reverse(region.range.byte_len));
        for region in regions {
            if !visit(region) {
                return;
            }
        }
    }

    /// Every owner that currently has resident (unsubstituted)
    /// committed pages.
    pub fn owners(&self) -> Vec<MemoryOwner> {
        let mut owners: Vec<MemoryOwner> = self
            .reservations
            .iter()
            .flat_map(|reservation| reservation.committed.iter())
            .filter(|region| !region.substituted)
            .map(|region| region.owner)
            .filter(|owner| !owner.is_none())
            .collect();
        owners.sort_unstable();
        owners.dedup();
        owners
    }

    fn find(&self, virt: VirtRange) -> Result<&Reservation, AddressSpaceError> {
        self.reservations
            .iter()
            .find(|reservation| range_contains(reservation.range, virt))
            .ok_or(AddressSpaceError::NotReserved)
    }

    fn find_mut(&mut self, virt: VirtRange) -> Result<&mut Reservation, AddressSpaceError> {
        self.reservations
            .iter_mut()
            .find(|reservation| range_contains(reservation.range, virt))
            .ok_or(AddressSpaceError::NotReserved)
    }
}

/// The parts of `range` that lie outside every range of `excluded`,
/// ascending. `excluded` is ascending and non-overlapping, as
/// [`Decommitted::substituted`] and [`CommittedRemoval::substituted`]
/// are.
///
/// A backend whose unmap refuses a page with no leaf unmaps these
/// instead of `range`, once it has dropped the substituted parts.
pub fn ranges_outside(range: VirtRange, excluded: &[VirtRange]) -> Vec<VirtRange> {
    let mut parts = Vec::new();
    let mut cursor = range.start.raw();
    let end = range.end().raw();
    for hole in excluded {
        let Some(hole) = range_intersection(*hole, range) else {
            continue;
        };
        if cursor < hole.start.raw() {
            parts.push(VirtRange::new(
                VirtAddr::new(cursor),
                hole.start.raw() - cursor,
            ));
        }
        cursor = hole.end().raw();
    }
    if cursor < end {
        parts.push(VirtRange::new(VirtAddr::new(cursor), end - cursor));
    }
    parts
}

/// Rejects empty and misaligned ranges before any bookkeeping.
pub fn validate_range(virt: VirtRange) -> Result<(), AddressSpaceError> {
    if virt.byte_len == 0 {
        return Err(AddressSpaceError::EmptyRange);
    }
    if !virt.is_page_aligned() {
        return Err(AddressSpaceError::Misaligned);
    }
    Ok(())
}

/// Lock-free bump allocator over a fixed user VA window, used when the
/// free-list has no reusable range.
pub struct VaCursor {
    next: AtomicUsize,
    end: usize,
}

impl VaCursor {
    pub const fn new(start: usize, end: usize) -> Self {
        Self {
            next: AtomicUsize::new(start),
            end,
        }
    }

    pub fn carve(&self, byte_len: usize) -> Option<VirtRange> {
        let mut current = self.next.load(Ordering::Acquire);
        loop {
            let next = current.checked_add(byte_len)?;
            if next > self.end {
                return None;
            }
            match self
                .next
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(VirtRange::new(VirtAddr::new(current), byte_len)),
                Err(observed) => current = observed,
            }
        }
    }
}

fn range_contains(outer: VirtRange, inner: VirtRange) -> bool {
    outer.start.raw() <= inner.start.raw() && inner.end().raw() <= outer.end().raw()
}

fn ranges_overlap(a: VirtRange, b: VirtRange) -> bool {
    a.start.raw() < b.end().raw() && b.start.raw() < a.end().raw()
}

fn range_intersection(a: VirtRange, b: VirtRange) -> Option<VirtRange> {
    let start = a.start.raw().max(b.start.raw());
    let end = a.end().raw().min(b.end().raw());
    (start < end).then(|| VirtRange::new(VirtAddr::new(start), end - start))
}

/// Where the committed extent containing `cursor` ends: the end of the
/// committed region holding it, or the end of its page when it is a
/// swapped-out one. `None` when `cursor` is uncommitted.
///
/// This is the one definition of "committed" the tracker's coverage
/// checks and its accessibility plan share.
fn committed_extent_end(
    regions: &[CommittedRegion],
    swapped: &[SwapEntry],
    cursor: usize,
) -> Option<usize> {
    let addr = VirtAddr::new(cursor);
    if let Some(region) = regions.iter().find(|region| region.range.contains(addr)) {
        Some(region.range.end().raw())
    } else {
        swapped
            .iter()
            .any(|entry| entry.addr == addr)
            .then_some(cursor + PhysFrame::SIZE)
    }
}

/// The start of the nearest committed extent above `cursor`, a region
/// or a swapped-out page, if any.
fn next_committed_extent_start(
    regions: &[CommittedRegion],
    swapped: &[SwapEntry],
    cursor: usize,
) -> Option<usize> {
    regions
        .iter()
        .map(|region| region.range.start.raw())
        .chain(swapped.iter().map(|entry| entry.addr.raw()))
        .filter(|start| cursor < *start)
        .min()
}

/// Whether every page of `range` is inside a committed region or is a
/// swapped-out page.
fn committed_or_swapped_cover(
    regions: &[CommittedRegion],
    swapped: &[SwapEntry],
    range: VirtRange,
) -> bool {
    let mut cursor = range.start.raw();
    let end = range.end().raw();
    while cursor < end {
        let Some(next) = committed_extent_end(regions, swapped, cursor) else {
            return false;
        };
        cursor = next.min(end);
    }
    true
}

/// Whether substituted regions cover every page of `range`.
fn substituted_regions_cover(regions: &[CommittedRegion], range: VirtRange) -> bool {
    let mut cursor = range.start.raw();
    let end = range.end().raw();
    while cursor < end {
        let Some(region) = regions
            .iter()
            .find(|region| region.substituted && region.range.contains(VirtAddr::new(cursor)))
        else {
            return false;
        };
        cursor = region.range.end().raw().min(end);
    }
    true
}

/// Every region intersecting `range`, clipped to it, ascending.
fn clipped_regions(regions: &[CommittedRegion], range: VirtRange) -> Vec<CommittedRegion> {
    let mut clipped: Vec<CommittedRegion> = regions
        .iter()
        .filter_map(|region| {
            range_intersection(region.range, range).map(|clip| CommittedRegion {
                range: clip,
                ..*region
            })
        })
        .collect();
    clipped.sort_by_key(|region| region.range.start.raw());
    clipped
}

/// The substituted parts of `range`, adjacent ones joined, ascending.
fn substituted_parts(regions: &[CommittedRegion], range: VirtRange) -> Vec<VirtRange> {
    let mut parts: Vec<VirtRange> = Vec::new();
    for region in clipped_regions(regions, range)
        .into_iter()
        .filter(|region| region.substituted)
    {
        match parts.last_mut() {
            Some(last) if last.end().raw() == region.range.start.raw() => {
                last.byte_len += region.range.byte_len;
            }
            _ => parts.push(region.range),
        }
    }
    parts
}

/// Removes and returns every swap entry inside `range`.
fn take_swapped_range(entries: &mut Vec<SwapEntry>, range: VirtRange) -> Vec<SwapEntry> {
    let mut taken = Vec::new();
    entries.retain(|entry| {
        if range.contains(entry.addr) {
            taken.push(*entry);
            false
        } else {
            true
        }
    });
    taken
}

fn remove_committed_range(regions: &mut Vec<CommittedRegion>, range: VirtRange) {
    let mut index = 0;
    while index < regions.len() {
        let region = regions[index];
        if !ranges_overlap(region.range, range) {
            index += 1;
            continue;
        }

        regions.swap_remove(index);
        if region.range.start.raw() < range.start.raw() {
            regions.push(CommittedRegion {
                range: VirtRange::new(
                    region.range.start,
                    range.start.raw() - region.range.start.raw(),
                ),
                flags: region.flags,
                owner: region.owner,
                substituted: region.substituted,
            });
        }
        if range.end().raw() < region.range.end().raw() {
            regions.push(CommittedRegion {
                range: VirtRange::new(range.end(), region.range.end().raw() - range.end().raw()),
                flags: region.flags,
                owner: region.owner,
                substituted: region.substituted,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 4096;

    fn range(start: usize, pages: usize) -> VirtRange {
        VirtRange::new(VirtAddr::new(start), pages * PAGE)
    }

    fn tracker_with_reservation(start: usize, pages: usize) -> ReservationTracker {
        let mut tracker = ReservationTracker::new();
        tracker.reserve(range(start, pages));
        tracker
    }

    #[test]
    fn commit_precheck_rejects_overlap_and_foreign_ranges() {
        let mut tracker = tracker_with_reservation(0x1_0000, 4);
        tracker
            .record_commit(range(0x1_0000, 2), PageFlags::READ, MemoryOwner::NONE)
            .unwrap();
        assert_eq!(
            tracker.precheck_commit(range(0x1_1000, 2)),
            Err(AddressSpaceError::Overlap)
        );
        assert_eq!(
            tracker.precheck_commit(range(0x9_0000, 1)),
            Err(AddressSpaceError::NotReserved)
        );
        tracker.precheck_commit(range(0x1_2000, 2)).unwrap();
    }

    #[test]
    fn decommit_requires_full_coverage_and_splits_regions() {
        let mut tracker = tracker_with_reservation(0x1_0000, 4);
        tracker
            .record_commit(
                range(0x1_0000, 4),
                PageFlags::READ | PageFlags::WRITE,
                MemoryOwner::NONE,
            )
            .unwrap();
        tracker.record_decommit(range(0x1_1000, 1)).unwrap();
        assert_eq!(
            tracker.record_decommit(range(0x1_1000, 1)).err(),
            Some(AddressSpaceError::NotCommitted)
        );
        assert_eq!(
            tracker.lookup(VirtAddr::new(0x1_1000)),
            ReservationLookup::Reserved
        );
        assert!(matches!(
            tracker.lookup(VirtAddr::new(0x1_0000)),
            ReservationLookup::Committed(_)
        ));
        assert!(matches!(
            tracker.lookup(VirtAddr::new(0x1_2000)),
            ReservationLookup::Committed(_)
        ));
    }

    #[test]
    fn protect_replaces_flags_in_place() {
        let mut tracker = tracker_with_reservation(0x1_0000, 2);
        tracker
            .record_commit(
                range(0x1_0000, 2),
                PageFlags::READ | PageFlags::WRITE,
                MemoryOwner::NONE,
            )
            .unwrap();
        tracker.ensure_committed(range(0x1_0000, 1)).unwrap();
        tracker
            .record_protect(range(0x1_0000, 1), PageFlags::READ)
            .unwrap();
        match tracker.lookup(VirtAddr::new(0x1_0000)) {
            ReservationLookup::Committed(region) => assert_eq!(region.flags, PageFlags::READ),
            other => panic!("expected committed lookup, got {other:?}"),
        }
        match tracker.lookup(VirtAddr::new(0x1_1000)) {
            ReservationLookup::Committed(region) => {
                assert_eq!(region.flags, PageFlags::READ | PageFlags::WRITE);
            }
            other => panic!("expected committed lookup, got {other:?}"),
        }
    }

    #[test]
    fn release_returns_committed_regions_and_frees_range() {
        let mut tracker = tracker_with_reservation(0x1_0000, 4);
        tracker
            .record_commit(range(0x1_0000, 1), PageFlags::READ, MemoryOwner::NONE)
            .unwrap();
        assert_eq!(
            tracker.release(range(0x1_0000, 2)).err(),
            Some(AddressSpaceError::NotReserved),
            "release must match the exact reserved range"
        );
        let released = tracker.release(range(0x1_0000, 4)).unwrap();
        assert_eq!(released.committed.len(), 1);
        assert!(released.swapped.is_empty());
        tracker.push_free_range(range(0x1_0000, 4));
        assert_eq!(tracker.reuse_free_range(2 * PAGE), Some(range(0x1_0000, 2)));
        assert_eq!(tracker.reuse_free_range(2 * PAGE), Some(range(0x1_2000, 2)));
        assert_eq!(tracker.reuse_free_range(PAGE), None);
    }

    #[test]
    fn accessibility_plan_splits_protect_and_commit() {
        let mut tracker = tracker_with_reservation(0x1_0000, 4);
        tracker
            .record_commit(range(0x1_1000, 1), PageFlags::READ, MemoryOwner::NONE)
            .unwrap();
        let plan = tracker.accessibility_plan(range(0x1_0000, 4)).unwrap();
        assert_eq!(plan.protect, [range(0x1_1000, 1)]);
        assert_eq!(plan.commit, [range(0x1_0000, 1), range(0x1_2000, 2)]);
    }

    #[test]
    fn a_swapped_page_is_committed_to_the_plan_and_to_protect() {
        let mut tracker = tracker_with_reservation(0x1_0000, 4);
        tracker
            .record_commit(
                range(0x1_0000, 4),
                PageFlags::READ | PageFlags::WRITE,
                MemoryOwner::NONE,
            )
            .unwrap();
        let token = SwapToken::new(7).unwrap();
        tracker
            .record_swap_out(VirtAddr::new(0x1_1000), token)
            .unwrap();

        let plan = tracker.accessibility_plan(range(0x1_0000, 4)).unwrap();
        assert!(
            plan.commit.is_empty(),
            "a commit over the swapped page would replace its contents"
        );
        assert_eq!(
            plan.protect.iter().map(|r| r.byte_len).sum::<usize>(),
            4 * PAGE
        );

        tracker.ensure_committed(range(0x1_0000, 4)).unwrap();
        tracker
            .record_protect(range(0x1_0000, 4), PageFlags::READ)
            .unwrap();
        let entry = tracker.swap_entry(VirtAddr::new(0x1_1000)).unwrap();
        assert_eq!(entry.token, token);
        assert_eq!(entry.flags, PageFlags::READ, "the entry follows a protect");

        tracker.take_swap_entry(VirtAddr::new(0x1_1000)).unwrap();
        let regions = tracker.committed_intersections(range(0x1_0000, 4)).unwrap();
        assert!(regions.iter().all(|region| region.flags == PageFlags::READ));
    }

    #[test]
    fn decommit_reaches_a_swapped_page_and_returns_its_entry() {
        let mut tracker = tracker_with_reservation(0x1_0000, 3);
        tracker
            .record_commit(range(0x1_0000, 3), PageFlags::READ, MemoryOwner::NONE)
            .unwrap();
        let token = SwapToken::new(9).unwrap();
        tracker
            .record_swap_out(VirtAddr::new(0x1_1000), token)
            .unwrap();
        let decommitted = tracker.record_decommit(range(0x1_0000, 3)).unwrap();
        assert_eq!(
            decommitted
                .swapped
                .iter()
                .map(|e| e.token)
                .collect::<Vec<_>>(),
            [token]
        );
        assert_eq!(tracker.swapped_pages(), 0);
        assert_eq!(
            tracker.lookup(VirtAddr::new(0x1_1000)),
            ReservationLookup::Reserved
        );
    }

    #[test]
    fn the_plan_commits_only_the_uncommitted_gaps_around_a_swapped_page() {
        let mut tracker = tracker_with_reservation(0x1_0000, 5);
        tracker
            .record_commit(range(0x1_2000, 1), PageFlags::READ, MemoryOwner::NONE)
            .unwrap();
        tracker
            .record_swap_out(VirtAddr::new(0x1_2000), SwapToken::new(3).unwrap())
            .unwrap();
        let plan = tracker.accessibility_plan(range(0x1_0000, 5)).unwrap();
        assert_eq!(plan.protect, [range(0x1_2000, 1)]);
        assert_eq!(plan.commit, [range(0x1_0000, 2), range(0x1_3000, 2)]);
    }

    #[test]
    fn take_committed_intersections_clips_and_removes() {
        let mut tracker = tracker_with_reservation(0x1_0000, 4);
        tracker
            .record_commit(range(0x1_0000, 2), PageFlags::READ, MemoryOwner::NONE)
            .unwrap();
        let taken = tracker
            .take_committed_intersections(range(0x1_1000, 3))
            .unwrap();
        assert_eq!(taken.ranges, [range(0x1_1000, 1)]);
        assert!(taken.substituted.is_empty());
        assert!(matches!(
            tracker.lookup(VirtAddr::new(0x1_0000)),
            ReservationLookup::Committed(_)
        ));
        assert_eq!(
            tracker.lookup(VirtAddr::new(0x1_1000)),
            ReservationLookup::Reserved
        );
    }

    const RW: PageFlags = PageFlags::READ.union(PageFlags::WRITE);

    fn owner(raw: u64) -> MemoryOwner {
        MemoryOwner::new(raw)
    }

    /// A four-page reservation committed as two regions with different
    /// owners, like a linear memory that grew once.
    fn grown_tracker() -> ReservationTracker {
        let mut tracker = tracker_with_reservation(0x1_0000, 4);
        tracker
            .record_commit(range(0x1_0000, 2), RW, owner(7))
            .unwrap();
        tracker
            .record_commit(range(0x1_2000, 2), PageFlags::READ, owner(8))
            .unwrap();
        tracker
    }

    #[test]
    fn substitution_requires_committed_unsubstituted_pages() {
        let mut tracker = tracker_with_reservation(0x1_0000, 4);
        tracker
            .record_commit(range(0x1_0000, 2), RW, owner(7))
            .unwrap();
        assert_eq!(
            tracker.record_substitute(range(0x1_1000, 2)),
            Err(AddressSpaceError::NotCommitted),
            "a range that reaches an uncommitted page is refused"
        );
        assert_eq!(
            tracker.record_substitute(range(0x9_0000, 1)),
            Err(AddressSpaceError::NotReserved)
        );
        tracker.record_substitute(range(0x1_1000, 1)).unwrap();
        assert_eq!(
            tracker.record_substitute(range(0x1_0000, 2)),
            Err(AddressSpaceError::Substituted),
            "a range that overlaps a live substitution is refused"
        );
        assert!(matches!(
            tracker.lookup(VirtAddr::new(0x1_0000)),
            ReservationLookup::Committed(CommittedRegion {
                substituted: false,
                ..
            })
        ));
        assert!(matches!(
            tracker.lookup(VirtAddr::new(0x1_1000)),
            ReservationLookup::Committed(CommittedRegion {
                substituted: true,
                ..
            })
        ));
    }

    #[test]
    fn substitution_keeps_flags_and_owner_per_region_and_skips_accounting() {
        let mut tracker = grown_tracker();
        assert_eq!(tracker.owned_resident_bytes(owner(7)), 2 * PAGE as u64);
        tracker.record_substitute(range(0x1_1000, 2)).unwrap();
        // The substituted pages are in both regions: one page of each.
        assert_eq!(tracker.owned_resident_bytes(owner(7)), PAGE as u64);
        assert_eq!(tracker.owned_resident_bytes(owner(8)), PAGE as u64);
        let regions = tracker.substituted_regions(range(0x1_1000, 2)).unwrap();
        assert_eq!(
            regions,
            [
                CommittedRegion {
                    range: range(0x1_1000, 1),
                    flags: RW,
                    owner: owner(7),
                    substituted: true,
                },
                CommittedRegion {
                    range: range(0x1_2000, 1),
                    flags: PageFlags::READ,
                    owner: owner(8),
                    substituted: true,
                },
            ]
        );
        let mut visited = Vec::new();
        tracker.owned_committed_regions(owner(7), |region| {
            visited.push(region.range);
            true
        });
        assert_eq!(
            visited,
            [range(0x1_0000, 1)],
            "scans skip substituted pages"
        );
        assert_eq!(tracker.owners(), [owner(7), owner(8)]);
    }

    #[test]
    fn protect_and_accessibility_plan_treat_substituted_pages_as_committed() {
        let mut tracker = grown_tracker();
        tracker.record_substitute(range(0x1_1000, 2)).unwrap();
        let plan = tracker.accessibility_plan(range(0x1_0000, 4)).unwrap();
        assert!(
            plan.commit.is_empty(),
            "nothing commits over a substitution"
        );
        assert_eq!(
            plan.protect.iter().map(|r| r.byte_len).sum::<usize>(),
            4 * PAGE
        );
        tracker.ensure_committed(range(0x1_0000, 4)).unwrap();
        tracker
            .record_protect(range(0x1_0000, 4), PageFlags::READ)
            .unwrap();
        let regions = tracker.committed_intersections(range(0x1_0000, 4)).unwrap();
        assert!(regions.iter().all(|region| region.flags == PageFlags::READ));
        let substituted: usize = regions
            .iter()
            .filter(|region| region.substituted)
            .map(|region| region.range.byte_len)
            .sum();
        assert_eq!(substituted, 2 * PAGE, "a protection change keeps the mark");
        let owners: Vec<MemoryOwner> = regions
            .iter()
            .filter(|region| region.range.start.raw() == 0x1_2000)
            .map(|region| region.owner)
            .collect();
        assert_eq!(owners, [owner(8)], "and the owner of each region");
    }

    #[test]
    fn decommit_reports_the_substituted_parts_and_take_forgets_them() {
        let mut tracker = grown_tracker();
        tracker.record_substitute(range(0x1_1000, 2)).unwrap();
        let removed = tracker.record_decommit(range(0x1_0000, 4)).unwrap();
        assert!(removed.swapped.is_empty());
        assert_eq!(
            removed.substituted,
            [range(0x1_1000, 2)],
            "adjacent substituted pieces of two regions are one part"
        );
        assert_eq!(
            tracker.lookup(VirtAddr::new(0x1_1000)),
            ReservationLookup::Reserved
        );

        let mut tracker = grown_tracker();
        tracker.record_substitute(range(0x1_0000, 1)).unwrap();
        let removal = tracker
            .take_committed_intersections(range(0x1_0000, 4))
            .unwrap();
        assert_eq!(removal.substituted, [range(0x1_0000, 1)]);
        assert_eq!(
            removal.ranges.iter().map(|r| r.byte_len).sum::<usize>(),
            4 * PAGE
        );
        assert_eq!(tracker.owned_resident_bytes(owner(7)), 0);
    }

    #[test]
    fn release_hands_back_substituted_regions_marked() {
        let mut tracker = grown_tracker();
        tracker.record_substitute(range(0x1_2000, 1)).unwrap();
        let released = tracker.release(range(0x1_0000, 4)).unwrap();
        let marked: Vec<VirtRange> = released
            .committed
            .iter()
            .filter(|region| region.substituted)
            .map(|region| region.range)
            .collect();
        assert_eq!(marked, [range(0x1_2000, 1)]);
    }

    #[test]
    fn partial_decommit_leaves_the_rest_of_a_substitution_substituted() {
        let mut tracker = grown_tracker();
        tracker.record_substitute(range(0x1_0000, 4)).unwrap();
        let removed = tracker.record_decommit(range(0x1_1000, 1)).unwrap();
        assert_eq!(removed.substituted, [range(0x1_1000, 1)]);
        // The two remainders are still substituted and can still be
        // withdrawn, each on its own.
        tracker.record_withdraw(range(0x1_0000, 1)).unwrap();
        tracker.record_restore(range(0x1_2000, 2)).unwrap();
        assert_eq!(
            tracker.record_withdraw(range(0x1_2000, 1)),
            Err(AddressSpaceError::NotSubstituted)
        );
    }

    #[test]
    fn restore_clears_the_mark_and_withdraw_uncommits() {
        let mut tracker = grown_tracker();
        tracker.record_substitute(range(0x1_1000, 2)).unwrap();
        assert_eq!(
            tracker.record_restore(range(0x1_0000, 2)),
            Err(AddressSpaceError::NotSubstituted),
            "a range that is only partly substituted is not restorable"
        );
        tracker.record_restore(range(0x1_1000, 2)).unwrap();
        assert_eq!(
            tracker.record_restore(range(0x1_1000, 2)),
            Err(AddressSpaceError::NotSubstituted)
        );
        assert_eq!(tracker.owned_resident_bytes(owner(7)), 2 * PAGE as u64);
        assert_eq!(tracker.owned_resident_bytes(owner(8)), 2 * PAGE as u64);

        tracker.record_substitute(range(0x1_1000, 2)).unwrap();
        tracker.record_withdraw(range(0x1_1000, 2)).unwrap();
        assert_eq!(
            tracker.lookup(VirtAddr::new(0x1_1000)),
            ReservationLookup::Reserved
        );
        assert_eq!(
            tracker.lookup(VirtAddr::new(0x1_2000)),
            ReservationLookup::Reserved
        );
        // Reserved-uncommitted is what a commit expects to find.
        tracker.precheck_commit(range(0x1_1000, 2)).unwrap();
        assert!(matches!(
            tracker.lookup(VirtAddr::new(0x1_0000)),
            ReservationLookup::Committed(_)
        ));
    }

    #[test]
    fn a_substituted_page_is_never_swapped_out() {
        let mut tracker = tracker_with_reservation(0x1_0000, 2);
        tracker
            .record_commit(range(0x1_0000, 2), RW, owner(7))
            .unwrap();
        tracker.record_substitute(range(0x1_0000, 1)).unwrap();
        let token = SwapToken::new(1).unwrap();
        assert_eq!(
            tracker.record_swap_out(VirtAddr::new(0x1_0000), token),
            Err(AddressSpaceError::Substituted)
        );
        tracker
            .record_swap_out(VirtAddr::new(0x1_1000), token)
            .unwrap();
    }

    #[test]
    fn substitution_covers_swapped_pages_and_keeps_their_flags_and_owner() {
        let mut tracker = grown_tracker();
        let token_a = SwapToken::new(1).unwrap();
        let token_b = SwapToken::new(2).unwrap();
        tracker
            .record_swap_out(VirtAddr::new(0x1_1000), token_a)
            .unwrap();
        tracker
            .record_swap_out(VirtAddr::new(0x1_2000), token_b)
            .unwrap();
        tracker.check_substitutable(range(0x1_0000, 4)).unwrap();
        let swapped = tracker.record_substitute(range(0x1_0000, 4)).unwrap();
        assert_eq!(
            swapped,
            [
                SwapEntry {
                    addr: VirtAddr::new(0x1_1000),
                    token: token_a,
                    flags: RW,
                    owner: owner(7),
                },
                SwapEntry {
                    addr: VirtAddr::new(0x1_2000),
                    token: token_b,
                    flags: PageFlags::READ,
                    owner: owner(8),
                },
            ],
            "the swap entries come back for the caller to release"
        );
        assert!(
            tracker.take_swap_entries_in(range(0x1_0000, 4)).is_empty(),
            "nothing is left swapped"
        );
        let regions = tracker.substituted_regions(range(0x1_0000, 4)).unwrap();
        let flags_and_owner = |addr: usize| {
            regions
                .iter()
                .find(|region| region.range.contains(VirtAddr::new(addr)))
                .map(|region| (region.flags, region.owner))
        };
        assert_eq!(flags_and_owner(0x1_0000), Some((RW, owner(7))));
        assert_eq!(flags_and_owner(0x1_1000), Some((RW, owner(7))));
        assert_eq!(flags_and_owner(0x1_2000), Some((PageFlags::READ, owner(8))));
        assert_eq!(flags_and_owner(0x1_3000), Some((PageFlags::READ, owner(8))));
        assert_eq!(
            regions.iter().map(|r| r.range.byte_len).sum::<usize>(),
            4 * PAGE
        );
        tracker.record_restore(range(0x1_0000, 4)).unwrap();
        assert_eq!(tracker.owned_resident_bytes(owner(7)), 2 * PAGE as u64);
        assert_eq!(tracker.owned_resident_bytes(owner(8)), 2 * PAGE as u64);
    }

    #[test]
    fn substitution_refuses_a_hole_beside_a_swapped_page_and_changes_nothing() {
        let mut tracker = tracker_with_reservation(0x1_0000, 4);
        tracker
            .record_commit(range(0x1_0000, 3), RW, owner(7))
            .unwrap();
        let token = SwapToken::new(1).unwrap();
        tracker
            .record_swap_out(VirtAddr::new(0x1_1000), token)
            .unwrap();
        // The fourth page was never committed.
        assert_eq!(
            tracker.record_substitute(range(0x1_0000, 4)),
            Err(AddressSpaceError::NotCommitted)
        );
        assert_eq!(
            tracker.take_swap_entries_in(range(0x1_0000, 4)).len(),
            1,
            "the swap entry is still filed"
        );
    }

    #[test]
    fn substitution_refuses_a_substituted_page_beside_a_swapped_one() {
        let mut tracker = tracker_with_reservation(0x1_0000, 3);
        tracker
            .record_commit(range(0x1_0000, 3), RW, owner(7))
            .unwrap();
        tracker.record_substitute(range(0x1_2000, 1)).unwrap();
        let token = SwapToken::new(1).unwrap();
        tracker
            .record_swap_out(VirtAddr::new(0x1_0000), token)
            .unwrap();
        assert_eq!(
            tracker.record_substitute(range(0x1_0000, 3)),
            Err(AddressSpaceError::Substituted)
        );
        assert_eq!(tracker.take_swap_entries_in(range(0x1_0000, 3)).len(), 1);
    }

    #[test]
    fn ranges_outside_cuts_the_excluded_parts_out() {
        let whole = range(0x1_0000, 6);
        assert_eq!(ranges_outside(whole, &[]), [whole]);
        assert_eq!(
            ranges_outside(whole, &[range(0x1_1000, 1), range(0x1_3000, 2)]),
            [range(0x1_0000, 1), range(0x1_2000, 1), range(0x1_5000, 1)]
        );
        assert!(ranges_outside(whole, &[whole]).is_empty());
        assert_eq!(
            ranges_outside(
                range(0x1_2000, 2),
                &[range(0x1_0000, 3), range(0x9_0000, 1)]
            ),
            [range(0x1_3000, 1)],
            "holes outside the range are ignored and straddling ones are clipped"
        );
    }

    #[test]
    fn va_cursor_carves_until_window_end() {
        let cursor = VaCursor::new(0x1_0000, 0x1_0000 + 4 * PAGE);
        assert_eq!(cursor.carve(3 * PAGE), Some(range(0x1_0000, 3)));
        assert_eq!(cursor.carve(2 * PAGE), None);
        assert_eq!(cursor.carve(PAGE), Some(range(0x1_3000, 1)));
    }
}
