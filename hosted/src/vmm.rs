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

use std::ffi::CString;
use std::ptr;
use std::sync::Mutex;

use helios_hal::device::{DeviceRegion, DmaPlacement};
use helios_hal::iommu::PhysicalRange;
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{
    AddressSpace, AddressSpaceError, PageAge, PageFlags, SwapToken, Translation, VirtAddr,
    VirtRange,
};
use helios_hal::{align_down, align_up};
use helios_kernel::{
    CommittedRegion, MemoryOwner, ReservationLookup, ReservationTracker, SwapEntry,
    current_user_memory_owner, validate_range,
};

const PAGE_SIZE: usize = PhysFrame::SIZE;

/// One run [`AddressSpace::allocate_run`] took: host memory that can
/// appear at a second address, because it is shared and file-backed.
struct HostedRun {
    /// Where the backend's own view of the run lives, which is what a
    /// hosted "physical address" is.
    base: usize,
    /// Bytes the run was asked for.
    bytes: usize,
    /// Bytes mapped, which is `bytes` up to the host page.
    mapped_bytes: usize,
    /// The unlinked temporary file the run's pages live in. A
    /// substitution maps this file at the span it shows the run at.
    descriptor: i32,
    /// Who the run is charged to.
    owner: MemoryOwner,
}

pub struct HostedAddressSpace {
    reservations: Mutex<ReservationTracker>,
    /// Runs taken and not yet freed. Lock order is `reservations` then
    /// `runs`; nothing takes `runs` and then `reservations`.
    runs: Mutex<Vec<HostedRun>>,
    host_page: usize,
    /// Swap tokens whose pages went away underneath them. Same reason
    /// as the bare-metal backends: releasing an extent is asynchronous
    /// and this runs under the reservation lock.
    orphans: Mutex<Vec<SwapToken>>,
    /// The processor commits are attributed to. The hosted backend runs
    /// the kernel on one processor, so there is one.
    processor: helios_hal::cpu::ProcessorId,
}

impl HostedAddressSpace {
    pub fn new() -> Self {
        Self {
            reservations: Mutex::new(ReservationTracker::new()),
            runs: Mutex::new(Vec::new()),
            host_page: host_page_size(),
            orphans: Mutex::new(Vec::new()),
            processor: helios_hal::cpu::ProcessorId::new(0),
        }
    }

    fn orphan(&self, entries: Vec<SwapEntry>) {
        if entries.is_empty() {
            return;
        }
        self.orphans
            .lock()
            .unwrap()
            .extend(entries.into_iter().map(|entry| entry.token));
    }

    /// Whether `virt` starts and ends on host pages: the unit a
    /// substitution changes a mapping at.
    fn require_host_aligned(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        if virt.start.raw().is_multiple_of(self.host_page)
            && virt.byte_len.is_multiple_of(self.host_page)
        {
            Ok(())
        } else {
            Err(AddressSpaceError::Misaligned)
        }
    }

    /// Show `descriptor` at `offset` over `virt`, replacing the pages
    /// there in one `MAP_FIXED` mapping, then mark the range substituted.
    ///
    /// The reservation lock is held from the check that every page of
    /// `virt` is committed or swapped out, and none substituted, to the
    /// bookkeeping. The host replaces the old pages atomically, so no
    /// thread ever finds the range unmapped, and there is no other
    /// processor to shoot down: the host's `mmap` has published the
    /// change to every thread by the time it returns.
    fn substitute_shared(
        &self,
        virt: VirtRange,
        descriptor: i32,
        offset: libc::off_t,
        flags: PageFlags,
        refused: AddressSpaceError,
    ) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        self.require_host_aligned(virt)?;
        let mut reservations = self.reservations.lock().unwrap();
        reservations.check_substitutable(virt)?;
        // SAFETY: `virt` is a committed or swapped-out range of a
        // reservation this address space made, so the fixed mapping
        // replaces pages that are ours to replace.
        let mapped = unsafe {
            libc::mmap(
                virt.start.raw() as *mut _,
                virt.byte_len,
                page_flags_to_prot(flags),
                libc::MAP_FIXED | libc::MAP_SHARED,
                descriptor,
                offset,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(refused);
        }
        // A swapped-out page of the range was replaced with the rest, and
        // its swap extent is no longer wanted.
        self.orphan(reservations.record_substitute(virt)?);
        Ok(())
    }

    /// Give `virt` back to the host as anonymous `PROT_NONE` address
    /// space, which is what an uncommitted part of a reservation is.
    /// Whatever was shown there is untouched, and dropping the mapping
    /// is what makes it provably unreachable.
    fn drop_substituted(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        debug_assert!(
            self.require_host_aligned(virt).is_ok(),
            "a substitution ends at host page granularity: {virt:?}"
        );
        // SAFETY: as in `substitute_shared`.
        let restored = unsafe {
            libc::mmap(
                virt.start.raw() as *mut _,
                virt.byte_len,
                libc::PROT_NONE,
                libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if restored == libc::MAP_FAILED {
            return Err(AddressSpaceError::OutOfFrames);
        }
        Ok(())
    }

    /// Makes the host pages covering `virt` readable and writable so the
    /// kernel can copy a page in or out, whatever the guest's own flags
    /// on that range are.
    fn open_for_transfer(
        &self,
        tracker: &ReservationTracker,
        virt: VirtRange,
    ) -> Result<(), AddressSpaceError> {
        let hull = self.host_hull(tracker, virt)?;
        set_protection(hull, libc::PROT_READ | libc::PROT_WRITE)
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

/// The page protection a device region has to be mapped with.
///
/// Normal memory and a register file get the same host protection: the
/// host kernel has no uncached mapping to offer a user process, and the
/// distinction only bites on real hardware. `hosted` exists to exercise
/// the kernel's bookkeeping, and it says so rather than pretending the
/// mapping is uncached.
fn device_region_flags(region: DeviceRegion) -> PageFlags {
    let mut flags = PageFlags::READ;
    if region.attributes.writable {
        flags |= PageFlags::WRITE;
    }
    flags
}

/// The smallest unit this address space can change a mapping at.
pub fn host_mapping_granule() -> u64 {
    host_page_size() as u64
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
        let released = self.reservations.lock().unwrap().release(virt)?;
        self.orphan(released.swapped);
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
        let owner = current_user_memory_owner(self.processor);
        // Committing over a swapped-out page throws its contents away;
        // the extent behind the token still has to be given back.
        self.orphan(reservations.record_commit(virt, flags, owner)?);
        if let Err(error) = self.apply_host_protection(&reservations, virt, false) {
            let _ = reservations.record_decommit(virt)?;
            return Err(error);
        }
        Ok(())
    }

    fn decommit(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let mut reservations = self.reservations.lock().unwrap();
        reservations.ensure_committed(virt)?;
        let removed = reservations.record_decommit(virt)?;
        self.orphan(removed.swapped);
        // A substitution inside the range is dropped, and what it showed
        // is left alone, as a withdraw would.
        for part in &removed.substituted {
            self.drop_substituted(*part)?;
        }
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
        // A substitution has no pool frames to round-trip.
        reservations.ensure_unsubstituted(virt)?;
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

    /// One shared, file-backed mapping the host places, charged to
    /// whoever the processor is committing for in
    /// [`Self::owned_resident_bytes`]. Physical contiguity is free here:
    /// a "physical" address is the host virtual address, so the mapping
    /// is contiguous to a device by construction, and being file-backed
    /// is what lets it appear at a second address. The pages are zero
    /// because the file is fresh. No translation changes, so no
    /// reservation lock.
    fn allocate_run(
        &self,
        bytes: usize,
        placement: DmaPlacement,
    ) -> Result<PhysicalRange, AddressSpaceError> {
        if bytes == 0 {
            return Err(AddressSpaceError::EmptyRange);
        }
        if !bytes.is_multiple_of(PAGE_SIZE) {
            return Err(AddressSpaceError::Misaligned);
        }
        let align = usize::try_from(placement.align)
            .ok()
            .filter(|align| align.is_power_of_two() && *align >= PAGE_SIZE)
            .ok_or(AddressSpaceError::Misaligned)?;
        let mapped_bytes = align_up(bytes, self.host_page);
        let descriptor = shared_backing_file(mapped_bytes)?;
        let Some(base) = map_shared_aligned(descriptor, mapped_bytes, align.max(self.host_page))
        else {
            // SAFETY: `descriptor` is the file this call just made.
            unsafe { libc::close(descriptor) };
            return Err(AddressSpaceError::OutOfFrames);
        };
        if !placement.accepts(base as u64, bytes as u64) {
            // SAFETY: the mapping and the file are this call's own.
            unsafe {
                libc::munmap(base as *mut _, mapped_bytes);
                libc::close(descriptor);
            }
            return Err(AddressSpaceError::OutOfFrames);
        }
        let owner = current_user_memory_owner(self.processor);
        self.runs.lock().unwrap().push(HostedRun {
            base,
            bytes,
            mapped_bytes,
            descriptor,
            owner,
        });
        Ok(PhysicalRange::new(base as u64, bytes as u64))
    }

    /// The host placed the pages, so the alignment is not owed back to
    /// it.
    fn free_run(&self, run: PhysicalRange, _align: u64) -> Result<(), AddressSpaceError> {
        let mut runs = self.runs.lock().unwrap();
        let index = runs
            .iter()
            .position(|held| held.base as u64 == run.start && held.bytes as u64 == run.bytes)
            .ok_or(AddressSpaceError::NotCommitted)?;
        let held = runs.swap_remove(index);
        drop(runs);
        // SAFETY: the mapping and the file are the run's own, and the
        // run is out of the list, so nothing frees them twice.
        unsafe {
            libc::munmap(held.base as *mut _, held.mapped_bytes);
            libc::close(held.descriptor);
        }
        Ok(())
    }

    fn substitute_memory(
        &self,
        virt: VirtRange,
        physical: PhysicalRange,
        flags: PageFlags,
    ) -> Result<(), AddressSpaceError> {
        if physical.bytes as usize != virt.byte_len {
            return Err(AddressSpaceError::Misaligned);
        }
        let (descriptor, offset) = {
            let runs = self.runs.lock().unwrap();
            let held = runs
                .iter()
                .find(|held| {
                    held.base as u64 <= physical.start
                        && physical.start + physical.bytes <= (held.base + held.mapped_bytes) as u64
                })
                .ok_or(AddressSpaceError::Misaligned)?;
            (
                held.descriptor,
                (physical.start - held.base as u64) as libc::off_t,
            )
        };
        self.substitute_shared(
            virt,
            descriptor,
            offset,
            flags,
            AddressSpaceError::OutOfFrames,
        )
    }

    /// The device's pages are file-backed and shared, which is what lets
    /// them appear at a second address. A region no hosted device
    /// published cannot be shown: there is no host memory behind the
    /// address, and showing anonymous pages there would hand the owner a
    /// device that silently is not one.
    ///
    /// There is no other processor to shoot down: the hosted kernel runs
    /// on one, and the host's own `mmap` has published the change to
    /// every thread by the time it returns.
    fn substitute_device(
        &self,
        virt: VirtRange,
        region: DeviceRegion,
    ) -> Result<(), AddressSpaceError> {
        let backing = crate::device::backing_for(&region)
            .ok_or(AddressSpaceError::DeviceMappingUnsupported)?;
        self.substitute_shared(
            virt,
            backing.descriptor,
            backing.offset,
            device_region_flags(region),
            AddressSpaceError::DeviceMappingUnsupported,
        )
    }

    /// Fresh anonymous pages replace the shown ones in one `MAP_FIXED`
    /// mapping, so the range is never unmapped, and they read as zero.
    /// The reservation lock is held from the check to the bookkeeping.
    fn restore(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        self.require_host_aligned(virt)?;
        let mut reservations = self.reservations.lock().unwrap();
        let regions = reservations.substituted_regions(virt)?;
        let union = regions
            .iter()
            .fold(PageFlags::empty(), |acc, region| acc | region.flags);
        // SAFETY: as in `substitute_shared`.
        let fresh = unsafe {
            libc::mmap(
                virt.start.raw() as *mut _,
                virt.byte_len,
                page_flags_to_prot(union),
                libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if fresh == libc::MAP_FAILED {
            return Err(AddressSpaceError::OutOfFrames);
        }
        reservations.record_restore(virt)?;
        // The mapping carries the union of the flags; each host page is
        // brought to what the bookkeeping says.
        self.apply_host_protection(&reservations, virt, false)
    }

    fn withdraw(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        self.require_host_aligned(virt)?;
        let mut reservations = self.reservations.lock().unwrap();
        reservations.substituted_regions(virt)?;
        self.drop_substituted(virt)?;
        reservations.record_withdraw(virt)
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

    fn swap_out_page(
        &self,
        addr: VirtAddr,
        token: SwapToken,
        out: &mut [u8],
    ) -> Result<PageFlags, AddressSpaceError> {
        if out.len() != PAGE_SIZE {
            return Err(AddressSpaceError::BadPageBuffer);
        }
        if !addr.is_page_aligned() {
            return Err(AddressSpaceError::Misaligned);
        }
        let page = VirtRange::new(addr, PAGE_SIZE);
        let mut reservations = self.reservations.lock().unwrap();
        self.open_for_transfer(&reservations, page)?;
        // SAFETY: the range is committed (`record_swap_out` below
        // rejects it otherwise) and the host pages covering it were just
        // made readable.
        unsafe {
            ptr::copy_nonoverlapping(addr.raw() as *const u8, out.as_mut_ptr(), PAGE_SIZE);
        }
        let flags = match reservations.record_swap_out(addr, token) {
            Ok(flags) => flags,
            Err(error) => {
                self.apply_host_protection(&reservations, page, false)?;
                return Err(error);
            }
        };
        // The page is no longer committed, so this drops it to PROT_NONE
        // and hands the host its memory back.
        self.apply_host_protection(&reservations, page, true)?;
        Ok(flags)
    }

    fn swap_in_page(&self, addr: VirtAddr, bytes: &[u8]) -> Result<SwapToken, AddressSpaceError> {
        if bytes.len() != PAGE_SIZE {
            return Err(AddressSpaceError::BadPageBuffer);
        }
        if !addr.is_page_aligned() {
            return Err(AddressSpaceError::Misaligned);
        }
        let page = VirtRange::new(addr, PAGE_SIZE);
        let mut reservations = self.reservations.lock().unwrap();
        let entry = reservations.take_swap_entry(addr)?;
        self.open_for_transfer(&reservations, page)?;
        // SAFETY: the host pages covering `page` were just made
        // writable, and only this 4 KiB of them is written.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), addr.raw() as *mut u8, PAGE_SIZE);
        }
        self.apply_host_protection(&reservations, page, false)?;
        Ok(entry.token)
    }

    fn swapped_token(&self, addr: VirtAddr) -> Option<SwapToken> {
        self.reservations
            .lock()
            .unwrap()
            .swap_entry(addr.page_floor())
            .map(|entry| entry.token)
    }

    /// The host kernel does not show a process its pages' access flags,
    /// so every committed page reports [`PageAge::Hot`]. Aging therefore
    /// frees nothing here, which is the honest answer: `hosted` cannot
    /// rank pages it cannot measure, and a pass that frees nothing is
    /// what hands the decision to the OOM killer.
    fn scan_committed_pages<Visit>(&self, owner: u64, mut visit: Visit) -> usize
    where
        Visit: FnMut(VirtAddr, PageFlags, PageAge) -> bool,
    {
        let reservations = self.reservations.lock().unwrap();
        let mut visited = 0_usize;
        let mut keep_going = true;
        reservations.owned_committed_regions(MemoryOwner::new(owner), |region| {
            for offset in (0..region.range.byte_len).step_by(PAGE_SIZE) {
                visited += 1;
                if !visit(
                    VirtAddr::new(region.range.start.raw() + offset),
                    region.flags,
                    PageAge::Hot,
                ) {
                    keep_going = false;
                    return false;
                }
            }
            true
        });
        let _ = keep_going;
        visited
    }

    fn owned_resident_bytes(&self, owner: u64) -> u64 {
        let owner = MemoryOwner::new(owner);
        let committed = self
            .reservations
            .lock()
            .unwrap()
            .owned_resident_bytes(owner);
        let pinned: u64 = self
            .runs
            .lock()
            .unwrap()
            .iter()
            .filter(|run| run.owner == owner)
            .map(|run| run.bytes as u64)
            .sum();
        committed + pinned
    }

    fn drain_orphaned_swap_tokens<Visit>(&self, mut visit: Visit) -> usize
    where
        Visit: FnMut(SwapToken),
    {
        let drained: Vec<SwapToken> = core::mem::take(&mut *self.orphans.lock().unwrap());
        let count = drained.len();
        for token in drained {
            visit(token);
        }
        count
    }
}

/// Restores the pre-protect committed regions after a failed protect.
fn restore_committed(
    tracker: &mut ReservationTracker,
    virt: VirtRange,
    previous: &[CommittedRegion],
) -> Result<(), AddressSpaceError> {
    tracker.replace_committed(virt, previous)
}

/// An unlinked temporary file of `bytes`, whose pages can appear at
/// more than one address: a mapping of it made anywhere is the same
/// memory as a mapping made anywhere else.
pub(crate) fn shared_backing_file(bytes: usize) -> Result<i32, AddressSpaceError> {
    let template =
        CString::new("/tmp/helios-hosted-XXXXXX").expect("the template holds no interior nul");
    let mut path = template.into_bytes_with_nul();
    // SAFETY: `path` is a nul-terminated, writable buffer holding a
    // template `mkstemp` accepts.
    let descriptor = unsafe { libc::mkstemp(path.as_mut_ptr().cast()) };
    if descriptor < 0 {
        return Err(AddressSpaceError::OutOfFrames);
    }
    // SAFETY: `path` is the nul-terminated name `mkstemp` filled in,
    // and `descriptor` is a fresh, empty regular file.
    let made = unsafe {
        libc::unlink(path.as_ptr().cast()) == 0
            && libc::ftruncate(descriptor, bytes as libc::off_t) == 0
    };
    if !made {
        // SAFETY: `descriptor` is the file this call just made.
        unsafe { libc::close(descriptor) };
        return Err(AddressSpaceError::OutOfFrames);
    }
    Ok(descriptor)
}

/// Map `bytes` of `descriptor` shared and writable at an address that
/// is a multiple of `align`, a power of two no smaller than the host
/// page, and return it.
fn map_shared_aligned(descriptor: i32, bytes: usize, align: usize) -> Option<usize> {
    let reserved = bytes + align;
    // SAFETY: an anonymous `PROT_NONE` mapping of address space nothing
    // else uses; the host picks the address.
    let region = unsafe {
        libc::mmap(
            ptr::null_mut(),
            reserved,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if region == libc::MAP_FAILED {
        return None;
    }
    let region = region as usize;
    let start = align_up(region, align);
    // SAFETY: `[start, start + bytes)` is inside the reservation above,
    // which this call owns, and `descriptor` names a file of `bytes`.
    let mapped = unsafe {
        libc::mmap(
            start as *mut _,
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_FIXED | libc::MAP_SHARED,
            descriptor,
            0,
        )
    };
    // SAFETY: both ends of the reservation are address space this call
    // owns and nothing is mapped in.
    unsafe {
        if mapped == libc::MAP_FAILED {
            libc::munmap(region as *mut _, reserved);
            return None;
        }
        if start > region {
            libc::munmap(region as *mut _, start - region);
        }
        let end = start + bytes;
        if region + reserved > end {
            libc::munmap(end as *mut _, region + reserved - end);
        }
    }
    Some(start)
}

fn host_page_size() -> usize {
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(value > 0, "sysconf(_SC_PAGESIZE) failed");
    value as usize
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

    const RW: PageFlags = PageFlags::READ.union(PageFlags::WRITE);

    /// Where the pages of a run are visible to the kernel: on this
    /// backend a run's "physical" address is where it is mapped.
    fn alias(run: PhysicalRange) -> *mut u8 {
        run.start as usize as *mut u8
    }

    fn placement() -> DmaPlacement {
        DmaPlacement {
            align: PAGE_SIZE as u64,
            limit: u64::MAX,
        }
    }

    fn fill(pointer: *mut u8, byte: u8, len: usize) {
        // SAFETY: every caller passes a mapped, writable range of `len`
        // bytes.
        unsafe { ptr::write_bytes(pointer, byte, len) };
    }

    fn all(pointer: *const u8, byte: u8, len: usize) -> bool {
        // SAFETY: every caller passes a mapped, readable range of `len`
        // bytes.
        unsafe { core::slice::from_raw_parts(pointer, len) }
            .iter()
            .all(|value| *value == byte)
    }

    /// A reservation of `host_pages` host pages, all committed
    /// read-write, and the number of bytes a host page is.
    fn committed(address_space: &HostedAddressSpace, host_pages: usize) -> (VirtRange, usize) {
        let unit = address_space.host_page;
        let range = address_space.reserve(host_pages * unit).unwrap();
        address_space.commit(range, RW).unwrap();
        (range, unit)
    }

    fn span(range: VirtRange, unit: usize, first: usize, pages: usize) -> VirtRange {
        VirtRange::new(
            VirtAddr::new(range.start.raw() + first * unit),
            pages * unit,
        )
    }

    #[test]
    fn a_substituted_range_and_its_run_are_the_same_bytes() {
        let address_space = HostedAddressSpace::new();
        let (range, unit) = committed(&address_space, 4);
        let view = span(range, unit, 1, 2);
        fill(view.start.raw() as *mut u8, 0xaa, view.byte_len);
        let run = address_space
            .allocate_run(view.byte_len, placement())
            .unwrap();
        assert!(all(alias(run), 0, view.byte_len), "a run is taken zeroed");

        address_space.substitute_memory(view, run, RW).unwrap();

        assert!(
            all(view.start.raw() as *const u8, 0, view.byte_len),
            "the owner sees the run, not the pages it wrote before"
        );
        fill(view.start.raw() as *mut u8, 0x5c, view.byte_len);
        assert!(all(alias(run), 0x5c, view.byte_len), "owner to kernel");
        fill(alias(run), 0x3d, view.byte_len);
        assert!(
            all(view.start.raw() as *const u8, 0x3d, view.byte_len),
            "kernel to owner"
        );
        assert!(
            all(range.start.raw() as *const u8, 0, unit),
            "pages outside the view are untouched"
        );
        match address_space.translate(view.start) {
            Translation::Committed { flags, .. } => assert_eq!(flags, RW),
            other => panic!("a substituted range stays committed, got {other:?}"),
        }
        address_space.withdraw(view).unwrap();
        address_space.free_run(run, PAGE_SIZE as u64).unwrap();
        address_space.release(range).unwrap();
    }

    #[test]
    fn restore_puts_zeroes_back_and_leaves_the_run_alone() {
        let address_space = HostedAddressSpace::new();
        let (range, unit) = committed(&address_space, 2);
        let view = span(range, unit, 0, 2);
        let run = address_space
            .allocate_run(view.byte_len, placement())
            .unwrap();
        address_space.substitute_memory(view, run, RW).unwrap();
        fill(alias(run), 0x77, view.byte_len);

        address_space.restore(view).unwrap();

        assert!(
            all(view.start.raw() as *const u8, 0, view.byte_len),
            "zeroes"
        );
        assert!(
            all(alias(run), 0x77, view.byte_len),
            "the run keeps what it held"
        );
        fill(view.start.raw() as *mut u8, 0x11, view.byte_len);
        assert!(
            all(alias(run), 0x77, view.byte_len),
            "the restored pages are not the run's"
        );
        assert_eq!(
            address_space.restore(view),
            Err(AddressSpaceError::NotSubstituted)
        );
        // A restored range can be substituted again.
        address_space.substitute_memory(view, run, RW).unwrap();
        assert!(all(view.start.raw() as *const u8, 0x77, view.byte_len));
        address_space.restore(view).unwrap();
        address_space.free_run(run, PAGE_SIZE as u64).unwrap();
        address_space.release(range).unwrap();
    }

    #[test]
    fn a_swapped_out_page_is_substituted_with_the_range_and_restored_as_it_was() {
        let address_space = HostedAddressSpace::new();
        let (range, unit) = committed(&address_space, 2);
        let view = range;
        // The first host page is swapped out, every hal page of it.
        let swapped_pages = unit / PAGE_SIZE;
        let tokens: Vec<SwapToken> = (1..=swapped_pages as u32)
            .map(|raw| SwapToken::new(raw).unwrap())
            .collect();
        fill(view.start.raw() as *mut u8, 0xc3, view.byte_len);
        let mut page = vec![0_u8; PAGE_SIZE];
        for (index, token) in tokens.iter().enumerate() {
            let addr = VirtAddr::new(range.start.raw() + index * PAGE_SIZE);
            address_space
                .swap_out_page(addr, *token, &mut page)
                .unwrap();
            assert_eq!(address_space.swapped_token(addr), Some(*token));
        }
        let run = address_space
            .allocate_run(view.byte_len, placement())
            .unwrap();
        fill(alias(run), 0x5a, view.byte_len);

        address_space.substitute_memory(view, run, RW).unwrap();

        assert!(
            all(view.start.raw() as *const u8, 0x5a, view.byte_len),
            "the swapped part shows the run like the rest"
        );
        for index in 0..swapped_pages {
            let addr = VirtAddr::new(range.start.raw() + index * PAGE_SIZE);
            assert_eq!(address_space.swapped_token(addr), None);
            assert_eq!(
                address_space.swap_in_page(addr, &page),
                Err(AddressSpaceError::NotSwapped),
                "a substituted page is no longer swapped"
            );
        }
        let mut orphaned = Vec::new();
        let drained = address_space.drain_orphaned_swap_tokens(|token| orphaned.push(token));
        orphaned.sort();
        assert_eq!(drained, swapped_pages);
        assert_eq!(orphaned, tokens, "every swap extent is given back");

        address_space.restore(view).unwrap();

        assert!(
            all(view.start.raw() as *const u8, 0, view.byte_len),
            "zeroes, swapped part included"
        );
        for offset in [0, unit] {
            match address_space.translate(VirtAddr::new(range.start.raw() + offset)) {
                Translation::Committed { flags, .. } => assert_eq!(flags, RW),
                other => panic!("a restored page is committed as it was, got {other:?}"),
            }
        }
        fill(view.start.raw() as *mut u8, 0x11, view.byte_len);
        address_space.free_run(run, PAGE_SIZE as u64).unwrap();
        address_space.release(range).unwrap();
    }

    #[test]
    fn withdraw_leaves_the_range_uncommitted_and_it_commits_again() {
        let address_space = HostedAddressSpace::new();
        let (range, unit) = committed(&address_space, 2);
        let view = span(range, unit, 1, 1);
        let run = address_space
            .allocate_run(view.byte_len, placement())
            .unwrap();
        address_space.substitute_memory(view, run, RW).unwrap();

        address_space.withdraw(view).unwrap();

        assert_eq!(address_space.translate(view.start), Translation::Reserved);
        assert_eq!(
            address_space.withdraw(view),
            Err(AddressSpaceError::NotSubstituted)
        );
        address_space.commit(view, RW).unwrap();
        assert!(all(view.start.raw() as *const u8, 0, view.byte_len));
        address_space.decommit(range).unwrap();
        address_space.commit(range, RW).unwrap();
        fill(range.start.raw() as *mut u8, 0x42, range.byte_len);
        address_space.free_run(run, PAGE_SIZE as u64).unwrap();
        address_space.release(range).unwrap();
    }

    #[test]
    fn substituting_an_uncommitted_or_substituted_range_changes_nothing() {
        let address_space = HostedAddressSpace::new();
        let unit = address_space.host_page;
        let range = address_space.reserve(4 * unit).unwrap();
        let committed_part = span(range, unit, 0, 2);
        address_space.commit(committed_part, RW).unwrap();
        fill(
            committed_part.start.raw() as *mut u8,
            0x66,
            committed_part.byte_len,
        );
        let run = address_space.allocate_run(2 * unit, placement()).unwrap();
        fill(alias(run), 0x21, 2 * unit);

        // Reaches past the committed part.
        let reaching = span(range, unit, 1, 2);
        assert_eq!(
            address_space.substitute_memory(reaching, run, RW),
            Err(AddressSpaceError::NotCommitted)
        );
        // Not committed at all.
        let beyond = span(range, unit, 2, 2);
        assert_eq!(
            address_space.substitute_memory(beyond, run, RW),
            Err(AddressSpaceError::NotCommitted)
        );
        assert!(all(
            committed_part.start.raw() as *const u8,
            0x66,
            committed_part.byte_len
        ));

        let first = span(range, unit, 0, 1);
        let first_run = PhysicalRange::new(run.start, unit as u64);
        address_space
            .substitute_memory(first, first_run, RW)
            .unwrap();
        // Overlaps a live substitution, whole or in part.
        assert_eq!(
            address_space.substitute_memory(first, first_run, RW),
            Err(AddressSpaceError::Substituted)
        );
        assert_eq!(
            address_space.substitute_memory(committed_part, run, RW),
            Err(AddressSpaceError::Substituted)
        );
        // Nothing changed: the first page still shows the run, the second
        // still holds what the owner wrote.
        assert!(all(first.start.raw() as *const u8, 0x21, unit));
        assert!(all(
            (committed_part.start.raw() + unit) as *const u8,
            0x66,
            unit
        ));
        // A range that is not substituted cannot be ended.
        assert_eq!(
            address_space.restore(committed_part),
            Err(AddressSpaceError::NotSubstituted)
        );
        assert_eq!(
            address_space.withdraw(span(range, unit, 1, 1)),
            Err(AddressSpaceError::NotSubstituted)
        );
        address_space.withdraw(first).unwrap();
        address_space.free_run(run, PAGE_SIZE as u64).unwrap();
        address_space.release(range).unwrap();
    }

    #[test]
    fn a_slot_reset_over_a_live_substitution_does_not_free_the_run() {
        let address_space = HostedAddressSpace::new();
        let (range, unit) = committed(&address_space, 4);
        let view = span(range, unit, 1, 2);
        let run = address_space
            .allocate_run(view.byte_len, placement())
            .unwrap();
        address_space.substitute_memory(view, run, RW).unwrap();
        fill(alias(run), 0x9e, view.byte_len);
        fill(range.start.raw() as *mut u8, 0x01, unit);

        // The runtime's reset of a pooled slot: the whole committed
        // extent goes, with the substitution still live inside it.
        address_space.decommit(range).unwrap();

        assert_eq!(address_space.translate(view.start), Translation::Reserved);
        assert!(
            all(alias(run), 0x9e, view.byte_len),
            "the run belongs to its owner, not to the slot"
        );
        // The slot is reusable at once, and the owner of the run can
        // still free it, once.
        address_space.commit(range, RW).unwrap();
        assert!(
            all(view.start.raw() as *const u8, 0, view.byte_len),
            "the span shows fresh memory, not the run"
        );
        address_space.free_run(run, PAGE_SIZE as u64).unwrap();
        assert_eq!(
            address_space.free_run(run, PAGE_SIZE as u64),
            Err(AddressSpaceError::NotCommitted)
        );
        // The pool is consistent: a run of the same size is available
        // again and is zero.
        let again = address_space
            .allocate_run(view.byte_len, placement())
            .unwrap();
        assert!(all(alias(again), 0, view.byte_len));
        address_space.free_run(again, PAGE_SIZE as u64).unwrap();
        address_space.release(range).unwrap();
    }

    #[test]
    fn release_drops_a_live_substitution_without_freeing_the_run() {
        let address_space = HostedAddressSpace::new();
        let (range, unit) = committed(&address_space, 2);
        let run = address_space.allocate_run(unit, placement()).unwrap();
        address_space
            .substitute_memory(span(range, unit, 0, 1), run, RW)
            .unwrap();
        fill(alias(run), 0x2b, unit);

        address_space.release(range).unwrap();

        assert!(all(alias(run), 0x2b, unit));
        address_space.free_run(run, PAGE_SIZE as u64).unwrap();
    }

    #[test]
    fn protect_applies_to_a_substituted_range_like_to_any_committed_one() {
        let address_space = HostedAddressSpace::new();
        let (range, unit) = committed(&address_space, 2);
        let view = span(range, unit, 0, 2);
        let run = address_space
            .allocate_run(view.byte_len, placement())
            .unwrap();
        address_space.substitute_memory(view, run, RW).unwrap();
        fill(alias(run), 0x55, view.byte_len);

        address_space.protect(range, PageFlags::READ).unwrap();

        match address_space.translate(view.start) {
            Translation::Committed { flags, .. } => assert_eq!(flags, PageFlags::READ),
            other => panic!("expected a committed, read-only range, got {other:?}"),
        }
        assert!(all(view.start.raw() as *const u8, 0x55, view.byte_len));
        // Still substituted: it cannot be shown a second thing, and it
        // can still be ended.
        assert_eq!(
            address_space.substitute_memory(view, run, RW),
            Err(AddressSpaceError::Substituted)
        );
        address_space.restore(view).unwrap();
        address_space.free_run(run, PAGE_SIZE as u64).unwrap();
        address_space.release(range).unwrap();
    }

    #[test]
    fn a_run_starts_on_the_alignment_it_was_asked_for() {
        let address_space = HostedAddressSpace::new();
        let align = 16 * address_space.host_page;
        let run = address_space
            .allocate_run(
                address_space.host_page,
                DmaPlacement {
                    align: align as u64,
                    limit: u64::MAX,
                },
            )
            .unwrap();
        assert_eq!(run.start as usize % align, 0);
        address_space.free_run(run, align as u64).unwrap();
        assert_eq!(
            address_space.allocate_run(
                address_space.host_page,
                DmaPlacement {
                    align: 1 << 20,
                    limit: 0
                }
            ),
            Err(AddressSpaceError::OutOfFrames),
            "a placement the host cannot satisfy is refused and given back"
        );
    }

    #[test]
    fn a_run_is_not_resident_memory_of_the_slot_it_is_shown_in() {
        use helios_kernel::{configure_user_memory_owner_processors, enter_user_memory_owner};
        configure_user_memory_owner_processors(crate::test_pool::single_processor_pool());
        let address_space = HostedAddressSpace::new();
        let owner = MemoryOwner::new(9);
        let _scope = enter_user_memory_owner(address_space.processor, owner);
        let (range, unit) = committed(&address_space, 2);
        assert_eq!(address_space.owned_resident_bytes(9), 2 * unit as u64);
        let run = address_space.allocate_run(unit, placement()).unwrap();
        assert_eq!(
            address_space.owned_resident_bytes(9),
            3 * unit as u64,
            "the run is charged to whoever asked for it"
        );
        address_space
            .substitute_memory(span(range, unit, 0, 1), run, RW)
            .unwrap();
        assert_eq!(
            address_space.owned_resident_bytes(9),
            2 * unit as u64,
            "the page the run replaced left the slot's account; the run's bytes count once"
        );
        address_space.restore(span(range, unit, 0, 1)).unwrap();
        address_space.free_run(run, PAGE_SIZE as u64).unwrap();
        address_space.release(range).unwrap();
    }
}
