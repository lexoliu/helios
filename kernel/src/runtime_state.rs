extern crate alloc;

use alloc::format;
use core::panic::Location;
use core::sync::atomic::{AtomicBool, Ordering};
use triomphe::Arc;

use crate::{
    DEFAULT_PERF_METRIC_CAPACITY, DEFAULT_PROFILE_STACK_CAPACITY, DEFAULT_TRACE_HISTORY_CAPACITY,
    EmbeddedBootFs, FoldedProfileSample, FutexKey, FutexTable, FutexWaitRegistration,
    InstanceRegistry, Notify, PerfMetricFilter, PerfMetricHistory, PerfMetricSample, ProfileFilter,
    ProfileHistory, ProfileScope, StatsSample, TraceEvent, TraceFilter, TraceHistory,
    embedded_init,
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
