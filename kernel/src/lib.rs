#![no_std]
#![allow(hidden_glob_reexports)]
extern crate alloc;
extern crate self as helios_kernel;
#[cfg(not(target_os = "none"))]
extern crate std;

mod block_on;
mod bootfs;
mod child_io;
mod codegen_platform;
mod component_cache;
mod component_fs;
mod component_fs_path;
mod component_host;
mod component_resources;
mod component_runtime;
mod component_runtime_backend;
mod component_types;
mod compute_pool;
mod embedded_component;
mod embedded_init;
mod embedded_program;
mod executor;
mod host_fs_client;
mod host_share;
mod instance;
mod log;
mod observer;
mod program;
mod program_service;
mod recording_console;
mod runtime_state;
mod runtime_types;
mod serial_transport;
mod sync;
mod task;
mod unsupported_host_fs;
mod time;
mod timer;
mod wasi_rights;
pub(crate) mod wasmtime_adapter;

pub use block_on::block_on;
pub use child_io::{ByteReader, ByteWriter, ClosedPeer, TryRead, byte_channel};
pub use codegen_platform::CodegenPlatform;
pub use bootfs::{
    BootDirectory, BootDirectoryEntry, BootDirectoryHandleExt, BootFile, EmbeddedBootFile,
    EmbeddedBootFs,
};
pub(crate) use component_cache::ComponentCache;
pub use component_fs::{
    ComponentFsNodeKind, ComponentFsResourceError, ComponentResourceTableError,
    map_resource_table_error,
};
pub use component_fs_path::{
    ComponentFsPathError, directory_prefix, parent_path, resolve_child_path,
};
pub use component_host::{
    ChildExit, ChildHandle, ComponentBindingSet, ComponentHostProcessorRole, ComponentRunMode,
    HostRuntimeState, ProgramServiceConfig, UserProgramService, component_host_processor_role,
    component_host_processors_to_start, component_host_system_processor,
    component_host_worker_count, install_component_host_program_service, install_program_service,
    install_program_service_with_config, run_component_host_processor_forever,
    run_embedded_component_forever, run_embedded_component_in_mode_forever,
    run_program_workers_forever, system_component_should_run_on,
};
pub use component_resources::{
    ComponentRawMutex, ComponentRawMutexGuard, ComponentRawRwLock, ComponentRawRwLockReadGuard,
    ComponentRawRwLockWriteGuard, ComponentSerialPort, ComponentTcpBackend, ComponentTcpStream,
};
pub use component_runtime::{
    ComponentOutputMode, ComponentOutputStreamKind, ComponentRuntimeState, ComponentStoreData,
    DeadlinePollable,
};
pub use component_runtime_backend::{
    CompiledComponent, ComponentExecContext, ComponentExecutor, ComponentExitStatus,
    ComponentRunResult, ComponentRuntimeEngine, ComponentRuntimeFactory, ComponentWorld,
};
pub use component_types::{
    RawMutexGuardResource, RawMutexResource, RawRwLockReadGuardResource, RawRwLockResource,
    RawRwLockWriteGuardResource, SerialPortResource, TcpStreamResource,
};
pub use compute_pool::{ComputePool, ComputePriority, SpawnError as ComputeSpawnError};
pub use embedded_component::EmbeddedComponent;
pub use embedded_init::{
    EmbeddedInit, embedded_boot_component, embedded_init, embedded_system_component,
    has_embedded_system_component,
};
pub use embedded_program::EmbeddedProgram;
pub use executor::{JoinHandle, LocalJoinHandle, Spawner};
pub use helios_hal::Platform;
pub use host_fs_client::{HostFsClient, HostFsFuture, HostFsTransport};
pub use host_share::{HOST_SHARE_GUEST_MOUNT_PATH, HOST_SHARE_MOUNT_TAG, guest_host_share_path};
pub use instance::{
    InstanceExecutionTransition, InstanceId, InstanceRegistry, InstanceSnapshot,
    RegisteredInstance, allow_instance_resource_growth, record_instance_transition,
};
pub use observer::{
    DEFAULT_TRACE_HISTORY_CAPACITY, StatsSample, TraceEvent, TraceField, TraceFilter, TraceHistory,
    TraceLevel, TraceValue, matches_trace_filter, parse_console_text,
};
pub use recording_console::RecordingConsole;
pub use program::{
    Blueprint, CompileError as ProgramCompileError, ExitCode, ProgramRuntime,
    ProgramRuntimeBackend, ProgramRuntimeConfig, ProgramRuntimeDriver, ProgramRuntimeDriverBackend,
    ProgramRuntimeError, ProgramRuntimeInitError, ProgramTask, ResourceTable,
    RunError as ProgramRunError, Task,
};
pub use program_service::{ProgramExecError, ProgramExecErrorKind, ProgramService};
pub use runtime_state::RuntimeState;
pub use runtime_types::{
    ComponentHostFilesystemState, ComponentNetworkService, ComponentNetworkState,
    DynComponentNetworkService, DynamicNetworkService, ExecOutput, ExecResult, HostDirEntry,
    HostFileSystem, HostFsError, HostFsErrorKind, HostMetadata, Ipv4Address, PingError,
    PingErrorKind, PingReply, TcpError, TcpErrorKind, TcpStreamToken,
};
pub use serial_transport::{
    emit_serial_error_marker, emit_serial_stage_marker, read_serial, try_read_serial,
    write_serial,
};
pub use sync::{
    Mutex, MutexGuard, Notified, Notify, OwnedRawMutexLease, OwnedRawRwLockReadLease,
    OwnedRawRwLockWriteLease, RawMutex, RawMutexLease, RawRwLock, RawRwLockReadLease,
    RawRwLockWriteLease, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
pub use task::{YieldNow, yield_now};
pub use unsupported_host_fs::UnsupportedHostFileSystem;
pub use wasi_rights::WasiRights;
pub use time::{elapsed_millis, monotonic_nanos};
pub use timer::{Sleep, Timer};
// Wasmtime-specific helpers are crate-internal only.
// External consumers use the ComponentRuntimeFactory trait.

use core::sync::atomic::{AtomicU8, Ordering};
use core::time::Duration;

use buddy_system_allocator::LockedHeap;
use executor::Executor;
use helios_hal::cpu::{Cpu, Instant, ProcessorId};
use helios_hal::memory::MemoryRegion;

const HEAP_ORDER: usize = 32;
const BOOT_UNINITIALIZED: u8 = 0;
const BOOT_INITIALIZING: u8 = 1;
const BOOT_READY: u8 = 2;

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

pub struct Kernel<CpuImpl: Cpu + Clone> {
    cpu: CpuImpl,
    executor: Executor,
    timer: Timer<CpuImpl>,
}

impl<CpuImpl: Cpu + Clone> Kernel<CpuImpl> {
    pub fn spawner(&self) -> Spawner<CpuImpl> {
        self.executor.spawner(self.cpu.clone())
    }

    pub fn timer(&self) -> Timer<CpuImpl> {
        self.timer.clone()
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
        }
    }

    pub fn run(&self) -> ! {
        loop {
            if self.run_until_stalled() != 0 {
                continue;
            }

            self.cpu.park_current();
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
    let Platform {
        console,
        cpu,
        memory_regions,
    } = platform;
    let current_processor = cpu.current_processor();

    if current_processor == cpu.bootstrap_processor() {
        match BOOT_STATE.load(Ordering::Acquire) {
            BOOT_UNINITIALIZED => bootstrap_init(console, memory_regions, &cpu),
            BOOT_INITIALIZING => finish_bootstrap(console, &cpu),
            state => panic!("bootstrap processor observed invalid boot state {state}"),
        }
    } else {
        wait_for_bootstrap(&cpu);
    }

    let kernel = Kernel {
        timer: Timer::new(cpu.clone()),
        cpu,
        executor: Executor::new(),
    };

    let processor_id = current_processor.id();
    kernel.spawn_detached(async move {
        tracing::info!("Processor online processor={processor_id}");
    });

    kernel
}

fn init_allocator<Regions>(memory_regions: Regions)
where
    Regions: IntoIterator<Item = MemoryRegion>,
{
    for mut region in memory_regions {
        let region = unsafe { region.as_mut() };
        let start = region.as_mut_ptr() as usize;
        let end = start + region.len();
        unsafe {
            ALLOCATOR.lock().add_to_heap(start, end);
        }
    }
}

pub fn prime_bootstrap_allocator<Regions>(memory_regions: Regions)
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
) where
    Console: core::fmt::Write + Send + 'static,
    CpuImpl: Cpu,
    Regions: IntoIterator<Item = MemoryRegion>,
{
    prime_bootstrap_allocator(memory_regions);
    finish_bootstrap(console, cpu);
}

fn finish_bootstrap<Console, CpuImpl>(console: Console, cpu: &CpuImpl)
where
    Console: core::fmt::Write + Send + 'static,
    CpuImpl: Cpu,
{
    log::init_logger(console);
    tracing::info!(
        "Kernel initialized on bootstrap processor={}",
        cpu.bootstrap_processor().id()
    );
    tracing::info!("Kernel is ready\n\n{}", include_str!("welcome.txt"));

    BOOT_STATE.store(BOOT_READY, Ordering::Release);

    for processor in 0..cpu.processor_count() {
        let processor = ProcessorId::new(processor as u16);
        if processor != cpu.bootstrap_processor() {
            cpu.start_processor(processor);
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
