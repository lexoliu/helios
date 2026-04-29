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
use core::ops::Range;
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
#[allow(dead_code)]
const PTE_GLOBAL: u64 = 1 << 5;
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

    fn entries_mut(&self) -> &mut [u64; PTE_COUNT] {
        unsafe { &mut *self.0.get() }
    }

    fn phys_addr(&self) -> usize {
        self.entries_mut().as_ptr() as usize
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
    let entries = ROOT_TABLE.entries_mut();
    for entry in entries.iter_mut() {
        *entry = 0;
    }
    for gib in 0..KERNEL_IDENTITY_GIB {
        let pa: u64 = (gib as u64) * (GIB as u64);
        let ppn = pa >> PAGE_SHIFT;
        entries[gib] = (ppn << PTE_PPN_SHIFT)
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
    old_phys: usize,
    old_entry: u64,
    new_phys: usize,
}

impl RiscvUserAddressSpace {
    fn new(root_phys: usize) -> Self {
        Self {
            root_phys,
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
            match self.next_va.compare_exchange(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(VirtRange::new(VirtAddr::new(current), byte_len)),
                Err(observed) => current = observed,
            }
        }
    }

    fn root_table(&self) -> &mut [u64; PTE_COUNT] {
        unsafe { &mut *(self.root_phys as *mut [u64; PTE_COUNT]) }
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

    fn map_4k(
        &self,
        virt: usize,
        phys: usize,
        flags: u64,
    ) -> Result<(), AddressSpaceError> {
        let l2 = self.root_table();
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
        *entry =
            (ppn << PTE_PPN_SHIFT) | flags | PTE_VALID | PTE_USER | PTE_ACCESSED | PTE_DIRTY;
        sfence_vma_addr(virt);
        Ok(())
    }

    fn unmap_4k(&self, virt: usize) -> Result<u64, AddressSpaceError> {
        let l2 = self.root_table();
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

    fn protect_4k(&self, virt: usize, flags: u64) -> Result<(), AddressSpaceError> {
        let l2 = self.root_table();
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
        let ppn = *entry >> PTE_PPN_SHIFT;
        *entry =
            (ppn << PTE_PPN_SHIFT) | flags | PTE_VALID | PTE_USER | PTE_ACCESSED | PTE_DIRTY;
        sfence_vma_addr(virt);
        Ok(())
    }

    fn translate_4k(&self, virt: usize) -> Option<(usize, u64)> {
        let l2 = self.root_table();
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
        let l2 = self.root_table();
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
        let raw = allocate_user_frame_zeroed().map_err(|_| AddressSpaceError::OutOfFrames)?;
        Ok(raw.as_ptr() as usize)
    }

    fn dealloc_user_phys(&self, phys: usize) {
        let ptr = NonNull::new(phys as *mut u8)
            .unwrap_or_else(|| panic!("RISC-V user-frame dealloc received null pointer"));
        deallocate_user_frame(ptr);
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
            for offset in (0..region.range.byte_len).step_by(PAGE_SIZE) {
                if let Ok(entry) = self.unmap_4k(region.range.start.raw() + offset) {
                    let phys = ((entry >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize;
                    self.dealloc_user_phys(phys);
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

        for offset in (0..virt.byte_len).step_by(PAGE_SIZE) {
            let virt_addr = virt.start.raw() + offset;
            let phys = self.alloc_user_frame()?;
            self.map_4k(virt_addr, phys, pte_flags)?;
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
        for offset in (0..virt.byte_len).step_by(PAGE_SIZE) {
            self.protect_4k(virt.start.raw() + offset, pte_flags)?;
        }
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

/// Range covered by the kernel identity map, used by sanity checks
/// in tests / assertions elsewhere in the backend.
#[allow(dead_code)]
pub fn identity_map_range() -> Range<usize> {
    0..(KERNEL_IDENTITY_GIB * GIB)
}

/// Register the boot-time `RiscvUserAddressSpace` as the active
/// wasmtime custom-virtual-memory backend. Must be called once on
/// the bootstrap hart, after `install_kernel_paging`, before any
/// wasmtime engine is constructed.
pub fn install_wasmtime_hooks() {
    custom_vm::install_hooks(&RISCV_VMM_HOOKS);
}

fn user_as() -> &'static RiscvUserAddressSpace {
    user_address_space().expect(
        "RiscvUserAddressSpace accessed before install_kernel_paging",
    )
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

/// Wasmtime custom-virtual-memory hook table for the riscv backend.
pub static RISCV_VMM_HOOKS: CustomVmHooks = CustomVmHooks {
    mmap_new: riscv_mmap_new,
    mmap_remap: riscv_mmap_remap,
    munmap: riscv_munmap,
    mprotect: riscv_mprotect,
    page_size: default_page_size,
    memory_image_new: default_memory_image_new,
    memory_image_free: default_memory_image_free,
    memory_image_map_at: default_memory_image_map_at,
};
