extern crate alloc;

use alloc::format;
use core::panic::Location;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use triomphe::Arc;

use crate::{
    DEFAULT_PERF_METRIC_CAPACITY, DEFAULT_PROFILE_STACK_CAPACITY, DEFAULT_TRACE_HISTORY_CAPACITY,
    EmbeddedBootFs, FoldedProfileSample, FutexKey, FutexTable, FutexWaitRegistration,
    HEAP_SIZE_CLASS_COUNT, HeapStats, InstanceRegistry, Notify, PerfMetricFilter,
    PerfMetricHistory, PerfMetricSample, PerfSample, ProfileFilter, ProfileScope, ProfileSink,
    StatsSample, TraceEvent, TraceFilter, TraceHistory, embedded_init,
};
use crate::{RootEntropy, RootEntropyHandle};
use helios_hal::cpu::HardwarePerfCounterDelta;
use helios_hal::rtc::{RealTimeClock, UnixSeconds};
use spin::{Mutex, Once};

use crate::memory::BalloonHandle;

use crate::BlockService;
use crate::ComponentHostVsockService;
use crate::component::{ComponentRuntimeState, ProviderSlot};
use crate::device::DeviceGrantRegistry;
use crate::network::HttpExchange;
use crate::runtime::types::ComponentHostFilesystemState;

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
    network_service_installed: AtomicBool,
    network_service: Once<NetworkService>,
    /// The boot-seeded root DRBG. Installed by the backend before any
    /// component runs; every instance's `EntropyPool` is derived from
    /// it, so a kernel that reaches component start-up without one is a
    /// bring-up bug rather than a degraded mode.
    root_entropy: Once<RootEntropyHandle>,
    /// Nanoseconds between the monotonic clock and wall time, as the
    /// platform's real-time clock set them during bring-up. Empty on a
    /// machine that carries no such clock, where wall time is uptime.
    wall_clock_offset_nanos: Once<i128>,
    /// Queue into the `http-client` kernel plugin. Empty on a kernel image
    /// that does not provision the plugin, in which case the runtime adapter
    /// answers a configuration error rather than trapping.
    http_client: ProviderSlot<HttpExchange>,
    host_fs_service: Mutex<Option<HostFsService>>,
    /// The scratch disk the platform gave this kernel, once it has been
    /// identified and proved. Empty on a machine with no block device.
    block_service: Once<BlockService>,
    /// The devices the machine is willing to hand to user-mode drivers,
    /// as discovery published them. Empty on a machine whose backend
    /// found nothing outside the hardware it drives itself.
    device_grants: DeviceGrantRegistry,
    /// The machine's display, once the backend brought the device up
    /// and the kernel took ownership of it. Empty on a machine with no
    /// display device, where a claim is refused rather than trapping.
    display_service: Once<crate::display::DisplayService>,
    /// The machine's input devices, once the backend brought them up
    /// and the kernel took ownership of them. Empty on a machine with
    /// none, where a claim is refused rather than trapping.
    input_service: Once<crate::input::InputService>,
    /// The machine's client windows. Always present — it costs a few
    /// words and a program may ask for a window on any machine — and
    /// empty of surfaces until a compositor is provisioned.
    surface_service: crate::surface::SurfaceService,
    /// Queue into the `compositor` kernel plugin. Empty on a kernel
    /// image that does not provision the plugin, in which case
    /// `helios:system/surface` answers `unavailable` rather than
    /// trapping.
    compositor: ProviderSlot<crate::surface::SurfaceRequest>,
    /// The machine's sound device, once the backend brought it up and
    /// the kernel took ownership of it. Empty on a machine with none,
    /// where a claim is refused rather than trapping.
    audio_service: Once<crate::audio::AudioService>,
    /// What the platform's IOMMU confines, once the backend has built
    /// the domains. Empty on a machine whose devices are not behind one.
    iommu_report: Once<alloc::sync::Arc<crate::IommuReport>>,
    /// The memory balloon the host resizes this guest through. Empty on
    /// a machine that gave the kernel none.
    balloon: Once<BalloonHandle>,
    /// Swap, once a backend with a lazy-commit address space and a disk
    /// to write to has brought it up. Empty everywhere else.
    swap: Once<crate::SwapHandle>,
    /// The machine's link to its host, once a backend brought a vsock
    /// device up. Empty on a machine with no vsock device, where the
    /// runtime adapter answers `unavailable` rather than trapping.
    vsock_service: Once<ComponentHostVsockService>,
    futex_table: Mutex<FutexTable>,
    bootfs: Mutex<Option<EmbeddedBootFs>>,
    tracing: Mutex<TraceHistory>,
    /// The profile and perf histories, held as a handle rather than as
    /// fields so a subsystem this state owns can record into them
    /// without naming the state's own type (see [`ProfileSink`]).
    profiles: ProfileSink,
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
    metrics.lock().record_parts(
        ProfileScope::Kernel,
        "kernel;heap;",
        name,
        PerfSample {
            events,
            elapsed_nanos: 0,
            counters: HardwarePerfCounterDelta::default(),
            bytes,
        },
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
                network_service_installed: AtomicBool::new(false),
                network_service: Once::new(),
                root_entropy: Once::new(),
                wall_clock_offset_nanos: Once::new(),
                http_client: ProviderSlot::new(),
                host_fs_service: Mutex::new(None),
                block_service: Once::new(),
                device_grants: DeviceGrantRegistry::new(),
                iommu_report: Once::new(),
                balloon: Once::new(),
                swap: Once::new(),
                display_service: Once::new(),
                input_service: Once::new(),
                surface_service: crate::surface::SurfaceService::new(),
                compositor: ProviderSlot::new(),
                audio_service: Once::new(),
                vsock_service: Once::new(),
                futex_table: Mutex::new(FutexTable::new()),
                bootfs: Mutex::new(embedded_init().map(|init| init.bootfs())),
                tracing: Mutex::new(TraceHistory::new(DEFAULT_TRACE_HISTORY_CAPACITY)),
                profiles: ProfileSink::new(
                    DEFAULT_PROFILE_STACK_CAPACITY,
                    DEFAULT_PERF_METRIC_CAPACITY,
                ),
                heap_perf_snapshot: HeapPerfSnapshot::new(),
            }),
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
        self.inner.profiles.set_enabled(enabled);
    }

    /// The profile and perf histories this state records into.
    ///
    /// Handed to the subsystems the state itself owns — the network
    /// service above all — so they can record without holding the state
    /// that holds them.
    pub fn profiles(&self) -> ProfileSink {
        self.inner.profiles.clone()
    }

    pub fn profiling_enabled(&self) -> bool {
        self.inner.profiles.enabled()
    }

    pub fn clear_profile(&self) {
        self.inner.profiles.profiling().lock().clear();
        self.inner.profiles.perf_metrics().lock().clear();
        crate::set_kernel_heap_size_class_metrics_enabled(self.inner.profiles.enabled());
        self.inner.heap_perf_snapshot.reset(crate::heap_stats());
    }

    pub fn record_profile_stack(
        &self,
        scope: ProfileScope,
        stack: alloc::string::String,
        weight_ticks: u64,
    ) {
        if !self.inner.profiles.enabled() {
            return;
        }
        let weight = self.ticks_to_nanos(weight_ticks);
        self.inner
            .profiles
            .profiling()
            .lock()
            .record(scope, stack, weight);
    }

    pub fn record_profile_stack_str(&self, scope: ProfileScope, stack: &str, weight_ticks: u64) {
        if !self.inner.profiles.enabled() {
            return;
        }
        let weight = self.ticks_to_nanos(weight_ticks);
        self.inner
            .profiles
            .profiling()
            .lock()
            .record_str(scope, stack, weight);
    }

    pub fn record_profile_stack_parts(
        &self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        weight_ticks: u64,
    ) {
        if !self.inner.profiles.enabled() {
            return;
        }
        let weight = self.ticks_to_nanos(weight_ticks);
        self.inner
            .profiles
            .profiling()
            .lock()
            .record_parts(scope, prefix, suffix, weight);
    }

    pub fn record_profile_stack_nanos(
        &self,
        scope: ProfileScope,
        stack: alloc::string::String,
        weight_nanos: u64,
    ) {
        if !self.inner.profiles.enabled() {
            return;
        }
        self.inner
            .profiles
            .profiling()
            .lock()
            .record(scope, stack, weight_nanos);
    }

    pub fn record_profile_stack_str_nanos(
        &self,
        scope: ProfileScope,
        stack: &str,
        weight_nanos: u64,
    ) {
        if !self.inner.profiles.enabled() {
            return;
        }
        self.inner
            .profiles
            .profiling()
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
        if !self.inner.profiles.enabled() {
            return;
        }
        self.inner
            .profiles
            .profiling()
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
            .profiles
            .profiling()
            .lock()
            .folded(filter, core::iter::empty(), limit)
    }

    pub fn record_perf_metric_parts(
        &self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        sample: PerfSample,
    ) {
        if !self.inner.profiles.enabled() {
            return;
        }
        self.inner
            .profiles
            .perf_metrics()
            .lock()
            .record_parts(scope, prefix, suffix, sample);
    }

    pub fn record_perf_metric_str(&self, scope: ProfileScope, name: &str, sample: PerfSample) {
        if !self.inner.profiles.enabled() {
            return;
        }
        self.inner
            .profiles
            .perf_metrics()
            .lock()
            .record_str(scope, name, sample);
    }

    pub fn record_kernel_heap_metrics(&self, stats: HeapStats) {
        if !self.inner.profiles.enabled() {
            return;
        }

        let metrics = self.inner.profiles.perf_metrics();
        let snapshot = &self.inner.heap_perf_snapshot;

        record_heap_delta_metric(
            metrics,
            "alloc",
            swap_delta(&snapshot.allocation_count, stats.allocation_count),
            swap_delta(
                &snapshot.total_allocation_bytes,
                stats.total_allocation_bytes,
            ),
        );
        record_heap_size_class_delta_metrics(
            metrics,
            HeapMetricKind::Alloc,
            heap_size_class_delta(
                &snapshot.size_class_allocation_count,
                stats.size_class_allocation_count,
            ),
            heap_size_class_delta(
                &snapshot.size_class_allocation_bytes,
                stats.size_class_allocation_bytes,
            ),
        );

        record_heap_delta_metric(
            metrics,
            "dealloc",
            swap_delta(&snapshot.deallocation_count, stats.deallocation_count),
            swap_delta(
                &snapshot.total_deallocation_bytes,
                stats.total_deallocation_bytes,
            ),
        );
        record_heap_size_class_delta_metrics(
            metrics,
            HeapMetricKind::Dealloc,
            heap_size_class_delta(
                &snapshot.size_class_deallocation_count,
                stats.size_class_deallocation_count,
            ),
            heap_size_class_delta(
                &snapshot.size_class_deallocation_bytes,
                stats.size_class_deallocation_bytes,
            ),
        );

        record_heap_delta_metric(
            metrics,
            "realloc",
            swap_delta(&snapshot.reallocation_count, stats.reallocation_count),
            swap_delta(
                &snapshot.total_reallocation_bytes,
                stats.total_reallocation_bytes,
            ),
        );
        record_heap_size_class_delta_metrics(
            metrics,
            HeapMetricKind::Realloc,
            heap_size_class_delta(
                &snapshot.size_class_reallocation_count,
                stats.size_class_reallocation_count,
            ),
            heap_size_class_delta(
                &snapshot.size_class_reallocation_bytes,
                stats.size_class_reallocation_bytes,
            ),
        );
    }

    #[track_caller]
    pub fn record_perf_metric_at_caller(&self, scope: ProfileScope, sample: PerfSample) {
        if !self.inner.profiles.enabled() {
            return;
        }
        let caller = Location::caller();
        let name = format!("kernel;callsite;{}:{}", caller.file(), caller.line());
        self.inner
            .profiles
            .perf_metrics()
            .lock()
            .record_str(scope, &name, sample);
    }

    pub fn perf_metrics(
        &self,
        filter: &PerfMetricFilter,
        limit: u32,
    ) -> alloc::vec::Vec<PerfMetricSample> {
        self.inner
            .profiles
            .perf_metrics()
            .lock()
            .recent(filter, limit)
    }

    pub fn ticks_to_nanos(&self, ticks: u64) -> u64 {
        crate::exec::ticks_to_nanos(ticks, self.inner.timebase_frequency)
    }

    pub fn uptime_nanos(&self, current_ticks: u64) -> u64 {
        self.uptime_clock().nanos_at(current_ticks)
    }

    /// The kernel's uptime clock, for a subsystem that reads uptime
    /// without holding the runtime state.
    ///
    /// The network service is one: it timestamps every protocol
    /// deadline and every profile sample, and it no longer carries the
    /// runtime state that used to answer for it. Handing it this keeps
    /// one origin for both.
    pub fn uptime_clock(&self) -> crate::UptimeClock {
        crate::UptimeClock::new(self.inner.boot_ticks, self.inner.timebase_frequency)
    }

    /// Places the monotonic clock on the wall, from the platform's
    /// real-time clock.
    ///
    /// The backend calls this once, on the bootstrap processor, after
    /// the kernel's log is installed and before any component runs, so
    /// every store the runtime opens afterwards reads the same calendar.
    /// The clock is read exactly once: the monotonic timer carries time
    /// forward from here, and nothing re-synchronises it.
    ///
    /// A clock that answers with something no calendar can mean is a
    /// bring-up fault rather than a degraded mode, so it panics instead
    /// of leaving the kernel quietly running at the epoch.
    pub fn seed_wall_clock<Rtc>(&self, current_ticks: u64, rtc: &Rtc) -> UnixSeconds
    where
        Rtc: RealTimeClock,
    {
        let wall = rtc.read().unwrap_or_else(|error| {
            panic!(
                "platform real-time clock {} could not be read: {error}",
                Rtc::SOURCE
            )
        });
        let offset = crate::exec::wall_clock_offset_nanos(wall, self.uptime_nanos(current_ticks));
        let mut installed = false;
        self.inner.wall_clock_offset_nanos.call_once(|| {
            installed = true;
            offset
        });
        assert!(installed, "the wall clock was seeded more than once");
        tracing::info!(
            "wall clock seeded source={} unix_seconds={}",
            Rtc::SOURCE,
            wall.get()
        );
        wall
    }

    /// Nanoseconds between the monotonic clock and wall time, zero until
    /// a real-time clock has seeded it.
    pub fn wall_clock_offset_nanos(&self) -> i128 {
        self.inner
            .wall_clock_offset_nanos
            .get()
            .copied()
            .unwrap_or_default()
    }

    /// Wall time in nanoseconds since the Unix epoch, as the seeded
    /// offset places the monotonic reading at `current_ticks`.
    pub fn wall_clock_nanos(&self, current_ticks: u64) -> u64 {
        let value = i128::from(self.uptime_nanos(current_ticks)) + self.wall_clock_offset_nanos();
        u64::try_from(value).unwrap_or_default()
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
            // Armed before the slot is read: installation broadcasts to
            // every task waiting for the service and banks nothing, so a
            // wait created after an install it just missed would park
            // for a second install that never comes.
            let installed = self.inner.program_service_ready.notified();
            if let Some(service) = self.program_service() {
                return service;
            }

            installed.await;
        }
    }

    pub fn install_network_service(&self, service: NetworkService) {
        assert!(
            !self
                .inner
                .network_service_installed
                .swap(true, Ordering::AcqRel),
            "network service was installed more than once"
        );
        self.inner.network_service.call_once(|| service);
    }

    pub fn network_service(&self) -> Option<NetworkService> {
        self.inner.network_service.get().cloned()
    }

    /// The hand-off slot for the `http-client` kernel plugin.
    ///
    /// The plugin supervisor installs the queue during startup; the runtime
    /// adapter's HTTP client binding sends every exchange through it. The slot
    /// stays empty when the plugin is not provisioned, which that binding
    /// reports to the guest as a configuration error.
    pub fn http_client(&self) -> &ProviderSlot<HttpExchange> {
        &self.inner.http_client
    }

    /// The devices discovery is willing to hand to user-mode drivers.
    ///
    /// The registry is the one place a device's ownership is decided, so
    /// a supervisor claims through it and the inspector lists through
    /// it.
    pub fn device_grants(&self) -> &DeviceGrantRegistry {
        &self.inner.device_grants
    }

    /// Publishes the machine's display.
    ///
    /// # Panics
    ///
    /// Panics when a second display is installed. One machine has one
    /// display device the kernel owns, and two would make a claim
    /// ambiguous.
    pub fn install_display_service(&self, service: crate::display::DisplayService) {
        let mut installed = false;
        self.inner.display_service.call_once(|| {
            installed = true;
            service
        });
        assert!(installed, "the display service was installed twice");
    }

    pub fn display_service(&self) -> Option<crate::display::DisplayService> {
        self.inner.display_service.get().cloned()
    }

    /// Publishes the machine's input devices.
    ///
    /// # Panics
    ///
    /// Panics when a second set is installed. One machine's devices are
    /// brought up once, together, and a second set would leave two
    /// answers to `available` and two claim words per device.
    pub fn install_input_service(&self, service: crate::input::InputService) {
        let mut installed = false;
        self.inner.input_service.call_once(|| {
            installed = true;
            service
        });
        assert!(installed, "the input service was installed twice");
    }

    pub fn input_service(&self) -> Option<crate::input::InputService> {
        self.inner.input_service.get().cloned()
    }

    /// The machine's client windows.
    pub fn surface_service(&self) -> crate::surface::SurfaceService {
        self.inner.surface_service.clone()
    }

    /// The hand-off slot for the `compositor` kernel plugin.
    ///
    /// The plugin supervisor installs the queue during startup; every
    /// `helios:system/surface` call sends its work through it. The slot
    /// stays empty when the plugin is not provisioned, which that
    /// binding reports to the guest as `unavailable`.
    pub fn compositor(&self) -> &ProviderSlot<crate::surface::SurfaceRequest> {
        &self.inner.compositor
    }

    /// Publishes the machine's sound device.
    ///
    /// # Panics
    ///
    /// Panics when a second device is installed. One machine's sound
    /// device is brought up once, and a second would leave two answers
    /// to `available` and two claim words per stream.
    pub fn install_audio_service(&self, service: crate::audio::AudioService) {
        let mut installed = false;
        self.inner.audio_service.call_once(|| {
            installed = true;
            service
        });
        assert!(installed, "the audio service was installed twice");
    }

    pub fn audio_service(&self) -> Option<crate::audio::AudioService> {
        self.inner.audio_service.get().cloned()
    }

    /// Publishes the root DRBG the backend seeded at boot.
    pub fn install_root_entropy(&self, root: RootEntropyHandle) {
        let mut installed = false;
        self.inner.root_entropy.call_once(|| {
            installed = true;
            root
        });
        assert!(installed, "root entropy was installed more than once");
    }

    /// The root DRBG every instance pool is derived from.
    pub fn root_entropy(&self) -> &RootEntropy {
        self.inner
            .root_entropy
            .get()
            .unwrap_or_else(|| panic!("root entropy was used before the backend installed it"))
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

    /// Publishes the block device the kernel owns.
    ///
    /// Called from the task that identified the disk and proved it round
    /// trips, so a service that is visible here is one every consumer may
    /// use without checking it first.
    pub fn install_block_service(&self, service: BlockService) {
        let mut installed = false;
        self.inner.block_service.call_once(|| {
            installed = true;
            service
        });
        assert!(installed, "block service was installed more than once");
    }

    pub fn block_service(&self) -> Option<BlockService> {
        self.inner.block_service.get().cloned()
    }

    /// Publishes what the platform's IOMMU confines.
    ///
    /// Called from the backend once every device it protects has been
    /// attached to its domain, so a report that is visible here already
    /// describes the machine's final device topology.
    pub fn install_iommu_report(&self, report: alloc::sync::Arc<crate::IommuReport>) {
        let mut installed = false;
        self.inner.iommu_report.call_once(|| {
            installed = true;
            report
        });
        assert!(installed, "IOMMU report was installed more than once");
    }

    /// Publishes the memory balloon the platform gave this kernel.
    pub fn install_memory_balloon(&self, balloon: BalloonHandle) {
        let mut installed = false;
        self.inner.balloon.call_once(|| {
            installed = true;
            balloon
        });
        assert!(installed, "memory balloon was installed more than once");
    }

    /// Publishes swap, once a backend has brought it up.
    pub fn install_swap(&self, swap: crate::SwapHandle) {
        let mut installed = false;
        self.inner.swap.call_once(|| {
            installed = true;
            swap
        });
        assert!(installed, "swap was installed more than once");
    }

    /// Publishes the machine's vsock link.
    ///
    /// Called from the backend that brought the device up, before any
    /// component runs, so a service visible here is one every consumer
    /// may use without checking the device first.
    pub fn install_vsock_service(&self, service: ComponentHostVsockService) {
        let mut installed = false;
        self.inner.vsock_service.call_once(|| {
            installed = true;
            service
        });
        assert!(installed, "vsock service was installed more than once");
    }

    pub fn vsock_service(&self) -> Option<ComponentHostVsockService> {
        self.inner.vsock_service.get().cloned()
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

/// The system-wide observation snapshot.
///
/// This sits in its own block because reporting the host share's cache
/// counters needs the filesystem service to actually be a filesystem,
/// which the rest of `RuntimeState` does not care about.
impl<ProgramService, NetworkService, HostFsService>
    RuntimeState<ProgramService, NetworkService, HostFsService>
where
    ProgramService: Clone,
    NetworkService: crate::NetworkAdminBackend,
    HostFsService: crate::HostFileSystem,
{
    pub fn snapshot(&self, current_ticks: u64) -> StatsSample {
        let uptime = self.ticks_to_nanos(current_ticks.saturating_sub(self.inner.boot_ticks));
        StatsSample {
            timestamp: uptime,
            uptime,
            wall_clock: self.wall_clock_nanos(current_ticks),
            configured_processors: self.inner.processor_count,
            online_processors: self.inner.processor_count,
            block: self.inner.block_service.get().map(BlockService::stats),
            iommu: self
                .inner
                .iommu_report
                .get()
                .map(|report| report.snapshot()),
            balloon: self.inner.balloon.get().map(BalloonHandle::stats),
            swap: self.inner.swap.get().map(crate::SwapHandle::stats),
            host_share: self
                .host_fs_service()
                .and_then(|service| service.cache_stats()),
            network: self
                .network_service()
                .map(|service| service.network_stats()),
            devices: self.inner.device_grants.snapshot(),
            inputs: self
                .inner
                .input_service
                .get()
                .map(crate::input::InputService::snapshot)
                .unwrap_or_default(),
        }
    }
}

impl<ProgramService, NetworkService, HostFsService> ComponentRuntimeState
    for RuntimeState<ProgramService, NetworkService, HostFsService>
where
    ProgramService: Clone + Send + 'static,
    NetworkService: crate::ComponentNetworkService,
    HostFsService: Clone + Send + 'static,
{
    fn retire_network_handles(&self, retired: &crate::SocketRetirementQueue) {
        if retired.is_empty() {
            return;
        }
        let service = self.network_service().expect(
            "a socket resource retired a network handle on a machine with no network service",
        );
        crate::retire_queued_handles(retired, &service);
    }

    fn uptime_nanos(&self, current_ticks: u64) -> u64 {
        RuntimeState::uptime_nanos(self, current_ticks)
    }

    fn wall_clock_offset_nanos(&self) -> i128 {
        RuntimeState::wall_clock_offset_nanos(self)
    }

    fn record_console_text(&self, current_ticks: u64, text: &str) {
        RuntimeState::record_console_text(self, current_ticks, text);
    }

    fn root_entropy(&self) -> &RootEntropy {
        RuntimeState::root_entropy(self)
    }

    fn memory_balloon(&self) -> Option<BalloonHandle> {
        self.inner.balloon.get().cloned()
    }

    fn device_grants(&self) -> &DeviceGrantRegistry {
        RuntimeState::device_grants(self)
    }

    fn display_service(&self) -> Option<crate::display::DisplayService> {
        RuntimeState::display_service(self)
    }

    fn input_service(&self) -> Option<crate::input::InputService> {
        RuntimeState::input_service(self)
    }

    fn surface_service(&self) -> crate::surface::SurfaceService {
        RuntimeState::surface_service(self)
    }

    fn audio_service(&self) -> Option<crate::audio::AudioService> {
        RuntimeState::audio_service(self)
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

    fn record_perf_metric_parts(
        &self,
        scope: ProfileScope,
        prefix: &str,
        suffix: &str,
        sample: PerfSample,
    ) {
        RuntimeState::record_perf_metric_parts(self, scope, prefix, suffix, sample);
    }
}

impl<ProgramService, NetworkService, HostFsService> ComponentHostFilesystemState<HostFsService>
    for RuntimeState<ProgramService, NetworkService, HostFsService>
where
    ProgramService: Clone + Send + 'static,
    NetworkService: Clone + Send + Sync + 'static,
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
