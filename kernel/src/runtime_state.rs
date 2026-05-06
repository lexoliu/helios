extern crate alloc;

use alloc::format;
use core::panic::Location;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use triomphe::Arc;

use crate::{
    DEFAULT_PERF_METRIC_CAPACITY, DEFAULT_PROFILE_STACK_CAPACITY, DEFAULT_TRACE_HISTORY_CAPACITY,
    EmbeddedBootFs, FoldedProfileSample, FutexKey, FutexTable, FutexWaitRegistration,
    HEAP_SIZE_CLASS_COUNT, HeapStats, InstanceRegistry, Notify, PerfMetricFilter,
    PerfMetricHistory, PerfMetricSample, ProfileFilter, ProfileHistory, ProfileScope, StatsSample,
    TraceEvent, TraceFilter, TraceHistory, embedded_init,
};
use helios_hal::cpu::HardwarePerfCounterDelta;
use spin::Mutex;

use crate::component_runtime::ComponentRuntimeState;
use crate::runtime_types::ComponentHostFilesystemState;

#[derive(Clone)]
pub struct RuntimeState<ProgramService, NetworkService, HostFsService> {
    inner: Arc<RuntimeStateInner<ProgramService, NetworkService, HostFsService>>,
}

struct RuntimeStateInner<ProgramService, NetworkService, HostFsService> {
    boot_ticks: u64,
    timebase_frequency: u64,
    processor_count: u32,
    instance_registry: InstanceRegistry,
    program_service: Mutex<Option<ProgramService>>,
    program_service_ready: Notify,
    network_service: Mutex<Option<NetworkService>>,
    host_fs_service: Mutex<Option<HostFsService>>,
    futex_table: Mutex<FutexTable>,
    bootfs: Mutex<Option<EmbeddedBootFs>>,
    tracing: Mutex<TraceHistory>,
    profiling_enabled: AtomicBool,
    profiling: Mutex<ProfileHistory>,
    perf_metrics: Mutex<PerfMetricHistory>,
    heap_perf_snapshot: HeapPerfSnapshot,
}

struct HeapPerfSnapshot {
    allocation_count: AtomicU64,
    deallocation_count: AtomicU64,
    reallocation_count: AtomicU64,
    total_allocation_bytes: AtomicU64,
    total_deallocation_bytes: AtomicU64,
    total_reallocation_bytes: AtomicU64,
    size_class_allocation_count: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_deallocation_count: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_reallocation_count: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_allocation_bytes: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_deallocation_bytes: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_reallocation_bytes: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
}

impl HeapPerfSnapshot {
    const fn new() -> Self {
        Self {
            allocation_count: AtomicU64::new(0),
            deallocation_count: AtomicU64::new(0),
            reallocation_count: AtomicU64::new(0),
            total_allocation_bytes: AtomicU64::new(0),
            total_deallocation_bytes: AtomicU64::new(0),
            total_reallocation_bytes: AtomicU64::new(0),
            size_class_allocation_count: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_deallocation_count: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_reallocation_count: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_allocation_bytes: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_deallocation_bytes: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_reallocation_bytes: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
        }
    }

    fn reset(&self, stats: HeapStats) {
        self.allocation_count
            .store(stats.allocation_count, Ordering::Release);
        self.deallocation_count
            .store(stats.deallocation_count, Ordering::Release);
        self.reallocation_count
            .store(stats.reallocation_count, Ordering::Release);
        self.total_allocation_bytes
            .store(stats.total_allocation_bytes, Ordering::Release);
        self.total_deallocation_bytes
            .store(stats.total_deallocation_bytes, Ordering::Release);
        self.total_reallocation_bytes
            .store(stats.total_reallocation_bytes, Ordering::Release);
        reset_heap_size_class_snapshot(
            &self.size_class_allocation_count,
            stats.size_class_allocation_count,
        );
        reset_heap_size_class_snapshot(
            &self.size_class_deallocation_count,
            stats.size_class_deallocation_count,
        );
        reset_heap_size_class_snapshot(
            &self.size_class_reallocation_count,
            stats.size_class_reallocation_count,
        );
        reset_heap_size_class_snapshot(
            &self.size_class_allocation_bytes,
            stats.size_class_allocation_bytes,
        );
        reset_heap_size_class_snapshot(
            &self.size_class_deallocation_bytes,
            stats.size_class_deallocation_bytes,
        );
        reset_heap_size_class_snapshot(
            &self.size_class_reallocation_bytes,
            stats.size_class_reallocation_bytes,
        );
    }

    fn allocation_count_delta(&self, stats: HeapStats) -> u64 {
        swap_delta(&self.allocation_count, stats.allocation_count)
    }

    fn deallocation_count_delta(&self, stats: HeapStats) -> u64 {
        swap_delta(&self.deallocation_count, stats.deallocation_count)
    }

    fn reallocation_count_delta(&self, stats: HeapStats) -> u64 {
        swap_delta(&self.reallocation_count, stats.reallocation_count)
    }

    fn allocation_bytes_delta(&self, stats: HeapStats) -> u64 {
        swap_delta(&self.total_allocation_bytes, stats.total_allocation_bytes)
    }

    fn deallocation_bytes_delta(&self, stats: HeapStats) -> u64 {
        swap_delta(
            &self.total_deallocation_bytes,
            stats.total_deallocation_bytes,
        )
    }

    fn reallocation_bytes_delta(&self, stats: HeapStats) -> u64 {
        swap_delta(
            &self.total_reallocation_bytes,
            stats.total_reallocation_bytes,
        )
    }

    fn allocation_size_class_count_delta(&self, stats: HeapStats) -> [u64; HEAP_SIZE_CLASS_COUNT] {
        heap_size_class_delta(
            &self.size_class_allocation_count,
            stats.size_class_allocation_count,
        )
    }

    fn deallocation_size_class_count_delta(
        &self,
        stats: HeapStats,
    ) -> [u64; HEAP_SIZE_CLASS_COUNT] {
        heap_size_class_delta(
            &self.size_class_deallocation_count,
            stats.size_class_deallocation_count,
        )
    }

    fn reallocation_size_class_count_delta(
        &self,
        stats: HeapStats,
    ) -> [u64; HEAP_SIZE_CLASS_COUNT] {
        heap_size_class_delta(
            &self.size_class_reallocation_count,
            stats.size_class_reallocation_count,
        )
    }

    fn allocation_size_class_bytes_delta(&self, stats: HeapStats) -> [u64; HEAP_SIZE_CLASS_COUNT] {
        heap_size_class_delta(
            &self.size_class_allocation_bytes,
            stats.size_class_allocation_bytes,
        )
    }

    fn deallocation_size_class_bytes_delta(
        &self,
        stats: HeapStats,
    ) -> [u64; HEAP_SIZE_CLASS_COUNT] {
        heap_size_class_delta(
            &self.size_class_deallocation_bytes,
            stats.size_class_deallocation_bytes,
        )
    }

    fn reallocation_size_class_bytes_delta(
        &self,
        stats: HeapStats,
    ) -> [u64; HEAP_SIZE_CLASS_COUNT] {
        heap_size_class_delta(
            &self.size_class_reallocation_bytes,
            stats.size_class_reallocation_bytes,
        )
    }
}

fn swap_delta(value: &AtomicU64, current: u64) -> u64 {
    let previous = value.swap(current, Ordering::AcqRel);
    current.saturating_sub(previous)
}

fn reset_heap_size_class_snapshot(
    snapshot: &[AtomicU64; HEAP_SIZE_CLASS_COUNT],
    current: [u64; HEAP_SIZE_CLASS_COUNT],
) {
    for (slot, value) in snapshot.iter().zip(current) {
        slot.store(value, Ordering::Release);
    }
}

fn heap_size_class_delta(
    snapshot: &[AtomicU64; HEAP_SIZE_CLASS_COUNT],
    current: [u64; HEAP_SIZE_CLASS_COUNT],
) -> [u64; HEAP_SIZE_CLASS_COUNT] {
    core::array::from_fn(|index| swap_delta(&snapshot[index], current[index]))
}

fn record_heap_delta_metric(
    metrics: &Mutex<PerfMetricHistory>,
    name: &'static str,
    events: u64,
    bytes: u64,
) {
    if events == 0 && bytes == 0 {
        return;
    }
    metrics.lock().record_parts_events(
        ProfileScope::Kernel,
        "kernel;heap;",
        name,
        events,
        0,
        HardwarePerfCounterDelta::default(),
        bytes,
    );
}

#[derive(Clone, Copy)]
enum HeapMetricKind {
    Alloc,
    Dealloc,
    Realloc,
}

fn record_heap_size_class_delta_metrics(
    metrics: &Mutex<PerfMetricHistory>,
    kind: HeapMetricKind,
    events: [u64; HEAP_SIZE_CLASS_COUNT],
    bytes: [u64; HEAP_SIZE_CLASS_COUNT],
) {
    for index in 0..HEAP_SIZE_CLASS_COUNT {
        record_heap_delta_metric(
            metrics,
            heap_size_class_metric_name(kind, index),
            events[index],
            bytes[index],
        );
    }
}

fn heap_size_class_metric_name(kind: HeapMetricKind, index: usize) -> &'static str {
    match (kind, index) {
        (HeapMetricKind::Alloc, 0) => "alloc-size-0008",
        (HeapMetricKind::Alloc, 1) => "alloc-size-0016",
        (HeapMetricKind::Alloc, 2) => "alloc-size-0032",
        (HeapMetricKind::Alloc, 3) => "alloc-size-0064",
        (HeapMetricKind::Alloc, 4) => "alloc-size-0128",
        (HeapMetricKind::Alloc, 5) => "alloc-size-0256",
        (HeapMetricKind::Alloc, 6) => "alloc-size-0512",
        (HeapMetricKind::Alloc, 7) => "alloc-size-1024",
        (HeapMetricKind::Alloc, 8) => "alloc-size-4096",
        (HeapMetricKind::Alloc, 9) => "alloc-size-16k",
        (HeapMetricKind::Alloc, 10) => "alloc-size-64k",
        (HeapMetricKind::Alloc, 11) => "alloc-size-large",
        (HeapMetricKind::Dealloc, 0) => "dealloc-size-0008",
        (HeapMetricKind::Dealloc, 1) => "dealloc-size-0016",
        (HeapMetricKind::Dealloc, 2) => "dealloc-size-0032",
        (HeapMetricKind::Dealloc, 3) => "dealloc-size-0064",
        (HeapMetricKind::Dealloc, 4) => "dealloc-size-0128",
        (HeapMetricKind::Dealloc, 5) => "dealloc-size-0256",
        (HeapMetricKind::Dealloc, 6) => "dealloc-size-0512",
        (HeapMetricKind::Dealloc, 7) => "dealloc-size-1024",
        (HeapMetricKind::Dealloc, 8) => "dealloc-size-4096",
        (HeapMetricKind::Dealloc, 9) => "dealloc-size-16k",
        (HeapMetricKind::Dealloc, 10) => "dealloc-size-64k",
        (HeapMetricKind::Dealloc, 11) => "dealloc-size-large",
        (HeapMetricKind::Realloc, 0) => "realloc-size-0008",
        (HeapMetricKind::Realloc, 1) => "realloc-size-0016",
        (HeapMetricKind::Realloc, 2) => "realloc-size-0032",
        (HeapMetricKind::Realloc, 3) => "realloc-size-0064",
        (HeapMetricKind::Realloc, 4) => "realloc-size-0128",
        (HeapMetricKind::Realloc, 5) => "realloc-size-0256",
        (HeapMetricKind::Realloc, 6) => "realloc-size-0512",
        (HeapMetricKind::Realloc, 7) => "realloc-size-1024",
        (HeapMetricKind::Realloc, 8) => "realloc-size-4096",
        (HeapMetricKind::Realloc, 9) => "realloc-size-16k",
        (HeapMetricKind::Realloc, 10) => "realloc-size-64k",
        (HeapMetricKind::Realloc, 11) => "realloc-size-large",
        _ => panic!("unknown kernel heap size class metric"),
    }
}

impl<ProgramService, NetworkService, HostFsService>
    RuntimeState<ProgramService, NetworkService, HostFsService>
where
    ProgramService: Clone,
    NetworkService: Clone,
    HostFsService: Clone,
{
    pub fn new(timebase_frequency: u64, processor_count: usize, boot_ticks: u64) -> Self {
        Self {
            inner: Arc::new(RuntimeStateInner {
                boot_ticks,
                timebase_frequency,
                processor_count: processor_count as u32,
                instance_registry: InstanceRegistry::new(),
                program_service: Mutex::new(None),
                program_service_ready: Notify::new(),
                network_service: Mutex::new(None),
                host_fs_service: Mutex::new(None),
                futex_table: Mutex::new(FutexTable::new()),
                bootfs: Mutex::new(embedded_init().map(|init| init.bootfs())),
                tracing: Mutex::new(TraceHistory::new(DEFAULT_TRACE_HISTORY_CAPACITY)),
                profiling_enabled: AtomicBool::new(false),
                profiling: Mutex::new(ProfileHistory::new(DEFAULT_PROFILE_STACK_CAPACITY)),
                perf_metrics: Mutex::new(PerfMetricHistory::new(DEFAULT_PERF_METRIC_CAPACITY)),
                heap_perf_snapshot: HeapPerfSnapshot::new(),
            }),
        }
    }

    pub fn snapshot(&self, current_ticks: u64) -> StatsSample {
        let uptime = self.ticks_to_nanos(current_ticks.saturating_sub(self.inner.boot_ticks));
        StatsSample {
            timestamp: uptime,
            uptime,
            configured_processors: self.inner.processor_count,
            online_processors: self.inner.processor_count,
        }
    }

    pub fn record_console_text(&self, current_ticks: u64, text: &str) {
        let timestamp = self.ticks_to_nanos(current_ticks.saturating_sub(self.inner.boot_ticks));
        self.inner
            .tracing
            .lock()
            .record_console_text(timestamp, text);
    }

    pub fn recent(&self, filter: &TraceFilter, limit: u32) -> alloc::vec::Vec<TraceEvent> {
        self.inner.tracing.lock().recent(filter, limit)
    }

    pub fn next_after(&self, cursor: u64, filter: &TraceFilter) -> Option<(u64, TraceEvent)> {
        self.inner.tracing.lock().next_after(cursor, filter)
    }

    pub fn set_profiling_enabled(&self, enabled: bool) {
        crate::set_kernel_heap_size_class_metrics_enabled(enabled);
        if enabled {
            self.inner.heap_perf_snapshot.reset(crate::heap_stats());
        }
        self.inner
            .profiling_enabled
            .store(enabled, Ordering::Release);
    }

    pub fn profiling_enabled(&self) -> bool {
        self.inner.profiling_enabled.load(Ordering::Acquire)
    }

    pub fn clear_profile(&self) {
        self.inner.profiling.lock().clear();
        self.inner.perf_metrics.lock().clear();
        crate::set_kernel_heap_size_class_metrics_enabled(
            self.inner.profiling_enabled.load(Ordering::Acquire),
        );
        self.inner.heap_perf_snapshot.reset(crate::heap_stats());
    }

    pub fn record_profile_stack(
        &self,
        scope: ProfileScope,
        stack: alloc::string::String,
        weight_ticks: u64,
    ) {
        if !self.inner.profiling_enabled.load(Ordering::Acquire) {
            return;
        }
        let weight = self.ticks_to_nanos(weight_ticks);
        self.inner.profiling.lock().record(scope, stack, weight);
    }

    pub fn record_profile_stack_str(&self, scope: ProfileScope, stack: &str, weight_ticks: u64) {
        if !self.inner.profiling_enabled.load(Ordering::Acquire) {
            return;
        }
        let weight = self.ticks_to_nanos(weight_ticks);
        self.inner.profiling.lock().record_str(scope, stack, weight);
    }

    pub fn record_profile_stack_parts(
        &self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        weight_ticks: u64,
    ) {
        if !self.inner.profiling_enabled.load(Ordering::Acquire) {
            return;
        }
        let weight = self.ticks_to_nanos(weight_ticks);
        self.inner
            .profiling
            .lock()
            .record_parts(scope, prefix, suffix, weight);
    }

    pub fn record_profile_stack_nanos(
        &self,
        scope: ProfileScope,
        stack: alloc::string::String,
        weight_nanos: u64,
    ) {
        if !self.inner.profiling_enabled.load(Ordering::Acquire) {
            return;
        }
        self.inner
            .profiling
            .lock()
            .record(scope, stack, weight_nanos);
    }

    pub fn record_profile_stack_str_nanos(
        &self,
        scope: ProfileScope,
        stack: &str,
        weight_nanos: u64,
    ) {
        if !self.inner.profiling_enabled.load(Ordering::Acquire) {
            return;
        }
        self.inner
            .profiling
            .lock()
            .record_str(scope, stack, weight_nanos);
    }

    pub fn record_profile_stack_parts_nanos(
        &self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        weight_nanos: u64,
    ) {
        if !self.inner.profiling_enabled.load(Ordering::Acquire) {
            return;
        }
        self.inner
            .profiling
            .lock()
            .record_parts(scope, prefix, suffix, weight_nanos);
    }

    pub fn folded_profile(
        &self,
        current_ticks: u64,
        filter: &ProfileFilter,
        limit: u32,
    ) -> alloc::vec::Vec<FoldedProfileSample> {
        let _ = current_ticks;
        self.inner
            .profiling
            .lock()
            .folded(filter, core::iter::empty(), limit)
    }

    pub fn record_perf_metric_parts_nanos(
        &self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        elapsed_nanos: u64,
        counters: HardwarePerfCounterDelta,
        bytes: u64,
    ) {
        if !self.inner.profiling_enabled.load(Ordering::Acquire) {
            return;
        }
        self.inner.perf_metrics.lock().record_parts(
            scope,
            prefix,
            suffix,
            elapsed_nanos,
            counters,
            bytes,
        );
    }

    pub fn record_perf_metric_parts_events_nanos(
        &self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        events: u64,
        elapsed_nanos: u64,
        counters: HardwarePerfCounterDelta,
        bytes: u64,
    ) {
        if !self.inner.profiling_enabled.load(Ordering::Acquire) {
            return;
        }
        self.inner.perf_metrics.lock().record_parts_events(
            scope,
            prefix,
            suffix,
            events,
            elapsed_nanos,
            counters,
            bytes,
        );
    }

    pub fn record_perf_metric_str_nanos(
        &self,
        scope: ProfileScope,
        name: &str,
        elapsed_nanos: u64,
        counters: HardwarePerfCounterDelta,
        bytes: u64,
    ) {
        if !self.inner.profiling_enabled.load(Ordering::Acquire) {
            return;
        }
        self.inner
            .perf_metrics
            .lock()
            .record_str(scope, name, elapsed_nanos, counters, bytes);
    }

    pub fn record_kernel_heap_metrics(&self, stats: HeapStats) {
        if !self.inner.profiling_enabled.load(Ordering::Acquire) {
            return;
        }

        let allocations = self.inner.heap_perf_snapshot.allocation_count_delta(stats);
        let allocation_bytes = self.inner.heap_perf_snapshot.allocation_bytes_delta(stats);
        record_heap_delta_metric(
            &self.inner.perf_metrics,
            "alloc",
            allocations,
            allocation_bytes,
        );
        record_heap_size_class_delta_metrics(
            &self.inner.perf_metrics,
            HeapMetricKind::Alloc,
            self.inner
                .heap_perf_snapshot
                .allocation_size_class_count_delta(stats),
            self.inner
                .heap_perf_snapshot
                .allocation_size_class_bytes_delta(stats),
        );

        let deallocations = self
            .inner
            .heap_perf_snapshot
            .deallocation_count_delta(stats);
        let deallocation_bytes = self
            .inner
            .heap_perf_snapshot
            .deallocation_bytes_delta(stats);
        record_heap_delta_metric(
            &self.inner.perf_metrics,
            "dealloc",
            deallocations,
            deallocation_bytes,
        );
        record_heap_size_class_delta_metrics(
            &self.inner.perf_metrics,
            HeapMetricKind::Dealloc,
            self.inner
                .heap_perf_snapshot
                .deallocation_size_class_count_delta(stats),
            self.inner
                .heap_perf_snapshot
                .deallocation_size_class_bytes_delta(stats),
        );

        let reallocations = self
            .inner
            .heap_perf_snapshot
            .reallocation_count_delta(stats);
        let reallocation_bytes = self
            .inner
            .heap_perf_snapshot
            .reallocation_bytes_delta(stats);
        record_heap_delta_metric(
            &self.inner.perf_metrics,
            "realloc",
            reallocations,
            reallocation_bytes,
        );
        record_heap_size_class_delta_metrics(
            &self.inner.perf_metrics,
            HeapMetricKind::Realloc,
            self.inner
                .heap_perf_snapshot
                .reallocation_size_class_count_delta(stats),
            self.inner
                .heap_perf_snapshot
                .reallocation_size_class_bytes_delta(stats),
        );
    }

    #[track_caller]
    pub fn record_perf_metric_at_caller_nanos(
        &self,
        scope: ProfileScope,
        elapsed_nanos: u64,
        counters: HardwarePerfCounterDelta,
        bytes: u64,
    ) {
        if !self.inner.profiling_enabled.load(Ordering::Acquire) {
            return;
        }
        let caller = Location::caller();
        let name = format!("kernel;callsite;{}:{}", caller.file(), caller.line());
        self.inner
            .perf_metrics
            .lock()
            .record_str(scope, &name, elapsed_nanos, counters, bytes);
    }

    pub fn perf_metrics(
        &self,
        filter: &PerfMetricFilter,
        limit: u32,
    ) -> alloc::vec::Vec<PerfMetricSample> {
        self.inner.perf_metrics.lock().recent(filter, limit)
    }

    pub fn ticks_to_nanos(&self, ticks: u64) -> u64 {
        ticks.saturating_mul(1_000_000_000) / self.inner.timebase_frequency
    }

    pub fn uptime_nanos(&self, current_ticks: u64) -> u64 {
        self.ticks_to_nanos(current_ticks.saturating_sub(self.inner.boot_ticks))
    }

    pub fn instance_registry(&self) -> InstanceRegistry {
        self.inner.instance_registry.clone()
    }

    pub fn install_program_service(&self, service: ProgramService) {
        let mut slot = self.inner.program_service.lock();
        assert!(
            slot.is_none(),
            "program service was installed more than once"
        );
        *slot = Some(service);
        self.inner.program_service_ready.notify_all();
    }

    pub fn program_service(&self) -> Option<ProgramService> {
        self.inner.program_service.lock().clone()
    }

    pub async fn wait_for_program_service(&self) -> ProgramService {
        loop {
            if let Some(service) = self.program_service() {
                return service;
            }

            self.inner.program_service_ready.notified().await;
        }
    }

    pub fn install_network_service(&self, service: NetworkService) {
        let mut slot = self.inner.network_service.lock();
        assert!(
            slot.is_none(),
            "network service was installed more than once"
        );
        *slot = Some(service);
    }

    pub fn network_service(&self) -> Option<NetworkService> {
        self.inner.network_service.lock().clone()
    }

    pub fn install_host_fs_service(&self, service: HostFsService) {
        let mut slot = self.inner.host_fs_service.lock();
        assert!(
            slot.is_none(),
            "host-fs service was installed more than once"
        );
        *slot = Some(service);
    }

    pub fn host_fs_service(&self) -> Option<HostFsService> {
        self.inner.host_fs_service.lock().clone()
    }

    pub fn prepare_futex_wait(&self, key: FutexKey) -> FutexWaitRegistration {
        self.inner.futex_table.lock().prepare_wait(key)
    }

    pub fn complete_futex_wait(&self, registration: FutexWaitRegistration) {
        self.inner.futex_table.lock().complete_wait(registration);
    }

    pub fn wake_futex(&self, key: FutexKey, count: usize) -> usize {
        self.inner.futex_table.lock().wake(key, count)
    }

    pub fn wake_all_futex(&self, key: FutexKey) -> usize {
        self.inner.futex_table.lock().wake_all(key)
    }

    pub fn bootfs(&self) -> Option<EmbeddedBootFs> {
        *self.inner.bootfs.lock()
    }

    pub fn retire_bootfs(&self) {
        *self.inner.bootfs.lock() = None;
    }
}

impl<ProgramService, NetworkService, HostFsService> ComponentRuntimeState
    for RuntimeState<ProgramService, NetworkService, HostFsService>
where
    ProgramService: Clone + Send + 'static,
    NetworkService: Clone + Send + 'static,
    HostFsService: Clone + Send + 'static,
{
    fn uptime_nanos(&self, current_ticks: u64) -> u64 {
        RuntimeState::uptime_nanos(self, current_ticks)
    }

    fn record_console_text(&self, current_ticks: u64, text: &str) {
        RuntimeState::record_console_text(self, current_ticks, text);
    }

    fn profiling_enabled(&self) -> bool {
        RuntimeState::profiling_enabled(self)
    }

    fn record_profile_stack_nanos(
        &self,
        scope: ProfileScope,
        stack: alloc::string::String,
        weight_nanos: u64,
    ) {
        RuntimeState::record_profile_stack_nanos(self, scope, stack, weight_nanos);
    }

    fn record_profile_stack_parts_nanos(
        &self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        weight_nanos: u64,
    ) {
        RuntimeState::record_profile_stack_parts_nanos(self, scope, prefix, suffix, weight_nanos);
    }

    fn record_perf_metric_parts_nanos(
        &self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        elapsed_nanos: u64,
        counters: HardwarePerfCounterDelta,
        bytes: u64,
    ) {
        RuntimeState::record_perf_metric_parts_nanos(
            self,
            scope,
            prefix,
            suffix,
            elapsed_nanos,
            counters,
            bytes,
        );
    }

    fn record_perf_metric_parts_events_nanos(
        &self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        events: u64,
        elapsed_nanos: u64,
        counters: HardwarePerfCounterDelta,
        bytes: u64,
    ) {
        RuntimeState::record_perf_metric_parts_events_nanos(
            self,
            scope,
            prefix,
            suffix,
            events,
            elapsed_nanos,
            counters,
            bytes,
        );
    }
}

impl<ProgramService, NetworkService, HostFsService> ComponentHostFilesystemState<HostFsService>
    for RuntimeState<ProgramService, NetworkService, HostFsService>
where
    ProgramService: Clone + Send + 'static,
    NetworkService: Clone + Send + 'static,
    HostFsService: crate::HostFileSystem,
{
    fn host_filesystem_service(&self) -> Option<HostFsService> {
        self.host_fs_service()
    }

    fn bootfs(&self) -> Option<EmbeddedBootFs> {
        RuntimeState::bootfs(self)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn runtime_state_records_kernel_heap_delta_metrics() {
        let state = RuntimeState::<(), (), ()>::new(1_000_000_000, 1, 0);
        let baseline = crate::heap_stats();
        state.inner.heap_perf_snapshot.reset(baseline);
        state.set_profiling_enabled(true);

        let mut current = baseline;
        current.allocation_count += 3;
        current.total_allocation_bytes += 96;
        current.deallocation_count += 2;
        current.total_deallocation_bytes += 64;
        current.reallocation_count += 1;
        current.total_reallocation_bytes += 128;
        current.size_class_allocation_count[3] += 2;
        current.size_class_allocation_bytes[3] += 96;
        current.size_class_deallocation_count[2] += 1;
        current.size_class_deallocation_bytes[2] += 32;
        current.size_class_reallocation_count[4] += 1;
        current.size_class_reallocation_bytes[4] += 128;
        state.inner.heap_perf_snapshot.reset(baseline);
        state.record_kernel_heap_metrics(current);

        let samples = state.perf_metrics(
            &PerfMetricFilter {
                name_prefixes: vec!["kernel;heap;".into()],
            },
            8,
        );

        let alloc = samples
            .iter()
            .find(|sample| sample.name == "kernel;heap;alloc")
            .expect("alloc metric should be recorded");
        assert_eq!(alloc.total_events, 3);
        assert_eq!(alloc.total_bytes, 96);

        let dealloc = samples
            .iter()
            .find(|sample| sample.name == "kernel;heap;dealloc")
            .expect("dealloc metric should be recorded");
        assert_eq!(dealloc.total_events, 2);
        assert_eq!(dealloc.total_bytes, 64);

        let realloc = samples
            .iter()
            .find(|sample| sample.name == "kernel;heap;realloc")
            .expect("realloc metric should be recorded");
        assert_eq!(realloc.total_events, 1);
        assert_eq!(realloc.total_bytes, 128);

        let alloc_size = samples
            .iter()
            .find(|sample| sample.name == "kernel;heap;alloc-size-0064")
            .expect("alloc size class metric should be recorded");
        assert_eq!(alloc_size.total_events, 2);
        assert_eq!(alloc_size.total_bytes, 96);

        let dealloc_size = samples
            .iter()
            .find(|sample| sample.name == "kernel;heap;dealloc-size-0032")
            .expect("dealloc size class metric should be recorded");
        assert_eq!(dealloc_size.total_events, 1);
        assert_eq!(dealloc_size.total_bytes, 32);

        let realloc_size = samples
            .iter()
            .find(|sample| sample.name == "kernel;heap;realloc-size-0128")
            .expect("realloc size class metric should be recorded");
        assert_eq!(realloc_size.total_events, 1);
        assert_eq!(realloc_size.total_bytes, 128);
    }
}
