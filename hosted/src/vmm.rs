//! Hosted virtual address space backed by `mmap`/`mprotect`.
//!
//! On the hosted backend, "physical" frames and virtual addresses share
//! the host process's address space. Each call to
//! [`AddressSpace::reserve`] runs `mmap(NULL, ..., PROT_NONE,
//! MAP_PRIVATE | MAP_ANONYMOUS)` and lets the host kernel pick the
//! virtual address. `commit`/`decommit`/`protect` translate to
//! `mprotect`. Decommit additionally calls `madvise` so the host kernel
//! actually returns the pages to its free pool — without that, RSS
//! grows monotonically across kill/respawn cycles, which would defeat
//! the OOM killer's purpose.
//!
//! The hal contract is 4 KiB-granular while the host page size may be
//! larger (16 KiB on Apple Silicon macOS, 64 KiB on some arm64 Linux
//! kernels). Bookkeeping stays hal-granular in [`ReservationTracker`];
//! hardware protection is applied per *host* page as the union of the
//! flags of every committed hal region overlapping that page, and
//! `madvise` reclaim only happens once a host page has no committed
//! region left. `translate` always answers from the hal-granular
//! bookkeeping.
//!
//! The reservation tracker lives in a `Mutex`; the mutex is held only
//! across bookkeeping and the libc syscall, never across an `await`,
//! so it does not violate AGENTS §4 even though the embedding kernel
//! runs an async executor.

use std::ptr;
use std::sync::Mutex;

use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{
    AddressSpace, AddressSpaceError, PageFlags, Translation, VirtAddr, VirtRange,
};
use helios_kernel::{CommittedRegion, ReservationLookup, ReservationTracker, validate_range};

const PAGE_SIZE: usize = PhysFrame::SIZE;

pub struct HostedAddressSpace {
    reservations: Mutex<ReservationTracker>,
    host_page: usize,
}

impl HostedAddressSpace {
    pub fn new() -> Self {
        Self {
            reservations: Mutex::new(ReservationTracker::new()),
            host_page: host_page_size(),
        }
    }

    /// Host-page hull of `virt`, clipped to its reservation. `mmap`
    /// bases are host-page aligned, so the clipped hull start is too.
    fn host_hull(
        &self,
        tracker: &ReservationTracker,
        virt: VirtRange,
    ) -> Result<VirtRange, AddressSpaceError> {
        let reservation = tracker.reservation_range(virt)?;
        let start = align_down(virt.start.raw(), self.host_page).max(reservation.start.raw());
        let end = align_up(virt.end().raw(), self.host_page).min(reservation.end().raw());
        Ok(VirtRange::new(VirtAddr::new(start), end - start))
    }

    /// Re-applies hardware protection for every host page in the hull of
    /// `virt`, from the current bookkeeping: each host page gets the
    /// union of the flags of the committed hal regions overlapping it,
    /// or `PROT_NONE` when none remain (with `madvise` reclaim if
    /// `reclaim` is set).
    fn apply_host_protection(
        &self,
        tracker: &ReservationTracker,
        virt: VirtRange,
        reclaim: bool,
    ) -> Result<(), AddressSpaceError> {
        let reservation = tracker.reservation_range(virt)?;
        let hull = self.host_hull(tracker, virt)?;
        let mut cursor = hull.start.raw();
        while cursor < hull.end().raw() {
            let page_end = (cursor + self.host_page).min(reservation.end().raw());
            let page_range = VirtRange::new(VirtAddr::new(cursor), page_end - cursor);
            let regions = tracker.committed_intersections(page_range)?;
            if regions.is_empty() {
                if reclaim {
                    unsafe {
                        #[cfg(target_os = "linux")]
                        libc::madvise(cursor as *mut _, page_range.byte_len, libc::MADV_DONTNEED);
                        #[cfg(target_os = "macos")]
                        libc::madvise(cursor as *mut _, page_range.byte_len, libc::MADV_FREE);
                    }
                }
                set_protection(page_range, libc::PROT_NONE)?;
            } else {
                let union = regions
                    .iter()
                    .fold(PageFlags::empty(), |acc, region| acc | region.flags);
                set_protection(page_range, page_flags_to_prot(union))?;
            }
            cursor += self.host_page;
        }
        Ok(())
    }
}

impl Default for HostedAddressSpace {
    fn default() -> Self {
        Self::new()
    }
}

impl AddressSpace for HostedAddressSpace {
    fn reserve(&self, byte_len: usize) -> Result<VirtRange, AddressSpaceError> {
        if byte_len == 0 {
            return Err(AddressSpaceError::EmptyRange);
        }
        if !byte_len.is_multiple_of(PAGE_SIZE) {
            return Err(AddressSpaceError::Misaligned);
        }
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                byte_len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(AddressSpaceError::OutOfFrames);
        }
        let range = VirtRange::new(VirtAddr::new(ptr as usize), byte_len);
        self.reservations.lock().unwrap().reserve(range);
        Ok(range)
    }

    fn release(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        self.reservations.lock().unwrap().release(virt)?;
        let result = unsafe { libc::munmap(virt.start.raw() as *mut _, virt.byte_len) };
        if result != 0 {
            return Err(AddressSpaceError::NotReserved);
        }
        Ok(())
    }

    fn commit(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let mut reservations = self.reservations.lock().unwrap();
        reservations.precheck_commit(virt)?;
        reservations.record_commit(virt, flags)?;
        if let Err(error) = self.apply_host_protection(&reservations, virt, false) {
            reservations.record_decommit(virt)?;
            return Err(error);
        }
        Ok(())
    }

    fn decommit(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let mut reservations = self.reservations.lock().unwrap();
        reservations.ensure_committed(virt)?;
        reservations.record_decommit(virt)?;
        self.apply_host_protection(&reservations, virt, true)
    }

    fn protect(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let mut reservations = self.reservations.lock().unwrap();
        reservations.ensure_committed(virt)?;
        let previous = reservations.committed_intersections(virt)?;
        reservations.record_protect(virt, flags)?;
        if let Err(error) = self.apply_host_protection(&reservations, virt, false) {
            restore_committed(&mut reservations, virt, &previous)?;
            return Err(error);
        }
        Ok(())
    }

    fn relocate(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let reservations = self.reservations.lock().unwrap();
        reservations.ensure_committed(virt)?;
        // Hosted has no concept of "physical frame allocation" — libc
        // owns the page-level placement under our mmap. The only
        // behaviour we can faithfully reproduce is to round-trip the
        // bytes through a temporary buffer, keeping the virtual base
        // stable. The reclaim below works on whole host pages, so every
        // committed region in the host-page hull is snapshotted and
        // restored, not just the requested range.
        let hull = self.host_hull(&reservations, virt)?;
        let snapshots: Vec<CommittedRegion> = reservations.committed_intersections(hull)?;
        set_protection(hull, libc::PROT_READ | libc::PROT_WRITE)?;
        let contents: Vec<Vec<u8>> = snapshots
            .iter()
            .map(|region| unsafe {
                core::slice::from_raw_parts(
                    region.range.start.raw() as *const u8,
                    region.range.byte_len,
                )
                .to_vec()
            })
            .collect();
        unsafe {
            #[cfg(target_os = "linux")]
            libc::madvise(
                hull.start.raw() as *mut _,
                hull.byte_len,
                libc::MADV_DONTNEED,
            );
            #[cfg(target_os = "macos")]
            libc::madvise(hull.start.raw() as *mut _, hull.byte_len, libc::MADV_FREE);
        }
        for (region, bytes) in snapshots.iter().zip(&contents) {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    region.range.start.raw() as *mut u8,
                    region.range.byte_len,
                );
            }
        }
        self.apply_host_protection(&reservations, hull, false)
    }

    fn translate(&self, addr: VirtAddr) -> Translation {
        match self.reservations.lock().unwrap().lookup(addr) {
            ReservationLookup::Committed(region) => Translation::Committed {
                frame: PhysFrame::from_phys_addr(addr.page_floor().raw()),
                flags: region.flags,
            },
            ReservationLookup::Reserved => Translation::Reserved,
            ReservationLookup::Unreserved => Translation::Unmapped,
        }
    }
}

/// Restores the pre-protect committed regions after a failed protect.
fn restore_committed(
    tracker: &mut ReservationTracker,
    virt: VirtRange,
    previous: &[CommittedRegion],
) -> Result<(), AddressSpaceError> {
    tracker.record_decommit(virt)?;
    for region in previous {
        tracker.record_commit(region.range, region.flags)?;
    }
    Ok(())
}

fn host_page_size() -> usize {
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(value > 0, "sysconf(_SC_PAGESIZE) failed");
    value as usize
}

fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

/// `range.start` must be host-page aligned; the kernel rounds the length
/// up to the host page internally.
fn set_protection(range: VirtRange, prot: i32) -> Result<(), AddressSpaceError> {
    let result = unsafe { libc::mprotect(range.start.raw() as *mut _, range.byte_len, prot) };
    if result == 0 {
        Ok(())
    } else {
        Err(AddressSpaceError::InvalidFlags)
    }
}

fn page_flags_to_prot(flags: PageFlags) -> i32 {
    let mut prot = 0i32;
    if flags.contains(PageFlags::READ) {
        prot |= libc::PROT_READ;
    }
    if flags.contains(PageFlags::WRITE) {
        prot |= libc::PROT_WRITE;
    }
    if flags.contains(PageFlags::EXECUTE) {
        prot |= libc::PROT_EXEC;
    }
    if prot == 0 {
        prot = libc::PROT_NONE;
    }
    prot
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_aligned_len(pages: usize) -> usize {
        pages * PAGE_SIZE
    }

    #[test]
    fn reserve_and_release_round_trip() {
        let address_space = HostedAddressSpace::new();
        let range = address_space.reserve(page_aligned_len(4)).unwrap();
        assert_eq!(range.byte_len, page_aligned_len(4));
        assert_eq!(address_space.translate(range.start), Translation::Reserved);
        address_space.release(range).unwrap();
        assert_eq!(address_space.translate(range.start), Translation::Unmapped);
    }

    #[test]
    fn commit_grants_read_write_then_decommit_removes_backing() {
        let address_space = HostedAddressSpace::new();
        let range = address_space.reserve(page_aligned_len(2)).unwrap();
        address_space
            .commit(range, PageFlags::READ | PageFlags::WRITE)
            .unwrap();

        let value: *mut u32 = range.start.raw() as *mut u32;
        unsafe {
            value.write_volatile(0xdead_beef);
            assert_eq!(value.read_volatile(), 0xdead_beef);
        }

        match address_space.translate(range.start) {
            Translation::Committed { flags, .. } => {
                assert!(flags.contains(PageFlags::READ));
                assert!(flags.contains(PageFlags::WRITE));
            }
            other => panic!("expected Committed, got {other:?}"),
        }

        address_space.decommit(range).unwrap();
        assert_eq!(address_space.translate(range.start), Translation::Reserved);
        address_space.release(range).unwrap();
    }

    #[test]
    fn adjacent_hal_pages_commit_separately_within_one_host_page() {
        let address_space = HostedAddressSpace::new();
        let range = address_space.reserve(page_aligned_len(2)).unwrap();
        let first = VirtRange::new(range.start, PAGE_SIZE);
        let second = VirtRange::new(VirtAddr::new(range.start.raw() + PAGE_SIZE), PAGE_SIZE);
        address_space
            .commit(first, PageFlags::READ | PageFlags::WRITE)
            .unwrap();
        address_space
            .commit(second, PageFlags::READ | PageFlags::WRITE)
            .unwrap();
        let pointer = second.start.raw() as *mut u8;
        unsafe {
            pointer.write_volatile(0x5a);
            assert_eq!(pointer.read_volatile(), 0x5a);
        }
        address_space.decommit(first).unwrap();
        assert_eq!(address_space.translate(first.start), Translation::Reserved);
        unsafe {
            assert_eq!(
                pointer.read_volatile(),
                0x5a,
                "partial host-page decommit must not disturb the neighbor region"
            );
        }
        address_space.release(range).unwrap();
    }

    #[test]
    fn reserve_rejects_misaligned_size() {
        let address_space = HostedAddressSpace::new();
        assert_eq!(
            address_space.reserve(123),
            Err(AddressSpaceError::Misaligned)
        );
    }

    #[test]
    fn release_unknown_range_is_an_error() {
        let address_space = HostedAddressSpace::new();
        let bogus = VirtRange::new(VirtAddr::new(0x1000), PAGE_SIZE);
        assert_eq!(
            address_space.release(bogus),
            Err(AddressSpaceError::NotReserved)
        );
    }

    #[test]
    fn passes_hal_conformance_suite() {
        use helios_hal::vmm_test_harness::{AddressSpaceFixture, run_conformance};
        run_conformance(AddressSpaceFixture {
            fresh: HostedAddressSpace::new,
            verify_writes: true,
        });
    }

    #[test]
    fn relocate_preserves_committed_bytes() {
        use helios_hal::vmm::AddressSpace;
        let address_space = HostedAddressSpace::new();
        let range = address_space.reserve(page_aligned_len(2)).unwrap();
        address_space
            .commit(range, PageFlags::READ | PageFlags::WRITE)
            .unwrap();
        let pointer = range.start.raw() as *mut u8;
        unsafe {
            for byte in 0..(range.byte_len.min(256)) {
                pointer.add(byte).write_volatile((byte & 0xff) as u8);
            }
        }
        address_space.relocate(range).expect("relocate succeeds");
        unsafe {
            for byte in 0..(range.byte_len.min(256)) {
                assert_eq!(pointer.add(byte).read_volatile(), (byte & 0xff) as u8);
            }
        }
        address_space.release(range).unwrap();
    }

    #[test]
    fn relocate_preserves_readonly_committed_bytes_and_flags() {
        use helios_hal::vmm::AddressSpace;
        let address_space = HostedAddressSpace::new();
        let range = address_space.reserve(page_aligned_len(1)).unwrap();
        address_space
            .commit(range, PageFlags::READ | PageFlags::WRITE)
            .unwrap();
        let pointer = range.start.raw() as *mut u8;
        unsafe {
            for byte in 0..128 {
                pointer.add(byte).write_volatile((byte ^ 0xa5) as u8);
            }
        }
        address_space.protect(range, PageFlags::READ).unwrap();
        address_space.relocate(range).expect("relocate succeeds");
        unsafe {
            for byte in 0..128 {
                assert_eq!(pointer.add(byte).read_volatile(), (byte ^ 0xa5) as u8);
            }
        }
        match address_space.translate(range.start) {
            Translation::Committed { flags, .. } => assert_eq!(flags, PageFlags::READ),
            other => panic!("expected committed read-only translation, got {other:?}"),
        }
        address_space.release(range).unwrap();
    }
}
