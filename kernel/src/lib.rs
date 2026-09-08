#![no_std]
#![cfg_attr(target_os = "none", feature(alloc_error_handler))]
#![allow(hidden_glob_reexports)]
extern crate alloc;
extern crate self as helios_kernel;
#[cfg(not(target_os = "none"))]
extern crate std;

mod bootfs;
mod component;
mod device;
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
mod profiling;
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
    RawRwLockWriteGuardResource, RetiredNetworkHandle, SerialPortResource, SocketRetirementQueue,
    SocketRetirementSender, StoreSocketRetirement, TcpStreamResource, UdpSocketResource,
    directory_prefix, map_resource_table_error, parent_path, path_is_within_directory,
    provider_channel, resolve_absolute_path, resolve_child_path, resolve_guest_path,
    retire_queued_handles, store_kernel_heap_bytes, strip_directory_prefix,
    wait_until_runtime_deadline,
};
pub use device::{
    DEFAULT_DMA_BUDGET_BYTES, DEVICE_WINDOW_BYTES, DeviceGrant, DeviceGrantRegistry,
    DeviceInterruptHooks, DeviceInterruptRoute, DeviceName, DeviceOwnership, DeviceVmHooks,
    DeviceWindow, DmaBudget, DmaBuffer, DmaBufferHandle, GrantError, GrantHandle, GrantInterrupt,
    GrantLease, GrantStats, GrantedDeviceSnapshot, InterruptEvent, InterruptRelay, InterruptStats,
    LinearMemory, MAX_DEVICE_NAME, MAX_DMA_BUFFERS, MAX_GRANT_INTERRUPTS, MAX_GRANT_REGIONS,
    MAX_GRANTS, MappedRegion, PublishedDevice, install_device_interrupt_hooks,
    install_device_vm_hooks,
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
    ProfileSink, ProgressChanged, ProgressMark, ProgressSignal, RawMutex, RawMutexLease, RawRwLock,
    RawRwLockReadLease, RawRwLockWriteLease, RwLock, RwLockReadGuard, RwLockWriteGuard, Sleep,
    Spawner, StatsSample, TaskCapacityError, TaskFunding, Timer, TraceEvent, TraceField,
    TraceFilter, TraceHistory, TraceLevel, TraceValue, UptimeClock, YieldNow, duration_to_ticks,
    elapsed_millis, matches_perf_metric_filter, matches_profile_filter, matches_trace_filter,
    monotonic_nanos, nanos_to_ticks_ceil_saturating, parse_console_text, wall_clock_offset_nanos,
    yield_now,
};
pub use helios_hal::Platform;
pub use helios_netstack::{
    ChecksumOffload, DEFAULT_POLL_BUDGET, EventDeliveryCapabilities, InterfaceCapabilities,
    InterfaceEventMark, LinkState, NetworkInterface as NetworkDevice, PacketBuffer, RxDrain,
    RxFrame, RxFrameOffload, SegmentationOffload, TxFrameRef,
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
    IommuReport, IommuStats, MAX_BLOCK_DEVICES, MAX_DEVICE_INTERRUPTS, MAX_IOMMU_ENDPOINTS,
    MAX_NETWORK_INTERRUPTS, PanicSerial, PollKey, PollRegistration, PollRegistry,
    PollRegistryError, PollSourceKind, RecordingConsole, SCRATCH_DISK_SERIAL, SerialReader,
    TryRead, TryWrite, byte_channel, emit_panic_report, install_block_devices, read_debug_serial,
    read_serial, try_read_serial, wake_queue_owners,
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
    allocate_user_run_zeroed_on, configure_user_memory_owner_processors, current_user_memory_owner,
    deallocate_user_frame, deallocate_user_frame_on, deallocate_user_run_on, disable_swap,
    enter_user_memory_owner, install_entropy_device, install_memory_balloon, install_swap,
    install_swap_hooks, installed_swap_handle, installed_swap_hooks, kernel_reserve_for,
    largest_servable_user_bytes, seed_root_entropy, set_user_memory_owner, swapped_token,
    task_arena_bytes_for, user_heap_stats, user_mapping_kernel_heap_bytes, validate_range,
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
pub use profiling::{
    KernelLlvmProfile, LlvmProfile, LlvmProfileError, MAX_PROFILE_READ, ProfileSection,
};
pub use runtime::{
    AuthorityDomain, ComponentHostFilesystemState, ComponentHostNetwork, ComponentNetworkService,
    ComponentNetworkState, DnsError, DnsErrorKind, ExecOutput, ExecResult, HostDirEntry,
    HostFileSystem, HostFsError, HostFsErrorKind, HostMetadata, Ipv4Address, NetworkErrorDetail,
    NetworkHandle, NetworkIpAddress, ObjectIdentity, PingError, PingErrorKind, PingReply,
    RegisteredTcpReadBuffer, RuntimeState, SocketReadiness, TcpAccepted, TcpError, TcpErrorKind,
    TcpListener, UdpBinding, UdpDatagram, UdpError, UdpErrorKind,
};
pub use vsock::{
    ComponentHostVsockService, MAX_VSOCK_BACKLOG, MAX_VSOCK_CONNECTIONS, MAX_VSOCK_LISTENERS,
    VSOCK_RECEIVE_WINDOW_BYTES, VsockError, VsockListenerId, VsockService, VsockStreamId,
    install_vsock_device,
};
#[cfg(feature = "wasmtime-runtime")]
pub use wasmtime_adapter::component_host::{
    ChildExit, ChildHandle, ComponentBindingSet, ComponentHostProcessorRole, HostRuntimeState,
    UserProgramService, component_host_processor_role, component_host_processors_to_start,
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
use helios_hal::cpu::{Cpu, Instant, ProcessorId, current_processor};
use helios_hal::memory::MemoryRegion;
use helios_hal::watchdog::{NoWatchdog, ProgressCounter, Watchdog};
use helios_hal::{DeviceInventory, DmaModel, ProcessorStartupPolicy, ProcessorTopology};
use talc::DefaultBinning;
use talc::base::{CHUNK_UNIT, Talc};
use talc::source::Manual;

use crate::memory::IrqSafeMutex;

/// Segregated free lists using the allocator's default size classes.
///
/// [`DefaultBinning`] covers small requests with linear classes and
/// larger requests with subdivisions of exponential classes. Its
/// availability bitmap has a fixed number of words per target, rather
/// than a size dependent on the memory map or the live allocations.
/// The class table is established in the first claimed heap region;
/// the allocator's counters include that permanent metadata in the
/// bytes unavailable to callers.
///
/// The binning configuration is the crate's general-purpose default,
/// not a workload-specific layout derived from one benchmark.
///
/// A selected free block is split around the actual allocation.
///
/// Alignment slack that can form a free block is returned to the
/// allocator immediately. Accounting follows those actual free blocks,
/// not a worst-case padding allowance derived from the request layout.
/// This matters to the reserve that controls kernel heap growth.
///
/// Bitmap updates and the free-list links belong to the allocator;
/// the kernel does not duplicate their representation or size rules.
///
/// The kernel heap's allocator has a bounded coalescing free path.
///
/// What this replaced was a buddy allocator whose free found a block's
/// buddy by walking that size class's free list, and walked the list
/// whole whenever the buddy was absent — the ordinary case in a mass
/// free. Tearing down a hundred instances is about forty thousand
/// frees, and the walk made the teardown quadratic in the blocks a
/// class was holding (#246). Talc uses boundary tags and doubly linked
/// free lists to unlink and coalesce the adjacent free blocks.
type KernelHeap = Talc<Manual, DefaultBinning>;
pub const HEAP_SIZE_CLASS_COUNT: usize = 12;
const BOOT_UNINITIALIZED: u8 = 0;
const BOOT_INITIALIZING: u8 = 1;
const BOOT_READY: u8 = 2;
const WATCHDOG_CHECK_DIVISOR: u32 = 4;
#[cfg(helios_watchdog_self_test)]
const WATCHDOG_SELF_TEST_DELAY_MILLIS_ENV: &str = env!("HELIOS_WATCHDOG_SELF_TEST_DELAY_MS");

#[cfg_attr(target_os = "none", global_allocator)]
static ALLOCATOR: KernelAllocator = KernelAllocator::empty();
static BOOT_STATE: AtomicU8 = AtomicU8::new(BOOT_UNINITIALIZED);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapStats {
    pub total_bytes: usize,
    pub allocated_bytes: usize,
    /// Heap the per-processor magazines hold ready to serve.
    ///
    /// It is counted in `allocated_bytes`, because the heap really did
    /// serve it, and it is free from a caller's point of view. A reader
    /// comparing the two needs this to tell a leak from a cache.
    pub magazine_cached_bytes: usize,
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

/// Heap accounting comes from the allocator's actual free blocks.
///
/// A request's size and alignment do not determine its occupied size:
/// the chosen address determines padding, and a split can return that
/// padding to the free lists. A worst-case search size therefore cannot
/// stand in for the bytes held by live allocations.
///
/// Talc's counters are updated when free blocks are registered and
/// deregistered, including splits and coalesces. Reading those counters
/// is constant time and needs neither a heap walk nor knowledge of
/// private block headers. Permanent heap metadata is unavailable too.
///
/// [`GlobalAlloc`] callers still supply their original layout on free;
/// only the allocator interprets it. The kernel maintains no duplicate
/// allocation charge that could disagree with the allocator's state.
struct KernelHeapState {
    allocator: KernelHeap,
    counters: HeapCounters,
}

/// The heap and its counters share the same IRQ-safe lock.
///
/// A stats read cannot catch a total that belongs to a different
/// allocation than the free space beside it. The allocator owns the
/// counters; this adapter exposes the quantities the kernel needs.
///
/// `total_bytes` counts claimed heap memory. `allocated_bytes` counts
/// all bytes unavailable to a caller, including live allocations,
/// their alignment and headers, and the allocator's permanent metadata.
/// Requested payload bytes are tracked separately by `HeapCounters`.
impl KernelHeapState {
    const fn new() -> Self {
        Self {
            allocator: Talc::new(Manual),
            counters: HeapCounters::new(),
        }
    }

    /// Every byte the allocator incorporated into its claimed heaps.
    /// Alignment slack excluded by `claim` is not advertised as usable
    /// heap memory.
    fn total_bytes(&self) -> usize {
        self.allocator.counters().claimed_bytes
    }

    /// Claimed memory unavailable for another allocation, including
    /// the allocator's own metadata.
    fn allocated_bytes(&self) -> usize {
        self.total_bytes() - self.free_bytes()
    }

    /// Heap not currently held by an allocation or allocator metadata.
    fn free_bytes(&self) -> usize {
        self.allocator.counters().available_bytes
    }

    /// Everything a stats caller wants, from one critical section.
    fn stats(&self) -> HeapStats {
        let counters = &self.counters;
        HeapStats {
            total_bytes: self.total_bytes(),
            allocated_bytes: self.allocated_bytes(),
            magazine_cached_bytes: 0,
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

    /// Gives the heap `start..end`.
    ///
    /// # Safety
    ///
    /// The range must be memory nothing else owns, and it must outlive
    /// the heap — which, this being the kernel's global allocator,
    /// means for as long as the kernel runs.
    unsafe fn insert(&mut self, start: usize, end: usize) {
        let len = end
            .checked_sub(start)
            .expect("kernel heap region ends before it starts");
        let start = ptr::NonNull::new(start as *mut u8)
            .expect("kernel heap region starts at the null address");
        // Safety: the caller gave us the region outright, and it
        // outlives the kernel.
        unsafe { self.allocator.claim(start.as_ptr(), len) }.unwrap_or_else(|| {
            panic!("kernel heap region of {len} bytes is too small to establish allocator metadata")
        });
    }

    fn growth_bytes(&self, layout: Layout) -> Option<usize> {
        let metadata = if self.allocator.is_metadata_established() {
            0
        } else {
            talc::min_first_heap_layout::<DefaultBinning>().size()
        };
        layout
            .size()
            .checked_add(layout.align())?
            .checked_add(CHUNK_UNIT * 2)?
            .checked_add(metadata)?
            .checked_next_power_of_two()
            .map(|bytes| bytes.max(memory::KERNEL_HEAP_GROWTH_CHUNK_BYTES))
    }

    /// Serves `layout` out of the heap, or answers null.
    fn allocate(&mut self, layout: Layout) -> *mut u8 {
        assert_ne!(layout.size(), 0, "kernel heap allocation has zero size");
        unsafe { self.allocator.try_allocate(layout) }.map_or(ptr::null_mut(), ptr::NonNull::as_ptr)
    }

    /// Returns one allocation to the heap.
    ///
    /// # Safety
    ///
    /// `ptr` must be an allocation this heap served under `layout`.
    unsafe fn deallocate(&mut self, ptr: ptr::NonNull<u8>, layout: Layout) {
        // Safety: the caller's promise is exactly `deallocate`'s, and
        // `layout` is the layout the block was allocated with.
        unsafe { self.allocator.deallocate(ptr.as_ptr(), layout) };
    }
}

struct KernelAllocator {
    /// The kernel heap, behind the mask every allocator in this kernel
    /// takes: an interrupt handler allocates and frees, so a plain spin
    /// lock here deadlocks the processor that was interrupted holding
    /// it, and then every other processor behind it (#206). See
    /// [`memory::IrqSafeMutex`] for the contract.
    heap: IrqSafeMutex<KernelHeapState>,
    /// Per-processor caches in front of `heap`; see
    /// [`memory::Magazines`] for what they hold and why.
    magazines: memory::Magazines,
    /// Every usable byte the boot memory map described, and the free
    /// kernel heap a user grow may not dip into. Both are fixed by
    /// [`memory::BootMemoryPlan`] at boot and never move afterwards:
    /// the heap's own size is demand-driven, so a reserve defined
    /// against it would be a floor that moved with the thing it is
    /// supposed to hold down.
    machine_usable_bytes: AtomicUsize,
    kernel_reserve_bytes: AtomicUsize,
    /// Allocations still to serve before another reserve top-up is
    /// attempted; see [`Self::alloc_growing`].
    top_up_backoff: AtomicUsize,
}

impl KernelAllocator {
    const fn empty() -> Self {
        Self {
            heap: IrqSafeMutex::new(KernelHeapState::new()),
            magazines: memory::Magazines::new(),
            machine_usable_bytes: AtomicUsize::new(0),
            kernel_reserve_bytes: AtomicUsize::new(0),
            top_up_backoff: AtomicUsize::new(0),
        }
    }

    /// Gives the heap `start..end`.
    ///
    /// # Safety
    ///
    /// The range must be memory nothing else owns and it must outlive
    /// the kernel; see [`KernelHeapState::insert`].
    unsafe fn add_to_heap(&self, start: usize, end: usize) {
        self.heap.with(|heap| unsafe {
            heap.insert(start, end);
        });
    }

    /// Returns one allocation to the heap.
    ///
    /// # Safety
    ///
    /// `ptr` must be an allocation this heap served under `layout`,
    /// which is what every [`GlobalAlloc`] caller already promises.
    ///
    /// `record` charges the operation to the counters inside the
    /// critical section the free already holds. A `dealloc` records a
    /// deallocation; a `realloc`, whose free is the second half of one
    /// operation, records the reallocation here and nothing at its
    /// allocation.
    unsafe fn free(&self, ptr: *mut u8, layout: Layout, record: impl FnOnce(&mut HeapCounters)) {
        let ptr = ptr::NonNull::new(ptr).expect("the global allocator was handed a null pointer");
        let served = memory::heap_layout(layout);
        self.heap.with(|heap| {
            unsafe { heap.deallocate(ptr, served) };
            record(&mut heap.counters);
        });
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

    /// One allocation attempt, plus the free space the heap was left
    /// with.
    ///
    /// The two are read under the same lock the allocation took, so the
    /// growth decision below is made against the state the allocation
    /// actually produced rather than a racing re-read.
    fn try_alloc(
        &self,
        layout: Layout,
        record: impl Fn(&mut HeapCounters) + Copy,
    ) -> (*mut u8, usize) {
        let served = memory::heap_layout(layout);
        self.heap.with(|heap| {
            let ptr = heap.allocate(served);
            if !ptr.is_null() {
                record(&mut heap.counters);
            }
            (ptr, heap.free_bytes())
        })
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
    fn alloc_growing(&self, layout: Layout, record: impl Fn(&mut HeapCounters) + Copy) -> *mut u8 {
        let (ptr, free) = self.try_alloc(layout, record);
        if !ptr.is_null() && free >= self.reserve_bytes() {
            return ptr;
        }
        if !ptr.is_null() && !self.top_up_is_due() {
            return ptr;
        }

        let Some(wanted) = self
            .heap
            .with(|heap| heap.growth_bytes(memory::heap_layout(layout)))
        else {
            return ptr;
        };
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
            self.try_alloc(layout, record).0
        } else {
            ptr
        }
    }

    /// Serves `layout` from this processor's magazine, refilling it
    /// from the heap under one lock when it is empty.
    ///
    /// Answers null for a layout no class serves, before the magazines
    /// exist, and while per-class metrics bypass them; the caller then
    /// takes the ordinary path.
    fn alloc_cached(&self, layout: Layout) -> *mut u8 {
        self.magazines.allocate(layout, |canonical, blocks| {
            self.heap.with(|heap| {
                let mut taken = 0;
                while taken < blocks.len() {
                    let block = heap.allocate(canonical);
                    if block.is_null() {
                        break;
                    }
                    blocks[taken] = block;
                    taken += 1;
                }
                taken
            })
        })
    }

    /// Returns `ptr` to this processor's magazine, flushing what is
    /// over capacity back to the heap under one lock.
    ///
    /// Answers `false` when no magazine took it, which is exactly when
    /// [`Self::alloc_cached`] would not have served it.
    ///
    /// # Safety
    ///
    /// `ptr` must be an allocation this heap served under `layout`.
    unsafe fn free_cached(&self, ptr: *mut u8, layout: Layout) -> bool {
        self.magazines.deallocate(ptr, layout, |canonical, blocks| {
            self.heap.with(|heap| {
                for block in blocks {
                    let Some(block) = ptr::NonNull::new(*block) else {
                        continue;
                    };
                    // Safety: every block came from a refill of this
                    // class, allocated with `canonical`.
                    unsafe { heap.deallocate(block, canonical) };
                }
            });
        })
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
        let mut stats = self.heap.with(|heap| heap.stats());
        let cached = self.magazines.stats();
        stats.allocation_count = stats.allocation_count.wrapping_add(cached.allocation_count);
        stats.deallocation_count = stats
            .deallocation_count
            .wrapping_add(cached.deallocation_count);
        stats.total_allocation_bytes = stats
            .total_allocation_bytes
            .wrapping_add(cached.total_allocation_bytes);
        stats.total_deallocation_bytes = stats
            .total_deallocation_bytes
            .wrapping_add(cached.total_deallocation_bytes);
        stats.requested_live_bytes = stats
            .requested_live_bytes
            .wrapping_add_signed(cached.live_bytes as isize);
        stats.magazine_cached_bytes = cached.cached_bytes;
        stats
    }

    fn set_size_class_metrics_enabled(&self, enabled: bool) {
        // Per-class counts describe what the heap was asked for, so the
        // caches stand aside while they are on: see `memory::Magazines`.
        self.magazines.set_bypassed(enabled);
        self.heap
            .with(|heap| heap.counters.size_class_metrics_enabled = enabled);
    }
}

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let cached = self.alloc_cached(layout);
        if !cached.is_null() {
            return cached;
        }
        let size = layout.size();
        self.alloc_growing(layout, |counters| counters.record_alloc(size))
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let mut ptr = self.alloc_cached(layout);
        if ptr.is_null() {
            ptr = self.alloc_growing(layout, |counters| counters.record_alloc(size));
        }
        if !ptr.is_null() {
            unsafe {
                ptr::write_bytes(ptr, 0, size);
            }
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Safety: the caller's promise is `dealloc`'s, and a block the
        // magazine accepts was served by the same class.
        if unsafe { self.free_cached(ptr, layout) } {
            return;
        }
        let size = layout.size();
        unsafe { self.free(ptr, layout, |counters| counters.record_dealloc(size)) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        // A reallocation is one operation: it is charged once, in the
        // free below, which is the critical section that ends it.
        let new_ptr = self.alloc_growing(new_layout, |_| {});
        if new_ptr.is_null() {
            return ptr::null_mut();
        }

        let old_size = layout.size();
        unsafe { ptr::copy_nonoverlapping(ptr, new_ptr, old_size.min(new_size)) };
        // Safety: `ptr` was served under `layout`, so the same class
        // that would take it back is the one that served it.
        if unsafe { self.free_cached(ptr, layout) } {
            self.heap
                .with(|heap| heap.counters.record_realloc(old_size, new_size));
        } else {
            unsafe {
                self.free(ptr, layout, |counters| {
                    counters.record_realloc(old_size, new_size);
                });
            }
        }
        new_ptr
    }
}

/// The kernel heap's own accounting, kept in the state the heap lock
/// already protects.
///
/// These words are written on every allocation and every free from
/// every processor. As atomics beside the lock they were three
/// read-modify-writes on lines no processor owns for long, bouncing
/// between cores on top of the lock the allocation had just taken.
/// Inside the locked state they are plain fields on lines the lock
/// holder owns exclusively, and a stats read gets them from the same
/// critical section that reads the allocator's own counters, so a
/// total can no longer belong to a different allocation than the free
/// space beside it.
struct HeapCounters {
    requested_live_bytes: usize,
    allocation_count: u64,
    deallocation_count: u64,
    reallocation_count: u64,
    total_allocation_bytes: u64,
    total_deallocation_bytes: u64,
    total_reallocation_bytes: u64,
    /// Off by default: the per-class arrays are diagnosis, and the
    /// branch keeps them off the hot path when nothing is asking.
    size_class_metrics_enabled: bool,
    size_class_allocation_count: [u64; HEAP_SIZE_CLASS_COUNT],
    size_class_deallocation_count: [u64; HEAP_SIZE_CLASS_COUNT],
    size_class_reallocation_count: [u64; HEAP_SIZE_CLASS_COUNT],
    size_class_allocation_bytes: [u64; HEAP_SIZE_CLASS_COUNT],
    size_class_deallocation_bytes: [u64; HEAP_SIZE_CLASS_COUNT],
    size_class_reallocation_bytes: [u64; HEAP_SIZE_CLASS_COUNT],
}

impl HeapCounters {
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
        self.requested_live_bytes -= size;
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
            self.requested_live_bytes -= old_size - new_size;
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
        if current_processor() == self.topology.bootstrap_processor {
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
        let processor = current_processor().id();
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
        let owner_processor = current_processor();
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
        if current_processor() != self.owner_processor {
            self.cpu.wake_processor(self.owner_processor);
        }
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.notified.store(true, Ordering::Release);
        if current_processor() != self.owner_processor {
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
    let current_processor = current_processor();
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
    let mut user_regions: ArrayVec<(usize, usize), MAX_BOOT_MEMORY_REGIONS> = ArrayVec::new();
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
        user_pool.get_or_insert_with(|| {
            let pool = memory::install_user_memory_pool(memory::allocate_user_memory_pool());
            pool.configure_processors(processor_count);
            // The swap policy asks which instance a committed page
            // belongs to, and the answer is per-processor; size that
            // table with the pool it describes.
            memory::configure_user_memory_owner_processors(processor_count);
            pool
        });
        user_regions.push((user.start, user.end));
    }

    assert_eq!(
        splitter.kernel_owed_bytes(),
        0,
        "the boot memory map is smaller than the kernel heap's boot share of {} bytes",
        plan.kernel_boot_bytes
    );
    let pool =
        user_pool.unwrap_or_else(|| panic!("bootstrap did not provide memory for user pool"));
    pool.initialize(&user_regions);
    // The caches come last: they allocate their own array, so the heap
    // has to be able to serve before they exist. Every allocation until
    // this point went straight to the heap, which is what the boot path
    // wants anyway — it runs on one processor and contends with nobody.
    ALLOCATOR.magazines.initialize(processor_count);
    pool
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

/// The processor identity contract for host test builds.
///
/// A test binary links no backend, so it defines the symbol itself and
/// answers from a per-thread slot. A host test binary is one processor
/// unless a test says otherwise, which the SMP test CPUs do when they
/// are built for a particular slot.
#[cfg(test)]
mod test_processor_identity {
    use helios_hal::cpu::ProcessorId;

    std::thread_local! {
        static CURRENT: core::cell::Cell<Option<ProcessorId>> =
            const { core::cell::Cell::new(None) };
    }

    pub(crate) fn set(processor: ProcessorId) {
        CURRENT.with(|slot| slot.set(Some(processor)));
    }

    #[unsafe(no_mangle)]
    extern "Rust" fn helios_current_processor() -> ProcessorId {
        CURRENT.with(|slot| slot.get().unwrap_or(ProcessorId::new(0)))
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use core::alloc::{GlobalAlloc, Layout};

    use super::*;

    const TEST_HEAP_BYTES: usize = 16 * 1024;

    #[repr(align(4096))]
    struct AlignedHeap([u8; TEST_HEAP_BYTES]);

    /// The heap's counters come back to where they started once
    /// every allocation is freed, whatever sizes and alignments went
    /// through it.
    ///
    /// Permanent allocator metadata remains occupied after the live
    /// allocations are freed. Everything else must return to the free
    /// blocks the allocator counts, because that is the reserve the
    /// growth path in [`KernelAllocator::alloc_growing`] measures
    /// itself against. Drift would leave the heap looking permanently
    /// fuller than it is and make it take chunks from the user pool
    /// unnecessarily.
    #[test]
    fn the_kernel_heaps_free_space_returns_after_every_allocation_is_freed() {
        let mut heap = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
        let allocator = KernelAllocator::empty();
        let start = heap.0.as_mut_ptr() as usize;
        unsafe {
            allocator.add_to_heap(start, start + TEST_HEAP_BYTES);
        }

        let empty = allocator.stats();
        assert!(empty.allocated_bytes > 0);
        assert!(empty.available_bytes() > 0);

        // Sizes and alignments that exercise padding and splitting:
        // ordinary small allocations mixed with over-aligned payloads
        // must return their occupied space after every free.
        let layouts = [(1, 1), (17, 8), (64, 16), (100, 32), (7, 64), (512, 256)]
            .map(|(size, align)| Layout::from_size_align(size, align).expect("valid test layout"));

        let mut live = ArrayVec::<*mut u8, 6>::new();
        for layout in layouts {
            let ptr = unsafe { GlobalAlloc::alloc(&allocator, layout) };
            assert!(!ptr.is_null(), "the test heap refused {layout:?}");
            assert!(
                (ptr as usize).is_multiple_of(layout.align()),
                "the heap returned {ptr:?} for {layout:?}"
            );
            live.push(ptr);
        }

        let held = allocator.stats();
        assert!(
            held.allocated_bytes >= layouts.iter().map(Layout::size).sum::<usize>(),
            "the heap charged {} bytes for allocations asking for {}",
            held.allocated_bytes,
            layouts.iter().map(Layout::size).sum::<usize>()
        );
        assert_eq!(held.total_bytes, empty.total_bytes);

        for (ptr, layout) in live.into_iter().zip(layouts) {
            unsafe {
                GlobalAlloc::dealloc(&allocator, ptr, layout);
            }
        }

        let drained = allocator.stats();
        assert_eq!(drained.allocated_bytes, empty.allocated_bytes);
        assert_eq!(drained.total_bytes, empty.total_bytes);
        assert_eq!(drained.available_bytes(), empty.available_bytes());
    }

    #[test]
    fn kernel_heap_reports_actual_free_space_after_aligned_allocations() {
        let mut backing = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
        let start = backing.0.as_mut_ptr() as usize;
        let mut heap = KernelHeapState::new();
        unsafe { heap.insert(start, start + TEST_HEAP_BYTES) };
        let empty_free = heap.free_bytes();
        let metadata = heap.allocated_bytes();
        assert!(metadata > 0);

        let small = Layout::from_size_align(1, 1).expect("valid small layout");
        let first = ptr::NonNull::new(heap.allocate(small)).expect("first allocation");
        for align in [32, 64, 256, 4096] {
            let layout = Layout::from_size_align(1, align).expect("valid aligned layout");
            let before = heap.free_bytes();
            let allocation = ptr::NonNull::new(heap.allocate(layout)).expect("aligned allocation");
            assert!((allocation.as_ptr() as usize).is_multiple_of(align));
            let counters = heap.allocator.counters();
            assert_eq!(heap.total_bytes(), counters.claimed_bytes);
            assert_eq!(heap.free_bytes(), counters.available_bytes);
            assert_eq!(
                heap.allocated_bytes(),
                counters.allocated_bytes + counters.overhead_bytes()
            );
            assert!(heap.free_bytes() < before);
            unsafe { heap.deallocate(allocation, layout) };
            assert_eq!(heap.free_bytes(), before);
        }
        unsafe { heap.deallocate(first, small) };
        assert_eq!(heap.free_bytes(), empty_free);
        assert_eq!(heap.allocated_bytes(), metadata);
    }

    #[test]
    fn kernel_heap_reclaims_fragmented_allocations_across_regions() {
        let mut first_region = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
        let mut second_region = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
        let mut heap = KernelHeapState::new();
        for region in [&mut first_region, &mut second_region] {
            let start = region.0.as_mut_ptr() as usize;
            unsafe { heap.insert(start, start + TEST_HEAP_BYTES) };
        }
        let empty_free = heap.free_bytes();
        let mut live = Vec::new();
        for index in 0..128 {
            let layout = Layout::from_size_align(1 + index * 3, 1 << (index % 9))
                .expect("valid varied layout");
            let Some(ptr) = ptr::NonNull::new(heap.allocate(layout)) else {
                break;
            };
            assert!((ptr.as_ptr() as usize).is_multiple_of(layout.align()));
            unsafe { ptr.as_ptr().write_bytes(index as u8, layout.size()) };
            live.push((ptr, layout, index as u8));
        }
        assert!(
            live.iter()
                .map(|(_, layout, _)| layout.size())
                .sum::<usize>()
                > TEST_HEAP_BYTES
        );
        for parity in 0..2 {
            for &(ptr, layout, value) in live.iter().skip(parity).step_by(2) {
                let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), layout.size()) };
                assert!(bytes.iter().all(|&byte| byte == value));
                unsafe { heap.deallocate(ptr, layout) };
            }
        }
        assert_eq!(heap.free_bytes(), empty_free);
    }

    #[test]
    fn kernel_heap_growth_includes_alignment_and_allocator_metadata() {
        for initialized in [false, true] {
            for (size, align) in [
                (memory::KERNEL_HEAP_GROWTH_CHUNK_BYTES, 4096),
                (1, memory::KERNEL_HEAP_GROWTH_CHUNK_BYTES),
            ] {
                let mut boot = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
                let mut region;
                let mut heap = KernelHeapState::new();
                if initialized {
                    let start = boot.0.as_mut_ptr() as usize;
                    unsafe { heap.insert(start, start + TEST_HEAP_BYTES) };
                }
                let layout = Layout::from_size_align(size, align).expect("valid growth layout");
                let bytes = heap.growth_bytes(layout).expect("representable growth");
                assert!(bytes.is_power_of_two());
                region = Box::<[u8]>::new_uninit_slice(bytes);
                let start = region.as_mut_ptr() as usize;
                unsafe { heap.insert(start, start + bytes) };
                let available = heap.free_bytes();
                let ptr =
                    ptr::NonNull::new(heap.allocate(layout)).expect("grown heap serves request");
                assert!((ptr.as_ptr() as usize).is_multiple_of(align));
                unsafe { heap.deallocate(ptr, layout) };
                assert_eq!(heap.free_bytes(), available);
            }
        }
    }

    #[test]
    fn kernel_heap_rejects_unrepresentable_growth() {
        let heap = KernelHeapState::new();
        let layout = Layout::from_size_align(isize::MAX as usize, 1).expect("valid maximal layout");
        assert_eq!(heap.growth_bytes(layout), None);
    }

    #[test]
    fn kernel_allocator_tracks_requested_allocation_pressure() {
        let mut heap = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
        let allocator = KernelAllocator::empty();
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

    /// The allocator has to serve a caller that is already inside a
    /// machine-wide critical section.
    ///
    /// The path is real: a virtio interrupt ends in
    /// [`Notify::notify_all`], which is `event_listener::Event::notify`;
    /// the kernel builds `event-listener` with its `critical-section`
    /// feature, so the notify takes a critical section and allocates its
    /// shared state inside one the first time it runs. If the kernel
    /// heap ever took a lock that could not nest inside the section its
    /// own caller holds, that first notify would hang the processor.
    ///
    /// The heap's own mask is processor-local and takes no owner word,
    /// so this nests trivially now. It did not always: the first version
    /// of the #206 fix took `critical_section::with` itself here, and
    /// depended on that section being re-entrant for the same
    /// processor. The test is kept because the caller's section is real
    /// whatever the heap does underneath.
    #[test]
    fn the_kernel_allocator_serves_a_caller_already_inside_a_critical_section() {
        let mut heap = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
        let allocator = KernelAllocator::empty();
        let start = heap.0.as_mut_ptr() as usize;
        unsafe {
            allocator.add_to_heap(start, start + TEST_HEAP_BYTES);
        }

        let layout = Layout::from_size_align(64, 8).expect("valid allocation layout");
        critical_section::with(|_| {
            let ptr = unsafe { GlobalAlloc::alloc(&allocator, layout) };
            assert!(
                !ptr.is_null(),
                "the kernel heap refused an allocation issued from inside a critical section"
            );
            let grown = unsafe { GlobalAlloc::realloc(&allocator, ptr, layout, 128) };
            assert!(!grown.is_null());
            let grown_layout = Layout::from_size_align(128, 8).expect("valid grown layout");
            unsafe {
                GlobalAlloc::dealloc(&allocator, grown, grown_layout);
            }
        });

        let stats = allocator.stats();
        assert_eq!(stats.allocation_count, 1);
        assert_eq!(stats.reallocation_count, 1);
        assert_eq!(stats.deallocation_count, 1);
        assert_eq!(stats.requested_live_bytes, 0);
    }
}
