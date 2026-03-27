extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BinaryHeap;
use alloc::sync::Arc;
use core::cmp::Ordering;
use core::mem;

use spinning_top::Spinlock;

/// Priority of a compute job submitted to the internal kernel compute pool.
///
/// Higher numeric values run first. Jobs of equal priority preserve FIFO order.
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

/// Immutable configuration for a compute pool.
///
/// The total memory limit covers:
/// - all worker stacks reserved by the pool
/// - all queued jobs currently waiting for a worker
///
/// Queued jobs are rejected once admitting them would exceed the configured
/// memory cap.
#[derive(Clone, Copy)]
pub struct ComputePoolConfig {
    pub worker_count: usize,
    pub worker_stack_size: usize,
    pub max_memory_bytes: usize,
}

impl ComputePoolConfig {
    pub const fn reserved_stack_bytes(self) -> usize {
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

/// Summary of current pool pressure.
#[derive(Clone, Copy)]
pub struct ComputePoolSnapshot {
    pub queued_jobs: usize,
    pub queued_bytes: usize,
    pub reserved_stack_bytes: usize,
    pub total_bytes: usize,
    pub max_memory_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    MemoryLimitExceeded,
    InvalidConfig,
}

impl ComputePool {
    pub fn new(config: ComputePoolConfig) -> Result<Self, SubmitError> {
        let reserved_stack_bytes = config.reserved_stack_bytes();
        if config.worker_count == 0 || config.worker_stack_size == 0 {
            return Err(SubmitError::InvalidConfig);
        }
        if reserved_stack_bytes > config.max_memory_bytes {
            return Err(SubmitError::InvalidConfig);
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

    /// Enqueues a pure compute job for execution by the pool.
    ///
    /// This API is intentionally restricted to owned `'static` work because the
    /// pool is meant for isolated kernel-internal computation without borrowing
    /// ambient async state or waiting on external resources.
    pub fn submit<F>(&self, priority: ComputePriority, job: F) -> Result<(), SubmitError>
    where
        F: FnOnce() + Send + 'static,
    {
        let queued_bytes = mem::size_of::<F>();
        critical_section::with(|_| {
            let mut state = self.state.lock();
            let reserved_stack_bytes = state.config.reserved_stack_bytes();
            let next_total = reserved_stack_bytes
                .checked_add(state.queued_bytes)
                .and_then(|n| n.checked_add(queued_bytes))
                .expect("compute pool memory accounting overflow");
            if next_total > state.config.max_memory_bytes {
                return Err(SubmitError::MemoryLimitExceeded);
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
                    callback: Box::new(job),
                },
            });
            Ok(())
        })
    }

    pub fn snapshot(&self) -> ComputePoolSnapshot {
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

    pub fn config(&self) -> ComputePoolConfig {
        critical_section::with(|_| self.state.lock().config)
    }

    /// Runs exactly one queued compute job, if any.
    ///
    /// This is the narrow execution boundary for worker implementations:
    /// queueing, prioritization, and memory accounting stay inside
    /// `ComputePool`, while the concrete worker runtime decides when to call
    /// `run_next`.
    pub fn run_next(&self) -> bool {
        let Some(job) = self.dequeue() else {
            return false;
        };

        job.run();
        true
    }

    /// Runs queued jobs until the pool becomes empty and returns the number of
    /// completed jobs.
    pub fn run_until_stalled(&self) -> usize {
        let mut completed = 0;

        while self.run_next() {
            completed += 1;
        }

        completed
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

    use spinning_top::Spinlock;

    use super::{ComputePool, ComputePoolConfig, ComputePriority, SubmitError};

    #[test]
    fn preserves_priority_and_fifo_order() {
        let pool = ComputePool::new(ComputePoolConfig {
            worker_count: 1,
            worker_stack_size: 4096,
            max_memory_bytes: 4096 + 4096,
        })
        .expect("pool config should be valid");
        let seen = Arc::new(Spinlock::new(Vec::new()));

        {
            let seen = seen.clone();
            pool.submit(ComputePriority::LOW, move || seen.lock().push(1))
                .expect("low priority job should fit");
        }
        {
            let seen = seen.clone();
            pool.submit(ComputePriority::HIGH, move || seen.lock().push(2))
                .expect("high priority job should fit");
        }
        {
            let seen = seen.clone();
            pool.submit(ComputePriority::HIGH, move || seen.lock().push(3))
                .expect("second high priority job should fit");
        }

        while pool.run_next() {}

        assert_eq!(&*seen.lock(), &[2, 3, 1]);
    }

    #[test]
    fn enforces_memory_limit() {
        struct Large([u8; 96]);

        fn large_job(count: Arc<AtomicUsize>) -> impl FnOnce() + Send + 'static {
            let payload = Large([0; 96]);
            move || {
                count.fetch_add(usize::from(payload.0[0]) + 1, Ordering::Relaxed);
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

        pool.submit(ComputePriority::NORMAL, large_job(count.clone()))
            .expect("first job should fit");

        let second_job = large_job(count);
        assert_eq!(core::mem::size_of_val(&second_job), queued_bytes);
        let result = pool.submit(ComputePriority::NORMAL, second_job);

        assert_eq!(result, Err(SubmitError::MemoryLimitExceeded));
    }
}
