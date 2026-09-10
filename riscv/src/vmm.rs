//! Sv48 paging for the RISC-V backend.
//!
//! The kernel boots in machine→supervisor mode via OpenSBI with
//! paging disabled (`satp = 0`); every address is a physical address.
//! This module brings up Sv48 with an identity map covering all
//! physical memory the kernel will ever touch, so paging activation is
//! a no-op for kernel addresses, then carves a high user-VA window
//! 0x0000_2000_0000_0000..0x0000_4000_0000_0000 (32 TiB) for
//! [`RiscvUserAddressSpace`] dynamic mappings.
//!
//! # Why Sv48 and not Sv39
//!
//! The user window has to hold the runtime's linear-memory
//! reservations, and those are what let Cranelift drop bounds checks:
//! one 4 GiB reservation plus a 32 MiB guard region per pooled slot,
//! a thousand slots per engine, two engines. Sv39's entire 512 GiB
//! address space cannot hold one engine's worth. Sv48 gives 256 TiB,
//! and the window carved here matches the x86 backend's so a VA in a
//! fault trace reads the same on both.
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
//! The identity map is one root-level leaf: Sv48's largest page is
//! 512 GiB, so a single PTE covers every physical address the platform
//! exposes and the map needs no page-table allocation at all. That
//! matters because paging comes up before the kernel heap does. The
//! user-VA window allocates fresh L2/L1/L0 tables on demand for
//! fine-grained mappings, by which time the heap exists; each table is
//! one 4 KiB physical frame from the kernel global allocator.
//!
//! # Why user pages are not marked `U`
//!
//! Every helios thread runs in supervisor mode, guest wasm included:
//! the "user" window is a range the kernel owns on a guest's behalf,
//! not a privilege boundary. An S-mode load or store to a page with
//! `PTE_USER` set faults unless `sstatus.SUM` is set, and an S-mode
//! instruction fetch from one faults regardless of `SUM` — which the
//! runtime's compiled code, mapped in this window, would hit on its
//! first call. So these mappings are supervisor-only, exactly like the
//! aarch64 backend's `AP_KERNEL_RW`.
//!
//! # SMP
//!
//! `enable_paging` runs on every hart that comes online. The bootstrap
//! hart sets up the root table, secondary harts share it via `satp`.
//! Every address-space mutation invalidates this hart's TLB entries
//! and dispatches one `sbi_rt::remote_sfence_vma` range fence to every
//! other hart, after the last page-table write and before any frame
//! goes back to the user-memory pool.

extern crate alloc;

use alloc::alloc::{Layout, alloc_zeroed};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ffi::c_int;
use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use helios_hal::device::{DeviceRegion, DeviceRegionAttributes, DmaPlacement};
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{
    AddressSpace, AddressSpaceError, PageFlags, Translation, VirtAddr, VirtRange,
};
use helios_kernel::runtime_memory::{
    self, RuntimeMemoryHooks, default_memory_image_free, default_memory_image_map_at,
    default_memory_image_new, default_page_size,
};
use helios_kernel::{
    FiberStackVmHooks, MemoryOwner, ReservationLookup, ReservationTracker, VaCursor,
    allocate_user_frame_zeroed_on, allocate_user_run_zeroed_on, deallocate_user_frame_on,
    deallocate_user_run_on, install_fiber_stack_hooks, validate_range,
};
use spin::{Mutex, Once};

const PAGE_SHIFT: u32 = 12;
const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
const PTE_COUNT: usize = 512;
const LEVEL_BITS: u32 = 9;
/// Sv48 resolves a virtual address through four page-table levels; the
/// root table is level 3 and the leaf table level 0.
const LEVELS: usize = 4;
const SV48_VA_BITS: u32 = 48;
const SV48_PA_BITS: u32 = 56;

const PTE_VALID: u64 = 1 << 0;
const PTE_READ: u64 = 1 << 1;
const PTE_WRITE: u64 = 1 << 2;
const PTE_EXECUTE: u64 = 1 << 3;
const PTE_ACCESSED: u64 = 1 << 6;
const PTE_DIRTY: u64 = 1 << 7;
const PTE_PPN_SHIFT: u32 = 10;
const PTE_FLAGS_MASK: u64 = (1 << PTE_PPN_SHIFT) - 1;
const PTE_LEAF_MASK: u64 = PTE_READ | PTE_WRITE | PTE_EXECUTE;

const SATP_MODE_SHIFT: u32 = 60;
const SATP_MODE_SV48: u64 = 9 << SATP_MODE_SHIFT;

const GIB: usize = 1 << 30;
/// Bytes one root-level leaf maps in Sv48: 512 GiB, the mode's largest
/// page size. Every physical address a RISC-V platform helios targets
/// exposes — MMIO below 1 GiB, RAM from 0x8000_0000 up — is inside the
/// first one, so the kernel identity map is a single PTE.
const KERNEL_IDENTITY_BYTES: usize = 1 << (PAGE_SHIFT + LEVEL_BITS * (LEVELS as u32 - 1));

/// Highest physical address the kernel identity map reaches. Physical
/// memory the firmware reports past this point is unreachable and the
/// backend refuses to boot rather than faulting on it later.
pub const KERNEL_IDENTITY_LIMIT: usize = KERNEL_IDENTITY_BYTES;

/// User-VA window. Starts well clear of the identity map so address
/// values are unambiguous when debugging fault traces, and matches the
/// x86 backend's window so the two read alike.
const USER_VA_BASE: usize = 0x0000_2000_0000_0000;
const USER_VA_END: usize = 0x0000_4000_0000_0000;

/// Pages whose entries one decommit batch holds before flushing.
///
/// A frame must not go back to the user-memory pool while another hart
/// can still have a translation for it, so the old entries are kept
/// until the shootdown for their range has been acknowledged. Batching
/// bounds that on-stack array while keeping the shootdown count to one
/// SBI call per 128 pages instead of one per page.
const TLB_DECOMMIT_BATCH_PAGES: usize = 128;

fn level_index(virt: usize, level: usize) -> usize {
    (virt >> (PAGE_SHIFT + LEVEL_BITS * level as u32)) & (PTE_COUNT - 1)
}

fn entry_phys(entry: u64) -> usize {
    ((entry >> PTE_PPN_SHIFT) << PAGE_SHIFT) as usize
}

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
    // A single 512 GiB leaf at root index 0: PPN 0, so virtual equals
    // physical across the whole identity window. Nothing is allocated,
    // which is what lets paging come up before the kernel heap does.
    entries[0] = PTE_VALID | PTE_READ | PTE_WRITE | PTE_EXECUTE | PTE_ACCESSED | PTE_DIRTY;
    let root_phys = ROOT_TABLE.phys_addr();
    assert!(
        root_phys.is_multiple_of(PAGE_SIZE),
        "Sv48 root table is not page-aligned"
    );
    PAGING_ROOT_PHYS.store(root_phys, Ordering::Release);
    USER_AS.call_once(|| RiscvUserAddressSpace::new(root_phys))
}

/// Switch the calling hart into Sv48 paging using the root table
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
        "Sv48 root table not installed before activate_paging"
    );
    let satp = SATP_MODE_SV48 | (root_phys as u64 >> PAGE_SHIFT);
    unsafe {
        core::arch::asm!(
            "csrw satp, {satp}",
            "sfence.vma zero, zero",
            satp = in(reg) satp,
            options(nostack, preserves_flags),
        );
    }
    // `satp.MODE` is WARL: a hart that does not implement Sv48 discards
    // the whole write and stays in bare mode, where the identity map
    // happens to make execution look fine right up until the first user
    // mapping is dereferenced. Reading the register back is the only way
    // to tell the two apart, and there is no 39-bit configuration to
    // fall back to, so a hart that refuses Sv48 stops here.
    let installed = riscv::register::satp::read().bits() as u64;
    assert!(
        installed == satp,
        "hart does not support Sv48 paging: satp reads {installed:#x} after writing {satp:#x}"
    );
}

/// Get the user address space registered at boot. Returns `None`
/// before [`install_kernel_paging`] has run.
pub fn user_address_space() -> Option<&'static RiscvUserAddressSpace> {
    USER_AS.get()
}

/// Sv48-backed user address space. Tracks reservations in a software
/// table; commit/decommit/protect mutate live page-table entries
/// hanging off the kernel root table at the root indices that fall
/// inside [`USER_VA_BASE`..`USER_VA_END`].
/// What backs one device mapping, and therefore what releasing it owes.
#[derive(Clone, Copy)]
enum DeviceBacking {
    /// A physical register window. It was never allocated, so tearing
    /// the mapping down frees nothing.
    Registers,
    /// A pinned run from the user pool: one buddy allocation at `phys`,
    /// made with `align`.
    Pinned { phys: usize, align: usize },
}

/// One device mapping inside a user reservation.
#[derive(Clone, Copy)]
struct DeviceMapping {
    range: VirtRange,
    backing: DeviceBacking,
}

pub struct RiscvUserAddressSpace {
    root_phys: usize,
    va_cursor: VaCursor,
    state: Mutex<ReservationTracker>,
    /// Mappings the device path installed inside a user reservation.
    ///
    /// The reservation tracker deliberately does not know about these.
    /// Its job is to hand frames back to the user pool when a
    /// reservation is released, and neither kind of device mapping may
    /// go there: a register window was never taken from the pool at
    /// all, and a pinned run is one buddy allocation that has to be
    /// released with the layout it was made with. Keeping them in their
    /// own list is also what makes `release` total, so a reservation
    /// torn down while a driver still holds it leaves no live
    /// translation to the hardware behind.
    ///
    /// Lock order is `state` then `devices`; nothing takes `devices`
    /// and then `state`.
    devices: Mutex<Vec<DeviceMapping>>,
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
            devices: Mutex::new(Vec::new()),
        }
    }

    /// Page-table attributes for one device region.
    ///
    /// RISC-V's base translation carries no memory type: whether an
    /// access is cacheable and reorderable is a physical memory
    /// attribute of the address, fixed by the platform, not something
    /// a leaf PTE selects. (Svpbmt adds one, and QEMU's `virt` does not
    /// offer it.) So a register window is an ordinary valid leaf
    /// pointing into the platform's I/O space, and the region's `kind`
    /// changes nothing here — it is already true of the address.
    /// Execute is never granted: a device holds no instructions the
    /// guest may run.
    fn device_pte(&self, attributes: DeviceRegionAttributes) -> u64 {
        if attributes.writable {
            PTE_READ | PTE_WRITE
        } else {
            PTE_READ
        }
    }

    /// Undo a device mapping's leaves, without freeing anything. The
    /// shootdown goes out to every hart through SBI before returning.
    fn tear_down_device_range(&self, range: VirtRange) -> Result<(), AddressSpaceError> {
        let pages = range.byte_len / PAGE_SIZE;
        for page in 0..pages {
            self.unmap_4k_no_flush(range.start.raw() + page * PAGE_SIZE)?;
        }
        flush_tlb_pages(range.start.raw(), pages);
        Ok(())
    }

    /// Remove every device mapping that falls inside `virt`.
    ///
    /// A reservation can be released while a driver still holds its
    /// device — a store torn down by an OOM kill does exactly that —
    /// and the address space is the last place that can guarantee the
    /// hardware is unreachable afterwards.
    fn sweep_device_mappings(&self, virt: VirtRange) {
        let mut devices = self.devices.lock();
        let mut index = 0;
        while index < devices.len() {
            let mapping = devices[index];
            if mapping.range.start.raw() < virt.start.raw()
                || mapping.range.start.raw() + mapping.range.byte_len
                    > virt.start.raw() + virt.byte_len
            {
                index += 1;
                continue;
            }
            devices.swap_remove(index);
            self.tear_down_device_range(mapping.range)
                .unwrap_or_else(|error| {
                    panic!(
                        "RISC-V release could not tear down the device mapping at {:#x}: {error}",
                        mapping.range.start.raw()
                    )
                });
            if let DeviceBacking::Pinned { phys, align } = mapping.backing {
                self.free_pinned_run(phys, mapping.range.byte_len, align);
            }
        }
    }

    /// Give one pinned run back to the user pool with the layout it was
    /// allocated with.
    fn free_pinned_run(&self, phys: usize, bytes: usize, align: usize) {
        let layout = Layout::from_size_align(bytes, align)
            .unwrap_or_else(|_| panic!("RISC-V pinned run has an invalid layout"));
        let ptr = NonNull::new(phys as *mut u8)
            .unwrap_or_else(|| panic!("RISC-V pinned run has a null pointer"));
        deallocate_user_run_on(crate::current_hart_id(), ptr, layout);
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
                *entry & PTE_LEAF_MASK == 0,
                "Sv48 user-VA index {index:#x} collides with a leaf mapping"
            );
            let phys = entry_phys(*entry);
            return unsafe { &mut *(phys as *mut [u64; PTE_COUNT]) };
        }
        let layout = Layout::from_size_align(PAGE_SIZE, PAGE_SIZE)
            .expect("Sv48 page-table layout is well-formed");
        let raw = unsafe { alloc_zeroed(layout) };
        let raw = NonNull::new(raw).expect("Sv48 page-table allocation failed");
        let phys = raw.as_ptr() as usize;
        *entry = ((phys as u64) >> PAGE_SHIFT) << PTE_PPN_SHIFT | PTE_VALID;
        unsafe { &mut *(phys as *mut [u64; PTE_COUNT]) }
    }

    /// Pointer to the leaf entry for `virt`, allocating the
    /// intermediate tables the walk is missing.
    ///
    /// The caller holds this address space's reservation lock, so no
    /// other hart is editing its tables.
    fn ensure_leaf_entry(&self, virt: usize) -> *mut u64 {
        let mut table = unsafe { &mut *self.root_table() };
        for level in (1..LEVELS).rev() {
            table = Self::ensure_intermediate(table, level_index(virt, level));
        }
        &mut table[level_index(virt, 0)]
    }

    /// Pointer to the leaf entry for `virt`, or `None` when the walk
    /// runs into a missing table.
    ///
    /// The caller holds this address space's reservation lock, so no
    /// other hart is editing its tables.
    fn leaf_entry(&self, virt: usize) -> Option<*mut u64> {
        let mut table = self.root_table();
        for level in (1..LEVELS).rev() {
            let entry = unsafe { (*table)[level_index(virt, level)] };
            if entry & PTE_VALID == 0 {
                return None;
            }
            assert!(
                entry & PTE_LEAF_MASK == 0,
                "Sv48 user VA {virt:#x} resolves through a leaf mapping at level {level}"
            );
            table = entry_phys(entry) as *mut [u64; PTE_COUNT];
        }
        Some(unsafe { &raw mut (*table)[level_index(virt, 0)] })
    }

    /// Builds every table level above the leaf for `virt`, leaving the
    /// leaf entry itself absent.
    ///
    /// The caller holds this address space's reservation lock, which is
    /// what makes `ensure_intermediate`'s heap allocation safe here and
    /// impossible in fault context.
    fn ensure_leaf_table(&self, virt: usize) {
        let mut table = unsafe { &mut *self.root_table() };
        for level in (1..LEVELS).rev() {
            table = Self::ensure_intermediate(table, level_index(virt, level));
        }
    }

    /// Unmaps every page of `virt` a demand commit mapped, leaving the
    /// pages nothing ever faulted on alone.
    fn unmap_demand_pages(&self, virt: VirtRange) {
        let mut batch_entries = [0u64; TLB_DECOMMIT_BATCH_PAGES];
        let mut batch_count = 0;
        let mut batch_start = virt.start.raw();
        for offset in (0..virt.byte_len).step_by(PAGE_SIZE) {
            let page = virt.start.raw() + offset;
            let entry = match self.unmap_4k_no_flush(page) {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if batch_count == 0 {
                batch_start = page;
            }
            batch_entries[batch_count] = entry;
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

    fn map_4k_no_flush(
        &self,
        virt: usize,
        phys: usize,
        flags: u64,
    ) -> Result<(), AddressSpaceError> {
        let entry_ptr = self.ensure_leaf_entry(virt);
        let entry = unsafe { entry_ptr.read_volatile() };
        if entry & PTE_VALID != 0 {
            return Err(AddressSpaceError::Overlap);
        }
        let ppn = (phys as u64) >> PAGE_SHIFT;
        unsafe {
            entry_ptr.write_volatile(
                (ppn << PTE_PPN_SHIFT) | flags | PTE_VALID | PTE_ACCESSED | PTE_DIRTY,
            );
        }
        Ok(())
    }

    fn unmap_4k_no_flush(&self, virt: usize) -> Result<u64, AddressSpaceError> {
        let entry_ptr = self
            .leaf_entry(virt)
            .ok_or(AddressSpaceError::NotCommitted)?;
        let entry = unsafe { entry_ptr.read_volatile() };
        if entry & PTE_VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        unsafe {
            entry_ptr.write_volatile(0);
        }
        Ok(entry)
    }

    fn protect_4k_no_flush(&self, virt: usize, flags: u64) -> Result<u64, AddressSpaceError> {
        let entry_ptr = self
            .leaf_entry(virt)
            .ok_or(AddressSpaceError::NotCommitted)?;
        let old_entry = unsafe { entry_ptr.read_volatile() };
        if old_entry & PTE_VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        let ppn = old_entry >> PTE_PPN_SHIFT;
        unsafe {
            entry_ptr.write_volatile(
                (ppn << PTE_PPN_SHIFT) | flags | PTE_VALID | PTE_ACCESSED | PTE_DIRTY,
            );
        }
        Ok(old_entry)
    }

    fn replace_4k_no_flush(&self, virt: usize, new_entry: u64) -> Result<u64, AddressSpaceError> {
        let entry_ptr = self
            .leaf_entry(virt)
            .ok_or(AddressSpaceError::NotCommitted)?;
        let old_entry = unsafe { entry_ptr.read_volatile() };
        if old_entry & PTE_VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        unsafe {
            entry_ptr.write_volatile(new_entry);
        }
        Ok(old_entry)
    }

    fn translate_4k(&self, virt: usize) -> Option<(usize, u64)> {
        let entry = unsafe { self.leaf_entry(virt)?.read_volatile() };
        if entry & PTE_VALID == 0 {
            return None;
        }
        if entry & PTE_LEAF_MASK == 0 {
            // Pointer to next table (shouldn't happen at the leaf level).
            return None;
        }
        Some((entry_phys(entry), entry))
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
            self.replace_4k_no_flush(page.virt, page.old_entry)
                .unwrap_or_else(|error| {
                    panic!(
                        "RISC-V AddressSpace::relocate rollback failed at {:#x}: {error}",
                        page.virt
                    )
                });
        }
        // The pages this restored were briefly mapped to the replacement
        // frames, so those frames are not free to reuse until every hart has
        // dropped the translation (AGENTS §3.4).
        if let Some(first) = pages.first() {
            flush_tlb_pages(first.virt, pages.len());
        }
        for page in pages {
            self.dealloc_user_phys(page.new_phys);
        }
    }

    fn rollback_partial_commit(&self, start: usize, mapped_pages: usize) {
        let mut entries = [0u64; TLB_DECOMMIT_BATCH_PAGES];
        for page in 0..mapped_pages {
            let virt = start + page * PAGE_SIZE;
            entries[page % TLB_DECOMMIT_BATCH_PAGES] =
                self.unmap_4k_no_flush(virt).unwrap_or_else(|error| {
                    panic!("RISC-V AddressSpace::commit rollback failed at {virt:#x}: {error}")
                });
            if (page + 1) % TLB_DECOMMIT_BATCH_PAGES == 0 {
                let batch_start = start + (page + 1 - TLB_DECOMMIT_BATCH_PAGES) * PAGE_SIZE;
                self.flush_and_dealloc_entries(batch_start, &entries);
            }
        }
        let tail = mapped_pages % TLB_DECOMMIT_BATCH_PAGES;
        if tail != 0 {
            let batch_start = start + (mapped_pages - tail) * PAGE_SIZE;
            self.flush_and_dealloc_entries(batch_start, &entries[..tail]);
        }
    }

    fn rollback_partial_protect(&self, start: usize, old_entries: &[u64]) {
        for (page, entry) in old_entries.iter().copied().enumerate().rev() {
            let virt = start + page * PAGE_SIZE;
            self.replace_4k_no_flush(virt, entry)
                .unwrap_or_else(|error| {
                    panic!("RISC-V AddressSpace::protect rollback failed at {virt:#x}: {error}")
                });
        }
        flush_tlb_pages(start, old_entries.len());
    }

    /// Unmap every page of `virt`, shooting the range down before its
    /// frames go back to the user-memory pool.
    fn decommit_mapped_range(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        let mut batch_entries = [0u64; TLB_DECOMMIT_BATCH_PAGES];
        let mut batch_count = 0;
        let mut batch_start = virt.start.raw();
        for offset in (0..virt.byte_len).step_by(PAGE_SIZE) {
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
        flush_tlb_pages(start, entries.len());
        for entry in entries {
            self.dealloc_user_phys(entry_phys(*entry));
        }
    }

    /// Decommit only the parts of `range` that are actually committed.
    ///
    /// The runtime resets a pooled slot by asking for a range whose
    /// committed extent it does not track precisely, so a whole-range
    /// decommit would fail on the first uncommitted page. Coalescing
    /// also keeps the shootdown count proportional to the number of
    /// committed sub-ranges rather than to the number of pages.
    fn decommit_committed_subranges(&self, range: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(range)?;
        let mut state = self.state.lock();
        for subrange in state.take_committed_intersections(range)? {
            self.decommit_mapped_range(subrange)?;
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

        let mut mapped_pages = 0;
        for offset in (0..virt.byte_len).step_by(PAGE_SIZE) {
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
        flush_tlb_pages(virt.start.raw(), mapped_pages);

        // This backend has no swap, so committing over the range can
        // never orphan a swap extent; the assertion keeps that true if
        // swap reaches this architecture (#25).
        let orphaned = state.record_commit(virt, flags, MemoryOwner::NONE)?;
        debug_assert!(orphaned.is_empty());
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
        let mut old_entries = Vec::new();
        for offset in (0..virt.byte_len).step_by(PAGE_SIZE) {
            match self.protect_4k_no_flush(virt.start.raw() + offset, pte_flags) {
                Ok(entry) => old_entries.push(entry),
                Err(error) => {
                    self.rollback_partial_protect(virt.start.raw(), &old_entries);
                    return Err(error);
                }
            }
        }
        flush_tlb_pages(virt.start.raw(), old_entries.len());
        state.record_protect(virt, flags)?;
        Ok(())
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
        // Before anything is given back: a driver whose store died
        // still has its device mapped here, and the frames under a
        // pinned run are the pool's, not this reservation's.
        self.sweep_device_mappings(virt);
        let mut state = self.state.lock();
        let released = state.release(virt)?;
        // This backend has no swap, so a released reservation never holds
        // a swap token; the assertion keeps that true if swap ever reaches
        // this architecture (#25).
        debug_assert!(released.swapped.is_empty());
        for region in &released.committed {
            self.decommit_mapped_range(region.range)?;
        }
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
        debug_assert!(swapped.is_empty());
        self.decommit_mapped_range(virt)
    }

    fn map_device(&self, virt: VirtRange, region: DeviceRegion) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        if !region.is_frame_aligned() || region.physical.bytes as usize != virt.byte_len {
            return Err(AddressSpaceError::Misaligned);
        }
        let pte_flags = self.device_pte(region.attributes);
        let phys_base = region.physical.start as usize;
        let pages = virt.byte_len / PAGE_SIZE;
        // The window is claimed before a single leaf is written, so two
        // grants naming overlapping physical space cannot both believe
        // they own it.
        let mut devices = self.devices.lock();
        if devices
            .iter()
            .any(|mapping| ranges_overlap(mapping.range, virt))
        {
            return Err(AddressSpaceError::DeviceMapped);
        }
        for page in 0..pages {
            let virt_addr = virt.start.raw() + page * PAGE_SIZE;
            if let Err(error) =
                self.map_4k_no_flush(virt_addr, phys_base + page * PAGE_SIZE, pte_flags)
            {
                for undone in 0..page {
                    let _ = self.unmap_4k_no_flush(virt.start.raw() + undone * PAGE_SIZE);
                }
                flush_tlb_pages(virt.start.raw(), page);
                return Err(error);
            }
        }
        flush_tlb_pages(virt.start.raw(), pages);
        devices.push(DeviceMapping {
            range: virt,
            backing: DeviceBacking::Registers,
        });
        Ok(())
    }

    fn unmap_device(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let mut devices = self.devices.lock();
        let index = devices
            .iter()
            .position(|mapping| mapping.range.start.raw() == virt.start.raw())
            .ok_or(AddressSpaceError::NotCommitted)?;
        let mapping = devices.swap_remove(index);
        drop(devices);
        self.tear_down_device_range(mapping.range)?;
        if let DeviceBacking::Pinned { phys, align } = mapping.backing {
            self.free_pinned_run(phys, mapping.range.byte_len, align);
        }
        Ok(())
    }

    fn commit_contiguous(
        &self,
        virt: VirtRange,
        flags: PageFlags,
        placement: DmaPlacement,
    ) -> Result<PhysFrame, AddressSpaceError> {
        validate_range(virt)?;
        let align = usize::try_from(placement.align)
            .ok()
            .filter(|align| align.is_power_of_two() && *align >= PAGE_SIZE)
            .ok_or(AddressSpaceError::Misaligned)?;
        let pte_flags = page_flags_to_pte(flags)?;
        let layout = Layout::from_size_align(virt.byte_len, align)
            .map_err(|_| AddressSpaceError::Misaligned)?;
        // One allocation, so the run is contiguous by construction: the
        // buddy allocator has no way to answer a single request with two
        // blocks. Zeroed, because the device sees this memory before the
        // driver writes a descriptor into it. The kernel identity-maps
        // physical memory on this backend, so the pointer is the
        // physical address.
        let raw = allocate_user_run_zeroed_on(crate::current_hart_id(), layout)
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        let phys = raw.as_ptr() as usize;
        if !placement.accepts(phys as u64, virt.byte_len as u64) {
            // The pool answered from above the device's address lines.
            // Truncation would show up as corruption inside the device,
            // so refuse rather than hand it over.
            self.free_pinned_run(phys, virt.byte_len, align);
            return Err(AddressSpaceError::OutOfFrames);
        }
        let pages = virt.byte_len / PAGE_SIZE;
        let mut devices = self.devices.lock();
        for page in 0..pages {
            let virt_addr = virt.start.raw() + page * PAGE_SIZE;
            if let Err(error) = self.map_4k_no_flush(virt_addr, phys + page * PAGE_SIZE, pte_flags)
            {
                for undone in 0..page {
                    let _ = self.unmap_4k_no_flush(virt.start.raw() + undone * PAGE_SIZE);
                }
                flush_tlb_pages(virt.start.raw(), page);
                drop(devices);
                self.free_pinned_run(phys, virt.byte_len, align);
                return Err(error);
            }
        }
        flush_tlb_pages(virt.start.raw(), pages);
        devices.push(DeviceMapping {
            range: virt,
            backing: DeviceBacking::Pinned { phys, align },
        });
        Ok(PhysFrame::from_phys_addr(phys))
    }

    fn release_contiguous(&self, virt: VirtRange, _align: u64) -> Result<(), AddressSpaceError> {
        // The alignment the run was made with was recorded at commit
        // time, so it is read back from there rather than trusted from
        // the caller: the allocator is owed the layout it produced.
        self.unmap_device(virt)
    }

    fn protect(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let mut state = self.state.lock();
        self.protect_locked(&mut state, virt, flags)
    }

    fn prepare_demand_commit(
        &self,
        virt: VirtRange,
        flags: PageFlags,
    ) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        // Validated here rather than at the first fault: a fault-time
        // commit has no way to report a bad flag combination.
        let _ = page_flags_to_pte(flags)?;
        let mut state = self.state.lock();
        state.precheck_commit(virt)?;
        for offset in (0..virt.byte_len).step_by(PAGE_SIZE) {
            self.ensure_leaf_table(virt.start.raw() + offset);
        }
        // `MemoryOwner::NONE` on purpose. Residency accounting and the
        // swap aging pass both key on a real owner, and neither may see
        // this region: its pages are not resident until something faults
        // on them, and ageing one would offer the policy the very stack
        // its fault handler is standing on.
        let orphaned = state.record_commit(virt, flags, MemoryOwner::NONE)?;
        debug_assert!(orphaned.is_empty());
        Ok(())
    }

    fn commit_demand_page(
        &self,
        addr: VirtAddr,
        frame: NonNull<u8>,
        flags: PageFlags,
    ) -> Result<(), AddressSpaceError> {
        if !addr.is_page_aligned() {
            return Err(AddressSpaceError::Misaligned);
        }
        let pte_flags = page_flags_to_pte(flags)?;
        let virt = addr.raw();
        let entry_ptr = self
            .leaf_entry(virt)
            .ok_or(AddressSpaceError::NotDemandCommit)?;
        // SAFETY: the walk above ran through live tables, and the leaf
        // it names belongs to a prepared demand-commit region, which
        // only the fiber running on it ever faults on. This backend
        // identity-maps physical memory, so the pool's pointer is the
        // physical address.
        let entry = unsafe { entry_ptr.read_volatile() };
        if entry & PTE_VALID != 0 {
            return Err(AddressSpaceError::Overlap);
        }
        let ppn = (frame.as_ptr() as u64) >> PAGE_SHIFT;
        // SAFETY: as above; the store publishes a complete leaf.
        unsafe {
            entry_ptr.write_volatile(
                (ppn << PTE_PPN_SHIFT) | pte_flags | PTE_VALID | PTE_ACCESSED | PTE_DIRTY,
            );
        }
        // A local fence and no remote one: the entry went from invalid
        // to valid, and RISC-V allows an implementation to have cached
        // the invalid entry on the hart that walked it — which is this
        // one, the one that just faulted. No other hart has looked at
        // this page, because no other hart runs this fiber.
        // SAFETY: `sfence.vma` with a virtual address operand is a
        // supervisor-legal local fence.
        unsafe {
            core::arch::asm!(
                "sfence.vma {addr}, zero",
                addr = in(reg) virt,
                options(nostack, preserves_flags),
            );
        }
        Ok(())
    }

    fn end_demand_commit(&self, virt: VirtRange) -> Result<(), AddressSpaceError> {
        validate_range(virt)?;
        let mut state = self.state.lock();
        let swapped = state.record_decommit(virt)?;
        debug_assert!(swapped.is_empty());
        self.unmap_demand_pages(virt);
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
        let state = self.state.lock();
        state.ensure_committed(virt)?;

        let pages = self.build_relocation_plan(virt)?;
        for (index, page) in pages.iter().enumerate() {
            let new_entry = (((page.new_phys as u64) >> PAGE_SHIFT) << PTE_PPN_SHIFT)
                | (page.old_entry & PTE_FLAGS_MASK);
            if let Err(error) = self.replace_4k_no_flush(page.virt, new_entry) {
                self.rollback_relocation(&pages, index);
                return Err(error);
            }
        }
        // Every entry now points at its new frame; the old frames stay
        // out of the pool until no hart can still translate to them.
        flush_tlb_pages(virt.start.raw(), virt.byte_len / PAGE_SIZE);
        for page in &pages {
            self.dealloc_user_phys(page.old_phys);
        }
        Ok(())
    }
}

/// Invalidate `pages` translations starting at `start` on this hart and
/// on every other hart in the system.
///
/// The local `sfence.vma` walk covers this hart; remote harts are
/// reached with a single SBI range fence, which returns once every
/// target hart has completed it. Callers issue every page-table write
/// first and release no frame until this returns (AGENTS §3.4).
fn flush_tlb_pages(start: usize, pages: usize) {
    if pages == 0 {
        return;
    }
    for page in 0..pages {
        let virt = start + page * PAGE_SIZE;
        unsafe {
            core::arch::asm!(
                "sfence.vma {addr}, zero",
                addr = in(reg) virt,
                options(nostack, preserves_flags),
            );
        }
    }
    let byte_len = pages * PAGE_SIZE;
    let ret = sbi_rt::remote_sfence_vma(
        sbi_rt::HartMask::from_mask_base(0, usize::MAX),
        start,
        byte_len,
    );
    assert!(
        ret.is_ok(),
        "SBI remote_sfence_vma failed for user VA {start:#x}+{byte_len:#x}: error={} value={}",
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
    assert!(SV48_VA_BITS == PAGE_SHIFT + LEVEL_BITS * LEVELS as u32);
    assert!(SV48_PA_BITS == 56);
    assert!(KERNEL_IDENTITY_BYTES == 512 * GIB);
    // The user window must not share a root-level entry with the
    // identity map's 512 GiB leaf, and must be a whole number of them
    // so the walk never has to split that leaf.
    assert!(USER_VA_BASE >= KERNEL_IDENTITY_BYTES);
    assert!(USER_VA_BASE.is_multiple_of(KERNEL_IDENTITY_BYTES));
    assert!(USER_VA_END > USER_VA_BASE);
    assert!(USER_VA_END.is_multiple_of(KERNEL_IDENTITY_BYTES));
    // Sv48 sign-extends bit 47, so a window in the lower half has to
    // stay clear of it to remain canonical.
    assert!(USER_VA_END <= 1 << (SV48_VA_BITS - 1));
    // The runtime pre-reserves 4 GiB plus a 32 MiB guard per pooled
    // linear-memory slot, a thousand slots per engine, and the kernel
    // builds one engine for system components and one for launched
    // programs. The window has to hold both.
    assert!(USER_VA_END - USER_VA_BASE >= 2 * 1000 * (4 * GIB + 32 * (1 << 20)));
};

/// Register the boot-time `RiscvUserAddressSpace` as the active
/// runtime custom-virtual-memory backend. Must be called once on
/// the bootstrap hart, after `install_kernel_paging`, before any
/// runtime engine is constructed.
pub fn install_runtime_memory_hooks() {
    runtime_memory::install_hooks(&RISCV_VMM_HOOKS);
    install_fiber_stack_hooks(&RISCV_FIBER_STACK_HOOKS);
}

/// The arena's address-space surface for this backend.
///
/// Separate from [`RISCV_VMM_HOOKS`] because it is a different contract:
/// that one answers the runtime's C mmap ABI, this one is the typed
/// route the kernel's fiber-stack arena takes to the same address space,
/// and one of its entries runs in trap context where the other's may
/// not.
static RISCV_FIBER_STACK_HOOKS: FiberStackVmHooks = FiberStackVmHooks {
    reserve: |bytes| user_as().reserve(bytes),
    prepare_demand_commit: |virt, flags| user_as().prepare_demand_commit(virt, flags),
    commit_demand_page: |addr, frame, flags| user_as().commit_demand_page(addr, frame, flags),
    end_demand_commit: |virt| user_as().end_demand_commit(virt),
};

/// The machine's one user address space.
pub(crate) fn user_address_space_or_panic() -> &'static RiscvUserAddressSpace {
    user_as()
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
        Err(error) => {
            tracing::error!(
                target: "helios_riscv::vmm",
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
                target: "helios_riscv::vmm",
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
        target: "helios_riscv::vmm",
        size,
        prot_flags,
        addr = range.start.raw(),
        "mmap_new ok"
    );
    0
}

extern "C" fn riscv_mmap_remap(addr: *mut u8, size: usize, prot_flags: u32) -> c_int {
    // The runtime's `mmap_remap` rebinds an existing mapping to fresh
    // anonymous-zero pages with `prot_flags`. This address space cannot
    // swap to brand-new frames in a single transaction, so the closest
    // faithful sequence is decommit→commit: decommit returns the old
    // frames to the user-memory pool, commit takes fresh zeroed ones and
    // maps them over the same range. The window where the range is
    // uncommitted is invisible to the runtime, which only remaps a slot
    // between instances, with no thread touching it.
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = VirtRange::new(VirtAddr::new(addr as usize), size);
    if let Err(error) = address_space.decommit_committed_subranges(range) {
        tracing::error!(
            target: "helios_riscv::vmm",
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
                target: "helios_riscv::vmm",
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

extern "C" fn riscv_munmap(ptr: *mut u8, size: usize) -> c_int {
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = VirtRange::new(VirtAddr::new(ptr as usize), size);
    match address_space.release(range) {
        Ok(()) => {
            tracing::trace!(target: "helios_riscv::vmm", addr = ptr as usize, size, "munmap ok");
            0
        }
        Err(error) => {
            tracing::error!(
                target: "helios_riscv::vmm",
                addr = ptr as usize,
                size,
                ?error,
                "munmap failed"
            );
            EINVAL
        }
    }
}

extern "C" fn riscv_mprotect(ptr: *mut u8, size: usize, prot_flags: u32) -> c_int {
    let size = round_up_to_page(size);
    let address_space = user_as();
    let range = VirtRange::new(VirtAddr::new(ptr as usize), size);
    if prot_flags == 0 {
        return match address_space.decommit_committed_subranges(range) {
            Ok(()) => 0,
            Err(error) => {
                tracing::error!(
                    target: "helios_riscv::vmm",
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
                target: "helios_riscv::vmm",
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

/// Runtime custom-virtual-memory hook table for the riscv backend.
/// Address-space mutations route through the singleton
/// `RiscvUserAddressSpace`; COW image creation is opted out of
/// (`default_memory_image_new` returns `NULL`), so the runtime falls
/// back to per-instance memcpy initialization.
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

const _: () = {
    // Pin the C ABI so a runtime ABI revision that changes the
    // `RuntimeMemoryImage` opaque ptr type fails the build instead
    // of mismatching at link time.
    let _: extern "C" fn(*const u8, usize, &mut *mut runtime_memory::RuntimeMemoryImage) -> c_int =
        default_memory_image_new;
};

/// Whether two ranges share any byte.
///
/// A device mapping is claimed by range rather than by start address:
/// two windows that overlap without starting at the same place are
/// still the same registers reached twice, and the second claim would
/// silently take the first one's page-table entries.
fn ranges_overlap(left: VirtRange, right: VirtRange) -> bool {
    left.start.raw() < right.start.raw() + right.byte_len
        && right.start.raw() < left.start.raw() + left.byte_len
}
