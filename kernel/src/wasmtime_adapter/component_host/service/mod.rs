mod argv;
pub(crate) use argv::ProgramArgv;
mod shared_memory;
use shared_memory::*;
mod memory;
use memory::*;
mod descriptor;
use descriptor::*;
mod compiler;
use compiler::*;
mod exec;
use exec::*;
mod http_plugin;
use http_plugin::*;
mod preview1;
use preview1::*;
mod wasix_proc;
use wasix_proc::*;
mod wasix_thread;
use wasix_thread::*;
mod wasix_net;
use wasix_net::*;
mod wasix_epoll;
use wasix_epoll::*;

use super::*;
use crate::process::FreeDescriptorSlots;
use crate::wasmtime_adapter::artifact_profile::{self, ArtifactProfileError};
use crate::wasmtime_adapter::config::AotCompileHint;
use crate::wasmtime_adapter::cwasm::{self, ArtifactTrustError, UntrustedCwasm};
use crate::wasmtime_adapter::{WasmtimeCompiledComponent, WasmtimeCompiledCoreModule};
use crate::wasmtime_adapter::{
    WasmtimePrecompiledKind,
    wasi::{
        DebugFileSystem, DebugFileSystemSnapshot, FsDescriptor, FsNodeKind, WasiImportSet,
        bindings::filesystem::types as fs_types, preview1 as p1,
    },
};
use crate::{ProgramExecErrorDetail, RuntimeMessage};
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use bytes::Bytes;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering as AtomicOrdering};
use core::task::{Context, Poll};
use core::time::Duration;
use hashbrown::HashMap;
use helios_compiler_abi::{
    CompileHint as CompilerAbiHint, CompilerRequestHeader, CompilerResponseHeader, CompilerStatus,
    HELIOS_COMPILER_ABI_VERSION, HELIOS_COMPILER_ALLOC, HELIOS_COMPILER_COMPILE,
    HELIOS_COMPILER_INITIALIZE, HELIOS_COMPILER_PTHREAD_SELF_OFFSET,
};
use helios_hal::watchdog::Watchdog;
use smallvec::SmallVec;
use thiserror::Error;
use wasmtime::component::{Component, InstancePre as ComponentInstancePre};
use wasmtime::{
    Caller, ExternType, InstancePre, Linker as CoreLinker, MemoryType, Module, SharedMemory, Val,
};

const COMPILER_PLUGIN_PATH: &str = "/bin/compiler.cwasm";
const HELIOS_PROCESS_ID_ENV: &str = "HELIOS_PROCESS_ID";
const RAYON_NUM_THREADS_ENV: &[u8] = b"RAYON_NUM_THREADS=";
const WASM_PAGE_SIZE: usize = 64 * 1024;
const PROGRAM_SHARED_MEMORY_MAX_PAGES: u32 = 8192;
const SHARED_MEMORY_POOL_FRACTION: usize = 16;
const WASIX_ASYNCIFY_DATA_SIZE: u32 = 8;
const WASIX_STACK_SNAPSHOT_SIZE: usize = 24;
const WASIX_THREAD_START_SIZE: usize = 64;
const WASIX_NO_PENDING_SIGNAL: u32 = u32::MAX;
const WASIX_MODULE: &str = "wasix_32v1";
const WASIX_NULL_DEVICE_PATH: &str = "/dev/null";
const DEFAULT_WASIX_SOCKET_BUFFER_BYTES: u64 = 64 * 1024;
const DEFAULT_WASIX_SOCKET_LOW_WATER_BYTES: u64 = 1;
const DEFAULT_WASIX_SOCKET_TTL: u64 = helios_netstack::DEFAULT_HOP_LIMIT as u64;
/// Ceiling for `SO_RCVBUF`: the receive window the netstack really reserves.
/// A larger request is clamped so a read-back reports the effective size.
const WASIX_SOCKET_RECEIVE_BUFFER_CEILING: u64 = helios_netstack::TCP_RECEIVE_WINDOW_BYTES as u64;
/// Ceiling for `SO_SNDBUF`: the netstack's per-socket transmit buffer.
const WASIX_SOCKET_SEND_BUFFER_CEILING: u64 = helios_netstack::TCP_TRANSMIT_BUFFER_BYTES as u64;
const DEFAULT_WASIX_SOCKET_MULTICAST_TTL: u64 = 1;
const WASIX_IPPROTO_TCP: u64 = 6;
const WASIX_IPPROTO_UDP: u64 = 17;
const WASIX_IPPROTO_TCP_I32: i32 = 6;
const WASIX_IPPROTO_UDP_I32: i32 = 17;
const WASIX_STREAM_SECURITY_UNENCRYPTED: u8 = 1 << 0;
const WASIX_STREAM_SECURITY_ANY_ENCRYPTION: u8 = 1 << 1;
const WASIX_STREAM_SECURITY_CLASSIC_ENCRYPTION: u8 = 1 << 2;
const WASIX_STREAM_SECURITY_DOUBLE_ENCRYPTION: u8 = 1 << 3;
const DEFAULT_WASIX_EXEC_SEARCH_PATHS: &[&str] = &["/usr/local/bin", "/bin", "/usr/bin"];
type Preview1Iovs = SmallVec<[(u32, u32); 8]>;
type Preview1IovRanges = SmallVec<[(usize, usize); 8]>;
type CompilerThreadTasks = SmallVec<[crate::JoinHandle<()>; 8]>;

const WASIX_PROC_SPAWN_FD_OP_SIZE: u32 = 56;
const WASIX_PROC_SPAWN_FD_OP_CMD_OFFSET: u32 = 0;
const WASIX_PROC_SPAWN_FD_OP_FD_OFFSET: u32 = 4;
const WASIX_PROC_SPAWN_FD_OP_SRC_FD_OFFSET: u32 = 8;
const WASIX_PROC_SPAWN_FD_OP_PATH_OFFSET: u32 = 12;
const WASIX_PROC_SPAWN_FD_OP_PATH_LEN_OFFSET: u32 = 16;
const WASIX_PROC_SPAWN_FD_OP_DIRFLAGS_OFFSET: u32 = 20;
const WASIX_PROC_SPAWN_FD_OP_OFLAGS_OFFSET: u32 = 24;
const WASIX_PROC_SPAWN_FD_OP_RIGHTS_BASE_OFFSET: u32 = 32;
const WASIX_PROC_SPAWN_FD_OP_FDFLAGS_OFFSET: u32 = 48;
const WASIX_PROC_SPAWN_FD_OP_FDFLAGSEXT_OFFSET: u32 = 50;
const WASIX_PROC_SPAWN_FD_OP_CLOSE: u8 = 0;
const WASIX_PROC_SPAWN_FD_OP_DUP2: u8 = 1;
const WASIX_PROC_SPAWN_FD_OP_OPEN: u8 = 2;
const WASIX_PROC_SPAWN_FD_OP_CHDIR: u8 = 3;
const WASIX_PROC_SPAWN_FD_OP_FCHDIR: u8 = 4;
const WASIX_SIGNAL_DISPOSITION_SIZE: u32 = 2;
const WASIX_SIGNAL_DISPOSITION_SIGNAL_OFFSET: u32 = 0;
const WASIX_SIGNAL_DISPOSITION_ACTION_OFFSET: u32 = 1;
const WASIX_SIGNAL_DISPOSITION_DEFAULT: u8 = 0;
const WASIX_SIGNAL_DISPOSITION_IGNORE: u8 = 1;

fn system_component_profile_stack(component_name: &str) -> String {
    let mut stack = String::with_capacity(
        "kernel;system-component;".len() + component_name.len() + ";poll".len(),
    );
    stack.push_str("kernel;system-component;");
    stack.push_str(component_name);
    stack.push_str(";poll");
    stack
}

fn kernel_processor_profile_stack(processor: u16) -> String {
    let mut stack = String::from("kernel;executor;processor-");
    write!(stack, "{processor}").expect("profile stack formatting should not fail");
    stack
}

#[derive(Clone)]
pub struct UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    inner: Arc<UserProgramServiceInner<CpuImpl, HostFs>>,
}

/// The program a launch runs and the credentials it runs under.
///
/// Distinct from [`ProgramExecContext`], which is the host the program
/// runs *in*, and from [`ChildStdio`], which is how the caller talks to
/// it once it is running.
pub(super) struct ProgramLaunch {
    argv: ProgramArgv,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    filesystem: Option<DebugFileSystemSnapshot>,
    /// Inherited preview1 descriptors. Only core modules have a
    /// descriptor table; a component ignores one.
    descriptors: Option<Preview1DescriptorTable>,
    signal_dispositions: Vec<WasixSignalDisposition>,
}

impl ProgramLaunch {
    /// A launch that inherits nothing: no descriptors, no signal
    /// dispositions.
    pub(super) fn new(
        argv: ProgramArgv,
        env: Vec<(String, String)>,
        authority: ProcessAuthority,
        filesystem: Option<DebugFileSystemSnapshot>,
    ) -> Self {
        Self {
            argv,
            env,
            authority,
            filesystem,
            descriptors: None,
            signal_dispositions: Vec::new(),
        }
    }
}

/// The ends of a child's standard streams that stay with the parent.
pub(super) struct ChildStdio {
    pub(super) stdin: Option<crate::ByteWriter>,
    pub(super) stdout: Option<crate::ByteReader>,
    pub(super) stderr: Option<crate::ByteReader>,
}

#[derive(Clone)]
pub(crate) struct ProgramExecContext<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    cpu: CpuImpl,
    timer: crate::Timer<CpuImpl>,
    spawner: crate::Spawner<CpuImpl>,
    runtime_state: HostRuntimeState<CpuImpl, HostFs>,
    instance_registry: crate::InstanceRegistry,
    parent_instance_id: Option<crate::InstanceId>,
    read_serial: crate::SerialReader,
    write_serial: crate::DebugSerialWriter,
}

impl<CpuImpl, HostFs> ProgramExecContext<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(crate) fn spawner(&self) -> crate::Spawner<CpuImpl> {
        self.spawner.clone()
    }
}

struct UserProgramServiceInner<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    runtime: crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    engine: crate::wasmtime_adapter::WasmtimeEngine,
    preview1_core_linker: CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
    shared_memory_pool: Arc<Mutex<SharedMemoryPool>>,
    component_cache: Mutex<ComponentCache<WasmtimeCompiledComponent>>,
    component_instance_pre_cache:
        Mutex<ComponentCache<ComponentInstancePre<StoreData<CpuImpl, HostFs>>>>,
    core_module_cache: Mutex<ComponentCache<WasmtimeCompiledCoreModule>>,
    // Release AArch64/HVF quickjs-loop evidence: caching the Preview1 core
    // InstancePre moved the median from 53 ms to 50-51 ms. Cache only modules
    // whose imports are independent of per-process shared-memory binding.
    core_module_instance_pre_cache: CoreModuleInstancePreCache<CpuImpl, HostFs>,
    compiler_artifact: Option<Bytes>,
    /// Lazily-built compiler kernel-plugin runtime. The plugin's
    /// `wasmtime::Module`, `InstancePre`, and 512 MiB `SharedMemory` are
    /// allocated on first compile and reused for every subsequent call,
    /// turning the plugin into a long-lived kernel resident — no more
    /// per-call buddy-heap churn that previously OOM'd after one compile.
    compiler_plugin: Mutex<Option<Arc<CompilerPluginRuntime<CpuImpl, HostFs>>>>,
    /// Serialises compile calls. The cached `SharedMemory` is the
    /// plugin's only scratch surface; concurrent calls would race on
    /// the bump allocator and corrupt each other's request/response
    /// buffers. The lock is held only on the kernel side; the rayon
    /// worker pool inside the plugin still parallelises a single
    /// compile across all cores.
    compile_in_progress: AtomicBool,
    clock_cpu: CpuImpl,
    _marker: core::marker::PhantomData<fn() -> HostFs>,
}

struct ProgramSpawnRequest {
    argv: ProgramArgv,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    filesystem: Option<DebugFileSystemSnapshot>,
    descriptors: Option<Preview1DescriptorTable>,
    signal_state: WasixSignalState,
    signal_dispositions: Vec<WasixSignalDisposition>,
}

pub(crate) enum ProgramSource {
    RawWasm(Bytes),
    SignedArtifact(Bytes),
    BootfsArtifact(Bytes),
}

/// Handle to a spawned child component as seen by the kernel and its
/// direct Rust callers. WIT `child` resources wrap one of these.
pub struct ChildHandle {
    pub instance_id: crate::InstanceId,
    signal_state: WasixSignalState,
    stdin: Option<crate::ByteWriter>,
    stdout: Option<crate::ByteReader>,
    stderr: Option<crate::ByteReader>,
    exit: Option<futures::channel::oneshot::Receiver<Result<ChildExit, ProgramExecError>>>,
}

impl ChildHandle {
    fn signal_state(&self) -> WasixSignalState {
        self.signal_state.clone()
    }

    /// Take the writer end of the child's stdin. Dropping it delivers
    /// EOF to the child.
    pub fn take_stdin(&mut self) -> Option<crate::ByteWriter> {
        self.stdin.take()
    }

    /// Take the reader end of the child's stdout stream.
    pub fn take_stdout(&mut self) -> Option<crate::ByteReader> {
        self.stdout.take()
    }

    /// Take the reader end of the child's stderr stream.
    pub fn take_stderr(&mut self) -> Option<crate::ByteReader> {
        self.stderr.take()
    }

    /// Take the exit-status receiver so an external driver (e.g. the WIT
    /// host `wait` impl that runs while the child resource is borrowed)
    /// can await it without consuming the handle.
    pub fn take_wait(
        &mut self,
    ) -> Option<futures::channel::oneshot::Receiver<Result<ChildExit, ProgramExecError>>> {
        self.exit.take()
    }

    /// Await child exit. Consumes the handle.
    pub async fn wait(mut self) -> Result<ChildExit, ProgramExecError> {
        match self.exit.take() {
            Some(rx) => match rx.await {
                Ok(result) => result,
                Err(_) => Err(ProgramExecError {
                    kind: ProgramExecErrorKind::Internal,
                    detail: ProgramExecErrorDetail::ChildExitChannelDropped,
                }),
            },
            None => Err(ProgramExecError {
                kind: ProgramExecErrorKind::Internal,
                detail: ProgramExecErrorDetail::ChildExitAlreadyConsumed,
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChildExit {
    pub instance_id: crate::InstanceId,
    pub exit_code: u32,
    filesystem: Option<DebugFileSystemSnapshot>,
}

pub fn install_program_service<CpuImpl, HostFs, WatchdogImpl>(
    kernel: &crate::Kernel<CpuImpl, WatchdogImpl>,
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
    read_serial: crate::SerialReader,
    write_serial: crate::DebugSerialWriter,
) -> UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    install_program_service_inner(kernel, cpu, debug_state, read_serial, write_serial)
}

pub fn install_component_host_program_service<CpuImpl, HostFs, WatchdogImpl>(
    kernel: &crate::Kernel<CpuImpl, WatchdogImpl>,
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
    read_serial: crate::SerialReader,
    write_serial: crate::DebugSerialWriter,
) -> Option<UserProgramService<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    let topology = kernel.topology();
    if cpu.current_processor() != topology.bootstrap_processor {
        return None;
    }

    Some(install_program_service_inner(
        kernel,
        cpu,
        debug_state,
        read_serial,
        write_serial,
    ))
}

fn install_program_service_inner<CpuImpl, HostFs, WatchdogImpl>(
    kernel: &crate::Kernel<CpuImpl, WatchdogImpl>,
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
    read_serial: crate::SerialReader,
    write_serial: crate::DebugSerialWriter,
) -> UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    if let Some(service) = debug_state.program_service() {
        return service;
    }

    let available_bytes = heap_stats().available_bytes();
    let cache_budget = available_bytes / COMPONENT_CACHE_FRACTION;
    let shared_memory_pool_budget =
        user_heap_stats().available_bytes() / SHARED_MEMORY_POOL_FRACTION;
    let runtime = crate::wasmtime_adapter::WasmtimeComponentRuntime::new(cpu.clone());
    let engine = <crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl> as crate::ComponentRuntimeFactory<CpuImpl, HostRuntimeState<CpuImpl, HostFs>, HostFs>>::create_engine(&runtime)
        .unwrap_or_else(|error| panic!("failed to create launched-program engine: {error:#}"));
    let preview1_core_linker = preview1_program_linker(engine.raw())
        .unwrap_or_else(|error| panic!("failed to create preview1 program linker: {error:#}"));
    let compiler_artifact = read_bootfs_artifact(debug_state, COMPILER_PLUGIN_PATH);
    crate::wasmtime_adapter::register_oom_kick_engine(engine.raw().clone());
    debug_state
        .instance_registry()
        .set_kill_notifier(crate::wasmtime_adapter::bump_user_engine_epoch);
    let service = UserProgramService {
        inner: Arc::new(UserProgramServiceInner {
            runtime,
            engine,
            preview1_core_linker,
            shared_memory_pool: Arc::new(Mutex::new(SharedMemoryPool::new(
                shared_memory_pool_budget,
            ))),
            component_cache: Mutex::new(ComponentCache::new(cache_budget)),
            component_instance_pre_cache: Mutex::new(ComponentCache::new(cache_budget)),
            core_module_cache: Mutex::new(ComponentCache::new(cache_budget)),
            core_module_instance_pre_cache: Arc::new(Mutex::new(ComponentCache::new(cache_budget))),
            compiler_artifact,
            compiler_plugin: Mutex::new(None),
            compile_in_progress: AtomicBool::new(false),
            clock_cpu: cpu.clone(),
            _marker: core::marker::PhantomData,
        }),
    };
    debug_state.install_program_service(service.clone());
    install_http_client_plugin(
        &service,
        ProgramExecContext {
            cpu: cpu.clone(),
            timer: kernel.timer(),
            spawner: kernel.spawner(),
            runtime_state: debug_state.clone(),
            instance_registry: debug_state.instance_registry(),
            parent_instance_id: None,
            read_serial,
            write_serial,
        },
    );
    service
}

pub fn run_embedded_component_forever<CpuImpl, HostFs, WatchdogImpl>(
    component: EmbeddedComponent,
    world: ComponentBindingSet,
    cpu: CpuImpl,
    kernel: &crate::Kernel<CpuImpl, WatchdogImpl>,
    debug_state: HostRuntimeState<CpuImpl, HostFs>,
    read_serial: crate::SerialReader,
    write_serial: crate::DebugSerialWriter,
) -> !
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    let component_name = component.name();
    write_serial.emit_stage_marker("boot");
    write_serial.emit_stage_marker("component-host:trace-begin");
    tracing::info!(
        component = component_name,
        "launching embedded system component"
    );
    write_serial.emit_stage_marker("component-host:run-local-begin");
    let stack = system_component_profile_stack(component_name);
    kernel
        .run_local_future(ProfiledSystemComponentFuture::new(
            run_system_component(
                component,
                world,
                super::SystemComponentHost {
                    cpu: cpu.clone(),
                    timer: kernel.timer(),
                    spawner: kernel.spawner(),
                    debug_state: debug_state.clone(),
                    read_serial,
                    write_serial,
                },
            ),
            cpu.clone(),
            debug_state,
            stack,
        ))
        .unwrap_or_else(|error| {
            write_serial
                .emit_error_marker("error failed-system-component", format_args!("{error:#}"));
            panic!("failed to exec embedded system component:\n{error:#}");
        });
    write_serial.emit_stage_marker("done");
    tracing::info!(
        component = component_name,
        "embedded system component exited cleanly"
    );
    cpu.shutdown()
}

struct ProfiledSystemComponentFuture<CpuImpl, HostFs, Fut>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    inner: Fut,
    cpu: CpuImpl,
    debug_state: HostRuntimeState<CpuImpl, HostFs>,
    stack: String,
}

impl<CpuImpl, HostFs, Fut> ProfiledSystemComponentFuture<CpuImpl, HostFs, Fut>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new(
        inner: Fut,
        cpu: CpuImpl,
        debug_state: HostRuntimeState<CpuImpl, HostFs>,
        stack: String,
    ) -> Self {
        Self {
            inner,
            cpu,
            debug_state,
            stack,
        }
    }
}

impl<CpuImpl, HostFs, Fut> Future for ProfiledSystemComponentFuture<CpuImpl, HostFs, Fut>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
    Fut: Future,
{
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `inner` is pinned in place with `Self`; this projection never moves it.
        let this = unsafe { self.get_unchecked_mut() };
        let started = this.cpu.now().ticks();
        // SAFETY: `inner` has not been moved after `Self` was pinned.
        let result = unsafe { Pin::new_unchecked(&mut this.inner) }.poll(cx);
        if this.debug_state.profiling_enabled() {
            this.debug_state.record_profile_stack_str(
                ProfileScope::Kernel,
                &this.stack,
                this.cpu.now().ticks().saturating_sub(started),
            );
        }
        result
    }
}

pub fn run_program_workers_forever<CpuImpl, HostFs, WatchdogImpl>(
    _cpu: CpuImpl,
    kernel: crate::Kernel<CpuImpl, WatchdogImpl>,
    _debug_state: HostRuntimeState<CpuImpl, HostFs>,
) -> !
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    kernel.run()
}

pub fn run_component_host_processor_forever<CpuImpl, HostFs, WatchdogImpl>(
    cpu: CpuImpl,
    kernel: crate::Kernel<CpuImpl, WatchdogImpl>,
    debug_state: HostRuntimeState<CpuImpl, HostFs>,
    read_serial: crate::SerialReader,
    write_serial: crate::DebugSerialWriter,
) -> !
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    let topology = kernel.topology();
    match component_host_processor_role(
        cpu.current_processor(),
        topology.configured_processors,
        topology.bootstrap_processor,
    ) {
        ComponentHostProcessorRole::Kernel => {
            run_kernel_processor_forever(cpu, kernel, debug_state);
        }
        ComponentHostProcessorRole::SharedRuntime | ComponentHostProcessorRole::SystemComponent => {
            match crate::embedded_system_component() {
                Some(component) => run_embedded_component_forever(
                    component,
                    ComponentBindingSet::System,
                    cpu,
                    &kernel,
                    debug_state,
                    read_serial,
                    write_serial,
                ),
                None => {
                    // A build that embeds the debugger must also embed
                    // the system component that launches it, so an empty
                    // slot there is a packaging error rather than the
                    // ordinary no-component boot.
                    if cfg!(feature = "embedded-debugger") {
                        panic!("embedded init bootfs is missing the system component");
                    }
                    run_kernel_processor_forever(cpu, kernel, debug_state);
                }
            }
        }
    }
}

fn run_kernel_processor_forever<CpuImpl, HostFs, WatchdogImpl>(
    cpu: CpuImpl,
    kernel: crate::Kernel<CpuImpl, WatchdogImpl>,
    debug_state: HostRuntimeState<CpuImpl, HostFs>,
) -> !
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    let processor = cpu.current_processor().id();
    let stack = kernel_processor_profile_stack(processor);
    loop {
        let progress = if debug_state.profiling_enabled() {
            let started = cpu.now().ticks();
            let counters = cpu.hardware_perf_counters();
            let stats = kernel.run_until_stalled_with_stats();
            let progress = stats.progress_count();
            if progress != 0 {
                record_executor_metrics(&cpu, &debug_state, &stack, started, counters, stats);
            }
            progress
        } else {
            kernel.run_until_stalled_with_stats().progress_count()
        };

        if progress != 0 {
            continue;
        }

        cpu.park_current();
    }
}

fn record_executor_metrics<CpuImpl, HostFs>(
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
    stack: &str,
    started: u64,
    counters: helios_hal::cpu::HardwarePerfCounters,
    stats: crate::KernelRunStats,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let progress = stats.progress_count();
    let ended = cpu.now().ticks();
    let elapsed = ended.saturating_sub(started);
    debug_state.record_profile_stack_str(ProfileScope::Kernel, stack, elapsed);
    let elapsed_nanos = debug_state
        .uptime_nanos(ended)
        .saturating_sub(debug_state.uptime_nanos(started));
    let counter_delta = cpu.hardware_perf_counters().delta_since(counters);
    debug_state.record_perf_metric_parts(
        ProfileScope::Kernel,
        "kernel;executor;",
        "run",
        crate::PerfSample {
            events: usize_to_u64(progress, "executor progress count"),
            elapsed_nanos,
            counters: counter_delta,
            bytes: 0,
        },
    );
    record_executor_event_metric(
        debug_state,
        "local-runnable",
        stats.executor_local_runnable_count,
    );
    record_executor_event_metric(
        debug_state,
        "global-runnable",
        stats.executor_global_runnable_count,
    );
    record_executor_event_metric(
        debug_state,
        "local-empty-pop",
        stats.executor_local_empty_pop_count,
    );
    record_executor_event_metric(
        debug_state,
        "global-empty-pop",
        stats.executor_global_empty_pop_count,
    );
    record_executor_event_metric(debug_state, "timer-fired", stats.timer_fired_count);
    debug_state.record_kernel_heap_metrics(crate::heap_stats());
}

fn record_executor_event_metric<CpuImpl, HostFs>(
    runtime_state: &HostRuntimeState<CpuImpl, HostFs>,
    name: &'static str,
    events: usize,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if events == 0 {
        return;
    }
    runtime_state.record_perf_metric_parts(
        ProfileScope::Kernel,
        "kernel;executor;",
        name,
        crate::PerfSample {
            events: usize_to_u64(events, "executor event count"),
            elapsed_nanos: 0,
            counters: helios_hal::cpu::HardwarePerfCounterDelta::default(),
            bytes: 0,
        },
    );
}

fn usize_to_u64(value: usize, label: &'static str) -> u64 {
    u64::try_from(value).unwrap_or_else(|_| panic!("{label} does not fit into u64"))
}

fn record_program_kernel_profile<CpuImpl, HostFs>(
    runtime_state: &HostRuntimeState<CpuImpl, HostFs>,
    cpu: &CpuImpl,
    phase: &'static str,
    started_ticks: u64,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if runtime_state.profiling_enabled() {
        runtime_state.record_profile_stack_parts(
            ProfileScope::Kernel,
            "kernel;program;",
            phase,
            cpu.now().ticks().saturating_sub(started_ticks),
        );
    }
}

fn record_named_program_kernel_profile<CpuImpl, HostFs>(
    runtime_state: &HostRuntimeState<CpuImpl, HostFs>,
    cpu: &CpuImpl,
    phase: &'static str,
    name: &str,
    started_ticks: u64,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if runtime_state.profiling_enabled() {
        let mut stack = String::with_capacity("kernel;program;;".len() + phase.len() + name.len());
        stack.push_str("kernel;program;");
        stack.push_str(phase);
        stack.push(';');
        stack.push_str(name);
        runtime_state.record_profile_stack_str(
            ProfileScope::Kernel,
            &stack,
            cpu.now().ticks().saturating_sub(started_ticks),
        );
    }
}

struct ProgramKernelProfile<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    runtime_state: HostRuntimeState<CpuImpl, HostFs>,
    cpu: CpuImpl,
    started_ticks: u64,
    counters: helios_hal::cpu::HardwarePerfCounters,
    started_heap: crate::HeapStats,
}

fn start_program_kernel_profile<CpuImpl, HostFs>(
    runtime_state: &HostRuntimeState<CpuImpl, HostFs>,
    cpu: &CpuImpl,
) -> Option<ProgramKernelProfile<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    runtime_state
        .profiling_enabled()
        .then(|| ProgramKernelProfile {
            runtime_state: runtime_state.clone(),
            cpu: cpu.clone(),
            started_ticks: cpu.now().ticks(),
            counters: cpu.hardware_perf_counters(),
            started_heap: crate::heap_stats(),
        })
}

fn record_program_kernel_profile_sample<CpuImpl, HostFs>(
    profile: Option<ProgramKernelProfile<CpuImpl, HostFs>>,
    phase: &'static str,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Some(profile) = profile {
        let ended_ticks = profile.cpu.now().ticks();
        let elapsed_ticks = ended_ticks.saturating_sub(profile.started_ticks);
        profile.runtime_state.record_profile_stack_parts(
            ProfileScope::Kernel,
            "kernel;program;",
            phase,
            elapsed_ticks,
        );
        let elapsed_nanos = profile
            .runtime_state
            .uptime_nanos(ended_ticks)
            .saturating_sub(profile.runtime_state.uptime_nanos(profile.started_ticks));
        let counter_delta = profile
            .cpu
            .hardware_perf_counters()
            .delta_since(profile.counters);
        profile.runtime_state.record_perf_metric_parts(
            ProfileScope::Kernel,
            "kernel;program;",
            phase,
            crate::PerfSample {
                events: 1,
                elapsed_nanos,
                counters: counter_delta,
                bytes: 0,
            },
        );

        let heap = crate::heap_stats();
        record_program_heap_delta(
            &profile.runtime_state,
            phase,
            "heap-alloc",
            heap.allocation_count
                .saturating_sub(profile.started_heap.allocation_count),
            heap.total_allocation_bytes
                .saturating_sub(profile.started_heap.total_allocation_bytes),
        );
        record_program_heap_delta(
            &profile.runtime_state,
            phase,
            "heap-realloc",
            heap.reallocation_count
                .saturating_sub(profile.started_heap.reallocation_count),
            heap.total_reallocation_bytes
                .saturating_sub(profile.started_heap.total_reallocation_bytes),
        );
        record_program_heap_delta(
            &profile.runtime_state,
            phase,
            "heap-dealloc",
            heap.deallocation_count
                .saturating_sub(profile.started_heap.deallocation_count),
            heap.total_deallocation_bytes
                .saturating_sub(profile.started_heap.total_deallocation_bytes),
        );
    }
}

fn record_program_heap_delta<CpuImpl, HostFs>(
    runtime_state: &HostRuntimeState<CpuImpl, HostFs>,
    phase: &'static str,
    kind: &'static str,
    events: u64,
    bytes: u64,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if events == 0 && bytes == 0 {
        return;
    }
    let phase_prefix = match kind {
        "heap-alloc" => "kernel;program-heap;alloc;",
        "heap-realloc" => "kernel;program-heap;realloc;",
        "heap-dealloc" => "kernel;program-heap;dealloc;",
        _ => panic!("unknown program heap metric kind {kind}"),
    };
    runtime_state.record_perf_metric_parts(
        ProfileScope::Kernel,
        phase_prefix,
        phase,
        crate::PerfSample {
            events,
            elapsed_nanos: 0,
            counters: helios_hal::cpu::HardwarePerfCounterDelta::default(),
            bytes,
        },
    );
}

impl<CpuImpl, HostFs> UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub fn increment_epoch(&self) {
        self.inner.engine.increment_epoch();
    }

    /// Spawn a new child program. The returned handle gives the caller
    /// direct access to the child's stdin/stdout/stderr channels and a
    /// future resolving with its exit status.
    pub(super) async fn spawn(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        source: ProgramSource,
        hint: Option<AotCompileHint>,
        launch: ProgramLaunch,
    ) -> Result<ChildHandle, ProgramExecError> {
        super::emit_program_stage_marker(exec_context.write_serial, "program:spawn-begin");
        let executable = self
            .load_executable(&exec_context, &source, hint, exec_context.write_serial)
            .await?;
        self.spawn_loaded(exec_context, executable, launch)
    }

    /// Spawn a child whose stdio the caller has already wired up.
    ///
    /// A preview1 descriptor table only means anything to a core
    /// module, so one supplied alongside a component is dropped here
    /// rather than at every call site.
    async fn spawn_with_output_mode(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        source: ProgramSource,
        hint: Option<AotCompileHint>,
        mut launch: ProgramLaunch,
        output_mode: OutputMode,
        stdio: ChildStdio,
    ) -> Result<ChildHandle, ProgramExecError> {
        super::emit_program_stage_marker(exec_context.write_serial, "program:spawn-begin");
        let executable = self
            .load_executable(&exec_context, &source, hint, exec_context.write_serial)
            .await?;
        if matches!(executable, ProgramExecutable::Component(_)) {
            launch.descriptors = None;
        }
        self.spawn_loaded_with_output_mode(exec_context, executable, launch, output_mode, stdio)
    }

    fn spawn_loaded(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        executable: ProgramExecutable<CpuImpl, HostFs>,
        launch: ProgramLaunch,
    ) -> Result<ChildHandle, ProgramExecError> {
        let (stdin_writer, stdin_reader) = crate::byte_channel();
        let (stdout_writer, stdout_reader) = crate::byte_channel();
        let (stderr_writer, stderr_reader) = crate::byte_channel();
        self.spawn_loaded_with_output_mode(
            exec_context,
            executable,
            launch,
            OutputMode::Child {
                stdin_rx: stdin_reader,
                stdout_tx: stdout_writer,
                stderr_tx: stderr_writer,
            },
            ChildStdio {
                stdin: Some(stdin_writer),
                stdout: Some(stdout_reader),
                stderr: Some(stderr_reader),
            },
        )
    }

    fn spawn_loaded_with_output_mode(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        executable: ProgramExecutable<CpuImpl, HostFs>,
        launch: ProgramLaunch,
        output_mode: OutputMode,
        stdio: ChildStdio,
    ) -> Result<ChildHandle, ProgramExecError> {
        let ProgramLaunch {
            argv,
            mut env,
            authority,
            filesystem,
            descriptors,
            signal_dispositions,
        } = launch;
        let started_at = exec_context
            .runtime_state
            .uptime_nanos(exec_context.cpu.now().ticks());
        let launched_instance = exec_context
            .instance_registry
            .register(argv.program_name().to_owned(), started_at);
        let instance_id = launched_instance.id();
        assert!(
            !env.iter()
                .any(|(name, _)| name.as_str() == HELIOS_PROCESS_ID_ENV),
            "{HELIOS_PROCESS_ID_ENV} is reserved for the kernel program launcher"
        );
        env.push((HELIOS_PROCESS_ID_ENV.into(), instance_id.raw().to_string()));
        let signal_state = WasixSignalState::new();
        let request = ProgramSpawnRequest {
            argv,
            env,
            authority,
            filesystem,
            descriptors,
            signal_state: signal_state.clone(),
            signal_dispositions,
        };

        let (exit_tx, exit_rx) = futures::channel::oneshot::channel();
        let runtime = self.inner.runtime.clone();
        let engine = self.inner.engine.clone();
        let preview1_core_linker = self.inner.preview1_core_linker.clone();
        let shared_memory_pool = self.inner.shared_memory_pool.clone();
        let core_module_instance_pre_cache = self.inner.core_module_instance_pre_cache.clone();
        // Everything this instance runs — its own task and every WASI
        // future its store spawns later — is funded from the arena's
        // instance share. A spawn storm therefore walks that share
        // empty and is refused right here, instead of taking the
        // executor capacity the kernel needs for its own tasks.
        let spawner = exec_context
            .spawner
            .instance_spawner(crate::TaskFunding::Instance);
        let run_spawner = spawner.clone();
        let progress = spawner.progress_counter();

        let launched = spawner.try_spawn_local_detached(async move {
            let result = run_program_executable(
                exec_context,
                request.argv,
                request.env,
                request.authority,
                request.filesystem,
                request.descriptors,
                request.signal_state,
                request.signal_dispositions,
                run_spawner,
                progress,
                executable,
                &engine,
                &runtime,
                preview1_core_linker,
                shared_memory_pool,
                core_module_instance_pre_cache,
                launched_instance,
                output_mode,
            )
            .await;
            let _ = exit_tx.send(result);
        });
        if let Err(error) = launched {
            tracing::warn!(
                target: "helios_kernel::program",
                instance = ?instance_id,
                %error,
                "refused a program launch: the executor's instance task share is full"
            );
            return Err(ProgramExecError {
                kind: ProgramExecErrorKind::OutOfMemory,
                detail: ProgramExecErrorDetail::ExecutorTaskCapacityExhausted,
            });
        }

        let ChildStdio {
            stdin,
            stdout,
            stderr,
        } = stdio;
        let child = ChildHandle {
            instance_id,
            signal_state,
            stdin,
            stdout,
            stderr,
            exit: Some(exit_rx),
        };
        Ok(child)
    }

    /// Convenience wrapper: spawn a program, feed it `stdin`, drain its
    /// stdout and stderr into buffers, and return the collected output
    /// along with the exit code.
    pub(super) async fn exec_buffered(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        source: ProgramSource,
        hint: Option<AotCompileHint>,
        stdin: Vec<u8>,
        launch: ProgramLaunch,
    ) -> Result<ExecResult, ProgramExecError> {
        self.exec_buffered_with_snapshot(exec_context, source, hint, stdin, launch)
            .await
            .map(|(result, _)| result)
    }

    async fn exec_buffered_with_snapshot(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        source: ProgramSource,
        hint: Option<AotCompileHint>,
        stdin: Vec<u8>,
        launch: ProgramLaunch,
    ) -> Result<(ExecResult, Option<DebugFileSystemSnapshot>), ProgramExecError> {
        let executable = self
            .load_executable(&exec_context, &source, hint, exec_context.write_serial)
            .await?;
        self.exec_loaded_buffered(exec_context, executable, stdin, launch)
            .await
    }

    async fn exec_loaded_buffered(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        executable: ProgramExecutable<CpuImpl, HostFs>,
        stdin: Vec<u8>,
        launch: ProgramLaunch,
    ) -> Result<(ExecResult, Option<DebugFileSystemSnapshot>), ProgramExecError> {
        let mut child = self.spawn_loaded(exec_context, executable, launch)?;

        // Feed stdin in one shot, then close the writer to signal EOF.
        if let Some(writer) = child.take_stdin() {
            if !stdin.is_empty() {
                // The child may not have started draining yet, so this
                // waits for room rather than overrunning the channel.
                let _: Result<(), crate::ClosedPeer> = writer.write(stdin).await;
            }
            drop(writer);
        }

        let stdout_reader = child.take_stdout();
        let stderr_reader = child.take_stderr();

        let stdout_task = async move {
            let mut bytes = Vec::new();
            if let Some(reader) = stdout_reader {
                while let Some(chunk) = reader.read().await {
                    bytes.extend_from_slice(&chunk);
                }
            }
            bytes
        };
        let stderr_task = async move {
            let mut bytes = Vec::new();
            if let Some(reader) = stderr_reader {
                while let Some(chunk) = reader.read().await {
                    bytes.extend_from_slice(&chunk);
                }
            }
            bytes
        };

        let wait_task = child.wait();
        let (stdout, (stderr, exit)) =
            futures::future::join(stdout_task, futures::future::join(stderr_task, wait_task)).await;
        let exit = exit?;

        Ok(ExecResult {
            instance_id: exit.instance_id,
            exit_code: exit.exit_code,
            output: crate::ExecOutput { stdout, stderr },
        })
        .map(|result| (result, exit.filesystem))
    }

    pub(crate) async fn aot(
        &self,
        exec_context: &ProgramExecContext<CpuImpl, HostFs>,
        wasm: &Bytes,
        hint: AotCompileHint,
        profile: bool,
    ) -> Result<Vec<u8>, ProgramExecError> {
        self.compile_raw_component_to_signed_artifact(exec_context, wasm, hint, profile)
            .await
    }

    async fn load_executable(
        &self,
        exec_context: &ProgramExecContext<CpuImpl, HostFs>,
        source: &ProgramSource,
        hint: Option<AotCompileHint>,
        write_serial: crate::DebugSerialWriter,
    ) -> Result<ProgramExecutable<CpuImpl, HostFs>, ProgramExecError> {
        let started_at = monotonic_nanos(&self.inner.clock_cpu);
        let payload = match source {
            ProgramSource::SignedArtifact(bytes) => {
                if hint.is_some() {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidHint,
                        detail: ProgramExecErrorDetail::HintNotAllowedForPrecompiledArtifact,
                    });
                }
                trusted_signed_payload(bytes)?
            }
            ProgramSource::BootfsArtifact(bytes) => {
                if hint.is_some() {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidHint,
                        detail: ProgramExecErrorDetail::HintNotAllowedForPrecompiledArtifact,
                    });
                }
                trusted_bootfs_payload(bytes)?
            }
            ProgramSource::RawWasm(wasm) => {
                let _profile = artifact_profile::classify_raw_wasm(wasm)
                    .map_err(map_artifact_profile_error)?;
                let hint = hint.unwrap_or(AotCompileHint::Performance);
                let signed = self
                    .compile_raw_component_to_signed_artifact(exec_context, wasm, hint, false)
                    .await?;
                let signed = Bytes::from(signed);
                trusted_signed_payload(&signed)?
            }
        };
        self.load_precompiled_executable(payload, write_serial, started_at)
    }

    fn load_precompiled_executable(
        &self,
        payload: Bytes,
        write_serial: crate::DebugSerialWriter,
        started_at: u64,
    ) -> Result<ProgramExecutable<CpuImpl, HostFs>, ProgramExecError> {
        match WasmtimePrecompiledKind::detect(&payload) {
            Some(WasmtimePrecompiledKind::Component) => self
                .load_precompiled_component(payload, write_serial, started_at)
                .map(ProgramExecutable::Component),
            Some(WasmtimePrecompiledKind::CoreModule) => self
                .load_precompiled_core_module(payload, write_serial, started_at)
                .map(ProgramExecutable::CoreModule),
            None => Err(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::InvalidRuntimeArtifact,
            }),
        }
    }

    fn load_precompiled_component(
        &self,
        payload: Bytes,
        write_serial: crate::DebugSerialWriter,
        started_at: u64,
    ) -> Result<Arc<ComponentInstancePre<StoreData<CpuImpl, HostFs>>>, ProgramExecError> {
        let component = if let Some(component) = self.inner.component_cache.lock().get(&payload) {
            super::emit_program_stage_marker(write_serial, "program:deserialize-cache-hit");
            let now = monotonic_nanos(&self.inner.clock_cpu);
            tracing::debug!(
                target: "helios_component_host::program_host",
                phase = "deserialize-component",
                cache = "hit",
                cwasm_bytes = payload.len(),
                elapsed_ms = elapsed_millis(started_at, now),
                "program component deserialization cache hit"
            );
            component
        } else {
            super::emit_program_stage_marker(write_serial, "program:deserialize-begin");
            tracing::debug!(
                target: "helios_component_host::program_host",
                phase = "deserialize-component",
                cache = "miss",
                cwasm_bytes = payload.len(),
                "program component deserialization started"
            );
            match WasmtimePrecompiledKind::detect(&payload) {
                Some(WasmtimePrecompiledKind::Component) => {}
                Some(WasmtimePrecompiledKind::CoreModule) => {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::InvalidRuntimeArtifact,
                    });
                }
                None => {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::InvalidRuntimeArtifact,
                    });
                }
            }
            let compiled = self.deserialize_component(&payload)?;
            super::emit_program_stage_marker(write_serial, "program:deserialize-end");
            let component = Arc::new(compiled);
            let now = monotonic_nanos(&self.inner.clock_cpu);
            tracing::debug!(
                target: "helios_component_host::program_host",
                phase = "deserialize-component",
                cache = "miss",
                cwasm_bytes = payload.len(),
                elapsed_ms = elapsed_millis(started_at, now),
                "program component deserialized"
            );
            self.inner
                .component_cache
                .lock()
                .insert_if_missing(payload.clone(), component)
        };

        if let Some(instance_pre) = self.inner.component_instance_pre_cache.lock().get(&payload) {
            super::emit_program_stage_marker(write_serial, "program:instantiate-pre-cache-hit");
            return Ok(instance_pre);
        }

        super::emit_program_stage_marker(write_serial, "program:instantiate-pre-begin");
        let linker = super::component_linker(
            self.inner.engine.raw(),
            ComponentBindingSet::Program,
            &component.component,
        )
        .map_err(map_program_runtime_error)?;
        let instance_pre = Arc::new(
            linker
                .instantiate_pre(&component.component)
                .map_err(map_program_runtime_error)?,
        );
        super::emit_program_stage_marker(write_serial, "program:instantiate-pre-end");
        Ok(self
            .inner
            .component_instance_pre_cache
            .lock()
            .insert_if_missing(payload, instance_pre))
    }

    fn load_precompiled_core_module(
        &self,
        payload: Bytes,
        write_serial: crate::DebugSerialWriter,
        started_at: u64,
    ) -> Result<Arc<WasmtimeCompiledCoreModule>, ProgramExecError> {
        if let Some(module) = self.inner.core_module_cache.lock().get(&payload) {
            super::emit_program_stage_marker(write_serial, "program:deserialize-core-cache-hit");
            let now = monotonic_nanos(&self.inner.clock_cpu);
            tracing::debug!(
                target: "helios_component_host::program_host",
                phase = "deserialize-core-module",
                cache = "hit",
                cwasm_bytes = payload.len(),
                elapsed_ms = elapsed_millis(started_at, now),
                "program core module deserialization cache hit"
            );
            return Ok(module);
        }

        super::emit_program_stage_marker(write_serial, "program:deserialize-core-begin");
        let module = unsafe { Module::deserialize(self.inner.engine.raw(), payload.as_ref()) }
            .map_err(map_program_runtime_error)?;
        validate_preview1_program_module_imports(&module)?;
        super::emit_program_stage_marker(write_serial, "program:deserialize-core-end");
        let compiled = Arc::new(WasmtimeCompiledCoreModule {
            cache_key: payload.clone(),
            module,
        });
        let now = monotonic_nanos(&self.inner.clock_cpu);
        tracing::debug!(
            target: "helios_component_host::program_host",
            phase = "deserialize-core-module",
            cache = "miss",
            cwasm_bytes = payload.len(),
            elapsed_ms = elapsed_millis(started_at, now),
            "program core module deserialized"
        );
        Ok(self
            .inner
            .core_module_cache
            .lock()
            .insert_if_missing(payload, compiled))
    }

    fn deserialize_component(
        &self,
        payload: &[u8],
    ) -> Result<WasmtimeCompiledComponent, ProgramExecError> {
        let component = unsafe { Component::deserialize(self.inner.engine.raw(), payload) }
            .map_err(|error| {
                tracing::error!(?error, "program component deserialization failed");
                ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::RuntimeFailure(RuntimeMessage::of(&error)),
                }
            })?;
        let imports = WasiImportSet::from_component(self.inner.engine.raw(), &component);
        for import in imports.names() {
            artifact_profile::validate_component_import_name(import)
                .map_err(map_artifact_profile_error)?;
        }
        Ok(WasmtimeCompiledComponent { component })
    }

    async fn compile_raw_component_to_signed_artifact(
        &self,
        exec_context: &ProgramExecContext<CpuImpl, HostFs>,
        wasm: &Bytes,
        hint: AotCompileHint,
        profile: bool,
    ) -> Result<Vec<u8>, ProgramExecError> {
        let compiler_artifact = self.read_compiler_plugin_artifact(exec_context)?;
        let compiler_payload = trusted_bootfs_payload(&compiler_artifact)?;
        let payload = self
            .invoke_compiler_core_module(exec_context, compiler_payload, wasm, hint, profile)
            .await?;
        let signed =
            cwasm::sign_trusted_artifact_payload(&payload).map_err(map_artifact_trust_error)?;
        cwasm::verify_signed_artifact(UntrustedCwasm::new(&signed))
            .map_err(map_artifact_trust_error)?;
        Ok(signed)
    }

    async fn invoke_compiler_core_module(
        &self,
        exec_context: &ProgramExecContext<CpuImpl, HostFs>,
        compiler_payload: Bytes,
        wasm: &Bytes,
        hint: AotCompileHint,
        profile: bool,
    ) -> Result<Vec<u8>, ProgramExecError> {
        let _compile_guard = self.acquire_compiler_compile_slot().await;
        let result =
            self.invoke_compiler_inner(exec_context, &compiler_payload, wasm, hint, profile);
        self.await_compiler_thread_tasks().await;
        // Plugin supervisor: on a kill or fatal-state error, drop the
        // cached runtime so the next call rebuilds the Module +
        // SharedMemory + InstancePre from scratch. This is the
        // auto-restart path the user-spec calls for ("内核插件需要有
        // 自动重启的功能").
        if let Err(error) = &result
            && plugin_runtime_should_be_recycled(error)
        {
            tracing::warn!(
                target: "helios_kernel::supervisor",
                detail = %error.detail,
                "compiler plugin runtime invalidated; next compile rebuilds from scratch"
            );
            *self.inner.compiler_plugin.lock() = None;
        }
        result
    }

    async fn acquire_compiler_compile_slot(&self) -> CompilerCompileSlot<'_> {
        loop {
            if self
                .inner
                .compile_in_progress
                .compare_exchange(
                    false,
                    true,
                    AtomicOrdering::Acquire,
                    AtomicOrdering::Relaxed,
                )
                .is_ok()
            {
                return CompilerCompileSlot {
                    occupied: &self.inner.compile_in_progress,
                };
            }
            crate::yield_now().await;
        }
    }

    async fn await_compiler_thread_tasks(&self) {
        loop {
            let tasks = self
                .inner
                .compiler_plugin
                .lock()
                .as_ref()
                .map(|plugin| {
                    plugin
                        .shared
                        .thread_tasks
                        .lock()
                        .drain(..)
                        .collect::<CompilerThreadTasks>()
                })
                .unwrap_or_default();
            if tasks.is_empty() {
                break;
            }
            for task in tasks {
                task.await;
            }
        }
    }

    fn invoke_compiler_inner(
        &self,
        exec_context: &ProgramExecContext<CpuImpl, HostFs>,
        compiler_payload: &Bytes,
        wasm: &Bytes,
        hint: AotCompileHint,
        profile: bool,
    ) -> Result<Vec<u8>, ProgramExecError> {
        let plugin = self.ensure_compiler_plugin(exec_context, compiler_payload)?;
        let engine = self.inner.engine.raw();
        let started_at = exec_context
            .runtime_state
            .uptime_nanos(exec_context.cpu.now().ticks());
        let compiler_instance = exec_context.instance_registry.register_with_policy(
            "compiler-plugin",
            started_at,
            crate::OomPolicy::KernelPlugin,
        );
        let store_data = CompilerCoreStore {
            cpu: exec_context.cpu.clone(),
            spawner: exec_context.spawner.clone(),
            runtime_state: exec_context.runtime_state.clone(),
            instance: Arc::new(compiler_instance),
            shared: plugin.shared.clone(),
            preview1_descriptors: CompilerPreview1Descriptors::new(),
            write_serial: exec_context.write_serial,
            _marker: core::marker::PhantomData,
        };
        let mut store = wasmtime::Store::new(engine, store_data);
        configure_compiler_core_store(&mut store);
        let instance = plugin
            .instance_pre
            .instantiate(&mut store)
            .map_err(map_program_runtime_error)?;
        let tls_base = compiler_tls_base(&mut store, &instance)?;
        let pthread_self_offset = instance
            .get_typed_func::<(), i32>(&mut store, HELIOS_COMPILER_PTHREAD_SELF_OFFSET)
            .map_err(map_program_runtime_error)?
            .call(&mut store, ())
            .map_err(map_program_runtime_error)? as u32;
        let thread_pointer = tls_base
            .checked_add(pthread_self_offset)
            .ok_or(ProgramExecError {
                kind: ProgramExecErrorKind::Internal,
                detail: ProgramExecErrorDetail::CompilerThreadPointerOverflow,
            })?;
        let initialize = instance
            .get_typed_func::<i32, ()>(&mut store, HELIOS_COMPILER_INITIALIZE)
            .map_err(map_program_runtime_error)?;
        initialize
            .call(&mut store, thread_pointer as i32)
            .map_err(map_program_runtime_error)?;
        let alloc = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, HELIOS_COMPILER_ALLOC)
            .map_err(map_program_runtime_error)?;
        let compile = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, HELIOS_COMPILER_COMPILE)
            .map_err(map_program_runtime_error)?;
        let target = env!("HELIOS_BUILD_TARGET").as_bytes();
        let wasm_ptr = compiler_alloc(&mut store, &alloc, wasm.len(), 1)?;
        let target_ptr = compiler_alloc(&mut store, &alloc, target.len(), 1)?;
        let request_ptr = compiler_alloc(
            &mut store,
            &alloc,
            core::mem::size_of::<CompilerRequestHeader>(),
            core::mem::align_of::<CompilerRequestHeader>(),
        )?;
        write_shared_memory(store.data().memory(), wasm_ptr, wasm)?;
        write_shared_memory(store.data().memory(), target_ptr, target)?;
        let header = CompilerRequestHeader {
            abi_version: HELIOS_COMPILER_ABI_VERSION,
            hint: match hint {
                AotCompileHint::Fast => CompilerAbiHint::Fast,
                AotCompileHint::Balanced => CompilerAbiHint::Balanced,
                AotCompileHint::Performance => CompilerAbiHint::Performance,
            },
            wasm_ptr,
            wasm_len: wasm.len() as u32,
            target_ptr,
            target_len: target.len() as u32,
            flags: if profile {
                helios_compiler_abi::HELIOS_COMPILER_REQUEST_PROFILE
            } else {
                0
            },
        };
        write_shared_memory(store.data().memory(), request_ptr, unsafe {
            core::slice::from_raw_parts(
                core::ptr::addr_of!(header).cast::<u8>(),
                core::mem::size_of::<CompilerRequestHeader>(),
            )
        })?;
        let compile_started = store.data().cpu.now().ticks();
        let response_ptr = compile.call(
            &mut store,
            (
                request_ptr as i32,
                core::mem::size_of::<CompilerRequestHeader>() as i32,
            ),
        );
        let compile_elapsed = store
            .data()
            .cpu
            .now()
            .ticks()
            .saturating_sub(compile_started);
        store.data().record_user_ticks(compile_elapsed);
        let response_ptr = response_ptr.map_err(map_program_runtime_error)? as u32;
        let response = read_compiler_response(store.data().memory(), response_ptr)?;
        if response.abi_version != HELIOS_COMPILER_ABI_VERSION {
            return Err(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::CompilerAbiMismatch,
            });
        }
        let diagnostic = read_shared_memory(
            store.data().memory(),
            response.diagnostic_ptr,
            response.diagnostic_len,
        )?;
        if response.status != CompilerStatus::Ok {
            if !diagnostic.is_empty() {
                tracing::error!(
                    diagnostic = %core::str::from_utf8(&diagnostic).unwrap_or("<non-utf8 compiler diagnostic>"),
                    "compiler plugin rejected input"
                );
            }
            return Err(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::CompilerRejectedInput,
            });
        }
        if !diagnostic.is_empty() {
            store.data().write_serial.emit(&diagnostic);
        }
        read_shared_memory(
            store.data().memory(),
            response.precompiled_ptr,
            response.precompiled_len,
        )
    }

    fn read_compiler_plugin_artifact(
        &self,
        exec_context: &ProgramExecContext<CpuImpl, HostFs>,
    ) -> Result<Bytes, ProgramExecError> {
        self.inner
            .compiler_artifact
            .clone()
            .or_else(|| read_bootfs_artifact(&exec_context.runtime_state, COMPILER_PLUGIN_PATH))
            .ok_or(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidPath,
                detail: ProgramExecErrorDetail::CompilerUnavailable,
            })
    }

    /// Build the compiler kernel-plugin runtime on first compile, reuse
    /// it forever after. The cached `wasmtime::Module`, `InstancePre`
    /// and 512 MiB `SharedMemory` are stable across calls; per-call work
    /// drops to a fresh `wasmtime::Store` and `instance_pre.instantiate`.
    fn ensure_compiler_plugin(
        &self,
        exec_context: &ProgramExecContext<CpuImpl, HostFs>,
        compiler_payload: &Bytes,
    ) -> Result<Arc<CompilerPluginRuntime<CpuImpl, HostFs>>, ProgramExecError> {
        let mut slot = self.inner.compiler_plugin.lock();
        if let Some(plugin) = slot.as_ref() {
            return Ok(plugin.clone());
        }

        // The compiler plugin is a precompiled core module; a component
        // or anything else is not the artifact this builds a runtime for.
        if !matches!(
            WasmtimePrecompiledKind::detect(compiler_payload),
            Some(WasmtimePrecompiledKind::CoreModule)
        ) {
            return Err(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::CompilerPluginInvalid,
            });
        }

        let worker_threads = compiler_plugin_worker_threads(&exec_context.cpu);
        tracing::info!(
            worker_threads,
            "building cached compiler plugin runtime (one-time per kernel boot)"
        );

        let engine = self.inner.engine.raw();
        let module = unsafe { Module::deserialize(engine, compiler_payload.as_ref()) }
            .map_err(map_program_runtime_error)?;
        let shared_memory = compiler_shared_memory(engine, &module)?;
        let shared = Arc::new(CompilerCoreShared {
            memory: shared_memory.clone(),
            entropy: Mutex::new(crate::EntropyPool::derive(
                exec_context.runtime_state.root_entropy(),
                0,
            )),
            instance_pre: spin::Once::new(),
            next_thread_id: AtomicI32::new(0),
            thread_tasks: Mutex::new(Vec::new()),
        });
        let mut linker: CoreLinker<CompilerCoreStore<CpuImpl, HostFs>> = CoreLinker::new(engine);
        add_compiler_core_imports(&mut linker, shared_memory.clone())?;

        // `linker.define` requires an `AsContext<Data = T>`; build a
        // throwaway store solely to satisfy that bound. The store has no
        // role beyond `define_compiler_shared_memory`; its transient
        // `RegisteredInstance` (named "compiler-plugin-init") deregisters
        // on drop at the end of this method.
        let scratch_started_at = exec_context
            .runtime_state
            .uptime_nanos(exec_context.cpu.now().ticks());
        let scratch_instance = exec_context.instance_registry.register_with_policy(
            "compiler-plugin-init",
            scratch_started_at,
            crate::OomPolicy::KernelPlugin,
        );
        let scratch_store_data = CompilerCoreStore {
            cpu: exec_context.cpu.clone(),
            spawner: exec_context.spawner.clone(),
            runtime_state: exec_context.runtime_state.clone(),
            instance: Arc::new(scratch_instance),
            shared: shared.clone(),
            preview1_descriptors: CompilerPreview1Descriptors::new(),
            write_serial: exec_context.write_serial,
            _marker: core::marker::PhantomData,
        };
        let scratch_store = wasmtime::Store::new(engine, scratch_store_data);
        define_compiler_shared_memory(&mut linker, &scratch_store, &module, shared_memory)?;
        drop(scratch_store);

        let instance_pre = Arc::new(
            linker
                .instantiate_pre(&module)
                .map_err(map_program_runtime_error)?,
        );
        shared.instance_pre.call_once(|| instance_pre.clone());

        let _ = module; // InstancePre holds the Module via Arc internally.
        let plugin = Arc::new(CompilerPluginRuntime {
            instance_pre,
            shared,
        });
        *slot = Some(plugin.clone());
        Ok(plugin)
    }
}

impl<CpuImpl, HostFs> ProgramExecContext<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(crate) fn from_store(store: &StoreData<CpuImpl, HostFs>) -> Self {
        Self {
            cpu: store.cpu.clone(),
            timer: store.timer(),
            // The launch path funds a child instance from a fresh
            // instance share; what it needs from the parent is the
            // processor its tasks will live on.
            spawner: store.spawner().launch_spawner().clone(),
            runtime_state: store.runtime_state.clone(),
            instance_registry: store.instance_registry.clone(),
            parent_instance_id: Some(store.instance().id()),
            read_serial: store.serial_reader_fn(),
            write_serial: store.serial_writer(),
        }
    }
}

const P1_RIGHT_FD_DATASYNC: u64 = 1 << 0;
const P1_RIGHT_FD_READ: u64 = 1 << 1;
const P1_RIGHT_FD_SEEK: u64 = 1 << 2;
const P1_RIGHT_FD_FDSTAT_SET_FLAGS: u64 = 1 << 3;
const P1_RIGHT_FD_SYNC: u64 = 1 << 4;
const P1_RIGHT_FD_TELL: u64 = 1 << 5;
const P1_RIGHT_FD_WRITE: u64 = 1 << 6;
const P1_RIGHT_FD_ADVISE: u64 = 1 << 7;
const P1_RIGHT_FD_ALLOCATE: u64 = 1 << 8;
const P1_RIGHT_PATH_CREATE_DIRECTORY: u64 = 1 << 9;
const P1_RIGHT_PATH_CREATE_FILE: u64 = 1 << 10;
const P1_RIGHT_PATH_LINK_SOURCE: u64 = 1 << 11;
const P1_RIGHT_PATH_LINK_TARGET: u64 = 1 << 12;
const P1_RIGHT_PATH_OPEN: u64 = 1 << 13;
const P1_RIGHT_FD_READDIR: u64 = 1 << 14;
const P1_RIGHT_PATH_READLINK: u64 = 1 << 15;
const P1_RIGHT_PATH_RENAME_SOURCE: u64 = 1 << 16;
const P1_RIGHT_PATH_RENAME_TARGET: u64 = 1 << 17;
const P1_RIGHT_PATH_FILESTAT_GET: u64 = 1 << 18;
const P1_RIGHT_PATH_FILESTAT_SET_SIZE: u64 = 1 << 19;
const P1_RIGHT_PATH_FILESTAT_SET_TIMES: u64 = 1 << 20;
const P1_RIGHT_FD_FILESTAT_GET: u64 = 1 << 21;
const P1_RIGHT_FD_FILESTAT_SET_SIZE: u64 = 1 << 22;
const P1_RIGHT_FD_FILESTAT_SET_TIMES: u64 = 1 << 23;
const P1_RIGHT_PATH_SYMLINK: u64 = 1 << 24;
const P1_RIGHT_PATH_REMOVE_DIRECTORY: u64 = 1 << 25;
const P1_RIGHT_PATH_UNLINK_FILE: u64 = 1 << 26;
const P1_RIGHT_POLL_FD_READWRITE: u64 = 1 << 27;
const P1_FDFLAG_APPEND: u16 = 1 << 0;
const P1_FDFLAG_DSYNC: u16 = 1 << 1;
const P1_FDFLAG_NONBLOCK: u16 = 1 << 2;
const P1_FDFLAG_RSYNC: u16 = 1 << 3;
const P1_FDFLAG_SYNC: u16 = 1 << 4;
const P1_FILE_FDFLAGS: u16 =
    P1_FDFLAG_APPEND | P1_FDFLAG_DSYNC | P1_FDFLAG_NONBLOCK | P1_FDFLAG_RSYNC | P1_FDFLAG_SYNC;
const P1_SOCKET_FDFLAGS: u16 = P1_FDFLAG_NONBLOCK;
const WASIX_FDFLAGSEXT_CLOEXEC: u16 = 1 << 0;
const WASIX_EVENTFDFLAG_SEMAPHORE: u32 = 1 << 0;
const WASIX_OPTION_NONE: u8 = 0;
const WASIX_OPTION_SOME: u8 = 1;
const WASIX_OPTION_UNION_U32_OFFSET: u32 = 4;
const WASIX_OPTION_UNION_U64_OFFSET: u32 = 8;
const WASIX_STDIO_MODE_PIPED: i32 = 0;
const WASIX_STDIO_MODE_INHERIT: i32 = 1;
const WASIX_STDIO_MODE_NULL: i32 = 2;
const WASIX_STDIO_MODE_LOG: i32 = 3;
const WASIX_PROCESS_HANDLES_STDIN_OFFSET: u32 = 4;
const WASIX_PROCESS_HANDLES_STDOUT_OFFSET: u32 = 12;
const WASIX_PROCESS_HANDLES_STDERR_OFFSET: u32 = 20;
const WASIX_JOIN_FLAG_NON_BLOCKING: u32 = 1 << 0;
const WASIX_JOIN_FLAGS_SUPPORTED: u32 = WASIX_JOIN_FLAG_NON_BLOCKING | (1 << 1);
const WASIX_JOIN_STATUS_NOTHING: u8 = 0;
const WASIX_JOIN_STATUS_EXIT_NORMAL: u8 = 1;
const WASIX_JOIN_STATUS_UNION_OFFSET: u32 = 2;
const WASIX_SOCK_TYPE_STREAM: i32 = 1;
const WASIX_SOCK_TYPE_DGRAM: i32 = 2;
const WASIX_SOCK_STATUS_OPENED: u8 = 1;
const WASIX_SOCK_OPTION_REUSE_PORT: i32 = 1;
const WASIX_SOCK_OPTION_REUSE_ADDR: i32 = 2;
const WASIX_SOCK_OPTION_NO_DELAY: i32 = 3;
const WASIX_SOCK_OPTION_DONT_ROUTE: i32 = 4;
const WASIX_SOCK_OPTION_ONLY_V6: i32 = 5;
const WASIX_SOCK_OPTION_BROADCAST: i32 = 6;
const WASIX_SOCK_OPTION_MULTICAST_LOOP_V4: i32 = 7;
const WASIX_SOCK_OPTION_MULTICAST_LOOP_V6: i32 = 8;
const WASIX_SOCK_OPTION_PROMISCUOUS: i32 = 9;
const WASIX_SOCK_OPTION_LISTENING: i32 = 10;
const WASIX_SOCK_OPTION_KEEP_ALIVE: i32 = 12;
const WASIX_SOCK_OPTION_LINGER: i32 = 13;
const WASIX_SOCK_OPTION_OOB_INLINE: i32 = 14;
const WASIX_SOCK_OPTION_RECV_BUF_SIZE: i32 = 15;
const WASIX_SOCK_OPTION_SEND_BUF_SIZE: i32 = 16;
const WASIX_SOCK_OPTION_RECV_LOWAT: i32 = 17;
const WASIX_SOCK_OPTION_SEND_LOWAT: i32 = 18;
const WASIX_SOCK_OPTION_RECV_TIMEOUT: i32 = 19;
const WASIX_SOCK_OPTION_SEND_TIMEOUT: i32 = 20;
const WASIX_SOCK_OPTION_CONNECT_TIMEOUT: i32 = 21;
const WASIX_SOCK_OPTION_ACCEPT_TIMEOUT: i32 = 22;
const WASIX_SOCK_OPTION_TTL: i32 = 23;
const WASIX_SOCK_OPTION_MULTICAST_TTL_V4: i32 = 24;
const WASIX_SOCK_OPTION_TYPE: i32 = 25;
const WASIX_SOCK_OPTION_PROTO: i32 = 26;
const WASIX_RIFLAGS_DATA_TRUNCATED: u16 = 1 << 2;
const WASIX_ADDRESS_FAMILY_UNSPEC: u8 = 0;
const WASIX_ADDRESS_FAMILY_IP_INET4: u8 = 1;
const WASIX_ADDRESS_FAMILY_IP_INET6: u8 = 2;
const WASIX_ADDRESS_FAMILY_UNIX: u8 = 3;
const WASIX_ADDRESS_FAMILY_UNSPEC_I32: i32 = 0;
const WASIX_ADDRESS_FAMILY_IP_INET4_I32: i32 = 1;
const WASIX_ADDRESS_FAMILY_IP_INET6_I32: i32 = 2;
const WASIX_ADDRESS_FAMILY_UNIX_I32: i32 = 3;
const WASIX_ADDR_IP_UNION_OFFSET: u32 = 2;
const WASIX_ADDR_IP_SIZE: u32 = 18;
const WASIX_ADDR_CIDR_SIZE: u32 = 28;
const WASIX_ADDR_CIDR_UNION_OFFSET: u32 = 2;
const WASIX_ADDR_CIDR_IP4_ADDRESS_OFFSET: u32 = WASIX_ADDR_CIDR_UNION_OFFSET;
const WASIX_ADDR_CIDR_IP4_PREFIX_OFFSET: u32 = WASIX_ADDR_CIDR_IP4_ADDRESS_OFFSET + 4;
const WASIX_ADDR_PORT_UNION_OFFSET: u32 = 2;
const WASIX_ADDR_PORT_IP4_ADDRESS_OFFSET: u32 = 4;
/// `__wasi_addr_ip6_port_t` overlays `__wasi_addr_ip4_port_t` in the
/// `__wasi_addr_port_t` union: both are `{ u16 port; address }`, so the
/// address starts at the same offset and only its width differs.
const WASIX_ADDR_PORT_IP6_ADDRESS_OFFSET: u32 = WASIX_ADDR_PORT_IP4_ADDRESS_OFFSET;
const WASIX_ROUTE_SIZE: u32 = 176;
const WASIX_ROUTE_CIDR_OFFSET: u32 = 0;
const WASIX_ROUTE_ROUTER_OFFSET: u32 = 28;
const WASIX_ROUTE_PREFERRED_UNTIL_OFFSET: u32 = 144;
const WASIX_ROUTE_EXPIRES_AT_OFFSET: u32 = 160;
const WASIX_EPOLL_TYPE_EPOLLIN: u32 = 1 << 0;
const WASIX_EPOLL_TYPE_EPOLLOUT: u32 = 1 << 1;
const WASIX_EPOLL_TYPE_EPOLLERR: u32 = 1 << 4;
const WASIX_EPOLL_TYPE_EPOLLHUP: u32 = 1 << 5;
const WASIX_EPOLL_TYPE_EPOLLONESHOT: u32 = 1 << 7;
const WASIX_EPOLL_CTL_ADD: i32 = 0;
const WASIX_EPOLL_CTL_MOD: i32 = 1;
const WASIX_EPOLL_CTL_DEL: i32 = 2;
const WASIX_EPOLL_EVENT_SIZE: u32 = 32;
const WASIX_EPOLL_EVENT_PADDING_OFFSET: u32 = 4;
const WASIX_EPOLL_EVENT_DATA_OFFSET: u32 = 8;
const WASIX_EPOLL_EVENT_DATA_FD_OFFSET: u32 = 12;
const WASIX_EPOLL_EVENT_DATA1_OFFSET: u32 = 16;
const WASIX_EPOLL_EVENT_DATA_PADDING_OFFSET: u32 = 20;
const WASIX_EPOLL_EVENT_DATA2_OFFSET: u32 = 24;
const P1_FSTFLAG_ATIM: u16 = 1 << 0;
const P1_FSTFLAG_ATIM_NOW: u16 = 1 << 1;
const P1_FSTFLAG_MTIM: u16 = 1 << 2;
const P1_FSTFLAG_MTIM_NOW: u16 = 1 << 3;
const P1_EVENTTYPE_CLOCK: u8 = 0;
const P1_EVENTTYPE_FD_READ: u8 = 1;
const P1_EVENTTYPE_FD_WRITE: u8 = 2;
const P1_SDFLAGS_RD: u8 = 1 << 0;
const P1_SDFLAGS_WR: u8 = 1 << 1;
const P1_SDFLAGS_SUPPORTED: u8 = P1_SDFLAGS_RD | P1_SDFLAGS_WR;
const P1_SUBSCRIPTION_CLOCK_ABSTIME: u16 = 1;
const P1_SUBSCRIPTION_SIZE: u32 = 48;
const P1_EVENT_SIZE: u32 = 32;
const PREVIEW1_PROGRAM_LINKED_IMPORTS: &[&str] = &[
    "args_get",
    "args_sizes_get",
    "environ_get",
    "environ_sizes_get",
    "clock_res_get",
    "clock_time_get",
    "fd_advise",
    "fd_allocate",
    "fd_close",
    "fd_datasync",
    "fd_fdstat_get",
    "fd_fdstat_set_flags",
    "fd_fdstat_set_rights",
    "fd_filestat_get",
    "fd_filestat_set_size",
    "fd_filestat_set_times",
    "fd_pread",
    "fd_prestat_get",
    "fd_prestat_dir_name",
    "fd_pwrite",
    "fd_read",
    "fd_readdir",
    "fd_renumber",
    "fd_seek",
    "fd_sync",
    "fd_tell",
    "fd_write",
    "path_create_directory",
    "path_filestat_get",
    "path_filestat_set_times",
    "path_link",
    "path_open",
    "path_readlink",
    "path_remove_directory",
    "path_rename",
    "path_symlink",
    "path_unlink_file",
    "poll_oneoff",
    "proc_exit",
    "proc_raise",
    "sched_yield",
    "random_get",
    "sock_accept",
    "sock_recv",
    "sock_send",
    "sock_shutdown",
];
const P1_RIGHT_PATH_READ_MASK: u64 =
    P1_RIGHT_PATH_OPEN | P1_RIGHT_FD_READDIR | P1_RIGHT_PATH_READLINK | P1_RIGHT_PATH_FILESTAT_GET;
const P1_RIGHT_PATH_FILE_WRITE_MASK: u64 =
    P1_RIGHT_PATH_CREATE_FILE | P1_RIGHT_PATH_FILESTAT_SET_SIZE | P1_RIGHT_PATH_FILESTAT_SET_TIMES;
const P1_RIGHT_PATH_MUTATE_MASK: u64 = P1_RIGHT_PATH_CREATE_DIRECTORY
    | P1_RIGHT_PATH_CREATE_FILE
    | P1_RIGHT_PATH_LINK_SOURCE
    | P1_RIGHT_PATH_LINK_TARGET
    | P1_RIGHT_PATH_RENAME_SOURCE
    | P1_RIGHT_PATH_RENAME_TARGET
    | P1_RIGHT_PATH_SYMLINK
    | P1_RIGHT_PATH_REMOVE_DIRECTORY
    | P1_RIGHT_PATH_UNLINK_FILE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview1_program_linked_imports_match_manifest() {
        assert_eq!(PREVIEW1_PROGRAM_LINKED_IMPORTS, p1::PREVIEW1_FUNCTIONS);
    }

    #[test]
    fn preview1_fdstat_layout_is_little_endian_and_zero_padded() {
        let bytes = p1_fdstat_bytes(2, 0x1234, 0x0102_0304_0506_0708);
        assert_eq!(bytes[0], 2);
        assert_eq!(bytes[1], 0);
        assert_eq!(&bytes[2..4], &0x1234u16.to_le_bytes());
        assert_eq!(&bytes[4..8], &[0; 4]);
        assert_eq!(&bytes[8..16], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &0x0102_0304_0506_0708u64.to_le_bytes());
    }

    #[test]
    fn wasix_program_linked_imports_have_authority_mapping() {
        for import in crate::wasmtime_adapter::wasix::LINKED_IMPORTS {
            assert!(
                crate::wasmtime_adapter::wasix::authority_for(import).is_some(),
                "WASIX linked import {import} has no capability mapping"
            );
        }
    }

    #[test]
    fn wasix_manifest_is_linked_by_core_adapter() {
        assert_eq!(
            crate::wasmtime_adapter::wasix::LINKED_IMPORTS,
            crate::wasmtime_adapter::wasix::manifest().collect::<Vec<_>>()
        );
    }

    #[test]
    fn wasix_linked_imports_are_accepted_by_core_validator() {
        for import in crate::wasmtime_adapter::wasix::LINKED_IMPORTS {
            validate_preview1_program_import(WASIX_MODULE, import)
                .expect("linked WASIX imports should validate");
        }
    }

    #[test]
    fn wasi_unstable_imports_are_accepted_as_preview1() {
        for import in PREVIEW1_PROGRAM_LINKED_IMPORTS {
            validate_preview1_program_import("wasi_unstable", import)
                .expect("wasi_unstable imports should validate as preview1");
        }
    }

    #[test]
    fn wasix_getcwd_reports_path_length_without_trailing_nul() {
        assert_eq!(wasix_getcwd_required_len("/"), Ok(1));
        assert_eq!(wasix_getcwd_required_len("/tmp/work"), Ok(9));
    }

    #[test]
    fn wasix_exec_search_path_builds_confined_candidates() {
        assert_eq!(
            wasix_search_path_candidate(Some("/work"), "/bin", "dash"),
            Some(String::from("/bin/dash"))
        );
        assert_eq!(
            wasix_search_path_candidate(Some("/work"), "tools", "qjs"),
            Some(String::from("/work/tools/qjs"))
        );
        assert_eq!(
            wasix_search_path_candidate(Some("/work"), "", "local"),
            Some(String::from("/work/local"))
        );
        assert_eq!(
            wasix_search_path_candidate(Some("/work"), "../escape", "bad"),
            None
        );
        assert_eq!(
            wasix_search_path_candidate(Some("/work"), "/bin", "../bad"),
            None
        );
        assert_eq!(wasix_search_path_candidate(None, "bin", "dash"), None);
    }

    #[test]
    fn null_device_open_is_confined_to_authorized_path_resolution() {
        let root = FsDescriptor {
            path: "/".into(),
            kind: FsNodeKind::Directory,
            flags: fs_types::DescriptorFlags::READ,
            identity: None,
        };
        let work = FsDescriptor {
            path: "/work".into(),
            kind: FsNodeKind::Directory,
            flags: fs_types::DescriptorFlags::READ,
            identity: None,
        };

        assert_eq!(
            p1_open_null_device(&root, "dev/null", fs_types::OpenFlags::empty()),
            Ok(true)
        );
        assert_eq!(
            p1_open_null_device(&work, "dev/null", fs_types::OpenFlags::empty()),
            Ok(false)
        );
        assert_eq!(
            p1_open_null_device(&root, "dev/null", fs_types::OpenFlags::DIRECTORY),
            Err(p1::errno::NOTDIR)
        );
        assert_eq!(
            p1_open_null_device(
                &root,
                "dev/null",
                fs_types::OpenFlags::CREATE | fs_types::OpenFlags::EXCLUSIVE
            ),
            Err(p1::errno::EXIST)
        );
    }

    #[test]
    fn null_device_rights_and_stat_match_character_device_semantics() {
        let rights = p1_descriptor_rights(&Preview1Descriptor::NullDevice);
        assert_ne!(rights & P1_RIGHT_FD_READ, 0);
        assert_ne!(rights & P1_RIGHT_FD_WRITE, 0);
        assert_ne!(rights & P1_RIGHT_POLL_FD_READWRITE, 0);
        assert!(matches!(
            p1_null_device_stat().type_,
            fs_types::DescriptorType::CharacterDevice
        ));
        assert!(matches!(
            p1_probe_descriptor(Some(&Preview1Descriptor::NullDevice), P1_EVENTTYPE_FD_WRITE),
            Ok(P1Probe::Local(P1Readiness::Ready { bytes })) if bytes == usize::MAX as u64
        ));
    }

    #[test]
    fn fd_renumber_replaces_stdio_descriptor() {
        let file = Preview1Descriptor::File {
            descriptor: FsDescriptor {
                path: "/redirected".into(),
                kind: FsNodeKind::File,
                flags: fs_types::DescriptorFlags::WRITE,
                identity: None,
            },
            offset: 0,
            fdflags: 0,
        };
        let mut table = Preview1DescriptorTable::from_entries(vec![
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stdin {
                    carry: Bytes::new(),
                },
                false,
            )),
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stdout,
                false,
            )),
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stderr,
                false,
            )),
            Some(Preview1DescriptorEntry::new(file, true)),
        ]);

        assert_eq!(table.renumber(3, 1), p1::errno::SUCCESS);
        match table.get(1) {
            Some(Preview1Descriptor::File { descriptor, .. }) => {
                assert_eq!(descriptor.path, "/redirected");
            }
            _ => panic!("fd 1 should be redirected to the file descriptor"),
        }
        assert_eq!(table.close_on_exec(1), Ok(false));
        assert!(table.get(3).is_none());
    }

    #[test]
    fn fd_close_can_close_stdio_slots() {
        let mut table = Preview1DescriptorTable::from_entries(vec![
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stdin {
                    carry: Bytes::new(),
                },
                false,
            )),
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stdout,
                false,
            )),
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stderr,
                false,
            )),
        ]);

        assert_eq!(table.close(1), p1::errno::SUCCESS);
        assert!(table.get(1).is_none());
        assert_eq!(table.close(1), p1::errno::BADF);
    }

    #[test]
    fn descriptor_insert_reuses_lowest_closed_slot() {
        let mut table = Preview1DescriptorTable::from_entries(vec![
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stdin {
                    carry: Bytes::new(),
                },
                false,
            )),
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stdout,
                false,
            )),
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stderr,
                false,
            )),
        ]);

        assert_eq!(table.close(2), p1::errno::SUCCESS);
        assert_eq!(table.close(0), p1::errno::SUCCESS);
        assert_eq!(table.insert(Preview1Descriptor::NullDevice), Ok(0));
    }

    #[test]
    fn fd_dup2_replaces_exact_target_descriptor() {
        let file = Preview1Descriptor::File {
            descriptor: FsDescriptor {
                path: "/redirected".into(),
                kind: FsNodeKind::File,
                flags: fs_types::DescriptorFlags::WRITE,
                identity: None,
            },
            offset: 0,
            fdflags: 0,
        };
        let mut table = Preview1DescriptorTable::from_entries(vec![
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stdin {
                    carry: Bytes::new(),
                },
                false,
            )),
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stdout,
                false,
            )),
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stderr,
                false,
            )),
            Some(Preview1DescriptorEntry::new(file, false)),
        ]);

        assert_eq!(table.dup_to(3, 1, true), Ok(1));
        match table.get(1) {
            Some(Preview1Descriptor::File { descriptor, .. }) => {
                assert_eq!(descriptor.path, "/redirected");
            }
            _ => panic!("fd 1 should be duplicated to the file descriptor"),
        }
        assert_eq!(table.close_on_exec(1), Ok(true));
    }

    #[test]
    fn descriptor_table_preserves_fdflags_across_dup_and_exec_snapshot() {
        let file = Preview1Descriptor::File {
            descriptor: FsDescriptor {
                path: "/nonblock".into(),
                kind: FsNodeKind::File,
                flags: fs_types::DescriptorFlags::READ,
                identity: None,
            },
            offset: 0,
            fdflags: P1_FDFLAG_NONBLOCK,
        };
        let mut table = Preview1DescriptorTable::from_entries(vec![Some(
            Preview1DescriptorEntry::new(file, false),
        )]);

        assert_eq!(table.fdflags(0), Ok(P1_FDFLAG_NONBLOCK));
        assert_eq!(table.dup(0), Ok(1));
        assert_eq!(table.fdflags(1), Ok(P1_FDFLAG_NONBLOCK));
        assert_eq!(table.close_on_exec(1), Ok(false));
        assert_eq!(
            table.set_fdflags(1, P1_FDFLAG_NONBLOCK | P1_FDFLAG_APPEND),
            p1::errno::SUCCESS
        );

        let exec_table = table.clone_for_exec();

        assert_eq!(
            exec_table.fdflags(1),
            Ok(P1_FDFLAG_NONBLOCK | P1_FDFLAG_APPEND)
        );
    }

    #[test]
    fn socket_descriptor_accepts_only_nonblock_fdflag() {
        let (writer, reader) = crate::byte_channel();
        let socket = Preview1Descriptor::Socket(WasixSocketDescriptor::Pair {
            reader,
            writer,
            carry: Bytes::new(),
            options: WasixSocketOptions::default(),
            socket_type: WASIX_SOCK_TYPE_STREAM,
        });
        let mut table = Preview1DescriptorTable::from_entries(vec![Some(
            Preview1DescriptorEntry::new(socket, false),
        )]);

        assert_eq!(table.set_fdflags(0, P1_FDFLAG_NONBLOCK), p1::errno::SUCCESS);
        assert_eq!(table.fdflags(0), Ok(P1_FDFLAG_NONBLOCK));
        assert!(p1_socket_fdflags_supported(P1_FDFLAG_NONBLOCK));
        assert!(!p1_socket_fdflags_supported(P1_FDFLAG_APPEND));
    }

    #[test]
    fn wasix_socket_timeout_respects_nonblocking_fdflag() {
        assert_eq!(wasix_effective_socket_timeout(Some(99), 0), 99);
        assert_eq!(wasix_effective_socket_timeout(None, 0), u64::MAX);
        assert_eq!(
            wasix_effective_socket_timeout(Some(99), P1_FDFLAG_NONBLOCK),
            0
        );
        assert_eq!(
            p1_errno_from_tcp_error_for_fdflags(
                crate::TcpError {
                    kind: crate::TcpErrorKind::Timeout,
                    detail: crate::NetworkErrorDetail::TcpConnectTimeout,
                },
                P1_FDFLAG_NONBLOCK,
            ),
            p1::errno::AGAIN
        );
        assert_eq!(
            p1_errno_from_udp_error_for_fdflags(
                crate::UdpError {
                    kind: crate::UdpErrorKind::Timeout,
                    detail: crate::NetworkErrorDetail::UdpReceiveTimeout,
                },
                P1_FDFLAG_NONBLOCK,
            ),
            p1::errno::AGAIN
        );
    }

    #[test]
    fn wasix_split_helpers_preallocate_nonempty_entries() {
        let lines = wasix_split_lines("alpha\n\nbeta\ngamma\n");
        assert_eq!(lines, ["alpha", "beta", "gamma"]);
        assert_eq!(lines.capacity(), 3);

        let environment = wasix_split_environment("A=1\nEMPTY\n\nB=two\n");
        assert_eq!(
            environment,
            [
                ("A".into(), "1".into()),
                ("EMPTY".into(), String::new()),
                ("B".into(), "two".into())
            ]
        );
        assert_eq!(environment.capacity(), 3);
    }

    #[test]
    fn proc_spawn_preopen_guest_name_resolves_relative_to_cwd() {
        let cwd = Preview1Cwd {
            guest_name: "/workspace".into(),
            descriptor: FsDescriptor {
                path: "/mnt/workspace".into(),
                kind: FsNodeKind::Directory,
                flags: fs_types::DescriptorFlags::READ,
                identity: None,
            },
        };

        assert_eq!(
            wasix_proc_spawn_preopen_guest_name(Some(&cwd), "tools").unwrap(),
            "/workspace/tools"
        );
        assert_eq!(
            wasix_proc_spawn_preopen_guest_name(Some(&cwd), "/bin").unwrap(),
            "/bin"
        );
        assert_eq!(
            wasix_proc_spawn_preopen_guest_name(Some(&cwd), "../escape"),
            Err(p1::errno::PERM)
        );
        assert_eq!(
            wasix_proc_spawn_preopen_guest_name(None, "relative"),
            Err(p1::errno::NOTCAPABLE)
        );
    }

    #[test]
    fn proc_spawn_preopen_authority_inherits_only_non_directory_rights() {
        let mut parent = ProcessAuthority::empty();
        parent.insert_directory_preopen(
            crate::DirectoryPreopen::new(
                "/workspace",
                "/workspace",
                crate::DirectoryAuthorityRights::READ,
            )
            .expect("test preopen should be valid"),
        );
        parent.grant_network_rights(crate::NetworkAuthorityRights::TCP);
        parent.grant_clock_rights(crate::ClockAuthorityRights::SET_WALL_CLOCK);
        parent.grant_terminal_rights(crate::TerminalAuthorityRights::OUTPUT);
        parent.grant_process_rights(crate::ProcessAuthorityRights::SPAWN);
        parent.grant_link_rights(crate::LinkAuthorityRights::SYMLINK_READ);

        let child = wasix_proc_spawn_inherited_non_directory_authority(&parent);

        assert!(child.directory_preopens().is_empty());
        assert_eq!(child.network_rights(), crate::NetworkAuthorityRights::TCP);
        assert_eq!(
            child.clock_rights(),
            crate::ClockAuthorityRights::SET_WALL_CLOCK
        );
        assert_eq!(
            child.terminal_rights(),
            crate::TerminalAuthorityRights::OUTPUT
        );
        assert_eq!(child.process_rights(), crate::ProcessAuthorityRights::SPAWN);
        assert_eq!(
            child.link_rights(),
            crate::LinkAuthorityRights::SYMLINK_READ
        );
        assert!(parent.contains_authority(&child));
    }

    #[test]
    fn chroot_authority_resolution_maps_guest_root_to_derived_source() {
        let mut authority = ProcessAuthority::empty();
        authority.insert_directory_preopen(
            crate::DirectoryPreopen::new(
                "/mnt/workspace/app",
                "/",
                crate::DirectoryAuthorityRights::READ | crate::DirectoryAuthorityRights::WRITE,
            )
            .expect("test preopen should be valid"),
        );
        let cwd = authority
            .derive_directory_cap(
                "/mnt/workspace/app",
                "/",
                crate::DirectoryAuthorityRights::READ,
            )
            .expect("cwd cap should derive from chroot root");
        authority.chdir(cwd);

        let (source, flags) = wasix_authority_resolve_absolute_guest_path(&authority, "/bin/tool")
            .expect("guest path should resolve inside chroot root");

        assert_eq!(source, "/mnt/workspace/app/bin/tool");
        assert!(flags.contains(fs_types::DescriptorFlags::READ));
        assert!(flags.contains(fs_types::DescriptorFlags::WRITE));
    }

    #[test]
    fn exec_descriptor_snapshot_drops_close_on_exec_entries() {
        let closed = Preview1Descriptor::File {
            descriptor: FsDescriptor {
                path: "/closed-on-exec".into(),
                kind: FsNodeKind::File,
                flags: fs_types::DescriptorFlags::READ,
                identity: None,
            },
            offset: 0,
            fdflags: 0,
        };
        let retained = Preview1Descriptor::File {
            descriptor: FsDescriptor {
                path: "/retained".into(),
                kind: FsNodeKind::File,
                flags: fs_types::DescriptorFlags::READ,
                identity: None,
            },
            offset: 0,
            fdflags: 0,
        };
        let table = Preview1DescriptorTable::from_entries(vec![
            Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Stdin {
                    carry: Bytes::new(),
                },
                false,
            )),
            Some(Preview1DescriptorEntry::new(closed, true)),
            Some(Preview1DescriptorEntry::new(retained, false)),
        ]);

        let exec_table = table.clone_for_exec();

        assert!(exec_table.get(1).is_none());
        match exec_table.get(2) {
            Some(Preview1Descriptor::File { descriptor, .. }) => {
                assert_eq!(descriptor.path, "/retained");
            }
            _ => panic!("fd 2 should be retained across exec"),
        }
    }

    #[test]
    fn spawn_fd_snapshot_maps_directory_source_to_guest_path() {
        let snapshot = WasixSpawnFdSnapshot {
            descriptors: Preview1DescriptorTable::from_entries(vec![Some(
                Preview1DescriptorEntry::new(
                    Preview1Descriptor::Preopen {
                        guest_name: "/workspace".into(),
                        descriptor: FsDescriptor {
                            path: "/mnt/workspace".into(),
                            kind: FsNodeKind::Directory,
                            flags: fs_types::DescriptorFlags::READ,
                            identity: None,
                        },
                    },
                    false,
                ),
            )]),
            authority: ProcessAuthority::root(),
            cwd: None,
        };

        assert_eq!(
            wasix_spawn_guest_name_for_source(&snapshot, "/mnt/workspace/tools").unwrap(),
            "/workspace/tools"
        );
        assert_eq!(
            wasix_spawn_guest_name_for_source(&snapshot, "/outside"),
            Err(p1::errno::NOTCAPABLE)
        );
    }

    #[test]
    fn spawn_fd_fchdir_updates_child_authority_cwd() {
        let preopen = Preview1Descriptor::Preopen {
            guest_name: "/workspace".into(),
            descriptor: FsDescriptor {
                path: "/mnt/workspace".into(),
                kind: FsNodeKind::Directory,
                flags: fs_types::DescriptorFlags::READ,
                identity: None,
            },
        };
        let mut authority = ProcessAuthority::empty();
        authority.insert_directory_preopen(
            crate::DirectoryPreopen::new(
                "/mnt/workspace",
                "/workspace",
                DirectoryAuthorityRights::READ,
            )
            .expect("test preopen must be valid"),
        );
        let mut snapshot = WasixSpawnFdSnapshot {
            descriptors: Preview1DescriptorTable::from_entries(vec![Some(
                Preview1DescriptorEntry::new(preopen, false),
            )]),
            authority,
            cwd: None,
        };

        wasix_apply_spawn_fchdir(&mut snapshot, 0).expect("preopen fchdir must succeed");
        assert_eq!(
            snapshot
                .authority
                .cwd()
                .expect("fchdir should set child cwd")
                .guest_name(),
            "/workspace"
        );
    }

    #[test]
    fn spawn_fd_open_base_uses_child_cwd_for_relative_paths() {
        let snapshot = WasixSpawnFdSnapshot {
            descriptors: Preview1DescriptorTable::from_entries(vec![Some(
                Preview1DescriptorEntry::new(
                    Preview1Descriptor::Preopen {
                        guest_name: "/root".into(),
                        descriptor: FsDescriptor {
                            path: "/source/root".into(),
                            kind: FsNodeKind::Directory,
                            flags: fs_types::DescriptorFlags::READ,
                            identity: None,
                        },
                    },
                    false,
                ),
            )]),
            authority: ProcessAuthority::root(),
            cwd: Some(Preview1Cwd {
                guest_name: "/workspace".into(),
                descriptor: FsDescriptor {
                    path: "/source/workspace".into(),
                    kind: FsNodeKind::Directory,
                    flags: fs_types::DescriptorFlags::READ,
                    identity: None,
                },
            }),
        };

        let (base, path) =
            wasix_spawn_resolve_open_base(&snapshot, 0, "out.log").expect("cwd should resolve");

        assert_eq!(base.path, "/source/workspace");
        assert_eq!(path, "out.log");
    }

    #[test]
    fn spawn_fd_open_base_resolves_absolute_guest_path_through_preopen() {
        let snapshot = WasixSpawnFdSnapshot {
            descriptors: Preview1DescriptorTable::from_entries(vec![Some(
                Preview1DescriptorEntry::new(
                    Preview1Descriptor::Preopen {
                        guest_name: "/workspace".into(),
                        descriptor: FsDescriptor {
                            path: "/source/workspace".into(),
                            kind: FsNodeKind::Directory,
                            flags: fs_types::DescriptorFlags::READ,
                            identity: None,
                        },
                    },
                    false,
                ),
            )]),
            authority: ProcessAuthority::root(),
            cwd: None,
        };

        let (base, path) = wasix_spawn_resolve_open_base(&snapshot, 0, "/workspace/logs/out")
            .expect("absolute preopen path should resolve");

        assert_eq!(base.path, "/source/workspace");
        assert_eq!(path, "logs/out");
    }

    #[test]
    fn wasix_signal_disposition_validation_accepts_default_and_ignore() {
        assert_eq!(
            wasix_signal_disposition_from_raw(2, WASIX_SIGNAL_DISPOSITION_DEFAULT),
            Ok(WasixSignalDisposition {
                signal: 2,
                action: WasixSignalDispositionAction::Default,
            })
        );
        assert_eq!(
            wasix_signal_disposition_from_raw(15, WASIX_SIGNAL_DISPOSITION_IGNORE),
            Ok(WasixSignalDisposition {
                signal: 15,
                action: WasixSignalDispositionAction::Ignore,
            })
        );
        assert_eq!(
            wasix_signal_disposition_from_raw(0, WASIX_SIGNAL_DISPOSITION_DEFAULT),
            Err(p1::errno::INVAL)
        );
        assert_eq!(
            wasix_signal_disposition_from_raw(1, 2),
            Err(p1::errno::INVAL)
        );
    }

    #[test]
    fn wasix_bridge_security_accepts_documented_values_only() {
        assert_eq!(
            wasix_bridge_security(i32::from(WASIX_STREAM_SECURITY_UNENCRYPTED))
                .map(crate::NetworkBridgeSecurity::raw),
            Ok(WASIX_STREAM_SECURITY_UNENCRYPTED)
        );
        assert_eq!(
            wasix_bridge_security(i32::from(WASIX_STREAM_SECURITY_ANY_ENCRYPTION))
                .map(crate::NetworkBridgeSecurity::raw),
            Ok(WASIX_STREAM_SECURITY_ANY_ENCRYPTION)
        );
        assert_eq!(wasix_bridge_security(0), Err(p1::errno::INVAL));
        assert_eq!(
            wasix_bridge_security(
                i32::from(WASIX_STREAM_SECURITY_CLASSIC_ENCRYPTION)
                    | i32::from(WASIX_STREAM_SECURITY_DOUBLE_ENCRYPTION)
            ),
            Err(p1::errno::INVAL)
        );
    }

    #[test]
    fn wasix_socket_operations_select_network_authority_by_socket_kind() {
        let tcp =
            Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Connected {
                family: WasixSocketFamily::Ipv4,
                stream: 1,
                peer_address: crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([
                    127, 0, 0, 1,
                ])),
                peer_port: 80,
                options: WasixSocketOptions::default(),
            }));
        let udp_bound =
            Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
                family: WasixSocketFamily::Ipv4,
                socket: 2,
                local_port: 5353,
                options: WasixSocketOptions::default(),
            }));
        let udp_unbound =
            Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound {
                family: WasixSocketFamily::Ipv4,
                options: WasixSocketOptions::default(),
            }));
        let (left_writer, right_reader) = crate::byte_channel();
        let pair = Preview1Descriptor::Socket(WasixSocketDescriptor::Pair {
            reader: right_reader,
            writer: left_writer,
            carry: Bytes::new(),
            options: WasixSocketOptions::default(),
            socket_type: WASIX_SOCK_TYPE_STREAM,
        });
        let nonsocket = Preview1Descriptor::Stdout;

        assert_eq!(
            wasix_sock_recv_authority(Some(&tcp)),
            Ok(WasixSocketAuthority::Tcp)
        );
        assert_eq!(
            wasix_sock_send_authority(Some(&tcp)),
            Ok(WasixSocketAuthority::Tcp)
        );
        assert_eq!(
            wasix_sock_recv_authority(Some(&udp_bound)),
            Ok(WasixSocketAuthority::Udp)
        );
        assert_eq!(
            wasix_sock_send_authority(Some(&udp_bound)),
            Ok(WasixSocketAuthority::Udp)
        );
        assert_eq!(
            wasix_sock_send_authority(Some(&udp_unbound)),
            Ok(WasixSocketAuthority::Udp)
        );
        assert_eq!(
            wasix_sock_bind_authority(Some(&udp_unbound)),
            Ok(WasixSocketAuthority::Udp)
        );
        assert_eq!(
            wasix_sock_recv_authority(Some(&pair)),
            Ok(WasixSocketAuthority::LocalOnly)
        );
        assert_eq!(
            wasix_sock_send_authority(Some(&pair)),
            Ok(WasixSocketAuthority::LocalOnly)
        );
        assert_eq!(
            wasix_sock_bind_authority(Some(&udp_bound)),
            Err(p1::errno::INVAL)
        );
        assert_eq!(
            wasix_sock_listen_authority(Some(&tcp)),
            Ok(WasixSocketAuthority::Tcp)
        );
        assert_eq!(
            wasix_sock_listen_authority(Some(&udp_bound)),
            Err(p1::errno::INVAL)
        );
        assert_eq!(
            wasix_sock_listen_authority(Some(&pair)),
            Err(p1::errno::INVAL)
        );
        assert_eq!(
            wasix_sock_recv_authority(Some(&udp_unbound)),
            Err(p1::errno::INVAL)
        );
        assert_eq!(
            wasix_sock_send_authority(Some(&nonsocket)),
            Err(p1::errno::NOTSOCK)
        );
        assert_eq!(
            wasix_sock_listen_authority(Some(&nonsocket)),
            Err(p1::errno::NOTSOCK)
        );
        assert_eq!(wasix_sock_recv_authority(None), Err(p1::errno::BADF));
        assert_eq!(wasix_sock_send_authority(None), Err(p1::errno::BADF));
        assert_eq!(wasix_sock_listen_authority(None), Err(p1::errno::BADF));
    }

    #[test]
    fn wasix_multicast_preflight_accepts_udp_sockets_only() {
        let udp = Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound {
            family: WasixSocketFamily::Ipv4,
            options: WasixSocketOptions::default(),
        }));
        let tcp =
            Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Unconnected {
                family: WasixSocketFamily::Ipv4,
                options: WasixSocketOptions::default(),
            }));
        let file = Preview1Descriptor::Stdout;

        assert_eq!(
            wasix_udp_socket_descriptor_status(Some(&udp)),
            p1::errno::SUCCESS
        );
        assert_eq!(
            wasix_udp_socket_descriptor_status(Some(&tcp)),
            p1::errno::INVAL
        );
        assert_eq!(
            wasix_udp_socket_descriptor_status(Some(&file)),
            p1::errno::NOTSOCK
        );
        assert_eq!(wasix_udp_socket_descriptor_status(None), p1::errno::BADF);
    }

    #[test]
    fn wasix_multicast_v6_accepts_only_ipv6_address_tags() {
        assert_eq!(
            wasix_addr_ip6_family_status(WASIX_ADDRESS_FAMILY_IP_INET6),
            Ok(())
        );
        assert_eq!(
            wasix_addr_ip6_family_status(WASIX_ADDRESS_FAMILY_IP_INET4),
            Err(p1::errno::INVAL)
        );
        assert_eq!(
            wasix_addr_ip6_family_status(WASIX_ADDRESS_FAMILY_UNSPEC),
            Err(p1::errno::INVAL)
        );
        assert_eq!(
            wasix_addr_ip6_family_status(WASIX_ADDRESS_FAMILY_UNIX),
            Err(p1::errno::NOTSUP)
        );
        assert_eq!(wasix_addr_ip6_family_status(0xff), Err(p1::errno::INVAL));
    }

    #[test]
    fn preview1_fixed_memory_reads_copy_into_caller_buffer() {
        let guest = [1_u8, 2, 3, 4, 5, 6];
        let memory = Preview1Memory {
            base: guest.as_ptr() as usize,
            len: guest.len(),
        };
        let mut bytes = [0_u8; 4];

        preview1_read_memory_into(memory, 1, &mut bytes).expect("fixed read should fit");

        assert_eq!(bytes, [2, 3, 4, 5]);
        let error =
            preview1_read_memory_into(memory, 4, &mut bytes).expect_err("read should overflow");
        assert_eq!(error.kind, crate::ProgramExecErrorKind::InvalidBinary);
        assert_eq!(
            error.detail,
            ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds
        );
    }

    #[test]
    fn wasix_socket_family_pins_wildcard_address_and_rejects_cross_family_peers() {
        let v4 = crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([192, 0, 2, 1]));
        let v6 = crate::NetworkIpAddress::Ipv6(helios_netstack::Ipv6Address::new([
            0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2,
        ]));

        // A wildcard bind means 0.0.0.0 on AF_INET and :: on AF_INET6.
        assert_eq!(
            WasixSocketFamily::Ipv4.unspecified_address(),
            crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([0, 0, 0, 0]))
        );
        assert_eq!(
            WasixSocketFamily::Ipv6.unspecified_address(),
            crate::NetworkIpAddress::Ipv6(helios_netstack::Ipv6Address::UNSPECIFIED)
        );

        // There is no v4-mapped-v6 path: each family accepts only its own.
        assert!(WasixSocketFamily::Ipv4.accepts(v4));
        assert!(!WasixSocketFamily::Ipv4.accepts(v6));
        assert!(WasixSocketFamily::Ipv6.accepts(v6));
        assert!(!WasixSocketFamily::Ipv6.accepts(v4));
    }

    #[test]
    fn wasix_socket_creation_validates_family_and_protocol() {
        assert_eq!(
            wasix_validate_network_socket_request(
                WASIX_ADDRESS_FAMILY_IP_INET4_I32,
                WASIX_SOCK_TYPE_STREAM,
                WASIX_IPPROTO_TCP_I32,
            ),
            Ok(())
        );
        assert_eq!(
            wasix_validate_network_socket_request(
                WASIX_ADDRESS_FAMILY_UNSPEC_I32,
                WASIX_SOCK_TYPE_DGRAM,
                0,
            ),
            Ok(())
        );
        assert_eq!(
            wasix_validate_network_socket_request(
                WASIX_ADDRESS_FAMILY_UNIX_I32,
                WASIX_SOCK_TYPE_STREAM,
                0,
            ),
            Err(p1::errno::NOTSUP)
        );
        // AF_INET6 is a supported family for both socket types; only
        // AF_UNIX remains unsupported for network sockets.
        assert_eq!(
            wasix_validate_network_socket_request(
                WASIX_ADDRESS_FAMILY_IP_INET6_I32,
                WASIX_SOCK_TYPE_STREAM,
                WASIX_IPPROTO_TCP_I32,
            ),
            Ok(())
        );
        assert_eq!(
            wasix_validate_network_socket_request(
                WASIX_ADDRESS_FAMILY_IP_INET6_I32,
                WASIX_SOCK_TYPE_DGRAM,
                WASIX_IPPROTO_UDP_I32,
            ),
            Ok(())
        );
        // A mismatched protocol is still rejected within AF_INET6.
        assert_eq!(
            wasix_validate_network_socket_request(
                WASIX_ADDRESS_FAMILY_IP_INET6_I32,
                WASIX_SOCK_TYPE_STREAM,
                WASIX_IPPROTO_UDP_I32,
            ),
            Err(p1::errno::INVAL)
        );
        // socket_pair stays AF_UNIX/AF_UNSPEC only.
        assert_eq!(
            wasix_validate_socket_pair_request(
                WASIX_ADDRESS_FAMILY_IP_INET6_I32,
                WASIX_SOCK_TYPE_STREAM,
                0,
            ),
            Err(p1::errno::NOTSUP)
        );
        assert_eq!(
            wasix_validate_network_socket_request(
                WASIX_ADDRESS_FAMILY_IP_INET4_I32,
                WASIX_SOCK_TYPE_STREAM,
                WASIX_IPPROTO_UDP_I32,
            ),
            Err(p1::errno::INVAL)
        );
        assert_eq!(
            wasix_validate_socket_pair_request(
                WASIX_ADDRESS_FAMILY_UNIX_I32,
                WASIX_SOCK_TYPE_STREAM,
                0,
            ),
            Ok(())
        );
        assert_eq!(
            wasix_validate_socket_pair_request(
                WASIX_ADDRESS_FAMILY_IP_INET4_I32,
                WASIX_SOCK_TYPE_STREAM,
                0,
            ),
            Err(p1::errno::NOTSUP)
        );
        assert_eq!(
            wasix_validate_socket_pair_request(
                WASIX_ADDRESS_FAMILY_IP_INET6_I32,
                WASIX_SOCK_TYPE_STREAM,
                0,
            ),
            Err(p1::errno::NOTSUP)
        );
        assert_eq!(
            wasix_validate_socket_pair_request(
                WASIX_ADDRESS_FAMILY_UNIX_I32,
                WASIX_SOCK_TYPE_DGRAM,
                WASIX_IPPROTO_UDP_I32,
            ),
            Err(p1::errno::INVAL)
        );
    }

    /// Ready mask for a descriptor whose readiness needs no network service.
    fn local_epoll_mask(descriptor: Option<&Preview1Descriptor>, interest: u32) -> u32 {
        if descriptor.is_none() {
            return WASIX_EPOLL_TYPE_EPOLLERR | WASIX_EPOLL_TYPE_EPOLLHUP;
        }
        let event_type = if interest & WASIX_EPOLL_TYPE_EPOLLOUT != 0 {
            P1_EVENTTYPE_FD_WRITE
        } else {
            P1_EVENTTYPE_FD_READ
        };
        let readiness = match p1_probe_descriptor(descriptor, event_type) {
            Ok(P1Probe::Local(readiness)) => Ok(readiness),
            Ok(P1Probe::Network(_)) => panic!("descriptor needs the network service"),
            Err(errno) => Err(errno),
        };
        wasix_epoll_mask_bit(readiness, event_type)
    }

    #[test]
    fn wasix_epoll_ready_mask_reports_supported_descriptor_readiness() {
        let stdout = Preview1Descriptor::Stdout;
        assert_eq!(
            local_epoll_mask(Some(&stdout), WASIX_EPOLL_TYPE_EPOLLOUT),
            WASIX_EPOLL_TYPE_EPOLLOUT
        );

        let event = Preview1Descriptor::Event(EventFd::new(1, false));
        assert_eq!(
            local_epoll_mask(Some(&event), WASIX_EPOLL_TYPE_EPOLLIN),
            WASIX_EPOLL_TYPE_EPOLLIN
        );

        let empty_event = Preview1Descriptor::Event(EventFd::new(0, false));
        assert_eq!(
            local_epoll_mask(Some(&empty_event), WASIX_EPOLL_TYPE_EPOLLIN),
            0
        );

        let (pipe_writer, pipe_reader) = crate::byte_channel();
        let pipe = Preview1Descriptor::PipeRead {
            reader: pipe_reader,
            carry: Bytes::new(),
        };
        assert_eq!(local_epoll_mask(Some(&pipe), WASIX_EPOLL_TYPE_EPOLLIN), 0);
        assert_eq!(
            pipe_writer.try_write(Bytes::from_static(b"pipe")),
            crate::TryWrite::Written,
            "pipe reader is still open"
        );
        assert_eq!(
            local_epoll_mask(Some(&pipe), WASIX_EPOLL_TYPE_EPOLLIN),
            WASIX_EPOLL_TYPE_EPOLLIN
        );

        let (pair_writer, pair_reader) = crate::byte_channel();
        let pair = Preview1Descriptor::Socket(WasixSocketDescriptor::Pair {
            reader: pair_reader,
            writer: pair_writer.clone(),
            carry: Bytes::new(),
            options: WasixSocketOptions::default(),
            socket_type: WASIX_SOCK_TYPE_STREAM,
        });
        assert_eq!(local_epoll_mask(Some(&pair), WASIX_EPOLL_TYPE_EPOLLIN), 0);
        assert_eq!(
            pair_writer.try_write(Bytes::from_static(b"pair")),
            crate::TryWrite::Written,
            "socket-pair reader is still open"
        );
        assert_eq!(
            local_epoll_mask(Some(&pair), WASIX_EPOLL_TYPE_EPOLLIN),
            WASIX_EPOLL_TYPE_EPOLLIN
        );

        assert_eq!(
            local_epoll_mask(None, WASIX_EPOLL_TYPE_EPOLLIN),
            WASIX_EPOLL_TYPE_EPOLLERR | WASIX_EPOLL_TYPE_EPOLLHUP
        );
    }

    #[test]
    fn shared_memory_pool_reuses_matching_spec_bucket() {
        let mut config = wasmtime::Config::new();
        config.wasm_threads(true);
        config.shared_memory(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let spec = SharedMemorySpec {
            initial_pages: 1,
            maximum_pages: 1,
        };
        let mut pool = SharedMemoryPool::new(spec.byte_size() * 2);
        let memory = SharedMemory::new(&engine, spec.memory_type()).unwrap();
        let ptr = memory.data().as_ptr().cast::<u8>() as *mut u8;
        unsafe {
            ptr.write(0xa5);
        }

        // The recycle path scrubs before re-pooling, so acquire never
        // zeroes on the spawn path and pooled entries come back clean.
        assert!(pool.reserve_for_recycle(spec, &memory));
        assert_eq!(pool.resident_bytes, spec.byte_size());
        futures_lite::future::block_on(scrub_shared_memory(&memory));
        pool.finish_recycle(spec, memory);

        let reused = pool.acquire(&engine, spec).unwrap();
        assert_eq!(pool.resident_bytes, 0);
        assert!(pool.buckets.contains_key(&spec));
        let zeroed = unsafe { reused.data()[0].get().read() };
        assert_eq!(zeroed, 0);
    }

    #[test]
    fn shared_memory_pool_evicts_other_specs_under_pressure() {
        let mut config = wasmtime::Config::new();
        config.wasm_threads(true);
        config.shared_memory(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        let spec = SharedMemorySpec {
            initial_pages: 1,
            maximum_pages: 1,
        };
        let mut pool = SharedMemoryPool::new(spec.byte_size() * 2);
        let memory = SharedMemory::new(&engine, spec.memory_type()).unwrap();
        assert!(pool.reserve_for_recycle(spec, &memory));
        futures_lite::future::block_on(scrub_shared_memory(&memory));
        pool.finish_recycle(spec, memory);

        // A retained entry under another spec is dropped, releasing its
        // budget and user RAM, rather than starving a failing allocation.
        assert!(pool.evict_one());
        assert_eq!(pool.resident_bytes, 0);
        assert!(!pool.evict_one());
    }

    #[test]
    fn wasix_socket_size_options_are_descriptor_local_state() {
        let mut descriptor = WasixSocketDescriptor::Tcp(WasixTcpSocket::Unconnected {
            family: WasixSocketFamily::Ipv4,
            options: WasixSocketOptions::default(),
        });

        assert_eq!(
            descriptor.options().receive_buffer_size,
            DEFAULT_WASIX_SOCKET_BUFFER_BYTES
        );
        assert_eq!(
            descriptor.options().send_buffer_size,
            DEFAULT_WASIX_SOCKET_BUFFER_BYTES
        );
        assert_eq!(
            descriptor.options().size(WASIX_SOCK_OPTION_RECV_LOWAT),
            Ok(DEFAULT_WASIX_SOCKET_LOW_WATER_BYTES)
        );
        assert_eq!(
            descriptor.options().size(WASIX_SOCK_OPTION_TTL),
            Ok(DEFAULT_WASIX_SOCKET_TTL)
        );

        assert_eq!(
            descriptor
                .options_mut()
                .set_size(WASIX_SOCK_OPTION_RECV_BUF_SIZE, 4096),
            p1::errno::SUCCESS
        );
        assert_eq!(
            descriptor
                .options_mut()
                .set_size(WASIX_SOCK_OPTION_SEND_BUF_SIZE, 8192),
            p1::errno::SUCCESS
        );
        assert_eq!(
            descriptor
                .options_mut()
                .set_size(WASIX_SOCK_OPTION_RECV_LOWAT, 2),
            p1::errno::SUCCESS
        );
        assert_eq!(
            descriptor
                .options_mut()
                .set_size(WASIX_SOCK_OPTION_TTL, 128),
            p1::errno::SUCCESS
        );

        assert_eq!(descriptor.options().receive_buffer_size, 4096);
        assert_eq!(descriptor.options().send_buffer_size, 8192);
        assert_eq!(
            descriptor.options().size(WASIX_SOCK_OPTION_RECV_LOWAT),
            Ok(2)
        );
        assert_eq!(descriptor.options().size(WASIX_SOCK_OPTION_TTL), Ok(128));
        assert_eq!(
            descriptor
                .options_mut()
                .set_size(WASIX_SOCK_OPTION_TYPE, WASIX_SOCK_TYPE_STREAM as u64),
            p1::errno::INVAL
        );
        assert_eq!(
            descriptor.options().size(WASIX_SOCK_OPTION_RECV_TIMEOUT),
            Err(p1::errno::INVAL)
        );
    }

    #[test]
    fn wasix_socket_flag_options_are_descriptor_local_state() {
        let mut descriptor = WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound {
            family: WasixSocketFamily::Ipv4,
            options: WasixSocketOptions::default(),
        });

        assert_eq!(
            descriptor
                .options_mut()
                .set_flag(WASIX_SOCK_OPTION_BROADCAST, true),
            p1::errno::SUCCESS
        );
        assert_eq!(
            descriptor
                .options_mut()
                .set_flag(WASIX_SOCK_OPTION_REUSE_ADDR, true),
            p1::errno::SUCCESS
        );
        assert_eq!(
            descriptor.options().flag(WASIX_SOCK_OPTION_BROADCAST),
            Ok(true)
        );
        assert_eq!(
            descriptor.options().flag(WASIX_SOCK_OPTION_REUSE_ADDR),
            Ok(true)
        );
        assert_eq!(
            descriptor.options().flag(WASIX_SOCK_OPTION_KEEP_ALIVE),
            Ok(false)
        );

        assert_eq!(
            descriptor
                .options_mut()
                .set_flag(WASIX_SOCK_OPTION_BROADCAST, false),
            p1::errno::SUCCESS
        );
        assert_eq!(
            descriptor.options().flag(WASIX_SOCK_OPTION_BROADCAST),
            Ok(false)
        );
        assert_eq!(
            descriptor.options().flag(WASIX_SOCK_OPTION_RECV_BUF_SIZE),
            Err(p1::errno::INVAL)
        );
    }

    /// Options the netstack cannot honour must fail loudly.
    ///
    /// The stack has no keepalive timer and transmits as soon as the window
    /// allows, so `SO_KEEPALIVE` on and `TCP_NODELAY` off are behaviours it
    /// will never produce. Recording them silently would tell a guest its
    /// connections are being probed or coalesced when they are not.
    #[test]
    fn wasix_socket_rejects_flags_the_netstack_cannot_honour() {
        let mut descriptor = WasixSocketDescriptor::Tcp(WasixTcpSocket::Unconnected {
            family: WasixSocketFamily::Ipv4,
            options: WasixSocketOptions::default(),
        });

        assert_eq!(
            descriptor
                .options_mut()
                .set_flag(WASIX_SOCK_OPTION_KEEP_ALIVE, true),
            p1::errno::NOTSUP
        );
        assert_eq!(
            descriptor.options().flag(WASIX_SOCK_OPTION_KEEP_ALIVE),
            Ok(false)
        );
        assert_eq!(
            descriptor
                .options_mut()
                .set_flag(WASIX_SOCK_OPTION_KEEP_ALIVE, false),
            p1::errno::SUCCESS
        );

        assert_eq!(
            descriptor
                .options_mut()
                .set_flag(WASIX_SOCK_OPTION_NO_DELAY, true),
            p1::errno::SUCCESS
        );
        assert_eq!(
            descriptor.options().flag(WASIX_SOCK_OPTION_NO_DELAY),
            Ok(true)
        );
        assert_eq!(
            descriptor
                .options_mut()
                .set_flag(WASIX_SOCK_OPTION_NO_DELAY, false),
            p1::errno::NOTSUP
        );
        assert_eq!(
            descriptor.options().flag(WASIX_SOCK_OPTION_NO_DELAY),
            Ok(true)
        );
    }

    /// Buffer hints clamp to the netstack's real per-socket reservations, and
    /// a TTL outside the IP header's range is rejected instead of truncated.
    #[test]
    fn wasix_socket_size_options_clamp_to_netstack_capacity() {
        let mut descriptor = WasixSocketDescriptor::Tcp(WasixTcpSocket::Unconnected {
            family: WasixSocketFamily::Ipv4,
            options: WasixSocketOptions::default(),
        });

        assert_eq!(
            descriptor
                .options_mut()
                .set_size(WASIX_SOCK_OPTION_RECV_BUF_SIZE, u64::MAX),
            p1::errno::SUCCESS
        );
        assert_eq!(
            descriptor.options().receive_buffer_size,
            WASIX_SOCKET_RECEIVE_BUFFER_CEILING
        );
        assert_eq!(
            descriptor
                .options_mut()
                .set_size(WASIX_SOCK_OPTION_SEND_BUF_SIZE, u64::MAX),
            p1::errno::SUCCESS
        );
        assert_eq!(
            descriptor.options().send_buffer_size,
            WASIX_SOCKET_SEND_BUFFER_CEILING
        );

        assert_eq!(
            descriptor.options_mut().set_size(WASIX_SOCK_OPTION_TTL, 0),
            p1::errno::INVAL
        );
        assert_eq!(
            descriptor
                .options_mut()
                .set_size(WASIX_SOCK_OPTION_TTL, 256),
            p1::errno::INVAL
        );
        assert_eq!(
            descriptor.options().hop_limit(),
            DEFAULT_WASIX_SOCKET_TTL as u8
        );
        assert_eq!(
            descriptor.options_mut().set_size(WASIX_SOCK_OPTION_TTL, 9),
            p1::errno::SUCCESS
        );
        assert_eq!(descriptor.options().hop_limit(), 9);
    }

    /// `/dev/null` has no backing filesystem object, so it needs a device id
    /// of its own for `st_dev`/`st_ino` to stay distinct and stable.
    #[test]
    fn null_device_reports_a_stable_device_identity() {
        let identity = p1_null_device_identity();

        assert_eq!(identity, p1_null_device_identity());
        assert_eq!(identity.domain(), crate::AuthorityDomain::GUEST_DEVICES);
        assert_ne!(identity.domain(), crate::AuthorityDomain::GUEST_BOOTFS);
        assert_ne!(identity.domain(), crate::AuthorityDomain::HOST_SHARE_9P);
        assert_ne!(identity.domain().raw(), 0);
        assert_ne!(identity.local(), 0);
    }

    #[test]
    fn wasix_socket_time_options_are_descriptor_local_state() {
        let mut descriptor = WasixSocketDescriptor::Tcp(WasixTcpSocket::Unconnected {
            family: WasixSocketFamily::Ipv4,
            options: WasixSocketOptions::default(),
        });

        assert_eq!(
            descriptor.options().time(WASIX_SOCK_OPTION_RECV_TIMEOUT),
            Ok(None)
        );
        assert_eq!(
            descriptor
                .options_mut()
                .set_time(WASIX_SOCK_OPTION_RECV_TIMEOUT, Some(1_000)),
            p1::errno::SUCCESS
        );
        assert_eq!(
            descriptor.options().time(WASIX_SOCK_OPTION_RECV_TIMEOUT),
            Ok(Some(1_000))
        );
        assert_eq!(
            descriptor
                .options_mut()
                .set_time(WASIX_SOCK_OPTION_LINGER, Some(2_000)),
            p1::errno::SUCCESS
        );
        assert_eq!(
            descriptor.options().time(WASIX_SOCK_OPTION_LINGER),
            Ok(Some(2_000))
        );
        assert_eq!(
            descriptor
                .options_mut()
                .set_time(WASIX_SOCK_OPTION_SEND_BUF_SIZE, None),
            p1::errno::INVAL
        );
    }
}
