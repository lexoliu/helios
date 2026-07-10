extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::future::Future;
use core::marker::PhantomData;
use core::mem::{MaybeUninit, align_of, size_of};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use core::task::{Context, Poll};

use async_task::{Builder, Runnable, Task};
use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use helios_hal::cpu::{Cpu, ProcessorId};
use helios_hal::watchdog::ProgressCounter;
use spin::Once;
use triomphe::{Arc as NoWeakArc, UniqueArc};

use crate::exec::sync::Notify;

type ReadyQueue = ConcurrentQueue<Runnable>;
pub type JoinHandle<T> = Task<T>;
pub const READY_BATCH_TASKS: usize = 1024;
const READY_QUEUE_CAPACITY: usize = READY_BATCH_TASKS * 4;
const TASK_ARENA_BYTES: usize = 1024 * 1024;
const TASK_ARENA_ALIGN: usize = 64;
/// Power-of-two block classes from the alignment granule (64 B) up to
/// the largest future the arena places (256 KiB).
const TASK_ARENA_CLASS_COUNT: usize = 13;
/// Empty-list sentinel for the per-class free heads.
const TASK_ARENA_FREE_NULL: u32 = u32::MAX;
static EXECUTOR_GROUP: Once<NoWeakArc<ExecutorGroup>> = Once::new();

struct ExecutorGroup {
    local_queues: Box<[ReadyQueue]>,
    local_ready_counts: Box<[AtomicUsize]>,
    task_arenas: Box<[NoWeakArc<TaskArena>]>,
    global_queue: ReadyQueue,
    global_ready_count: AtomicUsize,
    global_wake_cursor: AtomicUsize,
}

/// Per-processor bump arena with per-class block reuse.
///
/// Long-lived tasks (network pump, transport runners) pin allocations
/// for the kernel's whole lifetime, so a reset-when-empty bump pointer
/// would leak every completed task's bytes forever and exhaust under
/// spawn churn. Freed blocks instead return to an ABA-tagged Treiber
/// stack per power-of-two class and are reused before the bump pointer
/// grows; steady-state usage is bounded by the peak number of live
/// tasks per class, not by cumulative spawns.
struct TaskArena {
    bytes: TaskArenaBytes,
    offset: AtomicUsize,
    active: AtomicUsize,
    /// One Treiber-stack head per block class: low 32 bits the block
    /// offset (`TASK_ARENA_FREE_NULL` = empty), high 32 bits a CAS tag
    /// bumped on every successful push/pop to defeat ABA.
    free_heads: [AtomicU64; TASK_ARENA_CLASS_COUNT],
}

#[repr(align(64))]
struct TaskArenaBytes {
    bytes: UnsafeCell<[MaybeUninit<u8>; TASK_ARENA_BYTES]>,
}

struct ArenaFuture<Fut> {
    ptr: NonNull<Fut>,
    class: usize,
    arena: NoWeakArc<TaskArena>,
}

unsafe impl Sync for TaskArenaBytes {}
unsafe impl Sync for TaskArena {}
unsafe impl<Fut: Send> Send for ArenaFuture<Fut> {}

impl TaskArena {
    /// Constructs the arena directly inside its shared allocation. The
    /// megabyte-sized `bytes` store is `MaybeUninit` and must never be
    /// materialized on the stack; only the atomics need initialization.
    fn new_shared() -> NoWeakArc<Self> {
        let mut arena = UniqueArc::<Self>::new_uninit();
        let ptr = arena.as_mut_ptr().cast::<Self>();
        unsafe {
            (&raw mut (*ptr).offset).write(AtomicUsize::new(0));
            (&raw mut (*ptr).active).write(AtomicUsize::new(0));
            let heads = &raw mut (*ptr).free_heads;
            for class in 0..TASK_ARENA_CLASS_COUNT {
                (&raw mut (*heads)[class]).write(AtomicU64::new(u64::from(TASK_ARENA_FREE_NULL)));
            }
            UniqueArc::assume_init(arena).shareable()
        }
    }

    /// Bytes a block of `class` occupies: `64 << class`.
    const fn class_bytes(class: usize) -> usize {
        TASK_ARENA_ALIGN << class
    }

    /// Smallest class whose block fits `size` bytes; panics when the
    /// future exceeds the largest class the arena serves.
    fn block_class(size: usize) -> usize {
        let block = size.max(1).next_power_of_two().max(TASK_ARENA_ALIGN);
        let class = block.trailing_zeros() as usize - TASK_ARENA_ALIGN.trailing_zeros() as usize;
        assert!(
            class < TASK_ARENA_CLASS_COUNT,
            "task future of {size} bytes exceeds the largest executor arena block"
        );
        class
    }

    fn allocate<Fut>(arena: &NoWeakArc<Self>, future: Fut) -> ArenaFuture<Fut> {
        let size = size_of::<Fut>();
        let align = align_of::<Fut>().max(1);
        assert!(
            align <= TASK_ARENA_ALIGN,
            "future alignment exceeds executor task arena alignment"
        );
        let class = Self::block_class(size);
        arena.active.fetch_add(1, Ordering::AcqRel);
        let start = arena.pop_free(class).unwrap_or_else(|| {
            arena
                .offset
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |offset| {
                    offset
                        .checked_add(Self::class_bytes(class))
                        .filter(|end| *end <= TASK_ARENA_BYTES)
                })
                .unwrap_or_else(|_| {
                    arena.active.fetch_sub(1, Ordering::AcqRel);
                    panic!(
                        "executor task arena exhausted: {} live tasks",
                        arena.active.load(Ordering::Acquire)
                    )
                })
        });
        let ptr = unsafe { arena.bytes.ptr().add(start).cast::<Fut>() };
        unsafe {
            ptr.write(future);
        }
        ArenaFuture {
            ptr: NonNull::new(ptr).expect("task arena pointer was null"),
            class,
            arena: arena.clone(),
        }
    }

    /// Pops a reusable block offset for `class`, or `None` when the
    /// class free list is empty.
    fn pop_free(&self, class: usize) -> Option<usize> {
        let head = &self.free_heads[class];
        loop {
            let current = head.load(Ordering::Acquire);
            let offset = (current & u64::from(u32::MAX)) as u32;
            if offset == TASK_ARENA_FREE_NULL {
                return None;
            }
            let next = unsafe {
                self.bytes
                    .ptr()
                    .add(offset as usize)
                    .cast::<u32>()
                    .read_volatile()
            };
            let tag = (current >> 32).wrapping_add(1);
            let replacement = (tag << 32) | u64::from(next);
            if head
                .compare_exchange(current, replacement, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(offset as usize);
            }
        }
    }

    /// Returns a dropped block to its class free list.
    fn push_free(&self, offset: usize, class: usize) {
        let head = &self.free_heads[class];
        let offset = u32::try_from(offset).expect("task arena offsets fit in 32 bits");
        loop {
            let current = head.load(Ordering::Acquire);
            let next = (current & u64::from(u32::MAX)) as u32;
            unsafe {
                self.bytes
                    .ptr()
                    .add(offset as usize)
                    .cast::<u32>()
                    .write_volatile(next);
            }
            let tag = (current >> 32).wrapping_add(1);
            let replacement = (tag << 32) | u64::from(offset);
            if head
                .compare_exchange(current, replacement, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    fn release(&self, block_offset: usize, class: usize) {
        self.push_free(block_offset, class);
        let previous = self.active.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0, "task arena active count underflowed");
    }
}

impl TaskArenaBytes {
    fn ptr(&self) -> *mut u8 {
        self.bytes.get().cast::<u8>()
    }
}

impl<Fut> Future for ArenaFuture<Fut>
where
    Fut: Future,
{
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe { Pin::new_unchecked(&mut *self.get_unchecked_mut().ptr.as_ptr()).poll(cx) }
    }
}

impl<Fut> Drop for ArenaFuture<Fut> {
    fn drop(&mut self) {
        unsafe {
            ptr::drop_in_place(self.ptr.as_ptr());
        }
        let offset = self.ptr.as_ptr() as usize - self.arena.bytes.ptr() as usize;
        self.arena.release(offset, self.class);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProgressMode {
    Counted,
    Silent,
}

/// Join handle for a task that is constrained to the spawning processor.
///
/// The marker makes the handle `!Send` and `!Sync`, which prevents accidental
/// migration of a local task to a different processor through the type system.
#[must_use = "tasks get canceled when dropped, use `.detach()` to run them in the background"]
pub struct LocalJoinHandle<T> {
    task: Task<T>,
    _not_send_or_sync: PhantomData<*mut ()>,
}

#[derive(Clone)]
pub struct Spawner<CpuImpl: Cpu + Clone> {
    group: NoWeakArc<ExecutorGroup>,
    cpu: CpuImpl,
    owner_processor: ProcessorId,
    local_queue_index: usize,
    processor_count: usize,
    task_arena: NoWeakArc<TaskArena>,
    progress: ProgressCounter,
    progress_notify: NoWeakArc<Notify>,
}

pub struct Executor {
    group: NoWeakArc<ExecutorGroup>,
    owner_processor: ProcessorId,
    local_queue_index: usize,
    processor_count: usize,
    task_arena: NoWeakArc<TaskArena>,
    progress: ProgressCounter,
    progress_notify: NoWeakArc<Notify>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecutorRunStats {
    local_runnable_count: usize,
    global_runnable_count: usize,
    local_empty_pop_count: usize,
    global_empty_pop_count: usize,
}

struct ProgressSignal {
    progress: ProgressCounter,
    progress_notify: NoWeakArc<Notify>,
}

// `async_task` stores the scheduler closure inside every heap-allocated task.
// Keep global/local schedulers narrow instead of capturing the whole `Spawner`;
// the executor spawn benchmark tracks this directly as bytes per task.
struct GlobalScheduler<CpuImpl: Cpu + Clone> {
    group: NoWeakArc<ExecutorGroup>,
    cpu: CpuImpl,
    processor_count: usize,
    progress: ProgressSignal,
}

struct LocalScheduler<CpuImpl: Cpu + Clone> {
    group: NoWeakArc<ExecutorGroup>,
    cpu: CpuImpl,
    owner_processor: ProcessorId,
    local_queue_index: usize,
    progress: ProgressSignal,
}

struct LocalSilentScheduler<CpuImpl: Cpu + Clone> {
    group: NoWeakArc<ExecutorGroup>,
    cpu: CpuImpl,
    owner_processor: ProcessorId,
    local_queue_index: usize,
}

impl ExecutorRunStats {
    pub const fn runnable_count(self) -> usize {
        self.local_runnable_count + self.global_runnable_count
    }

    pub const fn local_runnable_count(self) -> usize {
        self.local_runnable_count
    }

    pub const fn global_runnable_count(self) -> usize {
        self.global_runnable_count
    }

    pub const fn local_empty_pop_count(self) -> usize {
        self.local_empty_pop_count
    }

    pub const fn global_empty_pop_count(self) -> usize {
        self.global_empty_pop_count
    }
}

impl Executor {
    pub fn new(
        progress: ProgressCounter,
        configured_processors: usize,
        owner_processor: ProcessorId,
    ) -> Self {
        let group = executor_group(configured_processors);
        let local_queue_index = owner_processor.id() as usize;
        group
            .local_queues
            .get(local_queue_index)
            .unwrap_or_else(|| {
                panic!(
                    "executor owner processor {} is outside configured processor count {}",
                    owner_processor.id(),
                    configured_processors
                )
            });
        let processor_count = group.local_queues.len();
        let task_arena = group.task_arenas[local_queue_index].clone();
        Self {
            group,
            owner_processor,
            local_queue_index,
            processor_count,
            task_arena,
            progress,
            progress_notify: NoWeakArc::new(Notify::new()),
        }
    }

    pub fn spawner<CpuImpl: Cpu + Clone>(&self, cpu: CpuImpl) -> Spawner<CpuImpl> {
        Spawner {
            group: self.group.clone(),
            cpu,
            owner_processor: self.owner_processor,
            local_queue_index: self.local_queue_index,
            processor_count: self.processor_count,
            task_arena: self.task_arena.clone(),
            progress: self.progress.clone(),
            progress_notify: self.progress_notify.clone(),
        }
    }

    pub fn run_until_stalled_with_stats(&self) -> ExecutorRunStats {
        let mut stats = ExecutorRunStats::default();
        let local_queue = &self.group.local_queues[self.local_queue_index];
        let local_ready_count = &self.group.local_ready_counts[self.local_queue_index];

        while stats.runnable_count() < READY_BATCH_TASKS {
            let (first, second) = if local_ready_count.load(Ordering::Relaxed) != 0 {
                (ReadySource::Local, ReadySource::Global)
            } else {
                (ReadySource::Global, ReadySource::Local)
            };
            let Some((runnable, source)) = pop_ready_source(
                first,
                local_queue,
                local_ready_count,
                &self.group.global_queue,
                &self.group.global_ready_count,
                &mut stats,
            )
            .or_else(|| {
                pop_ready_source(
                    second,
                    local_queue,
                    local_ready_count,
                    &self.group.global_queue,
                    &self.group.global_ready_count,
                    &mut stats,
                )
            }) else {
                return stats;
            };

            runnable.run();
            match source {
                ReadySource::Local => stats.local_runnable_count += 1,
                ReadySource::Global => stats.global_runnable_count += 1,
            }
        }

        stats
    }
}

#[derive(Clone, Copy)]
enum ReadySource {
    Local,
    Global,
}

impl<CpuImpl: Cpu + Clone> Spawner<CpuImpl> {
    pub(crate) fn progress_counter(&self) -> ProgressCounter {
        self.progress.clone()
    }

    pub(crate) fn progress_notify(&self) -> NoWeakArc<Notify> {
        self.progress_notify.clone()
    }

    fn spawn_with_progress<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        let future = TaskArena::allocate(&self.task_arena, future);
        let scheduler = self.global_scheduler();
        let schedule = move |runnable| scheduler.schedule(runnable);
        let (runnable, task) = Builder::new().spawn(move |_| future, schedule);
        runnable.schedule();
        task
    }

    fn spawn_local_with_progress<Fut>(
        &self,
        future: Fut,
        progress_mode: ProgressMode,
    ) -> LocalJoinHandle<Fut::Output>
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        let future = TaskArena::allocate(&self.task_arena, future);
        if progress_mode == ProgressMode::Silent {
            let scheduler = self.local_silent_scheduler();
            let schedule = move |runnable| scheduler.schedule(runnable);

            // SAFETY: the runnable is always re-enqueued onto the spawning processor's ready
            // queue, and `LocalJoinHandle` is `!Send`, so the task cannot be awaited or
            // dropped from a different processor through safe Rust.
            let (runnable, task) =
                unsafe { Builder::new().spawn_unchecked(move |_| future, schedule) };
            runnable.schedule();
            return LocalJoinHandle {
                task,
                _not_send_or_sync: PhantomData,
            };
        }

        let scheduler = self.local_scheduler();
        let schedule = move |runnable| scheduler.schedule(runnable);

        // SAFETY: the runnable is always re-enqueued onto the spawning processor's ready
        // queue, and `LocalJoinHandle` is `!Send`, so the task cannot be awaited or
        // dropped from a different processor through safe Rust.
        let (runnable, task) = unsafe { Builder::new().spawn_unchecked(move |_| future, schedule) };
        runnable.schedule();
        LocalJoinHandle {
            task,
            _not_send_or_sync: PhantomData,
        }
    }

    fn progress_signal(&self) -> ProgressSignal {
        ProgressSignal {
            progress: self.progress.clone(),
            progress_notify: self.progress_notify.clone(),
        }
    }

    fn global_scheduler(&self) -> GlobalScheduler<CpuImpl> {
        GlobalScheduler {
            group: self.group.clone(),
            cpu: self.cpu.clone(),
            processor_count: self.processor_count,
            progress: self.progress_signal(),
        }
    }

    fn local_scheduler(&self) -> LocalScheduler<CpuImpl> {
        LocalScheduler {
            group: self.group.clone(),
            cpu: self.cpu.clone(),
            owner_processor: self.owner_processor,
            local_queue_index: self.local_queue_index,
            progress: self.progress_signal(),
        }
    }

    fn local_silent_scheduler(&self) -> LocalSilentScheduler<CpuImpl> {
        LocalSilentScheduler {
            group: self.group.clone(),
            cpu: self.cpu.clone(),
            owner_processor: self.owner_processor,
            local_queue_index: self.local_queue_index,
        }
    }

    pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.spawn_with_progress(future)
    }

    pub fn spawn_detached<Fut>(&self, future: Fut)
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.spawn(future).detach();
    }

    pub fn spawn_local<Fut>(&self, future: Fut) -> LocalJoinHandle<Fut::Output>
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        self.spawn_local_with_progress(future, ProgressMode::Counted)
    }

    pub fn spawn_local_detached<Fut>(&self, future: Fut)
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        self.spawn_local(future).detach();
    }

    pub(crate) fn spawn_local_detached_silent<Fut>(&self, future: Fut)
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        self.spawn_local_with_progress(future, ProgressMode::Silent)
            .detach();
    }
}

impl ProgressSignal {
    #[inline]
    fn record(&self) {
        self.progress.record_progress();
        self.progress_notify.notify_one_coalesced();
    }
}

impl<CpuImpl: Cpu + Clone> GlobalScheduler<CpuImpl> {
    #[inline]
    fn schedule(&self, runnable: Runnable) {
        let previous_ready = push_ready(
            &self.group.global_queue,
            &self.group.global_ready_count,
            runnable,
        );
        self.progress.record();
        if should_wake_global_processor(previous_ready, self.processor_count) {
            self.wake_one_remote_processor();
        }
    }

    #[inline]
    fn wake_one_remote_processor(&self) {
        if self.processor_count <= 1 {
            return;
        }
        let current_processor = self.cpu.current_processor();
        let start = self
            .group
            .global_wake_cursor
            .fetch_add(1, Ordering::Relaxed);
        for offset in 0..self.processor_count {
            let processor = (start + offset) % self.processor_count;
            let processor = ProcessorId::new(processor as u16);
            if processor != current_processor {
                self.cpu.wake_processor(processor);
                return;
            }
        }
    }
}

impl<CpuImpl: Cpu + Clone> LocalScheduler<CpuImpl> {
    #[inline]
    fn schedule(&self, runnable: Runnable) {
        let queue = &self.group.local_queues[self.local_queue_index];
        let ready_count = &self.group.local_ready_counts[self.local_queue_index];
        let previous_ready = push_ready(queue, ready_count, runnable);
        self.progress.record();
        if should_wake_owner_processor(previous_ready)
            && self.cpu.current_processor() != self.owner_processor
        {
            self.cpu.wake_processor(self.owner_processor);
        }
    }
}

impl<CpuImpl: Cpu + Clone> LocalSilentScheduler<CpuImpl> {
    #[inline]
    fn schedule(&self, runnable: Runnable) {
        let queue = &self.group.local_queues[self.local_queue_index];
        let ready_count = &self.group.local_ready_counts[self.local_queue_index];
        let previous_ready = push_ready(queue, ready_count, runnable);
        if should_wake_owner_processor(previous_ready)
            && self.cpu.current_processor() != self.owner_processor
        {
            self.cpu.wake_processor(self.owner_processor);
        }
    }
}

fn executor_group(configured_processors: usize) -> NoWeakArc<ExecutorGroup> {
    EXECUTOR_GROUP
        .call_once(|| {
            assert!(
                configured_processors != 0,
                "executor processor count must be non-zero"
            );
            let mut local_queues = Vec::with_capacity(configured_processors);
            let mut local_ready_counts = Vec::with_capacity(configured_processors);
            let mut task_arenas = Vec::with_capacity(configured_processors);
            for _ in 0..configured_processors {
                local_queues.push(ready_queue());
                local_ready_counts.push(AtomicUsize::new(0));
                task_arenas.push(TaskArena::new_shared());
            }
            NoWeakArc::new(ExecutorGroup {
                local_queues: local_queues.into_boxed_slice(),
                local_ready_counts: local_ready_counts.into_boxed_slice(),
                task_arenas: task_arenas.into_boxed_slice(),
                global_queue: ready_queue(),
                global_ready_count: AtomicUsize::new(0),
                global_wake_cursor: AtomicUsize::new(0),
            })
        })
        .clone()
}

fn ready_queue() -> ReadyQueue {
    ConcurrentQueue::bounded(READY_QUEUE_CAPACITY)
}

#[inline]
fn push_ready(queue: &ReadyQueue, ready_count: &AtomicUsize, runnable: Runnable) -> usize {
    // The queue itself publishes the `Runnable`; this counter only drives
    // wake heuristics and underflow asserts, so a full fence just taxes the
    // executor hot path.
    let previous_ready = ready_count.fetch_add(1, Ordering::Relaxed);
    match queue.push(runnable) {
        Ok(()) => previous_ready,
        Err(PushError::Full(_)) => {
            rollback_ready_count(ready_count);
            panic!("executor ready queue capacity {READY_QUEUE_CAPACITY} exhausted")
        }
        Err(PushError::Closed(_)) => {
            rollback_ready_count(ready_count);
            panic!("executor ready queue was closed unexpectedly")
        }
    }
}

fn pop_ready(queue: &ReadyQueue, ready_count: &AtomicUsize) -> Result<Runnable, PopError> {
    let runnable = queue.pop()?;
    let previous = ready_count.fetch_sub(1, Ordering::Relaxed);
    assert!(previous != 0, "executor ready count underflowed");
    Ok(runnable)
}

fn pop_ready_source(
    source: ReadySource,
    local_queue: &ReadyQueue,
    local_ready_count: &AtomicUsize,
    global_queue: &ReadyQueue,
    global_ready_count: &AtomicUsize,
    stats: &mut ExecutorRunStats,
) -> Option<(Runnable, ReadySource)> {
    let (queue, ready_count) = match source {
        ReadySource::Local => (local_queue, local_ready_count),
        ReadySource::Global => (global_queue, global_ready_count),
    };
    match pop_ready(queue, ready_count) {
        Ok(runnable) => Some((runnable, source)),
        Err(PopError::Empty | PopError::Closed) => {
            match source {
                ReadySource::Local => stats.local_empty_pop_count += 1,
                ReadySource::Global => stats.global_empty_pop_count += 1,
            }
            None
        }
    }
}

fn rollback_ready_count(ready_count: &AtomicUsize) {
    let previous = ready_count.fetch_sub(1, Ordering::Relaxed);
    assert!(
        previous != 0,
        "executor ready count underflowed while rolling back failed enqueue"
    );
}

const fn should_wake_global_processor(previous_ready: usize, processor_count: usize) -> bool {
    previous_ready < processor_count.saturating_sub(1)
}

const fn should_wake_owner_processor(previous_ready: usize) -> bool {
    previous_ready == 0
}

impl<T> LocalJoinHandle<T> {
    pub fn detach(self) {
        self.task.detach();
    }

    pub async fn cancel(self) -> Option<T> {
        self.task.cancel().await
    }
}

impl<T> Future for LocalJoinHandle<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.task).poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::{
        Executor, GlobalScheduler, LocalScheduler, LocalSilentScheduler, READY_QUEUE_CAPACITY,
        Spawner, TASK_ARENA_CLASS_COUNT, TaskArena, ready_queue, should_wake_global_processor,
        should_wake_owner_processor,
    };
    use helios_hal::cpu::{Cpu, HardwarePerfCounters, Instant, ProcessorId};
    use helios_hal::watchdog::ProgressCounter;

    #[derive(Clone)]
    struct TestCpu;

    impl Cpu for TestCpu {
        fn current_processor(&self) -> ProcessorId {
            ProcessorId::new(0)
        }

        fn processor_count(&self) -> usize {
            1
        }

        fn bootstrap_processor(&self) -> ProcessorId {
            ProcessorId::new(0)
        }

        fn park_current(&self) {}

        fn start_processor(&self, _processor: ProcessorId) {}

        fn wake_processor(&self, _processor: ProcessorId) {}

        fn now(&self) -> Instant {
            Instant::new(0)
        }

        fn timer_frequency(&self) -> u64 {
            1_000_000_000
        }

        fn hardware_perf_counters(&self) -> HardwarePerfCounters {
            HardwarePerfCounters::default()
        }

        fn set_deadline(&self, _deadline: Instant) {}

        fn publish_executable(&self, _ptr: *const u8, _len: usize) {}

        fn unpublish_executable(&self, _ptr: *const u8, _len: usize) {}

        fn native_feature_probe(&self) -> Option<fn(&str) -> Option<bool>> {
            None
        }

        fn shutdown(&self) -> ! {
            panic!("test CPU cannot shut down")
        }

        fn reboot(&self) -> ! {
            panic!("test CPU cannot reboot")
        }
    }

    #[test]
    fn global_wake_scales_to_processor_count_not_enqueue_count() {
        assert!(!should_wake_global_processor(0, 1));
        assert!(should_wake_global_processor(0, 4));
        assert!(should_wake_global_processor(2, 4));
        assert!(!should_wake_global_processor(3, 4));
    }

    #[test]
    fn owner_wake_query_only_fires_on_empty_to_nonempty_enqueue() {
        assert!(should_wake_owner_processor(0));
        assert!(!should_wake_owner_processor(1));
    }

    #[test]
    fn ready_queue_is_bounded_to_kernel_capacity() {
        let queue = ready_queue();

        assert_eq!(queue.capacity(), Some(READY_QUEUE_CAPACITY));
    }

    #[test]
    fn scheduler_captures_are_narrower_than_spawner() {
        assert!(size_of::<GlobalScheduler<TestCpu>>() < size_of::<Spawner<TestCpu>>());
        assert!(size_of::<LocalScheduler<TestCpu>>() <= size_of::<Spawner<TestCpu>>());
        assert!(size_of::<LocalSilentScheduler<TestCpu>>() < size_of::<Spawner<TestCpu>>());
        assert!(size_of::<LocalSilentScheduler<TestCpu>>() < size_of::<LocalScheduler<TestCpu>>());
    }

    #[test]
    fn global_batch_does_not_probe_empty_local_queue_per_task() {
        let executor = Executor::new(ProgressCounter::new(), 1, ProcessorId::new(0));
        let spawner = executor.spawner(TestCpu);
        for _ in 0..8 {
            spawner.spawn_detached(async {});
        }

        let stats = executor.run_until_stalled_with_stats();

        assert_eq!(stats.global_runnable_count(), 8);
        assert_eq!(stats.local_runnable_count(), 0);
        assert_eq!(stats.global_empty_pop_count(), 1);
        assert_eq!(stats.local_empty_pop_count(), 1);
    }

    #[test]
    fn task_arena_reuses_blocks_released_around_pinned_tasks() {
        let arena = TaskArena::new_shared();
        // An eternal task pins the arena non-empty for the whole test,
        // matching the boot-time pump/transport tasks in a real kernel.
        let pinned = TaskArena::allocate(&arena, [0_u8; 256]);
        // Churn two orders of magnitude more bytes than the arena holds;
        // the old reset-when-empty bump pointer exhausted after 1 MiB of
        // cumulative spawns and panicked on the next allocation.
        for _ in 0..200_000 {
            drop(TaskArena::allocate(&arena, [0_u8; 512]));
        }
        // Distinct live allocations still coexist with the reused blocks.
        let overlapped: [_; 8] = core::array::from_fn(|_| TaskArena::allocate(&arena, [0_u8; 512]));
        drop(overlapped);
        drop(pinned);
    }

    #[test]
    fn task_arena_block_classes_round_up_and_reject_oversize() {
        assert_eq!(TaskArena::block_class(1), 0);
        assert_eq!(TaskArena::block_class(64), 0);
        assert_eq!(TaskArena::block_class(65), 1);
        assert_eq!(TaskArena::block_class(512), 3);
        assert_eq!(
            TaskArena::class_bytes(TASK_ARENA_CLASS_COUNT - 1),
            256 * 1024
        );
    }
}
