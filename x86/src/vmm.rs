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
//! invalidation uses `INVLPG`; cross-core TLB shootdown uses a
//! dedicated local-APIC IPI vector and waits for every online
//! processor to acknowledge the invalidation before old frames are
//! returned to the user-memory pool.

use alloc::vec::Vec;
use core::ffi::c_int;
use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{
    AddressSpace, AddressSpaceError, PageFlags, Translation, VirtAddr, VirtRange,
};
use helios_kernel::custom_vm::{
    self, CustomVmHooks, default_memory_image_free, default_memory_image_map_at,
    default_memory_image_new, default_page_size,
};
use helios_kernel::{allocate_user_frame_zeroed, deallocate_user_frame};
use spin::{Mutex, Once};
use x86_64::PhysAddr;
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

#[derive(Clone, Copy)]
struct RelocationPage {
    page: Page<Size4KiB>,
    virt: usize,
    old_phys: usize,
    old_flags: PageTableFlags,
    new_phys: usize,
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
        assert!(
            self.processor_count <= usize::BITS as usize,
            "X86UserAddressSpace supports at most {} processors in its TLB shootdown ack mask; got {}",
            usize::BITS,
            self.processor_count
        );
    }

    fn shootdown_range(&self, virt: VirtRange) {
        smp::shootdown_tlb_range(virt.start.raw(), virt.byte_len);
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
            let phys = self.alloc_user_frame()?;
            let frame = x86_64::structures::paging::PhysFrame::from_start_address(PhysAddr::new(
                phys as u64,
            ))
            .map_err(|_| AddressSpaceError::Misaligned)?;
            let page =
                Page::<Size4KiB>::from_start_address(X86VirtAddr::new(virt_addr as u64))
                    .map_err(|_| AddressSpaceError::Misaligned)?;
            unsafe {
                mapper
                    .map_to(page, frame, flags, frame_allocator)
                    .map_err(|_| {
                        self.dealloc_user_phys(phys);
                        AddressSpaceError::PageTableExhausted
                    })?
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
                Ok((frame, flush)) => {
                    flush.flush();
                    self.dealloc_user_phys(frame.start_address().as_u64() as usize);
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

    fn alloc_user_frame(&self) -> Result<usize, AddressSpaceError> {
        let raw = allocate_user_frame_zeroed().map_err(|_| AddressSpaceError::OutOfFrames)?;
        Ok(raw.as_ptr() as usize - self.physical_memory_offset)
    }

    fn hhdm_ptr(&self, phys: usize) -> *mut u8 {
        let virt = phys
            .checked_add(self.physical_memory_offset)
            .unwrap_or_else(|| panic!("x86 user-frame HHDM address overflow"));
        virt as *mut u8
    }

    fn dealloc_user_phys(&self, phys: usize) {
        let ptr = NonNull::new(self.hhdm_ptr(phys))
            .unwrap_or_else(|| panic!("x86 user-frame dealloc received null HHDM pointer"));
        deallocate_user_frame(ptr);
    }

    fn build_relocation_plan(
        &self,
        mapper: &OffsetPageTable<'static>,
        virt: VirtRange,
    ) -> Result<Vec<RelocationPage>, AddressSpaceError> {
        let mut pages: Vec<RelocationPage> = Vec::new();
        for offset in (0..virt.byte_len).step_by(PAGE) {
            let virt_addr = virt.start.raw() + offset;
            let page = Page::<Size4KiB>::from_start_address(X86VirtAddr::new(virt_addr as u64))
                .map_err(|_| AddressSpaceError::Misaligned)?;
            let (old_phys, old_flags) = match mapper.translate(X86VirtAddr::new(virt_addr as u64))
            {
                TranslateResult::Mapped { frame, flags, .. } => {
                    (frame.start_address().as_u64() as usize, flags)
                }
                _ => {
                    for page in pages {
                        self.dealloc_user_phys(page.new_phys);
                    }
                    return Err(AddressSpaceError::NotCommitted);
                }
            };
            let new_phys = match self.alloc_user_frame() {
                Ok(phys) => phys,
                Err(error) => {
                    for page in pages {
                        self.dealloc_user_phys(page.new_phys);
                    }
                    return Err(error);
                }
            };
            unsafe {
                ptr::copy_nonoverlapping(
                    self.hhdm_ptr(old_phys) as *const u8,
                    self.hhdm_ptr(new_phys),
                    PAGE,
                );
            }
            pages.push(RelocationPage {
                page,
                virt: virt_addr,
                old_phys,
                old_flags,
                new_phys,
            });
        }
        Ok(pages)
    }

    fn remap_page(
        &self,
        mapper: &mut OffsetPageTable<'static>,
        frame_allocator: &mut DirectMappedFrameAllocator,
        page: RelocationPage,
        phys: usize,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        let (old_frame, flush) = mapper
            .unmap(page.page)
            .map_err(|_| AddressSpaceError::NotCommitted)?;
        flush.flush();
        let new_frame = x86_64::structures::paging::PhysFrame::from_start_address(PhysAddr::new(
            phys as u64,
        ))
        .map_err(|_| AddressSpaceError::Misaligned)?;
        match unsafe { mapper.map_to(page.page, new_frame, flags, frame_allocator) } {
            Ok(flush) => {
                flush.flush();
                Ok(())
            }
            Err(_) => {
                unsafe {
                    mapper
                        .map_to(page.page, old_frame, page.old_flags, frame_allocator)
                        .unwrap_or_else(|error| {
                            panic!(
                                "x86 AddressSpace::relocate failed to restore page {:#x}: {error:?}",
                                page.virt
                            )
                        })
                        .flush();
                }
                Err(AddressSpaceError::PageTableExhausted)
            }
        }
    }

    fn rollback_relocation(
        &self,
        mapper: &mut OffsetPageTable<'static>,
        frame_allocator: &mut DirectMappedFrameAllocator,
        pages: &[RelocationPage],
        installed: usize,
    ) {
        for page in pages[..installed].iter().rev().copied() {
            self.remap_page(mapper, frame_allocator, page, page.old_phys, page.old_flags)
                .unwrap_or_else(|error| {
                    panic!(
                        "x86 AddressSpace::relocate rollback failed at {:#x}: {error}",
                        page.virt
                    )
                });
        }
        for page in pages {
            self.dealloc_user_phys(page.new_phys);
        }
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
            self.shootdown_range(region.range);
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
        self.shootdown_range(virt);

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
        if !committed_regions_cover(&reservation.committed, virt) {
            return Err(AddressSpaceError::NotCommitted);
        }
        remove_committed_range(&mut reservation.committed, virt);
        drop(state);

        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        self.unmap_pages(&mut mapper, virt)?;
        self.shootdown_range(virt);
        Ok(())
    }

    fn protect(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        self.assert_smp_safe();
        validate_range(virt)?;
        let pt_flags = page_flags_to_pt(flags)?;
        // PT is the source of truth — update entries first, treat
        // bookkeeping as cache. Wasmtime's mprotect call patterns
        // routinely span what we recorded as separate commits, so a
        // single-`CommittedRegion`-must-contain-virt gate refuses
        // valid requests.
        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        self.protect_pages(&mut mapper, virt, pt_flags)?;
        self.shootdown_range(virt);
        let mut state = self.state.lock();
        if let Some(reservation) = state
            .reservations
            .iter_mut()
            .find(|reservation| range_contains(reservation.range, virt))
        {
            for region in reservation.committed.iter_mut() {
                if region.range == virt {
                    region.flags = flags;
                }
            }
        }
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

    fn relocate(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        self.assert_smp_safe();
        validate_range(virt)?;
        let state = self.state.lock();
        let reservation = state
            .reservations
            .iter()
            .find(|reservation| range_contains(reservation.range, virt))
            .ok_or(AddressSpaceError::NotReserved)?;
        if !committed_regions_cover(&reservation.committed, virt) {
            return Err(AddressSpaceError::NotCommitted);
        }
        drop(state);

        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        let mut frame_allocator = DirectMappedFrameAllocator {
            physical_memory_offset: self.physical_memory_offset,
        };
        let pages = self.build_relocation_plan(&mapper, virt)?;
        for (index, page) in pages.iter().copied().enumerate() {
            if let Err(error) = self.remap_page(
                &mut mapper,
                &mut frame_allocator,
                page,
                page.new_phys,
                page.old_flags,
            ) {
                self.rollback_relocation(&mut mapper, &mut frame_allocator, &pages, index);
                return Err(error);
            }
        }
        self.shootdown_range(virt);
        for page in &pages {
            self.dealloc_user_phys(page.old_phys);
        }
        Ok(())
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
            });
        }
        if range.end().raw() < region.range.end().raw() {
            regions.push(CommittedRegion {
                range: VirtRange::new(
                    range.end(),
                    region.range.end().raw() - range.end().raw(),
                ),
                flags: region.flags,
            });
        }
    }
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

static USER_AS: Once<X86UserAddressSpace> = Once::new();

/// Initialise the boot-time user address space and install the
/// wasmtime custom-virtual-memory hooks. Must be called once on the
/// bootstrap CPU after Limine handoff is processed and before any
/// wasmtime engine is constructed.
pub fn install_user_address_space(physical_memory_offset: usize, processor_count: usize) {
    USER_AS.call_once(|| X86UserAddressSpace::new(physical_memory_offset, processor_count));
    custom_vm::install_hooks(&X86_VMM_HOOKS);
}

fn user_as() -> &'static X86UserAddressSpace {
    USER_AS
        .get()
        .expect("X86UserAddressSpace accessed before install_user_address_space")
}

const ENOMEM: c_int = 12;
const EINVAL: c_int = 22;

const PROT_READ: u32 = 1 << 0;
const PROT_WRITE: u32 = 1 << 1;
const PROT_EXEC: u32 = 1 << 2;

fn prot_to_flags(prot: u32) -> PageFlags {
    let mut flags = PageFlags::empty();
    if prot & PROT_READ != 0 {
        flags |= PageFlags::READ;
    }
    if prot & PROT_WRITE != 0 {
        flags |= PageFlags::WRITE;
    }
    if prot & PROT_EXEC != 0 {
        flags |= PageFlags::EXECUTE;
    }
    flags
}

fn round_up_to_page(size: usize) -> usize {
    (size + PAGE - 1) & !(PAGE - 1)
}

extern "C" fn x86_mmap_new(size: usize, prot_flags: u32, ret: &mut *mut u8) -> c_int {
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = match address_space.reserve(size) {
        Ok(range) => range,
        Err(_) => return ENOMEM,
    };
    if prot_flags != 0 {
        let flags = prot_to_flags(prot_flags);
        if address_space.commit(range, flags).is_err() {
            let _ = address_space.release(range);
            return ENOMEM;
        }
    }
    *ret = range.start.raw() as *mut u8;
    0
}

extern "C" fn x86_mmap_remap(addr: *mut u8, size: usize, prot_flags: u32) -> c_int {
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = VirtRange::new(VirtAddr::new(addr as usize), size);
    let _ = address_space.decommit(range);
    if prot_flags != 0 {
        let flags = prot_to_flags(prot_flags);
        if address_space.commit(range, flags).is_err() {
            return ENOMEM;
        }
    }
    0
}

extern "C" fn x86_munmap(ptr: *mut u8, size: usize) -> c_int {
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = VirtRange::new(VirtAddr::new(ptr as usize), size);
    match address_space.release(range) {
        Ok(()) => 0,
        Err(_) => EINVAL,
    }
}

extern "C" fn x86_mprotect(ptr: *mut u8, size: usize, prot_flags: u32) -> c_int {
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = VirtRange::new(VirtAddr::new(ptr as usize), size);
    if prot_flags == 0 {
        return match address_space.decommit(range) {
            Ok(()) => 0,
            Err(_) => EINVAL,
        };
    }
    let flags = prot_to_flags(prot_flags);
    match address_space.protect(range, flags) {
        Ok(()) => 0,
        Err(AddressSpaceError::NotCommitted) => match address_space.commit(range, flags) {
            Ok(()) => 0,
            Err(_) => ENOMEM,
        },
        Err(_) => EINVAL,
    }
}

/// Wasmtime custom-virtual-memory hook table for the x86 backend.
pub static X86_VMM_HOOKS: CustomVmHooks = CustomVmHooks {
    mmap_new: x86_mmap_new,
    mmap_remap: x86_mmap_remap,
    munmap: x86_munmap,
    mprotect: x86_mprotect,
    page_size: default_page_size,
    memory_image_new: default_memory_image_new,
    memory_image_free: default_memory_image_free,
    memory_image_map_at: default_memory_image_map_at,
};
