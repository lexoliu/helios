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
//! The reservation table itself is a `Mutex<Vec<Reservation>>`; the
//! mutex is held only across the page-table walk and the libc syscall,
//! never across an `await`, so it does not violate AGENTS §4 even
//! though the embedding kernel runs an async executor.

use std::ptr;
use std::sync::Mutex;

use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{
    AddressSpace, AddressSpaceError, PageFlags, Translation, VirtAddr, VirtRange,
};

const PAGE_SIZE: usize = PhysFrame::SIZE;

pub struct HostedAddressSpace {
    reservations: Mutex<Vec<Reservation>>,
}

struct Reservation {
    range: VirtRange,
    committed: Vec<CommittedRegion>,
}

#[derive(Clone, Copy)]
struct CommittedRegion {
    range: VirtRange,
    flags: PageFlags,
}

impl HostedAddressSpace {
    pub fn new() -> Self {
        Self {
            reservations: Mutex::new(Vec::new()),
        }
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
        self.reservations.lock().unwrap().push(Reservation {
            range,
            committed: Vec::new(),
        });
        Ok(range)
    }

    fn release(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        let mut reservations = self.reservations.lock().unwrap();
        let index = reservations
            .iter()
            .position(|reservation| reservation.range == virt)
            .ok_or(AddressSpaceError::NotReserved)?;
        let reservation = reservations.swap_remove(index);
        drop(reservations);
        let result = unsafe { libc::munmap(reservation.range.start.raw() as *mut _, reservation.range.byte_len) };
        if result != 0 {
            return Err(AddressSpaceError::NotReserved);
        }
        Ok(())
    }

    fn commit(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let prot = page_flags_to_prot(flags);
        let mut reservations = self.reservations.lock().unwrap();
        let reservation = find_reservation_mut(&mut reservations, virt)?;
        let mprotect_result =
            unsafe { libc::mprotect(virt.start.raw() as *mut _, virt.byte_len, prot) };
        if mprotect_result != 0 {
            return Err(AddressSpaceError::InvalidFlags);
        }
        reservation.committed.push(CommittedRegion { range: virt, flags });
        Ok(())
    }

    fn decommit(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let mut reservations = self.reservations.lock().unwrap();
        let reservation = find_reservation_mut(&mut reservations, virt)?;
        unsafe {
            #[cfg(target_os = "linux")]
            libc::madvise(virt.start.raw() as *mut _, virt.byte_len, libc::MADV_DONTNEED);
            #[cfg(target_os = "macos")]
            libc::madvise(virt.start.raw() as *mut _, virt.byte_len, libc::MADV_FREE);
            if libc::mprotect(virt.start.raw() as *mut _, virt.byte_len, libc::PROT_NONE) != 0 {
                return Err(AddressSpaceError::InvalidFlags);
            }
        }
        reservation
            .committed
            .retain(|region| !ranges_overlap(region.range, virt));
        Ok(())
    }

    fn protect(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let prot = page_flags_to_prot(flags);
        let mut reservations = self.reservations.lock().unwrap();
        let reservation = find_reservation_mut(&mut reservations, virt)?;
        if !reservation
            .committed
            .iter()
            .any(|region| range_contains(region.range, virt))
        {
            return Err(AddressSpaceError::NotCommitted);
        }
        let mprotect_result =
            unsafe { libc::mprotect(virt.start.raw() as *mut _, virt.byte_len, prot) };
        if mprotect_result != 0 {
            return Err(AddressSpaceError::InvalidFlags);
        }
        for region in reservation.committed.iter_mut() {
            if region.range == virt {
                region.flags = flags;
            }
        }
        Ok(())
    }

    fn translate(&self, addr: VirtAddr) -> Translation {
        let reservations = self.reservations.lock().unwrap();
        for reservation in reservations.iter() {
            if !reservation.range.contains(addr) {
                continue;
            }
            for region in &reservation.committed {
                if region.range.contains(addr) {
                    return Translation::Committed {
                        frame: PhysFrame::from_phys_addr(addr.page_floor().raw()),
                        flags: region.flags,
                    };
                }
            }
            return Translation::Reserved;
        }
        Translation::Unmapped
    }
}

fn validate_range(virt: VirtRange) -> Result<(), AddressSpaceError> {
    if virt.byte_len == 0 {
        return Err(AddressSpaceError::EmptyRange);
    }
    if !virt.is_page_aligned() {
        return Err(AddressSpaceError::Misaligned);
    }
    Ok(())
}

fn find_reservation_mut<'a>(
    reservations: &'a mut Vec<Reservation>,
    virt: VirtRange,
) -> Result<&'a mut Reservation, AddressSpaceError> {
    reservations
        .iter_mut()
        .find(|reservation| range_contains(reservation.range, virt))
        .ok_or(AddressSpaceError::NotReserved)
}

fn range_contains(outer: VirtRange, inner: VirtRange) -> bool {
    outer.start.raw() <= inner.start.raw() && inner.end().raw() <= outer.end().raw()
}

fn ranges_overlap(a: VirtRange, b: VirtRange) -> bool {
    a.start.raw() < b.end().raw() && b.start.raw() < a.end().raw()
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
}
