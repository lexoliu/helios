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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommittedRegion {
    pub range: VirtRange,
    pub flags: PageFlags,
    pub owner: MemoryOwner,
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
        });
        Ok(orphaned)
    }

    /// Validates that `virt` lies inside a reservation and is fully
    /// covered by committed regions.
    pub fn ensure_committed(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        let reservation = self.find(virt)?;
        if !committed_regions_cover(&reservation.committed, virt) {
            return Err(AddressSpaceError::NotCommitted);
        }
        Ok(())
    }

    /// Removes `virt` from the committed set after validating coverage,
    /// returning any swapped pages inside it whose tokens the caller
    /// must release.
    pub fn record_decommit(
        &mut self,
        virt: VirtRange,
    ) -> Result<Vec<SwapEntry>, AddressSpaceError> {
        self.ensure_committed(virt)?;
        let reservation = self.find_mut(virt)?;
        remove_committed_range(&mut reservation.committed, virt);
        Ok(take_swapped_range(&mut reservation.swapped, virt))
    }

    /// Replaces the committed flags for `virt` after the backend changed
    /// the hardware protection. Coverage must have been prechecked with
    /// [`Self::ensure_committed`].
    pub fn record_protect(
        &mut self,
        virt: VirtRange,
        flags: PageFlags,
    ) -> Result<(), AddressSpaceError> {
        let reservation = self.find_mut(virt)?;
        let owner = reservation
            .committed
            .iter()
            .find(|region| ranges_overlap(region.range, virt))
            .map(|region| region.owner)
            .unwrap_or(MemoryOwner::NONE);
        remove_committed_range(&mut reservation.committed, virt);
        reservation.committed.push(CommittedRegion {
            range: virt,
            flags,
            owner,
        });
        // A protection change applies to the swapped pages too: they go
        // back with the flags the runtime last asked for.
        for entry in &mut reservation.swapped {
            if virt.contains(entry.addr) {
                entry.flags = flags;
            }
        }
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
        Ok(reservation
            .committed
            .iter()
            .filter_map(|region| {
                range_intersection(region.range, virt).map(|range| CommittedRegion {
                    range,
                    flags: region.flags,
                    owner: region.owner,
                })
            })
            .collect())
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
    /// for callers that decommit hardware state afterwards.
    pub fn take_committed_intersections(
        &mut self,
        virt: VirtRange,
    ) -> Result<Vec<VirtRange>, AddressSpaceError> {
        let reservation = self.find_mut(virt)?;
        let overlaps: Vec<VirtRange> = reservation
            .committed
            .iter()
            .filter_map(|region| range_intersection(region.range, virt))
            .collect();
        for subrange in &overlaps {
            remove_committed_range(&mut reservation.committed, *subrange);
        }
        Ok(overlaps)
    }

    /// Splits `virt` into already-committed sub-ranges (to re-protect)
    /// and uncommitted gaps (to commit).
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
            if let Some(region) = reservation
                .committed
                .iter()
                .find(|region| region.range.contains(VirtAddr::new(cursor)))
                .copied()
            {
                let next = region.range.end().raw().min(end);
                plan.protect
                    .push(VirtRange::new(VirtAddr::new(cursor), next - cursor));
                cursor = next;
                continue;
            }
            let next_committed_start = reservation
                .committed
                .iter()
                .filter_map(|region| {
                    let start = region.range.start.raw();
                    (cursor < start && start < end).then_some(start)
                })
                .min()
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

    /// Bytes committed for `owner` across every reservation.
    pub fn owned_resident_bytes(&self, owner: MemoryOwner) -> u64 {
        self.reservations
            .iter()
            .flat_map(|reservation| reservation.committed.iter())
            .filter(|region| region.owner == owner)
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
    /// fewest scans for the most reclaimed pages.
    pub fn owned_committed_regions<Visit>(&self, owner: MemoryOwner, mut visit: Visit)
    where
        Visit: FnMut(CommittedRegion) -> bool,
    {
        let mut regions: Vec<CommittedRegion> = self
            .reservations
            .iter()
            .flat_map(|reservation| reservation.committed.iter())
            .filter(|region| region.owner == owner)
            .copied()
            .collect();
        regions.sort_by_key(|region| core::cmp::Reverse(region.range.byte_len));
        for region in regions {
            if !visit(region) {
                return;
            }
        }
    }

    /// Every owner that currently has committed pages.
    pub fn owners(&self) -> Vec<MemoryOwner> {
        let mut owners: Vec<MemoryOwner> = self
            .reservations
            .iter()
            .flat_map(|reservation| reservation.committed.iter())
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

fn committed_regions_cover(regions: &[CommittedRegion], range: VirtRange) -> bool {
    let mut cursor = range.start.raw();
    let end = range.end().raw();
    while cursor < end {
        let Some(region) = regions
            .iter()
            .find(|region| region.range.contains(VirtAddr::new(cursor)))
        else {
            return false;
        };
        cursor = region.range.end().raw().min(end);
    }
    true
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
            });
        }
        if range.end().raw() < region.range.end().raw() {
            regions.push(CommittedRegion {
                range: VirtRange::new(range.end(), region.range.end().raw() - range.end().raw()),
                flags: region.flags,
                owner: region.owner,
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
            tracker.record_decommit(range(0x1_1000, 1)),
            Err(AddressSpaceError::NotCommitted)
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
    fn take_committed_intersections_clips_and_removes() {
        let mut tracker = tracker_with_reservation(0x1_0000, 4);
        tracker
            .record_commit(range(0x1_0000, 2), PageFlags::READ, MemoryOwner::NONE)
            .unwrap();
        let taken = tracker
            .take_committed_intersections(range(0x1_1000, 3))
            .unwrap();
        assert_eq!(taken, [range(0x1_1000, 1)]);
        assert!(matches!(
            tracker.lookup(VirtAddr::new(0x1_0000)),
            ReservationLookup::Committed(_)
        ));
        assert_eq!(
            tracker.lookup(VirtAddr::new(0x1_1000)),
            ReservationLookup::Reserved
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
