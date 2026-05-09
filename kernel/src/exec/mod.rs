//! Cooperative async execution and time-related primitives.
//!
//! `executor` and `task` carry the per-CPU run-queue executor and
//! its task-spawning surface. `sync` provides async-aware mutex /
//! rwlock / notify primitives. `timer` exposes the timing wheel and
//! the Sleep future. `time` provides the kernel monotonic clock.
//! `compaction` runs the periodic memory-pressure scan; `observer`
//! collects perf counters and tracing samples.

mod compaction;
mod executor;
mod observer;
mod sync;
mod task;
mod time;
mod timer;

pub use compaction::{
    CompactionBudget, CompactionPolicy, CompactionReport, CompactionTarget, Compactor,
    PressureLevel,
};
pub use executor::{Executor, ExecutorRunStats, JoinHandle, LocalJoinHandle, READY_BATCH_TASKS, Spawner};
pub use observer::{
    DEFAULT_PERF_METRIC_CAPACITY, DEFAULT_PROFILE_STACK_CAPACITY, DEFAULT_TRACE_HISTORY_CAPACITY,
    FoldedProfileSample, PerfMetricFilter, PerfMetricHistory, PerfMetricSample, ProfileFilter,
    ProfileHistory, ProfileScope, StatsSample, TraceEvent, TraceField, TraceFilter, TraceHistory,
    TraceLevel, TraceValue, matches_perf_metric_filter, matches_profile_filter,
    matches_trace_filter, parse_console_text,
};
pub use sync::{
    Mutex, MutexGuard, Notified, Notify, NotifyWaiter, OwnedRawMutexLease, OwnedRawRwLockReadLease,
    OwnedRawRwLockWriteLease, RawMutex, RawMutexLease, RawRwLock, RawRwLockReadLease,
    RawRwLockWriteLease, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
pub use task::{YieldNow, yield_now};
pub use time::{
    KernelClock, duration_to_ticks, elapsed_millis, monotonic_nanos, nanos_to_ticks_ceil_saturating,
};
pub use timer::{Sleep, Timer};
