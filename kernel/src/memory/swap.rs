//! Swap: the map from a page-table entry to its backing store, the
//! policy that decides what leaves memory, and the path a page takes
//! back in.
//!
//! # Shape
//!
//! One task owns everything. It holds the [`SwapBackend`], the swap map
//! (index → backend token), and the single page-sized staging buffer
//! that every transfer goes through, and it is the only thing that ever
//! talks to the swap device. Callers reach it through [`SwapHandle`],
//! which is a queue plus a notification: a page fault pushes a request
//! and waits, a teardown pushes tokens to release and does not.
//!
//! Serialising through one task is deliberate. There is one scratch
//! disk, the transfers are page-sized, and a single owner means the map
//! needs no lock and the staging buffer needs no allocation per fault.
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
//! access flag set, so it reads as hot for a full aging cycle and the
//! `Red` phase — which takes only cold pages — passes over it; and an
//! instance that has just taken a fault or made a host call is by
//! definition not ten seconds idle, so the `Yellow` phase, which
//! ignores age, has no claim on it either. That is what makes
//! [`SwapHandle::ensure_present`] a sound contract for kernel paths:
//! the pages it faults in stay in until the host call that asked for
//! them has finished with them.
//!
//! # Order against the balloon
//!
//! Both this and the memory balloon (`super::balloon`) read the same
//! [`PressureLevel`]. Swap acts first: at `Yellow` it evicts idle
//! instances, which costs a fault later, while the balloon at that point
//! only reports free pages to the host. At `Red` the balloon is already
//! forbidden to inflate past its floor and deflates on demand from the
//! out-of-memory path, so the ordering under pressure is: give the host
//! back nothing, swap cold pages out, and only then let the OOM killer
//! condemn an instance. A swap-out pass that frees nothing is what
//! hands the decision on.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use core::time::Duration;

use concurrent_queue::ConcurrentQueue;
use helios_hal::cpu::{Cpu, Timebase};
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{
    AddressSpaceError, PageAge, PageFlags, SwapBackend, SwapToken, VirtAddr, VirtRange,
};
use spin::Once;
use thiserror::Error;

use crate::exec::Spawner;
use crate::exec::{PressureLevel, monotonic_nanos};
use crate::instance::InstanceRegistry;
use crate::{Notify, Timer};

use super::owner::MemoryOwner;
use super::user::user_heap_stats;

/// How long an instance has to have been off every processor before a
/// `Yellow` pass will evict it. Ten seconds is long enough that an
/// instance between two guest calls is never mistaken for an idle one,
/// and short enough that a shell left open for a minute pays for itself.
pub const IDLE_SWAP_AFTER: Duration = Duration::from_secs(10);

/// Bytes one pass moves before it re-reads the pressure level. A pass
/// holds no lock across its transfers, but it does hold the swap device,
/// so it stops often enough for a fault to overtake it.
pub const SWAP_BATCH_BYTES: usize = 2 * 1024 * 1024;

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
    #[error("swap map has no entry for the token the page table carried")]
    UnknownToken,
    #[error("the swap backend could not read the page back")]
    Backend,
    #[error("the address space refused to reinstate the page")]
    AddressSpace,
    #[error("the page's contents were lost: neither memory nor the swap backend holds them")]
    PageLost,
}

/// What swap is holding, as observers see it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwapStats {
    /// Name of the backing store, for the stats panel.
    pub backend: &'static str,
    /// Bytes the backing store can hold in total.
    pub capacity_bytes: u64,
    /// Bytes of it currently holding swapped-out pages.
    pub used_bytes: u64,
    /// Pages written out since boot.
    pub pages_out: u64,
    /// Pages read back in since boot.
    pub pages_in: u64,
    /// Guest faults resolved by reading a page back.
    pub faults_served: u64,
    /// Mean time a faulting guest waited, over every fault served.
    pub mean_fault_latency_nanos: u64,
}

/// The platform address space's swap surface, as plain function
/// pointers.
///
/// Same shape and reason as `crate::runtime_memory`'s hook table: the
/// kernel is not generic over the address space (there is exactly one
/// per machine, chosen at link time), and a `dyn AddressSpace` would put
/// a vtable on the page-fault path. The backend builds one `&'static`
/// table and installs it at boot.
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
    pub swap_out_page: fn(VirtAddr, SwapToken, &mut [u8]) -> Result<PageFlags, AddressSpaceError>,
    /// Reinstate one page from the caller's buffer.
    pub swap_in_page: fn(VirtAddr, &[u8]) -> Result<SwapToken, AddressSpaceError>,
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

/// What [`SwapVmHooks::scan_committed_pages`] calls for each page it
/// walks: the caller's context, the page, its flags and its age.
/// Returning `false` ends the scan.
pub type CommittedPageVisitor = fn(*mut (), VirtAddr, PageFlags, PageAge) -> bool;

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
    counters: SwapCounters,
    backend: &'static str,
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

#[derive(Default)]
struct SwapCounters {
    capacity_bytes: AtomicU64,
    used_bytes: AtomicU64,
    pages_out: AtomicU64,
    pages_in: AtomicU64,
    faults_served: AtomicU64,
    fault_latency_total_nanos: AtomicU64,
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
const FAULT_UNKNOWN_TOKEN: u8 = 2;
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
            FAULT_UNKNOWN_TOKEN => Some(Err(SwapFaultError::UnknownToken)),
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

impl SwapHandle {
    fn new(backend: &'static str, capacity_bytes: u64, clock: SwapClock) -> Self {
        let counters = SwapCounters::default();
        counters
            .capacity_bytes
            .store(capacity_bytes, Ordering::Release);
        Self {
            shared: Arc::new(SwapShared {
                faults: ConcurrentQueue::bounded(MAX_INFLIGHT_FAULTS),
                work: Notify::new(),
                counters,
                backend,
                clock,
            }),
        }
    }

    /// What swap is holding right now.
    pub fn stats(&self) -> SwapStats {
        let counters = &self.shared.counters;
        let faults_served = counters.faults_served.load(Ordering::Acquire);
        let latency_total = counters.fault_latency_total_nanos.load(Ordering::Acquire);
        SwapStats {
            backend: self.shared.backend,
            capacity_bytes: counters.capacity_bytes.load(Ordering::Acquire),
            used_bytes: counters.used_bytes.load(Ordering::Acquire),
            pages_out: counters.pages_out.load(Ordering::Acquire),
            pages_in: counters.pages_in.load(Ordering::Acquire),
            faults_served,
            mean_fault_latency_nanos: latency_total.checked_div(faults_served).unwrap_or(0),
        }
    }

    /// Reads the page at `addr` back into memory, waiting for the swap
    /// device.
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
    pub async fn fault_in(&self, addr: VirtAddr) -> Result<(), SwapFaultError> {
        // A fault reports the address the instruction touched, not the
        // page it lives in; everything below this point works in pages.
        let addr = addr.page_floor();
        let reply = Arc::new(SwapFaultReply::new());
        let request = SwapFaultRequest {
            addr,
            reply: reply.clone(),
            issued_nanos: self.shared.clock.now_nanos(),
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

    /// Reads back every swapped-out page in `range`.
    ///
    /// Kernel code that touches user memory directly — a host call
    /// copying a guest buffer, a byte channel taking bytes out of guest
    /// memory, a DMA source built over a guest slice — calls this before
    /// the copy. Kernel stacks have no fault trampoline, so a fault
    /// there is fatal by design; pre-faulting is how those paths stay
    /// correct.
    pub async fn ensure_present(&self, range: VirtRange) -> Result<(), SwapFaultError> {
        let Some(hooks) = installed_swap_hooks() else {
            return Ok(());
        };
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
) -> Result<SwapHandle, Backend::Error>
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    Backend: SwapBackend,
{
    let hooks = installed_swap_hooks().unwrap_or_else(|| {
        panic!("swap backend installed before the address space published its swap hooks")
    });
    // The staging page is the backend's: allocated before anything is
    // published, so a backend that cannot supply it leaves swap
    // uninstalled instead of half-installed.
    let page = backend.buffer(PhysFrame::SIZE)?;
    let clock = SwapClock {
        timebase: cpu.timebase(),
        now_ticks: hooks.now_ticks,
    };
    let handle = SwapHandle::new(backend_name, capacity_bytes, clock);
    let mut published = false;
    HANDLE.call_once(|| {
        published = true;
        handle.clone()
    });
    assert!(published, "swap backend installed more than once");

    let service = SwapService {
        backend,
        handle: handle.clone(),
        hooks,
        registry,
        cpu,
        map: SwapMap::new(),
        page: Some(page),
        report: SwapReport::default(),
    };
    spawner.spawn_local_detached(async move {
        service.run(timer).await;
    });
    tracing::info!(
        target: "helios_kernel::swap",
        backend = backend_name,
        capacity_bytes,
        "swap online"
    );
    Ok(handle)
}

/// What an index of the swap map holds.
#[derive(Clone, Copy)]
enum Slot<Token> {
    /// On the free list, or reserved and not yet filled, or its token was
    /// consumed by a read whose page is on its way back into memory.
    Vacant,
    /// The page is in the backend under this token.
    Filled(Token),
    /// The page's bytes are gone: its address space was refused the
    /// reinstatement and the backend refused the page back. The index
    /// stays reserved, because the page-table entry still names it, until
    /// the address space is torn down.
    Lost,
}

/// What the map knows about an index a page-table entry carries.
enum Lookup<Token> {
    Filled(Token),
    Lost,
    Vacant,
}

/// Index → backend token, with a free list so a released index is
/// reused before the map grows.
///
/// An index belongs to its page from [`Self::reserve`] until the page is
/// back in memory ([`Self::free`]) or its address space is torn down
/// ([`Self::release_for_teardown`]); nothing in between returns it to the
/// free list, because the page-table entry names it for that whole time.
///
/// Index 0 is never handed out: [`SwapToken`] is non-zero so an all-zero
/// page-table entry keeps meaning "nothing was ever mapped here".
struct SwapMap<Token> {
    slots: Vec<Slot<Token>>,
    free: Vec<u32>,
}

impl<Token: Copy> SwapMap<Token> {
    fn new() -> Self {
        Self {
            slots: Vec::from([Slot::Vacant]),
            free: Vec::new(),
        }
    }

    /// Claims an index before the page is written out.
    ///
    /// The index has to exist first: it is what the page-table entry
    /// carries, and the entry has to be in place before the frame goes
    /// back to the pool. The slot stays vacant until [`Self::fill`], and
    /// a fault cannot observe the gap because this task serves faults
    /// and swap-outs from the same loop.
    fn reserve(&mut self) -> Option<SwapToken> {
        if let Some(index) = self.free.pop() {
            return SwapToken::new(index);
        }
        let index = u32::try_from(self.slots.len()).ok()?;
        self.slots.push(Slot::Vacant);
        SwapToken::new(index)
    }

    /// Records where the page is. The index keeps its page: filling it
    /// again after a read consumed its extent is how a page goes back to
    /// the backend under the index its page-table entry carries.
    fn fill(&mut self, handle: SwapToken, token: Token) {
        self.slots[handle.raw() as usize] = Slot::Filled(token);
    }

    fn lookup(&self, handle: SwapToken) -> Lookup<Token> {
        match self.slots.get(handle.raw() as usize) {
            Some(Slot::Filled(token)) => Lookup::Filled(*token),
            Some(Slot::Lost) => Lookup::Lost,
            Some(Slot::Vacant) | None => Lookup::Vacant,
        }
    }

    /// Forgets the token of a page the backend has just handed back and
    /// freed, so nothing can read a token whose extent is gone. The index
    /// stays reserved.
    fn vacate(&mut self, handle: SwapToken) {
        self.slots[handle.raw() as usize] = Slot::Vacant;
    }

    /// Records that the page's bytes no longer exist anywhere. The index
    /// stays reserved.
    fn mark_lost(&mut self, handle: SwapToken) {
        self.slots[handle.raw() as usize] = Slot::Lost;
    }

    /// Gives an index back to the free list once nothing refers to it: the
    /// page was never detached, or it is resident again.
    fn free(&mut self, handle: SwapToken) {
        self.slots[handle.raw() as usize] = Slot::Vacant;
        self.free.push(handle.raw());
    }

    /// Gives back an index whose address space is gone, returning the
    /// token the backend still holds for it, if any. A lost index holds
    /// none. An index that is already vacant was either never filled or
    /// is already free, and is left alone.
    fn release_for_teardown(&mut self, handle: SwapToken) -> Option<Token> {
        let slot = self.slots.get_mut(handle.raw() as usize)?;
        let held = match *slot {
            Slot::Filled(token) => Some(token),
            Slot::Lost => None,
            Slot::Vacant => return None,
        };
        *slot = Slot::Vacant;
        self.free.push(handle.raw());
        held
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| matches!(slot, Slot::Filled(_)))
            .count()
    }
}

struct SwapService<Backend: SwapBackend, CpuImpl> {
    backend: Backend,
    handle: SwapHandle,
    hooks: &'static SwapVmHooks,
    registry: InstanceRegistry,
    cpu: CpuImpl,
    map: SwapMap<Backend::Token>,
    /// The one staging buffer every transfer goes through. Owning it
    /// here is what keeps the fault path allocation-free. The backend
    /// allocated it, so it is memory the storage behind the backend can
    /// transfer from and to in place; it may be longer than a page when
    /// the storage works in whole units, and only its first page is the
    /// page.
    ///
    /// A transfer moves it into the backend and gets it back with the
    /// result. `None` only after a transfer whose device stopped answering
    /// kept it (see [`helios_hal::dma`]): the next transfer asks the
    /// backend for a new one ([`SwapService::take_page`]).
    page: Option<Backend::Buffer>,
    /// What the last summary line reported, so the next one can report
    /// the difference. See [`SWAP_REPORT_INTERVAL`].
    report: SwapReport,
}

/// The running total the periodic summary line subtracts from.
#[derive(Clone, Copy, Default)]
struct SwapReport {
    at_nanos: u64,
    pages_in: u64,
    pages_out: u64,
    fault_latency_total_nanos: u64,
}

impl<Backend, CpuImpl> SwapService<Backend, CpuImpl>
where
    Backend: SwapBackend,
    CpuImpl: Cpu + Clone,
{
    /// The staging buffer, for one transfer. It is back in `self.page`
    /// once the transfer returns it; a device that kept the last one is
    /// replaced here rather than at the next allocation-free fault.
    fn take_page(&mut self) -> Result<Backend::Buffer, Backend::Error> {
        match self.page.take() {
            Some(page) => Ok(page),
            None => self.backend.buffer(PhysFrame::SIZE),
        }
    }

    async fn run(mut self, timer: Timer<CpuImpl>) -> ! {
        loop {
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
            let outcome = self.swap_in(request.addr).await;
            // From the fault's issue, not from the moment this loop got
            // to it: the guest has been stopped the whole time.
            let elapsed = self
                .handle
                .shared
                .clock
                .now_nanos()
                .saturating_sub(request.issued_nanos);
            let counters = &self.handle.shared.counters;
            counters.faults_served.fetch_add(1, Ordering::AcqRel);
            counters
                .fault_latency_total_nanos
                .fetch_add(elapsed, Ordering::AcqRel);
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
                        SwapFaultError::UnknownToken => FAULT_UNKNOWN_TOKEN,
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
        let counters = &self.handle.shared.counters;
        let current = SwapReport {
            at_nanos: now,
            pages_in: counters.pages_in.load(Ordering::Acquire),
            pages_out: counters.pages_out.load(Ordering::Acquire),
            fault_latency_total_nanos: counters.fault_latency_total_nanos.load(Ordering::Acquire),
        };
        let pages_in = current.pages_in - self.report.pages_in;
        let pages_out = current.pages_out - self.report.pages_out;
        // A quiet interval is the normal case and says nothing worth a
        // line; the window it would have covered rolls into the next.
        if pages_in == 0 && pages_out == 0 {
            return;
        }
        let latency = current.fault_latency_total_nanos - self.report.fault_latency_total_nanos;
        self.report = current;
        tracing::info!(
            target: "helios_kernel::swap",
            pages_in,
            pages_out,
            mean_fault_latency_nanos = latency.checked_div(pages_in).unwrap_or(0),
            used_bytes = counters.used_bytes.load(Ordering::Acquire),
            "swap active"
        );
    }

    async fn swap_in(&mut self, addr: VirtAddr) -> Result<(), SwapFaultError> {
        // A request exists only for an address whose entry carried a token
        // when the fault was taken (the backend's fault entry checks
        // `swapped_token` before it enters the trampoline, and
        // `ensure_present` before it asks). One that carries none now has
        // changed since: an earlier request reinstated the page, which is
        // the case of several guest threads or fibers faulting on one
        // page, or the address space dropped the entry. Either way the
        // retried instruction takes the fault again and a fresh fault is
        // classified from scratch, so succeeding cannot loop.
        let Some(handle) = (self.hooks.swapped_token)(addr) else {
            return Ok(());
        };
        let token = match self.map.lookup(handle) {
            Lookup::Filled(token) => token,
            Lookup::Lost => return Err(SwapFaultError::PageLost),
            Lookup::Vacant => return Err(SwapFaultError::UnknownToken),
        };
        let page = self.take_page().map_err(|error| {
            tracing::warn!(
                target: "helios_kernel::swap",
                addr = addr.raw(),
                %error,
                "swap backend could not supply a staging page"
            );
            SwapFaultError::Backend
        })?;
        // A failed read leaves the extent with the backend and the index
        // with the page-table entry, exactly as they were.
        let page = match self.backend.swap_in(token, page).await {
            Ok(((), page)) => page,
            Err(failure) => {
                tracing::warn!(
                    target: "helios_kernel::swap",
                    addr = addr.raw(),
                    error = %failure.error,
                    "swap backend could not read a page back"
                );
                self.page = failure.buffers;
                return Err(SwapFaultError::Backend);
            }
        };
        // The backend has consumed the extent, so the token is dead and
        // the staging buffer holds the page's only copy.
        self.map.vacate(handle);
        // The address space reads the flags the page had out of its own
        // bookkeeping, fills a fresh frame from the staging buffer, and
        // only publishes the mapping once the bytes are there.
        if let Err(error) = (self.hooks.swap_in_page)(addr, &page.as_ref()[..PhysFrame::SIZE]) {
            tracing::warn!(
                target: "helios_kernel::swap",
                addr = addr.raw(),
                ?error,
                "the address space refused to reinstate a page; returning it to the swap backend"
            );
            // `rehome` takes the page and puts it back in `self.page`.
            if self.rehome(handle, addr, page).await.is_err() {
                self.handle
                    .shared
                    .counters
                    .used_bytes
                    .fetch_sub(PhysFrame::SIZE as u64, Ordering::AcqRel);
            }
            return Err(SwapFaultError::AddressSpace);
        }
        self.page = Some(page);
        self.map.free(handle);
        let counters = &self.handle.shared.counters;
        counters.pages_in.fetch_add(1, Ordering::AcqRel);
        counters
            .used_bytes
            .fetch_sub(PhysFrame::SIZE as u64, Ordering::AcqRel);
        Ok(())
    }

    /// Writes `page`, the staging buffer holding the only copy of the page
    /// whose page-table entry carries `handle`, back to the backend under
    /// that same index. The buffer comes back to `self.page` whenever the
    /// device returns it.
    ///
    /// When the backend refuses too, the page's bytes exist nowhere: the
    /// index is marked lost and stays reserved, so no other page can be
    /// handed it and a fault on it fails rather than reading a stale
    /// token, until the owning address space's teardown releases it.
    async fn rehome(
        &mut self,
        handle: SwapToken,
        addr: VirtAddr,
        page: Backend::Buffer,
    ) -> Result<(), ()> {
        match self.backend.swap_out(page).await {
            Ok((token, page)) => {
                self.page = Some(page);
                self.map.fill(handle, token);
                Ok(())
            }
            Err(failure) => {
                self.page = failure.buffers;
                self.map.mark_lost(handle);
                tracing::error!(
                    target: "helios_kernel::swap",
                    addr = addr.raw(),
                    index = handle.raw(),
                    error = %failure.error,
                    "swapped-out page lost: the swap backend refused it back and no other copy exists"
                );
                Err(())
            }
        }
    }

    async fn release_orphaned_tokens(&mut self) {
        let mut orphans: Vec<SwapToken> = Vec::new();
        (self.hooks.drain_orphaned_swap_tokens)(
            (&raw mut orphans).cast::<()>(),
            |context, token| {
                // SAFETY: `context` is the `Vec` above, alive for this
                // call and reached from nowhere else.
                let orphans = unsafe { &mut *context.cast::<Vec<SwapToken>>() };
                orphans.push(token);
            },
        );
        for handle in orphans {
            // A lost page holds no extent and no `used_bytes`: both were
            // given up when it was lost, and only its reservation is
            // released here.
            let Some(token) = self.map.release_for_teardown(handle) else {
                continue;
            };
            self.backend.release(token).await;
            self.handle
                .shared
                .counters
                .used_bytes
                .fetch_sub(PhysFrame::SIZE as u64, Ordering::AcqRel);
        }
    }

    async fn run_policy_pass(&mut self) {
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
        let idle_after = IDLE_SWAP_AFTER.as_nanos() as u64;
        let mut candidates: Vec<MemoryOwner> = self
            .registry
            .idle_instances(now, idle_after)
            .into_iter()
            .map(|id| MemoryOwner::new(id.raw()))
            .collect();
        // Largest resident first: one pass over the biggest idle
        // instance frees more than several passes over small ones, and
        // resident bytes come from the address space rather than from
        // what the instance thinks it grew to.
        candidates.sort_by_key(|owner| {
            core::cmp::Reverse((self.hooks.owned_resident_bytes)(owner.raw()))
        });

        let mut moved = 0_usize;
        for owner in &candidates {
            if moved >= SWAP_BATCH_BYTES {
                break;
            }
            moved += self
                .evict_owner(*owner, SWAP_BATCH_BYTES - moved, EvictionAge::Any)
                .await;
        }

        if pressure != PressureLevel::Red {
            tracing::debug!(
                target: "helios_kernel::swap",
                bytes = moved,
                idle_instances = candidates.len(),
                "swapped idle instances out under yellow pressure"
            );
            return;
        }

        // Red: ten seconds of idleness was not enough of a bar. Drop it
        // to "not on a processor right now" and take only the pages the
        // hardware has not seen since the last pass. A pass that frees
        // nothing is the signal the OOM killer acts on — it is not this
        // task's job to condemn anything.
        //
        // The bar stays at "not executing" rather than dropping to
        // "anything": while an instance is on a processor it may be
        // inside a host call, whose frame already holds that fiber's
        // blocking context and so cannot block again to fault a page
        // back in. Never detaching a page from an executing instance is
        // what keeps the kernel out of that corner, and it is why the
        // pre-fault accessor is a contract for kernel paths that hold
        // guest pointers rather than a thing every host call must do.
        for id in self.registry.idle_instances(now, 0) {
            if moved >= SWAP_BATCH_BYTES {
                break;
            }
            moved += self
                .evict_owner(
                    MemoryOwner::new(id.raw()),
                    SWAP_BATCH_BYTES - moved,
                    EvictionAge::ColdOnly,
                )
                .await;
        }
        tracing::debug!(
            target: "helios_kernel::swap",
            bytes = moved,
            "swap pass under red pressure"
        );
    }

    /// Swaps up to `budget` bytes of `owner`'s pages out, and reports
    /// how many bytes actually left memory.
    async fn evict_owner(&mut self, owner: MemoryOwner, budget: usize, age: EvictionAge) -> usize {
        let mut plan = EvictionPlan {
            pages: Vec::new(),
            remaining: budget,
            visits_left: SCAN_BUDGET_PAGES,
            age,
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
            match self.swap_out(addr).await {
                Ok(()) => moved += PhysFrame::SIZE,
                Err(()) => break,
            }
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

    async fn swap_out(&mut self, addr: VirtAddr) -> Result<(), ()> {
        // The index has to exist before the page can be detached: the
        // page-table entry carries it, and the entry has to be in place
        // before the frame goes back to the pool.
        let Some(handle) = self.map.reserve() else {
            tracing::warn!(
                target: "helios_kernel::swap",
                addr = addr.raw(),
                "swap map is full"
            );
            return Err(());
        };
        let mut page = match self.take_page() {
            Ok(page) => page,
            Err(error) => {
                tracing::warn!(
                    target: "helios_kernel::swap",
                    addr = addr.raw(),
                    %error,
                    "swap backend could not supply a staging page"
                );
                // Nothing was detached, so the index goes straight back.
                self.map.free(handle);
                return Err(());
            }
        };
        // Detaching copies the page into our buffer, files the entry,
        // hands the frame back and shoots the TLB down, all under the
        // address space's own lock — so nothing can write the page
        // between the copy and the unmapping.
        if let Err(error) =
            (self.hooks.swap_out_page)(addr, handle, &mut page.as_mut()[..PhysFrame::SIZE])
        {
            tracing::debug!(
                target: "helios_kernel::swap",
                addr = addr.raw(),
                ?error,
                "page could not be detached for swap-out"
            );
            self.page = Some(page);
            self.map.free(handle);
            return Err(());
        }
        // From here the page is not present and its entry points at an
        // empty slot. A guest faulting on it in this window queues a
        // request that this same loop serves, and it only gets there
        // after this function returns, so the gap is never observed.
        match self.backend.swap_out(page).await {
            Ok((token, page)) => {
                self.page = Some(page);
                self.map.fill(handle, token);
            }
            Err(failure) => {
                // The device stopped answering with the page in flight and
                // still holds the only copy of it: the page's bytes exist
                // nowhere the kernel can reach, so the index is lost.
                let Some(page) = failure.buffers else {
                    self.map.mark_lost(handle);
                    tracing::error!(
                        target: "helios_kernel::swap",
                        addr = addr.raw(),
                        index = handle.raw(),
                        error = %failure.error,
                        "swap device stopped answering while it held a page; the page is lost"
                    );
                    return Err(());
                };
                tracing::warn!(
                    target: "helios_kernel::swap",
                    addr = addr.raw(),
                    error = %failure.error,
                    "swap backend refused a page; putting it back"
                );
                // The staging buffer holds the page's only copy. The index
                // is released only once the page is resident again.
                match (self.hooks.swap_in_page)(addr, &page.as_ref()[..PhysFrame::SIZE]) {
                    Ok(_) => {
                        self.page = Some(page);
                        self.map.free(handle);
                        return Err(());
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "helios_kernel::swap",
                            addr = addr.raw(),
                            ?error,
                            "the address space refused a page put back after a refused write; \
                             offering it to the swap backend again"
                        );
                        // The backend may have taken the write this time,
                        // which makes this a completed swap-out; if not,
                        // the page is lost and says so.
                        self.rehome(handle, addr, page).await?;
                    }
                }
            }
        }
        let counters = &self.handle.shared.counters;
        counters.pages_out.fetch_add(1, Ordering::AcqRel);
        counters
            .used_bytes
            .fetch_add(PhysFrame::SIZE as u64, Ordering::AcqRel);
        Ok(())
    }
}

/// Which pages an eviction pass is allowed to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EvictionAge {
    /// Everything: the owner is idle, so nothing it holds is hot.
    Any,
    /// Only pages the hardware has not touched since the last scan.
    ColdOnly,
}

struct EvictionPlan {
    pages: Vec<VirtAddr>,
    remaining: usize,
    /// Pages this pass may still look at. A scan that finds nothing
    /// cold must still end.
    visits_left: usize,
    age: EvictionAge,
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
        let take = match self.age {
            EvictionAge::Any => true,
            EvictionAge::ColdOnly => age == PageAge::Cold,
        };
        if take {
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
mod service_tests;

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
        let handle = SwapHandle::new("test", 0, test_clock());

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

    fn insert<Token: Copy>(map: &mut SwapMap<Token>, token: Token) -> SwapToken {
        let handle = map.reserve().expect("index");
        map.fill(handle, token);
        handle
    }

    #[test]
    fn the_map_never_hands_out_a_zero_index() {
        let mut map = SwapMap::new();
        let first = insert(&mut map, 11_u32);
        assert_ne!(first.raw(), 0);
        assert_eq!(map.release_for_teardown(first), Some(11));
    }

    #[test]
    fn the_map_reuses_a_released_index() {
        let mut map = SwapMap::new();
        let first = insert(&mut map, 1_u32);
        let second = insert(&mut map, 2_u32);
        assert_ne!(first, second);
        assert_eq!(map.release_for_teardown(first), Some(1));
        let third = insert(&mut map, 3_u32);
        assert_eq!(
            third, first,
            "a released index must be reused before growing"
        );
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn releasing_an_index_twice_reports_nothing() {
        let mut map = SwapMap::new();
        let handle = insert(&mut map, 5_u32);
        assert_eq!(map.release_for_teardown(handle), Some(5));
        assert_eq!(map.release_for_teardown(handle), None);
        assert_eq!(
            map.free
                .iter()
                .filter(|&&index| index == handle.raw())
                .count(),
            1,
            "an index is on the free list once"
        );
    }

    #[test]
    fn a_freed_index_is_never_filled() {
        let mut map: SwapMap<u32> = SwapMap::new();
        let handle = map.reserve().expect("index");
        map.free(handle);
        assert_eq!(map.release_for_teardown(handle), None);
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn a_lost_index_is_not_reused_until_teardown() {
        let mut map: SwapMap<u32> = SwapMap::new();
        let lost = insert(&mut map, 9_u32);
        map.mark_lost(lost);
        assert_ne!(map.reserve().expect("index"), lost);
        assert_eq!(map.release_for_teardown(lost), None);
        assert_eq!(map.reserve().expect("index"), lost);
    }

    #[test]
    fn an_idle_plan_takes_every_page_until_the_budget_runs_out() {
        let mut plan = EvictionPlan {
            pages: Vec::new(),
            remaining: 2 * PhysFrame::SIZE,
            visits_left: SCAN_BUDGET_PAGES,
            age: EvictionAge::Any,
        };
        assert!(plan.consider(VirtAddr::new(0x1000), PageAge::Hot));
        assert!(plan.consider(VirtAddr::new(0x2000), PageAge::Hot));
        assert!(
            !plan.consider(VirtAddr::new(0x3000), PageAge::Hot),
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
            age: EvictionAge::ColdOnly,
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
            age: EvictionAge::ColdOnly,
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
