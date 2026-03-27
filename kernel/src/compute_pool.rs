extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::future::Future;
use core::mem;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering as AtomicOrdering};
use core::task::{Context, Poll};

use atomic_waker::AtomicWaker;
use concurrent_queue::{ConcurrentQueue, PopError, PushError};

const PRIORITY_LEVELS: usize = 256;
const COMPLETION_PENDING: u8 = 0;
const COMPLETION_READY: u8 = 1;
const COMPLETION_CONSUMED: u8 = 2;

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

    const fn lane(self) -> usize {
        self.0 as usize
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
    config: ComputePoolConfig,
    queued_bytes: Arc<AtomicUsize>,
    queues: Arc<[ConcurrentQueue<ComputeJob>; PRIORITY_LEVELS]>,
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
    result: UnsafeCell<MaybeUninit<Result<T, SpawnError>>>,
    status: AtomicU8,
    waker: AtomicWaker,
}

unsafe impl<T: Send> Send for CompletionState<T> {}
unsafe impl<T: Send> Sync for CompletionState<T> {}

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
            config,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            queues: Arc::new(core::array::from_fn(|_| ConcurrentQueue::unbounded())),
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
        let queued_jobs = self.queues.iter().map(ConcurrentQueue::len).sum();
        let queued_bytes = self.queued_bytes.load(AtomicOrdering::Acquire);
        let reserved_stack_bytes = self.config.reserved_stack_bytes();

        ComputePoolSnapshot {
            queued_jobs,
            queued_bytes,
            reserved_stack_bytes,
            total_bytes: reserved_stack_bytes + queued_bytes,
            max_memory_bytes: self.config.max_memory_bytes,
        }
    }

    pub(crate) fn config(&self) -> ComputePoolConfig {
        self.config
    }

    pub(crate) fn run_next(&self) -> bool {
        let Some(job) = self.dequeue() else {
            return false;
        };

        (job.callback)();
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
        self.reserve_bytes(queued_bytes)?;
        let job = ComputeJob {
            queued_bytes,
            callback,
        };

        match self.queues[priority.lane()].push(job) {
            Ok(()) => Ok(()),
            Err(PushError::Full(_)) => unreachable!("unbounded compute queue reported full"),
            Err(PushError::Closed(job)) => {
                self.release_bytes(job.queued_bytes);
                panic!(
                    "compute queue for priority {} was closed unexpectedly",
                    priority.value()
                );
            }
        }
    }

    fn dequeue(&self) -> Option<ComputeJob> {
        for queue in self.queues.iter().rev() {
            match queue.pop() {
                Ok(job) => {
                    self.release_bytes(job.queued_bytes);
                    return Some(job);
                }
                Err(PopError::Empty | PopError::Closed) => continue,
            }
        }

        None
    }

    fn reserve_bytes(&self, queued_bytes: usize) -> Result<(), SpawnError> {
        let reserved_stack_bytes = self.config.reserved_stack_bytes();

        loop {
            let current = self.queued_bytes.load(AtomicOrdering::Acquire);
            let next_total = reserved_stack_bytes
                .checked_add(current)
                .and_then(|n| n.checked_add(queued_bytes))
                .expect("compute pool memory accounting overflow");
            if next_total > self.config.max_memory_bytes {
                return Err(SpawnError::MemoryLimitExceeded);
            }

            match self.queued_bytes.compare_exchange_weak(
                current,
                current + queued_bytes,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(_) => continue,
            }
        }
    }

    fn release_bytes(&self, queued_bytes: usize) {
        let previous = self
            .queued_bytes
            .fetch_sub(queued_bytes, AtomicOrdering::AcqRel);
        assert!(
            previous >= queued_bytes,
            "compute pool queued bytes underflow"
        );
    }
}

impl<T> Completion<T> {
    fn new() -> Self {
        Self {
            state: Arc::new(CompletionState {
                result: UnsafeCell::new(MaybeUninit::uninit()),
                status: AtomicU8::new(COMPLETION_PENDING),
                waker: AtomicWaker::new(),
            }),
        }
    }
}

impl<T> Future for Completion<T> {
    type Output = Result<T, SpawnError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.state.try_take() {
            return Poll::Ready(result);
        }

        self.state.waker.register(cx.waker());

        if let Some(result) = self.state.try_take() {
            return Poll::Ready(result);
        }

        Poll::Pending
    }
}

impl<T> CompletionState<T> {
    fn complete(&self, result: Result<T, SpawnError>) {
        let previous = self.status.load(AtomicOrdering::Acquire);
        assert_eq!(
            previous, COMPLETION_PENDING,
            "compute completion was resolved twice"
        );

        unsafe {
            (*self.result.get()).write(result);
        }
        self.status.store(COMPLETION_READY, AtomicOrdering::Release);
        self.waker.wake();
    }

    fn try_take(&self) -> Option<Result<T, SpawnError>> {
        if self
            .status
            .compare_exchange(
                COMPLETION_READY,
                COMPLETION_CONSUMED,
                AtomicOrdering::AcqRel,
                AtomicOrdering::Acquire,
            )
            .is_err()
        {
            return None;
        }

        Some(unsafe { (*self.result.get()).assume_init_read() })
    }
}

impl<T> Drop for CompletionState<T> {
    fn drop(&mut self) {
        if *self.status.get_mut() == COMPLETION_READY {
            unsafe {
                self.result.get_mut().assume_init_drop();
            }
        }
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
