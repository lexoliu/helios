//! Virtual address-space contract.
//!
//! The kernel and user address regions share a single virtual address
//! space today: kernel text and data are statically mapped at boot, MMIO
//! is direct-mapped, and user memory lives in a high virtual window
//! managed dynamically. Higher layers enforce their own
//! isolation above this page-table contract, so a per-user ASID is
//! intentionally not part of this contract.
//!
//! # Reservation / commit model
//!
//! The trait mirrors the same two-step reserve/commit model exposed by
//! conventional virtual-memory APIs:
//!
//! 1. `reserve(size)` carves out a contiguous virtual range with no
//!    physical backing. Accessing a reserved-but-uncommitted page
//!    faults; the backend's fault entry decides whether the page is
//!    swapped out (see [`AddressSpace::swapped_token`]) or the fault
//!    belongs to the runtime above.
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
use core::num::NonZeroU32;

use thiserror::Error;

use crate::device::DeviceRegion;
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
    #[error("this address space cannot swap pages out")]
    SwapUnsupported,
    #[error("page is not swapped out")]
    NotSwapped,
    #[error("page buffer is not exactly one frame")]
    BadPageBuffer,
    #[error("this address space cannot map device memory")]
    DeviceMappingUnsupported,
    #[error("the range is a device mapping, not ordinary memory")]
    DeviceMapped,
}

/// Identity of one swapped-out page's backing store.
///
/// The token is deliberately narrow: an address space keeps it in the
/// spare bits of the not-present page-table entry that replaces the
/// page, so the page table itself is the swap map and no side table has
/// to be consulted from trap context. Zero is not a valid token, which
/// keeps an all-zero descriptor meaning "nothing was ever here".
///
/// The value is an index into whatever the kernel's swap map holds; the
/// [`SwapBackend`]'s own token type never reaches the page table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SwapToken(NonZeroU32);

impl SwapToken {
    /// Bits an address space must be able to store to carry a token.
    pub const BITS: u32 = u32::BITS;

    pub const fn new(raw: u32) -> Option<Self> {
        match NonZeroU32::new(raw) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }

    pub const fn raw(self) -> u32 {
        self.0.get()
    }
}

/// How recently the hardware saw a committed page, as reported by
/// [`AddressSpace::scan_committed_pages`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageAge {
    /// The access flag was set: something touched the page since the
    /// last scan. Architectures with no access flag report every page
    /// this way, which stops aging from ranking pages it cannot rank.
    Hot,
    /// The access flag was clear across a whole scan interval.
    Cold,
}

/// Backend that can store decommitted user memory away from RAM and
/// reinstate it on demand. Implementations exist on top of any
/// persistent block-device-shaped resource: virtio-blk for the live
/// kernel, host files for `hosted/`, ramdisks for tests. The kernel's
/// page-fault path routes through this trait when the faulting address
/// carries a [`SwapToken`].
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

    /// Discard the storage behind `token` without reading it back.
    ///
    /// This is the path an instance's death takes: the pages it had
    /// swapped out are never coming back, and holding their extents
    /// would leak the swap device one dead instance at a time. Releasing
    /// a token that `swap_in` already consumed is not an error, so a
    /// caller racing a fault against a teardown does not have to
    /// serialise them.
    fn release<'a>(&'a self, token: Self::Token) -> impl Future<Output = ()> + Send + 'a;
}

/// Sentinel `SwapBackend` impl for kernels that do not yet expose a
/// persistent swap surface. Every operation reports
/// [`NoSwapError`]; callers should short-circuit to the OOM
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

    async fn swap_out(&self, _bytes: &[u8]) -> Result<Self::Token, Self::Error> {
        Err(NoSwapError)
    }

    async fn swap_in(&self, _token: Self::Token, _dst: &mut [u8]) -> Result<(), Self::Error> {
        Err(NoSwapError)
    }

    async fn release(&self, _token: Self::Token) {}
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

    /// Map `region`'s physical bytes at `virt`, which must be a
    /// sub-range of an existing reservation exactly as long as the
    /// region.
    ///
    /// No frame comes from this address space's pool: the bytes belong
    /// to a device and were never anyone's to allocate. The leaf entries
    /// carry the region's own attributes — a
    /// [`MemoryKind::Device`](crate::device::MemoryKind::Device) range
    /// is mapped so that accesses reach the device unmerged,
    /// unspeculated and in program order, and a region that is not
    /// writable is mapped read-only whatever `flags` ask for.
    ///
    /// Per the SMP contract the call invalidates the local TLB and
    /// shoots down every other processor that has run in this space
    /// before it returns, so no processor can reach the range through a
    /// stale translation.
    ///
    /// A backend that cannot express device memory in its page tables
    /// reports [`AddressSpaceError::DeviceMappingUnsupported`] rather
    /// than mapping the range as ordinary memory, because a register
    /// file behind a cacheable mapping is a silent corruption rather
    /// than a slower one.
    fn map_device(&self, _virt: VirtRange, _region: DeviceRegion) -> Result<(), AddressSpaceError> {
        Err(AddressSpaceError::DeviceMappingUnsupported)
    }

    /// Remove a mapping [`Self::map_device`] installed, leaving the
    /// range reserved and faulting.
    ///
    /// Nothing is returned to the frame pool — the frames were never
    /// taken from it — and the shootdown happens before the call
    /// returns, so the owner has provably lost its last path to the
    /// device's registers by the time the kernel reports the device
    /// free.
    fn unmap_device(&self, _virt: VirtRange) -> Result<(), AddressSpaceError> {
        Err(AddressSpaceError::DeviceMappingUnsupported)
    }

    /// Materialise physically contiguous backing for `virt` and report
    /// where it landed.
    ///
    /// [`Self::commit`] is free to satisfy a range out of whatever
    /// frames the pool has; a buffer a device reads by physical address
    /// is not, because the device walks it linearly and knows nothing
    /// about page tables. The returned frame is the first of
    /// `virt.frame_count()` consecutive ones, so the caller can hand the
    /// device one address and a length.
    ///
    /// `below` bounds the allocation: no frame of the run sits at or
    /// above it. A device that drives fewer than 64 address bits passes
    /// its limit here rather than discovering the truncation as
    /// corruption.
    fn commit_contiguous(
        &self,
        _virt: VirtRange,
        _flags: PageFlags,
        _below: u64,
    ) -> Result<PhysFrame, AddressSpaceError> {
        Err(AddressSpaceError::DeviceMappingUnsupported)
    }

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
    /// after the listed prerequisites are met, surfacing unsupported
    /// honestly rather than shipping a half-correct version that
    /// races TLBs or hands an upper layer a frame the page-fault
    /// handler does not own.
    fn relocate(&self, _virt: VirtRange) -> Result<(), AddressSpaceError> {
        Err(AddressSpaceError::InvalidFlags)
    }

    /// Detach the committed page at `addr` so its frame can be reused.
    ///
    /// The page's bytes are copied into `out` (exactly one frame long,
    /// caller-owned so the address space never hands a raw frame out),
    /// the leaf entry becomes a not-present entry carrying `token`, and
    /// the frame goes back to this address space's own pool. Per §3.4
    /// the call invalidates the local TLB and shoots down every other
    /// processor that has run in this space before it returns, so no
    /// processor can still reach the frame afterwards.
    ///
    /// The flags the page had are returned; [`Self::swap_in_page`]
    /// needs them to put the page back exactly as it was.
    fn swap_out_page(
        &self,
        _addr: VirtAddr,
        _token: SwapToken,
        _out: &mut [u8],
    ) -> Result<PageFlags, AddressSpaceError> {
        Err(AddressSpaceError::SwapUnsupported)
    }

    /// Put a swapped-out page back at `addr`.
    ///
    /// A fresh frame is taken from this address space's pool, filled
    /// from `bytes` through the kernel's own view of that frame, and
    /// only then mapped with the flags the page had when it left. The
    /// page therefore becomes readable at `addr` already complete: no
    /// processor can observe a half-restored page, which matters
    /// because the faulting processor is not the only one that may
    /// touch it.
    ///
    /// The flags come from the address space's own record of the page,
    /// so a caller putting a page back never has to remember them.
    ///
    /// Returns the token the entry carried, which the caller hands back
    /// to its [`SwapBackend`].
    fn swap_in_page(&self, _addr: VirtAddr, _bytes: &[u8]) -> Result<SwapToken, AddressSpaceError> {
        Err(AddressSpaceError::SwapUnsupported)
    }

    /// The swap token the entry at `addr` carries, or `None` when the
    /// page is not swapped out.
    ///
    /// Lock-free, because the page-fault path calls it from trap
    /// context where taking the address space's lock would deadlock
    /// against whichever processor is mutating the page table.
    fn swapped_token(&self, _addr: VirtAddr) -> Option<SwapToken> {
        None
    }

    /// Visit every committed page this address space holds for `owner`,
    /// reporting how recently the hardware saw it and clearing the
    /// access flag so the next scan measures a fresh interval.
    ///
    /// `visit` returns `false` to stop the scan early, which is how a
    /// swap-out pass keeps its batch bounded. The return value is the
    /// number of pages visited.
    ///
    /// `owner` is an opaque tag the kernel attached when the page was
    /// committed; this layer never interprets it.
    fn scan_committed_pages<Visit>(&self, _owner: u64, _visit: Visit) -> usize
    where
        Visit: FnMut(VirtAddr, PageFlags, PageAge) -> bool,
    {
        0
    }

    /// Bytes this address space currently has committed for `owner`.
    fn owned_resident_bytes(&self, _owner: u64) -> u64 {
        0
    }

    /// Take the swap tokens this address space dropped on its own.
    ///
    /// `release`, `decommit` and a `commit` that lands on top of a
    /// swapped page all discard entries whose backing store is still
    /// held by a [`SwapBackend`]. Those tokens are queued here rather
    /// than released inline, because the address space runs under a
    /// spinlock and releasing is asynchronous. The kernel drains the
    /// queue from a task and hands each token to the backend.
    ///
    /// Returns the number of tokens visited.
    fn drain_orphaned_swap_tokens<Visit>(&self, _visit: Visit) -> usize
    where
        Visit: FnMut(SwapToken),
    {
        0
    }
}
