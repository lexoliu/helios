extern crate alloc;

use alloc::sync::Arc;

use helios_kernel::{
    DEFAULT_TRACE_HISTORY_CAPACITY, InstanceRegistry, Notify, StatsSample, TraceEvent, TraceFilter,
    TraceHistory,
};
use spin::Mutex;

#[derive(Clone)]
pub(crate) struct RuntimeState {
    inner: Arc<RuntimeStateInner>,
}

struct RuntimeStateInner {
    boot_ticks: u64,
    timebase_frequency: u64,
    processor_count: u32,
    instance_registry: InstanceRegistry,
    program_service: Mutex<Option<crate::program_host::UserProgramService>>,
    program_service_ready: Notify,
    network_service: Mutex<Option<crate::net::NetworkService>>,
    host_fs_service: Mutex<Option<crate::host_fs::HostFileSystemService>>,
    tracing: Mutex<TraceHistory>,
}

impl RuntimeState {
    pub(crate) fn new(timebase_frequency: u64, processor_count: usize, boot_ticks: u64) -> Self {
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

    pub(crate) fn snapshot(&self, current_ticks: u64) -> StatsSample {
        let uptime = self.ticks_to_nanos(current_ticks.saturating_sub(self.inner.boot_ticks));
        StatsSample {
            timestamp: uptime,
            uptime,
            configured_processors: self.inner.processor_count,
            online_processors: self.inner.processor_count,
        }
    }

    pub(crate) fn record_console_text(&self, current_ticks: u64, text: &str) {
        let timestamp = self.ticks_to_nanos(current_ticks.saturating_sub(self.inner.boot_ticks));
        self.inner
            .tracing
            .lock()
            .record_console_text(timestamp, text);
    }

    pub(crate) fn recent(&self, filter: &TraceFilter, limit: u32) -> alloc::vec::Vec<TraceEvent> {
        self.inner.tracing.lock().recent(filter, limit)
    }

    pub(crate) fn next_after(
        &self,
        cursor: u64,
        filter: &TraceFilter,
    ) -> Option<(u64, TraceEvent)> {
        self.inner.tracing.lock().next_after(cursor, filter)
    }

    pub(crate) fn ticks_to_nanos(&self, ticks: u64) -> u64 {
        ticks.saturating_mul(1_000_000_000) / self.inner.timebase_frequency
    }

    pub(crate) fn uptime_nanos(&self, current_ticks: u64) -> u64 {
        self.ticks_to_nanos(current_ticks.saturating_sub(self.inner.boot_ticks))
    }

    pub(crate) fn instance_registry(&self) -> InstanceRegistry {
        self.inner.instance_registry.clone()
    }

    pub(crate) fn install_program_service(&self, service: crate::program_host::UserProgramService) {
        let mut slot = self.inner.program_service.lock();
        assert!(
            slot.is_none(),
            "program service was installed more than once"
        );
        *slot = Some(service);
        self.inner.program_service_ready.notify_all();
    }

    pub(crate) fn program_service(&self) -> Option<crate::program_host::UserProgramService> {
        self.inner.program_service.lock().clone()
    }

    pub(crate) async fn wait_for_program_service(&self) -> crate::program_host::UserProgramService {
        loop {
            if let Some(service) = self.program_service() {
                return service;
            }

            self.inner.program_service_ready.notified().await;
        }
    }

    pub(crate) fn install_network_service(&self, service: crate::net::NetworkService) {
        let mut slot = self.inner.network_service.lock();
        assert!(
            slot.is_none(),
            "network service was installed more than once"
        );
        *slot = Some(service);
    }

    pub(crate) fn network_service(&self) -> Option<crate::net::NetworkService> {
        self.inner.network_service.lock().clone()
    }

    pub(crate) fn install_host_fs_service(&self, service: crate::host_fs::HostFileSystemService) {
        let mut slot = self.inner.host_fs_service.lock();
        assert!(
            slot.is_none(),
            "host-fs service was installed more than once"
        );
        *slot = Some(service);
    }

    pub(crate) fn host_fs_service(&self) -> Option<crate::host_fs::HostFileSystemService> {
        self.inner.host_fs_service.lock().clone()
    }
}
