extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use async_task::{Builder, Runnable, Task};
use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use helios_hal::cpu::{Cpu, ProcessorId};
use helios_hal::watchdog::ProgressCounter;
use spin::Once;

use crate::sync::Notify;

type ReadyQueue = ConcurrentQueue<Runnable>;
pub type JoinHandle<T> = Task<T>;
pub const READY_BATCH_TASKS: usize = 1024;
static EXECUTOR_GROUP: Once<Arc<ExecutorGroup>> = Once::new();

struct ExecutorGroup {
    local_queues: Box<[Arc<ReadyQueue>]>,
    global_queue: Arc<ReadyQueue>,
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
    local_queue: Arc<ReadyQueue>,
    global_queue: Arc<ReadyQueue>,
    cpu: CpuImpl,
    owner_processor: ProcessorId,
    processor_count: usize,
    progress: ProgressCounter,
    progress_notify: Arc<Notify>,
}

pub struct Executor {
    local_queue: Arc<ReadyQueue>,
    global_queue: Arc<ReadyQueue>,
    processor_count: usize,
    progress: ProgressCounter,
    progress_notify: Arc<Notify>,
}

impl Executor {
    pub fn new(
        progress: ProgressCounter,
        configured_processors: usize,
        owner_processor: ProcessorId,
    ) -> Self {
        let group = executor_group(configured_processors);
        let local_queue = group
            .local_queues
            .get(owner_processor.id() as usize)
            .unwrap_or_else(|| {
                panic!(
                    "executor owner processor {} is outside configured processor count {}",
                    owner_processor.id(),
                    configured_processors
                )
            })
            .clone();
        Self {
            local_queue,
            global_queue: group.global_queue.clone(),
            processor_count: group.local_queues.len(),
            progress,
            progress_notify: Arc::new(Notify::new()),
        }
    }

    pub fn spawner<CpuImpl: Cpu + Clone>(&self, cpu: CpuImpl) -> Spawner<CpuImpl> {
        let owner_processor = cpu.current_processor();
        Spawner {
            local_queue: self.local_queue.clone(),
            global_queue: self.global_queue.clone(),
            cpu,
            owner_processor,
            processor_count: self.processor_count,
            progress: self.progress.clone(),
            progress_notify: self.progress_notify.clone(),
        }
    }

    pub fn run_until_stalled(&self) -> usize {
        let mut runnable_count = 0;

        while runnable_count < READY_BATCH_TASKS {
            let runnable = match self.local_queue.pop() {
                Ok(runnable) => runnable,
                Err(PopError::Empty | PopError::Closed) => match self.global_queue.pop() {
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

    pub(crate) fn progress_notify(&self) -> Arc<Notify> {
        self.progress_notify.clone()
    }

    fn schedule_on_queue(
        &self,
        queue: &Arc<ReadyQueue>,
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
            WakeTarget::AllProcessors => {
                let current_processor = self.cpu.current_processor();
                for processor in 0..self.processor_count {
                    let processor = ProcessorId::new(processor as u16);
                    if processor != current_processor {
                        self.cpu.wake_processor(processor);
                    }
                }
            }
            WakeTarget::OwnerProcessor => {
                if self.cpu.current_processor() != self.owner_processor {
                    self.cpu.wake_processor(self.owner_processor);
                }
            }
        }
    }

    fn schedule_local(&self, runnable: Runnable, progress_mode: ProgressMode) {
        self.schedule_on_queue(
            &self.local_queue,
            runnable,
            progress_mode,
            WakeTarget::OwnerProcessor,
        );
    }

    fn schedule_global(&self, runnable: Runnable, progress_mode: ProgressMode) {
        self.schedule_on_queue(
            &self.global_queue,
            runnable,
            progress_mode,
            WakeTarget::AllProcessors,
        );
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
    AllProcessors,
    OwnerProcessor,
}

fn executor_group(configured_processors: usize) -> Arc<ExecutorGroup> {
    EXECUTOR_GROUP
        .call_once(|| {
            assert!(
                configured_processors != 0,
                "executor processor count must be non-zero"
            );
            let mut local_queues = Vec::with_capacity(configured_processors);
            for _ in 0..configured_processors {
                local_queues.push(Arc::new(ConcurrentQueue::unbounded()));
            }
            Arc::new(ExecutorGroup {
                local_queues: local_queues.into_boxed_slice(),
                global_queue: Arc::new(ConcurrentQueue::unbounded()),
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
