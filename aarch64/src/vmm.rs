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
//!
//! Concurrency contract: every page-table mutation (leaf writes,
//! intermediate-table allocation in `ensure_table`, unmap, protect,
//! relocation) runs while holding the `state` mutex, so two
//! processors can never race an invalid table slot and orphan each
//! other's freshly mapped leaves. Reads (`translate_4k`) stay
//! lock-free: table frames are never freed while the address space
//! lives, and 64-bit descriptor reads are single-copy atomic.

extern crate alloc;

use alloc::alloc::{Layout, alloc_zeroed};
use alloc::vec::Vec;
use core::arch::asm;
use core::ffi::c_int;
use core::ptr::{self, NonNull};

use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{
    AddressSpace, AddressSpaceError, PageAge, PageFlags, SwapToken, Translation, VirtAddr,
    VirtRange,
};
use helios_kernel::runtime_memory::{
    self, RuntimeMemoryHooks, RuntimeMemoryImage, default_memory_image_free,
    default_memory_image_map_at, default_memory_image_new, default_page_size,
};
use helios_kernel::{
    MemoryOwner, ReservationLookup, ReservationTracker, SwapEntry, SwapVmHooks, VaCursor,
    allocate_user_frame_uninit_on, current_user_memory_owner, deallocate_user_frame_on,
    validate_range,
};
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

/// Layout of the descriptor that replaces a swapped-out page.
///
/// Bit 0 clear makes the descriptor invalid, so the MMU ignores every
/// other bit and the whole 64 bits are ours. Bit 1 marks the entry as
/// ours specifically, which keeps an all-zero descriptor meaning
/// "nothing was ever mapped here"; the token sits above it. The page
/// table is therefore the swap map, and the fault path reads it without
/// taking any lock.
const SWAP_MARKER: u64 = 1 << 1;
const SWAP_TOKEN_SHIFT: u32 = 2;

const fn swap_descriptor(token: SwapToken) -> u64 {
    SWAP_MARKER | ((token.raw() as u64) << SWAP_TOKEN_SHIFT)
}

fn swap_descriptor_token(entry: u64) -> Option<SwapToken> {
    if entry & VALID != 0 || entry & SWAP_MARKER == 0 {
        return None;
    }
    SwapToken::new((entry >> SWAP_TOKEN_SHIFT) as u32)
}

/// Pages whose access flag one scan clears before it re-arms the TLB.
const AF_SCAN_BATCH_PAGES: usize = 128;

const USER_VA_BASE: usize = 0xFFFF_C000_0000_0000;
const USER_VA_END: usize = 0xFFFF_E000_0000_0000;

/// Per-platform aarch64 user address space.
pub struct Aarch64UserAddressSpace {
    physical_memory_offset: usize,
    va_cursor: VaCursor,
    state: Mutex<ReservationTracker>,
    /// Swap tokens whose pages went away underneath them, waiting for
    /// the kernel's swap task to hand them back to the backend.
    ///
    /// They cannot be released here: this runs under `state`'s spinlock
    /// and releasing an extent is asynchronous. Lock order is `state`
    /// then `orphans`, and the drain takes only `orphans`.
    orphans: Mutex<Vec<SwapToken>>,
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
            va_cursor: VaCursor::new(USER_VA_BASE, USER_VA_END),
            state: Mutex::new(ReservationTracker::new()),
            orphans: Mutex::new(Vec::new()),
        }
    }

    fn orphan(&self, entries: Vec<SwapEntry>) {
        if entries.is_empty() {
            return;
        }
        let mut orphans = self.orphans.lock();
        orphans.extend(entries.into_iter().map(|entry| entry.token));
    }

    /// Reads the level-3 descriptor for `virt` whether or not it is
    /// valid, which is what tells a swap entry apart from an unmapped
    /// page. Lock-free: table frames are never freed while the address
    /// space lives and a 64-bit descriptor read is single-copy atomic.
    fn leaf_entry(&self, virt: usize) -> Option<u64> {
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
        Some(unsafe { l3.add((virt >> 12) & 0x1ff).read_volatile() })
    }

    fn leaf_ptr(&self, virt: usize) -> Option<*mut u64> {
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
        Some(unsafe { l3.add((virt >> 12) & 0x1ff) })
    }

    /// Turns a swap descriptor back into a plain unmapped entry.
    fn clear_swap_descriptor(&self, virt: usize) {
        let Some(entry_ptr) = self.leaf_ptr(virt) else {
            return;
        };
        let entry = unsafe { entry_ptr.read_volatile() };
        if swap_descriptor_token(entry).is_none() {
            return;
        }
        unsafe {
            entry_ptr.write_volatile(0);
            flush_tlb_pages(virt, 1);
        }
    }

    /// Swaps one committed page out: copy it, file the entry, hand the
    /// frame back.
    ///
    /// The copy happens with the page still mapped and the address
    /// space's lock held, so nothing can write it between the copy and
    /// the unmapping, and the broadcast TLB invalidation inside
    /// `replace_4k` retires every other processor's translation before
    /// the frame is recycled.
    fn swap_out_page_locked(
        &self,
        addr: VirtAddr,
        token: SwapToken,
        out: &mut [u8],
    ) -> Result<PageFlags, AddressSpaceError> {
        if out.len() != PAGE {
            return Err(AddressSpaceError::BadPageBuffer);
        }
        if !addr.is_page_aligned() {
            return Err(AddressSpaceError::Misaligned);
        }
        let mut state = self.state.lock();
        let virt = addr.raw();
        let entry = self
            .translate_4k(virt)
            .ok_or(AddressSpaceError::NotCommitted)?;
        let flags = state.record_swap_out(addr, token)?;
        let phys = (entry & PT_ADDR_MASK) as usize;
        unsafe {
            ptr::copy_nonoverlapping(self.hhdm_ptr(phys) as *const u8, out.as_mut_ptr(), PAGE);
        }
        match self.replace_4k(virt, swap_descriptor(token)) {
            Ok(_) => {
                self.dealloc_user_phys(phys);
                Ok(flags)
            }
            Err(error) => {
                // Nothing moved; put the bookkeeping back the way it was.
                let _ = state.take_swap_entry(addr);
                Err(error)
            }
        }
    }

    /// Puts a swapped-out page back: fill a fresh frame first, publish
    /// the mapping second, so no processor sees a half-restored page.
    fn swap_in_page_locked(
        &self,
        addr: VirtAddr,
        bytes: &[u8],
    ) -> Result<SwapToken, AddressSpaceError> {
        if bytes.len() != PAGE {
            return Err(AddressSpaceError::BadPageBuffer);
        }
        if !addr.is_page_aligned() {
            return Err(AddressSpaceError::Misaligned);
        }
        let mut state = self.state.lock();
        let entry: SwapEntry = state.take_swap_entry(addr)?;
        let pte_flags = match page_flags_to_pte(entry.flags) {
            Ok(pte_flags) => pte_flags,
            Err(error) => {
                let _ = state.record_swap_out(addr, entry.token);
                return Err(error);
            }
        };
        let phys = match self.alloc_user_frame() {
            Ok(phys) => phys,
            Err(error) => {
                let _ = state.record_swap_out(addr, entry.token);
                return Err(error);
            }
        };
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), self.hhdm_ptr(phys), PAGE);
        }
        let descriptor = (phys as u64) | pte_flags | PAGE_DESCRIPTOR | AF | SH_INNER;
        let virt = addr.raw();
        let Some(entry_ptr) = self.leaf_ptr(virt) else {
            self.dealloc_user_phys(phys);
            let _ = state.record_swap_out(addr, entry.token);
            return Err(AddressSpaceError::NotSwapped);
        };
        unsafe {
            entry_ptr.write_volatile(descriptor);
            flush_tlb_pages(virt, 1);
        }
        Ok(entry.token)
    }

    /// Reports how recently the hardware saw each of `owner`'s pages and
    /// clears the access flag behind the scan.
    ///
    /// This machine may not update the access flag in hardware, so
    /// clearing it arms an access-flag fault instead: the next touch
    /// traps, [`Self::set_access_flag`] sets the bit and the instruction
    /// is retried. That is what makes a page read as `Hot` on the next
    /// pass, and it costs one cheap trap per page the aging touched.
    fn scan_committed_pages_locked<Visit>(&self, owner: MemoryOwner, mut visit: Visit) -> usize
    where
        Visit: FnMut(VirtAddr, PageFlags, PageAge) -> bool,
    {
        let state = self.state.lock();
        let mut visited = 0_usize;
        let mut keep_going = true;
        state.owned_committed_regions(owner, |region| {
            let mut cleared_from = region.range.start.raw();
            let mut cleared = 0_usize;
            for offset in (0..region.range.byte_len).step_by(PAGE) {
                let virt = region.range.start.raw() + offset;
                let Some(entry_ptr) = self.leaf_ptr(virt) else {
                    continue;
                };
                let entry = unsafe { entry_ptr.read_volatile() };
                if entry & VALID == 0 {
                    continue;
                }
                let age = if entry & AF != 0 {
                    if cleared == 0 {
                        cleared_from = virt;
                    }
                    unsafe {
                        entry_ptr.write_volatile(entry & !AF);
                    }
                    cleared += 1;
                    PageAge::Hot
                } else {
                    PageAge::Cold
                };
                visited += 1;
                if cleared == AF_SCAN_BATCH_PAGES {
                    unsafe {
                        flush_tlb_pages(cleared_from, cleared);
                    }
                    cleared = 0;
                }
                if !visit(VirtAddr::new(virt), region.flags, age) {
                    keep_going = false;
                    break;
                }
            }
            if cleared != 0 {
                unsafe {
                    flush_tlb_pages(cleared_from, cleared);
                }
            }
            keep_going
        });
        visited
    }

    /// Resolves an access-flag fault by setting the bit the aging pass
    /// cleared. Returns `false` when the address is not a committed page
    /// with a clear access flag, in which case the fault belongs to
    /// someone else.
    pub fn set_access_flag(&self, addr: VirtAddr) -> bool {
        let virt = addr.page_floor().raw();
        let Some(entry_ptr) = self.leaf_ptr(virt) else {
            return false;
        };
        let entry = unsafe { entry_ptr.read_volatile() };
        if entry & VALID == 0 || entry & AF != 0 {
            return false;
        }
        unsafe {
            entry_ptr.write_volatile(entry | AF);
            flush_tlb_pages(virt, 1);
        }
        true
    }
    fn carve_reservation(&self, byte_len: usize) -> Option<VirtRange> {
        self.state
            .lock()
            .reuse_free_range(byte_len)
            .or_else(|| self.va_cursor.carve(byte_len))
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

    fn protect_4k_no_flush(&self, virt: usize, flags: u64) -> Result<u64, AddressSpaceError> {
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
        Ok(entry)
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
        let raw = allocate_user_frame_uninit_on(crate::current_processor_runtime().logical_id())
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        // SAFETY: `raw` came from the user-memory pool above which
        // produced a `PhysFrame::SIZE` aligned, exclusively owned
        // writable region. The aarch64 backend's `dc zva` helper
        // zeros the whole frame at cache-line granularity.
        unsafe {
            crate::aarch64_zero_memory(raw.as_ptr(), helios_hal::pmm::PhysFrame::SIZE);
        }
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

    fn rollback_partial_commit(&self, start: usize, mapped_pages: usize) {
        let mut batch_entries = [0u64; TLB_DECOMMIT_BATCH_PAGES];
        let mut batch_count = 0;
        let mut batch_start = start;
        for page in 0..mapped_pages {
            let virt = start + page * PAGE;
            if batch_count == 0 {
                batch_start = virt;
            }
            batch_entries[batch_count] = self.unmap_4k_no_flush(virt).unwrap_or_else(|error| {
                panic!("Aarch64 AddressSpace::commit rollback failed at {virt:#x}: {error}")
            });
            batch_count += 1;
            if batch_count == TLB_DECOMMIT_BATCH_PAGES {
                self.flush_and_dealloc_entries(batch_start, &batch_entries[..batch_count]);
                batch_count = 0;
            }
        }
        if batch_count != 0 {
            self.flush_and_dealloc_entries(batch_start, &batch_entries[..batch_count]);
        }
    }

    fn rollback_partial_protect(&self, start: usize, old_entries: &[u64]) {
        for (page, entry) in old_entries.iter().copied().enumerate().rev() {
            let virt = start + page * PAGE;
            self.replace_4k(virt, entry).unwrap_or_else(|error| {
                panic!("Aarch64 AddressSpace::protect rollback failed at {virt:#x}: {error}")
            });
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

    // Release AArch64/HVF `quickjs-loop` evidence: once the custom VM stopped
    // remapping full static reservations, the remaining instantiate/drop cost
    // was dominated by these hooks calling decommit/protect/commit one page at
    // a time. Range coalescing moved the dirty median from 44 ms to 22 ms and
    // cut Store teardown from 52.0 ms to 3.0 ms over five runs.
    fn decommit_committed_subranges(&self, range: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(range)?;
        let mut state = self.state.lock();
        let swapped = state.take_swap_entries_in(range);
        for entry in &swapped {
            self.clear_swap_descriptor(entry.addr.raw());
        }
        self.orphan(swapped);
        let decommit_ranges = state.take_committed_intersections(range)?;
        for subrange in decommit_ranges {
            self.decommit_mapped_range(subrange)?;
        }
        Ok(())
    }

    fn ensure_accessible_subranges(
        &self,
        range: VirtRange,
        flags: PageFlags,
    ) -> Result<(), AddressSpaceError> {
        validate_range(range)?;
        let mut state = self.state.lock();
        let plan = state.accessibility_plan(range)?;
        for subrange in plan.protect {
            self.protect_locked(&mut state, subrange, flags)?;
        }
        for subrange in plan.commit {
            self.commit_locked(&mut state, subrange, flags)?;
        }
        Ok(())
    }

    fn commit_locked(
        &self,
        state: &mut ReservationTracker,
        virt: VirtRange,
        flags: PageFlags,
    ) -> Result<(), AddressSpaceError> {
        let pte_flags = page_flags_to_pte(flags)?;
        state.precheck_commit(virt)?;
        // Committing over a swapped-out page throws its contents away:
        // the runtime asked for fresh anonymous memory here. Clearing
        // the descriptor is what `map_4k_no_flush` needs to see, and the
        // extent behind the token still has to be given back.
        self.orphan(state.take_swap_entries_in(virt));
        for offset in (0..virt.byte_len).step_by(PAGE) {
            self.clear_swap_descriptor(virt.start.raw() + offset);
        }

        let mut mapped_pages = 0;
        for offset in (0..virt.byte_len).step_by(PAGE) {
            let virt_addr = virt.start.raw() + offset;
            let phys = match self.alloc_user_frame() {
                Ok(phys) => phys,
                Err(error) => {
                    self.rollback_partial_commit(virt.start.raw(), mapped_pages);
                    return Err(error);
                }
            };
            if let Err(error) = self.map_4k_no_flush(virt_addr, phys, pte_flags) {
                self.dealloc_user_phys(phys);
                self.rollback_partial_commit(virt.start.raw(), mapped_pages);
                return Err(error);
            }
            mapped_pages += 1;
        }
        if mapped_pages != 0 {
            unsafe {
                flush_tlb_pages(virt.start.raw(), mapped_pages);
            }
        }

        let owner = current_user_memory_owner(crate::current_processor_runtime().logical_id());
        self.orphan(state.record_commit(virt, flags, owner)?);
        Ok(())
    }

    fn protect_locked(
        &self,
        state: &mut ReservationTracker,
        virt: VirtRange,
        flags: PageFlags,
    ) -> Result<(), AddressSpaceError> {
        let pte_flags = page_flags_to_pte(flags)?;
        state.ensure_committed(virt)?;
        let mut protected_pages = 0;
        let mut old_entries = Vec::new();
        for offset in (0..virt.byte_len).step_by(PAGE) {
            match self.protect_4k_no_flush(virt.start.raw() + offset, pte_flags) {
                Ok(entry) => old_entries.push(entry),
                Err(error) => {
                    self.rollback_partial_protect(virt.start.raw(), &old_entries);
                    if protected_pages != 0 {
                        unsafe {
                            flush_tlb_pages(virt.start.raw(), protected_pages);
                        }
                    }
                    return Err(error);
                }
            }
            protected_pages += 1;
        }
        if protected_pages != 0 {
            unsafe {
                flush_tlb_pages(virt.start.raw(), protected_pages);
            }
        }
        state.record_protect(virt, flags)?;
        Ok(())
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
        self.state.lock().reserve(range);
        Ok(range)
    }

    fn release(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        let mut state = self.state.lock();
        let released = state.release(virt)?;
        for region in &released.committed {
            for offset in (0..region.range.byte_len).step_by(PAGE) {
                if let Ok(entry) = self.unmap_4k(region.range.start.raw() + offset) {
                    self.dealloc_user_frame(entry);
                }
            }
        }
        for entry in &released.swapped {
            self.clear_swap_descriptor(entry.addr.raw());
        }
        self.orphan(released.swapped);
        state.push_free_range(virt);
        Ok(())
    }

    fn commit(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let mut state = self.state.lock();
        self.commit_locked(&mut state, virt, flags)
    }

    fn decommit(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let mut state = self.state.lock();
        let swapped = state.record_decommit(virt)?;
        for entry in &swapped {
            self.clear_swap_descriptor(entry.addr.raw());
        }
        self.orphan(swapped);
        self.decommit_mapped_range(virt)?;
        Ok(())
    }

    fn protect(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let mut state = self.state.lock();
        self.protect_locked(&mut state, virt, flags)
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

    fn swap_out_page(
        &self,
        addr: VirtAddr,
        token: SwapToken,
        out: &mut [u8],
    ) -> Result<PageFlags, AddressSpaceError> {
        self.swap_out_page_locked(addr, token, out)
    }

    fn swap_in_page(&self, addr: VirtAddr, bytes: &[u8]) -> Result<SwapToken, AddressSpaceError> {
        self.swap_in_page_locked(addr, bytes)
    }

    fn swapped_token(&self, addr: VirtAddr) -> Option<SwapToken> {
        swap_descriptor_token(self.leaf_entry(addr.page_floor().raw())?)
    }

    fn scan_committed_pages<Visit>(&self, owner: u64, visit: Visit) -> usize
    where
        Visit: FnMut(VirtAddr, PageFlags, PageAge) -> bool,
    {
        self.scan_committed_pages_locked(MemoryOwner::new(owner), visit)
    }

    fn owned_resident_bytes(&self, owner: u64) -> u64 {
        self.state
            .lock()
            .owned_resident_bytes(MemoryOwner::new(owner))
    }

    fn drain_orphaned_swap_tokens<Visit>(&self, mut visit: Visit) -> usize
    where
        Visit: FnMut(SwapToken),
    {
        let drained: Vec<SwapToken> = core::mem::take(&mut *self.orphans.lock());
        let count = drained.len();
        for token in drained {
            visit(token);
        }
        count
    }

    fn relocate(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let state = self.state.lock();
        state.ensure_committed(virt)?;

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
    helios_kernel::install_swap_hooks(&AARCH64_SWAP_HOOKS);
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
    address_space.decommit_committed_subranges(range)
}

fn ensure_accessible(
    address_space: &Aarch64UserAddressSpace,
    range: VirtRange,
    flags: PageFlags,
) -> Result<(), AddressSpaceError> {
    address_space.ensure_accessible_subranges(range, flags)
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

fn hook_swap_out_page(
    addr: VirtAddr,
    token: SwapToken,
    out: &mut [u8],
) -> Result<PageFlags, AddressSpaceError> {
    user_as().swap_out_page(addr, token, out)
}

fn hook_swap_in_page(addr: VirtAddr, bytes: &[u8]) -> Result<SwapToken, AddressSpaceError> {
    user_as().swap_in_page(addr, bytes)
}

fn hook_swapped_token(addr: VirtAddr) -> Option<SwapToken> {
    user_as().swapped_token(addr)
}

fn hook_scan_committed_pages(
    owner: u64,
    context: *mut (),
    visit: fn(*mut (), VirtAddr, PageFlags, PageAge) -> bool,
) -> usize {
    user_as().scan_committed_pages(owner, |addr, flags, age| visit(context, addr, flags, age))
}

fn hook_owned_resident_bytes(owner: u64) -> u64 {
    user_as().owned_resident_bytes(owner)
}

fn hook_drain_orphaned_swap_tokens(context: *mut (), visit: fn(*mut (), SwapToken)) -> usize {
    user_as().drain_orphaned_swap_tokens(|token| visit(context, token))
}

/// The kernel's view of this backend's swap surface.
pub static AARCH64_SWAP_HOOKS: SwapVmHooks = SwapVmHooks {
    swap_out_page: hook_swap_out_page,
    swap_in_page: hook_swap_in_page,
    swapped_token: hook_swapped_token,
    scan_committed_pages: hook_scan_committed_pages,
    owned_resident_bytes: hook_owned_resident_bytes,
    drain_orphaned_swap_tokens: hook_drain_orphaned_swap_tokens,
};

/// Resolves an access-flag fault raised by the swap policy's aging pass.
pub fn resolve_access_flag_fault(addr: usize) -> bool {
    let Some(address_space) = USER_AS.get() else {
        return false;
    };
    address_space.set_access_flag(VirtAddr::new(addr))
}

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
