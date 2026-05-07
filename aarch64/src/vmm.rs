//! ARMv8-A user virtual address space hanging off the live TTBR1.
//!
//! Limine sets up a higher-half direct map (HHDM) and a kernel image
//! mapping; the kernel runs in TTBR1's upper half. The MMU is already
//! enabled when control reaches Rust code, so this AS impl mutates
//! the live page tables rather than installing fresh ones.
//!
//! Reservations live in `[USER_VA_BASE..USER_VA_END)`, a clean 32 TiB
//! window inside TTBR1 well clear of HHDM and the kernel image so
//! address values are unambiguous in fault traces. Page-table
//! intermediate frames are allocated through the kernel global
//! allocator (`alloc_zeroed`); each frame is one 4 KiB physical
//! page reachable through HHDM at virtual address
//! `physical_memory_offset + phys`.
//!
//! TLB invalidation uses the inner-shareable VAALE1IS instruction so
//! every CPU running in the same inner-shareable domain sees the
//! update without an explicit IPI. AArch64's broadcast TLB
//! invalidation is the architectural equivalent of the x86 IPI
//! shootdown protocol.

extern crate alloc;

use alloc::alloc::{Layout, alloc_zeroed};
use alloc::vec::Vec;
use core::arch::asm;
use core::ffi::c_int;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering};

use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{
    AddressSpace, AddressSpaceError, PageFlags, Translation, VirtAddr, VirtRange,
};
use helios_kernel::runtime_memory::{
    self, RuntimeMemoryHooks, RuntimeMemoryImage, default_memory_image_free,
    default_memory_image_map_at, default_memory_image_new, default_page_size,
};
use helios_kernel::{allocate_user_frame_zeroed_on, deallocate_user_frame_on};
use spin::{Mutex, Once};

const PAGE: usize = 4096;
// Release AArch64/HVF `quickjs-loop` evidence: after Wasmtime stopped asking
// the custom VM to scan full static reservations, batching page-table barriers
// moved the profiled median from 46 ms to 45 ms and reduced Store teardown from
// 54.3 ms to 52.6 ms over five runs. Keep frame release after the batched TLB
// flush so stale translations cannot observe recycled frames.
const TLB_DECOMMIT_BATCH_PAGES: usize = 128;

const VALID: u64 = 1 << 0;
const PAGE_DESCRIPTOR: u64 = 0b11;
const TABLE_DESCRIPTOR: u64 = 0b11;
const AF: u64 = 1 << 10;
const SH_INNER: u64 = 0b11 << 8;
const ATTR_NORMAL: u64 = 0; // MAIR index 0: outer/inner write-back cacheable.
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const PT_ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;

const AP_KERNEL_RW: u64 = 0b00 << 6;
const AP_KERNEL_RO: u64 = 0b10 << 6;

const USER_VA_BASE: usize = 0xFFFF_C000_0000_0000;
const USER_VA_END: usize = 0xFFFF_E000_0000_0000;

/// Per-platform aarch64 user address space.
pub struct Aarch64UserAddressSpace {
    physical_memory_offset: usize,
    next_va: AtomicUsize,
    state: Mutex<UserState>,
}

#[derive(Default)]
struct UserState {
    reservations: Vec<Reservation>,
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
    virt: usize,
    old_entry: u64,
    new_phys: usize,
}

impl Aarch64UserAddressSpace {
    pub fn new(physical_memory_offset: usize) -> Self {
        Self {
            physical_memory_offset,
            next_va: AtomicUsize::new(USER_VA_BASE),
            state: Mutex::new(UserState::default()),
        }
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
            match self
                .next_va
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(VirtRange::new(VirtAddr::new(current), byte_len)),
                Err(observed) => current = observed,
            }
        }
    }

    fn root(&self) -> *mut u64 {
        let ttbr1 = read_ttbr1_el1();
        (ttbr1 + self.physical_memory_offset) as *mut u64
    }

    fn ensure_table(&self, parent: *mut u64, index: usize) -> *mut u64 {
        let entry_ptr = unsafe { parent.add(index) };
        let entry = unsafe { entry_ptr.read_volatile() };
        if entry & 0b11 == TABLE_DESCRIPTOR {
            let phys = (entry & PT_ADDR_MASK) as usize;
            return (phys + self.physical_memory_offset) as *mut u64;
        }
        if entry & VALID != 0 {
            panic!("Aarch64 user-VA L? index {index:#x} collides with a non-table descriptor");
        }
        let layout = Layout::from_size_align(PAGE, PAGE).expect("PT layout");
        let raw = unsafe { alloc_zeroed(layout) };
        let raw = NonNull::new(raw).expect("PT alloc");
        let virt = raw.as_ptr() as usize;
        let phys = virt - self.physical_memory_offset;
        let new_descriptor = (phys as u64) | TABLE_DESCRIPTOR;
        unsafe {
            entry_ptr.write_volatile(new_descriptor);
            asm!("dsb ishst", options(nostack, preserves_flags));
        }
        virt as *mut u64
    }

    fn map_4k_no_flush(
        &self,
        virt: usize,
        phys: usize,
        flags: u64,
    ) -> Result<(), AddressSpaceError> {
        let l0 = self.root();
        let l0_index = (virt >> 39) & 0x1ff;
        let l1 = self.ensure_table(l0, l0_index);
        let l1_index = (virt >> 30) & 0x1ff;
        let l2 = self.ensure_table(l1, l1_index);
        let l2_index = (virt >> 21) & 0x1ff;
        let l3 = self.ensure_table(l2, l2_index);
        let l3_index = (virt >> 12) & 0x1ff;
        let entry_ptr = unsafe { l3.add(l3_index) };
        let entry = unsafe { entry_ptr.read_volatile() };
        if entry & VALID != 0 {
            return Err(AddressSpaceError::Overlap);
        }
        let descriptor = (phys as u64) | flags | PAGE_DESCRIPTOR | AF | SH_INNER;
        unsafe {
            entry_ptr.write_volatile(descriptor);
        }
        Ok(())
    }

    fn unmap_4k(&self, virt: usize) -> Result<u64, AddressSpaceError> {
        let entry = self.unmap_4k_no_flush(virt)?;
        unsafe {
            flush_tlb_pages(virt, 1);
        }
        Ok(entry)
    }

    fn unmap_4k_no_flush(&self, virt: usize) -> Result<u64, AddressSpaceError> {
        let l0 = self.root();
        let l0_index = (virt >> 39) & 0x1ff;
        let l0_entry = unsafe { l0.add(l0_index).read_volatile() };
        if l0_entry & 0b11 != TABLE_DESCRIPTOR {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l1 = ((l0_entry & PT_ADDR_MASK) as usize + self.physical_memory_offset) as *mut u64;
        let l1_index = (virt >> 30) & 0x1ff;
        let l1_entry = unsafe { l1.add(l1_index).read_volatile() };
        if l1_entry & 0b11 != TABLE_DESCRIPTOR {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l2 = ((l1_entry & PT_ADDR_MASK) as usize + self.physical_memory_offset) as *mut u64;
        let l2_index = (virt >> 21) & 0x1ff;
        let l2_entry = unsafe { l2.add(l2_index).read_volatile() };
        if l2_entry & 0b11 != TABLE_DESCRIPTOR {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l3 = ((l2_entry & PT_ADDR_MASK) as usize + self.physical_memory_offset) as *mut u64;
        let l3_index = (virt >> 12) & 0x1ff;
        let entry_ptr = unsafe { l3.add(l3_index) };
        let entry = unsafe { entry_ptr.read_volatile() };
        if entry & VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        unsafe {
            entry_ptr.write_volatile(0);
        }
        Ok(entry)
    }

    fn protect_4k_no_flush(&self, virt: usize, flags: u64) -> Result<(), AddressSpaceError> {
        let l0 = self.root();
        let l0_index = (virt >> 39) & 0x1ff;
        let l0_entry = unsafe { l0.add(l0_index).read_volatile() };
        if l0_entry & 0b11 != TABLE_DESCRIPTOR {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l1 = ((l0_entry & PT_ADDR_MASK) as usize + self.physical_memory_offset) as *mut u64;
        let l1_index = (virt >> 30) & 0x1ff;
        let l1_entry = unsafe { l1.add(l1_index).read_volatile() };
        if l1_entry & 0b11 != TABLE_DESCRIPTOR {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l2 = ((l1_entry & PT_ADDR_MASK) as usize + self.physical_memory_offset) as *mut u64;
        let l2_index = (virt >> 21) & 0x1ff;
        let l2_entry = unsafe { l2.add(l2_index).read_volatile() };
        if l2_entry & 0b11 != TABLE_DESCRIPTOR {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l3 = ((l2_entry & PT_ADDR_MASK) as usize + self.physical_memory_offset) as *mut u64;
        let l3_index = (virt >> 12) & 0x1ff;
        let entry_ptr = unsafe { l3.add(l3_index) };
        let entry = unsafe { entry_ptr.read_volatile() };
        if entry & VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        let phys = entry & PT_ADDR_MASK;
        let new_descriptor = phys | flags | PAGE_DESCRIPTOR | AF | SH_INNER;
        unsafe {
            entry_ptr.write_volatile(new_descriptor);
        }
        Ok(())
    }

    fn translate_4k(&self, virt: usize) -> Option<u64> {
        let l0 = self.root();
        let l0_entry = unsafe { l0.add((virt >> 39) & 0x1ff).read_volatile() };
        if l0_entry & 0b11 != TABLE_DESCRIPTOR {
            return None;
        }
        let l1 = ((l0_entry & PT_ADDR_MASK) as usize + self.physical_memory_offset) as *mut u64;
        let l1_entry = unsafe { l1.add((virt >> 30) & 0x1ff).read_volatile() };
        if l1_entry & 0b11 != TABLE_DESCRIPTOR {
            return None;
        }
        let l2 = ((l1_entry & PT_ADDR_MASK) as usize + self.physical_memory_offset) as *mut u64;
        let l2_entry = unsafe { l2.add((virt >> 21) & 0x1ff).read_volatile() };
        if l2_entry & 0b11 != TABLE_DESCRIPTOR {
            return None;
        }
        let l3 = ((l2_entry & PT_ADDR_MASK) as usize + self.physical_memory_offset) as *mut u64;
        let entry = unsafe { l3.add((virt >> 12) & 0x1ff).read_volatile() };
        if entry & VALID == 0 {
            return None;
        }
        Some(entry)
    }

    fn replace_4k(&self, virt: usize, new_entry: u64) -> Result<u64, AddressSpaceError> {
        let l0 = self.root();
        let l0_entry = unsafe { l0.add((virt >> 39) & 0x1ff).read_volatile() };
        if l0_entry & 0b11 != TABLE_DESCRIPTOR {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l1 = ((l0_entry & PT_ADDR_MASK) as usize + self.physical_memory_offset) as *mut u64;
        let l1_entry = unsafe { l1.add((virt >> 30) & 0x1ff).read_volatile() };
        if l1_entry & 0b11 != TABLE_DESCRIPTOR {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l2 = ((l1_entry & PT_ADDR_MASK) as usize + self.physical_memory_offset) as *mut u64;
        let l2_entry = unsafe { l2.add((virt >> 21) & 0x1ff).read_volatile() };
        if l2_entry & 0b11 != TABLE_DESCRIPTOR {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l3 = ((l2_entry & PT_ADDR_MASK) as usize + self.physical_memory_offset) as *mut u64;
        let entry_ptr = unsafe { l3.add((virt >> 12) & 0x1ff) };
        let old_entry = unsafe { entry_ptr.read_volatile() };
        if old_entry & VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        unsafe {
            entry_ptr.write_volatile(new_entry);
            asm!("dsb ishst", options(nostack, preserves_flags));
            invalidate_tlb_one(virt);
            asm!("dsb ish", options(nostack, preserves_flags));
            asm!("isb", options(nostack, preserves_flags));
        }
        Ok(old_entry)
    }

    fn alloc_user_frame(&self) -> Result<usize, AddressSpaceError> {
        let raw = allocate_user_frame_zeroed_on(crate::current_processor_runtime().logical_id())
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let virt = raw.as_ptr() as usize;
        Ok(virt - self.physical_memory_offset)
    }

    fn hhdm_ptr(&self, phys: usize) -> *mut u8 {
        let virt = phys
            .checked_add(self.physical_memory_offset)
            .unwrap_or_else(|| panic!("Aarch64 user-frame HHDM address overflow"));
        virt as *mut u8
    }

    fn dealloc_user_phys(&self, phys: usize) {
        let ptr = NonNull::new(self.hhdm_ptr(phys))
            .unwrap_or_else(|| panic!("Aarch64 user-frame dealloc received null HHDM pointer"));
        deallocate_user_frame_on(crate::current_processor_runtime().logical_id(), ptr);
    }

    fn dealloc_user_frame(&self, entry: u64) {
        let phys = (entry & PT_ADDR_MASK) as usize;
        self.dealloc_user_phys(phys);
    }

    fn build_relocation_plan(
        &self,
        virt: VirtRange,
    ) -> Result<Vec<RelocationPage>, AddressSpaceError> {
        let mut pages: Vec<RelocationPage> = Vec::new();
        for offset in (0..virt.byte_len).step_by(PAGE) {
            let virt_addr = virt.start.raw() + offset;
            let old_entry = match self.translate_4k(virt_addr) {
                Some(entry) => entry,
                None => {
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
                    self.hhdm_ptr((old_entry & PT_ADDR_MASK) as usize) as *const u8,
                    self.hhdm_ptr(new_phys),
                    PAGE,
                );
            }
            pages.push(RelocationPage {
                virt: virt_addr,
                old_entry,
                new_phys,
            });
        }
        Ok(pages)
    }

    fn rollback_relocation(&self, pages: &[RelocationPage], installed: usize) {
        for page in pages[..installed].iter().rev() {
            self.replace_4k(page.virt, page.old_entry)
                .unwrap_or_else(|error| {
                    panic!(
                        "Aarch64 AddressSpace::relocate rollback failed at {:#x}: {error}",
                        page.virt
                    )
                });
        }
        for page in pages {
            self.dealloc_user_phys(page.new_phys);
        }
    }

    fn decommit_mapped_range(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        let mut batch_entries = [0u64; TLB_DECOMMIT_BATCH_PAGES];
        let mut batch_count = 0;
        let mut batch_start = virt.start.raw();
        for offset in (0..virt.byte_len).step_by(PAGE) {
            let page = virt.start.raw() + offset;
            if batch_count == 0 {
                batch_start = page;
            }
            batch_entries[batch_count] = match self.unmap_4k_no_flush(page) {
                Ok(entry) => entry,
                Err(error) => {
                    if batch_count != 0 {
                        self.flush_and_dealloc_entries(batch_start, &batch_entries[..batch_count]);
                    }
                    return Err(error);
                }
            };
            batch_count += 1;
            if batch_count == TLB_DECOMMIT_BATCH_PAGES {
                self.flush_and_dealloc_entries(batch_start, &batch_entries[..batch_count]);
                batch_count = 0;
            }
        }
        if batch_count != 0 {
            self.flush_and_dealloc_entries(batch_start, &batch_entries[..batch_count]);
        }
        Ok(())
    }

    fn flush_and_dealloc_entries(&self, start: usize, entries: &[u64]) {
        unsafe {
            flush_tlb_pages(start, entries.len());
        }
        for entry in entries {
            self.dealloc_user_frame(*entry);
        }
    }
}

impl AddressSpace for Aarch64UserAddressSpace {
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
        let mut state = self.state.lock();
        let index = state
            .reservations
            .iter()
            .position(|reservation| reservation.range == virt)
            .ok_or(AddressSpaceError::NotReserved)?;
        let reservation = state.reservations.swap_remove(index);
        drop(state);
        for region in &reservation.committed {
            for offset in (0..region.range.byte_len).step_by(PAGE) {
                if let Ok(entry) = self.unmap_4k(region.range.start.raw() + offset) {
                    self.dealloc_user_frame(entry);
                }
            }
        }
        self.state.lock().free_list.push(reservation.range);
        Ok(())
    }

    fn commit(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let pte_flags = page_flags_to_pte(flags)?;

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

        let mut mapped_pages = 0;
        for offset in (0..virt.byte_len).step_by(PAGE) {
            let virt_addr = virt.start.raw() + offset;
            let phys = match self.alloc_user_frame() {
                Ok(phys) => phys,
                Err(error) => {
                    if mapped_pages != 0 {
                        unsafe {
                            flush_tlb_pages(virt.start.raw(), mapped_pages);
                        }
                    }
                    return Err(error);
                }
            };
            if let Err(error) = self.map_4k_no_flush(virt_addr, phys, pte_flags) {
                self.dealloc_user_phys(phys);
                if mapped_pages != 0 {
                    unsafe {
                        flush_tlb_pages(virt.start.raw(), mapped_pages);
                    }
                }
                return Err(error);
            }
            mapped_pages += 1;
        }
        if mapped_pages != 0 {
            unsafe {
                flush_tlb_pages(virt.start.raw(), mapped_pages);
            }
        }

        let mut state = self.state.lock();
        let reservation = find_reservation_mut(&mut state.reservations, virt)?;
        reservation
            .committed
            .push(CommittedRegion { range: virt, flags });
        Ok(())
    }

    fn decommit(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let mut state = self.state.lock();
        let reservation = find_reservation_mut(&mut state.reservations, virt)?;
        if !committed_regions_cover(&reservation.committed, virt) {
            return Err(AddressSpaceError::NotCommitted);
        }
        remove_committed_range(&mut reservation.committed, virt);
        drop(state);
        self.decommit_mapped_range(virt)?;
        Ok(())
    }

    fn protect(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let pte_flags = page_flags_to_pte(flags)?;
        // Walk the page table directly rather than constraining the
        // call to a single bookkeeping `CommittedRegion`. The runtime's
        // `make_readonly` / `make_executable` routinely span what
        // we recorded as separate commits (e.g. when a code memory
        // is committed in halves and then protected as a single
        // span); updating per-page PTEs and rejecting only on
        // genuinely missing mappings keeps the semantic at "the PT
        // is the source of truth".
        let mut protected_pages = 0;
        for offset in (0..virt.byte_len).step_by(PAGE) {
            if let Err(error) = self.protect_4k_no_flush(virt.start.raw() + offset, pte_flags) {
                if protected_pages != 0 {
                    unsafe {
                        flush_tlb_pages(virt.start.raw(), protected_pages);
                    }
                }
                return Err(error);
            }
            protected_pages += 1;
        }
        if protected_pages != 0 {
            unsafe {
                flush_tlb_pages(virt.start.raw(), protected_pages);
            }
        }
        // Best-effort bookkeeping update — flag changes on
        // exact-match committed regions stay accurate, partial
        // overlaps lose the recorded flags and fall back to
        // re-querying the PT through `translate`.
        let mut state = self.state.lock();
        if let Some(reservation) = state
            .reservations
            .iter_mut()
            .find(|reservation| range_contains(reservation.range, virt))
        {
            remove_committed_range(&mut reservation.committed, virt);
            reservation
                .committed
                .push(CommittedRegion { range: virt, flags });
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
        let virt = addr.raw() & !(PAGE - 1);
        match self.translate_4k(virt) {
            Some(entry) => {
                let phys = (entry & PT_ADDR_MASK) as usize;
                Translation::Committed {
                    frame: PhysFrame::from_phys_addr(phys),
                    flags: committed.flags,
                }
            }
            None => Translation::Reserved,
        }
    }

    fn relocate(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
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

        let pages = self.build_relocation_plan(virt)?;
        for (index, page) in pages.iter().enumerate() {
            let new_entry = (page.new_phys as u64) | (page.old_entry & !PT_ADDR_MASK);
            if let Err(error) = self.replace_4k(page.virt, new_entry) {
                self.rollback_relocation(&pages, index);
                return Err(error);
            }
        }

        for page in &pages {
            self.dealloc_user_frame(page.old_entry);
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
                range: VirtRange::new(range.end(), region.range.end().raw() - range.end().raw()),
                flags: region.flags,
            });
        }
    }
}

fn page_flags_to_pte(flags: PageFlags) -> Result<u64, AddressSpaceError> {
    if flags.is_empty() {
        return Err(AddressSpaceError::InvalidFlags);
    }
    let mut pte = (ATTR_NORMAL << 2) | PXN | UXN;
    if flags.contains(PageFlags::WRITE) {
        pte |= AP_KERNEL_RW;
    } else {
        pte |= AP_KERNEL_RO;
    }
    if flags.contains(PageFlags::EXECUTE) {
        pte &= !PXN;
    }
    Ok(pte)
}

fn read_ttbr1_el1() -> usize {
    let value: usize;
    unsafe {
        asm!("mrs {value}, ttbr1_el1", value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value & PT_ADDR_MASK as usize
}

unsafe fn invalidate_tlb_one(virt: usize) {
    let va_page = virt >> 12;
    unsafe {
        asm!("tlbi vaale1is, {va_page}", va_page = in(reg) va_page, options(nostack, preserves_flags));
    }
}

unsafe fn flush_tlb_pages(start: usize, pages: usize) {
    if pages == 0 {
        return;
    }
    unsafe {
        asm!("dsb ishst", options(nostack, preserves_flags));
        for page in 0..pages {
            invalidate_tlb_one(start + page * PAGE);
        }
        asm!("dsb ish", options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

/// Boot-time singleton holding the live `Aarch64UserAddressSpace`.
/// `install_user_address_space` is the only writer; the runtime
/// custom-virtual-memory hooks are the only readers. The Once means
/// the AS is in BSS until the first `call_once`, after which every
/// access is a single atomic load.
static USER_AS: Once<Aarch64UserAddressSpace> = Once::new();

/// Initialise the boot-time user address space and register its
/// runtime custom-virtual-memory hooks. Must be called once on the
/// bootstrap hart, before any runtime engine is constructed. The
/// runtime's first reserved-memory call dispatches through the kernel's
/// `runtime_memory::install_hooks` table to the function pointers below.
pub fn install_user_address_space(physical_memory_offset: usize) {
    USER_AS.call_once(|| Aarch64UserAddressSpace::new(physical_memory_offset));
    runtime_memory::install_hooks(&AARCH64_VMM_HOOKS);
}

fn user_as() -> &'static Aarch64UserAddressSpace {
    USER_AS
        .get()
        .expect("Aarch64UserAddressSpace accessed before install_user_address_space")
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

extern "C" fn aarch64_mmap_new(size: usize, prot_flags: u32, ret: &mut *mut u8) -> c_int {
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = match address_space.reserve(size) {
        Ok(range) => range,
        Err(error) => {
            tracing::error!(
                target: "helios_aarch64::vmm",
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
                target: "helios_aarch64::vmm",
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
        target: "helios_aarch64::vmm",
        size,
        prot_flags,
        addr = range.start.raw(),
        "mmap_new ok"
    );
    0
}

extern "C" fn aarch64_mmap_remap(addr: *mut u8, size: usize, prot_flags: u32) -> c_int {
    // The runtime's `mmap_remap` semantically rebinds an existing
    // mapping to fresh anonymous-zero pages with `prot_flags`. Our
    // AddressSpace cannot atomically swap to brand-new frames in a
    // single transaction (that's what `relocate` will eventually
    // do), so the closest faithful sequence is decommit→commit:
    // decommit releases the old frames back to the kernel
    // allocator, commit grabs fresh frames from the unified user-memory
    // pool and maps them at the same virtual range with the requested flags. The
    // intermediate window where the range is uncommitted is
    // invisible to the runtime because remap is called between
    // instances on a slot the runtime guarantees no thread is
    // touching.
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = VirtRange::new(VirtAddr::new(addr as usize), size);
    match decommit_committed_pages(address_space, range) {
        Ok(()) => {}
        Err(error) => {
            tracing::error!(
                target: "helios_aarch64::vmm",
                addr = addr as usize,
                size,
                ?error,
                "mmap_remap decommit failed"
            );
            return EINVAL;
        }
    }
    if prot_flags != 0 {
        let flags = prot_to_flags(prot_flags);
        if address_space.commit(range, flags).is_err() {
            return ENOMEM;
        }
    }
    0
}

extern "C" fn aarch64_munmap(ptr: *mut u8, size: usize) -> c_int {
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = VirtRange::new(VirtAddr::new(ptr as usize), size);
    match address_space.release(range) {
        Ok(()) => {
            tracing::trace!(target: "helios_aarch64::vmm", addr = ptr as usize, size, "munmap ok");
            0
        }
        Err(error) => {
            tracing::error!(
                target: "helios_aarch64::vmm",
                addr = ptr as usize,
                size,
                ?error,
                "munmap failed"
            );
            EINVAL
        }
    }
}

extern "C" fn aarch64_mprotect(ptr: *mut u8, size: usize, prot_flags: u32) -> c_int {
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = VirtRange::new(VirtAddr::new(ptr as usize), size);
    if prot_flags == 0 {
        return match decommit_committed_pages(address_space, range) {
            Ok(()) => 0,
            Err(error) => {
                tracing::error!(
                    target: "helios_aarch64::vmm",
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
    match ensure_accessible(address_space, range, flags) {
        Ok(()) => 0,
        Err(error) => {
            tracing::error!(
                target: "helios_aarch64::vmm",
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

fn decommit_committed_pages(
    address_space: &Aarch64UserAddressSpace,
    range: VirtRange,
) -> Result<(), AddressSpaceError> {
    for offset in (0..range.byte_len).step_by(PAGE) {
        let page = VirtRange::new(VirtAddr::new(range.start.raw() + offset), PAGE);
        match address_space.translate(page.start) {
            Translation::Committed { .. } => address_space.decommit(page)?,
            Translation::Reserved => {}
            Translation::Unmapped => return Err(AddressSpaceError::NotReserved),
        }
    }
    Ok(())
}

fn ensure_accessible(
    address_space: &Aarch64UserAddressSpace,
    range: VirtRange,
    flags: PageFlags,
) -> Result<(), AddressSpaceError> {
    for offset in (0..range.byte_len).step_by(PAGE) {
        let page = VirtRange::new(VirtAddr::new(range.start.raw() + offset), PAGE);
        match address_space.translate(page.start) {
            Translation::Committed { .. } => address_space.protect(page, flags)?,
            Translation::Reserved => address_space.commit(page, flags)?,
            Translation::Unmapped => return Err(AddressSpaceError::NotReserved),
        }
    }
    Ok(())
}

/// Runtime custom-virtual-memory hook table for the aarch64
/// backend. Address-space mutations route through the singleton
/// `Aarch64UserAddressSpace`; COW image creation is opted out of
/// (`default_memory_image_new` returns `NULL`), so the runtime falls
/// back to per-instance memcpy initialization for now.
pub static AARCH64_VMM_HOOKS: RuntimeMemoryHooks = RuntimeMemoryHooks {
    mmap_new: aarch64_mmap_new,
    mmap_remap: aarch64_mmap_remap,
    munmap: aarch64_munmap,
    mprotect: aarch64_mprotect,
    page_size: default_page_size,
    memory_image_new: default_memory_image_new,
    memory_image_free: default_memory_image_free,
    memory_image_map_at: default_memory_image_map_at,
};

const _: () = {
    // Pin the C ABI so a runtime ABI revision that changes the
    // `RuntimeMemoryImage` opaque ptr type fails the build instead
    // of mismatching at link time.
    let _: extern "C" fn(*const u8, usize, &mut *mut RuntimeMemoryImage) -> c_int =
        default_memory_image_new;
};

pub fn publish_code_memory(ptr: *const u8, len: usize) {
    protect_code_memory(ptr, len, PageFlags::READ | PageFlags::EXECUTE);
}

pub fn unpublish_code_memory(ptr: *const u8, len: usize) {
    protect_code_memory(ptr, len, PageFlags::READ | PageFlags::WRITE);
}

fn protect_code_memory(ptr: *const u8, len: usize, flags: PageFlags) {
    if len == 0 {
        return;
    }
    let start = ptr as usize;
    assert!(
        start.is_multiple_of(PAGE),
        "AArch64 code-memory range start {start:#x} is not page-aligned"
    );
    assert!(
        len.is_multiple_of(PAGE),
        "AArch64 code-memory range len {len:#x} is not page-aligned"
    );
    let range = VirtRange::new(VirtAddr::new(start), len);
    user_as()
        .protect(range, flags)
        .unwrap_or_else(|error| panic!("AArch64 code-memory protect failed: {error:?}"));
}
