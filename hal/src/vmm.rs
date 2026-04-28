//! Virtual address-space contract.
//!
//! The kernel and every user-mode wasm guest share a single virtual
//! address space today: kernel text and data are statically mapped at
//! boot, MMIO is direct-mapped, and user memory lives in a high virtual
//! window managed dynamically. WebAssembly sandboxing is enforced at the
//! bytecode layer, not at the page-table layer, so a per-guest ASID is
//! intentionally not part of this contract.
//!
//! # Reservation / commit model
//!
//! The trait mirrors the same two-step model that POSIX `mmap` and
//! Wasmtime's custom virtual-memory ABI both expose:
//!
//! 1. `reserve(size)` carves out a contiguous virtual range with no
//!    physical backing. Accessing a reserved-but-uncommitted page
//!    faults; the kernel page-fault handler decides whether to commit
//!    on demand, swap in, or report a guard-page trap.
//! 2. `commit(range, flags)` materialises physical frames behind a
//!    sub-range and grants the requested permissions.
//! 3. `decommit(range)` releases the physical frames; the virtual range
//!    stays reserved and faulting again triggers re-commit.
//! 4. `release(range)` drops the reservation entirely.
//!
//! The AS owns its physical-frame source and never exposes raw frames
//! to callers, so the OOM killer / supervisor can release a victim's
//! whole reservation in one call without bookkeeping leaks.
//!
//! # SMP contract
//!
//! All methods take `&self` and are safe to call from any processor.
//! Mutating operations hold a per-AS spinlock over the page-table walk,
//! then issue a TLB shootdown IPI to every other configured processor
//! before returning. After a call returns, no processor has a stale TLB
//! entry for the affected range.
//!
//! `translate` is lock-free; the answer is a snapshot.

use bitflags::bitflags;
use thiserror::Error;

use crate::cpu::ProcessorId;
use crate::pmm::PhysFrame;

/// Virtual address. No alignment guarantees; ranges check
/// [`VirtRange::is_page_aligned`] where required.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VirtAddr(usize);

impl VirtAddr {
    pub const fn new(addr: usize) -> Self {
        Self(addr)
    }

    pub const fn raw(self) -> usize {
        self.0
    }

    pub const fn is_page_aligned(self) -> bool {
        self.0.is_multiple_of(PhysFrame::SIZE)
    }

    pub const fn page_floor(self) -> Self {
        Self(self.0 & !(PhysFrame::SIZE - 1))
    }

    pub const fn saturating_add(self, bytes: usize) -> Self {
        Self(self.0.saturating_add(bytes))
    }
}

/// Half-open virtual range `[start, start + byte_len)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtRange {
    pub start: VirtAddr,
    pub byte_len: usize,
}

impl VirtRange {
    pub const fn new(start: VirtAddr, byte_len: usize) -> Self {
        Self { start, byte_len }
    }

    pub const fn end(self) -> VirtAddr {
        self.start.saturating_add(self.byte_len)
    }

    pub const fn frame_count(self) -> usize {
        self.byte_len.div_ceil(PhysFrame::SIZE)
    }

    pub const fn is_page_aligned(self) -> bool {
        self.start.is_page_aligned() && self.byte_len.is_multiple_of(PhysFrame::SIZE)
    }

    pub fn contains(self, addr: VirtAddr) -> bool {
        addr.raw() >= self.start.raw() && addr.raw() < self.end().raw()
    }
}

bitflags! {
    /// Permission flags for committed pages. Reserved-only pages have
    /// no flags (they always fault on access).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct PageFlags: u32 {
        const READ    = 1 << 0;
        const WRITE   = 1 << 1;
        const EXECUTE = 1 << 2;
    }
}

/// Outcome of [`AddressSpace::translate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Translation {
    /// Page is committed and accessible with the listed flags.
    Committed { frame: PhysFrame, flags: PageFlags },
    /// Page is in a reservation but no physical backing is mapped.
    /// Access would fault and route to the page-fault handler.
    Reserved,
    /// Page is not in any reservation owned by this address space.
    Unmapped,
}

/// Failures from [`AddressSpace`] mutating operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum AddressSpaceError {
    #[error("range is not page-aligned")]
    Misaligned,
    #[error("range is empty")]
    EmptyRange,
    #[error("reservation overlaps an existing one")]
    Overlap,
    #[error("range is not within an existing reservation")]
    NotReserved,
    #[error("range is reserved but not committed")]
    NotCommitted,
    #[error("backing physical-frame pool is exhausted")]
    OutOfFrames,
    #[error("page-table backing storage is exhausted")]
    PageTableExhausted,
    #[error("requested flag combination is invalid for this architecture")]
    InvalidFlags,
}

/// Per-platform virtual address space. See module docs for semantics.
pub trait AddressSpace: Send + Sync + 'static {
    /// Carve out a fresh reservation of `byte_len` bytes. Pages in the
    /// returned range are inaccessible until [`Self::commit`] runs.
    /// The implementation picks the virtual address; callers should not
    /// assume anything about its location.
    fn reserve(&self, byte_len: usize) -> Result<VirtRange, AddressSpaceError>;

    /// Drop a reservation. Any committed pages inside are decommitted
    /// first, returning their frames to the AS-internal pool.
    fn release(&self, virt: VirtRange) -> Result<(), AddressSpaceError>;

    /// Materialise physical backing for `virt` (which must be a
    /// sub-range of an existing reservation) with the requested flags.
    fn commit(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError>;

    /// Release the physical backing for `virt` while keeping the
    /// reservation intact. Subsequent accesses fault into the
    /// page-fault handler.
    fn decommit(&self, virt: VirtRange) -> Result<(), AddressSpaceError>;

    /// Change permissions on already-committed pages. No frame churn.
    fn protect(&self, virt: VirtRange, flags: PageFlags) -> Result<(), AddressSpaceError>;

    /// Look up the current state of `addr`. Lock-free.
    fn translate(&self, addr: VirtAddr) -> Translation;
}

/// Outcome of a kernel-supplied page-fault handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageFaultOutcome {
    /// Kernel installed (or restored) a mapping covering the faulting
    /// address. The faulting instruction should be retried.
    Resolved,
    /// Fault is not the kernel VM's responsibility — for example a
    /// wasm bounds-check trap into a guard region. The backend should
    /// fall through to its language-runtime trap handler (Wasmtime).
    NotOurs,
    /// Fault is fatal: no reservation, no policy. The backend should
    /// terminate the offending instance, or panic the kernel if no
    /// instance is responsible.
    Fatal,
}

/// Page-fault handler installed once at boot by `kernel/src/user_memory`.
///
/// The handler is a function pointer (no `dyn`) so backends store it in
/// a static and call it directly from architecture-specific trap entry
/// without indirection through the kernel async layer.
pub type PageFaultHandler = fn(
    faulting_addr: VirtAddr,
    instruction_pointer: usize,
    fault_processor: ProcessorId,
) -> PageFaultOutcome;
