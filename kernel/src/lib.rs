#![no_std]
#![cfg_attr(target_os = "none", feature(alloc_error_handler))]
#![allow(hidden_glob_reexports)]
extern crate alloc;
extern crate self as helios_kernel;
#[cfg(not(target_os = "none"))]
extern crate std;

mod bootfs;
mod child_io;
mod compaction;
mod component_cache;
mod component_fs;
mod component_fs_path;
mod component_resources;
mod component_runtime;
mod component_runtime_backend;
mod component_types;
mod descriptor_table;
mod embedded_component;
mod embedded_init;
mod embedded_program;
mod entropy_pool;
mod executor;
mod frame_slab;
mod futex_table;
mod host_fs_client;
mod host_share;
mod instance;
mod kernel_exception;
mod log;
mod network_control;
mod network_service;
mod observer;
mod pmm;
mod poll_registry;
mod process_authority;
mod process_table;
mod program_service;
mod recording_console;
mod runtime_state;
mod runtime_types;
mod serial_transport;
mod socket_stack;
mod sync;
mod task;
mod thread_table;
mod time;
mod timer;
mod unsupported_host_fs;
mod user_memory;
#[cfg(feature = "wasmtime-runtime")]
pub(crate) mod wasmtime_adapter;

#[cfg(all(
    target_os = "none",
    any(
        feature = "wasmtime-aarch64",
        feature = "wasmtime-riscv64",
        feature = "wasmtime-x86"
    )
))]
pub mod runtime_memory {
    //! Re-export of the runtime custom-virtual-memory dispatcher
    //! so bare-metal backends can install their `RuntimeMemoryHooks`
    //! tables without reaching into kernel-private modules.
    pub use crate::wasmtime_adapter::custom_vm::{
        RuntimeMemoryHooks, RuntimeMemoryImage, default_memory_image_free,
        default_memory_image_map_at, default_memory_image_new, default_page_size, install_hooks,
    };
}
pub use bootfs::{
    BootDirectory, BootDirectoryEntry, BootDirectoryHandleExt, BootFile, EmbeddedBootFile,
    EmbeddedBootFs,
};
pub use child_io::{ByteReadWait, ByteReader, ByteWriter, ClosedPeer, TryRead, byte_channel};
pub use compaction::{
    CompactionBudget, CompactionPolicy, CompactionReport, CompactionTarget, Compactor,
    PressureLevel,
};
pub(crate) use component_cache::ComponentCache;
pub use component_fs::{
    ComponentFsNodeKind, ComponentFsResourceError, ComponentResourceTableError,
    map_resource_table_error,
};
pub use component_fs_path::{
    ComponentFsPathError, directory_prefix, parent_path, resolve_absolute_path, resolve_child_path,
    resolve_guest_path,
};
pub use component_resources::{
    ComponentRawMutex, ComponentRawMutexGuard, ComponentRawRwLock, ComponentRawRwLockReadGuard,
    ComponentRawRwLockWriteGuard, ComponentSerialPort, ComponentTcpBackend, ComponentTcpStream,
    ComponentUdpBackend, ComponentUdpSocket,
};
pub use component_runtime::{
    ComponentOutputMode, ComponentOutputStreamKind, ComponentRuntimeState, ComponentStoreData,
    DeadlinePollable, InstanceKilled, wait_until_runtime_deadline,
};
pub use component_runtime_backend::{
    CompiledComponent, ComponentExecContext, ComponentExecutor, ComponentExitStatus,
    ComponentRunResult, ComponentRuntimeEngine, ComponentRuntimeFactory, ComponentWorld,
};
pub use component_types::{
    RawMutexGuardResource, RawMutexResource, RawRwLockReadGuardResource, RawRwLockResource,
    RawRwLockWriteGuardResource, SerialPortResource, TcpStreamResource, UdpSocketResource,
};
pub use descriptor_table::{DescriptorEntry, DescriptorId, DescriptorTable, DescriptorTableError};
pub use embedded_component::EmbeddedComponent;
pub use embedded_init::{
    EmbeddedInit, embedded_boot_component, embedded_init, embedded_system_component,
    has_embedded_system_component,
};
pub use embedded_program::EmbeddedProgram;
pub use entropy_pool::{EntropyError, EntropyPool};
pub use executor::{JoinHandle, LocalJoinHandle, Spawner};
pub use futex_table::{
    FutexKey, FutexTable, FutexWaitRegistration, GuestAddress, ProcessMemoryIdentity,
};
pub use helios_hal::Platform;
pub use host_fs_client::{HostFsClient, HostFsTransport};
pub use host_share::{HOST_SHARE_GUEST_MOUNT_PATH, HOST_SHARE_MOUNT_TAG, guest_host_share_path};
pub use instance::{
    DEFAULT_RESTART_COST, InstanceExecutionTransition, InstanceId, InstanceProfileTotal,
    InstanceRegistry, InstanceSnapshot, KillReason, OomVictim, PLUGIN_RESTART_COST,
    RegisteredInstance, SYSTEM_COMPONENT_RESTART_COST, allow_instance_resource_growth,
    record_instance_transition,
};
pub use kernel_exception::{
    KernelException, KernelExceptionCause, KernelExceptionDispatch, KernelNativeTrapHandler,
};
pub use network_control::{
    Ipv4Cidr, Ipv4Route, MacAddress, NetworkAdminBackend, NetworkBridgeRequest,
    NetworkBridgeSecurity, NetworkControl, NetworkControlError, NetworkPortId,
};
pub use network_service::{NetworkDevice, NetworkService, TcpListenerId, TcpStreamId, UdpSocketId};
pub use observer::{
    DEFAULT_PROFILE_STACK_CAPACITY, DEFAULT_TRACE_HISTORY_CAPACITY, FoldedProfileSample,
    ProfileFilter, ProfileHistory, ProfileScope, StatsSample, TraceEvent, TraceField, TraceFilter,
    TraceHistory, TraceLevel, TraceValue, matches_profile_filter, matches_trace_filter,
    parse_console_text,
};
pub use pmm::KernelPhysFrameAllocator;
pub use poll_registry::{
    PollKey, PollRegistration, PollRegistry, PollRegistryError, PollSourceKind,
};
pub use process_authority::{
    ClockAuthorityRights, DirectoryAuthorityRights, DirectoryCap, DirectoryPreopen, DnsCap,
    ExecAuthority, ForkAuthority, JoinAuthority, LinkAuthorityRights, LinkSourceCap,
    LinkTargetDirectoryCap, MulticastCap, NetworkAdminCap, NetworkAuthorityRights, NetworkCap,
    PrivilegedBindCap, ProcessAuthority, ProcessAuthorityError, ProcessAuthorityRights,
    SetWallClockCap, SignalAuthority, SpawnAuthority, SymlinkCreateCap, SymlinkReadCap, TcpCap,
    TerminalAuthorityRights, TerminalInputCap, TerminalOutputCap, TtyControlCap, UdpCap,
};
pub use process_table::{ProcessId, ProcessRecord, ProcessState, ProcessTable, ProcessTableError};
pub use program_service::{
    ProgramExecError, ProgramExecErrorDetail, ProgramExecErrorKind, ProgramOutOfMemory,
};
pub use recording_console::RecordingConsole;
pub use runtime_state::RuntimeState;
pub use runtime_types::{
    AuthorityDomain, ComponentHostFilesystemState, ComponentNetworkService, ComponentNetworkState,
    DnsError, DnsErrorKind, ExecOutput, ExecResult, HostDirEntry, HostFileSystem, HostFsError,
    HostFsErrorKind, HostMetadata, Ipv4Address, NetworkErrorDetail, ObjectIdentity, PingError,
    PingErrorKind, PingReply, TcpAccepted, TcpError, TcpErrorKind, TcpListener, UdpBinding,
    UdpDatagram, UdpError, UdpErrorKind,
};
pub use serial_transport::{
    emit_serial_error_marker, emit_serial_stage_marker, read_serial, try_read_serial, write_serial,
};
pub use socket_stack::SocketStack;
pub use sync::{
    Mutex, MutexGuard, Notified, Notify, NotifyWaiter, OwnedRawMutexLease, OwnedRawRwLockReadLease,
    OwnedRawRwLockWriteLease, RawMutex, RawMutexLease, RawRwLock, RawRwLockReadLease,
    RawRwLockWriteLease, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
pub use task::{YieldNow, yield_now};
pub use thread_table::{ThreadId, ThreadRecord, ThreadState, ThreadTable, ThreadTableError};
pub use time::{
    KernelClock, duration_to_ticks, elapsed_millis, monotonic_nanos, nanos_to_ticks_ceil_saturating,
};
pub use timer::{Sleep, Timer};
pub use unsupported_host_fs::UnsupportedHostFileSystem;
pub use user_memory::{
    UserHeapStats, allocate_user_frame_zeroed, deallocate_user_frame, user_heap_stats,
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
use core::future::Future;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use core::task::{Context, Poll, Waker};
use core::time::Duration;

use buddy_system_allocator::LockedHeap;
use executor::Executor;
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::memory::MemoryRegion;
use helios_hal::watchdog::{NoWatchdog, ProgressCounter, Watchdog};
use helios_hal::{DeviceInventory, DmaModel, ProcessorStartupPolicy, ProcessorTopology};

const HEAP_ORDER: usize = 32;
const BOOT_UNINITIALIZED: u8 = 0;
const BOOT_INITIALIZING: u8 = 1;
const BOOT_READY: u8 = 2;
const WATCHDOG_CHECK_DIVISOR: u32 = 4;
#[cfg(helios_watchdog_self_test)]
const WATCHDOG_SELF_TEST_DELAY_MILLIS_ENV: &str = env!("HELIOS_WATCHDOG_SELF_TEST_DELAY_MS");

#[cfg_attr(target_os = "none", global_allocator)]
static ALLOCATOR: LockedHeap<HEAP_ORDER> = LockedHeap::empty();
static BOOT_STATE: AtomicU8 = AtomicU8::new(BOOT_UNINITIALIZED);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapStats {
    pub total_bytes: usize,
    pub allocated_bytes: usize,
}

impl HeapStats {
    pub fn available_bytes(self) -> usize {
        self.total_bytes.saturating_sub(self.allocated_bytes)
    }
}

const USER_MEMORY_KERNEL_RESERVE_FRACTION: usize = 4;
const USER_MEMORY_MIN_KERNEL_RESERVE_BYTES: usize = 32 * 1024 * 1024;
const USER_HEAP_REGION_FRACTION: usize = 2;
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
        let mut progress = 0;

        loop {
            let fired = self.timer.fire_expired();
            let ran = self.executor.run_until_stalled();

            if fired == 0 && ran == 0 {
                return progress;
            }

            progress += fired + ran;
            if ran == executor::READY_BATCH_TASKS || progress >= executor::READY_BATCH_TASKS {
                return progress;
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
        self.cpu.wake_processor(self.owner_processor);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.notified.store(true, Ordering::Release);
        self.cpu.wake_processor(self.owner_processor);
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

fn init_allocator<Regions>(memory_regions: Regions) -> &'static user_memory::UserMemoryPool
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
            ALLOCATOR.lock().add_to_heap(start, kernel_end);
        }
        let pool = *user_pool.get_or_insert_with(|| {
            user_memory::install_user_memory_pool(user_memory::allocate_user_memory_pool())
        });
        if let Some(user_start) = user_start {
            pool.add_region(user_start, end);
        }
    }
    user_pool.unwrap_or_else(|| panic!("bootstrap did not provide memory for user pool"))
}

fn split_bootstrap_memory_region(start: usize, end: usize) -> (usize, Option<usize>) {
    let len = end.saturating_sub(start);
    if len < USER_HEAP_MIN_REGION_BYTES * USER_HEAP_REGION_FRACTION {
        return (end, None);
    }

    let user_len = len / USER_HEAP_REGION_FRACTION;
    let user_start = align_down(end.saturating_sub(user_len), USER_HEAP_MIN_REGION_BYTES);
    if user_start <= start || end.saturating_sub(user_start) < USER_HEAP_MIN_REGION_BYTES {
        return (end, None);
    }
    (user_start, Some(user_start))
}

const fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

pub fn prime_bootstrap_allocator<Regions>(
    memory_regions: Regions,
) -> &'static user_memory::UserMemoryPool
where
    Regions: IntoIterator<Item = MemoryRegion>,
{
    match BOOT_STATE.compare_exchange(
        BOOT_UNINITIALIZED,
        BOOT_INITIALIZING,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => init_allocator(memory_regions),
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
    prime_bootstrap_allocator(memory_regions);
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
    tracing::info!(
        "Platform dma_model={dma_model:?} debug_serial={} network={} block_devices={} host_share={}",
        devices.has_debug_serial,
        devices.has_network,
        devices.block_device_count,
        devices.has_host_share
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
    let allocator = ALLOCATOR.lock();
    HeapStats {
        total_bytes: allocator.stats_total_bytes(),
        allocated_bytes: allocator.stats_alloc_actual(),
    }
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
