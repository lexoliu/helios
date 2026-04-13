extern crate alloc;

use alloc::sync::Arc;

use crate::{
    DEFAULT_TRACE_HISTORY_CAPACITY, InstanceRegistry, Notify, StatsSample, TraceEvent, TraceFilter,
    TraceHistory,
};
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
    tracing: Mutex<TraceHistory>,
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
                tracing: Mutex::new(TraceHistory::new(DEFAULT_TRACE_HISTORY_CAPACITY)),
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
}
