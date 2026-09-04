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

use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{
    AddressSpace, AddressSpaceError, PageFlags, Translation, VirtAddr, VirtRange,
};
use helios_kernel::runtime_memory::{
    self, RuntimeMemoryHooks, default_memory_image_free, default_memory_image_map_at,
    default_memory_image_new, default_page_size,
};
use helios_kernel::{
    MemoryOwner, ReservationLookup, ReservationTracker, VaCursor, allocate_user_frame_zeroed_on,
    deallocate_user_frame_on, validate_range,
};
use spin::{Mutex, Once};
use x86_64::PhysAddr;
use x86_64::VirtAddr as X86VirtAddr;
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

/// Pages one TLB-shootdown batch holds before flushing.
///
/// A frame must not be reusable while another processor can still translate
/// to it, so the frames an unmap produces are held until the shootdown for
/// their range has been acknowledged. Batching bounds that on-stack array
/// while keeping the IPI count to one round per 128 pages.
const TLB_SHOOTDOWN_BATCH_PAGES: usize = 128;

/// Owned x86 user address space. Built once at boot, accessed through
/// `&'static`.
pub struct X86UserAddressSpace {
    physical_memory_offset: usize,
    processor_count: usize,
    /// Bump pointer for fresh reservations within the user-VA window.
    /// Reservations released via `release` are returned to a free
    /// list rather than to the bump pointer; the simple bump path
    /// covers the steady-state allocate-and-release-much-later
    /// pattern that the runtime drives, while the free list catches
    /// tight reservation churn.
    va_cursor: VaCursor,
    state: Mutex<ReservationTracker>,
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
            va_cursor: VaCursor::new(USER_VA_BASE, USER_VA_END),
            state: Mutex::new(ReservationTracker::new()),
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
        self.state
            .lock()
            .reuse_free_range(byte_len)
            .or_else(|| self.va_cursor.carve(byte_len))
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
            let mapped_pages = offset / PAGE;
            let phys = match self.alloc_user_frame() {
                Ok(phys) => phys,
                Err(error) => {
                    self.rollback_partial_commit(mapper, virt.start.raw(), mapped_pages);
                    return Err(error);
                }
            };
            let frame = x86_64::structures::paging::PhysFrame::from_start_address(PhysAddr::new(
                phys as u64,
            ))
            .map_err(|_| {
                self.dealloc_user_phys(phys);
                self.rollback_partial_commit(mapper, virt.start.raw(), mapped_pages);
                AddressSpaceError::Misaligned
            })?;
            let page = Page::<Size4KiB>::from_start_address(X86VirtAddr::new(virt_addr as u64))
                .map_err(|_| {
                    self.dealloc_user_phys(phys);
                    self.rollback_partial_commit(mapper, virt.start.raw(), mapped_pages);
                    AddressSpaceError::Misaligned
                })?;
            unsafe {
                match mapper.map_to(page, frame, flags, frame_allocator) {
                    Ok(flush) => flush.flush(),
                    Err(_) => {
                        self.dealloc_user_phys(phys);
                        self.rollback_partial_commit(mapper, virt.start.raw(), mapped_pages);
                        return Err(AddressSpaceError::PageTableExhausted);
                    }
                }
            }
        }
        Ok(())
    }

    /// Unmap every committed page of `virt`, shooting the range down on
    /// every processor before its frames go back to the user-memory pool.
    ///
    /// `MapperFlush::flush` issues `INVLPG` on the calling processor only, so
    /// until the IPI has been acknowledged another core can still translate
    /// to a frame this loop has unmapped. Handing such a frame back would let
    /// the next allocation alias it through that stale translation, so the
    /// frames wait in a bounded batch and are freed on the far side of the
    /// shootdown (AGENTS §3.4). Batching is what keeps that array on the
    /// stack while still costing one IPI round per 128 pages rather than one
    /// per page.
    fn unmap_pages(
        &self,
        mapper: &mut OffsetPageTable<'static>,
        virt: VirtRange,
    ) -> Result<(), AddressSpaceError> {
        let mut batch = [0usize; TLB_SHOOTDOWN_BATCH_PAGES];
        let mut batch_count = 0;
        let mut batch_start = virt.start.raw();
        for offset in (0..virt.byte_len).step_by(PAGE) {
            let virt_addr = virt.start.raw() + offset;
            let page = Page::<Size4KiB>::from_start_address(X86VirtAddr::new(virt_addr as u64))
                .map_err(|_| AddressSpaceError::Misaligned)?;
            match mapper.unmap(page) {
                Ok((frame, flush)) => {
                    flush.flush();
                    if batch_count == 0 {
                        batch_start = virt_addr;
                    }
                    batch[batch_count] = frame.start_address().as_u64() as usize;
                    batch_count += 1;
                    if batch_count == TLB_SHOOTDOWN_BATCH_PAGES {
                        self.shootdown_and_dealloc(batch_start, &batch[..batch_count]);
                        batch_count = 0;
                    }
                }
                Err(_) => {
                    // The page was never committed. It also breaks the run the
                    // batch describes, so what is held has to be shot down as
                    // its own range before the walk moves past this address.
                    if batch_count != 0 {
                        self.shootdown_and_dealloc(batch_start, &batch[..batch_count]);
                        batch_count = 0;
                    }
                }
            }
        }
        if batch_count != 0 {
            self.shootdown_and_dealloc(batch_start, &batch[..batch_count]);
        }
        Ok(())
    }

    /// Invalidate the `frames.len()` pages starting at `start` everywhere,
    /// then return those frames to the user-memory pool.
    fn shootdown_and_dealloc(&self, start: usize, frames: &[usize]) {
        smp::shootdown_tlb_range(start, frames.len() * PAGE);
        for phys in frames {
            self.dealloc_user_phys(*phys);
        }
    }

    fn protect_pages(
        &self,
        mapper: &mut OffsetPageTable<'static>,
        virt: VirtRange,
        flags: PageTableFlags,
    ) -> Result<(), AddressSpaceError> {
        let mut old_flags = Vec::new();
        for offset in (0..virt.byte_len).step_by(PAGE) {
            let virt_addr = virt.start.raw() + offset;
            let page = Page::<Size4KiB>::from_start_address(X86VirtAddr::new(virt_addr as u64))
                .map_err(|_| AddressSpaceError::Misaligned)?;
            let previous = match mapper.translate(X86VirtAddr::new(virt_addr as u64)) {
                TranslateResult::Mapped { flags, .. } => flags,
                _ => {
                    rollback_partial_protect(mapper, &old_flags);
                    return Err(AddressSpaceError::NotCommitted);
                }
            };
            match unsafe { mapper.update_flags(page, flags) } {
                Ok(flush) => {
                    flush.flush();
                    old_flags.push((page, previous));
                }
                Err(_) => {
                    rollback_partial_protect(mapper, &old_flags);
                    return Err(AddressSpaceError::NotCommitted);
                }
            }
        }
        Ok(())
    }

    fn alloc_user_frame(&self) -> Result<usize, AddressSpaceError> {
        let raw = allocate_user_frame_zeroed_on(smp::current_processor())
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
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
        deallocate_user_frame_on(smp::current_processor(), ptr);
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
            let (old_phys, old_flags) = match mapper.translate(X86VirtAddr::new(virt_addr as u64)) {
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
        let new_frame =
            x86_64::structures::paging::PhysFrame::from_start_address(PhysAddr::new(phys as u64))
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
        // The pages this restored were briefly mapped to the replacement
        // frames, and `remap_page` only invalidates the local processor, so
        // the replacements are not free to reuse until every processor has
        // dropped those translations (AGENTS §3.4).
        if let Some(first) = pages.first() {
            self.shootdown_range(VirtRange::new(
                VirtAddr::new(first.virt),
                pages.len() * PAGE,
            ));
        }
        for page in pages {
            self.dealloc_user_phys(page.new_phys);
        }
    }

    fn rollback_partial_commit(
        &self,
        mapper: &mut OffsetPageTable<'static>,
        start: usize,
        mapped_pages: usize,
    ) {
        let mut batch = [0usize; TLB_SHOOTDOWN_BATCH_PAGES];
        let mut batch_count = 0;
        let mut batch_start = start;
        for page_index in 0..mapped_pages {
            let virt = start + page_index * PAGE;
            let page = Page::<Size4KiB>::from_start_address(X86VirtAddr::new(virt as u64))
                .unwrap_or_else(|error| {
                    panic!("x86 AddressSpace::commit rollback got invalid page {virt:#x}: {error}")
                });
            let (frame, flush) = mapper.unmap(page).unwrap_or_else(|error| {
                panic!("x86 AddressSpace::commit rollback failed at {virt:#x}: {error:?}")
            });
            flush.flush();
            if batch_count == 0 {
                batch_start = virt;
            }
            batch[batch_count] = frame.start_address().as_u64() as usize;
            batch_count += 1;
            if batch_count == TLB_SHOOTDOWN_BATCH_PAGES {
                self.shootdown_and_dealloc(batch_start, &batch[..batch_count]);
                batch_count = 0;
            }
        }
        if batch_count != 0 {
            self.shootdown_and_dealloc(batch_start, &batch[..batch_count]);
        }
    }

    /// Decommit only the parts of `range` that are actually committed.
    ///
    /// The runtime resets a pooled slot by asking for a range whose committed
    /// extent it does not track precisely, so a whole-range decommit would
    /// fail on the first uncommitted page.
    fn decommit_committed_subranges(&self, range: VirtRange) -> Result<(), AddressSpaceError> {
        self.assert_smp_safe();
        validate_range(range)?;
        let mut state = self.state.lock();
        let subranges = state.take_committed_intersections(range)?;
        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        for subrange in subranges {
            self.unmap_pages(&mut mapper, subrange)?;
        }
        Ok(())
    }

    /// Give `range` `flags`, committing the parts of it that are not
    /// committed yet and re-protecting the parts that are.
    fn ensure_accessible_subranges(
        &self,
        range: VirtRange,
        flags: PageFlags,
    ) -> Result<(), AddressSpaceError> {
        self.assert_smp_safe();
        validate_range(range)?;
        let pt_flags = page_flags_to_pt(flags)?;
        let mut state = self.state.lock();
        let plan = state.accessibility_plan(range)?;
        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        let mut frame_allocator = DirectMappedFrameAllocator {
            physical_memory_offset: self.physical_memory_offset,
        };
        for subrange in plan.protect {
            state.ensure_committed(subrange)?;
            self.protect_pages(&mut mapper, subrange, pt_flags)?;
            self.shootdown_range(subrange);
            state.record_protect(subrange, flags)?;
        }
        for subrange in plan.commit {
            state.precheck_commit(subrange)?;
            self.map_pages(&mut mapper, &mut frame_allocator, subrange, pt_flags)?;
            self.shootdown_range(subrange);
            // This backend has no swap, so committing over the range can
            // never orphan a swap extent; the assertion keeps that true if
            // swap reaches this architecture (#25).
            let orphaned = state.record_commit(subrange, flags, MemoryOwner::NONE)?;
            debug_assert!(orphaned.is_empty());
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
        self.state.lock().reserve(range);
        Ok(range)
    }

    fn release(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        self.assert_smp_safe();
        let released = self.state.lock().release(virt)?;
        // This backend has no swap, so a released reservation never holds a
        // swap token; the assertion keeps that true if swap ever reaches
        // this architecture (#25).
        debug_assert!(released.swapped.is_empty());
        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        for region in &released.committed {
            self.unmap_pages(&mut mapper, region.range)?;
        }

        self.state.lock().push_free_range(virt);
        Ok(())
    }

    fn commit(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        self.assert_smp_safe();
        validate_range(virt)?;
        let pt_flags = page_flags_to_pt(flags)?;

        self.state.lock().precheck_commit(virt)?;

        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        let mut frame_allocator = DirectMappedFrameAllocator {
            physical_memory_offset: self.physical_memory_offset,
        };
        self.map_pages(&mut mapper, &mut frame_allocator, virt, pt_flags)?;
        self.shootdown_range(virt);

        self.state
            .lock()
            .record_commit(virt, flags, MemoryOwner::NONE)?;
        Ok(())
    }

    fn decommit(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        self.assert_smp_safe();
        validate_range(virt)?;
        let _ = self.state.lock().record_decommit(virt)?;

        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        self.unmap_pages(&mut mapper, virt)
    }

    fn protect(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        self.assert_smp_safe();
        validate_range(virt)?;
        let pt_flags = page_flags_to_pt(flags)?;
        self.state.lock().ensure_committed(virt)?;
        let mut mapper = unsafe { smp::current_mapper(self.physical_memory_offset) };
        self.protect_pages(&mut mapper, virt, pt_flags)?;
        self.shootdown_range(virt);
        self.state.lock().record_protect(virt, flags)?;
        Ok(())
    }

    fn translate(&self, addr: VirtAddr) -> Translation {
        if addr.raw() < USER_VA_BASE || addr.raw() >= USER_VA_END {
            return Translation::Unmapped;
        }
        let committed = match self.state.lock().lookup(addr) {
            ReservationLookup::Unreserved => return Translation::Unmapped,
            ReservationLookup::Reserved => return Translation::Reserved,
            ReservationLookup::Committed(region) => region,
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
        self.state.lock().ensure_committed(virt)?;

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

fn rollback_partial_protect(
    mapper: &mut OffsetPageTable<'static>,
    old_flags: &[(Page<Size4KiB>, PageTableFlags)],
) {
    for (page, flags) in old_flags.iter().rev().copied() {
        unsafe {
            mapper
                .update_flags(page, flags)
                .unwrap_or_else(|error| {
                    panic!("x86 AddressSpace::protect rollback failed: {error:?}")
                })
                .flush();
        }
    }
}

fn page_flags_to_pt(flags: PageFlags) -> Result<PageTableFlags, AddressSpaceError> {
    if flags.is_empty() {
        return Err(AddressSpaceError::InvalidFlags);
    }
    // No `USER_ACCESSIBLE`: every helios thread runs in ring 0, guest wasm
    // included, so the "user" window is a range the kernel owns on a guest's
    // behalf rather than a privilege boundary. Marking it user-accessible
    // would buy nothing and would make the mappings unusable the moment SMAP
    // or SMEP is enabled — including for the runtime's compiled code, which
    // is mapped in this window and fetched in ring 0. This matches the
    // aarch64 backend's `AP_KERNEL_RW`.
    let mut pt = PageTableFlags::PRESENT;
    if flags.contains(PageFlags::WRITE) {
        pt |= PageTableFlags::WRITABLE;
    }
    if !flags.contains(PageFlags::EXECUTE) {
        pt |= PageTableFlags::NO_EXECUTE;
    }
    Ok(pt)
}

static USER_AS: Once<X86UserAddressSpace> = Once::new();

/// Initialise the boot-time user address space and install the
/// runtime custom-virtual-memory hooks. Must be called once on the
/// bootstrap CPU after Limine handoff is processed and before any
/// runtime engine is constructed.
pub fn install_user_address_space(physical_memory_offset: usize, processor_count: usize) {
    USER_AS.call_once(|| X86UserAddressSpace::new(physical_memory_offset, processor_count));
    runtime_memory::install_hooks(&X86_VMM_HOOKS);
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
        Err(error) => {
            tracing::error!(
                target: "helios_x86::vmm",
                size,
                prot_flags,
                ?error,
                "mmap_new reserve failed"
            );
            return ENOMEM;
        }
    };
    if prot_flags != 0 {
        let flags = prot_to_flags(prot_flags);
        if let Err(error) = address_space.commit(range, flags) {
            tracing::error!(
                target: "helios_x86::vmm",
                size,
                prot_flags,
                ?error,
                "mmap_new commit failed"
            );
            let _ = address_space.release(range);
            return ENOMEM;
        }
    }
    *ret = range.start.raw() as *mut u8;
    tracing::trace!(
        target: "helios_x86::vmm",
        size,
        prot_flags,
        addr = range.start.raw(),
        "mmap_new ok"
    );
    0
}

extern "C" fn x86_mmap_remap(addr: *mut u8, size: usize, prot_flags: u32) -> c_int {
    // The runtime's `mmap_remap` rebinds an existing mapping to fresh
    // anonymous-zero pages with `prot_flags`. This address space cannot swap
    // to brand-new frames in a single transaction, so the closest faithful
    // sequence is decommit→commit: decommit returns the old frames to the
    // user-memory pool, commit takes fresh zeroed ones and maps them over the
    // same range. The window where the range is uncommitted is invisible to
    // the runtime, which only remaps a slot between instances, with no thread
    // touching it.
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = VirtRange::new(VirtAddr::new(addr as usize), size);
    if let Err(error) = address_space.decommit_committed_subranges(range) {
        tracing::error!(
            target: "helios_x86::vmm",
            addr = addr as usize,
            size,
            ?error,
            "mmap_remap decommit failed"
        );
        return EINVAL;
    }
    if prot_flags != 0 {
        let flags = prot_to_flags(prot_flags);
        if let Err(error) = address_space.commit(range, flags) {
            tracing::error!(
                target: "helios_x86::vmm",
                addr = addr as usize,
                size,
                prot_flags,
                ?error,
                "mmap_remap commit failed"
            );
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
        return match address_space.decommit_committed_subranges(range) {
            Ok(()) => 0,
            Err(error) => {
                tracing::error!(
                    target: "helios_x86::vmm",
                    addr = ptr as usize,
                    size,
                    ?error,
                    "mprotect(prot=0) decommit failed"
                );
                EINVAL
            }
        };
    }
    let flags = prot_to_flags(prot_flags);
    match address_space.ensure_accessible_subranges(range, flags) {
        Ok(()) => 0,
        Err(error) => {
            tracing::error!(
                target: "helios_x86::vmm",
                addr = ptr as usize,
                size,
                prot_flags,
                ?error,
                "mprotect failed"
            );
            EINVAL
        }
    }
}

/// Runtime custom-virtual-memory hook table for the x86 backend.
/// Address-space mutations route through the singleton
/// `X86UserAddressSpace`; COW image creation is opted out of
/// (`default_memory_image_new` returns `NULL`), so the runtime falls back to
/// per-instance memcpy initialization.
pub static X86_VMM_HOOKS: RuntimeMemoryHooks = RuntimeMemoryHooks {
    mmap_new: x86_mmap_new,
    mmap_remap: x86_mmap_remap,
    munmap: x86_munmap,
    mprotect: x86_mprotect,
    page_size: default_page_size,
    memory_image_new: default_memory_image_new,
    memory_image_free: default_memory_image_free,
    memory_image_map_at: default_memory_image_map_at,
};

const _: () = {
    // Pin the C ABI so a runtime ABI revision that changes the
    // `RuntimeMemoryImage` opaque ptr type fails the build instead of
    // mismatching at link time.
    let _: extern "C" fn(*const u8, usize, &mut *mut runtime_memory::RuntimeMemoryImage) -> c_int =
        default_memory_image_new;
};
