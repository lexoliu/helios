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
use buddy_system_allocator::Heap;
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::critical_section::with_local_interrupts_masked;
use helios_hal::memory::MemoryRegion;
use helios_hal::watchdog::{NoWatchdog, ProgressCounter, Watchdog};
use helios_hal::{DeviceInventory, DmaModel, ProcessorStartupPolicy, ProcessorTopology};

use crate::memory::IrqSafeMutex;

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
    /// Bytes the per-processor magazines hold out of the shared heap
    /// (`memory::magazine`).
    ///
    /// They are inside `allocated_bytes` too, and correctly so: the
    /// buddy heap has handed them out and cannot merge them or serve a
    /// larger allocation from them until a magazine gives them back.
    /// This field says how much of that is a cache rather than a live
    /// kernel object.
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

    /// The zero every processor's counters are summed into.
    ///
    /// The counters live one block per processor now
    /// (`memory::magazine`), so a stats read is an accumulation rather
    /// than a set of loads, and this is what it starts from.
    pub(crate) const fn zeroed() -> Self {
        Self {
            total_bytes: 0,
            allocated_bytes: 0,
            magazine_cached_bytes: 0,
            requested_live_bytes: 0,
            allocation_count: 0,
            deallocation_count: 0,
            reallocation_count: 0,
            total_allocation_bytes: 0,
            total_deallocation_bytes: 0,
            total_reallocation_bytes: 0,
            size_class_allocation_count: [0; HEAP_SIZE_CLASS_COUNT],
            size_class_deallocation_count: [0; HEAP_SIZE_CLASS_COUNT],
            size_class_reallocation_count: [0; HEAP_SIZE_CLASS_COUNT],
            size_class_allocation_bytes: [0; HEAP_SIZE_CLASS_COUNT],
            size_class_deallocation_bytes: [0; HEAP_SIZE_CLASS_COUNT],
            size_class_reallocation_bytes: [0; HEAP_SIZE_CLASS_COUNT],
        }
    }
}

struct KernelAllocator<const ORDER: usize> {
    /// The kernel heap, behind the mask every allocator in this kernel
    /// takes: an interrupt handler allocates and frees, so a plain spin
    /// lock here deadlocks the processor that was interrupted holding
    /// it, and then every other processor behind it (#206). See
    /// [`memory::IrqSafeMutex`] for the contract.
    heap: IrqSafeMutex<Heap<ORDER>>,
    /// The per-processor front in front of that lock: a magazine of
    /// cached blocks per small size class, and the allocation counters,
    /// both owned by the processor they belong to. See
    /// `memory::magazine` for the concurrency contract. Most kernel
    /// allocations never reach the heap above.
    magazines: memory::HeapMagazines,
    /// The counters for allocations no processor could claim, because
    /// the processor serving them still carried a bootstrapping
    /// identity or the front had not been sized yet. Boot-only, and
    /// stepped atomically because two bootstrapping processors may
    /// reach it at once.
    unslotted_stats: memory::HeapCounters,
    /// Whether the size-class breakdown is being collected. Read on
    /// every allocation and written only when profiling is switched, so
    /// it is a shared line that is never written on the hot path.
    size_class_metrics_enabled: AtomicBool,
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

impl<const ORDER: usize> KernelAllocator<ORDER> {
    const fn empty() -> Self {
        Self {
            heap: IrqSafeMutex::new(Heap::new()),
            magazines: memory::HeapMagazines::new(),
            unslotted_stats: memory::HeapCounters::new(),
            size_class_metrics_enabled: AtomicBool::new(false),
            machine_usable_bytes: AtomicUsize::new(0),
            kernel_reserve_bytes: AtomicUsize::new(0),
            top_up_backoff: AtomicUsize::new(0),
        }
    }

    unsafe fn add_to_heap(&self, start: usize, end: usize) {
        self.heap.with(|heap| unsafe {
            heap.add_to_heap(start, end);
        });
    }

    /// The front belonging to the processor this call is running on, or
    /// `None` while that processor cannot name its slot.
    ///
    /// One load off the processor-local register — `fs` on x86-64,
    /// `tpidr_el1` on AArch64, `tp` on RISC-V — reached by linkage
    /// because a global allocator holds no [`Cpu`] and never can; see
    /// [`helios_hal::cpu::current_processor_slot`].
    #[inline]
    fn front(&self) -> Option<&memory::ProcessorFront> {
        let slot = helios_hal::cpu::current_processor_slot()?;
        self.magazines.front(slot)
    }

    #[inline]
    fn size_class_metrics(&self) -> bool {
        self.size_class_metrics_enabled.load(Ordering::Relaxed)
    }

    /// Serves `layout`, out of this processor's magazine when the class
    /// is one it caches and it has a block.
    ///
    /// This is the memory half only: the caller counts what it got, so
    /// that a reallocation can count itself as one rather than as an
    /// allocation and a free.
    fn allocate_block(&self, front: Option<&memory::ProcessorFront>, layout: Layout) -> *mut u8 {
        let Some(class) = memory::MagazineClass::of(layout) else {
            return self.alloc_growing(layout);
        };
        if let Some(front) = front
            && let Some(block) = front.take(class)
        {
            return block.as_ptr();
        }

        // A miss goes to the shared heap for the caller's own block and
        // then, in one more acquisition, for a batch behind it, so the
        // next `MAGAZINE_BATCH` allocations of this class do not come
        // back here. The block is asked for under the class's own
        // layout, not the caller's: that is what lets any processor
        // later serve it out of its magazine, and it is the same block
        // the heap would have chosen for the caller's layout anyway.
        let ptr = self.alloc_growing(class.layout());
        if !ptr.is_null()
            && let Some(front) = front
        {
            self.refill(front, class);
        }
        ptr
    }

    /// Returns `ptr` to this processor's magazine when the class is one
    /// it caches, and to the shared heap otherwise.
    ///
    /// # Safety
    ///
    /// `ptr` must be an allocation this allocator served under
    /// `layout`, which is what every [`GlobalAlloc`] caller already
    /// promises.
    unsafe fn release_block(
        &self,
        front: Option<&memory::ProcessorFront>,
        ptr: *mut u8,
        layout: Layout,
    ) {
        let Some(class) = memory::MagazineClass::of(layout) else {
            unsafe { self.free(ptr, layout) };
            return;
        };
        let block = ptr::NonNull::new(ptr).expect("the global allocator was handed a null pointer");
        let Some(front) = front else {
            // The class is cached but this processor has no front yet,
            // so the block goes back under the class layout it was
            // taken under; anything else would leave the heap's byte
            // accounting asymmetric.
            unsafe { self.free(ptr, class.layout()) };
            return;
        };
        // SAFETY: the block was served under the class layout — every
        // allocation of a cached class is, above — and the caller
        // promises nothing else references it.
        if let Some(mut batch) = unsafe { front.give(class, block) } {
            self.free_batch(class, &mut batch);
        }
    }

    /// Stocks one class of `front`'s magazine out of the shared heap.
    ///
    /// One acquisition for up to a batch. The heap is never grown from
    /// here — growth belongs to [`Self::alloc_growing`], which the
    /// caller's own block has already been through — and the refill
    /// stops before it would take the heap below its reserve, so a
    /// cache never eats the memory the kernel keeps for itself.
    fn refill(&self, front: &memory::ProcessorFront, class: memory::MagazineClass) {
        let layout = class.layout();
        let reserve = self.reserve_bytes();
        let mut blocks: ArrayVec<ptr::NonNull<u8>, { memory::MAGAZINE_BATCH }> = ArrayVec::new();
        self.heap.with(|heap| {
            while !blocks.is_full() {
                let free = heap
                    .stats_total_bytes()
                    .saturating_sub(heap.stats_alloc_actual());
                if free < reserve.saturating_add(layout.size()) {
                    break;
                }
                let Ok(block) = heap.alloc(layout) else {
                    break;
                };
                blocks.push(block);
            }
        });
        if blocks.is_empty() {
            return;
        }

        // SAFETY: every block came from `heap.alloc` under the class
        // layout and nothing else references it.
        unsafe { front.stock(class, &mut blocks) };
        if !blocks.is_empty() {
            self.free_batch(class, &mut blocks);
        }
    }

    /// Returns a magazine's overflow to the shared heap under one
    /// acquisition.
    fn free_batch(
        &self,
        class: memory::MagazineClass,
        batch: &mut ArrayVec<ptr::NonNull<u8>, { memory::MAGAZINE_BATCH }>,
    ) {
        let layout = class.layout();
        self.heap.with(|heap| {
            for block in batch.drain(..) {
                // SAFETY: every block in the batch was served by this
                // heap under `layout` and is no longer referenced.
                unsafe { heap.dealloc(block, layout) };
            }
        });
    }

    /// Returns one allocation to the heap.
    ///
    /// # Safety
    ///
    /// `ptr` must be an allocation this heap served under `layout`,
    /// which is what every [`GlobalAlloc`] caller already promises.
    unsafe fn free(&self, ptr: *mut u8, layout: Layout) {
        let ptr = ptr::NonNull::new(ptr).expect("the global allocator was handed a null pointer");
        self.heap.with(|heap| unsafe { heap.dealloc(ptr, layout) });
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
    fn try_alloc(&self, layout: Layout) -> (*mut u8, usize) {
        self.heap.with(|heap| {
            let ptr = heap
                .alloc(layout)
                .map_or(ptr::null_mut(), core::ptr::NonNull::as_ptr);
            let free = heap
                .stats_total_bytes()
                .saturating_sub(heap.stats_alloc_actual());
            (ptr, free)
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
    fn alloc_growing(&self, layout: Layout) -> *mut u8 {
        let (ptr, free) = self.try_alloc(layout);
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
            self.try_alloc(layout).0
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

    /// Sizes the per-processor front, once the heap it lives in has
    /// memory and the backend has said how many processors there are.
    fn configure_processors(&self, processor_count: usize) {
        self.magazines.configure_processors(processor_count);
    }

    fn stats(&self) -> HeapStats {
        let mut stats = HeapStats::zeroed();
        let (total_bytes, allocated_bytes) = self
            .heap
            .with(|heap| (heap.stats_total_bytes(), heap.stats_alloc_actual()));
        stats.total_bytes = total_bytes;
        stats.allocated_bytes = allocated_bytes;
        stats.magazine_cached_bytes = self.magazines.cached_bytes();
        self.unslotted_stats.accumulate_into(&mut stats);
        self.magazines.accumulate_counters(&mut stats);
        stats
    }

    fn set_size_class_metrics_enabled(&self, enabled: bool) {
        self.size_class_metrics_enabled
            .store(enabled, Ordering::Release);
    }

    /// Counts one allocation on the processor that served it.
    ///
    /// A processor's own counters are plain words behind the local
    /// interrupt mask; the block for a processor that cannot name a
    /// slot is shared and steps atomically. `memory::magazine` states
    /// why the two differ.
    #[inline]
    fn record_alloc(&self, front: Option<&memory::ProcessorFront>, size: usize) {
        let metrics = self.size_class_metrics();
        match front {
            Some(front) => with_local_interrupts_masked(|| {
                front
                    .counters()
                    .record_alloc::<memory::OwnedStep>(size, metrics);
            }),
            None => self
                .unslotted_stats
                .record_alloc::<memory::SharedStep>(size, metrics),
        }
    }

    /// Counts one deallocation; see [`Self::record_alloc`].
    #[inline]
    fn record_dealloc(&self, front: Option<&memory::ProcessorFront>, size: usize) {
        let metrics = self.size_class_metrics();
        match front {
            Some(front) => with_local_interrupts_masked(|| {
                front
                    .counters()
                    .record_dealloc::<memory::OwnedStep>(size, metrics);
            }),
            None => self
                .unslotted_stats
                .record_dealloc::<memory::SharedStep>(size, metrics),
        }
    }

    /// Counts one reallocation; see [`Self::record_alloc`].
    #[inline]
    fn record_realloc(
        &self,
        front: Option<&memory::ProcessorFront>,
        old_size: usize,
        new_size: usize,
    ) {
        let metrics = self.size_class_metrics();
        match front {
            Some(front) => with_local_interrupts_masked(|| {
                front
                    .counters()
                    .record_realloc::<memory::OwnedStep>(old_size, new_size, metrics);
            }),
            None => {
                self.unslotted_stats
                    .record_realloc::<memory::SharedStep>(old_size, new_size, metrics);
            }
        }
    }
}

unsafe impl<const ORDER: usize> GlobalAlloc for KernelAllocator<ORDER> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let front = self.front();
        let ptr = self.allocate_block(front, layout);
        if !ptr.is_null() {
            self.record_alloc(front, layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let front = self.front();
        let ptr = self.allocate_block(front, layout);
        if !ptr.is_null() {
            unsafe {
                ptr::write_bytes(ptr, 0, layout.size());
            }
            self.record_alloc(front, layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let front = self.front();
        unsafe { self.release_block(front, ptr, layout) };
        self.record_dealloc(front, layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let front = self.front();
        let new_ptr = self.allocate_block(front, new_layout);
        if new_ptr.is_null() {
            return ptr::null_mut();
        }

        unsafe {
            ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
            self.release_block(front, ptr, layout);
        }
        self.record_realloc(front, layout.size(), new_size);
        new_ptr
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
            // The kernel heap's own per-processor front is sized here
            // too, and for the same reason: this is the first point at
            // which the heap can allocate the array and the processor
            // count is known. Every allocation before it goes straight
            // to the shared heap.
            ALLOCATOR.configure_processors(processor_count);
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
        let allocator = KernelAllocator::<HEAP_ORDER>::empty();
        let mut heap = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
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

    /// A processor with a front serves its small allocations out of it:
    /// the block a free left in the magazine is the block the next
    /// allocation of that class gets, and the shared heap never sees
    /// either.
    #[test]
    fn a_cached_class_is_served_out_of_this_processors_magazine() {
        let allocator = KernelAllocator::<HEAP_ORDER>::empty();
        let mut heap = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
        let start = heap.0.as_mut_ptr() as usize;
        unsafe {
            allocator.add_to_heap(start, start + TEST_HEAP_BYTES);
        }
        allocator.configure_processors(1);

        let layout = Layout::from_size_align(64, 8).expect("valid allocation layout");
        test_support::as_processor(ProcessorId::new(0), || {
            let first = unsafe { GlobalAlloc::alloc(&allocator, layout) };
            assert!(!first.is_null());
            // The miss behind it stocked a batch, so the heap is
            // already holding blocks out for this processor.
            assert!(allocator.stats().magazine_cached_bytes > 0);

            unsafe { GlobalAlloc::dealloc(&allocator, first, layout) };
            let second = unsafe { GlobalAlloc::alloc(&allocator, layout) };
            assert_eq!(second, first, "the free left its block in the magazine");
            unsafe { GlobalAlloc::dealloc(&allocator, second, layout) };
        });

        let stats = allocator.stats();
        assert_eq!(stats.allocation_count, 2);
        assert_eq!(stats.deallocation_count, 2);
        assert_eq!(stats.requested_live_bytes, 0);
        assert!(
            stats.magazine_cached_bytes <= stats.allocated_bytes,
            "cached bytes are a share of what the heap has handed out"
        );
    }

    /// An allocation past the largest cached class goes straight to the
    /// shared heap, and comes back to it.
    #[test]
    fn an_uncached_class_never_touches_a_magazine() {
        let allocator = KernelAllocator::<HEAP_ORDER>::empty();
        let mut heap = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
        let start = heap.0.as_mut_ptr() as usize;
        unsafe {
            allocator.add_to_heap(start, start + TEST_HEAP_BYTES);
        }
        allocator.configure_processors(1);

        let layout = Layout::from_size_align(4096, 8).expect("valid allocation layout");
        test_support::as_processor(ProcessorId::new(0), || {
            let ptr = unsafe { GlobalAlloc::alloc(&allocator, layout) };
            assert!(!ptr.is_null());
            assert_eq!(allocator.stats().magazine_cached_bytes, 0);
            unsafe { GlobalAlloc::dealloc(&allocator, ptr, layout) };
            assert_eq!(allocator.stats().magazine_cached_bytes, 0);
        });

        assert_eq!(allocator.stats().requested_live_bytes, 0);
    }

    /// A processor that cannot name a slot allocates and frees through
    /// the shared heap, and its counters still balance.
    #[test]
    fn an_unslotted_processor_still_allocates_and_counts() {
        let allocator = KernelAllocator::<HEAP_ORDER>::empty();
        let mut heap = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
        let start = heap.0.as_mut_ptr() as usize;
        unsafe {
            allocator.add_to_heap(start, start + TEST_HEAP_BYTES);
        }
        allocator.configure_processors(1);

        let layout = Layout::from_size_align(64, 8).expect("valid allocation layout");
        let ptr = unsafe { GlobalAlloc::alloc(&allocator, layout) };
        assert!(!ptr.is_null());
        assert_eq!(
            allocator.stats().magazine_cached_bytes,
            0,
            "an allocation that named no slot cached nothing"
        );
        unsafe { GlobalAlloc::dealloc(&allocator, ptr, layout) };

        let stats = allocator.stats();
        assert_eq!(stats.allocation_count, 1);
        assert_eq!(stats.deallocation_count, 1);
        assert_eq!(stats.requested_live_bytes, 0);
    }

    /// A block allocated before the front existed is freed into a
    /// magazine afterwards without unbalancing the heap's own byte
    /// accounting, because every allocation of a cached class is served
    /// under the class layout whether a magazine is there or not.
    #[test]
    fn a_block_from_before_bring_up_can_be_freed_into_a_magazine() {
        let allocator = KernelAllocator::<HEAP_ORDER>::empty();
        let mut heap = Box::new(AlignedHeap([0; TEST_HEAP_BYTES]));
        let start = heap.0.as_mut_ptr() as usize;
        unsafe {
            allocator.add_to_heap(start, start + TEST_HEAP_BYTES);
        }

        let layout = Layout::from_size_align(48, 8).expect("valid allocation layout");
        let ptr = unsafe { GlobalAlloc::alloc(&allocator, layout) };
        assert!(!ptr.is_null());
        let allocated_before = allocator.stats().allocated_bytes;

        allocator.configure_processors(1);
        test_support::as_processor(ProcessorId::new(0), || {
            unsafe { GlobalAlloc::dealloc(&allocator, ptr, layout) };
            let taken = unsafe { GlobalAlloc::alloc(&allocator, layout) };
            assert_eq!(taken, ptr, "the magazine served the block back");
            unsafe { GlobalAlloc::dealloc(&allocator, taken, layout) };
        });

        // The block is in the magazine, so the heap still counts it as
        // handed out — and counts exactly the one block, not a rounded
        // one that would drift the accounting.
        let stats = allocator.stats();
        assert_eq!(stats.magazine_cached_bytes, allocated_before);
        assert_eq!(stats.requested_live_bytes, 0);
    }
}
