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
use core::future::Future;

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

/// Backend that can store decommitted user memory away from RAM and
/// reinstate it on demand. Implementations exist on top of any
/// persistent block-device-shaped resource: virtio-blk for the live
/// kernel, host files for `hosted/`, ramdisks for tests. The kernel
/// page-fault handler routes [`PageFaultOutcome::Resolved`] through
/// this trait when the faulting address lies in a swap-out region.
pub trait SwapBackend: Send + Sync + 'static {
    /// Token returned from a successful `swap_out`; opaque to
    /// callers, owned by the implementation. Backing the same range
    /// in again uses the token to find the data.
    type Token: Copy + Send + Sync + 'static;

    /// Reasons a swap operation can fail.
    type Error: core::fmt::Display + core::fmt::Debug + Send + Sync + 'static;

    /// Persist `bytes` away from RAM and return a token that can
    /// later resurrect them via `swap_in`. The implementation owns
    /// the lifecycle of the storage and decides eviction order on
    /// its own.
    fn swap_out<'a>(
        &'a self,
        bytes: &'a [u8],
    ) -> impl Future<Output = Result<Self::Token, Self::Error>> + Send + 'a;

    /// Restore previously swapped-out bytes into `dst`. After this
    /// returns successfully, the token is no longer valid (the
    /// storage may be reclaimed).
    fn swap_in<'a>(
        &'a self,
        token: Self::Token,
        dst: &'a mut [u8],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'a;
}

/// Sentinel `SwapBackend` impl for kernels that do not yet expose a
/// persistent swap surface. Every operation reports
/// [`NoSwap::Unsupported`]; callers should short-circuit to the OOM
/// killer instead. Provided so kernel code can be generic over a
/// `SwapBackend` even before virtio-blk swap lands.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSwap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("swap backend is not configured on this platform")]
pub struct NoSwapError;

impl SwapBackend for NoSwap {
    type Token = ();
    type Error = NoSwapError;

    fn swap_out<'a>(
        &'a self,
        _bytes: &'a [u8],
    ) -> impl Future<Output = Result<Self::Token, Self::Error>> + Send + 'a {
        async { Err(NoSwapError) }
    }

    fn swap_in<'a>(
        &'a self,
        _token: Self::Token,
        _dst: &'a mut [u8],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'a {
        async { Err(NoSwapError) }
    }
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

    /// Relocate the physical backing of an already-committed range
    /// without changing its virtual base address.
    ///
    /// A complete bare-metal implementation must:
    ///
    /// 1. Walk the page table and snapshot the current PTE flags
    ///    plus physical frame for every committed page in `virt`.
    /// 2. Allocate a fresh contiguous physical run from the
    ///    backend's PMM (not the kernel global allocator — those
    ///    frames are unconstrained by the user-pool free list and
    ///    may not even be reachable through the HHDM the backend
    ///    expects). The required primitive is a per-platform
    ///    `PhysFrameAllocator::allocate(count, zero_first_use=false)`
    ///    that returns a contiguous `PhysFrameRange`.
    /// 3. Copy bytes through the kernel data view of both the source
    ///    and destination frames (HHDM on x86/aarch64,
    ///    identity-map on riscv).
    /// 4. Atomically swap each PTE's PPN field, preserving the
    ///    flags captured in step 1.
    /// 5. Issue a TLB shootdown for every covered virtual page on
    ///    every processor that may have run in this AS — the SMP-
    ///    correctness requirement in AGENTS §3.4. AArch64 broadcast
    ///    `tlbi vaale1is` already fans out via the inner-shareable
    ///    domain; x86 needs an IPI shootdown protocol on top of the
    ///    wake-IPI infrastructure that landed in `e1fad4d`; riscv
    ///    needs `remote_sfence_vma` SBI calls.
    /// 6. Return the old `PhysFrameRange` to the PMM.
    /// 7. Roll back partial work on failure: every successfully
    ///    relocated page in this call must be reverted (rare
    ///    error path, but `OutOfFrames` mid-range is recoverable).
    ///
    /// The default impl reports unsupported. Backends opt in only
    /// after the listed prerequisites are met; today none do, and
    /// surface that honestly rather than ship a half-correct version
    /// that races TLBs or hands wasmtime a frame the page-fault
    /// handler does not own.
    fn relocate(&self, _virt: VirtRange) -> Result<(), AddressSpaceError> {
        Err(AddressSpaceError::InvalidFlags)
    }
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
