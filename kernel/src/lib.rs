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
    COMPONENT_ASYNC_STACK_SIZE, CompiledComponent, ComponentExecContext, ComponentExecutor,
    ComponentExitStatus, ComponentFsNodeKind, ComponentFsPathError, ComponentFsResourceError,
    ComponentOutputMode, ComponentOutputRoute, ComponentOutputSink, ComponentOutputStreamKind,
    ComponentRawMutex, ComponentRawMutexGuard, ComponentRawRwLock, ComponentRawRwLockReadGuard,
    ComponentRawRwLockWriteGuard, ComponentResourceTableError, ComponentRunResult,
    ComponentRuntimeEngine, ComponentRuntimeFactory, ComponentRuntimeState, ComponentSerialPort,
    ComponentStoreData, ComponentTcpBackend, ComponentTcpStream, ComponentUdpBackend,
    ComponentUdpSocket, ComponentWorld, DeadlinePollable, InstanceKilled, LocalOutputSink,
    ProviderAlreadyInstalled, ProviderError, ProviderReceiver, ProviderSender, ProviderSlot,
    RawMutexGuardResource, RawMutexResource, RawRwLockReadGuardResource, RawRwLockResource,
    RawRwLockWriteGuardResource, SerialPortResource, TcpStreamResource, UdpSocketResource,
    directory_prefix, map_resource_table_error, parent_path, path_is_within_directory,
    provider_channel, resolve_absolute_path, resolve_child_path, resolve_guest_path,
    store_kernel_heap_bytes, strip_directory_prefix, wait_until_runtime_deadline,
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
    InterfaceEventMark, LinkState, NetworkInterface as NetworkDevice, PacketBuffer, RxFrame,
    RxFrameOffload, SegmentationOffload, TxFrameRef,
};
pub use host_fs::{
    HOST_SHARE_GUEST_MOUNT_PATH, HOST_SHARE_MOUNT_TAG, HostFsCacheStats, HostFsClient,
    HostFsTransport, UnsupportedHostFileSystem, guest_host_share_path,
};
pub use instance::{
    ActivityChange, ActivityStep, CondemnedMemory, InstanceActivity, InstanceExecutionTransition,
    InstanceId, InstanceProfileTotal, InstanceRegistry, InstanceSnapshot, KernelHeapCharge,
    KillReason, MemoryPool, OOM_RECLAIM_GRACE, OomKillDecision, OomKillOutcome, OomPolicy,
    OomVictim, RegisteredInstance, allow_instance_resource_growth,
};
pub use io::{
    BlockInstallError, BlockSelfCheckError, BlockService, BlockStats, ByteReadWait, ByteReader,
    ByteWriteWait, ByteWriter, ClosedPeer, DebugConsole, DebugSerialAccess, DebugSerialWriter,
    ExternalInterruptHandler, ExternalInterruptRoutes, IommuDomains, IommuEndpointStats,
    IommuReport, IommuStats, MAX_BLOCK_DEVICES, MAX_IOMMU_ENDPOINTS, MAX_NETWORK_INTERRUPTS,
    PanicSerial, PollKey, PollRegistration, PollRegistry, PollRegistryError, PollSourceKind,
    RecordingConsole, SCRATCH_DISK_SERIAL, SerialReader, TryRead, TryWrite, byte_channel,
    emit_panic_report, install_block_devices, read_debug_serial, read_serial, try_read_serial,
    wake_queue_owners,
};
pub use kernel_exception::{
    KernelException, KernelExceptionCause, KernelExceptionDispatch, KernelNativeTrapHandler,
};
pub use memory::{
    AccessibilityPlan, BalloonHandle, BalloonStats, BootMemoryPlan, BootRegionSplitter,
    CommittedRegion, ENTROPY_RESEED_INTERVAL, EntropyPool, EntropySources,
    FREE_PAGE_REPORT_INTERVAL, HardwareEntropySource, IDLE_SWAP_AFTER, KERNEL_HEAP_BOOTSTRAP_BYTES,
    KERNEL_HEAP_GROWTH_CHUNK_BYTES, KERNEL_HEAP_MAX_BOOT_FRACTION, KERNEL_HEAP_MIN_RESERVE_BYTES,
    KERNEL_HEAP_RESERVE_FRACTION, KernelPhysFrameAllocator, MemoryOwner, NoCryptographicEntropy,
    NoEntropyDevice, ROOT_ENTROPY_MATERIAL_BYTES, RegionShares, ReleasedReservation,
    ReservationLookup, ReservationTracker, RootEntropy, RootEntropyHandle, SWAP_BATCH_BYTES,
    SWAP_TICK, SwapDisabled, SwapEntry, SwapFaultError, SwapHandle, SwapStats, SwapVmHooks,
    TASK_ARENA_FRACTION, TASK_ARENA_MIN_BYTES, USER_POOL_MIN_REGION_BYTES, UserHeapStats,
    UserMemoryOwnerScope, UserMemoryOwners, UserMemoryPool, VaCursor,
    allocate_user_frame_uninit_on, allocate_user_frame_zeroed, allocate_user_frame_zeroed_on,
    configure_user_memory_owner_processors, current_user_memory_owner, deallocate_user_frame,
    deallocate_user_frame_on, disable_swap, enter_user_memory_owner, install_entropy_device,
    install_memory_balloon, install_swap, install_swap_hooks, installed_swap_handle,
    installed_swap_hooks, kernel_reserve_for, largest_servable_user_bytes, seed_root_entropy,
    set_user_memory_owner, swapped_token, task_arena_bytes_for, user_heap_stats,
    user_mapping_kernel_heap_bytes, validate_range,
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
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};
use core::time::Duration;

use arrayvec::ArrayVec;
use buddy_system_allocator::Heap;
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::memory::MemoryRegion;
use helios_hal::watchdog::{NoWatchdog, ProgressCounter, Watchdog};
use helios_hal::{DeviceInventory, DmaModel, ProcessorStartupPolicy, ProcessorTopology};
use spin::Mutex as SpinMutex;

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

/// The kernel heap and everything counted about it, behind one lock.
///
/// The counters used to be global atomics beside the heap, which made
/// every allocation pay a lock acquisition *and* half a dozen
/// `fetch_add`s on lines every other processor was writing to as well.
/// They describe exactly the operations that already hold this lock, so
/// they live inside it: an allocation writes them from the critical
/// section that is already exclusive, and `stats()` reads a consistent
/// set of them under the same lock instead of a torn mix of atomics.
struct HeapState<const ORDER: usize> {
    heap: Heap<ORDER>,
    counters: AllocationCounters,
}

impl<const ORDER: usize> HeapState<ORDER> {
    const fn empty() -> Self {
        Self {
            heap: Heap::new(),
            counters: AllocationCounters::new(),
        }
    }

    /// Bytes the heap holds that it has not handed out.
    fn free_bytes(&self) -> usize {
        self.heap
            .stats_total_bytes()
            .saturating_sub(self.heap.stats_alloc_actual())
    }
}

struct KernelAllocator<const ORDER: usize> {
    heap: SpinMutex<HeapState<ORDER>>,
    /// Every usable byte the boot memory map described, and the free
    /// kernel heap a user grow may not dip into. Both are fixed by
    /// [`memory::BootMemoryPlan`] at boot and never move afterwards:
    /// the heap's own size is demand-driven, so a reserve defined
    /// against it would be a floor that moved with the thing it is
    /// supposed to hold down.
    ///
    /// These two stay outside the lock on purpose: they are written
    /// once at boot and read by callers that have no allocation to make,
    /// so putting them under the heap lock would make a health check
    /// contend with the allocation path it is reporting on.
    machine_usable_bytes: AtomicUsize,
    kernel_reserve_bytes: AtomicUsize,
    /// Allocations still to serve before another reserve top-up is
    /// attempted; see [`Self::alloc_growing`].
    top_up_backoff: AtomicUsize,
}

impl<const ORDER: usize> KernelAllocator<ORDER> {
    const fn empty() -> Self {
        Self {
            heap: SpinMutex::new(HeapState::empty()),
            machine_usable_bytes: AtomicUsize::new(0),
            kernel_reserve_bytes: AtomicUsize::new(0),
            top_up_backoff: AtomicUsize::new(0),
        }
    }

    unsafe fn add_to_heap(&self, start: usize, end: usize) {
        unsafe {
            self.heap.lock().heap.add_to_heap(start, end);
        }
    }

    /// Records what the boot memory map came to and what the kernel
    /// keeps out of it.
    fn install_plan(&self, plan: memory::BootMemoryPlan) {
        self.machine_usable_bytes
            .store(plan.usable_bytes, Ordering::Release);
        self.kernel_reserve_bytes
            .store(plan.kernel_reserve_bytes, Ordering::Release);
    }

    fn reserve_bytes(&self) -> usize {
        self.kernel_reserve_bytes.load(Ordering::Acquire)
    }

    fn machine_bytes(&self) -> usize {
        self.machine_usable_bytes.load(Ordering::Acquire)
    }

    /// One allocation attempt, the statistics it produces, and the free
    /// space the heap was left with — all in the same critical section.
    ///
    /// `record` runs only when the attempt succeeded, and it runs before
    /// the lock is dropped, so the counters it writes cost nothing
    /// beyond the lock the allocation already took. The free-space read
    /// is under the same lock as well, so the growth decision below is
    /// made against the state the allocation actually produced rather
    /// than a racing re-read.
    fn try_alloc(
        &self,
        layout: Layout,
        record: impl Fn(&mut AllocationCounters),
    ) -> (*mut u8, usize) {
        let mut state = self.heap.lock();
        let ptr = state
            .heap
            .alloc(layout)
            .map_or(ptr::null_mut(), core::ptr::NonNull::as_ptr);
        if !ptr.is_null() {
            record(&mut state.counters);
        }
        let free = state.free_bytes();
        (ptr, free)
    }

    /// Returns a block to the heap and records what that did to the
    /// statistics, in one critical section.
    ///
    /// `record` is what distinguishes a plain free from the trailing
    /// half of a reallocation: the latter counts as one reallocation
    /// rather than as an allocation and a deallocation.
    unsafe fn dealloc_recording(
        &self,
        ptr: *mut u8,
        layout: Layout,
        record: impl FnOnce(&mut AllocationCounters),
    ) {
        let mut state = self.heap.lock();
        // SAFETY: the caller guarantees `ptr` came from this heap under
        // `layout`, which the `GlobalAlloc` contract already requires.
        unsafe {
            state
                .heap
                .dealloc(core::ptr::NonNull::new_unchecked(ptr), layout);
        }
        record(&mut state.counters);
    }

    /// Serves `layout`, taking more memory out of the user pool when
    /// the heap cannot serve it or would be left under its reserve.
    ///
    /// The kernel heap owns only its boot share until this runs: see
    /// [`memory::policy`] for why the machine's memory starts in the
    /// user pool and comes here on demand, and why it never goes back.
    ///
    /// Two things ask for memory here, and they are not the same
    /// request. A failed allocation must be retried after a lend or the
    /// kernel dies, so it is never throttled. Topping the heap back up
    /// to its reserve is housekeeping, and it backs off when the pool
    /// has nothing to give: a lend costs a buddy allocation and a
    /// frame-slab drain, and retrying it on every kernel allocation
    /// would turn memory pressure into a throughput cliff exactly when
    /// throughput matters.
    ///
    /// Growth is attempted at most once per allocation. A pool that
    /// cannot serve one chunk cannot serve two, and a null return from
    /// here reaches `alloc_error_handler`, which panics — a kernel
    /// out-of-memory is fatal by contract, not something to spin on.
    ///
    /// `record` is applied by whichever of the two attempts succeeds,
    /// and by neither when both fail.
    fn alloc_growing(&self, layout: Layout, record: impl Fn(&mut AllocationCounters)) -> *mut u8 {
        let (ptr, free) = self.try_alloc(layout, &record);
        if !ptr.is_null() && free >= self.reserve_bytes() {
            return ptr;
        }
        if !ptr.is_null() && !self.top_up_is_due() {
            return ptr;
        }

        let wanted = layout
            .size()
            .max(layout.align())
            .next_power_of_two()
            .max(memory::KERNEL_HEAP_GROWTH_CHUNK_BYTES);
        match memory::lend_user_memory_to_kernel_heap(wanted) {
            Some((start, end)) => {
                unsafe {
                    self.add_to_heap(start, end);
                }
                self.top_up_backoff.store(0, Ordering::Relaxed);
            }
            None => self
                .top_up_backoff
                .store(KERNEL_HEAP_TOP_UP_BACKOFF, Ordering::Relaxed),
        }

        if ptr.is_null() {
            self.try_alloc(layout, &record).0
        } else {
            ptr
        }
    }

    /// Whether a reserve top-up should be attempted, counting down the
    /// backoff a failed one left behind.
    ///
    /// Relaxed and racy on purpose: this decides how often to retry
    /// housekeeping, and two processors landing on the same count costs
    /// one extra attempt.
    fn top_up_is_due(&self) -> bool {
        let remaining = self.top_up_backoff.load(Ordering::Relaxed);
        if remaining == 0 {
            return true;
        }
        self.top_up_backoff.store(remaining - 1, Ordering::Relaxed);
        false
    }

    fn stats(&self) -> HeapStats {
        let state = self.heap.lock();
        let counters = &state.counters;
        HeapStats {
            total_bytes: state.heap.stats_total_bytes(),
            allocated_bytes: state.heap.stats_alloc_actual(),
            requested_live_bytes: counters.requested_live_bytes,
            allocation_count: counters.allocation_count,
            deallocation_count: counters.deallocation_count,
            reallocation_count: counters.reallocation_count,
            total_allocation_bytes: counters.total_allocation_bytes,
            total_deallocation_bytes: counters.total_deallocation_bytes,
            total_reallocation_bytes: counters.total_reallocation_bytes,
            size_class_allocation_count: counters.size_class_allocation_count,
            size_class_deallocation_count: counters.size_class_deallocation_count,
            size_class_reallocation_count: counters.size_class_reallocation_count,
            size_class_allocation_bytes: counters.size_class_allocation_bytes,
            size_class_deallocation_bytes: counters.size_class_deallocation_bytes,
            size_class_reallocation_bytes: counters.size_class_reallocation_bytes,
        }
    }

    fn set_size_class_metrics_enabled(&self, enabled: bool) {
        self.heap.lock().counters.size_class_metrics_enabled = enabled;
    }
}

unsafe impl<const ORDER: usize> GlobalAlloc for KernelAllocator<ORDER> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        self.alloc_growing(layout, |counters| counters.record_alloc(size))
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let ptr = self.alloc_growing(layout, |counters| counters.record_alloc(size));
        if !ptr.is_null() {
            unsafe {
                ptr::write_bytes(ptr, 0, size);
            }
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = layout.size();
        unsafe {
            self.dealloc_recording(ptr, layout, |counters| counters.record_dealloc(size));
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        // The new block is not counted as an allocation and the old one
        // is not counted as a free: a reallocation is one operation in
        // the statistics, recorded when the old block goes back.
        let new_ptr = self.alloc_growing(new_layout, |_| {});
        if new_ptr.is_null() {
            return ptr::null_mut();
        }

        let old_size = layout.size();
        unsafe {
            ptr::copy_nonoverlapping(ptr, new_ptr, old_size.min(new_size));
            self.dealloc_recording(ptr, layout, |counters| {
                counters.record_realloc(old_size, new_size);
            });
        }
        new_ptr
    }
}

/// What the kernel heap has been asked for, counted while its lock is
/// held.
///
/// Plain integers rather than atomics: every write happens inside the
/// heap's critical section, and every read happens under the same lock,
/// so an atomic here would buy nothing but cross-processor line
/// traffic on the kernel's hottest path.
struct AllocationCounters {
    requested_live_bytes: usize,
    allocation_count: u64,
    deallocation_count: u64,
    reallocation_count: u64,
    total_allocation_bytes: u64,
    total_deallocation_bytes: u64,
    total_reallocation_bytes: u64,
    size_class_metrics_enabled: bool,
    size_class_allocation_count: [u64; HEAP_SIZE_CLASS_COUNT],
    size_class_deallocation_count: [u64; HEAP_SIZE_CLASS_COUNT],
    size_class_reallocation_count: [u64; HEAP_SIZE_CLASS_COUNT],
    size_class_allocation_bytes: [u64; HEAP_SIZE_CLASS_COUNT],
    size_class_deallocation_bytes: [u64; HEAP_SIZE_CLASS_COUNT],
    size_class_reallocation_bytes: [u64; HEAP_SIZE_CLASS_COUNT],
}

impl AllocationCounters {
    const fn new() -> Self {
        Self {
            requested_live_bytes: 0,
            allocation_count: 0,
            deallocation_count: 0,
            reallocation_count: 0,
            total_allocation_bytes: 0,
            total_deallocation_bytes: 0,
            total_reallocation_bytes: 0,
            size_class_metrics_enabled: false,
            size_class_allocation_count: [0; HEAP_SIZE_CLASS_COUNT],
            size_class_deallocation_count: [0; HEAP_SIZE_CLASS_COUNT],
            size_class_reallocation_count: [0; HEAP_SIZE_CLASS_COUNT],
            size_class_allocation_bytes: [0; HEAP_SIZE_CLASS_COUNT],
            size_class_deallocation_bytes: [0; HEAP_SIZE_CLASS_COUNT],
            size_class_reallocation_bytes: [0; HEAP_SIZE_CLASS_COUNT],
        }
    }

    fn record_alloc(&mut self, size: usize) {
        let size_u64 = usize_to_u64(size, "kernel allocation size");
        self.allocation_count += 1;
        self.requested_live_bytes += size;
        self.total_allocation_bytes += size_u64;
        if self.size_class_metrics_enabled {
            let class = heap_size_class(size);
            self.size_class_allocation_count[class] += 1;
            self.size_class_allocation_bytes[class] += size_u64;
        }
    }

    fn record_dealloc(&mut self, size: usize) {
        let size_u64 = usize_to_u64(size, "kernel deallocation size");
        self.deallocation_count += 1;
        self.requested_live_bytes = self.requested_live_bytes.saturating_sub(size);
        self.total_deallocation_bytes += size_u64;
        if self.size_class_metrics_enabled {
            let class = heap_size_class(size);
            self.size_class_deallocation_count[class] += 1;
            self.size_class_deallocation_bytes[class] += size_u64;
        }
    }

    fn record_realloc(&mut self, old_size: usize, new_size: usize) {
        let new_size_u64 = usize_to_u64(new_size, "kernel reallocation size");
        self.reallocation_count += 1;
        if new_size >= old_size {
            self.requested_live_bytes += new_size - old_size;
        } else {
            self.requested_live_bytes = self
                .requested_live_bytes
                .saturating_sub(old_size - new_size);
        }
        self.total_reallocation_bytes += new_size_u64;
        if self.size_class_metrics_enabled {
            let class = heap_size_class(new_size);
            self.size_class_reallocation_count[class] += 1;
            self.size_class_reallocation_bytes[class] += new_size_u64;
        }
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

/// The most usable regions a boot memory map may describe.
///
/// The map is walked twice — once to total it, once to divide it — and
/// the kernel cannot allocate a copy of it before the heap it is about
/// to build exists, so the copy lives on the stack. Limine publishes
/// around a dozen usable segments on the machines helios targets and
/// the riscv device tree fewer, so this is two kilobytes of boot stack
/// against a map an order of magnitude larger than any we have seen; a
/// map that still overruns it is a machine the kernel has not been told
/// about, and it says so rather than silently dropping the memory.
const MAX_BOOT_MEMORY_REGIONS: usize = 128;

/// Kernel allocations to serve before retrying a reserve top-up the
/// user pool has already refused once.
const KERNEL_HEAP_TOP_UP_BACKOFF: usize = 1024;

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

/// Divides the boot memory map between the kernel heap and the user
/// pool, and installs both.
///
/// The map is walked twice, because the policy is stated against the
/// machine and not against whichever region happens to come first: the
/// first pass totals the usable bytes, [`memory::BootMemoryPlan`] turns
/// that into the kernel's boot share and its reserve, and the second
/// pass hands the regions out. See [`memory::policy`] for the policy
/// itself and the evidence behind it.
fn init_allocator<Regions>(
    memory_regions: Regions,
    processor_count: usize,
) -> &'static memory::UserMemoryPool
where
    Regions: IntoIterator<Item = MemoryRegion>,
{
    let mut regions: ArrayVec<(usize, usize), MAX_BOOT_MEMORY_REGIONS> = ArrayVec::new();
    for mut region in memory_regions {
        let region = unsafe { region.as_mut() };
        let start = region.as_mut_ptr() as usize;
        regions
            .try_push((start, start + region.len()))
            .unwrap_or_else(|_| {
                panic!(
                    "boot memory map described more than {MAX_BOOT_MEMORY_REGIONS} usable regions"
                )
            });
    }

    let usable_bytes = regions
        .iter()
        .map(|(start, end)| end.saturating_sub(*start))
        .sum();
    let plan = memory::BootMemoryPlan::for_usable_bytes(usable_bytes);
    ALLOCATOR.install_plan(plan);
    let mut splitter = plan.splitter();

    let mut user_pool = None;
    for (start, end) in regions {
        let shares = splitter.split(start, end);
        if let Some(kernel) = shares.kernel {
            unsafe {
                ALLOCATOR.add_to_heap(kernel.start, kernel.end);
            }
        }
        // The pool is installed as soon as the kernel heap can allocate
        // it, and before the first user region is added: every later
        // region, and every byte the kernel heap later borrows back,
        // goes through it.
        let Some(user) = shares.user else {
            continue;
        };
        let pool = *user_pool.get_or_insert_with(|| {
            let pool = memory::install_user_memory_pool(memory::allocate_user_memory_pool());
            pool.configure_processors(processor_count);
            // The swap policy asks which instance a committed page
            // belongs to, and the answer is per-processor; size that
            // table with the pool it describes.
            memory::configure_user_memory_owner_processors(processor_count);
            pool
        });
        pool.add_region(user.start, user.end);
    }

    assert_eq!(
        splitter.kernel_owed_bytes(),
        0,
        "the boot memory map is smaller than the kernel heap's boot share of {} bytes",
        plan.kernel_boot_bytes
    );
    user_pool.unwrap_or_else(|| panic!("bootstrap did not provide memory for user pool"))
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
    let machine = machine_memory();
    tracing::info!(
        "User memory pool total_bytes={} available_bytes={}",
        user_heap.total_bytes,
        user_heap.available_bytes()
    );
    // The pool line above is the pool's own view. This one is the
    // policy: what the machine has, what the kernel heap started with
    // and what it will never give back, and what it takes at a time
    // when it needs more. `memory::policy` states why.
    tracing::info!(
        "Memory policy usable_bytes={} kernel_heap_bytes={} kernel_reserve_bytes={} \
         kernel_growth_chunk_bytes={} task_arena_bytes={}",
        machine.usable_bytes,
        heap_stats().total_bytes,
        kernel_heap_reserve_bytes(),
        memory::KERNEL_HEAP_GROWTH_CHUNK_BYTES,
        exec::task_arena_bytes(machine.usable_bytes)
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

/// Free kernel heap a user-memory grow may not dip into.
///
/// Derived from the boot memory map once and fixed for the life of the
/// kernel — see [`BootMemoryPlan`]. The kernel heap's own total
/// grows on demand, so a reserve expressed as a share of it would rise
/// every time the kernel took more memory, which is the opposite of
/// what a floor is for.
pub fn kernel_heap_reserve_bytes() -> usize {
    ALLOCATOR.reserve_bytes()
}

/// The machine's memory, across both domains.
///
/// The kernel heap and the user pool draw on the same physical memory
/// now, so the honest answer to "how much memory is there" is one
/// number for both: everything the boot memory map described, and
/// everything neither domain has spent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachineMemory {
    /// Every usable byte the boot memory map described.
    pub usable_bytes: usize,
    /// Bytes neither the kernel heap nor the user pool has handed out.
    pub free_bytes: usize,
}

/// Every usable byte the boot memory map described, as the installed
/// [`BootMemoryPlan`] recorded it.
///
/// The kernel sizes what has to move with the machine against this: the
/// user pool, the kernel heap's reserve, and every processor's executor
/// task arena.
pub(crate) fn machine_usable_bytes() -> usize {
    ALLOCATOR.machine_bytes()
}

pub fn machine_memory() -> MachineMemory {
    let heap = heap_stats();
    MachineMemory {
        usable_bytes: machine_usable_bytes(),
        free_bytes: heap
            .available_bytes()
            .saturating_add(memory::user_pool_available_bytes()),
    }
}

/// The kernel heap's free space measured against the reserve it keeps
/// for itself, and what a user-memory grow may take out of it.
///
/// Kernel and user memory are separate ownership domains (AGENTS §3).
/// A wasm grow is served from the user pool; what it costs the *kernel*
/// heap is the page tables and reservation records that address the new
/// pages — [`user_mapping_kernel_heap_bytes`] — not the pages
/// themselves. Charging the growth itself here refused grows the kernel
/// heap was never asked to fund, and refused them by an amount two
/// orders of magnitude larger than the real cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelHeapHeadroom {
    /// Kernel heap not currently allocated.
    pub available_bytes: usize,
    /// Kernel heap held back for the kernel's own working set. A user
    /// grow may not dip into it: a kernel OOM is fatal, so the reserve
    /// is what keeps user-mode demand from being able to end the
    /// kernel.
    pub reserve_bytes: usize,
}

impl KernelHeapHeadroom {
    /// The kernel heap's headroom right now.
    ///
    /// `available_bytes` counts the user pool's free memory as well as
    /// the kernel heap's own, because the kernel heap takes what it
    /// needs out of the pool: what bounds a user grow's kernel-side
    /// cost is the machine, not the share the kernel happens to be
    /// holding at the moment it is asked.
    pub fn current() -> Self {
        Self::of(heap_stats(), memory::user_pool_available_bytes())
    }

    pub fn of(heap: HeapStats, user_pool_available_bytes: usize) -> Self {
        Self {
            available_bytes: heap
                .available_bytes()
                .saturating_add(user_pool_available_bytes),
            reserve_bytes: kernel_heap_reserve_bytes(),
        }
    }

    /// The kernel heap a user-memory grow of `growth_bytes` cannot find
    /// above the reserve, or `None` when its kernel-side cost fits.
    ///
    /// The shortfall — not the growth, and not the cost alone — is what
    /// the OOM killer is asked to reclaim: it is the number of
    /// kernel-heap bytes that have to come back before the same grow
    /// can be admitted, which on a heap already under its reserve
    /// includes the breach as well as the cost.
    ///
    /// A grow of nothing needs nothing, even from a heap under its
    /// reserve: this answers a grow request, not a health check.
    pub const fn growth_shortfall_bytes(self, growth_bytes: usize) -> Option<usize> {
        let cost = user_mapping_kernel_heap_bytes(growth_bytes);
        if cost == 0 {
            return None;
        }
        match self
            .reserve_bytes
            .saturating_add(cost)
            .checked_sub(self.available_bytes)
        {
            None | Some(0) => None,
            Some(shortfall) => Some(shortfall),
        }
    }
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
    #[test]
    fn kernel_allocator_statistics_survive_two_processors_allocating_at_once() {
        // The counters live inside the heap lock now, so what proves
        // them is two threads racing through the same allocator: the
        // totals have to come out exactly right, not approximately.
        const THREADS: usize = 2;
        const ROUNDS: usize = 4096;
        const BLOCK_BYTES: usize = 64;
        const HEAP_BYTES: usize = 64 * 1024;

        #[repr(align(4096))]
        struct SharedHeap([u8; HEAP_BYTES]);

        let allocator = KernelAllocator::<HEAP_ORDER>::empty();
        allocator.set_size_class_metrics_enabled(true);
        let mut heap = Box::new(SharedHeap([0; HEAP_BYTES]));
        let start = heap.0.as_mut_ptr() as usize;
        unsafe {
            allocator.add_to_heap(start, start + HEAP_BYTES);
        }

        let layout = Layout::from_size_align(BLOCK_BYTES, 8).expect("valid allocation layout");
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let allocator = &allocator;
                scope.spawn(move || {
                    for _ in 0..ROUNDS {
                        let ptr = unsafe { GlobalAlloc::alloc(allocator, layout) };
                        assert!(!ptr.is_null(), "shared test heap ran dry");
                        unsafe {
                            GlobalAlloc::dealloc(allocator, ptr, layout);
                        }
                    }
                });
            }
        });

        let stats = allocator.stats();
        let operations = (THREADS * ROUNDS) as u64;
        let class = heap_size_class(BLOCK_BYTES);
        assert_eq!(stats.allocation_count, operations);
        assert_eq!(stats.deallocation_count, operations);
        assert_eq!(stats.reallocation_count, 0);
        assert_eq!(
            stats.total_allocation_bytes,
            operations * BLOCK_BYTES as u64
        );
        assert_eq!(
            stats.total_deallocation_bytes,
            operations * BLOCK_BYTES as u64
        );
        assert_eq!(stats.requested_live_bytes, 0);
        assert_eq!(stats.size_class_allocation_count[class], operations);
        assert_eq!(stats.size_class_deallocation_count[class], operations);
    }
}
