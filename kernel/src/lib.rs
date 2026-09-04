#![no_std]
#![cfg_attr(target_os = "none", feature(alloc_error_handler))]
#![allow(hidden_glob_reexports)]
extern crate alloc;
extern crate self as helios_kernel;
#[cfg(not(target_os = "none"))]
extern crate std;

mod bootfs;
mod component;
mod embedded;
mod exec;
mod host_fs;
mod instance;
mod io;
mod kernel_exception;
mod log;
mod memory;
mod network;
mod process;
mod runtime;
#[cfg(test)]
mod test_support;
mod vsock;
#[cfg(feature = "wasmtime-runtime")]
pub(crate) mod wasmtime_adapter;
#[cfg(feature = "wasmtime-runtime")]
pub use wasmtime_adapter::swap_fault::resolve_swap_fault_blocking;
pub use wasmtime_adapter::tls::WasmtimeTlsSlots;

#[cfg(all(target_os = "none", feature = "wasmtime-bare-metal"))]
pub mod runtime_memory {
    //! Re-export of the runtime custom-virtual-memory dispatcher
    //! so bare-metal backends can install their `RuntimeMemoryHooks`
    //! tables without reaching into kernel-private modules.
    pub use crate::wasmtime_adapter::custom_vm::{
        RuntimeMemoryHooks, RuntimeMemoryImage, default_memory_image_free,
        default_memory_image_map_at, default_memory_image_new, default_page_size, install_hooks,
        publish_code_memory, unpublish_code_memory,
    };
}
pub use bootfs::{
    BootDirectory, BootDirectoryEntry, BootDirectoryHandleExt, BootFile, EmbeddedBootDirectory,
    EmbeddedBootFile, EmbeddedBootFs,
};
pub(crate) use component::ComponentCache;
pub use component::{
    CompiledComponent, ComponentExecContext, ComponentExecutor, ComponentExitStatus,
    ComponentFsNodeKind, ComponentFsPathError, ComponentFsResourceError, ComponentOutputMode,
    ComponentOutputRoute, ComponentOutputSink, ComponentOutputStreamKind, ComponentRawMutex,
    ComponentRawMutexGuard, ComponentRawRwLock, ComponentRawRwLockReadGuard,
    ComponentRawRwLockWriteGuard, ComponentResourceTableError, ComponentRunResult,
    ComponentRuntimeEngine, ComponentRuntimeFactory, ComponentRuntimeState, ComponentSerialPort,
    ComponentStoreData, ComponentTcpBackend, ComponentTcpStream, ComponentUdpBackend,
    ComponentUdpSocket, ComponentWorld, DeadlinePollable, InstanceKilled, LocalOutputSink,
    ProviderAlreadyInstalled, ProviderError, ProviderReceiver, ProviderSender, ProviderSlot,
    RawMutexGuardResource, RawMutexResource, RawRwLockReadGuardResource, RawRwLockResource,
    RawRwLockWriteGuardResource, SerialPortResource, TcpStreamResource, UdpSocketResource,
    directory_prefix, map_resource_table_error, parent_path, path_is_within_directory,
    provider_channel, resolve_absolute_path, resolve_child_path, resolve_guest_path,
    strip_directory_prefix, wait_until_runtime_deadline,
};
pub use embedded::{
    EmbeddedComponent, EmbeddedInit, embedded_boot_component, embedded_init,
    embedded_system_component, has_embedded_system_component,
};
pub use exec::{
    CompactionBudget, CompactionPolicy, CompactionReport, CompactionTarget, Compactor,
    DEFAULT_PERF_METRIC_CAPACITY, DEFAULT_PROFILE_STACK_CAPACITY, DEFAULT_TRACE_HISTORY_CAPACITY,
    Executor, ExecutorRunStats, FoldedProfileSample, InstanceSpawner, JoinHandle, KernelClock,
    LocalJoinHandle, Mutex, MutexGuard, Notified, Notify, NotifyWaiter, OwnedRawMutexLease,
    OwnedRawRwLockReadLease, OwnedRawRwLockWriteLease, PerfMetricFilter, PerfMetricHistory,
    PerfMetricSample, PerfSample, PressureLevel, ProfileFilter, ProfileHistory, ProfileScope,
    ProgressChanged, ProgressMark, ProgressSignal, RawMutex, RawMutexLease, RawRwLock,
    RawRwLockReadLease, RawRwLockWriteLease, RwLock, RwLockReadGuard, RwLockWriteGuard, Sleep,
    Spawner, StatsSample, TaskCapacityError, TaskFunding, Timer, TraceEvent, TraceField,
    TraceFilter, TraceHistory, TraceLevel, TraceValue, YieldNow, duration_to_ticks, elapsed_millis,
    matches_perf_metric_filter, matches_profile_filter, matches_trace_filter, monotonic_nanos,
    nanos_to_ticks_ceil_saturating, parse_console_text, wall_clock_offset_nanos, yield_now,
};
pub use helios_hal::Platform;
pub use helios_netstack::{
    ChecksumOffload, DEFAULT_POLL_BUDGET, EventDeliveryCapabilities, InterfaceCapabilities,
    LinkState, NetworkInterface as NetworkDevice, PacketBuffer, RxFrame, RxFrameOffload,
    SegmentationOffload, TxFrameRef,
};
pub use host_fs::{
    HOST_SHARE_GUEST_MOUNT_PATH, HOST_SHARE_MOUNT_TAG, HostFsCacheStats, HostFsClient,
    HostFsTransport, UnsupportedHostFileSystem, guest_host_share_path,
};
pub use instance::{
    CondemnedMemory, InstanceExecutionTransition, InstanceId, InstanceProfileTotal,
    InstanceRegistry, InstanceSnapshot, KillReason, OOM_RECLAIM_GRACE, OomKillDecision,
    OomKillOutcome, OomPolicy, OomVictim, RegisteredInstance, allow_instance_resource_growth,
    record_instance_transition,
};
pub use io::{
    BlockInstallError, BlockSelfCheckError, BlockService, BlockStats, ByteReadWait, ByteReader,
    ByteWriteWait, ByteWriter, ClosedPeer, ExternalInterruptHandler, ExternalInterruptRoutes,
    IommuDomains, IommuEndpointStats, IommuReport, IommuStats, MAX_BLOCK_DEVICES,
    MAX_IOMMU_ENDPOINTS, MAX_NETWORK_INTERRUPTS, PollKey, PollRegistration, PollRegistry,
    PollRegistryError, PollSourceKind, RecordingConsole, SCRATCH_DISK_SERIAL, SerialReader,
    TryRead, TryWrite, byte_channel, emit_console_line, emit_serial_error_marker,
    emit_serial_stage_marker, install_block_devices, read_serial, try_read_serial,
    wake_queue_owners, write_serial,
};
pub use kernel_exception::{
    KernelException, KernelExceptionCause, KernelExceptionDispatch, KernelNativeTrapHandler,
};
pub use memory::{
    AccessibilityPlan, BalloonHandle, BalloonStats, CommittedRegion, ENTROPY_RESEED_INTERVAL,
    EntropyPool, EntropySources, FREE_PAGE_REPORT_INTERVAL, HardwareEntropySource, IDLE_SWAP_AFTER,
    KernelPhysFrameAllocator, MemoryOwner, NoCryptographicEntropy, NoEntropyDevice,
    ROOT_ENTROPY_MATERIAL_BYTES, ReleasedReservation, ReservationLookup, ReservationTracker,
    RootEntropy, RootEntropyHandle, SWAP_BATCH_BYTES, SWAP_TICK, SwapDisabled, SwapEntry,
    SwapFaultError, SwapHandle, SwapStats, SwapVmHooks, UserHeapStats, UserMemoryOwnerScope,
    UserMemoryOwners, VaCursor, allocate_user_frame_uninit_on, allocate_user_frame_zeroed,
    allocate_user_frame_zeroed_on, configure_user_memory_owner_processors,
    current_user_memory_owner, deallocate_user_frame, deallocate_user_frame_on, disable_swap,
    enter_user_memory_owner, install_entropy_device, install_memory_balloon, install_swap,
    install_swap_hooks, installed_swap_handle, installed_swap_hooks, largest_servable_user_bytes,
    seed_root_entropy, set_user_memory_owner, swapped_token, user_heap_stats, validate_range,
};
pub use network::{
    HTTP_FORBIDDEN_FIELD_NAMES, HTTP_MAX_FIELD_SECTION_BYTES, HTTP_MAX_FIELD_VALUE_BYTES, HttpBody,
    HttpDnsErrorPayload, HttpErrorCode, HttpExchange, HttpFieldName, HttpFieldSizePayload,
    HttpFields, HttpHeaderError, HttpMethod, HttpRequestHead, HttpRequestOptions,
    HttpRequestOptionsError, HttpResponse, HttpResponseHead, HttpScheme, HttpSyntaxError,
    HttpSyntaxKind, HttpTlsAlertReceivedPayload, Ipv4Cidr, Ipv4Route, MacAddress,
    NetworkAdminBackend, NetworkBridgeRequest, NetworkBridgeSecurity, NetworkControl,
    NetworkControlError, NetworkPortId, NetworkQueueStats, NetworkService, NetworkStats,
    SocketStack, TcpListenerId, TcpStreamId, UdpSocketId, validate_http_authority,
    validate_http_path_with_query, validate_http_status_code,
};
pub use process::{
    ClockAuthorityRights, DescriptorEntry, DescriptorId, DescriptorTable, DescriptorTableError,
    DirectoryAuthorityRights, DirectoryCap, DirectoryPreopen, DnsCap, ExecAuthority, ForkAuthority,
    FutexKey, FutexTable, FutexWaitRegistration, GuestAddress, JoinAuthority, LinkAuthorityRights,
    LinkSourceCap, LinkTargetDirectoryCap, MulticastCap, NetworkAdminCap, NetworkAuthorityRights,
    NetworkCap, PrivilegedBindCap, ProcessAuthority, ProcessAuthorityError, ProcessAuthorityRights,
    ProcessId, ProcessMemoryIdentity, ProcessRecord, ProcessState, ProcessTable, ProcessTableError,
    ProgramExecError, ProgramExecErrorDetail, ProgramExecErrorKind, ProgramOutOfMemory,
    RuntimeMessage, SetWallClockCap, SignalAuthority, SpawnAuthority, SymlinkCreateCap,
    SymlinkReadCap, TcpCap, TerminalAuthorityRights, TerminalInputCap, TerminalOutputCap, ThreadId,
    ThreadRecord, ThreadState, ThreadTable, ThreadTableError, TtyControlCap, UdpCap,
};
pub use runtime::{
    AuthorityDomain, ComponentHostFilesystemState, ComponentNetworkService, ComponentNetworkState,
    DnsError, DnsErrorKind, ExecOutput, ExecResult, HostDirEntry, HostFileSystem, HostFsError,
    HostFsErrorKind, HostMetadata, Ipv4Address, NetworkErrorDetail, NetworkIpAddress,
    ObjectIdentity, PingError, PingErrorKind, PingReply, RegisteredTcpReadBuffer, RuntimeState,
    SocketReadiness, TcpAccepted, TcpError, TcpErrorKind, TcpListener, UdpBinding, UdpDatagram,
    UdpError, UdpErrorKind,
};
pub use vsock::{
    ComponentHostVsockService, MAX_VSOCK_BACKLOG, MAX_VSOCK_CONNECTIONS, MAX_VSOCK_LISTENERS,
    VSOCK_RECEIVE_WINDOW_BYTES, VsockError, VsockListenerId, VsockService, VsockStreamId,
    install_vsock_device,
};
#[cfg(feature = "wasmtime-runtime")]
pub use wasmtime_adapter::component_host::{
    ChildExit, ChildHandle, ComponentBindingSet, ComponentHostNetworkService,
    ComponentHostProcessorRole, ComponentHostTcpListenerToken, ComponentHostTcpStreamToken,
    ComponentHostUdpSocketToken, HostRuntimeState, UserProgramService,
    component_host_processor_role, component_host_processors_to_start,
    component_host_system_processor, component_host_worker_count,
    install_component_host_program_service, install_program_service,
    run_component_host_processor_forever, run_embedded_component_forever,
    run_program_workers_forever, system_component_should_run_on,
};
// Concrete runtime helpers are crate-internal only.
// External consumers use the ComponentRuntimeFactory trait.

use alloc::sync::Arc;
use alloc::task::Wake;
use core::alloc::{GlobalAlloc, Layout};
use core::future::Future;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};
use core::time::Duration;

use buddy_system_allocator::LockedHeap;
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::memory::MemoryRegion;
use helios_hal::watchdog::{NoWatchdog, ProgressCounter, Watchdog};
use helios_hal::{DeviceInventory, DmaModel, ProcessorStartupPolicy, ProcessorTopology};

const HEAP_ORDER: usize = 32;
pub const HEAP_SIZE_CLASS_COUNT: usize = 12;
const BOOT_UNINITIALIZED: u8 = 0;
const BOOT_INITIALIZING: u8 = 1;
const BOOT_READY: u8 = 2;
const WATCHDOG_CHECK_DIVISOR: u32 = 4;
#[cfg(helios_watchdog_self_test)]
const WATCHDOG_SELF_TEST_DELAY_MILLIS_ENV: &str = env!("HELIOS_WATCHDOG_SELF_TEST_DELAY_MS");

#[cfg_attr(target_os = "none", global_allocator)]
static ALLOCATOR: KernelAllocator<HEAP_ORDER> = KernelAllocator::empty();
static BOOT_STATE: AtomicU8 = AtomicU8::new(BOOT_UNINITIALIZED);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapStats {
    pub total_bytes: usize,
    pub allocated_bytes: usize,
    pub requested_live_bytes: usize,
    pub allocation_count: u64,
    pub deallocation_count: u64,
    pub reallocation_count: u64,
    pub total_allocation_bytes: u64,
    pub total_deallocation_bytes: u64,
    pub total_reallocation_bytes: u64,
    pub size_class_allocation_count: [u64; HEAP_SIZE_CLASS_COUNT],
    pub size_class_deallocation_count: [u64; HEAP_SIZE_CLASS_COUNT],
    pub size_class_reallocation_count: [u64; HEAP_SIZE_CLASS_COUNT],
    pub size_class_allocation_bytes: [u64; HEAP_SIZE_CLASS_COUNT],
    pub size_class_deallocation_bytes: [u64; HEAP_SIZE_CLASS_COUNT],
    pub size_class_reallocation_bytes: [u64; HEAP_SIZE_CLASS_COUNT],
}

impl HeapStats {
    pub fn available_bytes(self) -> usize {
        self.total_bytes.saturating_sub(self.allocated_bytes)
    }
}

struct KernelAllocator<const ORDER: usize> {
    heap: LockedHeap<ORDER>,
    stats: KernelAllocationStats,
}

impl<const ORDER: usize> KernelAllocator<ORDER> {
    const fn empty() -> Self {
        Self {
            heap: LockedHeap::empty(),
            stats: KernelAllocationStats::new(),
        }
    }

    unsafe fn add_to_heap(&self, start: usize, end: usize) {
        unsafe {
            self.heap.lock().add_to_heap(start, end);
        }
    }

    fn stats(&self) -> HeapStats {
        let allocator = self.heap.lock();
        HeapStats {
            total_bytes: allocator.stats_total_bytes(),
            allocated_bytes: allocator.stats_alloc_actual(),
            requested_live_bytes: self.stats.requested_live_bytes.load(Ordering::Relaxed),
            allocation_count: self.stats.allocation_count.load(Ordering::Relaxed),
            deallocation_count: self.stats.deallocation_count.load(Ordering::Relaxed),
            reallocation_count: self.stats.reallocation_count.load(Ordering::Relaxed),
            total_allocation_bytes: self.stats.total_allocation_bytes.load(Ordering::Relaxed),
            total_deallocation_bytes: self.stats.total_deallocation_bytes.load(Ordering::Relaxed),
            total_reallocation_bytes: self.stats.total_reallocation_bytes.load(Ordering::Relaxed),
            size_class_allocation_count: self
                .stats
                .size_class_counts(&self.stats.size_class_allocation_count),
            size_class_deallocation_count: self
                .stats
                .size_class_counts(&self.stats.size_class_deallocation_count),
            size_class_reallocation_count: self
                .stats
                .size_class_counts(&self.stats.size_class_reallocation_count),
            size_class_allocation_bytes: self
                .stats
                .size_class_counts(&self.stats.size_class_allocation_bytes),
            size_class_deallocation_bytes: self
                .stats
                .size_class_counts(&self.stats.size_class_deallocation_bytes),
            size_class_reallocation_bytes: self
                .stats
                .size_class_counts(&self.stats.size_class_reallocation_bytes),
        }
    }

    fn set_size_class_metrics_enabled(&self, enabled: bool) {
        self.stats.set_size_class_metrics_enabled(enabled);
    }
}

unsafe impl<const ORDER: usize> GlobalAlloc for KernelAllocator<ORDER> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { GlobalAlloc::alloc(&self.heap, layout) };
        if !ptr.is_null() {
            self.stats.record_alloc(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { GlobalAlloc::alloc_zeroed(&self.heap, layout) };
        if !ptr.is_null() {
            self.stats.record_alloc(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            GlobalAlloc::dealloc(&self.heap, ptr, layout);
        }
        self.stats.record_dealloc(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let new_ptr = unsafe { GlobalAlloc::alloc(&self.heap, new_layout) };
        if new_ptr.is_null() {
            return ptr::null_mut();
        }

        unsafe {
            ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
            GlobalAlloc::dealloc(&self.heap, ptr, layout);
        }
        self.stats.record_realloc(layout.size(), new_size);
        new_ptr
    }
}

struct KernelAllocationStats {
    requested_live_bytes: AtomicUsize,
    allocation_count: AtomicU64,
    deallocation_count: AtomicU64,
    reallocation_count: AtomicU64,
    total_allocation_bytes: AtomicU64,
    total_deallocation_bytes: AtomicU64,
    total_reallocation_bytes: AtomicU64,
    size_class_metrics_enabled: AtomicBool,
    size_class_allocation_count: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_deallocation_count: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_reallocation_count: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_allocation_bytes: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_deallocation_bytes: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
    size_class_reallocation_bytes: [AtomicU64; HEAP_SIZE_CLASS_COUNT],
}

impl KernelAllocationStats {
    const fn new() -> Self {
        Self {
            requested_live_bytes: AtomicUsize::new(0),
            allocation_count: AtomicU64::new(0),
            deallocation_count: AtomicU64::new(0),
            reallocation_count: AtomicU64::new(0),
            total_allocation_bytes: AtomicU64::new(0),
            total_deallocation_bytes: AtomicU64::new(0),
            total_reallocation_bytes: AtomicU64::new(0),
            size_class_metrics_enabled: AtomicBool::new(false),
            size_class_allocation_count: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_deallocation_count: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_reallocation_count: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_allocation_bytes: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_deallocation_bytes: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
            size_class_reallocation_bytes: [const { AtomicU64::new(0) }; HEAP_SIZE_CLASS_COUNT],
        }
    }

    fn size_class_counts(
        &self,
        values: &[AtomicU64; HEAP_SIZE_CLASS_COUNT],
    ) -> [u64; HEAP_SIZE_CLASS_COUNT] {
        core::array::from_fn(|index| values[index].load(Ordering::Relaxed))
    }

    fn record_alloc(&self, size: usize) {
        let size_u64 = usize_to_u64(size, "kernel allocation size");
        self.allocation_count.fetch_add(1, Ordering::Relaxed);
        self.requested_live_bytes.fetch_add(size, Ordering::Relaxed);
        self.total_allocation_bytes
            .fetch_add(size_u64, Ordering::Relaxed);
        if self.size_class_metrics_enabled.load(Ordering::Relaxed) {
            let class = heap_size_class(size);
            self.size_class_allocation_count[class].fetch_add(1, Ordering::Relaxed);
            self.size_class_allocation_bytes[class].fetch_add(size_u64, Ordering::Relaxed);
        }
    }

    fn record_dealloc(&self, size: usize) {
        let size_u64 = usize_to_u64(size, "kernel deallocation size");
        self.deallocation_count.fetch_add(1, Ordering::Relaxed);
        self.requested_live_bytes.fetch_sub(size, Ordering::Relaxed);
        self.total_deallocation_bytes
            .fetch_add(size_u64, Ordering::Relaxed);
        if self.size_class_metrics_enabled.load(Ordering::Relaxed) {
            let class = heap_size_class(size);
            self.size_class_deallocation_count[class].fetch_add(1, Ordering::Relaxed);
            self.size_class_deallocation_bytes[class].fetch_add(size_u64, Ordering::Relaxed);
        }
    }

    fn record_realloc(&self, old_size: usize, new_size: usize) {
        let new_size_u64 = usize_to_u64(new_size, "kernel reallocation size");
        self.reallocation_count.fetch_add(1, Ordering::Relaxed);
        if new_size >= old_size {
            self.requested_live_bytes
                .fetch_add(new_size - old_size, Ordering::Relaxed);
        } else {
            self.requested_live_bytes
                .fetch_sub(old_size - new_size, Ordering::Relaxed);
        }
        self.total_reallocation_bytes
            .fetch_add(new_size_u64, Ordering::Relaxed);
        if self.size_class_metrics_enabled.load(Ordering::Relaxed) {
            let class = heap_size_class(new_size);
            self.size_class_reallocation_count[class].fetch_add(1, Ordering::Relaxed);
            self.size_class_reallocation_bytes[class].fetch_add(new_size_u64, Ordering::Relaxed);
        }
    }

    fn set_size_class_metrics_enabled(&self, enabled: bool) {
        self.size_class_metrics_enabled
            .store(enabled, Ordering::Release);
    }
}

fn heap_size_class(size: usize) -> usize {
    match size {
        0..=8 => 0,
        9..=16 => 1,
        17..=32 => 2,
        33..=64 => 3,
        65..=128 => 4,
        129..=256 => 5,
        257..=512 => 6,
        513..=1024 => 7,
        1025..=4096 => 8,
        4097..=16_384 => 9,
        16_385..=65_536 => 10,
        _ => 11,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KernelRunStats {
    pub timer_fired_count: usize,
    pub executor_local_runnable_count: usize,
    pub executor_global_runnable_count: usize,
    pub executor_local_empty_pop_count: usize,
    pub executor_global_empty_pop_count: usize,
}

impl KernelRunStats {
    pub const fn executor_runnable_count(self) -> usize {
        self.executor_local_runnable_count + self.executor_global_runnable_count
    }

    pub const fn progress_count(self) -> usize {
        self.timer_fired_count + self.executor_runnable_count()
    }
}

/// What the kernel heap keeps for itself, as a fraction of the memory
/// it is sizing against, and the floor under that fraction.
///
/// Two things use this: the growth path, which refuses to let user
/// memory eat into the kernel's reserve, and the bootstrap split below,
/// which decides how much of each memory region the kernel heap keeps
/// before the user pool takes the rest. One number for both is the
/// point — the reserve the kernel defends at run time is the reserve it
/// was given at boot.
const USER_MEMORY_KERNEL_RESERVE_FRACTION: usize = 4;
const USER_MEMORY_MIN_KERNEL_RESERVE_BYTES: usize = 32 * 1024 * 1024;
const USER_HEAP_MIN_REGION_BYTES: usize = 2 * 1024 * 1024;

pub struct Kernel<CpuImpl: Cpu + Clone, WatchdogImpl: Watchdog + Clone = NoWatchdog> {
    cpu: CpuImpl,
    executor: Executor,
    timer: Timer<CpuImpl>,
    watchdog: WatchdogImpl,
    topology: ProcessorTopology,
    dma_model: DmaModel,
    devices: DeviceInventory,
}

impl<CpuImpl: Cpu + Clone, WatchdogImpl: Watchdog + Clone> Kernel<CpuImpl, WatchdogImpl> {
    pub fn spawner(&self) -> Spawner<CpuImpl> {
        self.executor.spawner(self.cpu.clone())
    }

    pub fn timer(&self) -> Timer<CpuImpl> {
        self.timer.clone()
    }

    pub fn topology(&self) -> ProcessorTopology {
        self.topology
    }

    pub fn dma_model(&self) -> DmaModel {
        self.dma_model
    }

    pub fn devices(&self) -> DeviceInventory {
        self.devices
    }

    pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: core::future::Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.spawner().spawn(future)
    }

    pub fn spawn_detached<Fut>(&self, future: Fut)
    where
        Fut: core::future::Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.spawner().spawn_detached(future);
    }

    pub fn spawn_local<Fut>(&self, future: Fut) -> LocalJoinHandle<Fut::Output>
    where
        Fut: core::future::Future + 'static,
        Fut::Output: 'static,
    {
        self.spawner().spawn_local(future)
    }

    pub fn spawn_local_detached<Fut>(&self, future: Fut)
    where
        Fut: core::future::Future + 'static,
        Fut::Output: 'static,
    {
        self.spawner().spawn_local_detached(future);
    }

    pub fn sleep_until(&self, deadline: Instant) -> Sleep<CpuImpl> {
        self.timer.sleep_until(deadline)
    }

    pub fn sleep_for(&self, duration: Duration) -> Sleep<CpuImpl> {
        self.timer.sleep_for(duration)
    }

    pub fn run_until_stalled(&self) -> usize {
        self.run_until_stalled_with_stats().progress_count()
    }

    pub fn run_until_stalled_with_stats(&self) -> KernelRunStats {
        let mut progress = 0;
        let mut stats = KernelRunStats::default();

        loop {
            let fired = self.timer.fire_expired();
            let executor_stats = self.executor.run_until_stalled_with_stats();
            let ran = executor_stats.runnable_count();

            stats.timer_fired_count += fired;
            stats.executor_local_runnable_count += executor_stats.local_runnable_count();
            stats.executor_global_runnable_count += executor_stats.global_runnable_count();
            stats.executor_local_empty_pop_count += executor_stats.local_empty_pop_count();
            stats.executor_global_empty_pop_count += executor_stats.global_empty_pop_count();

            if fired == 0 && ran == 0 {
                return stats;
            }

            progress += fired + ran;
            if ran == exec::READY_BATCH_TASKS || progress >= exec::READY_BATCH_TASKS {
                return stats;
            }
        }
    }

    pub fn run(&self) -> ! {
        loop {
            if self.run_until_stalled() == 0 {
                self.cpu.park_current();
            }
        }
    }

    pub fn run_local_future<Fut>(&self, future: Fut) -> Fut::Output
    where
        Fut: Future + 'static,
        Fut::Output: 'static,
    {
        let parker = Arc::new(LocalFutureParker::new(self.cpu.clone()));
        let waker = Waker::from(parker.clone());
        let mut context = Context::from_waker(&waker);
        let mut task = core::pin::pin!(self.spawn_local(future));

        loop {
            parker.clear();
            match task.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => {
                    if self.run_until_stalled() == 0 {
                        parker.park();
                    }
                }
            }
        }
    }

    fn spawn_watchdog_supervisor(&self) {
        if !self.watchdog.is_enabled() {
            return;
        }

        let timeout = self.watchdog.timeout();
        assert!(
            timeout > Duration::ZERO,
            "enabled watchdog reported a zero timeout"
        );
        let interval = timeout
            .checked_div(WATCHDOG_CHECK_DIVISOR)
            .unwrap_or_else(|| panic!("watchdog timeout {timeout:?} is too short"));
        assert!(
            interval > Duration::ZERO,
            "watchdog check interval computed as zero for timeout {timeout:?}"
        );

        let min_pet_ticks = duration_to_ticks(interval, self.cpu.timer_frequency());
        assert!(
            min_pet_ticks != 0,
            "watchdog pet interval {interval:?} converted to zero timer ticks"
        );

        let cpu = self.cpu.clone();
        let watchdog = self.watchdog.clone();
        let progress_notify = self.spawner().progress_notify();
        if self.cpu.current_processor() == self.topology.bootstrap_processor {
            watchdog.arm();
        }
        self.spawner().spawn_local_detached_silent(async move {
            let mut last_pet_at = cpu.now();
            loop {
                progress_notify.notified().await;
                let now = cpu.now();
                if now.ticks().saturating_sub(last_pet_at.ticks()) < min_pet_ticks {
                    continue;
                }
                watchdog.pet();
                last_pet_at = now;
            }
        });
    }

    #[cfg(helios_watchdog_self_test)]
    fn spawn_watchdog_self_test(&self) {
        if !self.watchdog.is_enabled() {
            return;
        }

        let timer = self.timer();
        let processor = self.cpu.current_processor().id();
        let delay = watchdog_self_test_delay();
        self.spawner().spawn_local_detached_silent(async move {
            timer.sleep_for(delay).await;
            tracing::error!(
                processor,
                delay_ms = delay.as_millis() as u64,
                "watchdog self-test hanging processor"
            );
            loop {
                core::hint::spin_loop();
            }
        });
    }
}

#[cfg(helios_watchdog_self_test)]
fn watchdog_self_test_delay() -> Duration {
    let millis = WATCHDOG_SELF_TEST_DELAY_MILLIS_ENV
        .parse::<u64>()
        .unwrap_or_else(|error| {
            panic!(
                "invalid HELIOS_WATCHDOG_SELF_TEST_DELAY_MS={WATCHDOG_SELF_TEST_DELAY_MILLIS_ENV:?}: {error}"
            )
        });
    assert!(
        millis != 0,
        "HELIOS_WATCHDOG_SELF_TEST_DELAY_MS must be non-zero"
    );
    Duration::from_millis(millis)
}

struct LocalFutureParker<CpuImpl: Cpu + Clone> {
    cpu: CpuImpl,
    owner_processor: ProcessorId,
    notified: AtomicBool,
}

impl<CpuImpl: Cpu + Clone> LocalFutureParker<CpuImpl> {
    fn new(cpu: CpuImpl) -> Self {
        let owner_processor = cpu.current_processor();
        Self {
            cpu,
            owner_processor,
            notified: AtomicBool::new(false),
        }
    }

    fn clear(&self) {
        self.notified.store(false, Ordering::Release);
    }

    fn park(&self) {
        if self.notified.swap(false, Ordering::AcqRel) {
            return;
        }
        self.cpu.park_current();
    }
}

impl<CpuImpl: Cpu + Clone> Wake for LocalFutureParker<CpuImpl> {
    fn wake(self: Arc<Self>) {
        self.notified.store(true, Ordering::Release);
        if self.cpu.current_processor() != self.owner_processor {
            self.cpu.wake_processor(self.owner_processor);
        }
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.notified.store(true, Ordering::Release);
        if self.cpu.current_processor() != self.owner_processor {
            self.cpu.wake_processor(self.owner_processor);
        }
    }
}

pub fn init<Console, CpuImpl, Regions>(
    platform: Platform<Console, CpuImpl, Regions>,
) -> Kernel<CpuImpl>
where
    Console: core::fmt::Write + Send + 'static,
    CpuImpl: Cpu + Clone,
    Regions: IntoIterator<Item = MemoryRegion>,
{
    init_with_watchdog(platform)
}

pub fn init_with_watchdog<Console, CpuImpl, Regions, WatchdogImpl>(
    platform: Platform<Console, CpuImpl, Regions, WatchdogImpl>,
) -> Kernel<CpuImpl, WatchdogImpl>
where
    Console: core::fmt::Write + Send + 'static,
    CpuImpl: Cpu + Clone,
    Regions: IntoIterator<Item = MemoryRegion>,
    WatchdogImpl: Watchdog + Clone,
{
    let Platform {
        console,
        cpu,
        memory_regions,
        watchdog,
        topology,
        timer_frequency_hz,
        dma_model,
        devices,
    } = platform;
    let current_processor = cpu.current_processor();
    assert!(
        cpu.bootstrap_processor() == topology.bootstrap_processor,
        "platform topology bootstrap processor {} does not match CPU bootstrap processor {}",
        topology.bootstrap_processor.id(),
        cpu.bootstrap_processor().id()
    );
    assert!(
        cpu.processor_count() == topology.configured_processors,
        "platform topology processor count {} does not match CPU processor count {}",
        topology.configured_processors,
        cpu.processor_count()
    );
    assert!(
        cpu.timer_frequency() == timer_frequency_hz,
        "platform timer frequency {} does not match CPU timer frequency {}",
        timer_frequency_hz,
        cpu.timer_frequency()
    );

    if current_processor == topology.bootstrap_processor {
        match BOOT_STATE.load(Ordering::Acquire) {
            BOOT_UNINITIALIZED => {
                bootstrap_init(console, memory_regions, &cpu, topology, dma_model, devices)
            }
            BOOT_INITIALIZING => finish_bootstrap(console, &cpu, topology, dma_model, devices),
            state => panic!("bootstrap processor observed invalid boot state {state}"),
        }
    } else {
        wait_for_bootstrap(&cpu);
    }

    let progress = if watchdog.is_enabled() {
        watchdog.progress()
    } else {
        ProgressCounter::new()
    };
    let kernel = Kernel {
        timer: Timer::new(cpu.clone()),
        cpu,
        executor: Executor::new(progress, topology.configured_processors, current_processor),
        watchdog,
        topology,
        dma_model,
        devices,
    };
    #[cfg(helios_watchdog_self_test)]
    assert!(
        kernel.watchdog.is_enabled(),
        "watchdog self-test requires an enabled hardware watchdog"
    );

    let processor_id = current_processor.id();
    kernel.spawn_detached(async move {
        tracing::info!("Processor online processor={processor_id}");
    });
    kernel.spawn_watchdog_supervisor();
    #[cfg(helios_watchdog_self_test)]
    kernel.spawn_watchdog_self_test();

    kernel
}

fn init_allocator<Regions>(
    memory_regions: Regions,
    processor_count: usize,
) -> &'static memory::UserMemoryPool
where
    Regions: IntoIterator<Item = MemoryRegion>,
{
    let mut user_pool = None;
    for mut region in memory_regions {
        let region = unsafe { region.as_mut() };
        let start = region.as_mut_ptr() as usize;
        let end = start + region.len();
        let (kernel_end, user_start) = split_bootstrap_memory_region(start, end);
        unsafe {
            ALLOCATOR.add_to_heap(start, kernel_end);
        }
        let pool = *user_pool.get_or_insert_with(|| {
            let pool = memory::install_user_memory_pool(memory::allocate_user_memory_pool());
            pool.configure_processors(processor_count);
            // The swap policy asks which instance a committed page
            // belongs to, and the answer is per-processor; size that
            // table with the pool it describes.
            memory::configure_user_memory_owner_processors(processor_count);
            pool
        });
        if let Some(user_start) = user_start {
            pool.add_region(user_start, end);
        }
    }
    user_pool.unwrap_or_else(|| panic!("bootstrap did not provide memory for user pool"))
}

/// Splits one bootstrap memory region into the kernel heap's part and
/// the user pool's part, and reports where the user's part begins.
///
/// The kernel keeps its reserve and the user pool takes the rest. The
/// kernel is close to zero-allocation by contract — bounded pools, no
/// per-guest growth — and a kernel OOM is fatal rather than
/// recoverable, so what it needs is headroom over a small working set,
/// not a share proportional to the machine.
///
/// Halving every region got that wrong in a way that showed. On a 2 GiB
/// x86-64 machine the two halves came to 726 MiB of kernel heap and
/// 730 MiB of user pool, and at the point where the first guest program
/// is compiled the kernel was holding 39 MiB of its 726 — while the
/// user pool could not produce the single 512 MiB run a shared wasm
/// memory needs on a backend with no lazy-commit address space, because
/// it did not have one. The spawn failed with 95% of the kernel's half
/// untouched.
fn split_bootstrap_memory_region(start: usize, end: usize) -> (usize, Option<usize>) {
    let len = end.saturating_sub(start);
    let kernel_len = user_memory_kernel_reserve_bytes(len);
    let Some(user_len) = len.checked_sub(kernel_len) else {
        return (end, None);
    };
    if user_len < USER_HEAP_MIN_REGION_BYTES {
        return (end, None);
    }

    // Rounded up, away from the reserve: the kernel's share is a floor
    // it keeps, so the alignment comes out of the user pool's side.
    let user_start = align_up(start.saturating_add(kernel_len), USER_HEAP_MIN_REGION_BYTES);
    if user_start <= start || end.saturating_sub(user_start) < USER_HEAP_MIN_REGION_BYTES {
        return (end, None);
    }
    (user_start, Some(user_start))
}

/// `value` rounded up to a multiple of `align`, which must be a power
/// of two. Saturates rather than wrapping, so a caller that checks the
/// result against a region end rejects the overflow instead of
/// wrapping into it.
const fn align_up(value: usize, align: usize) -> usize {
    match value.checked_add(align - 1) {
        Some(sum) => sum & !(align - 1),
        None => usize::MAX,
    }
}

pub fn prime_bootstrap_allocator<Regions>(
    memory_regions: Regions,
    processor_count: usize,
) -> &'static memory::UserMemoryPool
where
    Regions: IntoIterator<Item = MemoryRegion>,
{
    match BOOT_STATE.compare_exchange(
        BOOT_UNINITIALIZED,
        BOOT_INITIALIZING,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => init_allocator(memory_regions, processor_count),
        Err(state) => panic!("bootstrap allocator observed invalid boot state {state}"),
    }
}

fn bootstrap_init<Console, CpuImpl, Regions>(
    console: Console,
    memory_regions: Regions,
    cpu: &CpuImpl,
    topology: ProcessorTopology,
    dma_model: DmaModel,
    devices: DeviceInventory,
) where
    Console: core::fmt::Write + Send + 'static,
    CpuImpl: Cpu,
    Regions: IntoIterator<Item = MemoryRegion>,
{
    prime_bootstrap_allocator(memory_regions, topology.configured_processors);
    finish_bootstrap(console, cpu, topology, dma_model, devices);
}

fn finish_bootstrap<Console, CpuImpl>(
    console: Console,
    cpu: &CpuImpl,
    topology: ProcessorTopology,
    dma_model: DmaModel,
    devices: DeviceInventory,
) where
    Console: core::fmt::Write + Send + 'static,
    CpuImpl: Cpu,
{
    log::init_logger(console);
    tracing::info!(
        "Kernel initialized on bootstrap processor={}",
        topology.bootstrap_processor.id()
    );
    tracing::info!(
        "Kernel topology processors={} startup_policy={:?}",
        topology.configured_processors,
        topology.startup_policy
    );
    let user_heap = memory::user_heap_stats();
    tracing::info!(
        "User memory pool total_bytes={} available_bytes={}",
        user_heap.total_bytes,
        user_heap.available_bytes()
    );
    tracing::info!(
        "Platform dma_model={dma_model:?} debug_serial={} network={} block_devices={} \
         host_share={} entropy_device={}",
        devices.has_debug_serial,
        devices.has_network,
        devices.block_device_count,
        devices.has_host_share,
        devices.has_entropy_device
    );
    tracing::info!("Kernel is ready\n\n{}", include_str!("welcome.txt"));

    BOOT_STATE.store(BOOT_READY, Ordering::Release);

    if topology.startup_policy == ProcessorStartupPolicy::StartAllSecondaries {
        for processor in 0..topology.configured_processors {
            let processor = ProcessorId::new(processor as u16);
            if processor != topology.bootstrap_processor {
                cpu.start_processor(processor);
            }
        }
    }
}

fn wait_for_bootstrap<CpuImpl: Cpu>(cpu: &CpuImpl) {
    loop {
        if BOOT_STATE.load(Ordering::Acquire) == BOOT_READY {
            return;
        }
        cpu.park_current();
    }
}

pub fn panic_log(info: &core::panic::PanicInfo) {
    panic_log_message(info.message(), info.location());
}

pub fn panic_log_message(
    message: impl core::fmt::Display,
    location: Option<&core::panic::Location<'_>>,
) {
    if let Some(location) = location {
        tracing::error!(
            "Kernel panic: {} ({}:{}:{})",
            message,
            location.file(),
            location.line(),
            location.column()
        );
        return;
    }

    tracing::error!("Kernel panic: {}", message);
}

pub fn heap_stats() -> HeapStats {
    ALLOCATOR.stats()
}

pub fn set_kernel_heap_size_class_metrics_enabled(enabled: bool) {
    ALLOCATOR.set_size_class_metrics_enabled(enabled);
}

fn usize_to_u64(value: usize, label: &'static str) -> u64 {
    u64::try_from(value).unwrap_or_else(|_| panic!("{label} does not fit into u64"))
}

pub fn user_memory_kernel_reserve_bytes(total_heap_bytes: usize) -> usize {
    (total_heap_bytes / USER_MEMORY_KERNEL_RESERVE_FRACTION)
        .max(USER_MEMORY_MIN_KERNEL_RESERVE_BYTES)
        .min(total_heap_bytes)
}

#[cfg(target_os = "none")]
#[alloc_error_handler]
fn kernel_alloc_error(layout: core::alloc::Layout) -> ! {
    panic!(
        "kernel allocator exhausted: requested size={} align={}",
        layout.size(),
        layout.align()
    )
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use core::alloc::{GlobalAlloc, Layout};

    use super::*;

    const TEST_HEAP_BYTES: usize = 16 * 1024;

    /// The compiler plugin's shared memory on a backend with no
    /// lazy-commit address space: half a gigabyte, allocated at its
    /// declared maximum in one piece because a shared memory can never
    /// move.
    const PLUGIN_SHARED_MEMORY_BYTES: usize = 512 * 1024 * 1024;

    #[test]
    fn the_kernel_keeps_its_reserve_and_the_user_pool_takes_the_rest() {
        // What the two halves of a 2 GiB x86-64 machine came to once
        // firmware had taken its share.
        let start = 0x0010_0000;
        let end = start + 1456 * 1024 * 1024;
        let (kernel_end, user_start) = split_bootstrap_memory_region(start, end);
        let user_start = user_start.expect("a gigabyte and a half is worth splitting");
        assert_eq!(kernel_end, user_start, "the split is one point");

        let kernel_len = kernel_end - start;
        let user_len = end - user_start;
        assert!(
            kernel_len >= USER_MEMORY_MIN_KERNEL_RESERVE_BYTES,
            "the kernel heap fell below its own floor at {kernel_len} bytes"
        );
        assert!(
            user_len > PLUGIN_SHARED_MEMORY_BYTES,
            "a user pool of {user_len} bytes cannot hold one              {PLUGIN_SHARED_MEMORY_BYTES}-byte allocation and anything else"
        );
    }

    #[test]
    fn a_region_that_cannot_spare_the_reserve_stays_with_the_kernel() {
        let start = 0x4000_0000;
        for len in [0_usize, 4096, USER_MEMORY_MIN_KERNEL_RESERVE_BYTES] {
            let end = start + len;
            assert_eq!(
                split_bootstrap_memory_region(start, end),
                (end, None),
                "a {len}-byte region has nothing left after the kernel reserve"
            );
        }
    }

    #[test]
    fn the_user_share_never_reaches_past_the_region_it_came_from() {
        for offset in [0_usize, 4096, 2 * 1024 * 1024, 37 * 1024 * 1024] {
            for len in [
                8 * 1024 * 1024,
                100 * 1024 * 1024,
                1024 * 1024 * 1024,
                3 * 1024 * 1024 * 1024,
            ] {
                let start = 0x4000_0000 + offset;
                let end = start + len;
                let (kernel_end, user_start) = split_bootstrap_memory_region(start, end);
                assert!(kernel_end >= start && kernel_end <= end);
                if let Some(user_start) = user_start {
                    assert_eq!(user_start, kernel_end);
                    assert!(user_start > start && user_start < end);
                    assert!(
                        user_start - start >= USER_MEMORY_MIN_KERNEL_RESERVE_BYTES,
                        "the kernel heap kept less than its floor"
                    );
                }
            }
        }
    }

    #[repr(align(4096))]
    struct AlignedHeap([u8; TEST_HEAP_BYTES]);

    #[test]
    fn kernel_allocator_tracks_requested_allocation_pressure() {
        let allocator = KernelAllocator::<HEAP_ORDER>::empty();
        let mut heap = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
        let start = heap.0.as_mut_ptr() as usize;
        unsafe {
            allocator.add_to_heap(start, start + TEST_HEAP_BYTES);
        }

        let layout = Layout::from_size_align(64, 8).expect("valid allocation layout");
        let ptr = unsafe { GlobalAlloc::alloc(&allocator, layout) };
        assert!(!ptr.is_null());

        let reallocated = unsafe { GlobalAlloc::realloc(&allocator, ptr, layout, 128) };
        assert!(!reallocated.is_null());

        let grown_layout = Layout::from_size_align(128, 8).expect("valid grown layout");
        unsafe {
            GlobalAlloc::dealloc(&allocator, reallocated, grown_layout);
        }

        let stats = allocator.stats();
        assert_eq!(stats.allocation_count, 1);
        assert_eq!(stats.reallocation_count, 1);
        assert_eq!(stats.deallocation_count, 1);
        assert_eq!(stats.total_allocation_bytes, 64);
        assert_eq!(stats.total_reallocation_bytes, 128);
        assert_eq!(stats.total_deallocation_bytes, 128);
        assert_eq!(stats.requested_live_bytes, 0);
    }
}
