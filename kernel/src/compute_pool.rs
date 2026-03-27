extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BinaryHeap;
use alloc::sync::Arc;
use core::cmp::Ordering;
use core::future::Future;
use core::mem;
use core::pin::Pin;
use core::task::{Context, Poll};

use atomic_waker::AtomicWaker;
use spinning_top::Spinlock;

/// Error returned when the compute pool cannot accept more work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnError {
    MemoryLimitExceeded,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComputePriority(u8);

impl ComputePriority {
    pub const LOW: Self = Self(0);
    pub const NORMAL: Self = Self(128);
    pub const HIGH: Self = Self(u8::MAX);

    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u8 {
        self.0
    }
}

/// Immutable configuration for the kernel-internal compute pool.
///
/// This is crate-private because the kernel, not arbitrary callers, owns the
/// worker topology and memory budget.
#[derive(Clone, Copy)]
pub(crate) struct ComputePoolConfig {
    pub(crate) worker_count: usize,
    pub(crate) worker_stack_size: usize,
    pub(crate) max_memory_bytes: usize,
}

impl ComputePoolConfig {
    pub(crate) const fn reserved_stack_bytes(self) -> usize {
        self.worker_count.saturating_mul(self.worker_stack_size)
    }
}

#[derive(Clone)]
pub struct ComputePool {
    state: Arc<Spinlock<ComputePoolState>>,
}

struct ComputePoolState {
    config: ComputePoolConfig,
    next_sequence: u64,
    queued_bytes: usize,
    queue: BinaryHeap<QueuedJob>,
}

struct QueuedJob {
    priority: ComputePriority,
    sequence: u64,
    job: ComputeJob,
}

struct ComputeJob {
    queued_bytes: usize,
    callback: Box<dyn FnOnce() + Send + 'static>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigError {
    InvalidConfig,
}

pub(crate) struct ComputePoolSnapshot {
    pub(crate) queued_jobs: usize,
    pub(crate) queued_bytes: usize,
    pub(crate) reserved_stack_bytes: usize,
    pub(crate) total_bytes: usize,
    pub(crate) max_memory_bytes: usize,
}

struct Completion<T> {
    state: Arc<CompletionState<T>>,
}

struct CompletionState<T> {
    result: Spinlock<Option<Result<T, SpawnError>>>,
    waker: AtomicWaker,
}

impl ComputePool {
    pub(crate) fn new(config: ComputePoolConfig) -> Result<Self, ConfigError> {
        let reserved_stack_bytes = config.reserved_stack_bytes();
        if config.worker_count == 0 || config.worker_stack_size == 0 {
            return Err(ConfigError::InvalidConfig);
        }
        if reserved_stack_bytes > config.max_memory_bytes {
            return Err(ConfigError::InvalidConfig);
        }

        Ok(Self {
            state: Arc::new(Spinlock::new(ComputePoolState {
                config,
                next_sequence: 0,
                queued_bytes: 0,
                queue: BinaryHeap::new(),
            })),
        })
    }

    /// Runs pure compute work on the internal compute pool and resolves once
    /// the job either completes or is rejected.
    pub async fn spawn<F, T>(&self, priority: ComputePriority, job: F) -> Result<T, SpawnError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_internal(priority, job).await
    }

    pub(crate) fn snapshot(&self) -> ComputePoolSnapshot {
        critical_section::with(|_| {
            let state = self.state.lock();
            let reserved_stack_bytes = state.config.reserved_stack_bytes();
            ComputePoolSnapshot {
                queued_jobs: state.queue.len(),
                queued_bytes: state.queued_bytes,
                reserved_stack_bytes,
                total_bytes: reserved_stack_bytes + state.queued_bytes,
                max_memory_bytes: state.config.max_memory_bytes,
            }
        })
    }

    pub(crate) fn config(&self) -> ComputePoolConfig {
        critical_section::with(|_| self.state.lock().config)
    }

    pub(crate) fn run_next(&self) -> bool {
        let Some(job) = self.dequeue() else {
            return false;
        };

        job.run();
        true
    }

    pub(crate) fn run_until_stalled(&self) -> usize {
        let mut completed = 0;

        while self.run_next() {
            completed += 1;
        }

        completed
    }

    fn spawn_internal<F, T>(&self, priority: ComputePriority, job: F) -> Completion<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let completion = Completion::new();
        let state = completion.state.clone();
        let queued_bytes = mem::size_of::<F>();
        let callback = move || {
            state.complete(Ok(job()));
        };

        if let Err(error) = self.enqueue(priority, queued_bytes, Box::new(callback)) {
            completion.state.complete(Err(error));
        }

        completion
    }

    fn enqueue(
        &self,
        priority: ComputePriority,
        queued_bytes: usize,
        callback: Box<dyn FnOnce() + Send + 'static>,
    ) -> Result<(), SpawnError> {
        critical_section::with(|_| {
            let mut state = self.state.lock();
            let reserved_stack_bytes = state.config.reserved_stack_bytes();
            let next_total = reserved_stack_bytes
                .checked_add(state.queued_bytes)
                .and_then(|n| n.checked_add(queued_bytes))
                .expect("compute pool memory accounting overflow");
            if next_total > state.config.max_memory_bytes {
                return Err(SpawnError::MemoryLimitExceeded);
            }

            let sequence = state.next_sequence;
            state.next_sequence = state
                .next_sequence
                .checked_add(1)
                .expect("compute pool sequence overflow");
            state.queued_bytes += queued_bytes;
            state.queue.push(QueuedJob {
                priority,
                sequence,
                job: ComputeJob {
                    queued_bytes,
                    callback,
                },
            });
            Ok(())
        })
    }

    fn dequeue(&self) -> Option<DequeuedJob> {
        critical_section::with(|_| {
            let mut state = self.state.lock();
            let queued = state.queue.pop()?;
            state.queued_bytes = state
                .queued_bytes
                .checked_sub(queued.job.queued_bytes)
                .expect("compute pool queued bytes underflow");
            Some(DequeuedJob {
                callback: queued.job.callback,
            })
        })
    }
}

pub(crate) struct DequeuedJob {
    callback: Box<dyn FnOnce() + Send + 'static>,
}

impl DequeuedJob {
    pub(crate) fn run(self) {
        (self.callback)();
    }
}

impl<T> Completion<T> {
    fn new() -> Self {
        Self {
            state: Arc::new(CompletionState {
                result: Spinlock::new(None),
                waker: AtomicWaker::new(),
            }),
        }
    }
}

impl<T> Future for Completion<T> {
    type Output = Result<T, SpawnError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.state.result.lock().take() {
            return Poll::Ready(result);
        }

        self.state.waker.register(cx.waker());

        if let Some(result) = self.state.result.lock().take() {
            return Poll::Ready(result);
        }

        Poll::Pending
    }
}

impl<T> CompletionState<T> {
    fn complete(&self, result: Result<T, SpawnError>) {
        let replaced = self.result.lock().replace(result);
        assert!(replaced.is_none(), "compute completion was resolved twice");
        self.waker.wake();
    }
}

impl PartialEq for QueuedJob {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.sequence == other.sequence
    }
}

impl Eq for QueuedJob {}

impl PartialOrd for QueuedJob {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueuedJob {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use futures_lite::future::block_on;
    use spinning_top::Spinlock;

    use super::{ComputePool, ComputePoolConfig, ComputePriority, SpawnError};

    #[test]
    fn preserves_priority_and_fifo_order() {
        let pool = ComputePool::new(ComputePoolConfig {
            worker_count: 1,
            worker_stack_size: 4096,
            max_memory_bytes: 4096 + 4096,
        })
        .expect("pool config should be valid");
        let seen = Arc::new(Spinlock::new(Vec::new()));

        let first = {
            let seen = seen.clone();
            pool.spawn_internal(ComputePriority::LOW, move || seen.lock().push(1))
        };
        let second = {
            let seen = seen.clone();
            pool.spawn_internal(ComputePriority::HIGH, move || seen.lock().push(2))
        };
        let third = {
            let seen = seen.clone();
            pool.spawn_internal(ComputePriority::HIGH, move || seen.lock().push(3))
        };

        while pool.run_next() {}

        block_on(first).expect("low priority job should complete");
        block_on(second).expect("first high priority job should complete");
        block_on(third).expect("second high priority job should complete");

        assert_eq!(&*seen.lock(), &[2, 3, 1]);
    }

    #[test]
    fn enforces_memory_limit() {
        struct Large([u8; 96]);

        fn large_job(count: Arc<AtomicUsize>) -> impl FnOnce() -> usize + Send + 'static {
            let payload = Large([0; 96]);
            move || {
                let value = usize::from(payload.0[0]) + 1;
                count.fetch_add(value, Ordering::Relaxed);
                value
            }
        }

        let first_job = large_job(Arc::new(AtomicUsize::new(0)));
        let queued_bytes = core::mem::size_of_val(&first_job);
        let pool = ComputePool::new(ComputePoolConfig {
            worker_count: 1,
            worker_stack_size: 128,
            max_memory_bytes: 128 + queued_bytes,
        })
        .expect("pool config should be valid");
        let count = Arc::new(AtomicUsize::new(0));

        let first = pool.spawn_internal(ComputePriority::NORMAL, large_job(count.clone()));
        let second = pool.spawn_internal(ComputePriority::NORMAL, large_job(count));

        pool.run_next();

        assert_eq!(block_on(first), Ok(1));
        assert_eq!(block_on(second), Err(SpawnError::MemoryLimitExceeded));
    }
}
