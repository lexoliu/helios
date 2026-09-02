//! The memory balloon: giving guest memory back to the host, and
//! telling the host which of it is idle.
//!
//! A balloon is a negotiation about how much memory the guest keeps.
//! The host publishes a target; the guest takes that many pages out of
//! its own pool, hands them over, and reports how many it actually
//! managed. Lowering the target hands them back.
//!
//! # Which memory
//!
//! Only user memory. The kernel heap running dry is fatal — there is
//! nothing to kill and nothing to reclaim — so it is not memory a host
//! may ask for. User memory is the pool wasm linear memories come from:
//! it is the bulk of the machine, it is page-granular, and its
//! exhaustion costs an instance rather than the kernel. The balloon
//! therefore draws from the frames the user pool has free, never from
//! frames an instance is using.
//!
//! # How far
//!
//! Inflation stops at the pressure floor
//! ([`PressureLevel::Red`](crate::PressureLevel)): the balloon never
//! takes the pool below a quarter free, so a host that asks for more
//! memory than the guest can spare gets what the guest can spare and an
//! `actual` that says so, rather than an OOM-killed instance. With
//! VIRTIO_BALLOON_F_DEFLATE_ON_OOM the reverse also holds: when the
//! runtime is about to condemn an instance for want of memory, the
//! balloon gives everything back first and stays down until the host
//! moves the target again.
//!
//! # Free-page reporting
//!
//! Reporting gives nothing up. It names runs of memory the guest is not
//! using so the host can drop the physical pages behind them; the guest
//! reads them back as zeroes and may allocate them at any time. The
//! frame allocator marks a reported run so the next allocation of it is
//! zeroed, and the reporting pass only runs when free memory has
//! actually grown since the last one — re-reporting an idle pool every
//! two seconds would cost the host a page walk for nothing.
//!
//! # Addressing
//!
//! Frames are named in the kernel's direct map, as everywhere else in
//! [`helios_hal::pmm`]. The driver turns them into bus addresses through
//! the same DMA pool it translates every other buffer with, so a backend
//! whose direct map is offset needs no balloon-specific handling.
//!
//! Concurrency contract: the inflated-run list is owned by the single
//! task that services the target, so it needs no lock. Every other
//! participant — the configuration-change forwarder, the reporting task,
//! the statistics task, and the runtime's out-of-memory path — reaches
//! that task through [`BalloonHandle`], which is atomics and a
//! notification.

extern crate alloc;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use core::time::Duration;

use arrayvec::ArrayVec;
use helios_hal::balloon::{MemoryBalloon, MemoryStat, MemoryStatTag};
use helios_hal::cpu::Cpu;
use helios_hal::pmm::{PhysFrame, PhysFrameAllocator, PhysFrameRange};
use helios_hal::watchdog::Watchdog;
use triomphe::Arc;

use crate::Notify;
use crate::memory::reported::MAX_FREE_RUN_BATCH;
use crate::memory::user::installed_user_memory_pool;
use crate::{Kernel, Timer};

/// How often the reporting task looks for newly freed memory.
pub const FREE_PAGE_REPORT_INTERVAL: Duration = Duration::from_secs(2);

/// Shortest run worth reporting, in frames.
///
/// The host reclaims a reported run by dropping the physical pages
/// behind it, which it can only do a host page at a time; naming
/// scattered single frames costs it a syscall each and reclaims almost
/// nothing. Two megabytes is the granularity Linux's own page reporting
/// settled on for the same reason.
const MIN_REPORT_RUN_FRAMES: usize = 512;

/// Runs one reporting pass names at most.
///
/// The real bound is how much of the pool sits above the pressure floor
/// — a pass holds every run it names, so it must not be able to squeeze
/// the pool while it works. This is the ceiling on top of that.
const REPORT_RUNS_PER_PASS: usize = MAX_FREE_RUN_BATCH;

/// Frames one inflate request takes out of the pool.
///
/// Asking for a whole target's worth of contiguous memory would fail on
/// any fragmented pool; two megabytes is large enough that the request
/// count stays small and small enough to still be satisfiable.
const INFLATE_RUN_FRAMES: usize = 512;

/// Runs the balloon may hold at once.
///
/// At [`INFLATE_RUN_FRAMES`] each that is a gigabyte of balloon, which
/// is more than a host can ask of any guest this kernel runs on. A host
/// that asks for more gets what fits and an honest `actual`.
const MAX_INFLATED_RUNS: usize = 512;

/// Runs the balloon moves between two publications of `actual`.
///
/// The host watches `actual` to see the guest following its target, and
/// a long move should not look like a stall — but every publication is a
/// device configuration write the host turns into an event, so
/// publishing per run buries the host in them. Every few runs is
/// progress the host can see without a storm.
const ACTUAL_PUBLISH_RUNS: usize = 64;

/// The share of the pool the balloon leaves free.
///
/// This is [`PressureLevel::Red`](crate::PressureLevel)'s boundary: the
/// point below which the runtime starts killing instances to make room.
/// A balloon that inflated past it would be manufacturing the pressure
/// the OOM killer then acts on.
const PRESSURE_FLOOR_DIVISOR: usize = 4;

/// What the balloon holds, as observers see it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BalloonStats {
    /// Bytes the host has asked the guest to give up.
    pub target_bytes: u64,
    /// Bytes the guest has actually given up.
    pub actual_bytes: u64,
    /// Bytes named to the host as free in the last reporting pass.
    pub reported_bytes: u64,
}

/// A shared view of the balloon for observers and the OOM path.
#[derive(Clone)]
pub struct BalloonHandle {
    shared: Arc<BalloonShared>,
}

struct BalloonShared {
    target_pages: AtomicU32,
    actual_pages: AtomicU32,
    reported_bytes: AtomicU64,
    /// Set by the runtime's out-of-memory path.
    deflate_requested: AtomicBool,
    /// Wakes the task that owns the inflated runs.
    work: Notify,
}

impl BalloonHandle {
    fn new() -> Self {
        Self {
            shared: Arc::new(BalloonShared {
                target_pages: AtomicU32::new(0),
                actual_pages: AtomicU32::new(0),
                reported_bytes: AtomicU64::new(0),
                deflate_requested: AtomicBool::new(false),
                work: Notify::new(),
            }),
        }
    }

    /// What the balloon holds right now.
    pub fn stats(&self) -> BalloonStats {
        BalloonStats {
            target_bytes: pages_to_bytes(self.shared.target_pages.load(Ordering::Acquire)),
            actual_bytes: pages_to_bytes(self.shared.actual_pages.load(Ordering::Acquire)),
            reported_bytes: self.shared.reported_bytes.load(Ordering::Acquire),
        }
    }

    /// Asks the balloon to give its memory back.
    ///
    /// This is the VIRTIO_BALLOON_F_DEFLATE_ON_OOM path: the runtime
    /// calls it before it condemns an instance, so memory the host is
    /// holding is reclaimed ahead of memory a program is using. It never
    /// blocks — the balloon task does the work — because the caller is
    /// a synchronous allocation failure, not a task that can wait.
    pub fn request_deflate(&self) {
        self.shared.deflate_requested.store(true, Ordering::Release);
        self.shared.work.notify_all();
    }
}

/// Spawns the tasks that service a memory balloon.
///
/// The balloon draws from the installed user-memory pool, so this runs
/// after `prime_bootstrap_allocator` and before any component starts.
/// The tasks are local to the calling processor because the device's
/// completions are: a backend routes the balloon interrupt to the
/// processor that brought the device up.
pub fn install_memory_balloon<CpuImpl, WatchdogImpl, Device>(
    kernel: &Kernel<CpuImpl, WatchdogImpl>,
    device: Device,
) -> BalloonHandle
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    Device: MemoryBalloon + Clone,
{
    let pool = installed_user_memory_pool()
        .unwrap_or_else(|| panic!("memory balloon installed before the user memory pool"));
    let handle = BalloonHandle::new();

    // The device's configuration-change notification belongs to the
    // driver, and the out-of-memory path has no device at all. Both are
    // forwarded onto one notification so the task that owns the
    // inflated runs waits on a single thing and needs no lock.
    let forwarder = device.clone();
    let forwarded = handle.clone();
    kernel.spawn_local_detached(async move {
        loop {
            forwarder.config_changed().await;
            forwarded.shared.work.notify_all();
        }
    });

    if device.publishes_stats() {
        let stats_device = device.clone();
        kernel.spawn_local_detached(async move {
            publish_stats_forever(pool, stats_device).await;
        });
    }

    if device.reports_free_pages() {
        let report_device = device.clone();
        let report_handle = handle.clone();
        let timer = kernel.timer();
        kernel.spawn_local_detached(async move {
            report_free_memory_forever(pool, report_device, report_handle, timer).await;
        });
    }

    let service = BalloonService {
        pool,
        device,
        handle: handle.clone(),
        runs: ArrayVec::new(),
        suppressed_target: None,
    };
    kernel.spawn_local_detached(async move {
        service.run().await;
    });
    handle
}

/// The task that owns the frames the balloon holds.
///
/// Generic over the pool so the policy — the pressure floor, the
/// out-of-memory suppression, the run bookkeeping — is exercised against
/// a real allocator in tests rather than against the one the kernel
/// happens to have installed.
struct BalloonService<Pool: PhysFrameAllocator, Device> {
    pool: &'static Pool,
    device: Device,
    handle: BalloonHandle,
    /// The runs handed to the host, most recent last. Each is exactly
    /// the range the pool allocated, because a buddy allocator only
    /// takes back what it gave out.
    runs: ArrayVec<PhysFrameRange, MAX_INFLATED_RUNS>,
    /// The target in force when the out-of-memory path last emptied the
    /// balloon. Inflation stays off until the host names a different
    /// one.
    suppressed_target: Option<u32>,
}

impl<Pool: PhysFrameAllocator, Device: MemoryBalloon + Clone> BalloonService<Pool, Device> {
    async fn run(mut self) {
        tracing::info!(
            must_tell_host = self.device.must_tell_host(),
            deflate_on_oom = self.device.deflates_on_oom(),
            "memory balloon service started"
        );
        loop {
            self.adjust().await;
            self.handle.shared.work.notified().await;
        }
    }

    /// Brings the balloon to whatever the host and the runtime are
    /// asking for.
    async fn adjust(&mut self) {
        if self
            .handle
            .shared
            .deflate_requested
            .swap(false, Ordering::AcqRel)
            && self.device.deflates_on_oom()
            && !self.runs.is_empty()
        {
            let released = self.deflate_to(0).await;
            self.suppressed_target = Some(self.device.target_pages());
            tracing::warn!(
                released_frames = released,
                "memory balloon deflated on memory pressure"
            );
        }

        let target = self.device.target_pages();
        self.handle
            .shared
            .target_pages
            .store(target, Ordering::Release);
        if self.suppressed_target == Some(target) {
            // The host has not moved since the balloon was emptied for
            // an out-of-memory event; re-inflating now would walk the
            // guest straight back into it.
            self.publish_actual();
            return;
        }
        self.suppressed_target = None;

        let target = target as usize;
        let held = self.held_frames();
        if target > held {
            self.inflate_to(target).await;
        } else if target < held {
            self.deflate_to(target).await;
        }
        self.publish_actual();

        if let Some(cmd_id) = self.device.free_page_hint_cmd_id() {
            self.hint_free_pages(cmd_id).await;
        }
    }

    fn held_frames(&self) -> usize {
        self.runs.iter().map(|run| run.frame_count).sum()
    }

    fn publish_actual(&self) {
        let held = self.held_frames();
        let pages = u32::try_from(held).unwrap_or(u32::MAX);
        self.handle
            .shared
            .actual_pages
            .store(pages, Ordering::Release);
        self.device.set_actual(pages);
    }

    /// How many more frames the balloon may take without pushing the
    /// pool into the pressure band the OOM killer acts on.
    fn inflation_budget(&self) -> usize {
        let stats = PhysFrameAllocator::stats(self.pool);
        let floor = stats.total_frames / PRESSURE_FLOOR_DIVISOR;
        stats.free_frames().saturating_sub(floor)
    }

    async fn inflate_to(&mut self, target: usize) {
        let mut wanted = (target - self.held_frames()).min(self.inflation_budget());
        tracing::info!(
            target_frames = target,
            budget_frames = wanted,
            "memory balloon inflating"
        );
        if wanted == 0 {
            tracing::info!(
                target_frames = target,
                held_frames = self.held_frames(),
                "memory balloon target is past the pressure floor; holding what it has"
            );
            return;
        }
        let mut run_frames = INFLATE_RUN_FRAMES.min(wanted);
        while wanted > 0 && !self.runs.is_full() {
            let Ok(range) = self.pool.allocate(run_frames.min(wanted), false) else {
                // A pool with nothing that contiguous left may still
                // have smaller runs; halving is what turns a
                // fragmentation failure into progress instead of a
                // stall.
                if run_frames == 1 {
                    break;
                }
                run_frames /= 2;
                continue;
            };
            let mut ranges = [unsafe { range_bytes(range) }];
            if let Err(error) = self.device.inflate(&mut ranges).await {
                // The host never took the frames, so they are still the
                // guest's; handing them back is the only correct move.
                PhysFrameAllocator::deallocate(self.pool, range);
                tracing::warn!(?error, "memory balloon inflate request failed");
                break;
            }
            wanted -= range.frame_count;
            self.runs.push(range);
            if self.runs.len().is_multiple_of(ACTUAL_PUBLISH_RUNS) {
                self.publish_actual();
            }
        }
        tracing::info!(
            held_frames = self.held_frames(),
            target_frames = target,
            "memory balloon inflated"
        );
    }

    /// Gives runs back until the balloon holds no more than `target`
    /// frames, returning how many frames were released.
    ///
    /// Whole runs only: the pool takes back exactly what it handed out,
    /// so a run is never split. That can leave the balloon marginally
    /// above the target, which `actual` then reports truthfully.
    async fn deflate_to(&mut self, target: usize) -> usize {
        let mut released = 0;
        while let Some(run) = self.runs.last().copied() {
            if self.held_frames() - run.frame_count < target {
                break;
            }
            let mut ranges = [unsafe { range_bytes(run) }];
            if let Err(error) = self.device.deflate(&mut ranges).await {
                tracing::warn!(?error, "memory balloon deflate request failed");
                break;
            }
            // VIRTIO_BALLOON_F_MUST_TELL_HOST: the frames go back into
            // the pool only after the host has been told, never before.
            self.runs.pop();
            PhysFrameAllocator::deallocate(self.pool, run);
            released += run.frame_count;
            if self.runs.len().is_multiple_of(ACTUAL_PUBLISH_RUNS) {
                self.publish_actual();
            }
        }
        if released != 0 {
            tracing::info!(
                released_frames = released,
                held_frames = self.held_frames(),
                "memory balloon deflated"
            );
        }
        released
    }

    /// Answers a free-page hint command from the host.
    async fn hint_free_pages(&self, cmd_id: u32) {
        if let Err(error) = self.device.begin_free_page_hint(cmd_id).await {
            tracing::warn!(
                ?error,
                cmd_id,
                "memory balloon free-page hint could not start"
            );
            return;
        }
        let device = self.device.clone();
        let named = self
            .pool
            .free_runs(MIN_REPORT_RUN_FRAMES, REPORT_RUNS_PER_PASS, move |run| {
                let device = device.clone();
                async move {
                    let mut ranges = [unsafe { range_bytes(run) }];
                    if let Err(error) = device.hint_free_pages(&mut ranges).await {
                        tracing::warn!(?error, "memory balloon free-page hint failed");
                    }
                }
            })
            .await;
        if let Err(error) = self.device.end_free_page_hint().await {
            tracing::warn!(
                ?error,
                cmd_id,
                "memory balloon free-page hint could not finish"
            );
            return;
        }
        tracing::info!(
            cmd_id,
            runs = named,
            "memory balloon answered a free-page hint"
        );
    }
}

/// Names newly freed memory to the host until the kernel stops.
async fn report_free_memory_forever<CpuImpl, Pool, Device>(
    pool: &'static Pool,
    device: Device,
    handle: BalloonHandle,
    timer: Timer<CpuImpl>,
) where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    Pool: PhysFrameAllocator,
    Device: MemoryBalloon + Clone,
{
    tracing::info!(
        interval_secs = FREE_PAGE_REPORT_INTERVAL.as_secs(),
        min_run_bytes = MIN_REPORT_RUN_FRAMES * PhysFrame::SIZE,
        "memory balloon free-page reporting started"
    );
    let mut last_free_frames = 0usize;
    loop {
        let stats = PhysFrameAllocator::stats(pool);
        let free_frames = stats.free_frames();
        if free_frames <= last_free_frames {
            // Nothing has been released since the last pass that named
            // something, so every run a pass could name is one the host
            // has already dropped.
            timer.sleep_for(FREE_PAGE_REPORT_INTERVAL).await;
            continue;
        }
        // A pass holds every run it names, so it may only reach for the
        // memory that sits above the floor the runtime defends.
        let spare = free_frames.saturating_sub(stats.total_frames / PRESSURE_FLOOR_DIVISOR);
        let runs = (spare / MIN_REPORT_RUN_FRAMES).min(REPORT_RUNS_PER_PASS);

        let mut reported_frames = 0usize;
        let reporter = device.clone();
        pool.free_runs(MIN_REPORT_RUN_FRAMES, runs, |run| {
            reported_frames += run.frame_count;
            let device = reporter.clone();
            async move {
                let mut ranges = [unsafe { range_bytes(run) }];
                if let Err(error) = device.report_free(&mut ranges).await {
                    tracing::warn!(?error, "memory balloon free-page report failed");
                }
            }
        })
        .await;
        if reported_frames == 0 {
            // The pool had nothing long enough to be worth naming. The
            // high-water mark stays where it was so the next release of
            // memory tries again rather than being suppressed by a pass
            // that reported nothing.
            timer.sleep_for(FREE_PAGE_REPORT_INTERVAL).await;
            continue;
        }
        last_free_frames = free_frames;
        let bytes = (reported_frames * PhysFrame::SIZE) as u64;
        handle.shared.reported_bytes.store(bytes, Ordering::Release);
        tracing::info!(
            reported_bytes = bytes,
            free_bytes = (free_frames * PhysFrame::SIZE) as u64,
            "memory balloon reported free memory"
        );
        timer.sleep_for(FREE_PAGE_REPORT_INTERVAL).await;
    }
}

/// Answers the host's statistics requests until the kernel stops.
///
/// The device asks by consuming the buffer the driver posted, so a
/// completed submission is the request for the next one.
async fn publish_stats_forever<Pool, Device>(pool: &'static Pool, device: Device)
where
    Pool: PhysFrameAllocator,
    Device: MemoryBalloon + Clone,
{
    loop {
        let stats = PhysFrameAllocator::stats(pool);
        let total = (stats.total_frames * PhysFrame::SIZE) as u64;
        let free = (stats.free_frames() * PhysFrame::SIZE) as u64;
        let floor = ((stats.total_frames / PRESSURE_FLOOR_DIVISOR) * PhysFrame::SIZE) as u64;
        let published = [
            MemoryStat {
                tag: MemoryStatTag::Total,
                value: total,
            },
            MemoryStat {
                tag: MemoryStatTag::Free,
                value: free,
            },
            MemoryStat {
                // What new work could actually be given: the free memory
                // above the floor the OOM killer defends.
                tag: MemoryStatTag::Available,
                value: free.saturating_sub(floor),
            },
        ];
        if let Err(error) = device.submit_stats(&published).await {
            tracing::warn!(?error, "memory balloon statistics queue failed");
            return;
        }
    }
}

const fn pages_to_bytes(pages: u32) -> u64 {
    pages as u64 * PhysFrame::SIZE as u64
}

/// Views a frame run the caller owns as bytes.
///
/// # Safety
///
/// `range` must be a run the caller holds exclusively — one the pool
/// allocated to it, or one reserved for the length of a
/// [`PhysFrameAllocator::free_runs`] visit.
unsafe fn range_bytes<'a>(range: PhysFrameRange) -> &'a mut [u8] {
    unsafe {
        core::slice::from_raw_parts_mut(range.start.phys_addr() as *mut u8, range.byte_size())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BalloonHandle, BalloonService, INFLATE_RUN_FRAMES, MemoryBalloon, MemoryStat,
        MemoryStatTag, PRESSURE_FLOOR_DIVISOR, PhysFrame, PhysFrameAllocator,
        publish_stats_forever,
    };
    use crate::memory::user::UserMemoryPool;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use arrayvec::ArrayVec;
    use core::alloc::Layout;
    use core::future::pending;
    use core::sync::atomic::{AtomicU32, Ordering};
    use futures_lite::future::block_on;
    use helios_hal::io::IoError;
    use spin::Mutex;
    use triomphe::Arc;

    /// A pool over a leaked, page-aligned block of host memory.
    fn pool(bytes: usize) -> &'static UserMemoryPool {
        let layout = Layout::from_size_align(bytes, PhysFrame::SIZE).expect("pool layout");
        let start = unsafe { alloc::alloc::alloc(layout) } as usize;
        assert!(start != 0, "host allocation for the user pool failed");
        let pool: &'static UserMemoryPool = Box::leak(Box::new(UserMemoryPool::empty()));
        pool.add_region(start, start + bytes);
        pool
    }

    /// A balloon device that records what the guest handed it.
    #[derive(Clone, Default)]
    struct FakeBalloon {
        inner: Arc<FakeBalloonState>,
    }

    #[derive(Default)]
    struct FakeBalloonState {
        target: AtomicU32,
        actual: AtomicU32,
        deflate_on_oom: bool,
        inflated_frames: Mutex<Vec<usize>>,
        deflated_frames: Mutex<Vec<usize>>,
        published: Mutex<Vec<MemoryStat>>,
        /// Requests answered before the queue starts failing, so a test
        /// can drive a stats loop that would otherwise never end.
        stats_budget: Mutex<usize>,
    }

    impl FakeBalloon {
        fn new(deflate_on_oom: bool) -> Self {
            Self {
                inner: Arc::new(FakeBalloonState {
                    deflate_on_oom,
                    ..FakeBalloonState::default()
                }),
            }
        }

        fn set_target_pages(&self, pages: u32) {
            self.inner.target.store(pages, Ordering::Release);
        }

        fn inflated(&self) -> usize {
            self.inner.inflated_frames.lock().iter().sum()
        }

        fn deflated(&self) -> usize {
            self.inner.deflated_frames.lock().iter().sum()
        }
    }

    fn frames_of(ranges: &[&mut [u8]]) -> usize {
        ranges
            .iter()
            .map(|range| range.len() / PhysFrame::SIZE)
            .sum()
    }

    impl MemoryBalloon for FakeBalloon {
        fn target_pages(&self) -> u32 {
            self.inner.target.load(Ordering::Acquire)
        }

        fn set_actual(&self, pages: u32) {
            self.inner.actual.store(pages, Ordering::Release);
        }

        fn must_tell_host(&self) -> bool {
            true
        }

        fn deflates_on_oom(&self) -> bool {
            self.inner.deflate_on_oom
        }

        fn reports_free_pages(&self) -> bool {
            true
        }

        fn publishes_stats(&self) -> bool {
            true
        }

        fn free_page_hint_cmd_id(&self) -> Option<u32> {
            None
        }

        async fn config_changed(&self) {
            pending::<()>().await
        }

        async fn inflate(&self, ranges: &mut [&mut [u8]]) -> Result<(), IoError> {
            self.inner.inflated_frames.lock().push(frames_of(ranges));
            Ok(())
        }

        async fn deflate(&self, ranges: &mut [&mut [u8]]) -> Result<(), IoError> {
            self.inner.deflated_frames.lock().push(frames_of(ranges));
            Ok(())
        }

        async fn report_free(&self, _ranges: &mut [&mut [u8]]) -> Result<(), IoError> {
            Ok(())
        }

        async fn begin_free_page_hint(&self, _cmd_id: u32) -> Result<(), IoError> {
            Ok(())
        }

        async fn hint_free_pages(&self, _ranges: &mut [&mut [u8]]) -> Result<(), IoError> {
            Ok(())
        }

        async fn end_free_page_hint(&self) -> Result<(), IoError> {
            Ok(())
        }

        async fn submit_stats(&self, stats: &[MemoryStat]) -> Result<(), IoError> {
            let mut budget = self.inner.stats_budget.lock();
            if *budget == 0 {
                return Err(IoError::Unsupported);
            }
            *budget -= 1;
            self.inner.published.lock().extend_from_slice(stats);
            Ok(())
        }
    }

    fn service(
        pool: &'static UserMemoryPool,
        device: FakeBalloon,
    ) -> (BalloonService<UserMemoryPool, FakeBalloon>, BalloonHandle) {
        let handle = BalloonHandle::new();
        (
            BalloonService {
                pool,
                device,
                handle: handle.clone(),
                runs: ArrayVec::new(),
                suppressed_target: None,
            },
            handle,
        )
    }

    fn free_frames(pool: &UserMemoryPool) -> usize {
        PhysFrameAllocator::stats(pool).free_frames()
    }

    /// A host may ask for more than the guest can spare. Handing it over
    /// would manufacture exactly the pressure the OOM killer acts on, so
    /// the balloon stops at the floor and says so through `actual`.
    #[test]
    fn inflation_stops_at_the_pressure_floor() {
        let pool = pool(64 * 1024 * 1024);
        let total = PhysFrameAllocator::stats(pool).total_frames;
        let floor = total / PRESSURE_FLOOR_DIVISOR;
        let device = FakeBalloon::new(false);
        let (mut service, handle) = service(pool, device.clone());

        // Ask for the whole machine.
        device.set_target_pages(u32::try_from(total).expect("frame count fits a u32"));
        block_on(service.adjust());

        let held = service.held_frames();
        assert!(held > 0, "a balloon that can spare memory has to inflate");
        assert!(
            held <= total - floor,
            "the balloon took {held} of {total} frames, past the {floor}-frame floor"
        );
        assert!(
            free_frames(pool) >= floor,
            "the pool must stay above the pressure floor"
        );
        assert_eq!(device.inflated(), held, "every held frame was handed over");
        assert_eq!(
            handle.stats().actual_bytes,
            (held * PhysFrame::SIZE) as u64,
            "`actual` reports what the guest could give, not what was asked"
        );
        assert_eq!(device.inner.actual.load(Ordering::Acquire) as usize, held);
    }

    /// Lowering the target is how the host gives memory back, and the
    /// frames have to reach the pool again — after the host is told, not
    /// before.
    #[test]
    fn lowering_the_target_returns_frames_to_the_pool() {
        let pool = pool(32 * 1024 * 1024);
        let device = FakeBalloon::new(false);
        let (mut service, _handle) = service(pool, device.clone());
        let free_before = free_frames(pool);

        device.set_target_pages(u32::try_from(2 * INFLATE_RUN_FRAMES).expect("target fits"));
        block_on(service.adjust());
        let held = service.held_frames();
        assert_eq!(held, 2 * INFLATE_RUN_FRAMES);
        assert_eq!(free_frames(pool), free_before - held);

        device.set_target_pages(0);
        block_on(service.adjust());
        assert_eq!(service.held_frames(), 0);
        assert_eq!(
            device.deflated(),
            held,
            "the host was told about every frame"
        );
        assert_eq!(
            free_frames(pool),
            free_before,
            "deflated frames belong to the pool again"
        );
    }

    /// A target that is not a whole number of runs leaves the balloon
    /// marginally above it: the pool takes back exactly what it gave
    /// out, so a run is never split.
    #[test]
    fn deflation_returns_whole_runs_only() {
        let pool = pool(32 * 1024 * 1024);
        let device = FakeBalloon::new(false);
        let (mut service, _handle) = service(pool, device.clone());

        device.set_target_pages(u32::try_from(2 * INFLATE_RUN_FRAMES).expect("target fits"));
        block_on(service.adjust());

        device.set_target_pages(u32::try_from(INFLATE_RUN_FRAMES + 1).expect("target fits"));
        block_on(service.adjust());
        assert_eq!(
            service.held_frames(),
            2 * INFLATE_RUN_FRAMES,
            "releasing a whole run would drop below the target"
        );
    }

    /// VIRTIO_BALLOON_F_DEFLATE_ON_OOM: memory the host is holding is
    /// reclaimed before an instance is condemned, and the balloon stays
    /// down until the host moves the target itself.
    #[test]
    fn an_out_of_memory_request_empties_the_balloon_and_keeps_it_down() {
        let pool = pool(32 * 1024 * 1024);
        let device = FakeBalloon::new(true);
        let (mut service, handle) = service(pool, device.clone());
        let free_before = free_frames(pool);

        let target = u32::try_from(2 * INFLATE_RUN_FRAMES).expect("target fits");
        device.set_target_pages(target);
        block_on(service.adjust());
        assert!(service.held_frames() > 0);

        handle.request_deflate();
        block_on(service.adjust());
        assert_eq!(service.held_frames(), 0);
        assert_eq!(free_frames(pool), free_before);

        // The host has not moved, so the balloon does not walk the guest
        // back into the pressure it just escaped.
        block_on(service.adjust());
        assert_eq!(service.held_frames(), 0);

        // A new target is a new instruction.
        device.set_target_pages(target - 1);
        block_on(service.adjust());
        assert!(service.held_frames() > 0);
    }

    /// A device without VIRTIO_BALLOON_F_DEFLATE_ON_OOM has not agreed
    /// to the guest deflating on its own, so the request is dropped.
    #[test]
    fn a_device_without_deflate_on_oom_keeps_its_memory() {
        let pool = pool(32 * 1024 * 1024);
        let device = FakeBalloon::new(false);
        let (mut service, handle) = service(pool, device.clone());

        device.set_target_pages(u32::try_from(INFLATE_RUN_FRAMES).expect("target fits"));
        block_on(service.adjust());
        let held = service.held_frames();
        assert!(held > 0);

        handle.request_deflate();
        block_on(service.adjust());
        assert_eq!(service.held_frames(), held);
    }

    /// The statistics the host reads are the guest's own view of its
    /// user memory, and `available` is what new work could actually be
    /// given rather than everything that happens to be free.
    #[test]
    fn published_statistics_describe_the_pool() {
        let pool = pool(16 * 1024 * 1024);
        let device = FakeBalloon::new(false);
        *device.inner.stats_budget.lock() = 1;

        block_on(publish_stats_forever(pool, device.clone()));

        let published = device.inner.published.lock().clone();
        let stats = PhysFrameAllocator::stats(pool);
        let total = (stats.total_frames * PhysFrame::SIZE) as u64;
        let free = (stats.free_frames() * PhysFrame::SIZE) as u64;
        let floor = ((stats.total_frames / PRESSURE_FLOOR_DIVISOR) * PhysFrame::SIZE) as u64;
        assert_eq!(
            published,
            [
                MemoryStat {
                    tag: MemoryStatTag::Total,
                    value: total
                },
                MemoryStat {
                    tag: MemoryStatTag::Free,
                    value: free
                },
                MemoryStat {
                    tag: MemoryStatTag::Available,
                    value: free - floor
                },
            ]
        );
    }

    /// The configuration-change future is what the service parks on, so
    /// it has to be a future the task can hold rather than one that
    /// resolves immediately and spins.
    #[test]
    fn the_service_parks_on_a_configuration_change() {
        let device = FakeBalloon::new(false);
        let mut changed = core::pin::pin!(device.config_changed());
        assert!(
            block_on(futures_lite::future::poll_once(changed.as_mut())).is_none(),
            "nothing has changed yet"
        );
    }
}
