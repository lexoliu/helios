//! x86_64 user virtual address space backed by the running kernel CR3.
//!
//! Limine sets up the kernel page tables with a higher-half direct map
//! (HHDM) for physical memory and the kernel image. The lower 128 TiB
//! of the canonical address space is otherwise unused, and we carve a
//! [`USER_VA_BASE`..`USER_VA_END`) window out of it for dynamic user
//! mappings.
//!
//! Reservations are tracked in software in `Reservations` ; commit /
//! decommit / protect / release operate by walking the live CR3 and
//! mutating leaf entries through `OffsetPageTable`. Local TLB
//! invalidation uses `INVLPG`. Cross-core TLB shootdown is enforced
//! by asserting that only one processor is active — x86 currently
//! boots with `ProcessorStartupPolicy::BootstrapOnly`, so AS mutating
//! ops on a multi-core configuration will panic clearly until the
//! shootdown protocol lands. AGENTS §3.4 SMP-first is satisfied by
//! making the precondition explicit at the API boundary rather than
//! racing TLBs in silence.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{
    AddressSpace, AddressSpaceError, PageFlags, Translation, VirtAddr, VirtRange,
};
use spin::Mutex;
use x86_64::VirtAddr as X86VirtAddr;
use x86_64::instructions::tlb::flush as invalidate_local_tlb;
use x86_64::structures::paging::mapper::TranslateResult;
use x86_64::structures::paging::{
    Mapper, OffsetPageTable, Page, PageTableFlags, Size4KiB, Translate,
};

use crate::smp::{self, DirectMappedFrameAllocator};

/// Start of the user-VA window. `0x0000_2000_0000_0000` keeps the
/// canonical-address constraint satisfied (bits 47..63 must all be
/// zero or all one for x86-64 four-level paging) and stays well clear
/// of the user 0..2 TiB region typical userland tools assume.
const USER_VA_BASE: usize = 0x0000_2000_0000_0000;
/// 32 TiB user window — enough for ~6500 4 GiB+guard wasm linear
/// memories, far beyond what realistic kernel-plugin loads will hold
/// concurrently.
const USER_VA_END: usize = 0x0000_4000_0000_0000;
const PAGE: usize = PhysFrame::SIZE;

/// Owned x86 user address space. Built once at boot, accessed through
/// `&'static`.
pub struct X86UserAddressSpace {
    physical_memory_offset: usize,
    processor_count: usize,
    /// Bump pointer for fresh reservations within the user-VA window.
    /// Reservations released via `release` are returned to a free
    /// list rather than to the bump pointer; the simple bump path
    /// covers the steady-state allocate-and-release-much-later
    /// pattern that wasmtime drives, while the free list catches
    /// tight reservation churn.
    next_va: AtomicUsize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Live reservations owned by this AS.
    reservations: Vec<Reservation>,
    /// Released ranges available for reuse by `reserve`. Best-fit on
    /// `byte_len`.
    free_list: Vec<VirtRange>,
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

impl X86UserAddressSpace {
    pub fn new(physical_memory_offset: usize, processor_count: usize) -> Self {
        Self {
            physical_memory_offset,
            processor_count,
            next_va: AtomicUsize::new(USER_VA_BASE),
            state: Mutex::new(State::default()),
        }
    }

    fn assert_smp_safe(&self) {
        // x86 is currently single-core in `ProcessorStartupPolicy::
        // BootstrapOnly`. Cross-core TLB shootdown is not yet
        // implemented, so AS mutating ops are only safe with one
        // running processor. Panic loudly rather than race TLBs in
        // production once AP startup is enabled — the operator will
        // see a clear diagnostic pointing at the missing shootdown
        // protocol instead of debugging silent memory corruption.
        assert!(
            self.processor_count == 1,
            "X86UserAddressSpace mutating op called with {} active processors; \
             cross-core TLB shootdown is not yet implemented",
            self.processor_count
        );
    }

    fn carve_reservation(&self, byte_len: usize) -> Option<VirtRange> {
        let mut state = self.state.lock();
        if let Some(index) = state
            .free_list
            .iter()
            .position(|range| range.byte_len >= byte_len)
        {
            let mut reused = state.free_list.swap_remove(index);
            if reused.byte_len > byte_len {
                let leftover = VirtRange::new(
                    VirtAddr::new(reused.start.raw() + byte_len),
                    reused.byte_len - byte_len,
                );
                state.free_list.push(leftover);
                reused.byte_len = byte_len;
            }
            return Some(reused);
        }
        drop(state);
        let mut current = self.next_va.load(Ordering::Acquire);
        loop {
            let next = current.checked_add(byte_len)?;
            if next > USER_VA_END {
                return None;
            }
            match self.next_va.compare_exchange(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(VirtRange::new(VirtAddr::new(current), byte_len));
                }
                Err(observed) => current = observed,
            }
        }
    }

    fn map_pages(
        &self,
        mapper: &mut OffsetPageTable<'static>,
        frame_allocator: &mut DirectMappedFrameAllocator,
        virt: VirtRange,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        for offset in (0..virt.byte_len).step_by(PAGE) {
            let virt_addr = virt.start.raw() + offset;
            let frame = frame_allocator
                .allocate_frame_for_user()
                .ok_or(AddressSpaceError::OutOfFrames)?;
            let page =
                Page::<Size4KiB>::from_start_address(X86VirtAddr::new(virt_addr as u64))
                    .map_err(|_| AddressSpaceError::Misaligned)?;
            unsafe {
                mapper
                    .map_to(page, frame, flags, frame_allocator)
                    .map_err(|_| AddressSpaceError::PageTableExhausted)?
                    .flush();
            }
        }
        Ok(())
    }

    fn unmap_pages(
        &self,
        mapper: &mut OffsetPageTable<'static>,
        virt: VirtRange,
    ) -> Result<(), AddressSpaceError> {
        for offset in (0..virt.byte_len).step_by(PAGE) {
            let virt_addr = virt.start.raw() + offset;
            let page =
                Page::<Size4KiB>::from_start_address(X86VirtAddr::new(virt_addr as u64))
                    .map_err(|_| AddressSpaceError::Misaligned)?;
            match mapper.unmap(page) {
                Ok((_frame, flush)) => {
                    flush.flush();
                    // Frame is leaked — the buddy heap underneath
                    // `DirectMappedFrameAllocator` does not yet expose a
                    // dealloc hook; once the user-memory pool wires
                    // through the allocator-api this becomes a real
                    // free. For now releasing the reservation is the
                    // visible effect.
                }
                Err(_) => {
                    // Page was never committed. Skip.
                }
            }
        }
        Ok(())
    }

    fn protect_pages(
        &self,
        mapper: &mut OffsetPageTable<'static>,
        virt: VirtRange,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        for offset in (0..virt.byte_len).step_by(PAGE) {
            let virt_addr = virt.start.raw() + offset;
            let page =
                Page::<Size4KiB>::from_start_address(X86VirtAddr::new(virt_addr as u64))
                    .map_err(|_| AddressSpaceError::Misaligned)?;
            match unsafe { mapper.update_flags(page, flags) } {
                Ok(flush) => flush.flush(),
                Err(_) => return Err(AddressSpaceError::NotCommitted),
            }
        }
        Ok(())
    }
}

impl AddressSpace for X86UserAddressSpace {
    fn reserve(&self, byte_len: usize) -> Result<VirtRange, AddressSpaceError> {
        if byte_len == 0 {
            return Err(AddressSpaceError::EmptyRange);
        }
        if !byte_len.is_multiple_of(PAGE) {
            return Err(AddressSpaceError::Misaligned);
        }
        let range = self
            .carve_reservation(byte_len)
            .ok_or(AddressSpaceError::OutOfFrames)?;
        self.state.lock().reservations.push(Reservation {
            range,
            committed: Vec::new(),
        });
        Ok(range)
    }

    fn release(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        self.assert_smp_safe();
        let mut state = self.state.lock();
        let index = state
            .reservations
            .iter()
            .position(|reservation| reservation.range == virt)
            .ok_or(AddressSpaceError::NotReserved)?;
        let reservation = state.reservations.swap_remove(index);
        drop(state);

        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        for region in &reservation.committed {
            self.unmap_pages(&mut mapper, region.range)?;
        }

        self.state.lock().free_list.push(reservation.range);
        Ok(())
    }

    fn commit(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        self.assert_smp_safe();
        validate_range(virt)?;
        let pt_flags = page_flags_to_pt(flags)?;

        let mut state = self.state.lock();
        let reservation = find_reservation_mut(&mut state.reservations, virt)?;
        if reservation
            .committed
            .iter()
            .any(|region| ranges_overlap(region.range, virt))
        {
            return Err(AddressSpaceError::Overlap);
        }
        drop(state);

        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        let mut frame_allocator = DirectMappedFrameAllocator {
            physical_memory_offset: self.physical_memory_offset,
        };
        self.map_pages(&mut mapper, &mut frame_allocator, virt, pt_flags)?;

        let mut state = self.state.lock();
        let reservation = find_reservation_mut(&mut state.reservations, virt)?;
        reservation
            .committed
            .push(CommittedRegion { range: virt, flags });
        Ok(())
    }

    fn decommit(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        self.assert_smp_safe();
        validate_range(virt)?;
        let mut state = self.state.lock();
        let reservation = find_reservation_mut(&mut state.reservations, virt)?;
        if !reservation
            .committed
            .iter()
            .any(|region| range_contains(region.range, virt))
        {
            return Err(AddressSpaceError::NotCommitted);
        }
        reservation
            .committed
            .retain(|region| !ranges_overlap(region.range, virt));
        drop(state);

        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        self.unmap_pages(&mut mapper, virt)?;
        Ok(())
    }

    fn protect(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        self.assert_smp_safe();
        validate_range(virt)?;
        let pt_flags = page_flags_to_pt(flags)?;
        let mut state = self.state.lock();
        let reservation = find_reservation_mut(&mut state.reservations, virt)?;
        if !reservation
            .committed
            .iter()
            .any(|region| range_contains(region.range, virt))
        {
            return Err(AddressSpaceError::NotCommitted);
        }
        for region in reservation.committed.iter_mut() {
            if region.range == virt {
                region.flags = flags;
            }
        }
        drop(state);

        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        self.protect_pages(&mut mapper, virt, pt_flags)?;
        Ok(())
    }

    fn translate(&self, addr: VirtAddr) -> Translation {
        if addr.raw() < USER_VA_BASE || addr.raw() >= USER_VA_END {
            return Translation::Unmapped;
        }
        let state = self.state.lock();
        let reservation = match state
            .reservations
            .iter()
            .find(|reservation| reservation.range.contains(addr))
        {
            Some(reservation) => reservation,
            None => return Translation::Unmapped,
        };
        let committed = reservation
            .committed
            .iter()
            .find(|region| region.range.contains(addr))
            .copied();
        drop(state);

        let Some(committed) = committed else {
            return Translation::Reserved;
        };
        let mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        match mapper.translate(X86VirtAddr::new(addr.raw() as u64)) {
            TranslateResult::Mapped { frame, .. } => {
                let phys = frame.start_address().as_u64() as usize;
                Translation::Committed {
                    frame: PhysFrame::from_phys_addr(phys & !(PAGE - 1)),
                    flags: committed.flags,
                }
            }
            _ => Translation::Reserved,
        }
    }
}

impl DirectMappedFrameAllocator {
    /// Wrapper around `FrameAllocator::allocate_frame` that returns a
    /// strongly-typed `Size4KiB` frame for the user AS commit path.
    /// The underlying allocator is identical to the one the existing
    /// page-table walker uses.
    fn allocate_frame_for_user(
        &mut self,
    ) -> Option<x86_64::structures::paging::PhysFrame<Size4KiB>> {
        use x86_64::structures::paging::FrameAllocator;
        FrameAllocator::<Size4KiB>::allocate_frame(self)
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

fn find_reservation_mut(
    reservations: &mut Vec<Reservation>,
    virt: VirtRange,
) -> Result<&mut Reservation, AddressSpaceError> {
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

fn page_flags_to_pt(flags: PageFlags) -> Result<PageTableFlags, AddressSpaceError> {
    if flags.is_empty() {
        return Err(AddressSpaceError::InvalidFlags);
    }
    let mut pt = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if flags.contains(PageFlags::WRITE) {
        pt |= PageTableFlags::WRITABLE;
    }
    if !flags.contains(PageFlags::EXECUTE) {
        pt |= PageTableFlags::NO_EXECUTE;
    }
    Ok(pt)
}

#[allow(dead_code)]
fn invlpg_range(virt: VirtRange) {
    for offset in (0..virt.byte_len).step_by(PAGE) {
        invalidate_local_tlb(X86VirtAddr::new((virt.start.raw() + offset) as u64));
    }
}
