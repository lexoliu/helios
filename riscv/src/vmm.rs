//! Sv39 paging for the RISC-V backend.
//!
//! The kernel boots in machine→supervisor mode via OpenSBI with
//! paging disabled (`satp = 0`); every address is a physical address.
//! This module brings up Sv39 with an identity map covering all
//! physical memory the kernel will ever touch (FDT RAM regions plus
//! the canonical MMIO window 0..0x4000_0000) so paging activation is
//! a no-op for kernel addresses, then carves a high user-VA window
//! 0x0000_0040_0000_0000..0x0000_0060_0000_0000 (128 GiB) for
//! [`RiscvUserAddressSpace`] dynamic mappings.
//!
//! # Why upfront identity-map then enable
//!
//! After `csrw satp` the next instruction fetch goes through the
//! MMU. If the page tables do not cover the kernel's instruction
//! pointer, stack, or heap, the hart traps immediately into a
//! recursive page fault and dies silently before any diagnostic can
//! reach the serial port. Identity-mapping every region the kernel
//! has actually touched (or will reach in normal operation) makes
//! activation visible only to subsequent user-VA mapping work, never
//! to existing kernel code paths.
//!
//! # Allocation model
//!
//! Identity mapping uses 1 GiB megapages at the L2 level — one PTE
//! per gibibyte of address space. The user-VA window allocates fresh
//! L1/L0 tables on demand for fine-grained mappings. Page tables
//! themselves are allocated through the kernel global allocator
//! (`alloc_zeroed`); each table is one 4 KiB physical frame.
//!
//! # SMP
//!
//! `enable_paging` runs on every hart that comes online. The bootstrap
//! hart sets up the root table, secondary harts share it via `satp`.
//! TLB shootdown for user-VA mutations dispatches `sbi_rt::
//! remote_sfence_vma` to the affected harts; for the kernel identity
//! map, no shootdown is ever needed because those mappings never
//! change after boot.

extern crate alloc;

use alloc::alloc::{Layout, alloc_zeroed};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ffi::c_int;
use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

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

const PAGE_SHIFT: u32 = 12;
const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
const PTE_COUNT: usize = 512;
const SV39_LEVEL_BITS: u32 = 9;
const SV39_VA_BITS: u32 = 39;
const SV39_PA_BITS: u32 = 56;

const PTE_VALID: u64 = 1 << 0;
const PTE_READ: u64 = 1 << 1;
const PTE_WRITE: u64 = 1 << 2;
const PTE_EXECUTE: u64 = 1 << 3;
const PTE_USER: u64 = 1 << 4;
const PTE_ACCESSED: u64 = 1 << 6;
const PTE_DIRTY: u64 = 1 << 7;
const PTE_PPN_SHIFT: u32 = 10;
const PTE_FLAGS_MASK: u64 = (1 << PTE_PPN_SHIFT) - 1;

const SATP_MODE_SV39: u64 = 8 << 60;

const GIB: usize = 1 << 30;
/// Cover MMIO region 0..1 GiB plus the typical RAM start at 0x8000_0000
/// out to 16 GiB. RISC-V virt platforms place RAM at 0x8000_0000 and
/// MMIO below; mapping the full first 16 GiB at L2 covers every real
/// platform helios targets.
const KERNEL_IDENTITY_GIB: usize = 16;

/// User-VA window. Starts well clear of the identity map so address
/// values are unambiguous when debugging fault traces.
const USER_VA_BASE: usize = 0x0000_0040_0000_0000;
const USER_VA_END: usize = 0x0000_0060_0000_0000;

#[repr(align(4096))]
struct PageTable(UnsafeCell<[u64; PTE_COUNT]>);

unsafe impl Sync for PageTable {}

impl PageTable {
    const fn new() -> Self {
        Self(UnsafeCell::new([0; PTE_COUNT]))
    }

    /// The table's entries as a raw array pointer.
    ///
    /// The root table is a `static` shared by every hart, so there is
    /// never a `&mut PageTable` to hand out; who may write it is decided
    /// by the paging bring-up order rather than by the borrow checker,
    /// and each caller states which side of that order it is on.
    fn entries_ptr(&self) -> *mut [u64; PTE_COUNT] {
        self.0.get()
    }

    fn phys_addr(&self) -> usize {
        self.entries_ptr().cast::<u64>() as usize
    }
}

/// Boot-time root page table. `static` so it is in BSS at a stable
/// physical address before paging is enabled. Only the bootstrap hart
/// initialises it; secondaries share it via the same `satp` value.
static ROOT_TABLE: PageTable = PageTable::new();

static PAGING_ROOT_PHYS: AtomicUsize = AtomicUsize::new(0);
static USER_AS: Once<RiscvUserAddressSpace> = Once::new();

/// Initialise the root table with the kernel identity map. The
/// caller is responsible for invoking `activate_paging` on each hart
/// that should run paged.
///
/// # Safety
/// Must be called exactly once, before any hart writes `satp`. The
/// returned `&'static RiscvUserAddressSpace` is the user-VA address
/// space owned by this kernel; it survives for the lifetime of the
/// process.
pub unsafe fn install_kernel_paging() -> &'static RiscvUserAddressSpace {
    // SAFETY: the caller guarantees this runs once, before any hart
    // writes `satp`, so the bootstrap hart is the only writer.
    let entries = unsafe { &mut *ROOT_TABLE.entries_ptr() };
    for entry in entries.iter_mut() {
        *entry = 0;
    }
    for (gib, entry) in entries.iter_mut().take(KERNEL_IDENTITY_GIB).enumerate() {
        let pa: u64 = (gib as u64) * (GIB as u64);
        let ppn = pa >> PAGE_SHIFT;
        *entry = (ppn << PTE_PPN_SHIFT)
            | PTE_VALID
            | PTE_READ
            | PTE_WRITE
            | PTE_EXECUTE
            | PTE_ACCESSED
            | PTE_DIRTY;
    }
    let root_phys = ROOT_TABLE.phys_addr();
    assert!(
        root_phys.is_multiple_of(PAGE_SIZE),
        "Sv39 root table is not page-aligned"
    );
    PAGING_ROOT_PHYS.store(root_phys, Ordering::Release);
    USER_AS.call_once(|| RiscvUserAddressSpace::new(root_phys))
}

/// Switch the calling hart into Sv39 paging using the root table
/// installed by [`install_kernel_paging`].
///
/// # Safety
/// `install_kernel_paging` must have completed on the bootstrap hart
/// before any hart calls this function. The caller must ensure no
/// instruction or data in flight relies on a physical address outside
/// the identity-mapped window.
pub unsafe fn activate_paging() {
    let root_phys = PAGING_ROOT_PHYS.load(Ordering::Acquire);
    assert!(
        root_phys != 0,
        "Sv39 root table not installed before activate_paging"
    );
    let satp = SATP_MODE_SV39 | (root_phys as u64 >> PAGE_SHIFT);
    unsafe {
        core::arch::asm!(
            "csrw satp, {satp}",
            "sfence.vma zero, zero",
            satp = in(reg) satp,
            options(nostack, preserves_flags),
        );
    }
}

/// Get the user address space registered at boot. Returns `None`
/// before [`install_kernel_paging`] has run.
pub fn user_address_space() -> Option<&'static RiscvUserAddressSpace> {
    USER_AS.get()
}

/// Sv39-backed user address space. Tracks reservations in a software
/// table; commit/decommit/protect mutate live page-table entries
/// hanging off the kernel root table at the L2 indices that fall
/// inside [`USER_VA_BASE`..`USER_VA_END`].
pub struct RiscvUserAddressSpace {
    root_phys: usize,
    va_cursor: VaCursor,
    state: Mutex<ReservationTracker>,
}

#[derive(Clone, Copy)]
struct RelocationPage {
    virt: usize,
    old_phys: usize,
    old_entry: u64,
    new_phys: usize,
}

impl RiscvUserAddressSpace {
    fn new(root_phys: usize) -> Self {
        Self {
            root_phys,
            va_cursor: VaCursor::new(USER_VA_BASE, USER_VA_END),
            state: Mutex::new(ReservationTracker::new()),
        }
    }

    fn carve_reservation(&self, byte_len: usize) -> Option<VirtRange> {
        self.state
            .lock()
            .reuse_free_range(byte_len)
            .or_else(|| self.va_cursor.carve(byte_len))
    }

    /// The root table this address space maps through.
    ///
    /// The table lives at a fixed physical address rather than inside
    /// the address space value, so it is reached by pointer; callers
    /// hold the reservation lock that serialises table edits.
    fn root_table(&self) -> *mut [u64; PTE_COUNT] {
        self.root_phys as *mut [u64; PTE_COUNT]
    }

    fn ensure_intermediate(table: &mut [u64; PTE_COUNT], index: usize) -> &mut [u64; PTE_COUNT] {
        let entry = &mut table[index];
        if *entry & PTE_VALID != 0 {
            assert!(
                *entry & (PTE_READ | PTE_WRITE | PTE_EXECUTE) == 0,
                "Sv39 user-VA index {index:#x} collides with a leaf mapping"
            );
            let phys = ((*entry >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
            return unsafe { &mut *(phys as *mut [u64; PTE_COUNT]) };
        }
        let layout = Layout::from_size_align(PAGE_SIZE, PAGE_SIZE)
            .expect("Sv39 page-table layout is well-formed");
        let raw = unsafe { alloc_zeroed(layout) };
        let raw = NonNull::new(raw).expect("Sv39 page-table allocation failed");
        let phys = raw.as_ptr() as usize;
        *entry = ((phys as u64) >> PAGE_SHIFT) << PTE_PPN_SHIFT | PTE_VALID;
        unsafe { &mut *(phys as *mut [u64; PTE_COUNT]) }
    }

    fn map_4k(&self, virt: usize, phys: usize, flags: u64) -> Result<(), AddressSpaceError> {
        // SAFETY: the caller holds this address space's reservation
        // lock, so no other hart is editing its tables.
        let l2 = unsafe { &mut *self.root_table() };
        let l2_index = (virt >> 30) & 0x1ff;
        let l1 = Self::ensure_intermediate(l2, l2_index);
        let l1_index = (virt >> 21) & 0x1ff;
        let l0 = Self::ensure_intermediate(l1, l1_index);
        let l0_index = (virt >> 12) & 0x1ff;
        let entry = &mut l0[l0_index];
        if *entry & PTE_VALID != 0 {
            return Err(AddressSpaceError::Overlap);
        }
        let ppn = (phys as u64) >> PAGE_SHIFT;
        *entry = (ppn << PTE_PPN_SHIFT) | flags | PTE_VALID | PTE_USER | PTE_ACCESSED | PTE_DIRTY;
        sfence_vma_addr(virt);
        Ok(())
    }

    fn unmap_4k(&self, virt: usize) -> Result<u64, AddressSpaceError> {
        // SAFETY: the caller holds this address space's reservation
        // lock, so no other hart is editing its tables.
        let l2 = unsafe { &mut *self.root_table() };
        let l2_index = (virt >> 30) & 0x1ff;
        if l2[l2_index] & PTE_VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l1_phys = ((l2[l2_index] >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
        let l1 = unsafe { &mut *(l1_phys as *mut [u64; PTE_COUNT]) };
        let l1_index = (virt >> 21) & 0x1ff;
        if l1[l1_index] & PTE_VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l0_phys = ((l1[l1_index] >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
        let l0 = unsafe { &mut *(l0_phys as *mut [u64; PTE_COUNT]) };
        let l0_index = (virt >> 12) & 0x1ff;
        let prev = l0[l0_index];
        if prev & PTE_VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        l0[l0_index] = 0;
        sfence_vma_addr(virt);
        Ok(prev)
    }

    fn protect_4k(&self, virt: usize, flags: u64) -> Result<u64, AddressSpaceError> {
        // SAFETY: the caller holds this address space's reservation
        // lock, so no other hart is editing its tables.
        let l2 = unsafe { &mut *self.root_table() };
        let l2_index = (virt >> 30) & 0x1ff;
        if l2[l2_index] & PTE_VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l1_phys = ((l2[l2_index] >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
        let l1 = unsafe { &mut *(l1_phys as *mut [u64; PTE_COUNT]) };
        let l1_index = (virt >> 21) & 0x1ff;
        if l1[l1_index] & PTE_VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l0_phys = ((l1[l1_index] >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
        let l0 = unsafe { &mut *(l0_phys as *mut [u64; PTE_COUNT]) };
        let l0_index = (virt >> 12) & 0x1ff;
        let entry = &mut l0[l0_index];
        if *entry & PTE_VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        let old_entry = *entry;
        let ppn = *entry >> PTE_PPN_SHIFT;
        *entry = (ppn << PTE_PPN_SHIFT) | flags | PTE_VALID | PTE_USER | PTE_ACCESSED | PTE_DIRTY;
        sfence_vma_addr(virt);
        Ok(old_entry)
    }

    fn translate_4k(&self, virt: usize) -> Option<(usize, u64)> {
        // SAFETY: the caller holds this address space's reservation
        // lock, so no other hart is editing its tables.
        let l2 = unsafe { &mut *self.root_table() };
        let l2_index = (virt >> 30) & 0x1ff;
        if l2[l2_index] & PTE_VALID == 0 {
            return None;
        }
        let l1_phys = ((l2[l2_index] >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
        let l1 = unsafe { &*(l1_phys as *const [u64; PTE_COUNT]) };
        let l1_index = (virt >> 21) & 0x1ff;
        if l1[l1_index] & PTE_VALID == 0 {
            return None;
        }
        let l0_phys = ((l1[l1_index] >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
        let l0 = unsafe { &*(l0_phys as *const [u64; PTE_COUNT]) };
        let l0_index = (virt >> 12) & 0x1ff;
        let entry = l0[l0_index];
        if entry & PTE_VALID == 0 {
            return None;
        }
        if entry & (PTE_READ | PTE_WRITE | PTE_EXECUTE) == 0 {
            // Pointer to next table (shouldn't happen at L0).
            return None;
        }
        let ppn = entry >> PTE_PPN_SHIFT;
        Some(((ppn as usize) << PAGE_SHIFT, entry))
    }

    fn replace_4k(&self, virt: usize, new_entry: u64) -> Result<u64, AddressSpaceError> {
        // SAFETY: the caller holds this address space's reservation
        // lock, so no other hart is editing its tables.
        let l2 = unsafe { &mut *self.root_table() };
        let l2_index = (virt >> 30) & 0x1ff;
        if l2[l2_index] & PTE_VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l1_phys = ((l2[l2_index] >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
        let l1 = unsafe { &mut *(l1_phys as *mut [u64; PTE_COUNT]) };
        let l1_index = (virt >> 21) & 0x1ff;
        if l1[l1_index] & PTE_VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        let l0_phys = ((l1[l1_index] >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
        let l0 = unsafe { &mut *(l0_phys as *mut [u64; PTE_COUNT]) };
        let l0_index = (virt >> 12) & 0x1ff;
        let old_entry = l0[l0_index];
        if old_entry & PTE_VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        l0[l0_index] = new_entry;
        sfence_vma_addr(virt);
        Ok(old_entry)
    }

    fn alloc_user_frame(&self) -> Result<usize, AddressSpaceError> {
        let raw = allocate_user_frame_zeroed_on(crate::current_hart_id())
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        Ok(raw.as_ptr() as usize)
    }

    fn dealloc_user_phys(&self, phys: usize) {
        let ptr = NonNull::new(phys as *mut u8)
            .unwrap_or_else(|| panic!("RISC-V user-frame dealloc received null pointer"));
        deallocate_user_frame_on(crate::current_hart_id(), ptr);
    }

    fn build_relocation_plan(
        &self,
        virt: VirtRange,
    ) -> Result<Vec<RelocationPage>, AddressSpaceError> {
        let mut pages: Vec<RelocationPage> = Vec::new();
        for offset in (0..virt.byte_len).step_by(PAGE_SIZE) {
            let virt_addr = virt.start.raw() + offset;
            let (old_phys, old_entry) = match self.translate_4k(virt_addr) {
                Some(translation) => translation,
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
                ptr::copy_nonoverlapping(old_phys as *const u8, new_phys as *mut u8, PAGE_SIZE);
            }
            pages.push(RelocationPage {
                virt: virt_addr,
                old_phys,
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
                        "RISC-V AddressSpace::relocate rollback failed at {:#x}: {error}",
                        page.virt
                    )
                });
        }
        for page in pages {
            self.dealloc_user_phys(page.new_phys);
        }
    }

    fn rollback_partial_commit(&self, start: usize, mapped_pages: usize) {
        for page in 0..mapped_pages {
            let virt = start + page * PAGE_SIZE;
            let entry = self.unmap_4k(virt).unwrap_or_else(|error| {
                panic!("RISC-V AddressSpace::commit rollback failed at {virt:#x}: {error}")
            });
            let phys = ((entry >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
            self.dealloc_user_phys(phys);
        }
    }

    fn rollback_partial_protect(&self, start: usize, old_entries: &[u64]) {
        for (page, entry) in old_entries.iter().copied().enumerate().rev() {
            let virt = start + page * PAGE_SIZE;
            self.replace_4k(virt, entry).unwrap_or_else(|error| {
                panic!("RISC-V AddressSpace::protect rollback failed at {virt:#x}: {error}")
            });
        }
    }
}

impl AddressSpace for RiscvUserAddressSpace {
    fn reserve(&self, byte_len: usize) -> Result<VirtRange, AddressSpaceError> {
        if byte_len == 0 {
            return Err(AddressSpaceError::EmptyRange);
        }
        if !byte_len.is_multiple_of(PAGE_SIZE) {
            return Err(AddressSpaceError::Misaligned);
        }
        let range = self
            .carve_reservation(byte_len)
            .ok_or(AddressSpaceError::OutOfFrames)?;
        self.state.lock().reserve(range);
        Ok(range)
    }

    fn release(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        let released = self.state.lock().release(virt)?;
        // This backend commits eagerly and has no swap, so a released
        // reservation never holds a swap token; the assertion keeps that
        // true if swap ever reaches this architecture (#59).
        debug_assert!(released.swapped.is_empty());
        for region in &released.committed {
            for offset in (0..region.range.byte_len).step_by(PAGE_SIZE) {
                if let Ok(entry) = self.unmap_4k(region.range.start.raw() + offset) {
                    let phys = ((entry >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
                    self.dealloc_user_phys(phys);
                }
            }
        }
        self.state.lock().push_free_range(virt);
        Ok(())
    }

    fn commit(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let pte_flags = page_flags_to_pte(flags)?;

        self.state.lock().precheck_commit(virt)?;

        for offset in (0..virt.byte_len).step_by(PAGE_SIZE) {
            let virt_addr = virt.start.raw() + offset;
            let mapped_pages = offset / PAGE_SIZE;
            let phys = match self.alloc_user_frame() {
                Ok(phys) => phys,
                Err(error) => {
                    self.rollback_partial_commit(virt.start.raw(), mapped_pages);
                    return Err(error);
                }
            };
            if let Err(error) = self.map_4k(virt_addr, phys, pte_flags) {
                self.dealloc_user_phys(phys);
                self.rollback_partial_commit(virt.start.raw(), mapped_pages);
                return Err(error);
            }
        }

        self.state
            .lock()
            .record_commit(virt, flags, MemoryOwner::NONE)?;
        Ok(())
    }

    fn decommit(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let _ = self.state.lock().record_decommit(virt)?;
        for offset in (0..virt.byte_len).step_by(PAGE_SIZE) {
            let entry = self.unmap_4k(virt.start.raw() + offset)?;
            let phys = ((entry >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
            self.dealloc_user_phys(phys);
        }
        Ok(())
    }

    fn protect(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let pte_flags = page_flags_to_pte(flags)?;
        self.state.lock().ensure_committed(virt)?;
        let mut old_entries = Vec::new();
        for offset in (0..virt.byte_len).step_by(PAGE_SIZE) {
            match self.protect_4k(virt.start.raw() + offset, pte_flags) {
                Ok(entry) => old_entries.push(entry),
                Err(error) => {
                    self.rollback_partial_protect(virt.start.raw(), &old_entries);
                    return Err(error);
                }
            }
        }
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
        let virt = addr.raw() & !(PAGE_SIZE - 1);
        match self.translate_4k(virt) {
            Some((phys, _)) => Translation::Committed {
                frame: PhysFrame::from_phys_addr(phys),
                flags: committed.flags,
            },
            None => Translation::Reserved,
        }
    }

    fn relocate(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        self.state.lock().ensure_committed(virt)?;

        let pages = self.build_relocation_plan(virt)?;
        for (index, page) in pages.iter().enumerate() {
            let new_entry = (((page.new_phys as u64) >> PAGE_SHIFT) << PTE_PPN_SHIFT)
                | (page.old_entry & PTE_FLAGS_MASK);
            if let Err(error) = self.replace_4k(page.virt, new_entry) {
                self.rollback_relocation(&pages, index);
                return Err(error);
            }
        }
        for page in &pages {
            self.dealloc_user_phys(page.old_phys);
        }
        Ok(())
    }
}

fn sfence_vma_addr(virt: usize) {
    unsafe {
        core::arch::asm!(
            "sfence.vma {addr}, zero",
            addr = in(reg) virt,
            options(nostack, preserves_flags),
        );
    }
    let ret = sbi_rt::remote_sfence_vma(
        sbi_rt::HartMask::from_mask_base(0, usize::MAX),
        virt,
        PAGE_SIZE,
    );
    assert!(
        ret.is_ok(),
        "SBI remote_sfence_vma failed for user VA {virt:#x}: error={} value={}",
        ret.error,
        ret.value
    );
}

fn page_flags_to_pte(flags: PageFlags) -> Result<u64, AddressSpaceError> {
    if flags.is_empty() {
        return Err(AddressSpaceError::InvalidFlags);
    }
    let mut pte = 0u64;
    if flags.contains(PageFlags::READ) {
        pte |= PTE_READ;
    }
    if flags.contains(PageFlags::WRITE) {
        pte |= PTE_WRITE;
    }
    if flags.contains(PageFlags::EXECUTE) {
        pte |= PTE_EXECUTE;
    }
    Ok(pte)
}

const _: () = {
    assert!(SV39_VA_BITS == 39);
    assert!(SV39_PA_BITS == 56);
    assert!(SV39_LEVEL_BITS == 9);
    assert!(KERNEL_IDENTITY_GIB <= 512);
    assert!(USER_VA_BASE.is_multiple_of(GIB));
    assert!(USER_VA_END > USER_VA_BASE);
};

/// Register the boot-time `RiscvUserAddressSpace` as the active
/// runtime custom-virtual-memory backend. Must be called once on
/// the bootstrap hart, after `install_kernel_paging`, before any
/// runtime engine is constructed.
pub fn install_runtime_memory_hooks() {
    runtime_memory::install_hooks(&RISCV_VMM_HOOKS);
}

fn user_as() -> &'static RiscvUserAddressSpace {
    user_address_space().expect("RiscvUserAddressSpace accessed before install_kernel_paging")
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
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

extern "C" fn riscv_mmap_new(size: usize, prot_flags: u32, ret: &mut *mut u8) -> c_int {
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

extern "C" fn riscv_mmap_remap(addr: *mut u8, size: usize, prot_flags: u32) -> c_int {
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

extern "C" fn riscv_munmap(ptr: *mut u8, size: usize) -> c_int {
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = VirtRange::new(VirtAddr::new(ptr as usize), size);
    match address_space.release(range) {
        Ok(()) => 0,
        Err(_) => EINVAL,
    }
}

extern "C" fn riscv_mprotect(ptr: *mut u8, size: usize, prot_flags: u32) -> c_int {
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

/// Runtime custom-virtual-memory hook table for the riscv backend.
pub static RISCV_VMM_HOOKS: RuntimeMemoryHooks = RuntimeMemoryHooks {
    mmap_new: riscv_mmap_new,
    mmap_remap: riscv_mmap_remap,
    munmap: riscv_munmap,
    mprotect: riscv_mprotect,
    page_size: default_page_size,
    memory_image_new: default_memory_image_new,
    memory_image_free: default_memory_image_free,
    memory_image_map_at: default_memory_image_map_at,
};
