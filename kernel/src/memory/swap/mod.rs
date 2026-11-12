//! Swap: the map from a page-table entry to its backing store, the
//! policy that decides what leaves memory, and the path a page takes
//! back in.
//!
//! # Shape
//!
//! A swapped-out page is in one of three tiers, tried in this order when
//! the page leaves memory:
//!
//! 1. **Zero.** A page of zeros has no storage at all: its slot says so.
//! 2. **The compressed store** ([`store`]): the page, LZ4-compressed, in a
//!    frame of the user pool shared with other compressed pages. A page
//!    takes it when it compresses to the policy's bound and the store is
//!    under its limit.
//! 3. **The swap device**, when one is attached: the compressed block if
//!    the page compressed well enough but the store was full, the raw page
//!    otherwise.
//!
//! A page that none of them takes goes back into memory. A page whose
//! device write is refused is parked, raw, in the store's spare frame, so a
//! detached page always has somewhere to go that needs no allocation and no
//! device.
//!
//! The store drains to the disk, and packs itself, on the same owner task:
//!
//! - **Writeback** works the store's frames oldest first, whole frames at a
//!   time, writing each object to the disk still compressed (a parked page
//!   raw). It runs when the store is over its limit, or refusing objects
//!   under its hysteresis, at any pressure, down to the accept level, and
//!   when pressure is still `Red` after a pass's eviction and compaction,
//!   with what is left of the pass's batch.
//! - **Compaction** runs at `Yellow` and above, when the store's frames waste
//!   a quarter of their granules, and moves objects out of the sparsest
//!   frames into the holes of others so that the frames they empty go back to
//!   the pool. It takes the claim on each object it moves, as a fault does.
//!
//! Both are bounded by the batch the eviction pass has left, and both stop
//! at the first fault queued for the owner.
//!
//! What a page-table entry's token names is a slot of the shared [`slot`]
//! table, one atomic word per swapped page that says which tier holds its
//! bytes. The claim protocol and its concurrency contract are in that
//! module.
//!
//! **One task owns eviction.** It holds the optional disk, the table of
//! backend tokens of the pages the disk holds, the store's allocator, and
//! the single page-sized staging buffer that every transfer goes through,
//! and it is the only thing that ever talks to the swap device. Callers
//! reach it through [`SwapHandle`]: a teardown pushes tokens to release
//! and does not wait, and a fault on a disk page pushes a request and waits.
//!
//! **A fault on a page the RAM tiers hold is resolved where it is taken**,
//! on the faulting processor, with no queue: it claims the slot word,
//! decompresses straight into the frame the address space is about to map,
//! and reinstates it ([`SwapHandle::fault_in`]). Compression is CPU work
//! with no device behind it, so there is nothing for an owner task to
//! serialise.
//!
//! Serialising device access through one task is deliberate. There is one
//! scratch disk, the transfers are page-sized, and a single owner means
//! the backend tokens need no lock and the staging buffer needs no
//! allocation per fault.
//!
//! # Concurrency contract
//!
//! | Operation | Where | Synchronisation |
//! | --- | --- | --- |
//! | Fault on a zero, stored or parked page | Faulting processor | Lock-free claim of the slot word; the address space's reservation lock inside `swap_in_page`. No await between claim and release. |
//! | Fault on a disk page | Owner task, through the queue | Owner-only state |
//! | Wait on a reserved or claimed slot | Faulting processor | [`ProgressSignal`]: mark before the test, then wait for a change |
//! | Eviction, compression, store allocation, orphan release, store drain | Owner task | Owner-only state, plus claims on slot words |
//! | Writeback, compaction | Owner task | A claim on each object's slot word, held across the device write (writeback) or the copy (compaction); a fault on that slot waits on [`ProgressSignal`] |
//! | [`SwapHandle::stats`] | Any | Relaxed sums of per-processor counters |
//! | [`SwapHandle::set_policy`] | Any | Relaxed stores to the settings |
//!
//! Nothing here is reachable from an interrupt handler or from trap
//! context: the fault entry classifies the address and redirects to the
//! trampoline, and the resolution runs on the fiber afterwards.
//!
//! # Who blocks where
//!
//! A guest touching a swapped-out page traps into the backend's fault
//! entry, which redirects the faulting fiber onto a kernel trampoline
//! running on that fiber's own stack. The trampoline blocks the fiber
//! the way an async host call does, so the executor keeps running and
//! this task — which may be on the very same processor — gets to do the
//! read. When the read lands the fiber is resumed and the faulting
//! instruction is retried; the runtime above never learns a fault
//! happened.
//!
//! Kernel code is not on a fiber and has no trampoline. **Any kernel
//! path that reads or writes user memory directly must call
//! [`SwapHandle::ensure_present`] on the range first.** A fault taken
//! outside a fiber is fatal, and it is fatal on purpose: silently
//! resolving it would need a nested executor.
//!
//! The matching half of that contract is enforced here rather than at
//! every host call: **a page is only ever detached from an instance
//! that is not on a processor.** An executing instance may be inside a
//! host call, and that frame already holds its fiber's blocking
//! context, so the trampoline could not block again to fault a page
//! back in. Both eviction phases below take that bar; the difference
//! between them is how long the instance has to have been off.
//!
//! The bar holds at the moment of each detach, not at the moment a
//! victim was picked. A pass picks its victims once and then awaits the
//! device for every page, and any victim may begin a turn during one of
//! those awaits. So the only way to a detach is
//! [`SwapVmHooks::detach_page`], which takes the
//! [`OffProcessor`] proof that
//! [`InstanceRegistry::while_off_processor`] lends: the page's instance
//! had no store on a processor when the hold was granted, and none can
//! begin a turn until the detach — leaf, shootdown and copy — is done.
//! A victim that is running when its next page comes up refuses the
//! hold, and the pass leaves it.
//!
//! "On a processor" is a store's processor turn (`crate::StoreTurns`):
//! the poll of the future that drives its guest, synchronous host calls
//! included. A fiber stopped in the trampoline has suspended, so its
//! instance is off a processor while the page is read back, and that is
//! safe without accounting of its own: the policy pass does not run
//! while a fault is queued, the reinstated page comes back hot, and a
//! retried instruction that meets another evicted page blocks in the
//! trampoline again — it is running wasm, which holds no blocking
//! context.
//!
//! Two more properties keep a page that has just come back from being
//! taken straight out again. A reinstated page is mapped with its
//! access flag set, so it reads as hot for a full aging cycle and both
//! phases — which take only cold pages — pass over it; and an
//! instance that has just taken a fault or made a host call is by
//! definition not idle for the policy's `idle_after`, so the `Yellow`
//! phase has no claim on it either. That is what makes
//! [`SwapHandle::ensure_present`] a sound contract for kernel paths:
//! the pages it faults in stay in until the host call that asked for
//! them has finished with them.
//!
//! # Order against the balloon
//!
//! Both this and the memory balloon (`super::balloon`) read the same
//! [`PressureLevel`]. Swap acts first: at `Yellow` it evicts the cold
//! pages of idle instances, which costs a fault later, while the balloon at
//! that point only reports free pages to the host. At `Red` the balloon is
//! already forbidden to inflate past its floor and deflates on demand from
//! the out-of-memory path, so the ordering under pressure is: give the host
//! back nothing, swap cold pages out, and let the out-of-memory path act
//! when a grow still fails. That path (`wasmtime_adapter::store`) acts on a
//! failed grow independently of swap; a swap pass is one way memory comes
//! free before it does, not a stage the killer waits on.
//!
//! Frames the compressed store holds are allocated from the same pool the
//! balloon and free-page reporting draw on, so neither can reach them:
//! compression only ever shows as more free frames.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};
use core::time::Duration;

use concurrent_queue::ConcurrentQueue;
use helios_hal::cpu::{Cpu, Timebase};
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{
    AddressSpaceError, PageAge, PageFlags, SwapBackend, SwapToken, VirtAddr, VirtRange,
};
use lz4_flex::block::{CompressTable, compress_into_with_table, decompress_into};
use slab::Slab;
use spin::Once;
use thiserror::Error;

use crate::exec::Spawner;
use crate::exec::{PressureLevel, ProgressMark, ProgressSignal, monotonic_nanos, yield_now};
use crate::instance::{InstanceId, InstanceRegistry, OffProcessor};
use crate::{Notify, Timer};

use super::owner::MemoryOwner;
use super::user::user_heap_stats;

mod counters;
mod hooks;
mod leaf;
mod policy;
mod slot;
mod space;
mod stack;
mod store;

use counters::{Counters, FaultSource, Held, Placement, Totals};
use policy::PolicyCells;
use slot::{Claim, DiskKey, SlotAllocator, SlotIndex, SlotState, SlotTable, StoreFrameIndex};
use store::{Limits, Store, StoreFrames, StoreFull};

pub use hooks::{InstalledAddressSpace, swap_vm_hooks};
pub use leaf::{AGE_BATCH_PAGES, SwapLeafFormat};
pub use policy::{SwapPolicy, SwapPolicyError};
pub use space::SwapSpace;

/// Bytes one pass moves before it re-reads the pressure level. A pass
/// holds no lock across its transfers, but it does hold the swap device,
/// so it stops often enough for a fault to overtake it.
const SWAP_BATCH_BYTES: usize = 2 * 1024 * 1024;

/// How often the policy re-reads pressure when nothing else wakes it.
pub const SWAP_TICK: Duration = Duration::from_millis(500);

/// How often the task reports what it has moved.
///
/// Swap is a per-page activity and a guest walking a swapped-out range
/// takes thousands of faults in a row, so a line per page — or even per
/// burst — is a flood on the one serial console the machine has. One
/// summary a second keeps the fact that swap is working visible without
/// crowding out everything else; the exact numbers live in the stats
/// record.
const SWAP_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Pages one pass will look at before giving up, however few of them it
/// found worth taking.
///
/// Without this a pass over a resident set with nothing cold in it walks
/// every page, clearing access flags and invalidating translations the
/// whole way, every tick — which costs far more than the memory it is
/// trying to reclaim. Four times the take budget leaves room to skip
/// hot pages and still fill a batch.
const SCAN_BUDGET_PAGES: usize = 4 * (SWAP_BATCH_BYTES / PhysFrame::SIZE);

/// Why a platform runs without swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum SwapDisabled {
    #[error(
        "backend installs no swap hooks, so a page has no not-present encoding to carry its \
         swap token"
    )]
    NoSwapHooks,
    #[error("machine gave the kernel no scratch block device to swap to")]
    NoSwapDevice,
}

/// Why a swap-in did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum SwapFaultError {
    #[error("no swap backend is configured on this platform")]
    NotConfigured,
    #[error("the swap backend could not read the page back")]
    Backend,
    #[error("the user pool had no frame to bring the page back into")]
    OutOfFrames,
    #[error("the address space refused to reinstate the page")]
    AddressSpace,
    #[error("the page's contents were lost: neither memory nor the swap backend holds them")]
    PageLost,
}

/// What swap is holding, as observers see it.
///
/// Gauges say what is held now; every other count is cumulative since boot.
/// What each quantity of interest is made of: the compression ratio is
/// `store.pages / store.frames`, the layout's efficiency is
/// `store.payload_bytes / (store.frames * 4096)`, and the store's occupancy
/// against its limit is `store.frames * 4096 / store.limit_bytes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapStats {
    /// The swap device, absent while none is attached.
    pub disk: Option<SwapDiskStats>,
    pub zero: SwapZeroStats,
    pub store: SwapStoreStats,
    pub faults: SwapFaultStats,
    /// Pages detached into a tier. A page put back is not counted. Net
    /// reclaim is this less `store.frames` less the pages faulted back.
    pub frames_released: u64,
    /// The settings in force.
    pub policy: SwapPolicy,
}

/// What the swap device holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapDiskStats {
    /// Name of the backing store, for the stats panel.
    pub backend: &'static str,
    /// Bytes the backing store can hold in total.
    pub capacity_bytes: u64,
    /// Gauge: bytes handed to the backend and still held.
    pub stored_bytes: u64,
    /// Gauge: pages on the device.
    pub pages: u64,
    /// Pages written out as they were.
    pub pages_out_raw: u64,
    /// Pages written out as LZ4 blocks.
    pub pages_out_compressed: u64,
    /// Objects moved from the store to the device.
    pub written_back: u64,
    /// Gauge: pages whose bytes no tier holds any more.
    pub pages_lost: u64,
}

/// Pages that were all zeros, which take no storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapZeroStats {
    /// Gauge: zero pages swapped out.
    pub pages: u64,
    pub pages_out: u64,
}

/// The compressed store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapStoreStats {
    /// The most the store may take of the pool now, in bytes.
    pub limit_bytes: u64,
    /// Gauge: user-pool frames the store holds, the spare included.
    pub frames: u64,
    /// Gauge: compressed pages.
    pub pages: u64,
    /// Gauge: bytes of their LZ4 blocks.
    pub payload_bytes: u64,
    /// Gauge: raw pages parked after a placement failed.
    pub parked_pages: u64,
    pub pages_out: u64,
    /// Pages that did not compress to the policy's bound.
    pub rejected_incompressible: u64,
    /// Compressible pages the store refused for its limit.
    pub rejected_full: u64,
    /// Pages no tier took, which stayed in memory.
    pub put_back: u64,
    /// Frames compaction emptied.
    pub compacted_frames: u64,
}

/// Faults by the tier that resolved them. Latency runs from the fault's
/// issue to its resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapFaultStats {
    pub zero: SwapFaultSource,
    pub store: SwapFaultSource,
    pub parked: SwapFaultSource,
    pub disk: SwapFaultSource,
    /// Faults that found their slot held by another path and waited.
    pub contended_waits: u64,
}

/// Faults one tier resolved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwapFaultSource {
    pub count: u64,
    /// Mean time a faulting guest waited, over `count` faults.
    pub mean_wait_nanos: u64,
}

/// The platform address space's swap surface, as plain function
/// pointers.
///
/// Same shape and reason as `crate::runtime_memory`'s hook table: the
/// kernel is not generic over the address space (there is exactly one
/// per machine, chosen at link time), and a `dyn AddressSpace` would put
/// a vtable on the page-fault path. The backend generates one `&'static`
/// table with [`swap_vm_hooks`] and installs it at boot.
///
/// The enumeration hooks take a context pointer and a plain function
/// rather than a closure, because a function pointer cannot be generic.
///
/// The two visiting hooks take their callback and an opaque context
/// pointer rather than a closure for the same reason: the caller keeps
/// its state on its own stack and the hook only has to be able to reach
/// it.
pub struct SwapVmHooks {
    /// Detach one committed page, copying it into the caller's buffer.
    /// Reached only through [`Self::detach_page`], which asks for the
    /// proof that the page's instance is off every processor.
    pub(crate) swap_out_page:
        fn(VirtAddr, SwapToken, &mut [u8]) -> Result<PageFlags, AddressSpaceError>,
    /// Reinstate one page, but only while its entry carries the given
    /// token. The address space allocates the frame and calls the fill
    /// with the caller's context on it before it takes its lock; see
    /// [`AddressSpace::swap_in_page`](helios_hal::vmm::AddressSpace::swap_in_page).
    pub swap_in_page: fn(VirtAddr, SwapToken, *mut (), PageFill) -> Result<(), AddressSpaceError>,
    /// The token the entry at this address carries. Lock-free; the
    /// fault entry calls it from trap context.
    pub swapped_token: fn(VirtAddr) -> Option<SwapToken>,
    /// Visit an owner's committed pages with their age, clearing the
    /// access flag as it goes. `visit` returns `false` to stop.
    pub scan_committed_pages: fn(u64, *mut (), CommittedPageVisitor) -> usize,
    /// Bytes this owner has committed right now.
    pub owned_resident_bytes: fn(u64) -> u64,
    /// Take tokens the address space dropped when a range was released,
    /// decommitted, or committed over.
    pub drain_orphaned_swap_tokens: fn(*mut (), OrphanedTokenVisitor) -> usize,
    /// The platform's monotonic tick counter, the one `Cpu::now` reads.
    /// A fault is stamped with it where it is issued, which is the
    /// fault entry and the pre-fault accessor: neither holds a `Cpu`, and
    /// the handle they reach swap through cannot be generic over one.
    pub now_ticks: fn() -> u64,
}

impl SwapVmHooks {
    /// Detach the committed page at `addr`, which belongs to the instance
    /// `off` holds off every processor, leaving `token` in its entry and
    /// its bytes in `out`.
    ///
    /// The proof is what keeps a page from leaving an instance that may
    /// be inside a host call (see the module docs); every path that takes
    /// a page out of memory, whatever it does with the bytes afterwards,
    /// comes through here.
    pub fn detach_page(
        &self,
        _off: &OffProcessor<'_>,
        addr: VirtAddr,
        token: SwapToken,
        out: &mut [u8],
    ) -> Result<PageFlags, AddressSpaceError> {
        (self.swap_out_page)(addr, token, out)
    }
}

/// What [`SwapVmHooks::scan_committed_pages`] calls for each page it
/// walks: the caller's context, the page, its flags and its age.
/// Returning `false` ends the scan.
pub type CommittedPageVisitor = fn(*mut (), VirtAddr, PageFlags, PageAge) -> bool;

/// What [`SwapVmHooks::swap_in_page`] calls to write the page's bytes into
/// the frame it is about to map: the caller's context and the frame as
/// one page. It must write every byte.
pub type PageFill = fn(*mut (), &mut [u8]);

/// What [`SwapVmHooks::drain_orphaned_swap_tokens`] calls for each token
/// the address space dropped: the caller's context and the token.
pub type OrphanedTokenVisitor = fn(*mut (), SwapToken);

/// A shared view of swap for the fault path, teardown, and observers.
#[derive(Clone)]
pub struct SwapHandle {
    shared: Arc<SwapShared>,
}

struct SwapShared {
    /// Faults waiting for a page. Bounded, because an unbounded queue
    /// here would mean an unbounded number of blocked fibers.
    faults: ConcurrentQueue<SwapFaultRequest>,
    /// Wakes the swap service. One consumer, level-triggered: a raise
    /// means "there is a fault queued or the free-memory picture moved",
    /// and the service re-reads both when it wakes, so one pending wake
    /// stands for however many raises landed while it was working.
    /// `notify_all` is the wrong primitive here — it banks permits for
    /// waits that have not happened yet, so the service's next park
    /// would return immediately and its loop would never yield.
    work: Notify,
    /// One word per swapped-out page: where its bytes are. Shared, with
    /// the concurrency contract in [`slot`].
    slots: SlotTable,
    /// The compressed store's frames: shared so a fault on any processor
    /// can read and free an object. The allocator is the owner's.
    store: StoreFrames,
    /// Raised whenever a slot a fault may be waiting on is published, put
    /// back or freed, and whenever the owner finishes placing a page.
    released: ProgressSignal,
    policy: PolicyCells,
    counters: Counters,
    /// The address space's swap surface, which the fault path reaches
    /// without the owner.
    hooks: &'static SwapVmHooks,
    backend: &'static str,
    capacity_bytes: u64,
    /// Reads the time a fault is issued and the time it completes, so
    /// both ends of its latency come from one clock.
    clock: SwapClock,
}

/// The clock fault latency is measured on: the platform's tick counter and
/// the timebase that converts it, the same pair `monotonic_nanos` reads
/// through a `Cpu`.
#[derive(Clone, Copy)]
struct SwapClock {
    timebase: Timebase,
    now_ticks: fn() -> u64,
}

impl SwapClock {
    fn now_nanos(&self) -> u64 {
        self.timebase.ticks_to_nanos((self.now_ticks)())
    }
}

struct SwapFaultRequest {
    addr: VirtAddr,
    reply: Arc<SwapFaultReply>,
    /// When the fault was issued, on [`SwapShared::clock`]. Its latency is
    /// measured from here, so time spent queued counts.
    issued_nanos: u64,
}

const FAULT_PENDING: u8 = 0;
const FAULT_RESOLVED: u8 = 1;
const FAULT_OUT_OF_FRAMES: u8 = 2;
const FAULT_BACKEND: u8 = 3;
const FAULT_ADDRESS_SPACE: u8 = 4;
const FAULT_PAGE_LOST: u8 = 5;

struct SwapFaultReply {
    ready: Notify,
    outcome: AtomicU8,
}

impl SwapFaultReply {
    fn new() -> Self {
        Self {
            ready: Notify::new(),
            outcome: AtomicU8::new(FAULT_PENDING),
        }
    }

    /// Publishes the outcome and releases the faulting fiber.
    ///
    /// A reply has exactly one consumer — the fiber parked in
    /// [`SwapHandle::fault_in`] — so this is a level-triggered
    /// single-consumer wake, not a broadcast: the permit is stored
    /// whether or not that fiber has parked yet, and the settled
    /// outcome is what the fiber re-reads when it wakes.
    fn settle(&self, outcome: u8) {
        self.outcome.store(outcome, Ordering::Release);
        self.ready.notify_one_coalesced();
    }

    fn read(&self) -> Option<Result<(), SwapFaultError>> {
        match self.outcome.load(Ordering::Acquire) {
            FAULT_PENDING => None,
            FAULT_RESOLVED => Some(Ok(())),
            FAULT_OUT_OF_FRAMES => Some(Err(SwapFaultError::OutOfFrames)),
            FAULT_BACKEND => Some(Err(SwapFaultError::Backend)),
            FAULT_ADDRESS_SPACE => Some(Err(SwapFaultError::AddressSpace)),
            FAULT_PAGE_LOST => Some(Err(SwapFaultError::PageLost)),
            other => panic!("swap fault reply carried an unknown outcome {other}"),
        }
    }
}

/// How many faults can be in flight before the queue refuses one. Each
/// entry is one blocked fiber, and a machine with more blocked fibers
/// than this has a problem swap cannot fix.
const MAX_INFLIGHT_FAULTS: usize = 256;

/// What one look at a faulting page's slot led to.
enum Step {
    /// The page is present, or no entry names a swap slot any more.
    Done,
    /// Something moved under the look: read the entry again.
    Again,
    /// The slot is held by another path. Waits for a change after the mark,
    /// which was taken before the slot was read.
    Wait(ProgressMark),
    /// The page is on the disk, which only the owner reads. The token is the
    /// entry's, and the key is where the slot says its backend token is.
    Disk(SwapToken, DiskKey),
    Failed(SwapFaultError),
}

/// What the fill of a RAM-tier reinstatement copies or decodes from. Built
/// on the faulting processor around the claim it holds.
struct RamFill<'a> {
    frames: &'a StoreFrames,
    claim: &'a Claim,
    state: SlotState,
}

impl RamFill<'_> {
    /// Writes the page's bytes into `page`, which is one frame.
    ///
    /// # Panics
    ///
    /// If a stored object does not decode to exactly one page. The kernel is
    /// the only writer of the store, so a block that fails to decode, or
    /// decodes short, means the store's bookkeeping or its memory is
    /// corrupt, which is never masked.
    fn write(&self, page: &mut [u8]) {
        match self.state {
            SlotState::Zero => page.fill(0),
            SlotState::Stored(object) => {
                let block = self.frames.payload(object, self.claim);
                match decompress_into(block, page) {
                    Ok(len) if len == PhysFrame::SIZE => {}
                    outcome => panic!(
                        "swap slot {} holds a store object (frame {}, granule {}, {} bytes) that \
                         decodes to {outcome:?} instead of one page",
                        self.claim.index().raw(),
                        object.frame().raw(),
                        object.first().raw(),
                        object.len().get(),
                    ),
                }
            }
            SlotState::Parked(frame) => {
                page.copy_from_slice(self.frames.parked_page(frame, self.claim));
            }
            other => unreachable!("a RAM-tier fill was built for the settled state {other:?}"),
        }
    }
}

impl SwapShared {
    /// Ends a claim by putting a settled word back, and tells the faults
    /// waiting on the slot.
    fn restore(&self, claim: Claim, state: SlotState) {
        self.slots.publish(claim, state);
        self.released.signal();
    }

    /// Ends a claim by giving the slot's index back, and tells the faults
    /// waiting on it.
    fn release(&self, claim: Claim) {
        self.slots.free(claim);
        self.released.signal();
    }
}

impl SwapHandle {
    fn new(
        backend: &'static str,
        capacity_bytes: u64,
        clock: SwapClock,
        hooks: &'static SwapVmHooks,
        processors: usize,
    ) -> Self {
        // The store can never hold more frames than the pool has.
        let pool_frames = user_heap_stats().total_bytes / PhysFrame::SIZE;
        Self {
            shared: Arc::new(SwapShared {
                faults: ConcurrentQueue::bounded(MAX_INFLIGHT_FAULTS),
                work: Notify::new(),
                slots: SlotTable::new(),
                store: StoreFrames::new(pool_frames),
                released: ProgressSignal::new(),
                policy: PolicyCells::new(SwapPolicy::default())
                    .expect("the default policy is within its ranges"),
                counters: Counters::new(processors),
                hooks,
                backend,
                capacity_bytes,
                clock,
            }),
        }
    }

    /// What swap is holding right now.
    pub fn stats(&self) -> SwapStats {
        let shared = &*self.shared;
        let totals = shared.counters.totals();
        let policy = shared.policy.load();
        let source = |source: FaultSource, count: u64| SwapFaultSource {
            count,
            mean_wait_nanos: totals.fault_wait_nanos[source as usize]
                .checked_div(count)
                .unwrap_or(0),
        };
        SwapStats {
            disk: Some(SwapDiskStats {
                backend: shared.backend,
                capacity_bytes: shared.capacity_bytes,
                stored_bytes: totals.disk_bytes,
                pages: totals.disk_pages,
                pages_out_raw: totals.pages_out_disk_raw,
                pages_out_compressed: totals.pages_out_disk_compressed,
                written_back: totals.written_back,
                pages_lost: totals.lost_pages,
            }),
            zero: SwapZeroStats {
                pages: totals.zero_pages,
                pages_out: totals.pages_out_zero,
            },
            store: SwapStoreStats {
                limit_bytes: (store_limits(&policy).limit_frames() * PhysFrame::SIZE) as u64,
                frames: shared.store.held_frames() as u64,
                pages: totals.store_pages,
                payload_bytes: totals.store_payload_bytes,
                parked_pages: totals.parked_pages,
                pages_out: totals.pages_out_store,
                rejected_incompressible: totals.rejected_incompressible,
                rejected_full: totals.rejected_full,
                put_back: totals.put_back,
                compacted_frames: totals.compacted_frames,
            },
            faults: SwapFaultStats {
                zero: source(FaultSource::Zero, totals.faults_zero),
                store: source(FaultSource::Store, totals.faults_store),
                parked: source(FaultSource::Parked, totals.faults_parked),
                disk: source(FaultSource::Disk, totals.faults_disk),
                contended_waits: totals.contended_waits,
            },
            frames_released: totals.pages_out(),
            policy,
        }
    }

    /// The settings in force.
    pub fn policy(&self) -> SwapPolicy {
        self.shared.policy.load()
    }

    /// Replaces the settings. The policy reads them once per decision, so a
    /// change takes effect at the next one.
    pub fn set_policy(&self, policy: SwapPolicy) -> Result<(), SwapPolicyError> {
        self.shared.policy.set(policy)?;
        self.poke();
        Ok(())
    }

    /// Brings the page at `addr` back into memory, waiting only when its
    /// bytes are on the swap device or another path holds its slot.
    ///
    /// This is what the page-fault trampoline blocks the faulting fiber
    /// on, and what [`Self::ensure_present`] calls per page.
    ///
    /// Callers ask about an address whose entry carried a swap token when
    /// they looked. Succeeding means the page is present now: it was read
    /// back for this request, or another request got there first. The
    /// instruction is retried either way, and if the entry has since been
    /// released or decommitted the retry takes a fresh fault that the
    /// fault entry classifies again.
    ///
    /// A page in a RAM tier is resolved right here, on the calling
    /// processor, with no await between claiming its slot and releasing it
    /// (see the module's concurrency contract).
    pub async fn fault_in(&self, addr: VirtAddr) -> Result<(), SwapFaultError> {
        // A fault reports the address the instruction touched, not the
        // page it lives in; everything below this point works in pages.
        let addr = addr.page_floor();
        let issued_nanos = self.shared.clock.now_nanos();
        loop {
            match self.step(addr, issued_nanos) {
                Step::Done => return Ok(()),
                Step::Again => {}
                Step::Wait(mark) => self.shared.released.changed(mark).await,
                Step::Disk(..) => return self.fault_via_owner(addr, issued_nanos).await,
                Step::Failed(error) => return Err(error),
            }
        }
    }

    /// Queues a fault on a disk page for the owner and waits for its reply.
    async fn fault_via_owner(
        &self,
        addr: VirtAddr,
        issued_nanos: u64,
    ) -> Result<(), SwapFaultError> {
        let reply = Arc::new(SwapFaultReply::new());
        let request = SwapFaultRequest {
            addr,
            reply: reply.clone(),
            issued_nanos,
        };
        if self.shared.faults.push(request).is_err() {
            // The queue is full or closed. Either way nothing will ever
            // read this page back, and a caller that returns to the
            // faulting instruction would fault forever.
            return Err(SwapFaultError::Backend);
        }
        self.shared.work.notify_one_coalesced();
        loop {
            if let Some(outcome) = reply.read() {
                return outcome;
            }
            reply.ready.notified().await;
        }
    }

    /// One look at the slot the entry at `addr` names, resolving the page if
    /// a RAM tier holds it. The owner runs this too, for the faults it is
    /// handed.
    fn step(&self, addr: VirtAddr, issued_nanos: u64) -> Step {
        let shared = &*self.shared;
        let Some(token) = (shared.hooks.swapped_token)(addr) else {
            return Step::Done;
        };
        let index = SlotIndex::from_token(token).unwrap_or_else(|| {
            panic!(
                "the entry at {:#x} carries swap token {token:?}, past the slot table",
                addr.raw()
            )
        });
        // Armed before the slot is read: a path that publishes it after the
        // read still lands after the mark.
        let mark = shared.released.mark();
        let state = shared.slots.state(index);
        match state {
            SlotState::Zero | SlotState::Stored(_) | SlotState::Parked(_) => {
                self.resolve_in_place(addr, token, index, state, issued_nanos)
            }
            SlotState::Disk(key) => Step::Disk(token, key),
            SlotState::Lost => Step::Failed(SwapFaultError::PageLost),
            SlotState::Reserved | SlotState::Claimed => {
                shared.counters.here().contended_wait();
                Step::Wait(mark)
            }
            SlotState::Free { .. } | SlotState::Vacant => {
                // The slot was freed after the entry was read, which means
                // the entry was consumed: read it again. If it still names
                // the slot, a live entry references a slot with no page
                // behind it.
                assert!(
                    (shared.hooks.swapped_token)(addr) != Some(token),
                    "the entry at {:#x} carries swap token {token:?}, whose slot reads {state:?}",
                    addr.raw()
                );
                Step::Again
            }
        }
    }

    /// Reinstates a page a RAM tier holds: claims the slot, decodes straight
    /// into the frame the address space maps, and releases the storage.
    ///
    /// The claim is held across no await.
    fn resolve_in_place(
        &self,
        addr: VirtAddr,
        token: SwapToken,
        index: SlotIndex,
        state: SlotState,
        issued_nanos: u64,
    ) -> Step {
        let shared = &*self.shared;
        let Ok(claim) = shared.slots.claim(index, state) else {
            return Step::Again;
        };
        let fill = RamFill {
            frames: &shared.store,
            claim: &claim,
            state,
        };
        let mut context = &fill;
        let result = (shared.hooks.swap_in_page)(
            addr,
            token,
            (&raw mut context).cast::<()>(),
            |context, page| {
                // SAFETY: `context` is the reference above, alive for this
                // call and reached from nowhere else.
                let fill = unsafe { *context.cast::<&RamFill<'_>>() };
                fill.write(page);
            },
        );
        match result {
            Ok(()) => {
                let (source, frame_queued) = match state {
                    SlotState::Zero => (FaultSource::Zero, false),
                    SlotState::Stored(object) => {
                        (FaultSource::Store, shared.store.free_object(object))
                    }
                    SlotState::Parked(frame) => {
                        (FaultSource::Parked, shared.store.free_parked(frame))
                    }
                    other => unreachable!("a RAM-tier claim was taken from {other:?}"),
                };
                shared.release(claim);
                let counters = shared.counters.here();
                counters.unhold(Held::of_ram(state));
                counters.fault(
                    source,
                    shared.clock.now_nanos().saturating_sub(issued_nanos),
                );
                if frame_queued {
                    // The owner drains the frame, and gives it back to the
                    // pool if this was its last object.
                    shared.work.notify_one_coalesced();
                }
                Step::Done
            }
            Err(AddressSpaceError::NotSwapped | AddressSpaceError::SwapTokenChanged) => {
                // The entry moved under the claim: its storage is intact,
                // and whichever path consumed the entry frees the slot.
                // That may be the orphan drain, which deferred the token
                // because this claim held it.
                shared.restore(claim, state);
                shared.work.notify_one_coalesced();
                Step::Again
            }
            Err(AddressSpaceError::OutOfFrames) => {
                shared.restore(claim, state);
                Step::Failed(SwapFaultError::OutOfFrames)
            }
            Err(error) => {
                tracing::warn!(
                    target: "helios_kernel::swap",
                    addr = addr.raw(),
                    ?error,
                    "the address space refused to reinstate a page from a RAM tier"
                );
                shared.restore(claim, state);
                Step::Failed(SwapFaultError::AddressSpace)
            }
        }
    }

    /// Reads back every swapped-out page in `range`.
    ///
    /// Kernel code that touches user memory directly — a host call
    /// copying a guest buffer, a byte channel taking bytes out of guest
    /// memory, a DMA source built over a guest slice — calls this before
    /// the copy. Kernel stacks have no fault trampoline, so a fault
    /// there is fatal by design; pre-faulting is how those paths stay
    /// correct.
    pub async fn ensure_present(&self, range: VirtRange) -> Result<(), SwapFaultError> {
        let hooks = self.shared.hooks;
        let mut addr = range.start.page_floor().raw();
        let end = range.end().raw();
        while addr < end {
            let page = VirtAddr::new(addr);
            if (hooks.swapped_token)(page).is_some() {
                self.fault_in(page).await?;
            }
            addr += PhysFrame::SIZE;
        }
        Ok(())
    }

    /// Wakes the policy, for callers that have just changed how much
    /// memory is free.
    pub fn poke(&self) {
        self.shared.work.notify_one_coalesced();
    }
}

/// The address-space hooks, installed once by the active backend.
static HOOKS: Once<&'static SwapVmHooks> = Once::new();
/// The live swap handle, so the page-fault entry can find it without
/// threading it through the trap frame.
static HANDLE: Once<SwapHandle> = Once::new();

/// Publishes the platform address space's swap surface. Called once by
/// the backend that owns the address space, before any component runs.
pub fn install_swap_hooks(hooks: &'static SwapVmHooks) {
    let mut installed = false;
    HOOKS.call_once(|| {
        installed = true;
        hooks
    });
    assert!(
        installed,
        "swap address-space hooks installed more than once"
    );
}

pub fn installed_swap_hooks() -> Option<&'static SwapVmHooks> {
    HOOKS.get().copied()
}

/// The live swap handle, if this platform has one.
pub fn installed_swap_handle() -> Option<&'static SwapHandle> {
    HANDLE.get()
}

/// The token the page-table entry at `addr` carries, or `None`.
///
/// Lock-free and safe to call from trap context: this is what a
/// backend's fault entry uses to tell a swapped-out page apart from a
/// guard-page access it must hand to the runtime.
pub fn swapped_token(addr: VirtAddr) -> Option<SwapToken> {
    let hooks = installed_swap_hooks()?;
    (hooks.swapped_token)(addr)
}

/// Records that this platform runs without swap, and why.
///
/// Backends without a lazy-commit address space call this instead of
/// [`install_swap`]: there is nothing to configure, and the reason
/// belongs in the boot log rather than in a silent absence.
pub fn disable_swap(reason: SwapDisabled) {
    tracing::info!(
        target: "helios_kernel::swap",
        %reason,
        "swap is disabled on this platform"
    );
}

/// Spawns the task that owns swap, and publishes its handle.
///
/// The task is local to the calling processor because the swap device's
/// completions are: a backend brings the scratch disk up on one
/// processor and routes its interrupt there.
pub fn install_swap<CpuImpl, Backend>(
    spawner: Spawner<CpuImpl>,
    timer: Timer<CpuImpl>,
    cpu: CpuImpl,
    registry: InstanceRegistry,
    backend: Backend,
    backend_name: &'static str,
    capacity_bytes: u64,
) -> SwapHandle
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    Backend: SwapBackend,
{
    let hooks = installed_swap_hooks().unwrap_or_else(|| {
        panic!("swap backend installed before the address space published its swap hooks")
    });
    let clock = SwapClock {
        timebase: cpu.timebase(),
        now_ticks: hooks.now_ticks,
    };
    let handle = SwapHandle::new(
        backend_name,
        capacity_bytes,
        clock,
        hooks,
        cpu.processor_count(),
    );
    let mut published = false;
    HANDLE.call_once(|| {
        published = true;
        handle.clone()
    });
    assert!(published, "swap backend installed more than once");

    let service = SwapService::new(
        Some(Disk::new(backend)),
        handle.clone(),
        hooks,
        registry,
        cpu,
    );
    spawner.spawn_local_detached(async move {
        service.run(timer).await;
    });
    tracing::info!(
        target: "helios_kernel::swap",
        backend = backend_name,
        capacity_bytes,
        "swap online"
    );
    handle
}

/// How a page's bytes are laid out in the extent the backend holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiskEncoding {
    /// The whole page.
    Raw,
    /// An LZ4 block of this many bytes, which the backend must be asked for
    /// by exactly that length.
    Lz4(slot::CompressedLen),
}

impl DiskEncoding {
    /// Bytes the extent holds.
    fn bytes(self) -> usize {
        match self {
            Self::Raw => PhysFrame::SIZE,
            Self::Lz4(len) => len.get(),
        }
    }
}

/// What the owner knows about a page on the disk: the backend's token for
/// its extent and how the extent is encoded.
#[derive(Clone, Copy)]
struct DiskEntry<Token> {
    token: Token,
    encoding: DiskEncoding,
}

/// The attached swap device and, for each page it holds, where.
///
/// Owner-only: the slot word says a page is on the disk and under which key,
/// and the owner is the only task that talks to the backend, so the tokens
/// live here instead of in the shared table.
struct Disk<Backend: SwapBackend> {
    backend: Backend,
    entries: Slab<DiskEntry<Backend::Token>>,
}

impl<Backend: SwapBackend> Disk<Backend> {
    fn new(backend: Backend) -> Self {
        Self {
            backend,
            entries: Slab::new(),
        }
    }

    fn insert(&mut self, token: Backend::Token, encoding: DiskEncoding) -> DiskKey {
        let key = self.entries.insert(DiskEntry { token, encoding });
        DiskKey::new(u32::try_from(key).expect("more disk-resident pages than a slot word keys"))
    }

    fn get(&self, key: DiskKey) -> DiskEntry<Backend::Token> {
        self.entries[key.raw() as usize]
    }

    fn remove(&mut self, key: DiskKey) -> DiskEntry<Backend::Token> {
        self.entries.remove(key.raw() as usize)
    }
}

/// What a disk fault did to the page it was asked for.
enum Landed {
    /// The page is resident, or its page-table entry is gone and its slot
    /// is queued to be freed.
    Resolved,
    /// The token the fault read no longer names this page's entry: the
    /// slot belonged to another page, which went back where it was. The
    /// fault reads the entry again.
    Stale,
}

struct SwapService<Backend: SwapBackend, CpuImpl> {
    /// The swap device, if one is attached. Without one, a page that no RAM
    /// tier takes stays in memory.
    disk: Option<Disk<Backend>>,
    handle: SwapHandle,
    hooks: &'static SwapVmHooks,
    registry: InstanceRegistry,
    cpu: CpuImpl,
    /// The owner's half of slot allocation. The words are in
    /// [`SwapShared::slots`].
    slots: SlotAllocator,
    /// The owner's half of the compressed store: where every frame is, by
    /// how much room it has. The frames are in [`SwapShared::store`].
    store: Store,
    /// Orphaned tokens whose slot was claimed when the drain met them,
    /// retried at the top of the next pass.
    ///
    /// The drain hands each token once and keeps nothing, so a token the
    /// owner cannot release yet would otherwise leak. It holds only tokens
    /// whose word was `Claimed`, and empties as soon as those claims
    /// resolve: the claimant's reinstatement fails because the entry is
    /// gone, and it puts the word back.
    deferred: Vec<SwapToken>,
    /// The one staging buffer every transfer goes through: the page a
    /// detach copies out, and the page a raw disk read lands in. Owning it
    /// here is what keeps the fault path allocation-free.
    page: Box<[u8]>,
    /// The LZ4 block of the page being placed, and the buffer a compressed
    /// disk read lands in.
    block: Box<[u8]>,
    /// Where a compressed disk read is decoded to, before it is mapped.
    decoded: Box<[u8]>,
    /// The compressor's hash table, reused for every page.
    table: Box<CompressTable>,
    /// What the last summary line reported, so the next one can report
    /// the difference. See [`SWAP_REPORT_INTERVAL`].
    report: SwapReport,
}

/// The running total the periodic summary line subtracts from.
#[derive(Clone, Copy, Default)]
struct SwapReport {
    at_nanos: u64,
    totals: Totals,
}

/// What became of a page the pass detached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placed {
    /// The page is out of memory, in a tier.
    Out,
    /// No tier took the page and it is resident again.
    Kept,
}

impl<Backend, CpuImpl> SwapService<Backend, CpuImpl>
where
    Backend: SwapBackend,
    CpuImpl: Cpu + Clone,
{
    fn new(
        disk: Option<Disk<Backend>>,
        handle: SwapHandle,
        hooks: &'static SwapVmHooks,
        registry: InstanceRegistry,
        cpu: CpuImpl,
    ) -> Self {
        Self {
            disk,
            handle,
            hooks,
            registry,
            cpu,
            slots: SlotAllocator::new(),
            store: Store::new(),
            deferred: Vec::new(),
            page: vec![0_u8; PhysFrame::SIZE].into_boxed_slice(),
            block: vec![0_u8; lz4_flex::block::get_maximum_output_size(PhysFrame::SIZE)]
                .into_boxed_slice(),
            decoded: vec![0_u8; PhysFrame::SIZE].into_boxed_slice(),
            table: Box::default(),
            report: SwapReport::default(),
        }
    }

    async fn run(mut self, timer: Timer<CpuImpl>) -> ! {
        loop {
            // Frames the faults emptied go back to the pool before anything
            // else asks the pool for one.
            self.store.drain(&self.handle.shared.store);
            // Faults first: a blocked fiber is a guest that has stopped,
            // while a swap-out pass is work that can wait a tick.
            self.serve_faults().await;
            self.release_orphaned_tokens().await;
            self.run_policy_pass().await;
            self.report_activity();
            let notified = self.handle.shared.work.notified();
            futures::future::select(
                core::pin::pin!(notified),
                core::pin::pin!(timer.sleep_for(SWAP_TICK)),
            )
            .await;
        }
    }

    async fn serve_faults(&mut self) {
        while let Ok(request) = self.handle.shared.faults.pop() {
            let outcome = self.swap_in(request.addr, request.issued_nanos).await;
            // From the fault's issue, not from the moment this loop got
            // to it: the guest has been stopped the whole time.
            let elapsed = self
                .handle
                .shared
                .clock
                .now_nanos()
                .saturating_sub(request.issued_nanos);
            match outcome {
                Ok(()) => {
                    tracing::debug!(
                        target: "helios_kernel::swap",
                        addr = request.addr.raw(),
                        latency_nanos = elapsed,
                        "swap in"
                    );
                    request.reply.settle(FAULT_RESOLVED);
                }
                Err(error) => {
                    tracing::warn!(
                        target: "helios_kernel::swap",
                        addr = request.addr.raw(),
                        %error,
                        "swap in failed"
                    );
                    request.reply.settle(match error {
                        SwapFaultError::OutOfFrames => FAULT_OUT_OF_FRAMES,
                        SwapFaultError::AddressSpace => FAULT_ADDRESS_SPACE,
                        SwapFaultError::PageLost => FAULT_PAGE_LOST,
                        SwapFaultError::NotConfigured | SwapFaultError::Backend => FAULT_BACKEND,
                    });
                }
            }
        }
    }

    /// Emits at most one line per [`SWAP_REPORT_INTERVAL`] describing
    /// what moved since the last one.
    fn report_activity(&mut self) {
        let now = monotonic_nanos(&self.cpu);
        if now.saturating_sub(self.report.at_nanos) < SWAP_REPORT_INTERVAL.as_nanos() as u64 {
            return;
        }
        let current = SwapReport {
            at_nanos: now,
            totals: self.handle.shared.counters.totals(),
        };
        let (before, after) = (self.report.totals, current.totals);
        let pages_in = after.faults() - before.faults();
        let pages_out = after.pages_out() - before.pages_out();
        // A quiet interval is the normal case and says nothing worth a
        // line; the window it would have covered rolls into the next.
        if pages_in == 0 && pages_out == 0 {
            return;
        }
        let latency = after.total_fault_wait_nanos() - before.total_fault_wait_nanos();
        self.report = current;
        tracing::info!(
            target: "helios_kernel::swap",
            pages_in,
            pages_out,
            out_zero = after.pages_out_zero - before.pages_out_zero,
            out_store = after.pages_out_store - before.pages_out_store,
            out_disk_raw = after.pages_out_disk_raw - before.pages_out_disk_raw,
            out_disk_compressed =
                after.pages_out_disk_compressed - before.pages_out_disk_compressed,
            out_parked = after.pages_out_parked - before.pages_out_parked,
            put_back = after.put_back - before.put_back,
            rejected_incompressible =
                after.rejected_incompressible - before.rejected_incompressible,
            rejected_full = after.rejected_full - before.rejected_full,
            in_zero = after.faults_zero - before.faults_zero,
            in_store = after.faults_store - before.faults_store,
            in_parked = after.faults_parked - before.faults_parked,
            in_disk = after.faults_disk - before.faults_disk,
            contended_waits = after.contended_waits - before.contended_waits,
            written_back = after.written_back - before.written_back,
            compacted_frames = after.compacted_frames - before.compacted_frames,
            mean_fault_latency_nanos = latency.checked_div(pages_in).unwrap_or(0),
            disk_pages = after.disk_pages,
            store_frames = self.store.frames(),
            "swap active"
        );
    }

    /// Resolves a fault the queue handed the owner: a page on the disk, or
    /// a page whose slot changed hands since the fault looked.
    async fn swap_in(&mut self, addr: VirtAddr, issued_nanos: u64) -> Result<(), SwapFaultError> {
        // A request exists only for an address whose entry carried a token
        // when the fault was taken (the backend's fault entry checks
        // `swapped_token` before it enters the trampoline, and
        // `ensure_present` before it asks). One that carries none now has
        // changed since: an earlier request reinstated the page, which is
        // the case of several guest threads or fibers faulting on one
        // page, or the address space dropped the entry. Either way the
        // retried instruction takes the fault again and a fresh fault is
        // classified from scratch, so succeeding cannot loop.
        loop {
            match self.handle.step(addr, issued_nanos) {
                Step::Done => return Ok(()),
                Step::Again => {}
                Step::Wait(mark) => self.handle.shared.released.changed(mark).await,
                Step::Disk(token, key) => {
                    match self.swap_in_disk(addr, token, key, issued_nanos).await? {
                        Landed::Resolved => return Ok(()),
                        Landed::Stale => {}
                    }
                }
                Step::Failed(error) => return Err(error),
            }
        }
    }

    /// Reads the page at `addr` back from the disk, given the token its
    /// entry carried and the key its slot said.
    ///
    /// The slot is claimed for the whole transfer. The address space
    /// reinstates the page only while its entry still carries `token`, so a
    /// token that went stale is refused there and never maps another
    /// page's bytes.
    async fn swap_in_disk(
        &mut self,
        addr: VirtAddr,
        token: SwapToken,
        key: DiskKey,
        issued_nanos: u64,
    ) -> Result<Landed, SwapFaultError> {
        let index = SlotIndex::from_token(token).expect("a step only hands out valid tokens");
        let shared = &self.handle.shared;
        let claim = shared
            .slots
            .claim(index, SlotState::Disk(key))
            .unwrap_or_else(|found| {
                panic!(
                    "swap slot {} was {found:?} when the owner claimed its disk page: only the \
                     owner claims one",
                    index.raw()
                )
            });
        let disk = self
            .disk
            .as_mut()
            .expect("a slot reads Disk only while a disk is attached");
        let entry = disk.get(key);
        // A failed read leaves the extent with the backend and the slot
        // with the page-table entry, exactly as they were. The backend
        // wants a buffer of exactly the length it stored.
        let read = match entry.encoding {
            DiskEncoding::Raw => &mut self.page[..],
            DiskEncoding::Lz4(len) => &mut self.block[..len.get()],
        };
        if let Err(error) = disk.backend.swap_in(entry.token, read).await {
            tracing::warn!(
                target: "helios_kernel::swap",
                addr = addr.raw(),
                %error,
                "swap backend could not read a page back"
            );
            shared.restore(claim, SlotState::Disk(key));
            return Err(SwapFaultError::Backend);
        }
        // The backend has consumed the extent, so the token is dead and
        // the buffer holds the page's only copy.
        disk.remove(key);
        shared
            .counters
            .here()
            .unhold(Held::Disk(entry.encoding.bytes()));
        if let DiskEncoding::Lz4(len) = entry.encoding {
            // The device is outside the kernel's integrity boundary: a block
            // that does not decode to one page is a failed read of this
            // page, not a corrupt kernel. The block is put back as it was.
            match decompress_into(&self.block[..len.get()], &mut self.decoded) {
                Ok(PhysFrame::SIZE) => {}
                outcome => {
                    tracing::error!(
                        target: "helios_kernel::swap",
                        addr = addr.raw(),
                        index = index.raw(),
                        ?outcome,
                        "a compressed page read back from the swap backend does not decode to \
                         one page"
                    );
                    // Back on the disk as it was: the next fault fails the
                    // same way, which is what the device holds. A refused
                    // rehome marks the page lost, which its own fault reports.
                    let _ = self.rehome(claim, addr, entry.encoding).await;
                    return Err(SwapFaultError::Backend);
                }
            }
        }
        // The address space reads the flags the page had out of its own
        // bookkeeping, fills a fresh frame from the buffer, and only
        // publishes the mapping once the bytes are there.
        let source = match entry.encoding {
            DiskEncoding::Raw => &self.page[..],
            DiskEncoding::Lz4(_) => &self.decoded[..],
        };
        match reinstate(self.hooks, addr, token, source) {
            Ok(()) => {
                shared.release(claim);
                shared.counters.here().fault(
                    FaultSource::Disk,
                    shared.clock.now_nanos().saturating_sub(issued_nanos),
                );
                Ok(Landed::Resolved)
            }
            Err(AddressSpaceError::NotSwapped) => {
                // The page-table reference went away while the read was in
                // flight: its range was decommitted or released, which
                // queued the token. The extent is already consumed, so the
                // slot has no storage left, and the orphan drain frees it.
                shared.restore(claim, SlotState::Vacant);
                Ok(Landed::Resolved)
            }
            Err(AddressSpaceError::SwapTokenChanged) => {
                // The entry at `addr` carries another token than the one
                // read, so the slot just claimed holds some other page. Its
                // bytes are in the buffer: give them back to where they were
                // and let the caller read the entry again.
                tracing::debug!(
                    target: "helios_kernel::swap",
                    addr = addr.raw(),
                    index = index.raw(),
                    "the entry at the faulting address changed under the fault; retrying"
                );
                // A refused rehome loses that other page, which its own
                // fault reports; it is no failure of this one.
                let _ = self.rehome(claim, addr, entry.encoding).await;
                Ok(Landed::Stale)
            }
            Err(error) => {
                tracing::warn!(
                    target: "helios_kernel::swap",
                    addr = addr.raw(),
                    ?error,
                    "the address space refused to reinstate a page; returning it to the swap backend"
                );
                let _ = self.rehome(claim, addr, entry.encoding).await;
                Err(match error {
                    AddressSpaceError::OutOfFrames => SwapFaultError::OutOfFrames,
                    _ => SwapFaultError::AddressSpace,
                })
            }
        }
    }

    /// Writes the buffer a disk read left the page in, which holds the only
    /// copy of the page whose slot `claim` holds, back to the disk under that
    /// same slot and in the encoding it came in.
    ///
    /// When the disk refuses too, the page's bytes exist nowhere: the slot is
    /// marked lost and stays reserved, so no other page can be handed it and
    /// a fault on it fails rather than reading a stale token, until the
    /// owning address space's teardown releases it.
    async fn rehome(
        &mut self,
        claim: Claim,
        addr: VirtAddr,
        encoding: DiskEncoding,
    ) -> Result<(), ()> {
        let disk = self
            .disk
            .as_mut()
            .expect("a page is rehomed only after a read from the disk");
        let bytes = match encoding {
            DiskEncoding::Raw => &self.page[..],
            DiskEncoding::Lz4(len) => &self.block[..len.get()],
        };
        let shared = &self.handle.shared;
        match disk.backend.swap_out(bytes).await {
            Ok(token) => {
                let key = disk.insert(token, encoding);
                shared.counters.here().hold(Held::Disk(encoding.bytes()));
                shared.restore(claim, SlotState::Disk(key));
                Ok(())
            }
            Err(error) => {
                tracing::error!(
                    target: "helios_kernel::swap",
                    addr = addr.raw(),
                    index = claim.index().raw(),
                    %error,
                    "swapped-out page lost: the swap backend refused it back and no other copy exists"
                );
                shared.restore(claim, SlotState::Lost);
                shared.counters.here().hold(Held::Lost);
                Err(())
            }
        }
    }

    async fn release_orphaned_tokens(&mut self) {
        // Tokens the last pass could not release come first; the drain
        // hands each new one once.
        let mut orphans = core::mem::take(&mut self.deferred);
        (self.hooks.drain_orphaned_swap_tokens)(
            (&raw mut orphans).cast::<()>(),
            |context, token| {
                // SAFETY: `context` is the `Vec` above, alive for this
                // call and reached from nowhere else.
                let orphans = unsafe { &mut *context.cast::<Vec<SwapToken>>() };
                orphans.push(token);
            },
        );
        for token in orphans {
            self.release_orphan(token).await;
        }
    }

    /// Releases the slot a discarded page-table entry named, and the
    /// storage behind it.
    ///
    /// The entry was consumed by the discard, so no fault can reinstate
    /// the page any more; one that is mid-flight loses its reinstatement
    /// and puts the word back.
    async fn release_orphan(&mut self, token: SwapToken) {
        let Some(index) = SlotIndex::from_token(token) else {
            return;
        };
        let shared = &self.handle.shared;
        let state = shared.slots.state(index);
        match state {
            // Already free, or never handed out: the entry was consumed
            // by a reinstatement that freed it. Nothing to release.
            SlotState::Free { .. } => {}
            // The owner holds `Reserved` across a detach and drains only
            // between slots, so a discarded entry cannot name one.
            SlotState::Reserved => panic!(
                "swap slot {} is reserved but its page-table entry was discarded",
                index.raw()
            ),
            // A fault holds the slot. It will lose its reinstatement, the
            // entry being gone, and put the word back; retry then.
            SlotState::Claimed => self.deferred.push(token),
            // A lost page holds no extent: its bytes were given up when it
            // was lost, and only its slot is released. A vacant one holds no
            // storage either, and a zero page never had any.
            SlotState::Lost | SlotState::Vacant | SlotState::Zero => {
                match shared.slots.claim(index, state) {
                    Ok(claim) => {
                        match state {
                            SlotState::Lost => shared.counters.here().unhold(Held::Lost),
                            SlotState::Zero => shared.counters.here().unhold(Held::Zero),
                            _ => {}
                        }
                        shared.release(claim);
                    }
                    Err(_) => self.deferred.push(token),
                }
            }
            SlotState::Stored(object) => match shared.slots.claim(index, state) {
                Ok(claim) => {
                    shared.store.free_object(object);
                    shared.counters.here().unhold(Held::of_ram(state));
                    shared.release(claim);
                }
                Err(_) => self.deferred.push(token),
            },
            SlotState::Parked(frame) => match shared.slots.claim(index, state) {
                Ok(claim) => {
                    shared.store.free_parked(frame);
                    shared.counters.here().unhold(Held::of_ram(state));
                    shared.release(claim);
                }
                Err(_) => self.deferred.push(token),
            },
            SlotState::Disk(key) => match shared.slots.claim(index, state) {
                Ok(claim) => {
                    let disk = self
                        .disk
                        .as_mut()
                        .expect("a slot reads Disk only while a disk is attached");
                    let entry = disk.remove(key);
                    disk.backend.release(entry.token).await;
                    shared
                        .counters
                        .here()
                        .unhold(Held::Disk(entry.encoding.bytes()));
                    shared.release(claim);
                }
                Err(_) => self.deferred.push(token),
            },
        }
    }

    async fn run_policy_pass(&mut self) {
        // A store over its limit drains to disk whatever the pressure: the
        // limit bounds what compression may take from the pool, and nothing
        // else brings it back under.
        self.write_back_over_limit().await;

        let pressure = current_pressure();
        if pressure == PressureLevel::Green {
            return;
        }
        // A queued fault is a guest stopped mid-instruction; reclaiming
        // memory can wait for it. The loop above serves the queue and
        // comes straight back here.
        if !self.handle.shared.faults.is_empty() {
            return;
        }

        let now = monotonic_nanos(&self.cpu);
        let idle_after = u64::try_from(self.handle.shared.policy.load().idle_after.as_nanos())
            .expect("a published policy holds an idle delay that fits the nanosecond clock");
        let mut candidates = self.registry.idle_instances(now, idle_after);
        // Largest resident first: one pass over the biggest idle
        // instance frees more than several passes over small ones, and
        // resident bytes come from the address space rather than from
        // what the instance thinks it grew to.
        candidates
            .sort_by_key(|id| core::cmp::Reverse((self.hooks.owned_resident_bytes)(id.raw())));

        let mut moved = 0_usize;
        for id in &candidates {
            if moved >= SWAP_BATCH_BYTES {
                break;
            }
            moved += self.evict_owner(*id, SWAP_BATCH_BYTES - moved).await;
        }

        if pressure != PressureLevel::Red {
            tracing::debug!(
                target: "helios_kernel::swap",
                bytes = moved,
                idle_instances = candidates.len(),
                "swapped idle instances out under yellow pressure"
            );
            self.compact_and_write_back(moved).await;
            return;
        }

        // Red: the idle delay was not enough of a bar. Drop it to "not on
        // a processor right now"; the pages taken are cold either way.
        //
        // The bar stays at "not executing" rather than dropping to
        // "anything": while an instance is on a processor it may be
        // inside a host call, whose frame already holds that fiber's
        // blocking context and so cannot block again to fault a page
        // back in. Never detaching a page from an executing instance is
        // what keeps the kernel out of that corner, and it is why the
        // pre-fault accessor is a contract for kernel paths that hold
        // guest pointers rather than a thing every host call must do.
        // The list below only nominates; each detach re-establishes the
        // bar for itself (`evict_owner`).
        for id in self.registry.idle_instances(now, 0) {
            if moved >= SWAP_BATCH_BYTES {
                break;
            }
            moved += self.evict_owner(id, SWAP_BATCH_BYTES - moved).await;
        }
        tracing::debug!(
            target: "helios_kernel::swap",
            bytes = moved,
            "swap pass under red pressure"
        );
        self.compact_and_write_back(moved).await;
    }

    /// The rest of a pass that has moved `moved` bytes of its batch: compacts
    /// the store if it wastes enough of its frames, then, if pressure is
    /// still `Red`, writes the store's oldest frames to disk with what is
    /// left of the batch.
    ///
    /// Compaction comes first because it frees frames with no device work and
    /// may be all the pass needs.
    async fn compact_and_write_back(&mut self, moved: usize) {
        let mut remaining = SWAP_BATCH_BYTES.saturating_sub(moved);
        if remaining == 0 || !self.handle.shared.faults.is_empty() {
            return;
        }
        let shared = &*self.handle.shared;
        let compacted = self.store.compact(
            &shared.store,
            &shared.slots,
            remaining,
            // A fault that found an object's slot held waits for this.
            || shared.released.signal(),
        );
        if compacted.objects != 0 {
            shared.counters.here().compacted(compacted.frames);
            tracing::debug!(
                target: "helios_kernel::swap",
                objects = compacted.objects,
                bytes = compacted.bytes,
                frames = compacted.frames,
                "compacted the swap store"
            );
        }
        remaining = remaining.saturating_sub(compacted.bytes);
        if current_pressure() == PressureLevel::Red {
            let frames = remaining / PhysFrame::SIZE;
            self.write_back(frames).await;
        }
    }

    /// Writes the store down to its accept level if it is over its limit, or
    /// refusing objects under the hysteresis.
    async fn write_back_over_limit(&mut self) {
        let policy = self.handle.shared.policy.load();
        if self.disk.is_none() || !policy.writeback {
            return;
        }
        let Some(over) = self.store.over_limit_by(store_limits(&policy)) else {
            return;
        };
        let freed = self.write_back(over).await;
        tracing::debug!(
            target: "helios_kernel::swap",
            frames = freed,
            "wrote the swap store back to disk, over its limit"
        );
    }

    /// Writes the store's oldest frames to disk, whole frames at a time,
    /// until `frames` of them are back in the pool, or the store has no frame
    /// it can empty, or a fault is queued. Returns the frames freed.
    ///
    /// Objects in the store are untouched since they were stored, because any
    /// access faults them out, so the frame filled first is the coldest, and
    /// writing every object in one frame frees exactly one frame: the
    /// shortage is in frames, and a write of an object whose neighbours stay
    /// frees none.
    ///
    /// Every frame is tried at most once, and a frame that is not emptied
    /// goes to the back of the age list.
    async fn write_back(&mut self, frames: usize) -> usize {
        if self.disk.is_none() || !self.handle.shared.policy.load().writeback {
            return 0;
        }
        let mut freed = 0;
        for _ in 0..self.store.aged_frames() {
            if freed >= frames || !self.handle.shared.faults.is_empty() {
                break;
            }
            let Some(frame) = self.store.rotate_oldest() else {
                break;
            };
            let proceed = self.write_back_frame(frame).await;
            freed += self.store.drain(&self.handle.shared.store);
            if !proceed {
                break;
            }
            yield_now().await;
        }
        freed
    }

    /// Writes each object of `frame` that no fault holds to the disk, still
    /// compressed, and a parked page raw. Returns whether the pass may go on:
    /// `false` when the device refused a write or a fault is queued.
    async fn write_back_frame(&mut self, frame: StoreFrameIndex) -> bool {
        let shared = &*self.handle.shared;
        let disk = self
            .disk
            .as_mut()
            .expect("writeback runs only when a disk is attached");
        let mut cursor = 0;
        while let Some((claim, state)) =
            self.store
                .claim_next(&shared.store, &shared.slots, frame, &mut cursor)
        {
            // Checked after the claim is held and before the write begins: a
            // fault queued meanwhile has a guest stopped behind it, and this
            // object goes back as it was.
            if !shared.faults.is_empty() {
                shared.restore(claim, state);
                return false;
            }
            let (bytes, encoding) = match state {
                SlotState::Stored(object) => (
                    shared.store.payload(object, &claim),
                    DiskEncoding::Lz4(object.len()),
                ),
                SlotState::Parked(index) => {
                    (shared.store.parked_page(index, &claim), DiskEncoding::Raw)
                }
                other => unreachable!("a store walk claimed a slot from {other:?}"),
            };
            // The claim is held across the write: a fault on this page waits
            // on the slot, and the bytes cannot move or be freed under it.
            let written = disk.backend.swap_out(bytes).await;
            match written {
                Ok(token) => {
                    let key = disk.insert(token, encoding);
                    // The frame is queued for the owner's drain, which the
                    // caller runs once the frame's objects are done.
                    match state {
                        SlotState::Stored(object) => shared.store.free_object(object),
                        SlotState::Parked(index) => shared.store.free_parked(index),
                        _ => unreachable!("matched above"),
                    };
                    let counters = shared.counters.here();
                    counters.written_back();
                    counters.unhold(Held::of_ram(state));
                    counters.hold(Held::Disk(encoding.bytes()));
                    shared.restore(claim, SlotState::Disk(key));
                }
                Err(error) => {
                    tracing::warn!(
                        target: "helios_kernel::swap",
                        %error,
                        "swap backend refused a page written back from the store"
                    );
                    shared.restore(claim, state);
                    return false;
                }
            }
        }
        true
    }

    /// Swaps up to `budget` bytes of `instance`'s cold pages out, and
    /// reports how many bytes actually left memory.
    ///
    /// The plan is a scan taken once, and the instance was off every
    /// processor when it was nominated; neither is true for long. Each
    /// page is detached under a hold of its own
    /// ([`InstanceRegistry::while_off_processor`]), taken after the
    /// previous page's device write has landed, and the first refusal —
    /// the instance began a turn, or is gone — ends the instance's part
    /// of the pass. The hold covers the detach and nothing after it: the
    /// device write runs with the page already out, and a turn that
    /// begins then meets a swapped page the way it would after any
    /// earlier pass.
    async fn evict_owner(&mut self, instance: InstanceId, budget: usize) -> usize {
        let owner = MemoryOwner::new(instance.raw());
        let mut plan = EvictionPlan {
            pages: Vec::new(),
            remaining: budget,
            visits_left: SCAN_BUDGET_PAGES,
        };
        (self.hooks.scan_committed_pages)(
            owner.raw(),
            (&raw mut plan).cast::<()>(),
            |context, addr, _flags, page_age| {
                // SAFETY: `context` is the plan above, alive for the
                // duration of this scan and reached from nowhere else.
                let plan = unsafe { &mut *context.cast::<EvictionPlan>() };
                plan.consider(addr, page_age)
            },
        );

        let mut moved = 0_usize;
        for addr in plan.pages {
            // The swap device is the resource both halves of this task
            // contend for, and a fault has a fiber stopped behind it.
            // Give the device up at the first page boundary after one
            // arrives rather than making it wait out the batch.
            if !self.handle.shared.faults.is_empty() {
                break;
            }
            match self.swap_out(instance, addr).await {
                Ok(Placed::Out) => moved += PhysFrame::SIZE,
                // No tier took the page; the next one may be compressible.
                Ok(Placed::Kept) => {}
                Err(()) => break,
            }
            // Compression is CPU work with no await in it, and a batch is
            // hundreds of pages: give the processor back between pages.
            yield_now().await;
        }
        if moved != 0 {
            tracing::debug!(
                target: "helios_kernel::swap",
                instance = owner.raw(),
                pages = moved / PhysFrame::SIZE,
                "swap out instance"
            );
        }
        moved
    }

    /// Swaps out the page at `addr`, which belongs to `instance`.
    ///
    /// `Err` ends the instance's part of the pass: the pool has no frame for
    /// the spare, the slot table is full, or the page could not be detached
    /// (the instance began a turn, or its range went away).
    async fn swap_out(&mut self, instance: InstanceId, addr: VirtAddr) -> Result<Placed, ()> {
        // Every detached page needs a placement that cannot fail, which is
        // the spare frame: held before the detach, so a failure of the pool
        // here costs nothing, and after it would cost the page.
        if self.store.ensure_spare(&self.handle.shared.store).is_err() {
            tracing::debug!(
                target: "helios_kernel::swap",
                "the user pool has no frame for the store's spare; ending the pass"
            );
            return Err(());
        }
        // The index has to exist before the page can be detached: the
        // page-table entry carries it, and the entry has to be in place
        // before the frame goes back to the pool.
        let Some(claim) = self.slots.reserve(&self.handle.shared.slots) else {
            tracing::warn!(
                target: "helios_kernel::swap",
                addr = addr.raw(),
                "swap slot table is full"
            );
            return Err(());
        };
        let handle = claim.index().token();
        // Detaching files the entry, replaces the leaf, shoots the TLB
        // down on every processor and only then copies the page into our
        // buffer and hands the frame back, all under the address space's
        // own lock: a store from a processor that was still running the
        // instance has landed in the frame before the copy, or faulted.
        // The hold around it keeps every store of the instance off every
        // processor until it is done.
        let hooks = self.hooks;
        let page = &mut self.page;
        let detached = self
            .registry
            .while_off_processor(instance, |off| hooks.detach_page(off, addr, handle, page));
        match detached {
            Ok(Ok(_flags)) => {}
            Ok(Err(error)) => {
                tracing::debug!(
                    target: "helios_kernel::swap",
                    addr = addr.raw(),
                    ?error,
                    "page could not be detached for swap-out"
                );
                self.handle.shared.release(claim);
                return Err(());
            }
            Err(refused) => {
                tracing::debug!(
                    target: "helios_kernel::swap",
                    instance = instance.raw(),
                    addr = addr.raw(),
                    %refused,
                    "instance cannot be held off the processors; leaving its pages"
                );
                self.handle.shared.release(claim);
                return Err(());
            }
        }
        // From here the page is not present and its entry points at a
        // reserved slot. A guest faulting on it in this window waits for
        // the slot to be placed (`Step::Wait`); the placement signals it.
        Ok(self.place(claim, addr).await)
    }

    /// Finds the detached page in the staging buffer a destination, down the
    /// ladder: zero, the compressed store, the disk, and last resort back
    /// into memory or parked.
    ///
    /// The spare frame was ensured before the detach, so every path out of
    /// here leaves the page somewhere: none of them allocates.
    async fn place(&mut self, claim: Claim, addr: VirtAddr) -> Placed {
        let policy = self.handle.shared.policy.load();

        // A page of zeros has no storage. The scan reads whole words.
        if self.page.chunks_exact(8).all(|word| word == [0; 8]) {
            let shared = &self.handle.shared;
            shared.restore(claim, SlotState::Zero);
            let counters = shared.counters.here();
            counters.page_out(Placement::Zero);
            counters.hold(Held::Zero);
            return Placed::Out;
        }

        // Rejection is decided after compressing: the encoder needs room for
        // the worst case up front and cannot stop early.
        let block_len = compress_into_with_table(&self.page, &mut self.block, &mut self.table)
            .expect("the block buffer holds the worst-case output of one page");
        let mut encoding = DiskEncoding::Raw;
        match policy.gate().accept(block_len) {
            Some(len) => {
                let shared = &self.handle.shared;
                match self.store.allocate(
                    &shared.store,
                    claim.index(),
                    &self.block[..block_len],
                    len,
                    store_limits(&policy),
                ) {
                    Ok(object) => {
                        shared.restore(claim, SlotState::Stored(object));
                        let counters = shared.counters.here();
                        counters.page_out(Placement::Store);
                        counters.hold(Held::Stored(len.get()));
                        return Placed::Out;
                    }
                    Err(StoreFull) => {
                        // The page compresses well enough to be worth
                        // keeping; the disk gets it compressed.
                        shared.counters.here().rejected_full();
                        encoding = DiskEncoding::Lz4(len);
                    }
                }
            }
            None => self.handle.shared.counters.here().rejected_incompressible(),
        }

        let token = claim.index().token();
        let Some(disk) = self.disk.as_mut() else {
            // Nowhere to put it: back into memory, from the staging buffer,
            // which holds the page's only copy.
            return match reinstate(self.hooks, addr, token, &self.page) {
                Ok(()) => {
                    let shared = &self.handle.shared;
                    shared.release(claim);
                    shared.counters.here().put_back();
                    Placed::Kept
                }
                Err(error) => {
                    tracing::warn!(
                        target: "helios_kernel::swap",
                        addr = addr.raw(),
                        ?error,
                        "the address space refused a page that no tier took; parking it"
                    );
                    self.park(claim)
                }
            };
        };
        let bytes = match encoding {
            DiskEncoding::Raw => &self.page[..],
            DiskEncoding::Lz4(len) => &self.block[..len.get()],
        };
        match disk.backend.swap_out(bytes).await {
            Ok(backend_token) => {
                let key = disk.insert(backend_token, encoding);
                let shared = &self.handle.shared;
                shared.restore(claim, SlotState::Disk(key));
                let counters = shared.counters.here();
                counters.hold(Held::Disk(encoding.bytes()));
                counters.page_out(match encoding {
                    DiskEncoding::Raw => Placement::DiskRaw,
                    DiskEncoding::Lz4(_) => Placement::DiskCompressed,
                });
                Placed::Out
            }
            Err(error) => {
                tracing::warn!(
                    target: "helios_kernel::swap",
                    addr = addr.raw(),
                    %error,
                    "swap backend refused a page; parking it"
                );
                self.park(claim)
            }
        }
    }

    /// Places the page in the staging buffer in the store's spare frame, raw.
    ///
    /// This is the placement of last resort. It allocates nothing and asks
    /// no device, and the spare was ensured before the page was detached.
    fn park(&mut self, claim: Claim) -> Placed {
        let shared = &self.handle.shared;
        let frame = self.store.park(&shared.store, &claim, &self.page);
        shared.restore(claim, SlotState::Parked(frame));
        let counters = shared.counters.here();
        counters.page_out(Placement::Parked);
        counters.hold(Held::Parked);
        Placed::Out
    }
}

/// The store's bounds under `policy`, against the pool as it is now: the
/// pool's total shrinks when the kernel heap borrows from it, so the bounds
/// are read for each decision.
fn store_limits(policy: &SwapPolicy) -> Limits {
    Limits {
        pool_frames: user_heap_stats().total_bytes / PhysFrame::SIZE,
        limit_percent: policy.store_limit_percent,
        accept_percent: policy.store_accept_percent,
    }
}

/// Reinstates the page in `source` at `addr`, which must still carry `token`
/// in its page-table entry.
fn reinstate(
    hooks: &SwapVmHooks,
    addr: VirtAddr,
    token: SwapToken,
    source: &[u8],
) -> Result<(), AddressSpaceError> {
    let mut source = source;
    (hooks.swap_in_page)(
        addr,
        token,
        (&raw mut source).cast::<()>(),
        |context, page| {
            // SAFETY: `context` is `source` above, alive for this call and
            // reached from nowhere else.
            let source = unsafe { *context.cast::<&[u8]>() };
            page.copy_from_slice(source);
        },
    )
}

struct EvictionPlan {
    pages: Vec<VirtAddr>,
    remaining: usize,
    /// Pages this pass may still look at. A scan that finds nothing
    /// cold must still end.
    visits_left: usize,
}

impl EvictionPlan {
    /// Returns `false` once the plan is full, which stops the scan.
    ///
    /// Every page the scan visits has had its access flag cleared by the
    /// address space, so a page passed over now is a candidate on the
    /// next pass if nothing touches it in between. That is the aging.
    fn consider(&mut self, addr: VirtAddr, age: PageAge) -> bool {
        if self.remaining < PhysFrame::SIZE || self.visits_left == 0 {
            return false;
        }
        self.visits_left -= 1;
        // Cold only, at every pressure level: a page the hardware has seen
        // since the last scan is in use, and a page that no tier takes is put
        // back hot, so taking hot pages would hand the same ones to every
        // pass.
        if age == PageAge::Cold {
            self.pages.push(addr);
            self.remaining -= PhysFrame::SIZE;
        }
        true
    }
}

/// Pressure as the user pool sees it, which is the pool swap reclaims
/// from and the one the balloon and the OOM killer read too.
fn current_pressure() -> PressureLevel {
    let heap = user_heap_stats();
    if heap.total_bytes == 0 {
        return PressureLevel::Green;
    }
    let free = heap.available_bytes() as f32 / heap.total_bytes as f32;
    PressureLevel::from_free_fraction(free)
}

#[cfg(test)]
mod lz4_tests;
#[cfg(test)]
mod service_tests;
#[cfg(test)]
mod slot_tests;
#[cfg(test)]
mod space_tests;
#[cfg(test)]
mod stack_tests;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
pub(crate) mod test_pool;
#[cfg(test)]
mod tier_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Polls `future` once, reporting whether it finished.
    fn poll_once(future: core::pin::Pin<&mut impl core::future::Future<Output = ()>>) -> bool {
        let waker = core::task::Waker::noop();
        let mut context = core::task::Context::from_waker(waker);
        matches!(future.poll(&mut context), core::task::Poll::Ready(()))
    }

    /// The swap service parks on its work signal between passes, which
    /// is the only point at which the processor it runs on gets to poll
    /// anything else. A signal that banks permits for waits that have
    /// not happened yet would make that park return immediately and turn
    /// the service loop into a spin inside a single `poll`.
    #[test]
    fn a_poke_releases_one_wait_and_the_next_one_parks() {
        let _turn = test_pool::serialise();
        let handle = SwapHandle::new("test", 0, test_clock(), &service_tests::TEST_HOOKS, 1);

        handle.poke();
        assert!(
            poll_once(core::pin::pin!(handle.shared.work.notified())),
            "the poke has to release the wait the service is parked on"
        );
        assert!(
            !poll_once(core::pin::pin!(handle.shared.work.notified())),
            "the service's next wait has to park, or its loop stops yielding"
        );
    }

    fn test_clock() -> SwapClock {
        SwapClock {
            timebase: Timebase::new(1_000_000_000),
            now_ticks: || 0,
        }
    }

    #[test]
    fn a_plan_stops_the_scan_once_the_budget_is_spent() {
        let mut plan = EvictionPlan {
            pages: Vec::new(),
            remaining: 2 * PhysFrame::SIZE,
            visits_left: SCAN_BUDGET_PAGES,
        };
        assert!(plan.consider(VirtAddr::new(0x1000), PageAge::Cold));
        assert!(plan.consider(VirtAddr::new(0x2000), PageAge::Cold));
        assert!(
            !plan.consider(VirtAddr::new(0x3000), PageAge::Cold),
            "a full plan must stop the scan"
        );
        assert_eq!(plan.pages.len(), 2);
    }

    #[test]
    fn aging_takes_cold_pages_and_passes_over_hot_ones() {
        let mut plan = EvictionPlan {
            pages: Vec::new(),
            remaining: 8 * PhysFrame::SIZE,
            visits_left: SCAN_BUDGET_PAGES,
        };
        plan.consider(VirtAddr::new(0x1000), PageAge::Hot);
        plan.consider(VirtAddr::new(0x2000), PageAge::Cold);
        plan.consider(VirtAddr::new(0x3000), PageAge::Hot);
        plan.consider(VirtAddr::new(0x4000), PageAge::Cold);
        assert_eq!(
            plan.pages,
            [VirtAddr::new(0x2000), VirtAddr::new(0x4000)],
            "only pages the hardware has not seen may be taken"
        );
    }

    #[test]
    fn a_scan_that_finds_nothing_cold_still_ends() {
        let mut plan = EvictionPlan {
            pages: Vec::new(),
            remaining: usize::MAX,
            visits_left: 3,
        };
        assert!(plan.consider(VirtAddr::new(0x1000), PageAge::Hot));
        assert!(plan.consider(VirtAddr::new(0x2000), PageAge::Hot));
        assert!(plan.consider(VirtAddr::new(0x3000), PageAge::Hot));
        assert!(
            !plan.consider(VirtAddr::new(0x4000), PageAge::Hot),
            "a pass must stop looking once its visit budget is spent"
        );
        assert!(plan.pages.is_empty());
    }

    #[test]
    fn a_fault_reply_reads_pending_until_it_is_settled() {
        let reply = SwapFaultReply::new();
        assert!(reply.read().is_none());
        reply.settle(FAULT_RESOLVED);
        assert_eq!(reply.read(), Some(Ok(())));
    }

    #[test]
    fn a_settled_fault_reply_carries_its_error() {
        let reply = SwapFaultReply::new();
        reply.settle(FAULT_BACKEND);
        assert_eq!(reply.read(), Some(Err(SwapFaultError::Backend)));
    }
}
