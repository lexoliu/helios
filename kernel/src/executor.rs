extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll};

use async_task::{Builder, Runnable, Task};
use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use helios_hal::cpu::{Cpu, ProcessorId};
use helios_hal::watchdog::ProgressCounter;
use spin::Once;
use triomphe::Arc as NoWeakArc;

use crate::sync::Notify;

type ReadyQueue = ConcurrentQueue<Runnable>;
pub type JoinHandle<T> = Task<T>;
pub const READY_BATCH_TASKS: usize = 1024;
static EXECUTOR_GROUP: Once<NoWeakArc<ExecutorGroup>> = Once::new();

struct ExecutorGroup {
    local_queues: Box<[ReadyQueue]>,
    global_queue: ReadyQueue,
    global_wake_cursor: AtomicUsize,
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
    progress: ProgressCounter,
    progress_notify: NoWeakArc<Notify>,
}

pub struct Executor {
    group: NoWeakArc<ExecutorGroup>,
    owner_processor: ProcessorId,
    local_queue_index: usize,
    processor_count: usize,
    progress: ProgressCounter,
    progress_notify: NoWeakArc<Notify>,
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
        Self {
            group,
            owner_processor,
            local_queue_index,
            processor_count,
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
            progress: self.progress.clone(),
            progress_notify: self.progress_notify.clone(),
        }
    }

    pub fn run_until_stalled(&self) -> usize {
        let mut runnable_count = 0;

        while runnable_count < READY_BATCH_TASKS {
            let local_queue = &self.group.local_queues[self.local_queue_index];
            let runnable = match local_queue.pop() {
                Ok(runnable) => runnable,
                Err(PopError::Empty | PopError::Closed) => match self.group.global_queue.pop() {
                    Ok(runnable) => runnable,
                    Err(PopError::Empty | PopError::Closed) => return runnable_count,
                },
            };

            runnable.run();
            runnable_count += 1;
        }

        runnable_count
    }
}

impl<CpuImpl: Cpu + Clone> Spawner<CpuImpl> {
    pub(crate) fn progress_counter(&self) -> ProgressCounter {
        self.progress.clone()
    }

    pub(crate) fn progress_notify(&self) -> NoWeakArc<Notify> {
        self.progress_notify.clone()
    }

    fn schedule_on_queue(
        &self,
        queue: &ReadyQueue,
        runnable: Runnable,
        progress_mode: ProgressMode,
        wake: WakeTarget,
    ) {
        if progress_mode == ProgressMode::Counted {
            self.progress.record_progress();
            self.progress_notify.notify_one();
        }

        match queue.push(runnable) {
            Ok(()) => {}
            Err(PushError::Full(_)) => unreachable!("unbounded ready queue reported full"),
            Err(PushError::Closed(_)) => panic!("executor ready queue was closed unexpectedly"),
        }

        match wake {
            WakeTarget::OneRemoteProcessor => self.wake_one_remote_processor(),
            WakeTarget::OwnerProcessor => {
                if self.cpu.current_processor() != self.owner_processor {
                    self.cpu.wake_processor(self.owner_processor);
                }
            }
        }
    }

    fn schedule_local(&self, runnable: Runnable, progress_mode: ProgressMode) {
        let queue = &self.group.local_queues[self.local_queue_index];
        self.schedule_on_queue(queue, runnable, progress_mode, WakeTarget::OwnerProcessor);
    }

    fn schedule_global(&self, runnable: Runnable, progress_mode: ProgressMode) {
        self.schedule_on_queue(
            &self.group.global_queue,
            runnable,
            progress_mode,
            WakeTarget::OneRemoteProcessor,
        );
    }

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

    fn spawn_with_progress<Fut>(
        &self,
        future: Fut,
        progress_mode: ProgressMode,
    ) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        let spawner = self.clone();
        let schedule = move |runnable| spawner.schedule_global(runnable, progress_mode);
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
        let spawner = self.clone();
        let schedule = move |runnable| spawner.schedule_local(runnable, progress_mode);

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

    pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.spawn_with_progress(future, ProgressMode::Counted)
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

#[derive(Clone, Copy)]
enum WakeTarget {
    OneRemoteProcessor,
    OwnerProcessor,
}

fn executor_group(configured_processors: usize) -> NoWeakArc<ExecutorGroup> {
    EXECUTOR_GROUP
        .call_once(|| {
            assert!(
                configured_processors != 0,
                "executor processor count must be non-zero"
            );
            let mut local_queues = Vec::with_capacity(configured_processors);
            for _ in 0..configured_processors {
                local_queues.push(ConcurrentQueue::unbounded());
            }
            NoWeakArc::new(ExecutorGroup {
                local_queues: local_queues.into_boxed_slice(),
                global_queue: ConcurrentQueue::unbounded(),
                global_wake_cursor: AtomicUsize::new(0),
            })
        })
        .clone()
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
