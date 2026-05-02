use super::*;
use crate::ProgramExecErrorDetail;
use crate::wasmtime_adapter::artifact_profile::{self, ArtifactProfileError};
use crate::wasmtime_adapter::config::AotCompileHint;
use crate::wasmtime_adapter::cwasm::{self, ArtifactTrustError, UntrustedCwasm};
use crate::wasmtime_adapter::{WasmtimeCompiledComponent, WasmtimeCompiledCoreModule};
use crate::wasmtime_adapter::{
    WasmtimePrecompiledKind,
    wasi::{
        DebugFileSystem, FsDescriptor, FsNodeKind, WasiImportSet,
        bindings::filesystem::types as fs_types, p1,
    },
};
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::vec;
use bytes::Bytes;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicI32, Ordering as AtomicOrdering};
use core::task::{Context, Poll};
use core::time::Duration;
use helios_compiler_abi::{
    CompileHint as CompilerAbiHint, CompilerRequestHeader, CompilerResponseHeader, CompilerStatus,
    HELIOS_COMPILER_ABI_VERSION, HELIOS_COMPILER_ALLOC, HELIOS_COMPILER_COMPILE,
    HELIOS_COMPILER_INITIALIZE, HELIOS_COMPILER_PTHREAD_SELF_OFFSET,
};
use helios_hal::watchdog::Watchdog;
use thiserror::Error;
use wasmtime::component::Component;
use wasmtime::{
    Caller, ExternType, InstancePre, Linker as CoreLinker, MemoryType, Module, SharedMemory, Val,
};

const COMPILER_PLUGIN_PATH: &str = "/bin/compiler.cwasm";
const HELIOS_PROCESS_ID_ENV: &str = "HELIOS_PROCESS_ID";
const RAYON_NUM_THREADS_ENV: &[u8] = b"RAYON_NUM_THREADS=";
const WASM_PAGE_SIZE: usize = 64 * 1024;
const PROGRAM_SHARED_MEMORY_MAX_PAGES: u32 = 8192;
const WASIX_ASYNCIFY_DATA_SIZE: u32 = 8;
const WASIX_STACK_SNAPSHOT_SIZE: usize = 24;
const WASIX_MODULE: &str = "wasix_32v1";

#[derive(Clone)]
pub struct UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    inner: Arc<UserProgramServiceInner<CpuImpl, HostFs>>,
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
    read_serial: fn(u32) -> Vec<u8>,
    write_serial: fn(&[u8]),
}

struct UserProgramServiceInner<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    runtime: crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    engine: crate::wasmtime_adapter::WasmtimeEngine,
    component_cache: Mutex<ComponentCache<WasmtimeCompiledComponent>>,
    core_module_cache: Mutex<ComponentCache<WasmtimeCompiledCoreModule>>,
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
    compile_lock: Mutex<()>,
    clock_cpu: CpuImpl,
    _marker: core::marker::PhantomData<fn() -> HostFs>,
}

/// Cached state of the compiler kernel plugin. Building it costs one
/// `Module::deserialize` + one `SharedMemory::new(8192 pages)` + one
/// `Linker::instantiate_pre`; the result is reused across compile
/// calls. Per-call work is reduced to a fresh `wasmtime::Store`,
/// `instance_pre.instantiate`, then `initialize` / `alloc` / `compile`.
struct CompilerPluginRuntime<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    instance_pre: Arc<InstancePre<CompilerCoreStore<CpuImpl, HostFs>>>,
    shared: Arc<CompilerCoreShared<CompilerCoreStore<CpuImpl, HostFs>>>,
}

struct ProgramSpawnRequest {
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
}

enum ProgramExecutable {
    Component(Arc<WasmtimeCompiledComponent>),
    CoreModule(Arc<WasmtimeCompiledCoreModule>),
    ForkedCoreModule {
        compiled: Arc<WasmtimeCompiledCoreModule>,
        restore: CoreModuleRestore,
    },
}

pub(crate) enum ProgramSource {
    RawWasm(Bytes),
    SignedArtifact(Bytes),
    BootfsArtifact(Bytes),
}

#[derive(Clone)]
struct CompilerCoreStore<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    cpu: CpuImpl,
    spawner: crate::Spawner<CpuImpl>,
    runtime_state: HostRuntimeState<CpuImpl, HostFs>,
    instance: Arc<crate::RegisteredInstance>,
    shared: Arc<CompilerCoreShared<CompilerCoreStore<CpuImpl, HostFs>>>,
    preview1_descriptors: CompilerPreview1Descriptors,
    write_serial: fn(&[u8]),
    _marker: core::marker::PhantomData<fn() -> HostFs>,
}

#[derive(Clone)]
struct CompilerPreview1Descriptors {
    stdout_open: bool,
    stderr_open: bool,
}

impl CompilerPreview1Descriptors {
    const fn new() -> Self {
        Self {
            stdout_open: true,
            stderr_open: true,
        }
    }

    fn can_write(&self, fd: i32) -> bool {
        match fd {
            1 => self.stdout_open,
            2 => self.stderr_open,
            _ => false,
        }
    }

    fn close(&mut self, fd: i32) -> i32 {
        match fd {
            1 if self.stdout_open => {
                self.stdout_open = false;
                p1::errno::SUCCESS
            }
            2 if self.stderr_open => {
                self.stderr_open = false;
                p1::errno::SUCCESS
            }
            _ => p1::errno::BADF,
        }
    }
}

struct CompilerCoreShared<T> {
    memory: SharedMemory,
    entropy: Mutex<crate::EntropyPool>,
    instance_pre: spin::Once<Arc<InstancePre<T>>>,
    next_thread_id: AtomicI32,
}

struct Preview1ProgramStore<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    cpu: CpuImpl,
    timer: crate::Timer<CpuImpl>,
    spawner: crate::Spawner<CpuImpl>,
    runtime_state: HostRuntimeState<CpuImpl, HostFs>,
    instance: crate::RegisteredInstance,
    parent_instance_id: Option<crate::InstanceId>,
    filesystem: DebugFileSystem<HostRuntimeState<CpuImpl, HostFs>, HostFs>,
    clock: crate::KernelClock<CpuImpl, HostRuntimeState<CpuImpl, HostFs>>,
    wall_clock_cap: Option<crate::SetWallClockCap>,
    cwd: Option<Preview1Cwd>,
    arguments: Vec<String>,
    environment: Vec<(String, String)>,
    output_mode: OutputMode,
    read_serial: fn(u32) -> Vec<u8>,
    write_serial: fn(&[u8]),
    imported_memory: Option<SharedMemory>,
    current_core_module: Option<Arc<WasmtimeCompiledCoreModule>>,
    entropy: crate::EntropyPool,
    authority: ProcessAuthority,
    tty_state: WasixTtyState,
    signal_callback: Option<String>,
    descriptors: Preview1DescriptorTable,
    asyncify: WasixAsyncifyState,
    children: Vec<WasixChildProcess>,
    requested_exit: Option<u32>,
}

struct WasixChildProcess {
    pid: u32,
    exit: Option<futures::channel::oneshot::Receiver<Result<ChildExit, ProgramExecError>>>,
    completed: Option<u32>,
}

struct WasixPreparedProgram {
    guest_name: String,
    source_path: String,
    source: ProgramSource,
}

struct CoreModuleRestore {
    memory: SharedMemory,
    descriptors: Preview1DescriptorTable,
    stack_lower: u32,
    stack_upper: u32,
    stack_pointer: u32,
    memory_stack: Vec<u8>,
    rewind_stack: Vec<u8>,
    value: u64,
}

#[derive(Clone)]
enum WasixOutputSink<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    Serial(fn(&[u8])),
    Trace {
        cpu: CpuImpl,
        runtime_state: HostRuntimeState<CpuImpl, HostFs>,
    },
    Child {
        stdout_tx: crate::ByteWriter,
        stderr_tx: crate::ByteWriter,
    },
    Discard,
}

impl<CpuImpl, HostFs> WasixOutputSink<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn write(&self, stream: crate::ComponentOutputStreamKind, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        match self {
            Self::Serial(write_serial) => write_serial(&bytes),
            Self::Trace { cpu, runtime_state } => {
                let text = core::str::from_utf8(&bytes).unwrap_or_else(|error| {
                    panic!("WASIX child wrote non-utf8 stdout/stderr bytes: {error}")
                });
                runtime_state.record_console_text(cpu.now().ticks(), text);
            }
            Self::Child {
                stdout_tx,
                stderr_tx,
            } => {
                let writer = match stream {
                    crate::ComponentOutputStreamKind::Stdout => stdout_tx,
                    crate::ComponentOutputStreamKind::Stderr => stderr_tx,
                };
                let _ = writer.write(bytes);
            }
            Self::Discard => {}
        }
    }
}

#[derive(Clone, Copy)]
struct WasixTtyState {
    cols: u32,
    rows: u32,
    width: u32,
    height: u32,
    stdin_tty: bool,
    stdout_tty: bool,
    stderr_tty: bool,
    echo: bool,
    line_buffered: bool,
    line_feeds: bool,
}

#[derive(Clone)]
struct Preview1Cwd {
    guest_name: String,
    descriptor: FsDescriptor,
}

#[derive(Clone)]
struct Preview1DescriptorTable {
    entries: Vec<Option<Preview1DescriptorEntry>>,
}

#[derive(Clone, Copy)]
struct Preview1Memory {
    base: usize,
    len: usize,
}

struct WasixAsyncifyState {
    snapshots: Vec<WasixStackSnapshot>,
    phase: WasixAsyncifyPhase,
    rewind_value: Option<u64>,
}

enum WasixAsyncifyPhase {
    Idle,
    Capturing {
        snapshot: u32,
        ret_value: u32,
        stack_lower: u32,
        stack_upper: u32,
        unwind_stack_begin: u32,
        memory_stack: Vec<u8>,
        stack_pointer: u32,
    },
    Restoring {
        hash: u128,
        value: u64,
        stack_lower: u32,
    },
    Forking {
        ret_pid: u32,
        stack_lower: u32,
        stack_upper: u32,
        unwind_stack_begin: u32,
        memory_stack: Vec<u8>,
        stack_pointer: u32,
    },
}

#[derive(Clone)]
struct WasixStackSnapshot {
    hash: u128,
    memory_stack: Vec<u8>,
    rewind_stack: Vec<u8>,
    stack_pointer: u32,
}

#[derive(Clone)]
struct Preview1DescriptorEntry {
    descriptor: Preview1Descriptor,
    close_on_exec: bool,
}

#[derive(Clone)]
enum Preview1Descriptor {
    Stdin {
        carry: Vec<u8>,
    },
    Stdout,
    Stderr,
    PipeRead {
        reader: crate::ByteReader,
        carry: Vec<u8>,
    },
    PipeWrite {
        writer: crate::ByteWriter,
    },
    Event(EventFd),
    Preopen {
        guest_name: String,
        descriptor: FsDescriptor,
    },
    File {
        descriptor: FsDescriptor,
        offset: u64,
        fdflags: u16,
    },
    Socket(WasixSocketDescriptor),
}

#[derive(Clone)]
enum WasixSocketDescriptor {
    Tcp(WasixTcpSocket),
    Udp(WasixUdpSocket),
    Pair {
        reader: crate::ByteReader,
        writer: crate::ByteWriter,
        carry: Vec<u8>,
    },
}

#[derive(Clone)]
enum WasixTcpSocket {
    Unconnected,
    Connected {
        stream: u64,
        peer_address: crate::Ipv4Address,
        peer_port: u16,
    },
}

#[derive(Clone)]
enum WasixUdpSocket {
    Unbound,
    Bound { socket: u64, local_port: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WasixSocketAuthority {
    LocalOnly,
    Tcp,
    Udp,
}

#[derive(Clone)]
struct EventFd {
    state: Arc<Mutex<EventFdState>>,
    notify: Arc<crate::Notify>,
    semaphore: bool,
}

struct EventFdState {
    value: u64,
}

#[derive(Debug, Error)]
#[error("guest requested wasi preview1 exit")]
struct Preview1Exit;

/// Handle to a spawned child component as seen by the kernel and its
/// direct Rust callers. WIT `child` resources wrap one of these.
pub struct ChildHandle {
    pub instance_id: crate::InstanceId,
    stdin: Option<crate::ByteWriter>,
    stdout: Option<crate::ByteReader>,
    stderr: Option<crate::ByteReader>,
    exit: Option<futures::channel::oneshot::Receiver<Result<ChildExit, ProgramExecError>>>,
}

impl ChildHandle {
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
}

pub fn install_program_service<CpuImpl, HostFs, WatchdogImpl>(
    _kernel: &crate::Kernel<CpuImpl, WatchdogImpl>,
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
) -> UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    install_program_service_inner(cpu, debug_state)
}

pub fn install_component_host_program_service<CpuImpl, HostFs, WatchdogImpl>(
    kernel: &crate::Kernel<CpuImpl, WatchdogImpl>,
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
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

    Some(install_program_service_inner(cpu, debug_state))
}

fn install_program_service_inner<CpuImpl, HostFs>(
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
) -> UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Some(service) = debug_state.program_service() {
        return service;
    }

    let available_bytes = heap_stats().available_bytes();
    let cache_budget = available_bytes / COMPONENT_CACHE_FRACTION;
    let runtime = crate::wasmtime_adapter::WasmtimeComponentRuntime::new(cpu.clone());
    let engine = <crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl> as crate::ComponentRuntimeFactory<CpuImpl, HostRuntimeState<CpuImpl, HostFs>, HostFs>>::create_engine(&runtime)
        .unwrap_or_else(|error| panic!("failed to create launched-program engine: {error:#}"));
    let compiler_artifact = read_bootfs_artifact(debug_state, COMPILER_PLUGIN_PATH);
    crate::wasmtime_adapter::register_oom_kick_engine(engine.raw().clone());
    debug_state
        .instance_registry()
        .set_kill_notifier(crate::wasmtime_adapter::bump_user_engine_epoch);
    let service = UserProgramService {
        inner: Arc::new(UserProgramServiceInner {
            runtime,
            engine,
            component_cache: Mutex::new(ComponentCache::new(cache_budget)),
            core_module_cache: Mutex::new(ComponentCache::new(cache_budget)),
            compiler_artifact,
            compiler_plugin: Mutex::new(None),
            compile_lock: Mutex::new(()),
            clock_cpu: cpu.clone(),
            _marker: core::marker::PhantomData,
        }),
    };
    debug_state.install_program_service(service.clone());
    service
}

pub fn run_embedded_component_forever<CpuImpl, HostFs, WatchdogImpl>(
    component: EmbeddedComponent,
    world: ComponentBindingSet,
    cpu: CpuImpl,
    kernel: &crate::Kernel<CpuImpl, WatchdogImpl>,
    debug_state: HostRuntimeState<CpuImpl, HostFs>,
    read_serial: fn(u32) -> Vec<u8>,
    write_serial: fn(&[u8]),
) -> !
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    let component_name = component.name();
    super::emit_stage_marker(write_serial, "boot");
    super::emit_stage_marker(write_serial, "component-host:trace-begin");
    tracing::info!(
        component = component_name,
        "launching embedded system component"
    );
    super::emit_stage_marker(write_serial, "component-host:run-local-begin");
    let stack = format!("kernel;system-component;{component_name};poll");
    kernel
        .run_local_future(ProfiledSystemComponentFuture::new(
            run_system_component(
                component,
                world,
                cpu.clone(),
                kernel.timer(),
                kernel.spawner(),
                debug_state.clone(),
                read_serial,
                write_serial,
            ),
            cpu.clone(),
            debug_state,
            stack,
        ))
        .unwrap_or_else(|error| {
            let message = alloc::format!("\n[KDBG error failed-system-component {error:#}]\n");
            write_serial(message.as_bytes());
            panic!("failed to exec embedded system component:\n{error:#}");
        });
    super::emit_stage_marker(write_serial, "done");
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
            this.debug_state.record_profile_stack(
                ProfileScope::Kernel,
                this.stack.clone(),
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
    read_serial: fn(u32) -> Vec<u8>,
    write_serial: fn(&[u8]),
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
            let component = crate::embedded_system_component()
                .unwrap_or_else(|| panic!("embedded init bootfs is missing the system component"));
            run_embedded_component_forever(
                component,
                ComponentBindingSet::System,
                cpu,
                &kernel,
                debug_state,
                read_serial,
                write_serial,
            );
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
    let stack = format!("kernel;executor;processor-{processor}");
    loop {
        let started = cpu.now().ticks();
        let progress = kernel.run_until_stalled();
        let elapsed = cpu.now().ticks().saturating_sub(started);
        if progress != 0 && debug_state.profiling_enabled() {
            debug_state.record_profile_stack(ProfileScope::Kernel, stack.clone(), elapsed);
        }
        if progress != 0 {
            continue;
        }

        cpu.park_current();
    }
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
    pub(crate) async fn spawn(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        name: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        source: ProgramSource,
        hint: Option<AotCompileHint>,
        authority: ProcessAuthority,
    ) -> Result<ChildHandle, ProgramExecError> {
        super::emit_stage_marker(exec_context.write_serial, "program:spawn-begin");
        let executable = self
            .load_executable(&exec_context, &source, hint, exec_context.write_serial)
            .await?;
        self.spawn_loaded(exec_context, name, args, env, executable, authority)
    }

    fn spawn_loaded(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        name: String,
        args: Vec<String>,
        mut env: Vec<(String, String)>,
        executable: ProgramExecutable,
        authority: ProcessAuthority,
    ) -> Result<ChildHandle, ProgramExecError> {
        // Three byte channels between parent and child.
        let (stdin_writer, stdin_reader) = crate::byte_channel();
        let (stdout_writer, stdout_reader) = crate::byte_channel();
        let (stderr_writer, stderr_reader) = crate::byte_channel();

        // Register the child instance synchronously on the parent
        // thread so we can return its id in the handle immediately.
        let started_at = exec_context
            .runtime_state
            .uptime_nanos(exec_context.cpu.now().ticks());
        let launched_instance = exec_context
            .instance_registry
            .register(name.clone(), started_at);
        let instance_id = launched_instance.id();
        assert!(
            !env.iter()
                .any(|(name, _)| name.as_str() == HELIOS_PROCESS_ID_ENV),
            "{HELIOS_PROCESS_ID_ENV} is reserved for the kernel program launcher"
        );
        env.push((HELIOS_PROCESS_ID_ENV.into(), instance_id.raw().to_string()));
        let request = ProgramSpawnRequest {
            name,
            args,
            env,
            authority,
        };

        let (exit_tx, exit_rx) = futures::channel::oneshot::channel();
        let runtime = self.inner.runtime.clone();
        let engine = self.inner.engine.clone();
        let spawner = exec_context.spawner.clone();
        let run_spawner = spawner.clone();
        let progress = spawner.progress_counter();

        spawner.spawn_detached(async move {
            let result = run_program_executable(
                exec_context,
                request.name,
                request.args,
                request.env,
                request.authority,
                run_spawner,
                progress,
                executable,
                &engine,
                &runtime,
                launched_instance,
                stdin_reader,
                stdout_writer,
                stderr_writer,
            )
            .await;
            let _ = exit_tx.send(result);
        });

        let child = ChildHandle {
            instance_id,
            stdin: Some(stdin_writer),
            stdout: Some(stdout_reader),
            stderr: Some(stderr_reader),
            exit: Some(exit_rx),
        };
        Ok(child)
    }

    /// Convenience wrapper: spawn a program, feed it `stdin`, drain its
    /// stdout and stderr into buffers, and return the collected output
    /// along with the exit code.
    pub(crate) async fn exec_buffered(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        name: impl Into<String>,
        args: Vec<String>,
        env: Vec<(String, String)>,
        source: ProgramSource,
        hint: Option<AotCompileHint>,
        stdin: Vec<u8>,
        authority: ProcessAuthority,
    ) -> Result<ExecResult, ProgramExecError> {
        let executable = self
            .load_executable(&exec_context, &source, hint, exec_context.write_serial)
            .await?;
        self.exec_loaded_buffered(
            exec_context,
            name.into(),
            args,
            env,
            executable,
            stdin,
            authority,
        )
        .await
    }

    async fn exec_loaded_buffered(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        name: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        executable: ProgramExecutable,
        stdin: Vec<u8>,
        authority: ProcessAuthority,
    ) -> Result<ExecResult, ProgramExecError> {
        let mut child = self.spawn_loaded(exec_context, name, args, env, executable, authority)?;

        // Feed stdin in one shot, then close the writer to signal EOF.
        if let Some(writer) = child.take_stdin() {
            if !stdin.is_empty() {
                let _ = writer.write(stdin);
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
        write_serial: fn(&[u8]),
    ) -> Result<ProgramExecutable, ProgramExecError> {
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
                let hint = hint.unwrap_or(AotCompileHint::Balanced);
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
        write_serial: fn(&[u8]),
        started_at: u64,
    ) -> Result<ProgramExecutable, ProgramExecError> {
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
        write_serial: fn(&[u8]),
        started_at: u64,
    ) -> Result<Arc<WasmtimeCompiledComponent>, ProgramExecError> {
        if let Some(component) = self.inner.component_cache.lock().get(payload.as_ref()) {
            super::emit_stage_marker(write_serial, "program:deserialize-cache-hit");
            let now = monotonic_nanos(&self.inner.clock_cpu);
            tracing::info!(
                target: "helios_component_host::program_host",
                phase = "deserialize-component",
                cache = "hit",
                cwasm_bytes = payload.len(),
                elapsed_ms = elapsed_millis(started_at, now),
                "program component deserialization cache hit"
            );
            return Ok(component);
        }

        super::emit_stage_marker(write_serial, "program:deserialize-begin");
        tracing::info!(
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
        super::emit_stage_marker(write_serial, "program:deserialize-end");
        let component = Arc::new(compiled);
        let now = monotonic_nanos(&self.inner.clock_cpu);
        tracing::info!(
            target: "helios_component_host::program_host",
            phase = "deserialize-component",
            cache = "miss",
            cwasm_bytes = payload.len(),
            elapsed_ms = elapsed_millis(started_at, now),
            "program component deserialized"
        );
        Ok(self
            .inner
            .component_cache
            .lock()
            .insert_if_missing(payload, component))
    }

    fn load_precompiled_core_module(
        &self,
        payload: Bytes,
        write_serial: fn(&[u8]),
        started_at: u64,
    ) -> Result<Arc<WasmtimeCompiledCoreModule>, ProgramExecError> {
        if let Some(module) = self.inner.core_module_cache.lock().get(payload.as_ref()) {
            super::emit_stage_marker(write_serial, "program:deserialize-core-cache-hit");
            let now = monotonic_nanos(&self.inner.clock_cpu);
            tracing::info!(
                target: "helios_component_host::program_host",
                phase = "deserialize-core-module",
                cache = "hit",
                cwasm_bytes = payload.len(),
                elapsed_ms = elapsed_millis(started_at, now),
                "program core module deserialization cache hit"
            );
            return Ok(module);
        }

        super::emit_stage_marker(write_serial, "program:deserialize-core-begin");
        let module = unsafe { Module::deserialize(self.inner.engine.raw(), payload.as_ref()) }
            .map_err(map_program_runtime_error)?;
        validate_preview1_program_module_imports(&module)?;
        super::emit_stage_marker(write_serial, "program:deserialize-core-end");
        let compiled = Arc::new(WasmtimeCompiledCoreModule { module });
        let now = monotonic_nanos(&self.inner.clock_cpu);
        tracing::info!(
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
                    detail: ProgramExecErrorDetail::RuntimeFailure,
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
        let payload =
            self.invoke_compiler_core_module(exec_context, compiler_payload, wasm, hint, profile)?;
        let signed =
            cwasm::sign_trusted_artifact_payload(&payload).map_err(map_artifact_trust_error)?;
        cwasm::verify_signed_artifact(UntrustedCwasm::new(&signed))
            .map_err(map_artifact_trust_error)?;
        Ok(signed)
    }

    fn invoke_compiler_core_module(
        &self,
        exec_context: &ProgramExecContext<CpuImpl, HostFs>,
        compiler_payload: Bytes,
        wasm: &Bytes,
        hint: AotCompileHint,
        profile: bool,
    ) -> Result<Vec<u8>, ProgramExecError> {
        let _compile_guard = self.inner.compile_lock.lock();
        let result =
            self.invoke_compiler_inner(exec_context, &compiler_payload, wasm, hint, profile);
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
        let compiler_instance = exec_context.instance_registry.register_with_cost(
            "compiler-plugin",
            started_at,
            crate::PLUGIN_RESTART_COST,
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
        let thread_pointer =
            tls_base
                .checked_add(pthread_self_offset)
                .ok_or_else(|| ProgramExecError {
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
            (store.data().write_serial)(&diagnostic);
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
            .ok_or_else(|| ProgramExecError {
                kind: ProgramExecErrorKind::InvalidPath,
                detail: ProgramExecErrorDetail::CompilerUnavailable,
            })
    }

    /// Build the compiler kernel-plugin runtime on first compile, reuse
    /// it forever after. The cached `wasmtime::Module` + `InstancePre`
    /// + 512 MiB `SharedMemory` are stable across calls; per-call work
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

        match WasmtimePrecompiledKind::detect(compiler_payload) {
            Some(WasmtimePrecompiledKind::CoreModule) => {}
            Some(WasmtimePrecompiledKind::Component) => {
                return Err(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::CompilerPluginInvalid,
                });
            }
            None => {
                return Err(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::CompilerPluginInvalid,
                });
            }
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
            entropy: Mutex::new(crate::EntropyPool::from_cpu(&exec_context.cpu, 0)),
            instance_pre: spin::Once::new(),
            next_thread_id: AtomicI32::new(0),
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
        let scratch_instance = exec_context.instance_registry.register_with_cost(
            "compiler-plugin-init",
            scratch_started_at,
            crate::PLUGIN_RESTART_COST,
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

fn read_bootfs_artifact<CpuImpl, HostFs>(
    runtime_state: &HostRuntimeState<CpuImpl, HostFs>,
    path: &str,
) -> Option<Bytes>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let filesystem = crate::wasmtime_adapter::wasi::DebugFileSystem::<
        HostRuntimeState<CpuImpl, HostFs>,
        HostFs,
    >::new(runtime_state.clone());
    filesystem.read_program_file_bytes(path).ok()
}

impl<CpuImpl, HostFs> Preview1ProgramStore<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    #[allow(clippy::too_many_arguments)]
    fn new(
        cpu: CpuImpl,
        timer: crate::Timer<CpuImpl>,
        spawner: crate::Spawner<CpuImpl>,
        runtime_state: HostRuntimeState<CpuImpl, HostFs>,
        instance: crate::RegisteredInstance,
        parent_instance_id: Option<crate::InstanceId>,
        arguments: Vec<String>,
        environment: Vec<(String, String)>,
        authority: ProcessAuthority,
        output_mode: OutputMode,
        read_serial: fn(u32) -> Vec<u8>,
        write_serial: fn(&[u8]),
        imported_memory: Option<SharedMemory>,
        current_core_module: Option<Arc<WasmtimeCompiledCoreModule>>,
    ) -> Self {
        let filesystem = DebugFileSystem::new(runtime_state.clone());
        let entropy = crate::EntropyPool::from_cpu(&cpu, instance.id().raw());
        let descriptors = Preview1DescriptorTable::from_authority(&authority);
        let clock = crate::KernelClock::new(cpu.clone(), runtime_state.clone());
        let wall_clock_cap = authority.derive_set_wall_clock_cap().ok();
        let cwd = preview1_cwd_from_authority(&authority);
        let tty_state = WasixTtyState::from_authority(&authority);
        Self {
            cpu,
            timer,
            spawner,
            runtime_state,
            instance,
            parent_instance_id,
            filesystem,
            clock,
            wall_clock_cap,
            cwd,
            arguments,
            environment,
            output_mode,
            read_serial,
            write_serial,
            imported_memory,
            current_core_module,
            entropy,
            authority,
            tty_state,
            signal_callback: None,
            descriptors,
            asyncify: WasixAsyncifyState::new(),
            children: Vec::new(),
            requested_exit: None,
        }
    }

    fn now_nanos(&self) -> u64 {
        self.runtime_state.uptime_nanos(self.cpu.now().ticks())
    }

    fn system_time_nanos(&self) -> u64 {
        self.clock.system_time_nanos()
    }

    fn futex_key(&self, address: u32) -> crate::FutexKey {
        crate::FutexKey::new(
            crate::ProcessMemoryIdentity::new(self.instance.id().raw()),
            crate::GuestAddress::new(u64::from(address)),
        )
    }

    fn set_system_time_nanos(&mut self, nanos: u64) -> i32 {
        let Some(cap) = &self.wall_clock_cap else {
            return p1::errno::NOTCAPABLE;
        };
        self.clock.set_system_time_nanos(cap, nanos);
        p1::errno::SUCCESS
    }

    fn require_tty_control(&self) -> i32 {
        self.authority
            .derive_tty_control_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    fn require_signal_authority(&self) -> i32 {
        self.authority
            .derive_signal_authority()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    fn require_dns_authority(&self) -> i32 {
        self.authority
            .derive_dns_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    fn require_tcp_authority(&self) -> i32 {
        self.authority
            .derive_tcp_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    fn require_udp_authority(&self) -> i32 {
        self.authority
            .derive_udp_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    fn require_socket_authority(&self, authority: WasixSocketAuthority) -> i32 {
        match authority {
            WasixSocketAuthority::LocalOnly => p1::errno::SUCCESS,
            WasixSocketAuthority::Tcp => self.require_tcp_authority(),
            WasixSocketAuthority::Udp => self.require_udp_authority(),
        }
    }

    fn require_multicast_authority(&self) -> i32 {
        self.authority
            .derive_multicast_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    fn require_network_admin_authority(&self) -> i32 {
        self.authority
            .derive_network_admin_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    fn require_privileged_bind_authority(&self) -> i32 {
        self.authority
            .derive_privileged_bind_cap()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    fn require_spawn_authority(&self) -> i32 {
        self.authority
            .derive_spawn_authority()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    fn require_exec_authority(&self) -> i32 {
        self.authority
            .derive_exec_authority()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    fn require_fork_authority(&self) -> i32 {
        self.authority
            .derive_fork_authority()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    fn require_join_authority(&self) -> i32 {
        self.authority
            .derive_join_authority()
            .map_or(p1::errno::NOTCAPABLE, |_| p1::errno::SUCCESS)
    }

    fn request_exit(&mut self, code: u32) {
        self.requested_exit = Some(code);
    }

    fn take_requested_exit(&mut self) -> Option<u32> {
        self.requested_exit.take()
    }

    fn exec_context(&self) -> ProgramExecContext<CpuImpl, HostFs> {
        ProgramExecContext {
            cpu: self.cpu.clone(),
            timer: self.timer.clone(),
            spawner: self.spawner.clone(),
            runtime_state: self.runtime_state.clone(),
            instance_registry: self.runtime_state.instance_registry(),
            parent_instance_id: Some(self.instance.id()),
            read_serial: self.read_serial,
            write_serial: self.write_serial,
        }
    }

    fn record_transition(&self, transition: crate::InstanceExecutionTransition) {
        let now_nanos = self.now_nanos();
        let elapsed = crate::record_instance_transition(&self.instance, transition, now_nanos);
        if let Some(elapsed) = elapsed
            && self.runtime_state.profiling_enabled()
        {
            self.runtime_state.record_profile_stack_nanos(
                crate::ProfileScope::User,
                format!("user;{}", self.instance.name()),
                elapsed,
            );
        }
    }

    fn check_pending_kill(&self) -> Option<crate::KillReason> {
        self.instance.pending_kill()
    }

    fn write_output(&self, stream: crate::ComponentOutputStreamKind, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        match &self.output_mode {
            OutputMode::Serial => (self.write_serial)(bytes),
            OutputMode::Trace => {
                let text = core::str::from_utf8(bytes).unwrap_or_else(|error| {
                    panic!("Preview1 guest wrote non-utf8 stdout/stderr bytes: {error}")
                });
                self.runtime_state
                    .record_console_text(self.cpu.now().ticks(), text);
            }
            OutputMode::Child {
                stdout_tx,
                stderr_tx,
                ..
            } => {
                let writer = match stream {
                    crate::ComponentOutputStreamKind::Stdout => stdout_tx,
                    crate::ComponentOutputStreamKind::Stderr => stderr_tx,
                };
                let _ = writer.write(bytes.to_vec());
            }
        }
    }

    fn output_sink(&self) -> WasixOutputSink<CpuImpl, HostFs> {
        match &self.output_mode {
            OutputMode::Serial => WasixOutputSink::Serial(self.write_serial),
            OutputMode::Trace => WasixOutputSink::Trace {
                cpu: self.cpu.clone(),
                runtime_state: self.runtime_state.clone(),
            },
            OutputMode::Child {
                stdout_tx,
                stderr_tx,
                ..
            } => WasixOutputSink::Child {
                stdout_tx: stdout_tx.clone(),
                stderr_tx: stderr_tx.clone(),
            },
        }
    }

    fn insert_child(
        &mut self,
        pid: u32,
        exit: futures::channel::oneshot::Receiver<Result<ChildExit, ProgramExecError>>,
    ) {
        self.children.push(WasixChildProcess {
            pid,
            exit: Some(exit),
            completed: None,
        });
    }

    fn poll_child_exit(&mut self, index: usize) -> Result<Option<u32>, i32> {
        if let Some(code) = self.children[index].completed {
            return Ok(Some(code));
        }
        let Some(exit) = self.children[index].exit.as_mut() else {
            return Ok(self.children[index].completed);
        };
        let waker = futures::task::noop_waker_ref();
        let mut context = Context::from_waker(waker);
        match Pin::new(exit).poll(&mut context) {
            Poll::Pending => Ok(None),
            Poll::Ready(Ok(Ok(exit))) => {
                let code = exit.exit_code;
                self.children[index].completed = Some(code);
                self.children[index].exit = None;
                Ok(Some(code))
            }
            Poll::Ready(Ok(Err(_))) | Poll::Ready(Err(_)) => {
                let code = u32::from(p1::errno::IO as u16);
                self.children[index].completed = Some(code);
                self.children[index].exit = None;
                Ok(Some(code))
            }
        }
    }

    fn find_child_index(&self, pid: Option<u32>) -> Option<usize> {
        match pid {
            Some(pid) => self.children.iter().position(|child| child.pid == pid),
            None => {
                if let Some(index) = self
                    .children
                    .iter()
                    .position(|child| child.completed.is_some())
                {
                    return Some(index);
                }
                (!self.children.is_empty()).then_some(0)
            }
        }
    }

    async fn read_stdin(&mut self, max_bytes: usize) -> Vec<u8> {
        let descriptor = self.descriptors.get_mut(0);
        let Some(Preview1Descriptor::Stdin { carry }) = descriptor else {
            return Vec::new();
        };
        if carry.is_empty() {
            match &self.output_mode {
                OutputMode::Serial => loop {
                    let bytes = (self.read_serial)(u32::MAX);
                    if !bytes.is_empty() {
                        *carry = bytes;
                        break;
                    }
                    crate::yield_now().await;
                },
                OutputMode::Trace => {}
                OutputMode::Child { stdin_rx, .. } => {
                    if let Some(bytes) = stdin_rx.read().await {
                        *carry = bytes;
                    }
                }
            }
        }
        if carry.len() <= max_bytes {
            core::mem::take(carry)
        } else {
            let tail = carry.split_off(max_bytes);
            core::mem::replace(carry, tail)
        }
    }

    async fn read_pipe(&mut self, fd: i32, max_bytes: usize) -> Result<Vec<u8>, i32> {
        let reader = match self.descriptors.get_mut(fd) {
            Some(Preview1Descriptor::PipeRead { reader, carry }) => {
                if !carry.is_empty() {
                    return Ok(take_preview1_carry(carry, max_bytes));
                }
                reader.clone()
            }
            Some(_) => return Err(p1::errno::BADF),
            None => return Err(p1::errno::BADF),
        };

        let bytes = reader.read().await.unwrap_or_default();
        let Some(Preview1Descriptor::PipeRead { carry, .. }) = self.descriptors.get_mut(fd) else {
            return Err(p1::errno::BADF);
        };
        *carry = bytes;
        Ok(take_preview1_carry(carry, max_bytes))
    }

    async fn read_socket_pair(&mut self, fd: i32, max_bytes: usize) -> Result<Vec<u8>, i32> {
        let reader = match self.descriptors.get_mut(fd) {
            Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair {
                reader, carry, ..
            })) => {
                if !carry.is_empty() {
                    return Ok(take_preview1_carry(carry, max_bytes));
                }
                reader.clone()
            }
            Some(Preview1Descriptor::Socket(_)) => return Err(p1::errno::INVAL),
            Some(_) => return Err(p1::errno::NOTSOCK),
            None => return Err(p1::errno::BADF),
        };

        let bytes = reader.read().await.unwrap_or_default();
        let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { carry, .. })) =
            self.descriptors.get_mut(fd)
        else {
            return Err(p1::errno::BADF);
        };
        *carry = bytes;
        Ok(take_preview1_carry(carry, max_bytes))
    }

    fn getcwd(&self) -> Result<&str, i32> {
        self.cwd
            .as_ref()
            .map(|cwd| cwd.guest_name.as_str())
            .ok_or(p1::errno::NOTCAPABLE)
    }

    fn chdir(&mut self, path: &str) -> i32 {
        let cwd = match self.resolve_cwd_target(path) {
            Ok(cwd) => cwd,
            Err(errno) => return errno,
        };
        let cap = match self.authority.derive_directory_cap(
            &cwd.descriptor.path,
            &cwd.guest_name,
            DirectoryAuthorityRights::READ,
        ) {
            Ok(cap) => cap,
            Err(_) => return p1::errno::NOTCAPABLE,
        };
        self.authority.chdir(cap);
        self.cwd = Some(cwd);
        p1::errno::SUCCESS
    }

    fn resolve_cwd_target(&self, path: &str) -> Result<Preview1Cwd, i32> {
        let (guest_name, source_path, flags) = if path.starts_with('/') {
            let guest_name =
                crate::resolve_absolute_path(path).map_err(p1_errno_from_component_path)?;
            let (source_path, flags) = self.resolve_absolute_guest_path(&guest_name)?;
            (guest_name, source_path, flags)
        } else {
            let cwd = self.cwd.as_ref().ok_or(p1::errno::NOTCAPABLE)?;
            let guest_name = crate::resolve_child_path(&cwd.guest_name, path)
                .map_err(p1_errno_from_component_path)?;
            let source_path = crate::resolve_child_path(&cwd.descriptor.path, path)
                .map_err(p1_errno_from_component_path)?;
            (guest_name, source_path, cwd.descriptor.flags)
        };

        if !flags.contains(fs_types::DescriptorFlags::READ) {
            return Err(p1::errno::NOTCAPABLE);
        }
        let stat = self
            .filesystem
            .stat(&source_path)
            .map_err(p1_errno_from_fs)?;
        if !matches!(stat.type_, fs_types::DescriptorType::Directory) {
            return Err(p1::errno::NOTDIR);
        }
        Ok(Preview1Cwd {
            guest_name,
            descriptor: FsDescriptor {
                path: source_path,
                kind: FsNodeKind::Directory,
                flags,
                identity: None,
            },
        })
    }

    fn resolve_absolute_guest_path(
        &self,
        guest_name: &str,
    ) -> Result<(String, fs_types::DescriptorFlags), i32> {
        let (descriptor, suffix) = self.resolve_absolute_guest_base(guest_name)?;
        let source_path = if suffix.is_empty() {
            descriptor.path.clone()
        } else {
            crate::resolve_child_path(&descriptor.path, &suffix)
                .map_err(p1_errno_from_component_path)?
        };
        Ok((source_path, descriptor.flags))
    }

    fn resolve_absolute_guest_base(&self, guest_name: &str) -> Result<(FsDescriptor, String), i32> {
        let mut best: Option<(&str, &FsDescriptor)> = None;
        for entry in &self.descriptors.entries {
            let Some(entry) = entry else {
                continue;
            };
            let Preview1Descriptor::Preopen {
                guest_name: preopen_guest,
                descriptor,
            } = &entry.descriptor
            else {
                continue;
            };
            if !guest_path_is_within_preopen(guest_name, preopen_guest) {
                continue;
            }
            if best.is_none_or(|(best_guest, _)| preopen_guest.len() > best_guest.len()) {
                best = Some((preopen_guest.as_str(), descriptor));
            }
        }

        let Some((preopen_guest, descriptor)) = best else {
            return Err(p1::errno::NOTCAPABLE);
        };
        let suffix = guest_path_suffix(guest_name, preopen_guest);
        Ok((descriptor.clone(), suffix.to_owned()))
    }

    fn resolve_wasix_path_base(&self, fd: i32, path: &str) -> Result<(FsDescriptor, String), i32> {
        if path.starts_with('/') {
            let guest_name =
                crate::resolve_absolute_path(path).map_err(p1_errno_from_component_path)?;
            return self.resolve_absolute_guest_base(&guest_name);
        }
        let base = match self.descriptors.get(fd) {
            Some(Preview1Descriptor::Preopen { descriptor, .. })
            | Some(Preview1Descriptor::File { descriptor, .. })
                if descriptor.kind == FsNodeKind::Directory =>
            {
                descriptor.clone()
            }
            Some(_) => return Err(p1::errno::NOTDIR),
            None => return Err(p1::errno::BADF),
        };
        Ok((base, path.to_owned()))
    }
}

impl WasixAsyncifyState {
    const fn new() -> Self {
        Self {
            snapshots: Vec::new(),
            phase: WasixAsyncifyPhase::Idle,
            rewind_value: None,
        }
    }
}

fn preview1_cwd_from_authority(authority: &ProcessAuthority) -> Option<Preview1Cwd> {
    authority.cwd().map(|preopen| Preview1Cwd {
        guest_name: preopen.guest_name().to_owned(),
        descriptor: FsDescriptor {
            path: preopen.source_path().to_owned(),
            kind: FsNodeKind::Directory,
            flags: directory_authority_to_descriptor_flags(preopen.rights()),
            identity: None,
        },
    })
}

fn take_preview1_carry(carry: &mut Vec<u8>, max_bytes: usize) -> Vec<u8> {
    if carry.len() <= max_bytes {
        core::mem::take(carry)
    } else {
        let tail = carry.split_off(max_bytes);
        core::mem::replace(carry, tail)
    }
}

fn guest_path_is_within_preopen(path: &str, preopen: &str) -> bool {
    if path == preopen {
        return true;
    }
    let prefix = crate::directory_prefix(preopen);
    path.starts_with(&prefix)
}

fn guest_path_suffix<'a>(path: &'a str, preopen: &str) -> &'a str {
    if preopen == "/" {
        path.strip_prefix('/').unwrap_or(path)
    } else {
        path.strip_prefix(preopen)
            .and_then(|suffix| suffix.strip_prefix('/'))
            .unwrap_or("")
    }
}

impl Preview1DescriptorTable {
    fn from_authority(authority: &ProcessAuthority) -> Self {
        let mut table = Self {
            entries: vec![
                Some(Preview1DescriptorEntry::new(
                    Preview1Descriptor::Stdin { carry: Vec::new() },
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
            ],
        };
        for preopen in authority.directory_preopens() {
            let descriptor = FsDescriptor {
                path: preopen.source_path().to_owned(),
                kind: FsNodeKind::Directory,
                flags: directory_authority_to_descriptor_flags(preopen.rights()),
                identity: None,
            };
            table.entries.push(Some(Preview1DescriptorEntry::new(
                Preview1Descriptor::Preopen {
                    guest_name: preopen.guest_name().to_owned(),
                    descriptor,
                },
                false,
            )));
        }
        table
    }

    fn get(&self, fd: i32) -> Option<&Preview1Descriptor> {
        usize::try_from(fd)
            .ok()
            .and_then(|index| self.entries.get(index))
            .and_then(Option::as_ref)
            .map(|entry| &entry.descriptor)
    }

    fn get_mut(&mut self, fd: i32) -> Option<&mut Preview1Descriptor> {
        usize::try_from(fd)
            .ok()
            .and_then(|index| self.entries.get_mut(index))
            .and_then(Option::as_mut)
            .map(|entry| &mut entry.descriptor)
    }

    fn insert(&mut self, descriptor: Preview1Descriptor) -> Result<u32, i32> {
        self.insert_with_close_on_exec(descriptor, false)
    }

    fn insert_with_close_on_exec(
        &mut self,
        descriptor: Preview1Descriptor,
        close_on_exec: bool,
    ) -> Result<u32, i32> {
        if let Some((index, slot)) = self
            .entries
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.is_none())
        {
            *slot = Some(Preview1DescriptorEntry::new(descriptor, close_on_exec));
            return u32::try_from(index).map_err(|_| p1::errno::OVERFLOW);
        }
        let index = self.entries.len();
        self.entries.push(Some(Preview1DescriptorEntry::new(
            descriptor,
            close_on_exec,
        )));
        u32::try_from(index).map_err(|_| p1::errno::OVERFLOW)
    }

    fn dup(&mut self, fd: i32) -> Result<u32, i32> {
        let descriptor = self.get(fd).cloned().ok_or(p1::errno::BADF)?;
        self.insert(descriptor)
    }

    fn dup_to(&mut self, fd: i32, to_fd: i32, close_on_exec: bool) -> Result<u32, i32> {
        let descriptor = self.get(fd).cloned().ok_or(p1::errno::BADF)?;
        let to = usize::try_from(to_fd).map_err(|_| p1::errno::BADF)?;
        if self.entries.len() <= to {
            self.entries.resize_with(to + 1, || None);
        }
        self.entries[to] = Some(Preview1DescriptorEntry::new(descriptor, close_on_exec));
        u32::try_from(to).map_err(|_| p1::errno::OVERFLOW)
    }

    fn close_on_exec(&self, fd: i32) -> Result<bool, i32> {
        usize::try_from(fd)
            .ok()
            .and_then(|index| self.entries.get(index))
            .and_then(Option::as_ref)
            .map(|entry| entry.close_on_exec)
            .ok_or(p1::errno::BADF)
    }

    fn set_close_on_exec(&mut self, fd: i32, close_on_exec: bool) -> i32 {
        match usize::try_from(fd)
            .ok()
            .and_then(|index| self.entries.get_mut(index))
            .and_then(Option::as_mut)
        {
            Some(entry) => {
                entry.close_on_exec = close_on_exec;
                p1::errno::SUCCESS
            }
            None => p1::errno::BADF,
        }
    }

    fn close(&mut self, fd: i32) -> i32 {
        match fd {
            0..=2 => p1::errno::BADF,
            _ => usize::try_from(fd)
                .ok()
                .and_then(|index| self.entries.get_mut(index))
                .and_then(Option::take)
                .map_or(p1::errno::BADF, |_| p1::errno::SUCCESS),
        }
    }

    fn renumber(&mut self, from: i32, to: i32) -> i32 {
        let Ok(from) = usize::try_from(from) else {
            return p1::errno::BADF;
        };
        let Ok(to) = usize::try_from(to) else {
            return p1::errno::BADF;
        };
        if from >= self.entries.len() || self.entries[from].is_none() {
            return p1::errno::BADF;
        }
        if to >= self.entries.len() {
            self.entries.resize_with(to + 1, || None);
        }
        self.entries[to] = self.entries[from].take();
        p1::errno::SUCCESS
    }
}

impl Preview1DescriptorEntry {
    const fn new(descriptor: Preview1Descriptor, close_on_exec: bool) -> Self {
        Self {
            descriptor,
            close_on_exec,
        }
    }
}

impl EventFd {
    fn new(value: u64, semaphore: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(EventFdState { value })),
            notify: Arc::new(crate::Notify::new()),
            semaphore,
        }
    }

    fn write(&self, increment: u64) -> Result<(), i32> {
        if increment == u64::MAX {
            return Err(p1::errno::INVAL);
        }
        let mut state = self.state.lock();
        let next = state
            .value
            .checked_add(increment)
            .filter(|value| *value != u64::MAX)
            .ok_or(p1::errno::AGAIN)?;
        state.value = next;
        drop(state);
        self.notify.notify_all();
        Ok(())
    }

    async fn read(&self) -> u64 {
        loop {
            {
                let mut state = self.state.lock();
                if state.value != 0 {
                    if self.semaphore {
                        state.value -= 1;
                        return 1;
                    }
                    return core::mem::take(&mut state.value);
                }
            }
            self.notify.notified().await;
        }
    }
}

impl WasixTtyState {
    fn from_authority(authority: &ProcessAuthority) -> Self {
        let rights = authority.terminal_rights();
        let input = rights.contains(TerminalAuthorityRights::INPUT);
        let output = rights.contains(TerminalAuthorityRights::OUTPUT);
        Self {
            cols: 80,
            rows: 24,
            width: 0,
            height: 0,
            stdin_tty: input,
            stdout_tty: output,
            stderr_tty: output,
            echo: input,
            line_buffered: input,
            line_feeds: true,
        }
    }
}

fn directory_authority_to_descriptor_flags(
    rights: DirectoryAuthorityRights,
) -> fs_types::DescriptorFlags {
    let mut flags = fs_types::DescriptorFlags::empty();
    if rights.contains(DirectoryAuthorityRights::READ) {
        flags |= fs_types::DescriptorFlags::READ;
    }
    if rights.contains(DirectoryAuthorityRights::WRITE) {
        flags |= fs_types::DescriptorFlags::WRITE;
    }
    if rights.contains(DirectoryAuthorityRights::MUTATE_DIRECTORY) {
        flags |= fs_types::DescriptorFlags::MUTATE_DIRECTORY;
    }
    flags
}

impl<CpuImpl, HostFs> CompilerCoreStore<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn memory(&self) -> &SharedMemory {
        &self.shared.memory
    }

    fn record_user_ticks(&self, ticks: u64) {
        if self.runtime_state.profiling_enabled() {
            self.runtime_state.record_profile_stack(
                ProfileScope::User,
                format!("user;{}", self.instance.name()),
                ticks,
            );
        }
    }
}

fn imported_shared_memory_with_declared_maximum(
    engine: &wasmtime::Engine,
    module: &Module,
) -> Result<Option<SharedMemory>, ProgramExecError> {
    imported_shared_memory(engine, module, None)
}

fn imported_shared_memory_with_user_budget(
    engine: &wasmtime::Engine,
    module: &Module,
) -> Result<Option<SharedMemory>, ProgramExecError> {
    let available_pages = user_heap_stats().available_bytes() / WASM_PAGE_SIZE;
    let available_pages = u32::try_from(available_pages).unwrap_or(u32::MAX);
    let budget_pages = available_pages.min(PROGRAM_SHARED_MEMORY_MAX_PAGES);
    imported_shared_memory(engine, module, Some(budget_pages))
}

fn imported_shared_memory(
    engine: &wasmtime::Engine,
    module: &Module,
    maximum_pages_budget: Option<u32>,
) -> Result<Option<SharedMemory>, ProgramExecError> {
    let mut memory_type = None;
    for import in module.imports() {
        if import.module() == "env" && import.name() == "memory" {
            memory_type = import.ty().memory().cloned();
            break;
        }
    }
    let Some(memory_type) = memory_type else {
        return Ok(None);
    };
    if !memory_type.is_shared() {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::ImportedSharedMemoryContractInvalid,
        });
    }
    let maximum_pages = memory_type.maximum().ok_or_else(|| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::ImportedSharedMemoryContractInvalid,
    })?;
    let initial_pages = u32::try_from(memory_type.minimum()).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::ImportedSharedMemoryContractInvalid,
    })?;
    let declared_maximum_pages = u32::try_from(maximum_pages).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::ImportedSharedMemoryContractInvalid,
    })?;
    let maximum_pages = maximum_pages_budget
        .map(|budget| declared_maximum_pages.min(budget))
        .unwrap_or(declared_maximum_pages);
    if maximum_pages < initial_pages {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: ProgramExecErrorDetail::ImportedSharedMemoryBudgetExceeded,
        });
    }
    SharedMemory::new(engine, MemoryType::shared(initial_pages, maximum_pages))
        .map(Some)
        .map_err(map_program_runtime_error)
}

fn compiler_shared_memory(
    engine: &wasmtime::Engine,
    module: &Module,
) -> Result<SharedMemory, ProgramExecError> {
    imported_shared_memory_with_declared_maximum(engine, module)?.ok_or(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::CompilerMemoryContractInvalid,
    })
}

fn define_imported_shared_memory<T>(
    linker: &mut CoreLinker<T>,
    store: &wasmtime::Store<T>,
    module: &Module,
    memory: SharedMemory,
) -> Result<(), ProgramExecError> {
    for import in module.imports() {
        if import.ty().memory().is_some() {
            linker
                .define(store, import.module(), import.name(), memory.clone())
                .map_err(map_program_runtime_error)?;
        }
    }
    Ok(())
}

fn define_compiler_shared_memory<T>(
    linker: &mut CoreLinker<T>,
    store: &wasmtime::Store<T>,
    module: &Module,
    memory: SharedMemory,
) -> Result<(), ProgramExecError> {
    define_imported_shared_memory(linker, store, module, memory)
}

fn add_compiler_core_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<CompilerCoreStore<CpuImpl, HostFs>>,
    memory: SharedMemory,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    add_wasi_p1_imports(linker)?;
    add_wasi_thread_spawn(linker)?;
    let _ = memory;
    Ok(())
}

fn add_wasi_p1_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<CompilerCoreStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "random_get",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>, ptr: i32, len: i32| -> i32 {
                fill_random(
                    caller.data().memory(),
                    &caller.data().shared.entropy,
                    ptr as u32,
                    len as u32,
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap("wasi_snapshot_preview1", "sched_yield", || -> i32 {
            p1::errno::SUCCESS
        })
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_time_get",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
             _id: i32,
             _precision: i64,
             ptr: i32|
             -> i32 {
                write_u64(
                    caller.data().memory(),
                    ptr as u32,
                    crate::monotonic_nanos(&caller.data().cpu),
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_write",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
             fd: i32,
             iovs: i32,
             iovs_len: i32,
             nwritten: i32|
             -> i32 {
                fd_write(caller, fd, iovs as u32, iovs_len as u32, nwritten as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "path_open",
            |_fd: i32,
             _dirflags: i32,
             _path: i32,
             _path_len: i32,
             _oflags: i32,
             _fs_rights_base: i64,
             _fs_rights_inheriting: i64,
             _fdflags: i32,
             _opened_fd: i32|
             -> i32 { p1::errno::BADF },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_get",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
             environ: i32,
             buf: i32|
             -> i32 { compiler_environ_get(caller, environ as u32, buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_sizes_get",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
             count: i32,
             size: i32|
             -> i32 {
                let thread_count = compiler_plugin_worker_threads(&caller.data().cpu);
                let env_len = compiler_rayon_env_len(thread_count);
                let memory = caller.data().memory();
                let first = write_u32(memory, count as u32, 1);
                let second = write_u32(memory, size as u32, env_len);
                first.max(second)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_close",
            |mut caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>, fd: i32| -> i32 {
                caller.data_mut().preview1_descriptors.close(fd)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_prestat_get",
            |_fd: i32, _buf: i32| -> i32 { p1::errno::BADF },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_prestat_dir_name",
            |_fd: i32, _path: i32, _len: i32| -> i32 { p1::errno::BADF },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap("wasi_snapshot_preview1", "proc_exit", |code: i32| -> () {
            panic!("compiler plugin called proc_exit({code})")
        })
        .map_err(map_program_runtime_error)?;
    Ok(())
}

fn compiler_plugin_worker_threads<CpuImpl: Cpu>(cpu: &CpuImpl) -> u32 {
    let worker_count =
        super::component_host_worker_count(cpu.processor_count(), cpu.bootstrap_processor()).max(1);
    u32::try_from(worker_count)
        .unwrap_or_else(|_| panic!("compiler plugin processor count exceeds u32"))
}

fn compiler_rayon_env_len(thread_count: u32) -> u32 {
    RAYON_NUM_THREADS_ENV.len() as u32 + decimal_len(thread_count) + 1
}

fn compiler_environ_get<CpuImpl, HostFs>(
    caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
    environ: u32,
    buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let thread_count = compiler_plugin_worker_threads(&caller.data().cpu);
    let memory = caller.data().memory();
    let pointer_status = write_u32(memory, environ, buf);
    let bytes_status = write_rayon_threads_env(memory, buf, thread_count);
    pointer_status.max(bytes_status)
}

fn write_rayon_threads_env(memory: &SharedMemory, ptr: u32, thread_count: u32) -> i32 {
    let prefix_status = write_shared_memory(memory, ptr, RAYON_NUM_THREADS_ENV).map_or(28, |_| 0);
    if prefix_status != 0 {
        return prefix_status;
    }
    let digits_start = ptr + RAYON_NUM_THREADS_ENV.len() as u32;
    let digits_len = write_decimal(memory, digits_start, thread_count);
    if digits_len < 0 {
        return 28;
    }
    let nul_ptr = digits_start + digits_len as u32;
    write_shared_memory(memory, nul_ptr, &[0]).map_or(28, |_| 0)
}

fn decimal_len(mut value: u32) -> u32 {
    let mut len = 1;
    while value >= 10 {
        value /= 10;
        len += 1;
    }
    len
}

fn write_decimal(memory: &SharedMemory, ptr: u32, mut value: u32) -> i32 {
    let mut digits = [0_u8; 10];
    let len = decimal_len(value) as usize;
    for index in (0..len).rev() {
        digits[index] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    write_shared_memory(memory, ptr, &digits[..len]).map_or(-1, |_| len as i32)
}

fn add_wasi_thread_spawn<CpuImpl, HostFs>(
    linker: &mut CoreLinker<CompilerCoreStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            "wasi",
            "thread-spawn",
            |caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>, start_arg: i32| -> i32 {
                let store_data = caller.data().clone();
                let next = store_data.shared.next_thread_id.fetch_update(
                    AtomicOrdering::Relaxed,
                    AtomicOrdering::Relaxed,
                    |value| (value <= 0x1fff_fffe).then_some(value + 1),
                );
                let Ok(previous) = next else {
                    return -1;
                };
                let thread_id = previous + 1;
                let instance_pre = store_data
                    .shared
                    .instance_pre
                    .get()
                    .unwrap_or_else(|| panic!("compiler thread-spawn called before instance pre"))
                    .clone();
                let spawner = store_data.spawner.clone();
                spawner.spawn_detached(async move {
                    let mut store =
                        wasmtime::Store::new(instance_pre.module().engine(), store_data);
                    configure_compiler_core_store(&mut store);
                    let thread_started = store.data().cpu.now().ticks();
                    let result = instance_pre.instantiate(&mut store).and_then(|instance| {
                        let start = instance
                            .get_typed_func::<(i32, i32), ()>(&mut store, "wasi_thread_start")?;
                        start.call(&mut store, (thread_id, start_arg))
                    });
                    let thread_elapsed = store
                        .data()
                        .cpu
                        .now()
                        .ticks()
                        .saturating_sub(thread_started);
                    store.data().record_user_ticks(thread_elapsed);
                    if let Err(error) = result {
                        tracing::error!(thread_id, "compiler plugin thread failed: {error:#}");
                    }
                });
                thread_id
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

fn add_wasix_extended_program_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_spawn_v2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             _args: i32,
             ret_tid: i32|
             -> i32 { wasix_thread_spawn_v2(&mut caller, ret_tid as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "stack_checkpoint",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (snapshot, ret_value): (i32, i32)| {
                Box::new(async move {
                    wasix_stack_checkpoint(&mut caller, snapshot as u32, ret_value as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "stack_restore",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (snapshot, value): (i32, i64)| {
                Box::new(async move {
                    wasix_stack_restore(&mut caller, snapshot as u32, value as u64).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_raise_interval",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             signal: i32,
             _interval: i64,
             _repeat: i32|
             -> i32 { wasix_proc_raise_interval(&mut caller, signal) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_fork",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (_copy_memory, ret_pid): (i32, i32)| {
                Box::new(async move { wasix_proc_fork(&mut caller, ret_pid as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_exec",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (name, name_len, args, args_len): (i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_proc_exec(
                        &mut caller,
                        name as u32,
                        name_len as u32,
                        args as u32,
                        args_len as u32,
                        None,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_exec2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (name, name_len, args, args_len, env, env_len): (i32, i32, i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_proc_exec(
                        &mut caller,
                        name as u32,
                        name_len as u32,
                        args as u32,
                        args_len as u32,
                        Some((env as u32, env_len as u32)),
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_exec3",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (name, name_len, args, args_len, env, env_len, search_path, path, path_len): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    wasix_proc_exec3(
                        &mut caller,
                        name as u32,
                        name_len as u32,
                        args as u32,
                        args_len as u32,
                        env as u32,
                        env_len as u32,
                        search_path,
                        path as u32,
                        path_len as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_spawn",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (
                name,
                name_len,
                chroot,
                args,
                args_len,
                preopen,
                preopen_len,
                stdin,
                stdout,
                stderr,
                working_dir,
                working_dir_len,
                ret_handles,
            ): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    wasix_proc_spawn(
                        &mut caller,
                        name as u32,
                        name_len as u32,
                        chroot,
                        args as u32,
                        args_len as u32,
                        preopen as u32,
                        preopen_len as u32,
                        stdin,
                        stdout,
                        stderr,
                        working_dir as u32,
                        working_dir_len as u32,
                        ret_handles as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_spawn2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (
                name,
                name_len,
                args,
                args_len,
                env,
                env_len,
                fd_ops,
                fd_ops_len,
                signals,
                signals_len,
                search_path,
                path,
                path_len,
                ret_pid,
            ): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    wasix_proc_spawn2(
                        &mut caller,
                        name as u32,
                        name_len as u32,
                        args as u32,
                        args_len as u32,
                        env as u32,
                        env_len as u32,
                        fd_ops as u32,
                        fd_ops_len as u32,
                        signals as u32,
                        signals_len as u32,
                        search_path,
                        path as u32,
                        path_len as u32,
                        ret_pid as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "proc_join",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (pid, flags, ret_status): (i32, i32, i32)| {
                Box::new(async move {
                    wasix_proc_join(&mut caller, pid as u32, flags as u32, ret_status as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_snapshot",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>| -> i32 {
                wasix_proc_snapshot(&mut caller)
            },
        )
        .map_err(map_program_runtime_error)?;
    add_wasix_port_imports(linker)?;
    add_wasix_socket_imports(linker)?;
    add_wasix_epoll_imports(linker)?;
    Ok(())
}

fn add_wasix_port_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_bridge",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             _network: i32,
             _network_len: i32,
             _token: i32,
             _token_len: i32,
             _security: i32|
             -> i32 { wasix_network_admin_unavailable(&mut caller) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_unbridge",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>| -> i32 {
                wasix_network_admin_unavailable(&mut caller)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_dhcp_acquire",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>| -> i32 {
                wasix_network_admin_unavailable(&mut caller)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_addr_add",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, _addr: i32| -> i32 {
                wasix_network_admin_unavailable(&mut caller)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_addr_remove",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, _addr: i32| -> i32 {
                wasix_network_admin_unavailable(&mut caller)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_addr_clear",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>| -> i32 {
                wasix_network_admin_unavailable(&mut caller)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_mac",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ret_mac: i32| -> i32 {
                wasix_port_mac(&mut caller, ret_mac as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_addr_list",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             _addrs: i32,
             naddrs: i32|
             -> i32 { wasix_port_empty_list(&mut caller, naddrs as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_gateway_set",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, _addr: i32| -> i32 {
                wasix_network_admin_unavailable(&mut caller)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_route_add",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             _cidr: i32,
             _router: i32,
             _preferred: i32,
             _expires: i32|
             -> i32 { wasix_network_admin_unavailable(&mut caller) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_route_remove",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, _cidr: i32| -> i32 {
                wasix_network_admin_unavailable(&mut caller)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_route_clear",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>| -> i32 {
                wasix_network_admin_unavailable(&mut caller)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "port_route_list",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             _routes: i32,
             nroutes: i32|
             -> i32 { wasix_port_empty_list(&mut caller, nroutes as u32) },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

fn add_wasix_socket_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_status",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             ret_status: i32|
             -> i32 { wasix_sock_status(&mut caller, fd, ret_status as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_addr_local",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             ret_addr: i32|
             -> i32 { wasix_sock_addr_local(&mut caller, fd, ret_addr as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_addr_peer",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             ret_addr: i32|
             -> i32 { wasix_sock_addr_peer(&mut caller, fd, ret_addr as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_open",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             _af: i32,
             socktype: i32,
             _proto: i32,
             ret_fd: i32|
             -> i32 { wasix_sock_open(&mut caller, socktype, ret_fd as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_pair",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             _af: i32,
             socktype: i32,
             _proto: i32,
             ret_fd0: i32,
             ret_fd1: i32|
             -> i32 {
                wasix_sock_pair(&mut caller, socktype, ret_fd0 as u32, ret_fd1 as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_set_opt_flag",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _option: i32,
             _flag: i32|
             -> i32 { wasix_sock_descriptor_unavailable(&mut caller, fd) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_get_opt_flag",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _option: i32,
             ret_flag: i32|
             -> i32 { wasix_sock_get_opt_flag(&mut caller, fd, ret_flag as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_set_opt_time",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _option: i32,
             _time: i32|
             -> i32 { wasix_sock_descriptor_unavailable(&mut caller, fd) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_get_opt_time",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _option: i32,
             ret_time: i32|
             -> i32 { wasix_sock_get_opt_time(&mut caller, fd, ret_time as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_set_opt_size",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _option: i32,
             _size: i64|
             -> i32 { wasix_sock_descriptor_unavailable(&mut caller, fd) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_get_opt_size",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _option: i32,
             ret_size: i32|
             -> i32 { wasix_sock_get_opt_size(&mut caller, fd, ret_size as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_join_multicast_v4",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _multiaddr: i32,
             _interface: i32|
             -> i32 { wasix_sock_multicast(&mut caller, fd) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_leave_multicast_v4",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _multiaddr: i32,
             _interface: i32|
             -> i32 { wasix_sock_multicast(&mut caller, fd) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_join_multicast_v6",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _multiaddr: i32,
             _interface: i32|
             -> i32 { wasix_sock_multicast(&mut caller, fd) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_leave_multicast_v6",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _multiaddr: i32,
             _interface: i32|
             -> i32 { wasix_sock_multicast(&mut caller, fd) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_bind",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, addr): (i32, i32)| {
                Box::new(async move { wasix_sock_bind(&mut caller, fd, addr as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_listen",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _backlog: i32|
             -> i32 { wasix_sock_descriptor_unavailable(&mut caller, fd) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "sock_accept_v2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _flags: i32,
             ret_fd: i32,
             ret_addr: i32|
             -> i32 {
                wasix_sock_accept_v2(&mut caller, fd, ret_fd as u32, ret_addr as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_connect",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, addr): (i32, i32)| {
                Box::new(async move { wasix_sock_connect(&mut caller, fd, addr as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_recv_from",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, iovs, iovs_len, flags, ret_size, ret_flags, ret_addr): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    wasix_sock_recv_from(
                        &mut caller,
                        fd,
                        iovs as u32,
                        iovs_len as u32,
                        flags as u16,
                        ret_size as u32,
                        ret_flags as u32,
                        ret_addr as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_send_to",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, iovs, iovs_len, flags, addr, ret_size): (i32, i32, i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_sock_send_to(
                        &mut caller,
                        fd,
                        iovs as u32,
                        iovs_len as u32,
                        flags as u16,
                        addr as u32,
                        ret_size as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sock_send_file",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (out_fd, in_fd, offset, count, ret_size): (i32, i32, i64, i64, i32)| {
                Box::new(async move {
                    wasix_sock_send_file(&mut caller, out_fd, in_fd, offset, count, ret_size as u32)
                        .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

fn add_wasix_epoll_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "epoll_create",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ret_fd: i32| -> i32 {
                wasix_epoll_create(&mut caller, ret_fd as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "epoll_ctl",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             epfd: i32,
             _op: i32,
             _fd: i32,
             _event: i32|
             -> i32 { wasix_epoll_ctl(&mut caller, epfd) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "epoll_wait",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (epfd, _event, _maxevents, timeout): (i32, i32, i32, i64)| {
                Box::new(async move { wasix_epoll_wait(&mut caller, epfd, timeout).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

fn configure_compiler_core_store<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, HostFs>>,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    store.set_epoch_deadline(u64::MAX);
}

fn compiler_tls_base<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let global = instance
        .get_global(&mut *store, "__tls_base")
        .ok_or_else(|| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::CompilerPluginInvalid,
        })?;
    match global.get(&mut *store) {
        Val::I32(value) => Ok(value as u32),
        value => Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: {
                tracing::error!(?value, "compiler __tls_base has invalid value type");
                ProgramExecErrorDetail::CompilerPluginInvalid
            },
        }),
    }
}

fn compiler_alloc<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, HostFs>>,
    alloc: &wasmtime::TypedFunc<(i32, i32), i32>,
    len: usize,
    align: usize,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let ptr = alloc
        .call(&mut *store, (len as i32, align as i32))
        .map_err(map_program_runtime_error)?;
    if ptr == 0 {
        let memory = store.data().memory();
        tracing::error!(
            len,
            pages = memory.size(),
            bytes = memory.data_size(),
            "compiler allocation returned null"
        );
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: ProgramExecErrorDetail::CompilerAllocationFailed,
        });
    }
    Ok(ptr as u32)
}

fn read_compiler_response(
    memory: &SharedMemory,
    ptr: u32,
) -> Result<CompilerResponseHeader, ProgramExecError> {
    let bytes = read_shared_memory(
        memory,
        ptr,
        core::mem::size_of::<CompilerResponseHeader>() as u32,
    )?;
    Ok(unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<CompilerResponseHeader>()) })
}

fn fd_write<CpuImpl, HostFs>(
    caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    nwritten: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if !caller.data().preview1_descriptors.can_write(fd) {
        return p1::errno::BADF;
    }
    let memory = caller.data().memory();
    let mut written = 0u32;
    for index in 0..iovs_len {
        let iov = iovs + index * 8;
        let Ok(ptr) = try_read_u32(memory, iov) else {
            return p1::errno::FAULT;
        };
        let Ok(len) = try_read_u32(memory, iov + 4) else {
            return p1::errno::FAULT;
        };
        let Ok(bytes) = read_shared_memory(memory, ptr, len) else {
            return p1::errno::FAULT;
        };
        (caller.data().write_serial)(&bytes);
        written = written.saturating_add(len);
    }
    write_u32(memory, nwritten, written)
}

fn fill_random(
    memory: &SharedMemory,
    entropy: &Mutex<crate::EntropyPool>,
    ptr: u32,
    len: u32,
) -> i32 {
    let mut bytes = alloc::vec![0_u8; len as usize];
    if entropy.lock().fill_secure(&mut bytes).is_err() {
        return p1::errno::IO;
    }
    write_shared_memory(memory, ptr, &bytes).map_or(p1::errno::FAULT, |_| p1::errno::SUCCESS)
}

fn try_read_u32(memory: &SharedMemory, ptr: u32) -> Result<u32, ProgramExecError> {
    let bytes = read_shared_memory(memory, ptr, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap_or_else(|_| {
        panic!("u32 read must return 4 bytes")
    })))
}

fn write_u32(memory: &SharedMemory, ptr: u32, value: u32) -> i32 {
    write_shared_memory(memory, ptr, &value.to_le_bytes())
        .map_or(p1::errno::FAULT, |_| p1::errno::SUCCESS)
}

fn write_u64(memory: &SharedMemory, ptr: u32, value: u64) -> i32 {
    write_shared_memory(memory, ptr, &value.to_le_bytes())
        .map_or(p1::errno::FAULT, |_| p1::errno::SUCCESS)
}

fn read_shared_memory(
    memory: &SharedMemory,
    ptr: u32,
    len: u32,
) -> Result<Vec<u8>, ProgramExecError> {
    let data = memory.data();
    let start = ptr as usize;
    let len = len as usize;
    let end = start.checked_add(len).ok_or_else(|| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::GuestMemoryAccessOverflow,
    })?;
    if end > data.len() {
        tracing::error!(
            start,
            end,
            memory_size = data.len(),
            "compiler plugin memory read is out of bounds"
        );
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        });
    }
    let mut bytes = Vec::with_capacity(len);
    unsafe {
        bytes.extend_from_slice(core::slice::from_raw_parts(
            data.as_ptr().cast::<u8>().add(start),
            len,
        ));
    }
    Ok(bytes)
}

fn write_shared_memory(
    memory: &SharedMemory,
    ptr: u32,
    bytes: &[u8],
) -> Result<(), ProgramExecError> {
    let data = memory.data();
    let start = ptr as usize;
    let end = start
        .checked_add(bytes.len())
        .ok_or_else(|| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOverflow,
        })?;
    if end > data.len() {
        tracing::error!(
            start,
            end,
            memory_size = data.len(),
            "compiler plugin memory write is out of bounds"
        );
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        });
    }
    unsafe {
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            data.as_ptr().cast::<u8>().add(start).cast_mut(),
            bytes.len(),
        );
    }
    Ok(())
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
            spawner: store.spawner().clone(),
            runtime_state: store.runtime_state.clone(),
            instance_registry: store.instance_registry.clone(),
            parent_instance_id: Some(store.instance().id()),
            read_serial: store.serial_reader_fn(),
            write_serial: store.serial_writer_fn(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_program_executable<CpuImpl, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, HostFs>,
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    spawner: crate::Spawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    executable: ProgramExecutable,
    engine: &crate::wasmtime_adapter::WasmtimeEngine,
    runtime: &crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    launched_instance: crate::RegisteredInstance,
    stdin_reader: crate::ByteReader,
    stdout_writer: crate::ByteWriter,
    stderr_writer: crate::ByteWriter,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match executable {
        ProgramExecutable::Component(compiled) => {
            run_program_component(
                exec_context,
                name,
                args,
                env,
                authority,
                spawner,
                progress,
                compiled,
                engine,
                runtime,
                launched_instance,
                stdin_reader,
                stdout_writer,
                stderr_writer,
            )
            .await
        }
        ProgramExecutable::CoreModule(compiled) => {
            run_program_core_module(
                exec_context,
                name,
                args,
                env,
                authority,
                spawner,
                progress,
                compiled,
                engine,
                launched_instance,
                stdin_reader,
                stdout_writer,
                stderr_writer,
            )
            .await
        }
        ProgramExecutable::ForkedCoreModule { compiled, restore } => {
            run_program_core_module_with_restore(
                exec_context,
                name,
                args,
                env,
                authority,
                spawner,
                progress,
                compiled,
                restore,
                engine,
                launched_instance,
                stdin_reader,
                stdout_writer,
                stderr_writer,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_program_core_module<CpuImpl, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, HostFs>,
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    spawner: crate::Spawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    compiled: Arc<WasmtimeCompiledCoreModule>,
    engine: &crate::wasmtime_adapter::WasmtimeEngine,
    launched_instance: crate::RegisteredInstance,
    stdin_reader: crate::ByteReader,
    stdout_writer: crate::ByteWriter,
    stderr_writer: crate::ByteWriter,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(name);
    argv.extend(args);
    let run_started_at = monotonic_nanos(&exec_context.cpu);
    let run_cpu = exec_context.cpu.clone();
    let instance_id = launched_instance.id();
    let imported_memory = imported_shared_memory_with_user_budget(engine.raw(), &compiled.module)?;
    let mut store = wasmtime::Store::new(
        engine.raw(),
        Preview1ProgramStore::<CpuImpl, HostFs>::new(
            exec_context.cpu,
            exec_context.timer,
            exec_context.spawner.clone(),
            exec_context.runtime_state,
            launched_instance,
            exec_context.parent_instance_id,
            argv,
            env,
            authority,
            OutputMode::Child {
                stdin_rx: stdin_reader,
                stdout_tx: stdout_writer,
                stderr_tx: stderr_writer,
            },
            exec_context.read_serial,
            exec_context.write_serial,
            imported_memory.clone(),
            Some(compiled.clone()),
        ),
    );
    configure_preview1_program_store(&mut store);

    let mut linker = CoreLinker::new(engine.raw());
    add_preview1_program_imports(&mut linker)?;
    if let Some(memory) = imported_memory {
        define_imported_shared_memory(&mut linker, &store, &compiled.module, memory)?;
    }

    super::emit_stage_marker(exec_context.write_serial, "program:instantiate-core-begin");
    let instance = linker
        .instantiate_async(&mut store, &compiled.module)
        .await
        .map_err(map_program_runtime_error)?;
    super::emit_stage_marker(exec_context.write_serial, "program:instantiate-core-ok");

    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::InvalidEntryPoint,
        })?;

    let run_done = Arc::new(core::sync::atomic::AtomicBool::new(false));
    super::spawn_component_phase_heartbeat(
        &spawner,
        &run_cpu,
        &progress,
        exec_context.write_serial,
        "program:run-core",
        run_started_at,
        &run_done,
    );
    super::emit_stage_marker(exec_context.write_serial, "program:run-core-begin");
    let result = loop {
        let result = start.call_async(&mut store, ()).await;
        if handle_wasix_asyncify_completion(&mut store, &instance).await? {
            continue;
        }
        break result;
    };
    run_done.store(true, core::sync::atomic::Ordering::Release);
    super::emit_stage_marker(exec_context.write_serial, "program:run-core-end");

    match result {
        Ok(()) => Ok(ChildExit {
            instance_id,
            exit_code: 0,
        }),
        Err(error) => match store.data_mut().take_requested_exit() {
            Some(code) => Ok(ChildExit {
                instance_id,
                exit_code: code,
            }),
            None => Err(map_program_runtime_error(error)),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_program_core_module_with_restore<CpuImpl, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, HostFs>,
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    spawner: crate::Spawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    compiled: Arc<WasmtimeCompiledCoreModule>,
    restore: CoreModuleRestore,
    engine: &crate::wasmtime_adapter::WasmtimeEngine,
    launched_instance: crate::RegisteredInstance,
    stdin_reader: crate::ByteReader,
    stdout_writer: crate::ByteWriter,
    stderr_writer: crate::ByteWriter,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(name);
    argv.extend(args);
    let run_started_at = monotonic_nanos(&exec_context.cpu);
    let run_cpu = exec_context.cpu.clone();
    let instance_id = launched_instance.id();
    let imported_memory = Some(restore.memory.clone());
    let mut store = wasmtime::Store::new(
        engine.raw(),
        Preview1ProgramStore::<CpuImpl, HostFs>::new(
            exec_context.cpu,
            exec_context.timer,
            exec_context.spawner.clone(),
            exec_context.runtime_state,
            launched_instance,
            exec_context.parent_instance_id,
            argv,
            env,
            authority,
            OutputMode::Child {
                stdin_rx: stdin_reader,
                stdout_tx: stdout_writer,
                stderr_tx: stderr_writer,
            },
            exec_context.read_serial,
            exec_context.write_serial,
            imported_memory.clone(),
            Some(compiled.clone()),
        ),
    );
    store.data_mut().descriptors = restore.descriptors;
    configure_preview1_program_store(&mut store);

    let mut linker = CoreLinker::new(engine.raw());
    add_preview1_program_imports(&mut linker)?;
    define_imported_shared_memory(
        &mut linker,
        &store,
        &compiled.module,
        restore.memory.clone(),
    )?;

    super::emit_stage_marker(exec_context.write_serial, "program:instantiate-core-begin");
    let instance = linker
        .instantiate_async(&mut store, &compiled.module)
        .await
        .map_err(map_program_runtime_error)?;
    super::emit_stage_marker(exec_context.write_serial, "program:instantiate-core-ok");

    wasix_begin_rewind(
        &mut store,
        &instance,
        restore.stack_lower,
        restore.stack_upper,
        restore.stack_pointer,
        restore.memory_stack,
        restore.rewind_stack,
        restore.value,
    )
    .await?;

    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::InvalidEntryPoint,
        })?;

    let run_done = Arc::new(core::sync::atomic::AtomicBool::new(false));
    super::spawn_component_phase_heartbeat(
        &spawner,
        &run_cpu,
        &progress,
        exec_context.write_serial,
        "program:run-core",
        run_started_at,
        &run_done,
    );
    super::emit_stage_marker(exec_context.write_serial, "program:run-core-begin");
    let result = loop {
        let result = start.call_async(&mut store, ()).await;
        if handle_wasix_asyncify_completion(&mut store, &instance).await? {
            continue;
        }
        break result;
    };
    run_done.store(true, core::sync::atomic::Ordering::Release);
    super::emit_stage_marker(exec_context.write_serial, "program:run-core-end");

    match result {
        Ok(()) => Ok(ChildExit {
            instance_id,
            exit_code: 0,
        }),
        Err(error) => match store.data_mut().take_requested_exit() {
            Some(code) => Ok(ChildExit {
                instance_id,
                exit_code: code,
            }),
            None => Err(map_program_runtime_error(error)),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_program_component<CpuImpl, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, HostFs>,
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    spawner: crate::Spawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    compiled: Arc<WasmtimeCompiledComponent>,
    engine: &crate::wasmtime_adapter::WasmtimeEngine,
    runtime: &crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    launched_instance: crate::RegisteredInstance,
    stdin_reader: crate::ByteReader,
    stdout_writer: crate::ByteWriter,
    stderr_writer: crate::ByteWriter,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    use crate::{ComponentExecContext, ComponentExecutor, ComponentRuntimeFactory, ComponentWorld};

    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(name);
    argv.extend(args);
    let run_started_at = monotonic_nanos(&exec_context.cpu);
    let run_cpu = exec_context.cpu.clone();

    let context = ComponentExecContext::new(
        exec_context.cpu,
        exec_context.timer,
        exec_context.spawner.clone(),
        exec_context.runtime_state.clone(),
        exec_context.instance_registry,
        launched_instance,
        false,
        exec_context.runtime_state,
        argv,
        env,
        authority,
        OutputMode::Child {
            stdin_rx: stdin_reader,
            stdout_tx: stdout_writer,
            stderr_tx: stderr_writer,
        },
        exec_context.read_serial,
        exec_context.write_serial,
    );

    // Use the engine that compiled the component — Wasmtime requires
    // component and store to share the same engine instance.
    super::emit_stage_marker(exec_context.write_serial, "program:instantiate-begin");
    let executor =
        <crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl> as ComponentRuntimeFactory<
            CpuImpl,
            HostRuntimeState<CpuImpl, HostFs>,
            HostFs,
        >>::instantiate(runtime, engine, &compiled, ComponentWorld::Program, context)
        .await
        .map_err(map_program_runtime_error)?;
    super::emit_stage_marker(exec_context.write_serial, "program:instantiate-ok");

    let run_done = Arc::new(core::sync::atomic::AtomicBool::new(false));
    super::spawn_component_phase_heartbeat(
        &spawner,
        &run_cpu,
        &progress,
        exec_context.write_serial,
        "program:run",
        run_started_at,
        &run_done,
    );
    super::emit_stage_marker(exec_context.write_serial, "program:run-begin");
    let result = executor.run().await;
    run_done.store(true, core::sync::atomic::Ordering::Release);
    let result = result.map_err(map_program_runtime_error)?;
    super::emit_stage_marker(exec_context.write_serial, "program:run-end");

    Ok(ChildExit {
        instance_id: result.instance_id,
        exit_code: result.exit_code,
    })
}

async fn handle_wasix_asyncify_completion<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
) -> Result<bool, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let phase = core::mem::replace(
        &mut store.data_mut().asyncify.phase,
        WasixAsyncifyPhase::Idle,
    );
    match phase {
        WasixAsyncifyPhase::Idle => Ok(false),
        WasixAsyncifyPhase::Capturing {
            snapshot,
            ret_value,
            stack_lower,
            stack_upper,
            unwind_stack_begin,
            mut memory_stack,
            stack_pointer,
        } => {
            let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
            })?;
            let unwind_stack_finish = preview1_read_u32(memory, stack_lower)?;
            if unwind_stack_finish < unwind_stack_begin || unwind_stack_finish > stack_pointer {
                return Err(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
                });
            }
            let unwind_len =
                usize::try_from(unwind_stack_finish - unwind_stack_begin).map_err(|_| {
                    ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
                    }
                })?;
            let rewind_stack = preview1_read_memory(memory, unwind_stack_begin, unwind_len)?;
            wasix_call_instance_func0(store, instance, "asyncify_stop_unwind").await?;

            let hash = wasix_next_stack_hash(store);
            let snapshot_bytes = wasix_stack_snapshot_bytes(ret_value, hash);
            if snapshot >= stack_pointer
                && snapshot.saturating_add(WASIX_STACK_SNAPSHOT_SIZE as u32) <= stack_upper
            {
                let offset =
                    usize::try_from(snapshot - stack_pointer).map_err(|_| ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
                    })?;
                let end = offset + snapshot_bytes.len();
                if end <= memory_stack.len() {
                    memory_stack[offset..end].copy_from_slice(&snapshot_bytes);
                }
            } else {
                let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
                })?;
                if preview1_write_memory(memory, snapshot, &snapshot_bytes) != p1::errno::SUCCESS {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
                    });
                }
            }

            store
                .data_mut()
                .asyncify
                .snapshots
                .push(WasixStackSnapshot {
                    hash,
                    memory_stack: memory_stack.clone(),
                    rewind_stack: rewind_stack.clone(),
                    stack_pointer,
                });
            wasix_begin_rewind(
                store,
                instance,
                stack_lower,
                stack_upper,
                stack_pointer,
                memory_stack,
                rewind_stack,
                0,
            )
            .await?;
            Ok(true)
        }
        WasixAsyncifyPhase::Restoring {
            hash,
            value,
            stack_lower,
        } => {
            wasix_call_instance_func0(store, instance, "asyncify_stop_unwind").await?;
            let snapshot = store
                .data()
                .asyncify
                .snapshots
                .iter()
                .find(|snapshot| snapshot.hash == hash)
                .cloned()
                .ok_or(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::WasixStackSnapshotMissing,
                })?;
            let stack_upper = wasix_global_u32_from_instance(store, instance, "__heap_base")?;
            wasix_begin_rewind(
                store,
                instance,
                stack_lower,
                stack_upper,
                snapshot.stack_pointer,
                snapshot.memory_stack,
                snapshot.rewind_stack,
                value,
            )
            .await?;
            Ok(true)
        }
        WasixAsyncifyPhase::Forking {
            ret_pid,
            stack_lower,
            stack_upper,
            unwind_stack_begin,
            mut memory_stack,
            stack_pointer,
        } => {
            let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
            })?;
            let unwind_stack_finish = preview1_read_u32(memory, stack_lower)?;
            if unwind_stack_finish < unwind_stack_begin || unwind_stack_finish > stack_pointer {
                return Err(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
                });
            }
            let unwind_len =
                usize::try_from(unwind_stack_finish - unwind_stack_begin).map_err(|_| {
                    ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
                    }
                })?;
            let rewind_stack = preview1_read_memory(memory, unwind_stack_begin, unwind_len)?;
            wasix_call_instance_func0(store, instance, "asyncify_stop_unwind").await?;
            let child_pid = spawn_wasix_fork_child(
                store,
                stack_lower,
                stack_upper,
                stack_pointer,
                memory_stack.clone(),
                rewind_stack.clone(),
            )?;
            let snapshot_bytes = child_pid.to_le_bytes();
            if ret_pid >= stack_pointer && ret_pid.saturating_add(4) <= stack_upper {
                let offset =
                    usize::try_from(ret_pid - stack_pointer).map_err(|_| ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
                    })?;
                let end = offset + snapshot_bytes.len();
                if end <= memory_stack.len() {
                    memory_stack[offset..end].copy_from_slice(&snapshot_bytes);
                }
            } else {
                let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
                })?;
                if preview1_write_memory(memory, ret_pid, &snapshot_bytes) != p1::errno::SUCCESS {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
                    });
                }
            }
            wasix_begin_rewind(
                store,
                instance,
                stack_lower,
                stack_upper,
                stack_pointer,
                memory_stack,
                rewind_stack,
                u64::from(child_pid),
            )
            .await?;
            Ok(true)
        }
    }
}

async fn wasix_begin_rewind<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
    stack_lower: u32,
    stack_upper: u32,
    stack_pointer: u32,
    memory_stack: Vec<u8>,
    rewind_stack: Vec<u8>,
    value: u64,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if stack_lower >= stack_pointer || stack_pointer > stack_upper {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
        });
    }
    let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
    })?;
    if preview1_write_memory(memory, stack_pointer, &memory_stack) != p1::errno::SUCCESS {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        });
    }
    wasix_set_global_u32_from_instance(store, instance, "__stack_pointer", stack_pointer)?;

    let rewind_stack_begin =
        stack_lower
            .checked_add(WASIX_ASYNCIFY_DATA_SIZE)
            .ok_or(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
            })?;
    let rewind_len = u32::try_from(rewind_stack.len()).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
    })?;
    let rewind_stack_end = rewind_stack_begin
        .checked_add(rewind_len)
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
        })?;
    if rewind_stack_end > stack_upper {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
        });
    }
    let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
    })?;
    let status = preview1_write_u32(memory, stack_lower, rewind_stack_end)
        .max(preview1_write_u32(memory, stack_lower + 4, stack_upper))
        .max(preview1_write_memory(
            memory,
            rewind_stack_begin,
            &rewind_stack,
        ));
    if status != p1::errno::SUCCESS {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        });
    }
    store.data_mut().asyncify.rewind_value = Some(value);
    wasix_call_instance_func1(store, instance, "asyncify_start_rewind", stack_lower).await?;
    Ok(())
}

fn spawn_wasix_fork_child<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    stack_lower: u32,
    stack_upper: u32,
    stack_pointer: u32,
    memory_stack: Vec<u8>,
    rewind_stack: Vec<u8>,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let memory = store
        .data()
        .imported_memory
        .as_ref()
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::ImportedSharedMemoryContractInvalid,
        })?;
    let data = memory.data();
    let current_bytes = data.len();
    let current_pages = current_bytes.div_ceil(WASM_PAGE_SIZE);
    let current_pages = u32::try_from(current_pages).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::OutOfMemory,
        detail: ProgramExecErrorDetail::ImportedSharedMemoryBudgetExceeded,
    })?;
    let service = store
        .data()
        .runtime_state
        .program_service()
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::Unavailable,
            detail: ProgramExecErrorDetail::HostOperationFailed,
        })?;
    let fork_memory = SharedMemory::new(
        service.inner.engine.raw(),
        MemoryType::shared(current_pages, PROGRAM_SHARED_MEMORY_MAX_PAGES),
    )
    .map_err(map_program_runtime_error)?;
    let fork_data = fork_memory.data();
    if fork_data.len() < current_bytes {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: ProgramExecErrorDetail::ImportedSharedMemoryBudgetExceeded,
        });
    }
    unsafe {
        core::ptr::copy_nonoverlapping(
            data.as_ptr().cast::<u8>(),
            fork_data.as_ptr().cast::<u8>().cast_mut(),
            current_bytes,
        );
    }
    let compiled = store
        .data()
        .current_core_module
        .clone()
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::WasixExecReplacementUnavailable,
        })?;
    let restore = CoreModuleRestore {
        memory: fork_memory,
        descriptors: store.data().descriptors.clone(),
        stack_lower,
        stack_upper,
        stack_pointer,
        memory_stack,
        rewind_stack,
        value: 0,
    };
    let argv = store.data().arguments.clone();
    let name = argv.first().cloned().ok_or(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::InvalidEntryPoint,
    })?;
    let args = argv.into_iter().skip(1).collect();
    let mut environment = store.data().environment.clone();
    environment.retain(|(name, _)| name.as_str() != HELIOS_PROCESS_ID_ENV);
    let child = service.spawn_loaded(
        store.data().exec_context(),
        name,
        args,
        environment,
        ProgramExecutable::ForkedCoreModule { compiled, restore },
        store.data().authority.clone(),
    )?;
    let pid = u32::try_from(child.instance_id.raw()).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::Internal,
        detail: ProgramExecErrorDetail::InternalInvariant,
    })?;
    let mut child = child;
    drop(child.take_stdin());
    let stdout = child.take_stdout();
    let stderr = child.take_stderr();
    let stdout_sink = store.data().output_sink();
    let stderr_sink = store.data().output_sink();
    let (exit_tx, exit) = futures::channel::oneshot::channel();
    store.data().spawner.spawn_detached(async move {
        let stdout_task = async move {
            if let Some(reader) = stdout {
                while let Some(bytes) = reader.read().await {
                    stdout_sink.write(crate::ComponentOutputStreamKind::Stdout, bytes);
                }
            }
        };
        let stderr_task = async move {
            if let Some(reader) = stderr {
                while let Some(bytes) = reader.read().await {
                    stderr_sink.write(crate::ComponentOutputStreamKind::Stderr, bytes);
                }
            }
        };
        let wait_task = child.wait();
        let (_, (_, exit)) =
            futures::future::join(stdout_task, futures::future::join(stderr_task, wait_task)).await;
        let _ = exit_tx.send(exit);
    });
    store.data_mut().insert_child(pid, exit);
    Ok(pid)
}

fn wasix_next_stack_hash<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> u128
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let lower = store.data_mut().entropy.insecure_u64();
    let upper = store.data_mut().entropy.insecure_u64();
    let hash = (u128::from(upper) << 64) | u128::from(lower);
    if hash == 0 { 1 } else { hash }
}

fn wasix_stack_snapshot_bytes(user: u32, hash: u128) -> [u8; WASIX_STACK_SNAPSHOT_SIZE] {
    let mut bytes = [0_u8; WASIX_STACK_SNAPSHOT_SIZE];
    bytes[0..8].copy_from_slice(&u64::from(user).to_le_bytes());
    bytes[8..16].copy_from_slice(&(hash as u64).to_le_bytes());
    bytes[16..24].copy_from_slice(&((hash >> 64) as u64).to_le_bytes());
    bytes
}

async fn wasix_call_instance_func0<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
    name: &str,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let function = instance
        .get_typed_func::<(), ()>(&mut *store, name)
        .map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::WasixAsyncifyExportInvalid,
        })?;
    function
        .call_async(&mut *store, ())
        .await
        .map_err(map_program_runtime_error)
}

async fn wasix_call_instance_func1<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
    name: &str,
    value: u32,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let function = instance
        .get_typed_func::<i32, ()>(&mut *store, name)
        .map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::WasixAsyncifyExportInvalid,
        })?;
    function
        .call_async(&mut *store, value as i32)
        .await
        .map_err(map_program_runtime_error)
}

fn wasix_global_u32_from_instance<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
    name: &str,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let global = instance
        .get_global(&mut *store, name)
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::WasixAsyncifyExportInvalid,
        })?;
    match global.get(&mut *store) {
        Val::I32(value) => Ok(value as u32),
        _ => Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryTypeMismatch,
        }),
    }
}

fn wasix_set_global_u32_from_instance<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
    name: &str,
    value: u32,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let global = instance
        .get_global(&mut *store, name)
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::WasixAsyncifyExportInvalid,
        })?;
    global
        .set(&mut *store, Val::I32(value as i32))
        .map_err(map_program_runtime_error)
}

fn configure_preview1_program_store<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    store.call_hook(
        |caller: StoreContextMut<'_, Preview1ProgramStore<CpuImpl, HostFs>>, hook| {
            let transition = crate::wasmtime_adapter::store::translate_call_hook(hook);
            caller.data().record_transition(transition);
            if let Some(reason) = caller.data().check_pending_kill() {
                return Err(wasmtime::Error::from(crate::InstanceKilled { reason }));
            }
            Ok(())
        },
    );
    store.set_epoch_deadline(1);
    store.epoch_deadline_async_yield_and_update(1);
}

fn add_preview1_program_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "args_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             argc: i32,
             argv_buf_size: i32|
             -> i32 {
                p1_args_sizes_get(&mut caller, argc as u32, argv_buf_size as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "args_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             argv: i32,
             argv_buf: i32|
             -> i32 { p1_args_get(&mut caller, argv as u32, argv_buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             count: i32,
             size: i32|
             -> i32 { p1_environ_sizes_get(&mut caller, count as u32, size as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "environ_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             environ: i32,
             environ_buf: i32|
             -> i32 { p1_environ_get(&mut caller, environ as u32, environ_buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_res_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             id: i32,
             resolution: i32|
             -> i32 { p1_clock_res_get(&mut caller, id, resolution as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "clock_time_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             id: i32,
             _precision: i64,
             timestamp: i32|
             -> i32 { p1_clock_time_get(&mut caller, id, timestamp as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "sched_yield",
            |_caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ()| {
                Box::new(async move {
                    crate::yield_now().await;
                    p1::errno::SUCCESS
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "random_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             ptr: i32,
             len: i32|
             -> i32 { p1_random_get(&mut caller, ptr as u32, len as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_write",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, iovs, iovs_len, nwritten): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_fd_write(
                        &mut caller,
                        fd,
                        iovs as u32,
                        iovs_len as u32,
                        nwritten as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_read",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, iovs, iovs_len, nread): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_fd_read(&mut caller, fd, iovs as u32, iovs_len as u32, nread as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_close",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, fd: i32| -> i32 {
                caller.data_mut().descriptors.close(fd)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_prestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             buf: i32|
             -> i32 { p1_fd_prestat_get(&mut caller, fd, buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_prestat_dir_name",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             path: i32,
             len: i32|
             -> i32 {
                p1_fd_prestat_dir_name(&mut caller, fd, path as u32, len as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             stat: i32|
             -> i32 { p1_fd_fdstat_get(&mut caller, fd, stat as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_set_flags",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             fdflags: i32|
             -> i32 { p1_fd_fdstat_set_flags(&mut caller, fd, fdflags as u16) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_fdstat_set_rights",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             rights_base: i64,
             rights_inheriting: i64|
             -> i32 {
                p1_fd_fdstat_set_rights(
                    &mut caller,
                    fd,
                    rights_base as u64,
                    rights_inheriting as u64,
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_filestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, stat): (i32, i32)| {
                Box::new(async move { p1_fd_filestat_get(&mut caller, fd, stat as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_filestat_set_size",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, size): (i32, i64)| {
                Box::new(async move { p1_fd_filestat_set_size(&mut caller, fd, size as u64).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_filestat_set_times",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             atim: i64,
             mtim: i64,
             fstflags: i32|
             -> i32 {
                p1_fd_filestat_set_times(&mut caller, fd, atim as u64, mtim as u64, fstflags as u16)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_advise",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _offset: i64,
             _len: i64,
             _advice: i32|
             -> i32 { p1_fd_advise(&mut caller, fd) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_allocate",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, offset, len): (i32, i64, i64)| {
                Box::new(
                    async move { p1_fd_allocate(&mut caller, fd, offset as u64, len as u64).await },
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_datasync",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, fd: i32| -> i32 {
                p1_fd_datasync(&mut caller, fd)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_sync",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, fd: i32| -> i32 {
                p1_fd_sync(&mut caller, fd)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_pread",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, iovs, iovs_len, offset, nread): (i32, i32, i32, i64, i32)| {
                Box::new(async move {
                    p1_fd_pread(
                        &mut caller,
                        fd,
                        iovs as u32,
                        iovs_len as u32,
                        offset as u64,
                        nread as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_pwrite",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, iovs, iovs_len, offset, nwritten): (i32, i32, i32, i64, i32)| {
                Box::new(async move {
                    p1_fd_pwrite(
                        &mut caller,
                        fd,
                        iovs as u32,
                        iovs_len as u32,
                        offset as u64,
                        nwritten as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_readdir",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, buf, buf_len, cookie, bufused): (i32, i32, i32, i64, i32)| {
                Box::new(async move {
                    p1_fd_readdir(
                        &mut caller,
                        fd,
                        buf as u32,
                        buf_len as u32,
                        cookie as u64,
                        bufused as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_renumber",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             from: i32,
             to: i32|
             -> i32 { p1_fd_renumber(&mut caller, from, to) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "fd_seek",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, offset, whence, new_offset): (i32, i64, i32, i32)| {
                Box::new(async move {
                    p1_fd_seek(&mut caller, fd, offset, whence as u8, new_offset as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_tell",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             offset: i32|
             -> i32 { p1_fd_tell(&mut caller, fd, offset as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_open",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (
                fd,
                dirflags,
                path,
                path_len,
                oflags,
                fs_rights_base,
                _fs_rights_inheriting,
                fdflags,
                opened_fd,
            ): (i32, i32, i32, i32, i32, i64, i64, i32, i32)| {
                Box::new(async move {
                    p1_path_open(
                        &mut caller,
                        fd,
                        dirflags as u32,
                        path as u32,
                        path_len as u32,
                        oflags as u16,
                        fs_rights_base as u64,
                        fdflags as u16,
                        opened_fd as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    add_preview1_program_remaining_imports(linker)?;
    add_wasix_program_imports(linker)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "proc_exit",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             code: i32|
             -> wasmtime::Result<()> {
                caller.data_mut().request_exit(code as u32);
                Err(wasmtime::Error::new(Preview1Exit))
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .alias_module("wasi_snapshot_preview1", "wasi_unstable")
        .map_err(map_program_runtime_error)?;
    Ok(())
}

fn add_wasix_program_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "clock_time_set",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             clock_id: i32,
             timestamp: i64|
             -> i32 { wasix_clock_time_set(&mut caller, clock_id, timestamp) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_dup",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             ret_fd: i32|
             -> i32 { wasix_fd_dup(&mut caller, fd, ret_fd as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_dup2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             min_result_fd: i32,
             cloexec: i32,
             ret_fd: i32|
             -> i32 {
                wasix_fd_dup2(&mut caller, fd, min_result_fd, cloexec != 0, ret_fd as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_pipe",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             ret_fd1: i32,
             ret_fd2: i32|
             -> i32 { wasix_fd_pipe(&mut caller, ret_fd1 as u32, ret_fd2 as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "tty_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, state: i32| -> i32 {
                wasix_tty_get(&mut caller, state as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "tty_set",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, state: i32| -> i32 {
                wasix_tty_set(&mut caller, state as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "getcwd",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             path: i32,
             path_len: i32|
             -> i32 { wasix_getcwd(&mut caller, path as u32, path_len as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "chdir",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             path: i32,
             path_len: i32|
             -> i32 { wasix_chdir(&mut caller, path as u32, path_len as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_event",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             initial_value: i64,
             flags: i32,
             ret_fd: i32|
             -> i32 {
                wasix_fd_event(
                    &mut caller,
                    initial_value as u64,
                    flags as u32,
                    ret_fd as u32,
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "path_open2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (
                fd,
                dirflags,
                path,
                path_len,
                oflags,
                fs_rights_base,
                _fs_rights_inheriting,
                fdflags,
                fdflagsext,
                opened_fd,
            ): (i32, i32, i32, i32, i32, i64, i64, i32, i32, i32)| {
                Box::new(async move {
                    wasix_path_open2(
                        &mut caller,
                        fd,
                        dirflags as u32,
                        path as u32,
                        path_len as u32,
                        oflags as u16,
                        fs_rights_base as u64,
                        fdflags as u16,
                        fdflagsext as u16,
                        opened_fd as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_fdflags_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             ret_flags: i32|
             -> i32 { wasix_fd_fdflags_get(&mut caller, fd, ret_flags as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_fdflags_set",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             flags: i32|
             -> i32 { wasix_fd_fdflags_set(&mut caller, fd, flags as u16) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "callback_signal",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             callback: i32,
             callback_len: i32|
             -> wasmtime::Result<()> {
                wasix_callback_signal(&mut caller, callback as u32, callback_len as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_id",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ret_pid: i32| -> i32 {
                wasix_proc_id(&mut caller, ret_pid as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_signal",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             pid: i32,
             signal: i32|
             -> wasmtime::Result<i32> {
                wasix_proc_signal(&mut caller, pid as u32, signal)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_signals_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, buf: i32| -> i32 {
                wasix_proc_signals_get(&mut caller, buf as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_signals_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ret_size: i32| -> i32 {
                wasix_proc_signals_sizes_get(&mut caller, ret_size as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "resolve",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (host, host_len, port, addrs, naddrs, ret_naddrs): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    wasix_resolve(
                        &mut caller,
                        host as u32,
                        host_len as u32,
                        port,
                        addrs as u32,
                        naddrs as u32,
                        ret_naddrs as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_parent",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             pid: i32,
             ret_pid: i32|
             -> i32 { wasix_proc_parent(&mut caller, pid as u32, ret_pid as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "thread_sleep",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, (duration,): (i64,)| {
                Box::new(async move { wasix_thread_sleep(&mut caller, duration).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_id",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ret_tid: i32| -> i32 {
                wasix_thread_id(&mut caller, ret_tid as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_join",
            |_caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, tid: i32| -> i32 {
                wasix_thread_join(tid as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_parallelism",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             ret_parallelism: i32|
             -> i32 { wasix_thread_parallelism(&mut caller, ret_parallelism as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_signal",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             tid: i32,
             signal: i32|
             -> wasmtime::Result<i32> { wasix_thread_signal(&mut caller, tid, signal) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "thread_exit",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             code: i32|
             -> wasmtime::Result<()> { wasix_thread_exit(&mut caller, code as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "futex_wait",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (futex, expected, timeout, ret_woken): (i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_futex_wait(
                        &mut caller,
                        futex as u32,
                        expected as u32,
                        timeout as u32,
                        ret_woken as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "futex_wake",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             futex: i32,
             ret_woken: i32|
             -> i32 { wasix_futex_wake(&mut caller, futex as u32, ret_woken as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "futex_wake_all",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             futex: i32,
             ret_woken: i32|
             -> i32 {
                wasix_futex_wake_all(&mut caller, futex as u32, ret_woken as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    add_wasix_preview1_alias_imports(linker)?;
    add_wasix_extended_program_imports(linker)?;
    Ok(())
}

fn add_wasix_preview1_alias_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap(
            WASIX_MODULE,
            "args_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             argc: i32,
             argv_buf_size: i32|
             -> i32 {
                p1_args_sizes_get(&mut caller, argc as u32, argv_buf_size as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "args_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             argv: i32,
             argv_buf: i32|
             -> i32 { p1_args_get(&mut caller, argv as u32, argv_buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "environ_sizes_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             count: i32,
             size: i32|
             -> i32 { p1_environ_sizes_get(&mut caller, count as u32, size as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "environ_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             environ: i32,
             environ_buf: i32|
             -> i32 { p1_environ_get(&mut caller, environ as u32, environ_buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "clock_time_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             id: i32,
             _precision: i64,
             timestamp: i32|
             -> i32 { p1_clock_time_get(&mut caller, id, timestamp as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_close",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, fd: i32| -> i32 {
                caller.data_mut().descriptors.close(fd)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_fdstat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             stat: i32|
             -> i32 { p1_fd_fdstat_get(&mut caller, fd, stat as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_fdstat_set_flags",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             fdflags: i32|
             -> i32 { p1_fd_fdstat_set_flags(&mut caller, fd, fdflags as u16) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_filestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, stat): (i32, i32)| {
                Box::new(async move { p1_fd_filestat_get(&mut caller, fd, stat as u32).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_prestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             buf: i32|
             -> i32 { p1_fd_prestat_get(&mut caller, fd, buf as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_prestat_dir_name",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             path: i32,
             len: i32|
             -> i32 {
                p1_fd_prestat_dir_name(&mut caller, fd, path as u32, len as u32)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_read",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, iovs, iovs_len, nread): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_fd_read(&mut caller, fd, iovs as u32, iovs_len as u32, nread as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_readdir",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, buf, buf_len, cookie, bufused): (i32, i32, i32, i64, i32)| {
                Box::new(async move {
                    p1_fd_readdir(
                        &mut caller,
                        fd,
                        buf as u32,
                        buf_len as u32,
                        cookie as u64,
                        bufused as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "fd_renumber",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             from: i32,
             to: i32|
             -> i32 { p1_fd_renumber(&mut caller, from, to) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_seek",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, offset, whence, new_offset): (i32, i64, i32, i32)| {
                Box::new(async move {
                    p1_fd_seek(&mut caller, fd, offset, whence as u8, new_offset as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "fd_write",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, iovs, iovs_len, nwritten): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_fd_write(
                        &mut caller,
                        fd,
                        iovs as u32,
                        iovs_len as u32,
                        nwritten as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "path_filestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, flags, path, path_len, stat): (i32, i32, i32, i32, i32)| {
                Box::new(async move {
                    wasix_path_filestat_get(
                        &mut caller,
                        fd,
                        flags as u32,
                        path as u32,
                        path_len as u32,
                        stat as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "path_open",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (
                fd,
                dirflags,
                path,
                path_len,
                oflags,
                fs_rights_base,
                _fs_rights_inheriting,
                fdflags,
                opened_fd,
            ): (i32, i32, i32, i32, i32, i64, i64, i32, i32)| {
                Box::new(async move {
                    wasix_path_open(
                        &mut caller,
                        fd,
                        dirflags as u32,
                        path as u32,
                        path_len as u32,
                        oflags as u16,
                        fs_rights_base as u64,
                        fdflags as u16,
                        opened_fd as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            WASIX_MODULE,
            "sched_yield",
            |_caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>, ()| {
                Box::new(async move {
                    crate::yield_now().await;
                    p1::errno::SUCCESS
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_exit",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             code: i32|
             -> wasmtime::Result<()> {
                caller.data_mut().request_exit(code as u32);
                Err(wasmtime::Error::new(Preview1Exit))
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            WASIX_MODULE,
            "proc_exit2",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             code: i32|
             -> wasmtime::Result<()> {
                caller.data_mut().request_exit(code as u32);
                Err(wasmtime::Error::new(Preview1Exit))
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

fn add_preview1_program_remaining_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_create_directory",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, path, path_len): (i32, i32, i32)| {
                Box::new(async move {
                    p1_path_create_directory(&mut caller, fd, path as u32, path_len as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_filestat_get",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, flags, path, path_len, stat): (i32, i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_path_filestat_get(
                        &mut caller,
                        fd,
                        flags as u32,
                        path as u32,
                        path_len as u32,
                        stat as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "path_filestat_set_times",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             flags: i32,
             path: i32,
             path_len: i32,
             atim: i64,
             mtim: i64,
             fstflags: i32|
             -> i32 {
                p1_path_filestat_set_times(
                    &mut caller,
                    fd,
                    flags as u32,
                    path as u32,
                    path_len as u32,
                    atim as u64,
                    mtim as u64,
                    fstflags as u16,
                )
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_link",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (old_fd, old_flags, old_path, old_path_len, new_fd, new_path, new_path_len): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_path_link(
                        &mut caller,
                        old_fd,
                        old_flags as u32,
                        old_path as u32,
                        old_path_len as u32,
                        new_fd,
                        new_path as u32,
                        new_path_len as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_readlink",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, path, path_len, buf, buf_len, bufused): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_path_readlink(
                        &mut caller,
                        fd,
                        path as u32,
                        path_len as u32,
                        buf as u32,
                        buf_len as u32,
                        bufused as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_remove_directory",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, path, path_len): (i32, i32, i32)| {
                Box::new(async move {
                    p1_path_remove_directory(&mut caller, fd, path as u32, path_len as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_rename",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (old_fd, old_path, old_path_len, new_fd, new_path, new_path_len): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_path_rename(
                        &mut caller,
                        old_fd,
                        old_path as u32,
                        old_path_len as u32,
                        new_fd,
                        new_path as u32,
                        new_path_len as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_symlink",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (old_path, old_path_len, fd, new_path, new_path_len): (
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_path_symlink(
                        &mut caller,
                        old_path as u32,
                        old_path_len as u32,
                        fd,
                        new_path as u32,
                        new_path_len as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "path_unlink_file",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, path, path_len): (i32, i32, i32)| {
                Box::new(async move {
                    p1_path_unlink_file(&mut caller, fd, path as u32, path_len as u32).await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "poll_oneoff",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (subscriptions, events, nsubscriptions, nevents): (i32, i32, i32, i32)| {
                Box::new(async move {
                    p1_poll_oneoff(
                        &mut caller,
                        subscriptions as u32,
                        events as u32,
                        nsubscriptions as u32,
                        nevents as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "proc_raise",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             signal: i32|
             -> wasmtime::Result<i32> { p1_proc_raise(&mut caller, signal as u32) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "sock_accept",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             fd: i32,
             _flags: i32,
             _fd_out: i32|
             -> i32 { p1_sock_accept(&mut caller, fd) },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "sock_recv",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, ri_data, ri_data_len, ri_flags, ro_datalen, ro_flags): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_sock_recv(
                        &mut caller,
                        fd,
                        ri_data as u32,
                        ri_data_len as u32,
                        ri_flags as u16,
                        ro_datalen as u32,
                        ro_flags as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "sock_send",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, si_data, si_data_len, si_flags, so_datalen): (
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    p1_sock_send(
                        &mut caller,
                        fd,
                        si_data as u32,
                        si_data_len as u32,
                        si_flags as u16,
                        so_datalen as u32,
                    )
                    .await
                })
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap_async(
            "wasi_snapshot_preview1",
            "sock_shutdown",
            |mut caller: Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
             (fd, how): (i32, i32)| {
                Box::new(async move { p1_sock_shutdown(&mut caller, fd, how as u8).await })
            },
        )
        .map_err(map_program_runtime_error)?;
    Ok(())
}

fn p1_args_sizes_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    argc: u32,
    argv_buf_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let count = match u32::try_from(caller.data().arguments.len()) {
        Ok(count) => count,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let size = match nul_terminated_list_size(caller.data().arguments.iter().map(String::as_str)) {
        Some(size) => size,
        None => return p1::errno::OVERFLOW,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u32(caller, memory, argc, count).max(p1_write_u32(caller, memory, argv_buf_size, size))
}

fn p1_args_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    argv: u32,
    argv_buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let values = caller.data().arguments.clone();
    p1_write_string_array(caller, argv, argv_buf, values.iter().map(String::as_str))
}

fn p1_environ_sizes_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    count: u32,
    size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let env = p1_environment_strings(caller.data());
    let env_count = match u32::try_from(env.len()) {
        Ok(count) => count,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let env_size = match nul_terminated_list_size(env.iter().map(String::as_str)) {
        Some(size) => size,
        None => return p1::errno::OVERFLOW,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u32(caller, memory, count, env_count).max(p1_write_u32(caller, memory, size, env_size))
}

fn p1_environ_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    environ: u32,
    environ_buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let env = p1_environment_strings(caller.data());
    p1_write_string_array(caller, environ, environ_buf, env.iter().map(String::as_str))
}

fn p1_clock_res_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    id: i32,
    resolution: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match id {
        0 | 1 => {
            let Some(memory) = p1_memory(caller) else {
                return p1::errno::FAULT;
            };
            p1_write_u64(caller, memory, resolution, 1)
        }
        _ => p1::errno::INVAL,
    }
}

fn p1_clock_time_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    id: i32,
    timestamp: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let value = match id {
        0 => caller.data().system_time_nanos(),
        1 => caller.data().now_nanos(),
        _ => return p1::errno::INVAL,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u64(caller, memory, timestamp, value)
}

fn p1_random_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ptr: u32,
    len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let mut bytes = vec![0; len as usize];
    if caller.data_mut().entropy.fill_secure(&mut bytes).is_err() {
        return p1::errno::IO;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_memory(caller, memory, ptr, &bytes)
}

async fn p1_fd_write<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    nwritten: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let iovs = match p1_read_iovs(caller, memory, iovs, iovs_len) {
        Ok(iovs) => iovs,
        Err(errno) => return errno,
    };
    let mut bytes = Vec::new();
    for (ptr, len) in iovs {
        let len = match usize::try_from(len) {
            Ok(len) => len,
            Err(_) => return p1::errno::OVERFLOW,
        };
        let Ok(chunk) = p1_read_memory(caller, memory, ptr, len) else {
            return p1::errno::FAULT;
        };
        bytes.extend_from_slice(&chunk);
    }
    let written = match p1_write_descriptor(caller, fd, &bytes).await {
        Ok(written) => written,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, nwritten, written)
}

async fn p1_fd_read<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    nread: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let iovs = match p1_read_iovs(caller, memory, iovs, iovs_len) {
        Ok(iovs) => iovs,
        Err(errno) => return errno,
    };
    let capacity = iovs
        .iter()
        .try_fold(0usize, |acc, (_, len)| acc.checked_add(*len as usize))
        .unwrap_or(usize::MAX);
    let bytes = match p1_read_descriptor(caller, fd, capacity).await {
        Ok(bytes) => bytes,
        Err(errno) => return errno,
    };
    let mut copied = 0usize;
    for (ptr, len) in iovs {
        if copied >= bytes.len() {
            break;
        }
        let len = (len as usize).min(bytes.len() - copied);
        let status = p1_write_memory(caller, memory, ptr, &bytes[copied..copied + len]);
        if status != p1::errno::SUCCESS {
            return status;
        }
        copied += len;
    }
    let copied = match u32::try_from(copied) {
        Ok(copied) => copied,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, nread, copied)
}

fn p1_fd_prestat_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(Preview1Descriptor::Preopen { guest_name, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let len = match u32::try_from(guest_name.len()) {
        Ok(len) => len,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u8(caller, memory, buf, 0).max(p1_write_u32(caller, memory, buf + 4, len))
}

fn p1_fd_prestat_dir_name<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    path: u32,
    len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let Some(Preview1Descriptor::Preopen { guest_name, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let bytes = guest_name.as_bytes();
    if bytes.len() != len as usize {
        return p1::errno::INVAL;
    }
    preview1_write_memory(memory, path, bytes)
}

fn p1_fd_fdstat_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    stat: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(descriptor) = caller.data().descriptors.get(fd) else {
        return p1::errno::BADF;
    };
    let filetype = match descriptor {
        Preview1Descriptor::Stdin { .. } => 2,
        Preview1Descriptor::Stdout | Preview1Descriptor::Stderr => 2,
        Preview1Descriptor::PipeRead { .. } | Preview1Descriptor::PipeWrite { .. } => 2,
        Preview1Descriptor::Event(_) => 2,
        Preview1Descriptor::Preopen { .. } => 3,
        Preview1Descriptor::File { descriptor, .. } => p1_filetype(descriptor.kind),
        Preview1Descriptor::Socket(_) => 6,
    };
    let fdflags = match descriptor {
        Preview1Descriptor::File { fdflags, .. } => *fdflags,
        _ => 0,
    };
    let rights = p1_descriptor_rights(descriptor);
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let status = p1_write_u8(caller, memory, stat, filetype);
    let status = status.max(p1_write_u16(caller, memory, stat + 2, fdflags));
    let status = status.max(p1_write_u64(caller, memory, stat + 8, rights));
    status.max(p1_write_u64(caller, memory, stat + 16, rights))
}

fn p1_fd_fdstat_set_flags<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    fdflags: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match caller.data_mut().descriptors.get_mut(fd) {
        Some(Preview1Descriptor::File {
            fdflags: current, ..
        }) => {
            *current = fdflags;
            p1::errno::SUCCESS
        }
        Some(_) if fdflags == 0 => p1::errno::SUCCESS,
        Some(_) => p1::errno::INVAL,
        None => p1::errno::BADF,
    }
}

fn p1_fd_fdstat_set_rights<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    rights_base: u64,
    rights_inheriting: u64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let requested = rights_base | rights_inheriting;
    let Some(descriptor) = caller.data().descriptors.get(fd) else {
        return p1::errno::BADF;
    };
    let current = p1_descriptor_rights(descriptor);
    if requested & !current != 0 {
        return p1::errno::NOTCAPABLE;
    }
    let lowered_flags = p1_descriptor_flags(requested, 0);
    match caller.data_mut().descriptors.get_mut(fd) {
        Some(Preview1Descriptor::Preopen { descriptor, .. })
        | Some(Preview1Descriptor::File { descriptor, .. }) => {
            descriptor.flags &= lowered_flags;
            p1::errno::SUCCESS
        }
        Some(_) => p1::errno::SUCCESS,
        None => p1::errno::BADF,
    }
}

async fn p1_fd_filestat_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    stat: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(path) = p1_descriptor_path(caller.data().descriptors.get(fd)) else {
        return p1::errno::BADF;
    };
    let stat_value = if let Some(host_path) =
        crate::guest_host_share_path(path).map(ToOwned::to_owned)
    {
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        match service.stat_path(&host_path).await {
            Ok(metadata) => p1_descriptor_stat_from_host_metadata(metadata),
            Err(error) => {
                return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
            }
        }
    } else {
        match caller.data().filesystem.stat(path) {
            Ok(stat) => stat,
            Err(error) => return p1_errno_from_fs(error),
        }
    };
    p1_write_filestat(caller, stat, stat_value)
}

async fn p1_fd_filestat_set_size<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    size: u64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(Preview1Descriptor::File { descriptor, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let descriptor = descriptor.clone();
    if let Some(host_path) = crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned) {
        if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
            return p1::errno::NOTCAPABLE;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .set_file_size(&host_path, size)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&descriptor.path);
                p1::errno::SUCCESS
            });
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .set_size(&descriptor, size, now_nanos)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

fn p1_fd_filestat_set_times<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    atim: u64,
    mtim: u64,
    fstflags: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let (Some(Preview1Descriptor::File { descriptor, .. })
    | Some(Preview1Descriptor::Preopen { descriptor, .. })) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let descriptor = descriptor.clone();
    if crate::guest_host_share_path(&descriptor.path).is_some() {
        return p1::errno::NOTSUP;
    }
    let now_nanos = caller.data().system_time_nanos();
    let access = p1_timestamp_from_fstflags(
        fstflags,
        P1_FSTFLAG_ATIM,
        P1_FSTFLAG_ATIM_NOW,
        atim,
        now_nanos,
    );
    let modified = p1_timestamp_from_fstflags(
        fstflags,
        P1_FSTFLAG_MTIM,
        P1_FSTFLAG_MTIM_NOW,
        mtim,
        now_nanos,
    );
    caller
        .data_mut()
        .filesystem
        .set_times(&descriptor, access, modified, now_nanos)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

fn p1_fd_advise<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    caller
        .data()
        .descriptors
        .get(fd)
        .map_or(p1::errno::BADF, |_| p1::errno::SUCCESS)
}

async fn p1_fd_allocate<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    offset: u64,
    len: u64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let end = match offset.checked_add(len) {
        Some(end) => end,
        None => return p1::errno::OVERFLOW,
    };
    let Some(Preview1Descriptor::File { descriptor, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let descriptor = descriptor.clone();
    if let Some(host_path) = crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned) {
        if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
            return p1::errno::NOTCAPABLE;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        let current = match service.stat_path(&host_path).await {
            Ok(metadata) => metadata.size,
            Err(error) => {
                return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
            }
        };
        if end <= current {
            return p1::errno::SUCCESS;
        }
        return service
            .set_file_size(&host_path, end)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&descriptor.path);
                p1::errno::SUCCESS
            });
    }
    let current = match caller.data().filesystem.stat(&descriptor.path) {
        Ok(stat) => stat.size,
        Err(error) => return p1_errno_from_fs(error),
    };
    if end <= current {
        return p1::errno::SUCCESS;
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .set_size(&descriptor, end, now_nanos)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

fn p1_fd_datasync<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    p1_fd_advise(caller, fd)
}

fn p1_fd_sync<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    p1_fd_advise(caller, fd)
}

async fn p1_fd_pread<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    offset: u64,
    nread: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let iovs = match p1_read_iovs(caller, memory, iovs, iovs_len) {
        Ok(iovs) => iovs,
        Err(errno) => return errno,
    };
    let capacity = iovs
        .iter()
        .try_fold(0usize, |acc, (_, len)| acc.checked_add(*len as usize))
        .unwrap_or(usize::MAX);
    let Some(Preview1Descriptor::File { descriptor, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let bytes = if let Some(host_path) =
        crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned)
    {
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        let max_bytes = match u32::try_from(capacity) {
            Ok(max_bytes) => max_bytes,
            Err(_) => return p1::errno::OVERFLOW,
        };
        match service.read_file_range(&host_path, offset, max_bytes).await {
            Ok(bytes) => bytes,
            Err(error) => {
                return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
            }
        }
    } else {
        match caller
            .data()
            .filesystem
            .read_file_chunk(descriptor, offset, capacity)
        {
            Ok(bytes) => bytes,
            Err(error) => return p1_errno_from_fs(error),
        }
    };
    p1_write_iovs_from_bytes(caller, memory, iovs, &bytes, nread)
}

async fn p1_fd_pwrite<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    offset: u64,
    nwritten: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let iovs = match p1_read_iovs(caller, memory, iovs, iovs_len) {
        Ok(iovs) => iovs,
        Err(errno) => return errno,
    };
    let mut bytes = Vec::new();
    for (ptr, len) in iovs {
        let Ok(chunk) = p1_read_memory(caller, memory, ptr, len as usize) else {
            return p1::errno::FAULT;
        };
        bytes.extend_from_slice(&chunk);
    }
    let Some(Preview1Descriptor::File { descriptor, .. }) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let offset: usize = match offset.try_into() {
        Ok(offset) => offset,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let descriptor = descriptor.clone();
    if let Some(host_path) = crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned) {
        if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
            return p1::errno::NOTCAPABLE;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        if let Err(error) = service.write_file(&host_path, offset as u64, &bytes).await {
            return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
        }
        caller
            .data_mut()
            .filesystem
            .invalidate_host_subtree(&descriptor.path);
        let written = match u32::try_from(bytes.len()) {
            Ok(written) => written,
            Err(_) => return p1::errno::OVERFLOW,
        };
        return p1_write_u32(caller, memory, nwritten, written);
    }
    let now_nanos = caller.data().now_nanos();
    if let Err(error) =
        caller
            .data_mut()
            .filesystem
            .write_at(&descriptor, offset, &bytes, now_nanos)
    {
        return p1_errno_from_fs(error);
    }
    let written = match u32::try_from(bytes.len()) {
        Ok(written) => written,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, nwritten, written)
}

async fn p1_fd_readdir<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    buf: u32,
    buf_len: u32,
    cookie: u64,
    bufused: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(path) = p1_descriptor_path(caller.data().descriptors.get(fd)) else {
        return p1::errno::BADF;
    };
    if let Some(host_path) = crate::guest_host_share_path(path).map(ToOwned::to_owned) {
        let directory_path = path.to_owned();
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        let entries = match service.read_dir(&host_path).await {
            Ok(entries) => entries,
            Err(error) => {
                return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
            }
        };
        caller
            .data_mut()
            .filesystem
            .seed_host_directory_entries(&directory_path, entries);
        let entries = match caller.data().filesystem.read_directory(&directory_path) {
            Ok(entries) => entries,
            Err(error) => return p1_errno_from_fs(error),
        };
        return p1_fd_readdir_entries(caller, entries, buf, buf_len, cookie, bufused);
    }
    let entries = match caller.data().filesystem.read_directory(path) {
        Ok(entries) => entries,
        Err(error) => return p1_errno_from_fs(error),
    };
    p1_fd_readdir_entries(caller, entries, buf, buf_len, cookie, bufused)
}

fn p1_fd_readdir_entries<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    entries: Vec<fs_types::DirectoryEntry>,
    buf: u32,
    buf_len: u32,
    cookie: u64,
    bufused: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let start_index = match usize::try_from(cookie) {
        Ok(index) => index,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let mut used = 0usize;
    let capacity = buf_len as usize;
    for (index, entry) in entries.iter().enumerate().skip(start_index) {
        let dirent_len = 24usize;
        if used >= capacity {
            break;
        }
        let next = match u64::try_from(index + 1) {
            Ok(next) => next,
            Err(_) => return p1::errno::OVERFLOW,
        };
        let name = entry.name.as_bytes();
        if capacity - used < dirent_len {
            break;
        }
        let dirent_ptr = match buf.checked_add(used as u32) {
            Some(ptr) => ptr,
            None => return p1::errno::OVERFLOW,
        };
        let status = p1_write_u64(caller, memory, dirent_ptr, next)
            .max(p1_write_u64(caller, memory, dirent_ptr + 8, next))
            .max(p1_write_u32(
                caller,
                memory,
                dirent_ptr + 16,
                name.len().try_into().unwrap_or(u32::MAX),
            ))
            .max(p1_write_u8(
                caller,
                memory,
                dirent_ptr + 20,
                p1_filetype_from_descriptor_type(entry.type_.clone()),
            ));
        if status != p1::errno::SUCCESS {
            return status;
        }
        used += dirent_len;
        let remaining = capacity - used;
        let copied = remaining.min(name.len());
        let name_ptr = match buf.checked_add(used as u32) {
            Some(ptr) => ptr,
            None => return p1::errno::OVERFLOW,
        };
        let status = p1_write_memory(caller, memory, name_ptr, &name[..copied]);
        if status != p1::errno::SUCCESS {
            return status;
        }
        used += copied;
        if copied < name.len() {
            break;
        }
    }
    let used = match u32::try_from(used) {
        Ok(used) => used,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, bufused, used)
}

fn p1_fd_renumber<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    from: i32,
    to: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    caller.data_mut().descriptors.renumber(from, to)
}

async fn p1_fd_seek<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    offset: i64,
    whence: u8,
    new_offset: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let (descriptor, current) = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::File {
            descriptor, offset, ..
        }) => (descriptor.clone(), *offset),
        Some(_) => return p1::errno::SPIPE,
        None => return p1::errno::BADF,
    };
    let base = match whence {
        0 => 0,
        1 => current,
        2 => {
            if let Some(host_path) =
                crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned)
            {
                let service = match caller.data().filesystem.host_service() {
                    Ok(service) => service,
                    Err(error) => return p1_errno_from_fs(error),
                };
                match service.stat_path(&host_path).await {
                    Ok(metadata) => metadata.size,
                    Err(error) => {
                        return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(
                            error,
                        ));
                    }
                }
            } else {
                match caller.data().filesystem.stat(&descriptor.path) {
                    Ok(stat) => stat.size,
                    Err(error) => return p1_errno_from_fs(error),
                }
            }
        }
        _ => return p1::errno::INVAL,
    };
    let next = if offset >= 0 {
        base.checked_add(offset as u64)
    } else {
        base.checked_sub(offset.unsigned_abs())
    };
    let Some(next) = next else {
        return p1::errno::INVAL;
    };
    let Some(Preview1Descriptor::File { offset, .. }) = caller.data_mut().descriptors.get_mut(fd)
    else {
        return p1::errno::BADF;
    };
    *offset = next;
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u64(caller, memory, new_offset, next)
}

fn p1_fd_tell<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    offset_out: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let offset = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::File { offset, .. }) => *offset,
        Some(_) => return p1::errno::SPIPE,
        None => return p1::errno::BADF,
    };
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u64(caller, memory, offset_out, offset)
}

#[allow(clippy::too_many_arguments)]
async fn p1_path_open<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    dirflags: u32,
    path: u32,
    path_len: u32,
    oflags: u16,
    rights: u64,
    fdflags: u16,
    opened_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let base = match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Preopen { descriptor, .. })
        | Some(Preview1Descriptor::File { descriptor, .. })
            if descriptor.kind == FsNodeKind::Directory =>
        {
            descriptor.clone()
        }
        Some(_) => return p1::errno::NOTDIR,
        None => return p1::errno::BADF,
    };
    let path_flags = p1_path_flags(dirflags);
    let open_flags = p1_open_flags(oflags);
    let descriptor_flags = p1_descriptor_flags(rights, fdflags);
    p1_path_open_resolved(
        caller,
        memory,
        base,
        path,
        path_flags,
        open_flags,
        descriptor_flags,
        fdflags,
        opened_fd,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn p1_path_open_resolved<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    base: FsDescriptor,
    path: String,
    path_flags: fs_types::PathFlags,
    open_flags: fs_types::OpenFlags,
    descriptor_flags: fs_types::DescriptorFlags,
    fdflags: u16,
    opened_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if base.kind != FsNodeKind::Directory {
        return p1::errno::NOTDIR;
    }
    if let Err(error) = crate::wasmtime_adapter::wasi::validate_descriptor_flags_within_base(
        base.flags,
        descriptor_flags,
    ) {
        return p1_errno_from_fs(error);
    }
    let absolute = match crate::resolve_child_path(&base.path, &path) {
        Ok(absolute) => absolute,
        Err(error) => return p1_errno_from_component_path(error),
    };
    if let Some(host_path) = crate::guest_host_share_path(&absolute).map(ToOwned::to_owned) {
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        let metadata = service.stat_path(&host_path).await;
        let (kind, identity, contents) = match metadata {
            Ok(metadata) => {
                let kind = if metadata.qid_type & 0x80 != 0 {
                    FsNodeKind::Directory
                } else {
                    FsNodeKind::File
                };
                if open_flags.contains(fs_types::OpenFlags::EXCLUSIVE)
                    && open_flags.contains(fs_types::OpenFlags::CREATE)
                {
                    return p1::errno::EXIST;
                }
                if open_flags.contains(fs_types::OpenFlags::DIRECTORY)
                    && kind != FsNodeKind::Directory
                {
                    return p1::errno::NOTDIR;
                }
                if !open_flags.contains(fs_types::OpenFlags::DIRECTORY)
                    && kind == FsNodeKind::Directory
                {
                    return p1::errno::ISDIR;
                }
                if open_flags.contains(fs_types::OpenFlags::TRUNCATE) {
                    if kind != FsNodeKind::File {
                        return p1::errno::ISDIR;
                    }
                    if !base
                        .flags
                        .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
                    {
                        return p1::errno::ROFS;
                    }
                    if let Err(error) = service.truncate_file(&host_path).await {
                        return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(
                            error,
                        ));
                    }
                }
                if kind == FsNodeKind::File {
                    let contents = match service.read_file(&host_path).await {
                        Ok(contents) => contents,
                        Err(error) => {
                            return p1_errno_from_fs(
                                crate::wasmtime_adapter::wasi::map_host_fs_error(error),
                            );
                        }
                    };
                    (kind, metadata.identity, Some(contents))
                } else {
                    let entries = match service.read_dir(&host_path).await {
                        Ok(entries) => entries,
                        Err(error) => {
                            return p1_errno_from_fs(
                                crate::wasmtime_adapter::wasi::map_host_fs_error(error),
                            );
                        }
                    };
                    caller
                        .data_mut()
                        .filesystem
                        .seed_host_directory_entries(&absolute, entries);
                    (kind, metadata.identity, None)
                }
            }
            Err(error) => {
                let error = crate::wasmtime_adapter::wasi::map_host_fs_error(error);
                if !matches!(error, fs_types::ErrorCode::NoEntry)
                    || !open_flags.contains(fs_types::OpenFlags::CREATE)
                {
                    return p1_errno_from_fs(error);
                }
                if !base
                    .flags
                    .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
                {
                    return p1::errno::ROFS;
                }
                if let Err(error) = service.create_file(&host_path).await {
                    return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(
                        error,
                    ));
                }
                let metadata = match service.stat_path(&host_path).await {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(
                            error,
                        ));
                    }
                };
                (FsNodeKind::File, metadata.identity, Some(Vec::new()))
            }
        };
        if let Some(contents) = contents {
            caller
                .data_mut()
                .filesystem
                .seed_host_file_content(&absolute, identity, contents);
        }
        let descriptor = FsDescriptor {
            path: absolute,
            kind,
            flags: descriptor_flags,
            identity: Some(identity),
        };
        let fd = match caller
            .data_mut()
            .descriptors
            .insert(Preview1Descriptor::File {
                descriptor,
                offset: 0,
                fdflags,
            }) {
            Ok(fd) => fd,
            Err(errno) => return errno,
        };
        return p1_write_u32(caller, memory, opened_fd, fd);
    }
    let now_nanos = caller.data().now_nanos();
    let opened = match caller.data_mut().filesystem.open_at(
        &base,
        path_flags,
        &path,
        open_flags,
        descriptor_flags,
        now_nanos,
    ) {
        Ok(descriptor) => descriptor,
        Err(error) => return p1_errno_from_fs(error),
    };
    let fd = match caller
        .data_mut()
        .descriptors
        .insert(Preview1Descriptor::File {
            descriptor: opened,
            offset: 0,
            fdflags,
        }) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, opened_fd, fd)
}

#[allow(clippy::too_many_arguments)]
async fn wasix_path_open<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    dirflags: u32,
    path: u32,
    path_len: u32,
    oflags: u16,
    rights: u64,
    fdflags: u16,
    opened_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (base, path) = match caller.data().resolve_wasix_path_base(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let path_flags = p1_path_flags(dirflags);
    let open_flags = p1_open_flags(oflags);
    let descriptor_flags = p1_descriptor_flags(rights, fdflags);
    p1_path_open_resolved(
        caller,
        memory,
        base,
        path,
        path_flags,
        open_flags,
        descriptor_flags,
        fdflags,
        opened_fd,
    )
    .await
}

fn p1_read_path<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    path: u32,
    path_len: u32,
) -> Result<String, ProgramExecError> {
    p1_read_memory(caller, memory, path, path_len as usize).and_then(|bytes| {
        String::from_utf8(bytes).map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidPath,
            detail: ProgramExecErrorDetail::InvalidProgramPathEncoding,
        })
    })
}

async fn p1_path_create_directory<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    path: u32,
    path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (Some(Preview1Descriptor::Preopen { descriptor, .. })
    | Some(Preview1Descriptor::File { descriptor, .. })) = caller.data().descriptors.get(fd)
    else {
        return p1::errno::BADF;
    };
    let descriptor = descriptor.clone();
    let absolute = match crate::resolve_child_path(&descriptor.path, &path) {
        Ok(path) => path,
        Err(error) => return p1_errno_from_component_path(error),
    };
    if let Some(host_path) = crate::guest_host_share_path(&absolute) {
        if descriptor.kind != FsNodeKind::Directory {
            return p1::errno::NOTDIR;
        }
        if !descriptor
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return p1::errno::ROFS;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .create_directory(host_path)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&absolute);
                p1::errno::SUCCESS
            });
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .create_directory_at(&descriptor, &path, now_nanos)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

async fn p1_path_filestat_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    _flags: u32,
    path: u32,
    path_len: u32,
    stat: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let Some(base) = p1_directory_descriptor(caller.data().descriptors.get(fd)) else {
        return p1::errno::BADF;
    };
    let absolute = match crate::resolve_child_path(&base.path, &path) {
        Ok(absolute) => absolute,
        Err(error) => return p1_errno_from_component_path(error),
    };
    let stat_value = if let Some(host_path) = crate::guest_host_share_path(&absolute) {
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        match service.stat_path(host_path).await {
            Ok(metadata) => p1_descriptor_stat_from_host_metadata(metadata),
            Err(error) => {
                return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
            }
        }
    } else {
        match caller.data().filesystem.stat(&absolute) {
            Ok(stat) => stat,
            Err(error) => return p1_errno_from_fs(error),
        }
    };
    p1_write_filestat(caller, stat, stat_value)
}

fn p1_path_filestat_set_times<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    _flags: u32,
    path: u32,
    path_len: u32,
    atim: u64,
    mtim: u64,
    fstflags: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let Some(base) = p1_directory_descriptor(caller.data().descriptors.get(fd)) else {
        return p1::errno::BADF;
    };
    let absolute = match crate::resolve_child_path(&base.path, &path) {
        Ok(absolute) => absolute,
        Err(error) => return p1_errno_from_component_path(error),
    };
    if crate::guest_host_share_path(&absolute).is_some() {
        return p1::errno::NOTSUP;
    }
    let now_nanos = caller.data().system_time_nanos();
    let access = p1_timestamp_from_fstflags(
        fstflags,
        P1_FSTFLAG_ATIM,
        P1_FSTFLAG_ATIM_NOW,
        atim,
        now_nanos,
    );
    let modified = p1_timestamp_from_fstflags(
        fstflags,
        P1_FSTFLAG_MTIM,
        P1_FSTFLAG_MTIM_NOW,
        mtim,
        now_nanos,
    );
    caller
        .data_mut()
        .filesystem
        .set_times_at_path(&absolute, access, modified, now_nanos)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

async fn wasix_path_filestat_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    _flags: u32,
    path: u32,
    path_len: u32,
    stat: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let (base, path) = match caller.data().resolve_wasix_path_base(fd, &path) {
        Ok(resolved) => resolved,
        Err(errno) => return errno,
    };
    let absolute = match crate::resolve_child_path(&base.path, &path) {
        Ok(absolute) => absolute,
        Err(error) => return p1_errno_from_component_path(error),
    };
    let stat_value = if let Some(host_path) = crate::guest_host_share_path(&absolute) {
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        match service.stat_path(host_path).await {
            Ok(metadata) => p1_descriptor_stat_from_host_metadata(metadata),
            Err(error) => {
                return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
            }
        }
    } else {
        match caller.data().filesystem.stat(&absolute) {
            Ok(stat) => stat,
            Err(error) => return p1_errno_from_fs(error),
        }
    };
    p1_write_filestat(caller, stat, stat_value)
}

async fn p1_path_link<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    old_fd: i32,
    _old_flags: u32,
    old_path: u32,
    old_path_len: u32,
    new_fd: i32,
    new_path: u32,
    new_path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let old_path = match p1_read_path(caller, memory, old_path, old_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let new_path = match p1_read_path(caller, memory, new_path, new_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let Some(source_base) = p1_directory_descriptor(caller.data().descriptors.get(old_fd)) else {
        return p1::errno::BADF;
    };
    let Some(destination_base) = p1_directory_descriptor(caller.data().descriptors.get(new_fd))
    else {
        return p1::errno::BADF;
    };
    let source_base = source_base.clone();
    let destination_base = destination_base.clone();
    let source_absolute = match crate::resolve_child_path(&source_base.path, &old_path) {
        Ok(path) => path,
        Err(error) => return p1_errno_from_component_path(error),
    };
    let destination_absolute = match crate::resolve_child_path(&destination_base.path, &new_path) {
        Ok(path) => path,
        Err(error) => return p1_errno_from_component_path(error),
    };
    let source_host = crate::guest_host_share_path(&source_absolute);
    let destination_host = crate::guest_host_share_path(&destination_absolute);
    if source_host.is_some() || destination_host.is_some() {
        if source_base.kind != FsNodeKind::Directory
            || destination_base.kind != FsNodeKind::Directory
        {
            return p1::errno::NOTDIR;
        }
        if !destination_base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return p1::errno::ROFS;
        }
        let Some(source_host) = source_host else {
            return p1::errno::XDEV;
        };
        let Some(destination_host) = destination_host else {
            return p1::errno::XDEV;
        };
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .hard_link(&source_host, &destination_host)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&destination_absolute);
                p1::errno::SUCCESS
            });
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .link_at(
            &source_base,
            &old_path,
            &destination_base,
            &new_path,
            now_nanos,
        )
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

async fn p1_path_readlink<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    path: u32,
    path_len: u32,
    buf: u32,
    buf_len: u32,
    bufused: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let Some(base) = p1_directory_descriptor(caller.data().descriptors.get(fd)) else {
        return p1::errno::BADF;
    };
    let base = base.clone();
    let absolute = match crate::resolve_child_path(&base.path, &path) {
        Ok(path) => path,
        Err(error) => return p1_errno_from_component_path(error),
    };
    let payload = if let Some(host_path) = crate::guest_host_share_path(&absolute) {
        if base.kind != FsNodeKind::Directory {
            return p1::errno::NOTDIR;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        let payload = match service.read_link(host_path).await {
            Ok(payload) => payload,
            Err(error) => {
                return p1_errno_from_fs(crate::wasmtime_adapter::wasi::map_host_fs_error(error));
            }
        };
        if let Err(error) =
            crate::wasmtime_adapter::wasi::resolve_symlink_payload(&absolute, &payload)
        {
            return p1_errno_from_fs(error);
        }
        payload
    } else {
        match caller.data().filesystem.readlink_at(&base, &path) {
            Ok(payload) => payload,
            Err(error) => return p1_errno_from_fs(error),
        }
    };
    let bytes = payload.as_bytes();
    let copied = (buf_len as usize).min(bytes.len());
    let status = p1_write_memory(caller, memory, buf, &bytes[..copied]);
    if status != p1::errno::SUCCESS {
        return status;
    }
    let copied = match u32::try_from(copied) {
        Ok(copied) => copied,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, bufused, copied)
}

async fn p1_path_remove_directory<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    path: u32,
    path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let Some(base) = p1_directory_descriptor(caller.data().descriptors.get(fd)) else {
        return p1::errno::BADF;
    };
    let base = base.clone();
    let absolute = match crate::resolve_child_path(&base.path, &path) {
        Ok(path) => path,
        Err(error) => return p1_errno_from_component_path(error),
    };
    if let Some(host_path) = crate::guest_host_share_path(&absolute) {
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return p1::errno::ROFS;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .remove(host_path, true)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&absolute);
                p1::errno::SUCCESS
            });
    }
    caller
        .data_mut()
        .filesystem
        .remove_directory_at(&base, &path)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

async fn p1_path_rename<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    old_fd: i32,
    old_path: u32,
    old_path_len: u32,
    new_fd: i32,
    new_path: u32,
    new_path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let old_path = match p1_read_path(caller, memory, old_path, old_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let new_path = match p1_read_path(caller, memory, new_path, new_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let Some(source_base) = p1_directory_descriptor(caller.data().descriptors.get(old_fd)) else {
        return p1::errno::BADF;
    };
    let Some(destination_base) = p1_directory_descriptor(caller.data().descriptors.get(new_fd))
    else {
        return p1::errno::BADF;
    };
    let source_base = source_base.clone();
    let destination_base = destination_base.clone();
    let source_absolute = match crate::resolve_child_path(&source_base.path, &old_path) {
        Ok(path) => path,
        Err(error) => return p1_errno_from_component_path(error),
    };
    let destination_absolute = match crate::resolve_child_path(&destination_base.path, &new_path) {
        Ok(path) => path,
        Err(error) => return p1_errno_from_component_path(error),
    };
    let source_host = crate::guest_host_share_path(&source_absolute).map(ToOwned::to_owned);
    let destination_host =
        crate::guest_host_share_path(&destination_absolute).map(ToOwned::to_owned);
    if source_host.is_some() || destination_host.is_some() {
        if !source_base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
            || !destination_base
                .flags
                .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return p1::errno::ROFS;
        }
        let Some(source_host) = source_host else {
            return p1::errno::XDEV;
        };
        let Some(destination_host) = destination_host else {
            return p1::errno::XDEV;
        };
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .rename(&source_host, &destination_host)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                let filesystem = &mut caller.data_mut().filesystem;
                filesystem.invalidate_host_subtree(&source_absolute);
                filesystem.invalidate_host_subtree(&destination_absolute);
                p1::errno::SUCCESS
            });
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .rename_at(
            &source_base,
            &old_path,
            &destination_base,
            &new_path,
            now_nanos,
        )
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

async fn p1_path_symlink<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    old_path: u32,
    old_path_len: u32,
    fd: i32,
    new_path: u32,
    new_path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let old_path = match p1_read_path(caller, memory, old_path, old_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let new_path = match p1_read_path(caller, memory, new_path, new_path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let Some(base) = p1_directory_descriptor(caller.data().descriptors.get(fd)) else {
        return p1::errno::BADF;
    };
    let base = base.clone();
    let absolute = match crate::resolve_child_path(&base.path, &new_path) {
        Ok(path) => path,
        Err(error) => return p1_errno_from_component_path(error),
    };
    if let Err(error) = crate::wasmtime_adapter::wasi::resolve_symlink_payload(&absolute, &old_path)
    {
        return p1_errno_from_fs(error);
    }
    if let Some(host_path) = crate::guest_host_share_path(&absolute) {
        if base.kind != FsNodeKind::Directory {
            return p1::errno::NOTDIR;
        }
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return p1::errno::ROFS;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .symlink(&old_path, host_path)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&absolute);
                p1::errno::SUCCESS
            });
    }
    let now_nanos = caller.data().now_nanos();
    caller
        .data_mut()
        .filesystem
        .symlink_at(&base, &new_path, &old_path, now_nanos)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

async fn p1_path_unlink_file<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    path: u32,
    path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    let Some(base) = p1_directory_descriptor(caller.data().descriptors.get(fd)) else {
        return p1::errno::BADF;
    };
    let base = base.clone();
    let absolute = match crate::resolve_child_path(&base.path, &path) {
        Ok(path) => path,
        Err(error) => return p1_errno_from_component_path(error),
    };
    if let Some(host_path) = crate::guest_host_share_path(&absolute) {
        if !base
            .flags
            .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
        {
            return p1::errno::ROFS;
        }
        let service = match caller.data().filesystem.host_service() {
            Ok(service) => service,
            Err(error) => return p1_errno_from_fs(error),
        };
        return service
            .remove(host_path, false)
            .await
            .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
            .map_or_else(p1_errno_from_fs, |_| {
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&absolute);
                p1::errno::SUCCESS
            });
    }
    caller
        .data_mut()
        .filesystem
        .unlink_file_at(&base, &path)
        .map_or_else(p1_errno_from_fs, |_| p1::errno::SUCCESS)
}

async fn p1_poll_oneoff<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    subscriptions: u32,
    events: u32,
    nsubscriptions: u32,
    nevents: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if nsubscriptions == 0 {
        return p1::errno::INVAL;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let mut event_count = 0u32;
    for index in 0..nsubscriptions {
        let Some(subscription_ptr) = index
            .checked_mul(P1_SUBSCRIPTION_SIZE)
            .and_then(|offset| subscriptions.checked_add(offset))
        else {
            return p1::errno::OVERFLOW;
        };
        let userdata = match p1_try_read_u64(caller, memory, subscription_ptr) {
            Ok(userdata) => userdata,
            Err(_) => return p1::errno::FAULT,
        };
        let event_type = match p1_try_read_u8(caller, memory, subscription_ptr + 8) {
            Ok(event_type) => event_type,
            Err(_) => return p1::errno::FAULT,
        };
        let mut error = p1::errno::SUCCESS as u16;
        let mut nbytes = 0u64;
        let mut fd_flags = 0u16;
        match event_type {
            P1_EVENTTYPE_CLOCK => {
                let clock_id = match p1_try_read_u32(caller, memory, subscription_ptr + 16) {
                    Ok(clock_id) => clock_id,
                    Err(_) => return p1::errno::FAULT,
                };
                let timeout = match p1_try_read_u64(caller, memory, subscription_ptr + 24) {
                    Ok(timeout) => timeout,
                    Err(_) => return p1::errno::FAULT,
                };
                let flags = match p1_try_read_u16(caller, memory, subscription_ptr + 40) {
                    Ok(flags) => flags,
                    Err(_) => return p1::errno::FAULT,
                };
                if !matches!(clock_id, 0 | 1) {
                    error = p1::errno::INVAL as u16;
                } else {
                    let now = if clock_id == 0 {
                        caller.data().system_time_nanos()
                    } else {
                        caller.data().now_nanos()
                    };
                    let duration = if flags & P1_SUBSCRIPTION_CLOCK_ABSTIME != 0 {
                        timeout.saturating_sub(now)
                    } else {
                        timeout
                    };
                    if duration != 0 {
                        caller
                            .data()
                            .timer
                            .sleep_for(Duration::from_nanos(duration))
                            .await;
                    }
                }
            }
            P1_EVENTTYPE_FD_READ | P1_EVENTTYPE_FD_WRITE => {
                let fd = match p1_try_read_u32(caller, memory, subscription_ptr + 16) {
                    Ok(fd) => fd as i32,
                    Err(_) => return p1::errno::FAULT,
                };
                match p1_poll_descriptor(caller.data().descriptors.get(fd), event_type) {
                    Ok(bytes) => nbytes = bytes,
                    Err(errno) => error = errno as u16,
                }
                fd_flags = 0;
            }
            _ => error = p1::errno::INVAL as u16,
        }
        let Some(event_ptr) = event_count
            .checked_mul(P1_EVENT_SIZE)
            .and_then(|offset| events.checked_add(offset))
        else {
            return p1::errno::OVERFLOW;
        };
        let status = p1_write_u64(caller, memory, event_ptr, userdata)
            .max(p1_write_u16(caller, memory, event_ptr + 8, error))
            .max(p1_write_u8(caller, memory, event_ptr + 10, event_type))
            .max(p1_write_u64(caller, memory, event_ptr + 16, nbytes))
            .max(p1_write_u16(caller, memory, event_ptr + 24, fd_flags));
        if status != p1::errno::SUCCESS {
            return status;
        }
        event_count += 1;
    }
    p1_write_u32(caller, memory, nevents, event_count)
}

fn p1_proc_raise<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    signal: u32,
) -> wasmtime::Result<i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    caller
        .data_mut()
        .request_exit(128u32.saturating_add(signal));
    Err(wasmtime::Error::new(Preview1Exit))
}

fn wasix_clock_time_set<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    clock_id: i32,
    timestamp: i64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if clock_id != 0 {
        return p1::errno::INVAL;
    }
    let Ok(timestamp) = u64::try_from(timestamp) else {
        return p1::errno::INVAL;
    };
    caller.data_mut().set_system_time_nanos(timestamp)
}

fn wasix_fd_dup<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let duplicated = match caller.data_mut().descriptors.dup(fd) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, ret_fd, duplicated)
}

fn wasix_fd_dup2<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    target_fd: i32,
    cloexec: bool,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let duplicated = match caller.data_mut().descriptors.dup_to(fd, target_fd, cloexec) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, ret_fd, duplicated)
}

fn wasix_fd_pipe<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_fd1: u32,
    ret_fd2: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let (writer, reader) = crate::byte_channel();
    let read_fd = match caller
        .data_mut()
        .descriptors
        .insert(Preview1Descriptor::PipeRead {
            reader,
            carry: Vec::new(),
        }) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    let write_fd = match caller
        .data_mut()
        .descriptors
        .insert(Preview1Descriptor::PipeWrite { writer })
    {
        Ok(fd) => fd,
        Err(errno) => {
            let _ = caller.data_mut().descriptors.close(read_fd as i32);
            return errno;
        }
    };
    let status = p1_write_u32(caller, memory, ret_fd1, read_fd);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_u32(caller, memory, ret_fd2, write_fd)
}

fn wasix_fd_event<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    initial_value: u64,
    flags: u32,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if flags & !WASIX_EVENTFDFLAG_SEMAPHORE != 0 {
        return p1::errno::INVAL;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let fd = match caller
        .data_mut()
        .descriptors
        .insert(Preview1Descriptor::Event(EventFd::new(
            initial_value,
            flags & WASIX_EVENTFDFLAG_SEMAPHORE != 0,
        ))) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, ret_fd, fd)
}

fn wasix_tty_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    state: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_tty_control();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let tty = caller.data().tty_state;
    write_wasix_tty_state(caller, memory, state, tty)
}

fn wasix_tty_set<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    state: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_tty_control();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let tty = match read_wasix_tty_state(caller, memory, state) {
        Ok(tty) => tty,
        Err(errno) => return errno,
    };
    caller.data_mut().tty_state = tty;
    p1::errno::SUCCESS
}

#[allow(clippy::too_many_arguments)]
async fn wasix_path_open2<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    dirflags: u32,
    path: u32,
    path_len: u32,
    oflags: u16,
    rights: u64,
    fdflags: u16,
    fdflagsext: u16,
    opened_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let close_on_exec = match wasix_close_on_exec_flag(fdflagsext) {
        Ok(close_on_exec) => close_on_exec,
        Err(errno) => return errno,
    };
    let status = wasix_path_open(
        caller, fd, dirflags, path, path_len, oflags, rights, fdflags, opened_fd,
    )
    .await;
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let opened = match p1_try_read_u32(caller, memory, opened_fd) {
        Ok(opened) => opened,
        Err(_) => return p1::errno::FAULT,
    };
    caller
        .data_mut()
        .descriptors
        .set_close_on_exec(opened as i32, close_on_exec)
}

fn wasix_fd_fdflags_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_flags: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let close_on_exec = match caller.data().descriptors.close_on_exec(fd) {
        Ok(close_on_exec) => close_on_exec,
        Err(errno) => return errno,
    };
    let flags = if close_on_exec {
        WASIX_FDFLAGSEXT_CLOEXEC
    } else {
        0
    };
    p1_write_u16(caller, memory, ret_flags, flags)
}

fn wasix_fd_fdflags_set<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    flags: u16,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let close_on_exec = match wasix_close_on_exec_flag(flags) {
        Ok(close_on_exec) => close_on_exec,
        Err(errno) => return errno,
    };
    caller
        .data_mut()
        .descriptors
        .set_close_on_exec(fd, close_on_exec)
}

fn wasix_close_on_exec_flag(flags: u16) -> Result<bool, i32> {
    if flags & !WASIX_FDFLAGSEXT_CLOEXEC != 0 {
        return Err(p1::errno::INVAL);
    }
    Ok(flags & WASIX_FDFLAGSEXT_CLOEXEC != 0)
}

fn wasix_getcwd<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    path: u32,
    path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let capacity = match p1_try_read_u32(caller, memory, path_len) {
        Ok(capacity) => capacity,
        Err(_) => return p1::errno::FAULT,
    };
    let cwd = match caller.data().getcwd() {
        Ok(cwd) => cwd,
        Err(errno) => return errno,
    };
    let needed = match u32::try_from(cwd.len()) {
        Ok(needed) => needed,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let status = preview1_write_u32(memory, path_len, needed);
    if status != p1::errno::SUCCESS {
        return status;
    }
    if capacity < needed {
        return p1::errno::RANGE;
    }
    preview1_write_memory(memory, path, cwd.as_bytes())
}

fn wasix_chdir<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    path: u32,
    path_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let path = match p1_read_path(caller, memory, path, path_len) {
        Ok(path) => path,
        Err(_) => return p1::errno::FAULT,
    };
    caller.data_mut().chdir(&path)
}

fn wasix_callback_signal<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    callback: u32,
    callback_len: u32,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        }));
    };
    let callback =
        p1_read_path(caller, memory, callback, callback_len).map_err(wasmtime::Error::new)?;
    caller.data_mut().signal_callback = Some(callback);
    Ok(())
}

fn wasix_proc_id<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_pid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let pid = match u32::try_from(caller.data().instance.id().raw()) {
        Ok(pid) => pid,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, ret_pid, pid)
}

fn wasix_proc_parent<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    pid: u32,
    ret_pid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let current = match u32::try_from(caller.data().instance.id().raw()) {
        Ok(pid) => pid,
        Err(_) => return p1::errno::OVERFLOW,
    };
    if pid != current {
        return p1::errno::NOENT;
    }
    let parent = match caller.data().parent_instance_id {
        Some(parent) => match u32::try_from(parent.raw()) {
            Ok(pid) => pid,
            Err(_) => return p1::errno::OVERFLOW,
        },
        None => 0,
    };
    p1_write_u32(caller, memory, ret_pid, parent)
}

fn wasix_proc_signal<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    pid: u32,
    signal: i32,
) -> wasmtime::Result<i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_signal_authority();
    if status != p1::errno::SUCCESS {
        return Ok(status);
    }
    let current = match u32::try_from(caller.data().instance.id().raw()) {
        Ok(pid) => pid,
        Err(_) => return Ok(p1::errno::OVERFLOW),
    };
    if pid != current {
        return Ok(p1::errno::SRCH);
    }
    let Ok(signal) = u32::try_from(signal) else {
        return Ok(p1::errno::INVAL);
    };
    caller
        .data_mut()
        .request_exit(128u32.saturating_add(signal));
    Err(wasmtime::Error::new(Preview1Exit))
}

fn wasix_proc_signals_get<CpuImpl, HostFs>(
    _caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    _buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    p1::errno::SUCCESS
}

fn wasix_proc_signals_sizes_get<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u32(caller, memory, ret_size, 0)
}

fn wasix_proc_raise_interval<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    signal: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_signal_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    if !(0..=31).contains(&signal) {
        return p1::errno::INVAL;
    }
    p1::errno::NOTSUP
}

async fn wasix_proc_fork<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_pid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Some(value) = caller.data_mut().asyncify.rewind_value.take() {
        if wasix_call_asyncify_stop_rewind(caller).await != p1::errno::SUCCESS {
            return p1::errno::NOTSUP;
        }
        let Some(memory) = p1_memory(caller) else {
            return p1::errno::FAULT;
        };
        let Ok(pid) = u32::try_from(value) else {
            return p1::errno::OVERFLOW;
        };
        return p1_write_u32(caller, memory, ret_pid, pid);
    }
    let status = caller.data().require_fork_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if caller.data().current_core_module.is_none() {
        return p1::errno::NOTSUP;
    }
    let Ok((stack_lower, stack_upper, stack_pointer)) = wasix_stack_bounds_from_caller(caller)
    else {
        return p1::errno::NOTSUP;
    };
    if stack_lower >= stack_pointer || stack_pointer > stack_upper {
        return p1::errno::INVAL;
    }
    let memory_stack_len = match usize::try_from(stack_upper - stack_pointer) {
        Ok(len) => len,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let memory_stack = match p1_read_memory(caller, memory, stack_pointer, memory_stack_len) {
        Ok(stack) => stack,
        Err(_) => return p1::errno::FAULT,
    };
    let unwind_stack_begin = match stack_lower.checked_add(WASIX_ASYNCIFY_DATA_SIZE) {
        Some(begin) if begin <= stack_pointer => begin,
        _ => return p1::errno::OVERFLOW,
    };
    let status = p1_write_u32(caller, memory, ret_pid, 0);
    if status != p1::errno::SUCCESS {
        return status;
    }
    let status = p1_write_u32(caller, memory, stack_lower, unwind_stack_begin).max(p1_write_u32(
        caller,
        memory,
        stack_lower + 4,
        stack_pointer,
    ));
    if status != p1::errno::SUCCESS {
        return status;
    }
    caller.data_mut().asyncify.phase = WasixAsyncifyPhase::Forking {
        ret_pid,
        stack_lower,
        stack_upper,
        unwind_stack_begin,
        memory_stack,
        stack_pointer,
    };
    wasix_call_asyncify_start_unwind(caller, stack_lower).await
}

async fn wasix_proc_exec<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: u32,
    name_len: u32,
    args: u32,
    args_len: u32,
    env: Option<(u32, u32)>,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_exec_authority();
    if status != p1::errno::SUCCESS {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::PermissionDenied,
            detail: ProgramExecErrorDetail::ProcessAuthorityDenied,
        }));
    }
    let Some(memory) = p1_memory(caller) else {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        }));
    };
    let prepared = wasix_prepare_program(caller, memory, name, name_len).await?;
    let guest_name = prepared.guest_name;
    let source_path = prepared.source_path;
    let mut argv = wasix_read_exec_string(caller, memory, args, args_len)
        .map(|value| wasix_split_lines(&value))
        .unwrap_or_default();
    if argv
        .first()
        .is_some_and(|arg| arg == &guest_name || arg == &source_path)
    {
        argv.remove(0);
    }
    let mut environment = match env {
        Some((ptr, len)) => wasix_read_exec_string(caller, memory, ptr, len)
            .map(|value| wasix_split_environment(&value))
            .unwrap_or_default(),
        None => caller.data().environment.clone(),
    };
    environment.retain(|(name, _)| name.as_str() != HELIOS_PROCESS_ID_ENV);
    let service = caller
        .data()
        .runtime_state
        .program_service()
        .ok_or_else(|| {
            wasmtime::Error::new(ProgramExecError {
                kind: ProgramExecErrorKind::Unavailable,
                detail: ProgramExecErrorDetail::HostOperationFailed,
            })
        })?;
    let exec_context = caller.data().exec_context();
    let authority = caller.data().authority.clone();
    let result = service
        .exec_buffered(
            exec_context,
            guest_name,
            argv,
            environment,
            prepared.source,
            None,
            Vec::new(),
            authority,
        )
        .await
        .map_err(wasmtime::Error::new)?;
    caller.data().write_output(
        crate::ComponentOutputStreamKind::Stdout,
        &result.output.stdout,
    );
    caller.data().write_output(
        crate::ComponentOutputStreamKind::Stderr,
        &result.output.stderr,
    );
    caller.data_mut().request_exit(result.exit_code);
    Err(wasmtime::Error::new(Preview1Exit))
}

async fn wasix_proc_exec3<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: u32,
    name_len: u32,
    args: u32,
    args_len: u32,
    env: u32,
    env_len: u32,
    search_path: i32,
    path: u32,
    path_len: u32,
) -> wasmtime::Result<i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if search_path != 0 || path != 0 || path_len != 0 {
        return Ok(p1::errno::NOTSUP);
    }
    wasix_proc_exec(caller, name, name_len, args, args_len, Some((env, env_len)))
        .await
        .map(|()| p1::errno::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
async fn wasix_proc_spawn<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: u32,
    name_len: u32,
    chroot: i32,
    args: u32,
    args_len: u32,
    _preopen: u32,
    preopen_len: u32,
    stdin: i32,
    stdout: i32,
    stderr: i32,
    working_dir: u32,
    working_dir_len: u32,
    ret_handles: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_spawn_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if chroot != 0 || preopen_len != 0 {
        return p1::errno::NOTSUP;
    }
    if working_dir_len != 0 {
        let working_dir = match wasix_read_exec_string(caller, memory, working_dir, working_dir_len)
        {
            Ok(working_dir) => working_dir,
            Err(_) => return p1::errno::FAULT,
        };
        if !working_dir.is_empty()
            && working_dir != "."
            && Some(working_dir.as_str())
                != caller
                    .data()
                    .cwd
                    .as_ref()
                    .map(|cwd| cwd.guest_name.as_str())
        {
            return p1::errno::NOTSUP;
        }
    }
    let argv = match wasix_read_exec_string(caller, memory, args, args_len) {
        Ok(value) => wasix_split_lines(&value),
        Err(_) => return p1::errno::FAULT,
    };
    let prepared = match wasix_prepare_program(caller, memory, name, name_len).await {
        Ok(prepared) => prepared,
        Err(error) => return p1_errno_from_wasmtime_error(&error),
    };
    let result = match wasix_spawn_child(
        caller,
        prepared,
        argv,
        None,
        WasixSpawnIo::from_modes(stdin, stdout, stderr),
    )
    .await
    {
        Ok(result) => result,
        Err(errno) => return errno,
    };
    wasix_write_process_handles(caller, memory, ret_handles, result)
}

#[allow(clippy::too_many_arguments)]
async fn wasix_proc_spawn2<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: u32,
    name_len: u32,
    args: u32,
    args_len: u32,
    env: u32,
    env_len: u32,
    fd_ops: u32,
    fd_ops_len: u32,
    signals: u32,
    signals_len: u32,
    search_path: i32,
    path: u32,
    path_len: u32,
    ret_pid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_spawn_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if fd_ops != 0
        || fd_ops_len != 0
        || signals != 0
        || signals_len != 0
        || search_path != 0
        || path != 0
        || path_len != 0
    {
        return p1::errno::NOTSUP;
    }
    let argv = match wasix_read_exec_string(caller, memory, args, args_len) {
        Ok(value) => wasix_split_lines(&value),
        Err(_) => return p1::errno::FAULT,
    };
    let environment = if env == 0 && env_len == 0 {
        None
    } else {
        match wasix_read_exec_string(caller, memory, env, env_len) {
            Ok(value) => Some(wasix_split_environment(&value)),
            Err(_) => return p1::errno::FAULT,
        }
    };
    let prepared = match wasix_prepare_program(caller, memory, name, name_len).await {
        Ok(prepared) => prepared,
        Err(error) => return p1_errno_from_wasmtime_error(&error),
    };
    let result =
        match wasix_spawn_child(caller, prepared, argv, environment, WasixSpawnIo::inherit()).await
        {
            Ok(result) => result,
            Err(errno) => return errno,
        };
    p1_write_u32(caller, memory, ret_pid, result.pid)
}

async fn wasix_proc_join<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    pid: u32,
    flags: u32,
    ret_status: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_join_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if flags & !WASIX_JOIN_FLAGS_SUPPORTED != 0 {
        return p1::errno::INVAL;
    }
    let requested_pid = match wasix_read_option_pid(caller, memory, pid) {
        Ok(pid) => pid,
        Err(errno) => return errno,
    };
    let Some(index) = caller.data().find_child_index(requested_pid) else {
        return wasix_write_join_nothing(caller, memory, ret_status);
    };
    if flags & WASIX_JOIN_FLAG_NON_BLOCKING != 0 {
        match caller.data_mut().poll_child_exit(index) {
            Ok(Some(code)) => {
                let child = caller.data_mut().children.swap_remove(index);
                wasix_write_join_exit(caller, memory, pid, ret_status, child.pid, code)
            }
            Ok(None) => wasix_write_join_nothing(caller, memory, ret_status),
            Err(errno) => errno,
        }
    } else {
        let child_pid = caller.data().children[index].pid;
        loop {
            match caller.data_mut().poll_child_exit(index) {
                Ok(Some(code)) => {
                    let child = caller.data_mut().children.swap_remove(index);
                    return wasix_write_join_exit(caller, memory, pid, ret_status, child.pid, code);
                }
                Ok(None) => crate::yield_now().await,
                Err(errno) => return errno,
            }
            if caller.data().find_child_index(Some(child_pid)).is_none() {
                return wasix_write_join_nothing(caller, memory, ret_status);
            }
        }
    }
}

fn wasix_proc_snapshot<CpuImpl, HostFs>(
    _caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    p1::errno::NOTSUP
}

#[derive(Clone, Copy)]
struct WasixSpawnIo {
    stdin: WasixStdioMode,
    stdout: WasixStdioMode,
    stderr: WasixStdioMode,
}

#[derive(Clone, Copy)]
enum WasixStdioMode {
    Piped,
    Inherit,
    Null,
    Log,
    Invalid,
}

struct WasixSpawnResult {
    pid: u32,
    stdin_fd: Option<u32>,
    stdout_fd: Option<u32>,
    stderr_fd: Option<u32>,
}

impl WasixSpawnIo {
    const fn inherit() -> Self {
        Self {
            stdin: WasixStdioMode::Inherit,
            stdout: WasixStdioMode::Inherit,
            stderr: WasixStdioMode::Inherit,
        }
    }

    const fn from_modes(stdin: i32, stdout: i32, stderr: i32) -> Self {
        Self {
            stdin: WasixStdioMode::from_raw(stdin),
            stdout: WasixStdioMode::from_raw(stdout),
            stderr: WasixStdioMode::from_raw(stderr),
        }
    }
}

impl WasixStdioMode {
    const fn from_raw(value: i32) -> Self {
        match value {
            WASIX_STDIO_MODE_PIPED => Self::Piped,
            WASIX_STDIO_MODE_INHERIT => Self::Inherit,
            WASIX_STDIO_MODE_NULL => Self::Null,
            WASIX_STDIO_MODE_LOG => Self::Log,
            _ => Self::Invalid,
        }
    }
}

async fn wasix_prepare_program<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    name: u32,
    name_len: u32,
) -> wasmtime::Result<WasixPreparedProgram>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let (guest_name, source_path) = wasix_resolve_exec_path(caller, memory, name, name_len)?;
    let host_path = crate::guest_host_share_path(&source_path).map(ToOwned::to_owned);
    let source_is_host = host_path.is_some();
    let source = if let Some(host_path) = host_path {
        let service = caller
            .data()
            .runtime_state
            .host_filesystem_service()
            .ok_or_else(|| {
                wasmtime::Error::new(ProgramExecError {
                    kind: ProgramExecErrorKind::PermissionDenied,
                    detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
                })
            })?;
        service
            .read_file(&host_path)
            .await
            .map(Bytes::from)
            .map_err(|_| {
                wasmtime::Error::new(ProgramExecError {
                    kind: ProgramExecErrorKind::PermissionDenied,
                    detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
                })
            })?
    } else {
        caller
            .data()
            .filesystem
            .read_program_file_bytes(&source_path)
            .map_err(|_| {
                wasmtime::Error::new(ProgramExecError {
                    kind: ProgramExecErrorKind::PermissionDenied,
                    detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
                })
            })?
    };
    let source = if cwasm::is_cwasm(&source) {
        if source_is_host {
            ProgramSource::SignedArtifact(source)
        } else {
            ProgramSource::BootfsArtifact(source)
        }
    } else {
        ProgramSource::RawWasm(source)
    };
    Ok(WasixPreparedProgram {
        guest_name,
        source_path,
        source,
    })
}

async fn wasix_spawn_child<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    prepared: WasixPreparedProgram,
    mut argv: Vec<String>,
    environment: Option<Vec<(String, String)>>,
    io: WasixSpawnIo,
) -> Result<WasixSpawnResult, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if matches!(
        (io.stdin, io.stdout, io.stderr),
        (WasixStdioMode::Invalid, _, _)
            | (_, WasixStdioMode::Invalid, _)
            | (_, _, WasixStdioMode::Invalid)
    ) {
        return Err(p1::errno::INVAL);
    }
    if argv
        .first()
        .is_some_and(|arg| arg == &prepared.guest_name || arg == &prepared.source_path)
    {
        argv.remove(0);
    }
    let mut environment = environment.unwrap_or_else(|| caller.data().environment.clone());
    environment.retain(|(name, _)| name.as_str() != HELIOS_PROCESS_ID_ENV);
    let service = caller
        .data()
        .runtime_state
        .program_service()
        .ok_or(p1::errno::NOTSUP)?;
    let exec_context = caller.data().exec_context();
    let authority = caller.data().authority.clone();
    let mut child = service
        .spawn(
            exec_context,
            prepared.guest_name,
            argv,
            environment,
            prepared.source,
            None,
            authority,
        )
        .await
        .map_err(|error| p1_errno_from_program_exec_error(&error))?;
    let pid = u32::try_from(child.instance_id.raw()).map_err(|_| p1::errno::OVERFLOW)?;
    let stdin_fd = wasix_configure_child_stdin(caller, &mut child, io.stdin)?;
    let stdout_fd = wasix_configure_child_output(
        caller,
        &mut child,
        io.stdout,
        crate::ComponentOutputStreamKind::Stdout,
    )?;
    let stderr_fd = wasix_configure_child_output(
        caller,
        &mut child,
        io.stderr,
        crate::ComponentOutputStreamKind::Stderr,
    )?;
    let exit = child.take_wait().ok_or(p1::errno::IO)?;
    caller.data_mut().insert_child(pid, exit);
    Ok(WasixSpawnResult {
        pid,
        stdin_fd,
        stdout_fd,
        stderr_fd,
    })
}

fn wasix_configure_child_stdin<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    child: &mut ChildHandle,
    mode: WasixStdioMode,
) -> Result<Option<u32>, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match mode {
        WasixStdioMode::Piped => {
            let writer = child.take_stdin().ok_or(p1::errno::IO)?;
            caller
                .data_mut()
                .descriptors
                .insert(Preview1Descriptor::PipeWrite { writer })
                .map(Some)
        }
        WasixStdioMode::Inherit | WasixStdioMode::Null | WasixStdioMode::Log => {
            drop(child.take_stdin());
            Ok(None)
        }
        WasixStdioMode::Invalid => Err(p1::errno::INVAL),
    }
}

fn wasix_configure_child_output<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    child: &mut ChildHandle,
    mode: WasixStdioMode,
    stream: crate::ComponentOutputStreamKind,
) -> Result<Option<u32>, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let reader = match stream {
        crate::ComponentOutputStreamKind::Stdout => child.take_stdout(),
        crate::ComponentOutputStreamKind::Stderr => child.take_stderr(),
    }
    .ok_or(p1::errno::IO)?;
    match mode {
        WasixStdioMode::Piped => caller
            .data_mut()
            .descriptors
            .insert(Preview1Descriptor::PipeRead {
                reader,
                carry: Vec::new(),
            })
            .map(Some),
        WasixStdioMode::Inherit | WasixStdioMode::Log => {
            let sink = caller.data().output_sink();
            wasix_spawn_output_drain(&caller.data().spawner, reader, sink, stream);
            Ok(None)
        }
        WasixStdioMode::Null => {
            wasix_spawn_output_drain(
                &caller.data().spawner,
                reader,
                WasixOutputSink::<CpuImpl, HostFs>::Discard,
                stream,
            );
            Ok(None)
        }
        WasixStdioMode::Invalid => Err(p1::errno::INVAL),
    }
}

fn wasix_spawn_output_drain<CpuImpl, HostFs>(
    spawner: &crate::Spawner<CpuImpl>,
    reader: crate::ByteReader,
    sink: WasixOutputSink<CpuImpl, HostFs>,
    stream: crate::ComponentOutputStreamKind,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    spawner.spawn_detached(async move {
        while let Some(bytes) = reader.read().await {
            sink.write(stream, bytes);
        }
    });
}

fn wasix_write_process_handles<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    ret_handles: u32,
    result: WasixSpawnResult,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    p1_write_u32(caller, memory, ret_handles, result.pid)
        .max(wasix_write_option_fd(
            caller,
            memory,
            ret_handles + WASIX_PROCESS_HANDLES_STDIN_OFFSET,
            result.stdin_fd,
        ))
        .max(wasix_write_option_fd(
            caller,
            memory,
            ret_handles + WASIX_PROCESS_HANDLES_STDOUT_OFFSET,
            result.stdout_fd,
        ))
        .max(wasix_write_option_fd(
            caller,
            memory,
            ret_handles + WASIX_PROCESS_HANDLES_STDERR_OFFSET,
            result.stderr_fd,
        ))
}

fn wasix_write_option_fd<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    fd: Option<u32>,
) -> i32 {
    let status = p1_write_u8(
        caller,
        memory,
        ptr,
        if fd.is_some() {
            WASIX_OPTION_SOME
        } else {
            WASIX_OPTION_NONE
        },
    );
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_u32(
        caller,
        memory,
        ptr + WASIX_OPTION_UNION_U32_OFFSET,
        fd.unwrap_or(0),
    )
}

fn wasix_write_join_nothing<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    ret_status: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    p1_write_u8(caller, memory, ret_status, WASIX_JOIN_STATUS_NOTHING).max(p1_write_u16(
        caller,
        memory,
        ret_status + WASIX_JOIN_STATUS_UNION_OFFSET,
        0,
    ))
}

fn wasix_write_join_exit<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    pid_ptr: u32,
    ret_status: u32,
    pid: u32,
    code: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let errno = u16::try_from(code).unwrap_or(u16::MAX);
    p1_write_u8(caller, memory, pid_ptr, WASIX_OPTION_SOME)
        .max(p1_write_u32(
            caller,
            memory,
            pid_ptr + WASIX_OPTION_UNION_U32_OFFSET,
            pid,
        ))
        .max(p1_write_u8(
            caller,
            memory,
            ret_status,
            WASIX_JOIN_STATUS_EXIT_NORMAL,
        ))
        .max(p1_write_u16(
            caller,
            memory,
            ret_status + WASIX_JOIN_STATUS_UNION_OFFSET,
            errno,
        ))
}

fn p1_errno_from_wasmtime_error(error: &wasmtime::Error) -> i32 {
    error
        .downcast_ref::<ProgramExecError>()
        .map_or(p1::errno::IO, p1_errno_from_program_exec_error)
}

fn p1_errno_from_program_exec_error(error: &ProgramExecError) -> i32 {
    match error.kind {
        ProgramExecErrorKind::InvalidBinary
        | ProgramExecErrorKind::MissingEntry
        | ProgramExecErrorKind::UnsupportedImport
        | ProgramExecErrorKind::InvalidSignature
        | ProgramExecErrorKind::InvalidHint => p1::errno::NOENT,
        ProgramExecErrorKind::InvalidPath => p1::errno::INVAL,
        ProgramExecErrorKind::PermissionDenied => p1::errno::NOTCAPABLE,
        ProgramExecErrorKind::OutOfMemory => p1::errno::NOMEM,
        ProgramExecErrorKind::Unavailable => p1::errno::NOTSUP,
        ProgramExecErrorKind::Internal => p1::errno::IO,
    }
}

fn wasix_resolve_exec_path<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    memory: Preview1Memory,
    name: u32,
    name_len: u32,
) -> wasmtime::Result<(String, String)>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let name = p1_read_path(caller, memory, name, name_len).map_err(wasmtime::Error::new)?;
    let guest_name = if name.starts_with('/') {
        crate::resolve_absolute_path(&name).map_err(|_| {
            wasmtime::Error::new(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidPath,
                detail: ProgramExecErrorDetail::InvalidProgramPath,
            })
        })?
    } else {
        let Some(cwd) = caller.data().cwd.as_ref() else {
            return Err(wasmtime::Error::new(ProgramExecError {
                kind: ProgramExecErrorKind::PermissionDenied,
                detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
            }));
        };
        crate::resolve_child_path(&cwd.guest_name, &name).map_err(|_| {
            wasmtime::Error::new(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidPath,
                detail: ProgramExecErrorDetail::InvalidProgramPath,
            })
        })?
    };
    let (path, _) = caller
        .data()
        .resolve_absolute_guest_path(&guest_name)
        .map_err(|_| {
            wasmtime::Error::new(ProgramExecError {
                kind: ProgramExecErrorKind::PermissionDenied,
                detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
            })
        })?;
    if !caller.data().authority.can_load_program(&path) {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::PermissionDenied,
            detail: ProgramExecErrorDetail::ProgramSourceNotGranted,
        }));
    }
    Ok((guest_name, path))
}

fn wasix_read_exec_string<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    len: u32,
) -> Result<String, ProgramExecError> {
    if len == 0 {
        return Ok(String::new());
    }
    p1_read_path(caller, memory, ptr, len)
}

fn wasix_split_lines(value: &str) -> Vec<String> {
    value
        .split('\n')
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn wasix_split_environment(value: &str) -> Vec<(String, String)> {
    value
        .split('\n')
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            entry
                .split_once('=')
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .unwrap_or_else(|| (entry.to_owned(), String::new()))
        })
        .collect()
}

async fn wasix_thread_sleep<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    duration: i64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Ok(duration) = u64::try_from(duration) else {
        return p1::errno::INVAL;
    };
    caller
        .data()
        .timer
        .sleep_for(Duration::from_nanos(duration))
        .await;
    p1::errno::SUCCESS
}

fn wasix_thread_id<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_tid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u32(caller, memory, ret_tid, 0)
}

fn wasix_thread_spawn_v2<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_tid: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let status = p1_write_u32(caller, memory, ret_tid, 0);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1::errno::NOTSUP
}

fn wasix_thread_join(tid: u32) -> i32 {
    if tid == 0 {
        p1::errno::INVAL
    } else {
        p1::errno::SRCH
    }
}

fn wasix_thread_parallelism<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_parallelism: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let parallelism = match u32::try_from(caller.data().cpu.processor_count()) {
        Ok(parallelism) => parallelism,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, ret_parallelism, parallelism)
}

fn wasix_thread_signal<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    tid: i32,
    signal: i32,
) -> wasmtime::Result<i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if tid != 0 {
        return Ok(p1::errno::SRCH);
    }
    let Ok(signal) = u32::try_from(signal) else {
        return Ok(p1::errno::INVAL);
    };
    caller
        .data_mut()
        .request_exit(128u32.saturating_add(signal));
    Err(wasmtime::Error::new(Preview1Exit))
}

fn wasix_thread_exit<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    code: u32,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    caller.data_mut().request_exit(code);
    Err(wasmtime::Error::new(Preview1Exit))
}

fn wasix_stack_bounds_from_caller<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Result<(u32, u32, u32), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let stack_lower = wasix_global_u32_from_caller(caller, "__data_end")?;
    let stack_upper = wasix_global_u32_from_caller(caller, "__heap_base")?;
    let stack_pointer = wasix_global_u32_from_caller(caller, "__stack_pointer")?;
    Ok((stack_lower, stack_upper, stack_pointer))
}

fn wasix_global_u32_from_caller<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    name: &str,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let global = caller
        .get_export(name)
        .and_then(|export| export.into_global())
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::WasixAsyncifyExportInvalid,
        })?;
    match global.get(&mut *caller) {
        Val::I32(value) => Ok(value as u32),
        _ => Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryTypeMismatch,
        }),
    }
}

async fn wasix_call_asyncify_start_unwind<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    data: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(function) = caller
        .get_export("asyncify_start_unwind")
        .and_then(|export| export.into_func())
    else {
        return p1::errno::NOTSUP;
    };
    let Ok(function) = function.typed::<i32, ()>(&mut *caller) else {
        return p1::errno::NOTSUP;
    };
    function
        .call_async(&mut *caller, data as i32)
        .await
        .map_or(p1::errno::NOTSUP, |_| p1::errno::SUCCESS)
}

async fn wasix_call_asyncify_stop_rewind<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(function) = caller
        .get_export("asyncify_stop_rewind")
        .and_then(|export| export.into_func())
    else {
        return p1::errno::NOTSUP;
    };
    let Ok(function) = function.typed::<(), ()>(&mut *caller) else {
        return p1::errno::NOTSUP;
    };
    function
        .call_async(&mut *caller, ())
        .await
        .map_or(p1::errno::NOTSUP, |_| p1::errno::SUCCESS)
}

async fn wasix_stack_checkpoint<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    snapshot: u32,
    ret_value: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Some(value) = caller.data_mut().asyncify.rewind_value.take() {
        if wasix_call_asyncify_stop_rewind(caller).await != p1::errno::SUCCESS {
            return p1::errno::NOTSUP;
        }
        let Some(memory) = p1_memory(caller) else {
            return p1::errno::FAULT;
        };
        return p1_write_u64(caller, memory, ret_value, value);
    }

    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let Ok((stack_lower, stack_upper, stack_pointer)) = wasix_stack_bounds_from_caller(caller)
    else {
        return p1::errno::NOTSUP;
    };
    if stack_lower >= stack_pointer || stack_pointer > stack_upper {
        return p1::errno::INVAL;
    }
    let memory_stack_len = match usize::try_from(stack_upper - stack_pointer) {
        Ok(len) => len,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let memory_stack = match p1_read_memory(caller, memory, stack_pointer, memory_stack_len) {
        Ok(stack) => stack,
        Err(_) => return p1::errno::FAULT,
    };
    let unwind_stack_begin = match stack_lower.checked_add(WASIX_ASYNCIFY_DATA_SIZE) {
        Some(begin) if begin <= stack_pointer => begin,
        _ => return p1::errno::OVERFLOW,
    };
    let status = p1_write_u32(caller, memory, stack_lower, unwind_stack_begin)
        .max(p1_write_u32(caller, memory, stack_lower + 4, stack_pointer))
        .max(p1_write_u64(caller, memory, ret_value, 0));
    if status != p1::errno::SUCCESS {
        return status;
    }
    caller.data_mut().asyncify.phase = WasixAsyncifyPhase::Capturing {
        snapshot,
        ret_value,
        stack_lower,
        stack_upper,
        unwind_stack_begin,
        memory_stack,
        stack_pointer,
    };
    wasix_call_asyncify_start_unwind(caller, stack_lower).await
}

async fn wasix_stack_restore<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    snapshot: u32,
    value: u64,
) -> wasmtime::Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        }));
    };
    let hash_lower = p1_try_read_u64(caller, memory, snapshot + 8).map_err(wasmtime::Error::new)?;
    let hash_upper =
        p1_try_read_u64(caller, memory, snapshot + 16).map_err(wasmtime::Error::new)?;
    let Ok((stack_lower, stack_upper, stack_pointer)) = wasix_stack_bounds_from_caller(caller)
    else {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
        }));
    };
    if stack_lower >= stack_pointer || stack_pointer > stack_upper {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
        }));
    }
    let unwind_stack_begin = stack_lower
        .checked_add(WASIX_ASYNCIFY_DATA_SIZE)
        .ok_or_else(|| {
            wasmtime::Error::new(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::WasixStackBoundsInvalid,
            })
        })?;
    let status = p1_write_u32(caller, memory, stack_lower, unwind_stack_begin).max(p1_write_u32(
        caller,
        memory,
        stack_lower + 4,
        stack_pointer,
    ));
    if status != p1::errno::SUCCESS {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        }));
    }
    caller.data_mut().asyncify.phase = WasixAsyncifyPhase::Restoring {
        hash: (u128::from(hash_upper) << 64) | u128::from(hash_lower),
        value: if value == 0 { 1 } else { value },
        stack_lower,
    };
    if wasix_call_asyncify_start_unwind(caller, stack_lower).await != p1::errno::SUCCESS {
        return Err(wasmtime::Error::new(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::WasixAsyncifyExportInvalid,
        }));
    }
    Ok(())
}

async fn wasix_futex_wait<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    futex: u32,
    expected: u32,
    timeout: u32,
    ret_woken: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let timeout = match wasix_read_optional_timestamp(caller, memory, timeout) {
        Ok(timeout) => timeout,
        Err(errno) => return errno,
    };
    let key = caller.data().futex_key(futex);
    let registration = caller.data().runtime_state.prepare_futex_wait(key);
    let current = match p1_try_read_u32(caller, memory, futex) {
        Ok(value) => value,
        Err(_) => {
            caller
                .data()
                .runtime_state
                .complete_futex_wait(registration);
            return p1::errno::FAULT;
        }
    };
    if current != expected {
        caller
            .data()
            .runtime_state
            .complete_futex_wait(registration);
        return p1::errno::INVAL;
    }

    let notify = registration.notify();
    let woken = match timeout {
        Some(0) => false,
        Some(timeout_nanos) => {
            let wake = notify.notified();
            let sleep = caller
                .data()
                .timer
                .sleep_for(Duration::from_nanos(timeout_nanos));
            futures::pin_mut!(wake);
            futures::pin_mut!(sleep);
            matches!(
                futures::future::select(wake, sleep).await,
                futures::future::Either::Left(_)
            )
        }
        None => {
            notify.notified().await;
            true
        }
    };
    caller
        .data()
        .runtime_state
        .complete_futex_wait(registration);
    p1_write_wasix_bool(caller, memory, ret_woken, woken)
}

fn wasix_futex_wake<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    futex: u32,
    ret_woken: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if p1_try_read_u32(caller, memory, futex).is_err() {
        return p1::errno::FAULT;
    }
    let key = caller.data().futex_key(futex);
    let woken = caller.data().runtime_state.wake_futex(key, 1) != 0;
    p1_write_wasix_bool(caller, memory, ret_woken, woken)
}

fn wasix_futex_wake_all<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    futex: u32,
    ret_woken: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    if p1_try_read_u32(caller, memory, futex).is_err() {
        return p1::errno::FAULT;
    }
    let key = caller.data().futex_key(futex);
    let woken = caller.data().runtime_state.wake_all_futex(key) != 0;
    p1_write_wasix_bool(caller, memory, ret_woken, woken)
}

async fn wasix_resolve<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    host: u32,
    host_len: u32,
    port: i32,
    addrs: u32,
    naddrs: u32,
    ret_naddrs: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_dns_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    if u16::try_from(port).is_err() {
        return p1::errno::INVAL;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let host = match p1_read_path(caller, memory, host, host_len) {
        Ok(host) => host,
        Err(_) => return p1::errno::FAULT,
    };
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let resolved = match service.dns_resolve(&host, u64::MAX).await {
        Ok(resolved) => resolved,
        Err(error) => return p1_errno_from_dns_error(error),
    };
    let returned = resolved.len().min(naddrs as usize);
    for (index, address) in resolved.iter().take(returned).enumerate() {
        let offset = match (index as u32).checked_mul(WASIX_ADDR_IP_SIZE) {
            Some(offset) => offset,
            None => return p1::errno::OVERFLOW,
        };
        let entry = match addrs.checked_add(offset) {
            Some(entry) => entry,
            None => return p1::errno::OVERFLOW,
        };
        let status = write_wasix_addr_ip4(caller, memory, entry, *address);
        if status != p1::errno::SUCCESS {
            return status;
        }
    }
    let returned = match u32::try_from(returned) {
        Ok(returned) => returned,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, ret_naddrs, returned)
}

fn wasix_network_admin_unavailable<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_network_admin_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1::errno::NETDOWN
}

fn wasix_port_mac<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    _ret_mac: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    wasix_network_admin_unavailable(caller)
}

fn wasix_port_empty_list<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_len: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_network_admin_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let status = p1_write_u32(caller, memory, ret_len, 0);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1::errno::NETDOWN
}

fn wasix_sock_status<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_status: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let status = wasix_sock_descriptor_unavailable(caller, fd);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_u8(caller, memory, ret_status, WASIX_SOCK_STATUS_OPENED)
}

fn wasix_sock_addr_local<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
            local_port,
            ..
        }))) => write_wasix_addr_port_ip4(
            caller,
            memory,
            ret_addr,
            crate::Ipv4Address::new([0, 0, 0, 0]),
            *local_port,
        ),
        Some(Preview1Descriptor::Socket(_)) => {
            write_wasix_addr_port_unspec(caller, memory, ret_addr)
        }
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

fn wasix_sock_addr_peer<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected {
                peer_address,
                peer_port,
                ..
            },
        ))) => write_wasix_addr_port_ip4(caller, memory, ret_addr, *peer_address, *peer_port),
        Some(Preview1Descriptor::Socket(_)) => {
            write_wasix_addr_port_unspec(caller, memory, ret_addr)
        }
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

fn wasix_sock_open<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    socktype: i32,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = match socktype {
        WASIX_SOCK_TYPE_STREAM => caller.data().require_tcp_authority(),
        WASIX_SOCK_TYPE_DGRAM => caller.data().require_udp_authority(),
        _ => return p1::errno::INVAL,
    };
    if status != p1::errno::SUCCESS {
        return status;
    }
    if caller.data().runtime_state.network_service().is_none() {
        return p1::errno::NETDOWN;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let descriptor = match socktype {
        WASIX_SOCK_TYPE_STREAM => {
            Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Unconnected))
        }
        WASIX_SOCK_TYPE_DGRAM => {
            Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound))
        }
        _ => return p1::errno::INVAL,
    };
    let fd = match caller.data_mut().descriptors.insert(descriptor) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    p1_write_u32(caller, memory, ret_fd, fd)
}

fn wasix_sock_pair<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    socktype: i32,
    ret_fd0: u32,
    ret_fd1: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = match socktype {
        WASIX_SOCK_TYPE_STREAM => caller.data().require_tcp_authority(),
        WASIX_SOCK_TYPE_DGRAM => caller.data().require_udp_authority(),
        _ => return p1::errno::INVAL,
    };
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let (left_writer, right_reader) = crate::byte_channel();
    let (right_writer, left_reader) = crate::byte_channel();
    let first = Preview1Descriptor::Socket(WasixSocketDescriptor::Pair {
        reader: left_reader,
        writer: left_writer,
        carry: Vec::new(),
    });
    let second = Preview1Descriptor::Socket(WasixSocketDescriptor::Pair {
        reader: right_reader,
        writer: right_writer,
        carry: Vec::new(),
    });
    let fd0 = match caller.data_mut().descriptors.insert(first) {
        Ok(fd) => fd,
        Err(errno) => return errno,
    };
    let fd1 = match caller.data_mut().descriptors.insert(second) {
        Ok(fd) => fd,
        Err(errno) => {
            let _ = caller.data_mut().descriptors.close(fd0 as i32);
            return errno;
        }
    };
    let status = p1_write_u32(caller, memory, ret_fd0, fd0);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_u32(caller, memory, ret_fd1, fd1)
}

fn wasix_sock_descriptor_unavailable<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(_)) => p1::errno::SUCCESS,
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

fn wasix_sock_recv_authority(
    descriptor: Option<&Preview1Descriptor>,
) -> Result<WasixSocketAuthority, i32> {
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
            ..
        }))) => Ok(WasixSocketAuthority::Udp),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound))) => {
            Err(p1::errno::INVAL)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { .. },
        ))) => Ok(WasixSocketAuthority::Tcp),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected,
        ))) => Err(p1::errno::INVAL),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { .. })) => {
            Ok(WasixSocketAuthority::LocalOnly)
        }
        Some(_) => Err(p1::errno::NOTSOCK),
        None => Err(p1::errno::BADF),
    }
}

fn wasix_sock_send_authority(
    descriptor: Option<&Preview1Descriptor>,
) -> Result<WasixSocketAuthority, i32> {
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(_))) => {
            Ok(WasixSocketAuthority::Udp)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { .. },
        ))) => Ok(WasixSocketAuthority::Tcp),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected,
        ))) => Err(p1::errno::INVAL),
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { .. })) => {
            Ok(WasixSocketAuthority::LocalOnly)
        }
        Some(_) => Err(p1::errno::NOTSOCK),
        None => Err(p1::errno::BADF),
    }
}

fn wasix_sock_bind_authority(
    descriptor: Option<&Preview1Descriptor>,
) -> Result<WasixSocketAuthority, i32> {
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound))) => {
            Ok(WasixSocketAuthority::Udp)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
            ..
        }))) => Err(p1::errno::INVAL),
        Some(Preview1Descriptor::Socket(_)) => Err(p1::errno::INVAL),
        Some(_) => Err(p1::errno::NOTSOCK),
        None => Err(p1::errno::BADF),
    }
}

fn wasix_sock_get_opt_flag<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_flag: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = wasix_sock_descriptor_unavailable(caller, fd);
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_wasix_bool(caller, memory, ret_flag, false)
}

fn wasix_sock_get_opt_time<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_time: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = wasix_sock_descriptor_unavailable(caller, fd);
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u8(caller, memory, ret_time, WASIX_OPTION_NONE)
}

fn wasix_sock_get_opt_size<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ret_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = wasix_sock_descriptor_unavailable(caller, fd);
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    p1_write_u64(caller, memory, ret_size, 0)
}

fn wasix_sock_multicast<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = caller.data().require_multicast_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    wasix_sock_descriptor_unavailable(caller, fd)
}

async fn wasix_sock_bind<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let (_, port) = match wasix_read_addr_port(caller, memory, addr) {
        Ok(addr) => addr,
        Err(errno) => return errno,
    };
    let descriptor = caller.data().descriptors.get(fd).cloned();
    let authority = match wasix_sock_bind_authority(descriptor.as_ref()) {
        Ok(authority) => authority,
        Err(errno) => return errno,
    };
    let status = caller.data().require_socket_authority(authority);
    if status != p1::errno::SUCCESS {
        return status;
    }
    if port < 1024 {
        let status = caller.data().require_privileged_bind_authority();
        if status != p1::errno::SUCCESS {
            return status;
        }
    }
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let binding = match service.udp_bind(port).await {
        Ok(binding) => binding,
        Err(error) => return p1_errno_from_udp_error(error),
    };
    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(slot))) =
        caller.data_mut().descriptors.get_mut(fd)
    else {
        return p1::errno::BADF;
    };
    *slot = WasixUdpSocket::Bound {
        socket: binding.socket,
        local_port: binding.local_port,
    };
    p1::errno::SUCCESS
}

fn wasix_sock_accept_v2<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    _ret_fd: u32,
    _ret_addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    wasix_sock_descriptor_unavailable(caller, fd)
}

async fn wasix_sock_connect<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let (Some(address), port) = (match wasix_read_addr_port(caller, memory, addr) {
        Ok(addr) => addr,
        Err(errno) => return errno,
    }) else {
        return p1::errno::INVAL;
    };
    let status = caller.data().require_tcp_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected,
        ))) => {}
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { .. },
        ))) => return p1::errno::INVAL,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(_))) => {
            return p1::errno::NOTSOCK;
        }
        Some(_) => return p1::errno::NOTSOCK,
        None => return p1::errno::BADF,
    }
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let mut host_buffer = [0; 15];
    let host = address.write_dotted_decimal(&mut host_buffer);
    let stream = match service.tcp_connect(host, port, u64::MAX).await {
        Ok(stream) => stream,
        Err(error) => return p1_errno_from_tcp_error(error),
    };
    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(slot))) =
        caller.data_mut().descriptors.get_mut(fd)
    else {
        return p1::errno::BADF;
    };
    *slot = WasixTcpSocket::Connected {
        stream,
        peer_address: address,
        peer_port: port,
    };
    p1::errno::SUCCESS
}

async fn wasix_sock_recv_from<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    flags: u16,
    ret_size: u32,
    ret_flags: u32,
    ret_addr: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let iovs = match p1_read_iovs(caller, memory, iovs, iovs_len) {
        Ok(iovs) => iovs,
        Err(errno) => return errno,
    };
    let capacity = iovs
        .iter()
        .try_fold(0u32, |sum, (_, len)| sum.checked_add(*len));
    let Some(capacity) = capacity else {
        return p1::errno::OVERFLOW;
    };
    let descriptor = caller.data().descriptors.get(fd).cloned();
    let authority = match wasix_sock_recv_authority(descriptor.as_ref()) {
        Ok(authority) => authority,
        Err(errno) => return errno,
    };
    let status = caller.data().require_socket_authority(authority);
    if status != p1::errno::SUCCESS {
        return status;
    }
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
            socket,
            ..
        }))) => {
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            let datagram = match service.udp_receive(socket, capacity, u64::MAX).await {
                Ok(Some(datagram)) => datagram,
                Ok(None) => return p1::errno::AGAIN,
                Err(error) => return p1_errno_from_udp_error(error),
            };
            let status = p1_write_iovs_from_bytes(caller, memory, iovs, &datagram.bytes, ret_size);
            if status != p1::errno::SUCCESS {
                return status;
            }
            let status = p1_write_u16(
                caller,
                memory,
                ret_flags,
                flags & WASIX_RIFLAGS_DATA_TRUNCATED,
            );
            if status != p1::errno::SUCCESS {
                return status;
            }
            write_wasix_addr_port_ip4(caller, memory, ret_addr, datagram.address, datagram.port)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound))) => {
            p1::errno::INVAL
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { .. })) => {
            let bytes = match caller
                .data_mut()
                .read_socket_pair(fd, capacity as usize)
                .await
            {
                Ok(bytes) => bytes,
                Err(errno) => return errno,
            };
            let status = p1_write_iovs_from_bytes(caller, memory, iovs, &bytes, ret_size);
            if status != p1::errno::SUCCESS {
                return status;
            }
            let status = p1_write_u16(caller, memory, ret_flags, 0);
            if status != p1::errno::SUCCESS {
                return status;
            }
            write_wasix_addr_port_unspec(caller, memory, ret_addr)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { stream, .. },
        ))) => {
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            let bytes = match service.tcp_read(stream, capacity, u64::MAX).await {
                Ok(Some(bytes)) => bytes,
                Ok(None) => Vec::new(),
                Err(error) => return p1_errno_from_tcp_error(error),
            };
            let status = p1_write_iovs_from_bytes(caller, memory, iovs, &bytes, ret_size);
            if status != p1::errno::SUCCESS {
                return status;
            }
            p1_write_u16(caller, memory, ret_flags, 0)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected,
        ))) => p1::errno::INVAL,
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

async fn wasix_sock_send_to<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    _flags: u16,
    addr: u32,
    ret_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let bytes = match p1_read_iovs_to_bytes(caller, memory, iovs, iovs_len) {
        Ok(bytes) => bytes,
        Err(errno) => return errno,
    };
    let descriptor = caller.data().descriptors.get(fd).cloned();
    let authority = match wasix_sock_send_authority(descriptor.as_ref()) {
        Ok(authority) => authority,
        Err(errno) => return errno,
    };
    let status = caller.data().require_socket_authority(authority);
    if status != p1::errno::SUCCESS {
        return status;
    }
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(socket))) => {
            let (Some(address), port) = (match wasix_read_addr_port(caller, memory, addr) {
                Ok(addr) => addr,
                Err(errno) => return errno,
            }) else {
                return p1::errno::INVAL;
            };
            let socket = match socket {
                WasixUdpSocket::Bound { socket, .. } => socket,
                WasixUdpSocket::Unbound => {
                    let Some(service) = caller.data().runtime_state.network_service() else {
                        return p1::errno::NETDOWN;
                    };
                    let binding = match service.udp_bind(0).await {
                        Ok(binding) => binding,
                        Err(error) => return p1_errno_from_udp_error(error),
                    };
                    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(slot))) =
                        caller.data_mut().descriptors.get_mut(fd)
                    else {
                        return p1::errno::BADF;
                    };
                    *slot = WasixUdpSocket::Bound {
                        socket: binding.socket,
                        local_port: binding.local_port,
                    };
                    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(
                        WasixUdpSocket::Bound { socket, .. },
                    ))) = caller.data().descriptors.get(fd)
                    else {
                        return p1::errno::BADF;
                    };
                    *socket
                }
            };
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            let mut host_buffer = [0; 15];
            let host = address.write_dotted_decimal(&mut host_buffer);
            let sent = match service.udp_send(socket, host, port, &bytes, u64::MAX).await {
                Ok(sent) => sent,
                Err(error) => return p1_errno_from_udp_error(error),
            };
            let sent = match u32::try_from(sent) {
                Ok(sent) => sent,
                Err(_) => return p1::errno::OVERFLOW,
            };
            p1_write_u32(caller, memory, ret_size, sent)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { stream, .. },
        ))) => {
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            if let Err(error) = service.tcp_write_all(stream, &bytes, u64::MAX).await {
                return p1_errno_from_tcp_error(error);
            }
            let written = match u32::try_from(bytes.len()) {
                Ok(written) => written,
                Err(_) => return p1::errno::OVERFLOW,
            };
            p1_write_u32(caller, memory, ret_size, written)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected,
        ))) => p1::errno::INVAL,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { writer, .. })) => {
            let written = match u32::try_from(bytes.len()) {
                Ok(written) => written,
                Err(_) => return p1::errno::OVERFLOW,
            };
            if writer.write(bytes).is_err() {
                return p1::errno::IO;
            }
            p1_write_u32(caller, memory, ret_size, written)
        }
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

async fn wasix_sock_send_file<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    out_fd: i32,
    in_fd: i32,
    offset: i64,
    count: i64,
    ret_size: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let file = match caller.data().descriptors.get(in_fd) {
        Some(Preview1Descriptor::File { descriptor, .. }) => descriptor.clone(),
        Some(_) => return p1::errno::BADF,
        None => return p1::errno::BADF,
    };
    let offset = match u64::try_from(offset) {
        Ok(offset) => offset,
        Err(_) => return p1::errno::INVAL,
    };
    let count = match u64::try_from(count) {
        Ok(count) => count,
        Err(_) => return p1::errno::INVAL,
    };
    let count = match usize::try_from(count) {
        Ok(count) => count,
        Err(_) => return p1::errno::OVERFLOW,
    };
    let bytes = match caller
        .data()
        .filesystem
        .read_file_chunk(&file, offset, count)
        .map_err(p1_errno_from_fs)
    {
        Ok(bytes) => bytes,
        Err(errno) => return errno,
    };
    let written = match u64::try_from(bytes.len()) {
        Ok(written) => written,
        Err(_) => return p1::errno::OVERFLOW,
    };
    match caller.data().descriptors.get(out_fd).cloned() {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { stream, .. },
        ))) => {
            let status = caller
                .data()
                .require_socket_authority(WasixSocketAuthority::Tcp);
            if status != p1::errno::SUCCESS {
                return status;
            }
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            if let Err(error) = service.tcp_write_all(stream, &bytes, u64::MAX).await {
                return p1_errno_from_tcp_error(error);
            }
            p1_write_u64(caller, memory, ret_size, written)
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { writer, .. })) => {
            if writer.write(bytes).is_err() {
                return p1::errno::IO;
            }
            p1_write_u64(caller, memory, ret_size, written)
        }
        Some(Preview1Descriptor::Socket(_)) => p1::errno::INVAL,
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

fn wasix_epoll_create<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    ret_fd: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let status = p1_write_u32(caller, memory, ret_fd, 0);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1::errno::NOTSUP
}

fn wasix_epoll_ctl<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    epfd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if caller.data().descriptors.get(epfd).is_some() {
        p1::errno::INVAL
    } else {
        p1::errno::BADF
    }
}

async fn wasix_epoll_wait<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    epfd: i32,
    timeout: i64,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let status = wasix_epoll_ctl(caller, epfd);
    if status != p1::errno::SUCCESS {
        return status;
    }
    if timeout > 0 {
        caller
            .data()
            .timer
            .sleep_for(Duration::from_nanos(timeout as u64))
            .await;
    }
    p1::errno::NOTSUP
}

fn wasix_read_addr_port<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<(Option<crate::Ipv4Address>, u16), i32> {
    let tag = p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?;
    match tag {
        WASIX_ADDRESS_FAMILY_UNSPEC => Ok((None, 0)),
        WASIX_ADDRESS_FAMILY_IP_INET4 => {
            let port = p1_try_read_u16(caller, memory, ptr + WASIX_ADDR_PORT_UNION_OFFSET)
                .map_err(|_| p1::errno::FAULT)?;
            let octets =
                p1_read_memory(caller, memory, ptr + WASIX_ADDR_PORT_IP4_ADDRESS_OFFSET, 4)
                    .map_err(|_| p1::errno::FAULT)?;
            Ok((
                Some(crate::Ipv4Address::new([
                    octets[0], octets[1], octets[2], octets[3],
                ])),
                port,
            ))
        }
        WASIX_ADDRESS_FAMILY_IP_INET6 => Err(p1::errno::NOTSUP),
        WASIX_ADDRESS_FAMILY_UNIX => Err(p1::errno::NOTSUP),
        _ => Err(p1::errno::INVAL),
    }
}

fn wasix_read_option_pid<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<Option<u32>, i32> {
    let tag = p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?;
    match tag {
        WASIX_OPTION_NONE => Ok(None),
        WASIX_OPTION_SOME => p1_try_read_u32(caller, memory, ptr + WASIX_OPTION_UNION_U32_OFFSET)
            .map(Some)
            .map_err(|_| p1::errno::FAULT),
        _ => Err(p1::errno::INVAL),
    }
}

fn wasix_read_optional_timestamp<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<Option<u64>, i32> {
    if ptr == 0 {
        return Ok(None);
    }
    let tag = p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?;
    match tag {
        0 => Ok(None),
        1 => {
            let value = ptr.checked_add(8).ok_or(p1::errno::OVERFLOW)?;
            p1_try_read_u64(caller, memory, value)
                .map(Some)
                .map_err(|_| p1::errno::FAULT)
        }
        _ => Err(p1::errno::INVAL),
    }
}

fn p1_write_wasix_bool<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    value: bool,
) -> i32 {
    p1_write_u8(caller, memory, ptr, u8::from(value))
}

fn p1_read_wasix_bool<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<bool, i32> {
    match p1_try_read_u8(caller, memory, ptr).map_err(|_| p1::errno::FAULT)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(p1::errno::INVAL),
    }
}

fn read_wasix_tty_state<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<WasixTtyState, i32> {
    Ok(WasixTtyState {
        cols: p1_try_read_u32(caller, memory, ptr).map_err(|_| p1::errno::FAULT)?,
        rows: p1_try_read_u32(caller, memory, ptr + 4).map_err(|_| p1::errno::FAULT)?,
        width: p1_try_read_u32(caller, memory, ptr + 8).map_err(|_| p1::errno::FAULT)?,
        height: p1_try_read_u32(caller, memory, ptr + 12).map_err(|_| p1::errno::FAULT)?,
        stdin_tty: p1_read_wasix_bool(caller, memory, ptr + 16)?,
        stdout_tty: p1_read_wasix_bool(caller, memory, ptr + 17)?,
        stderr_tty: p1_read_wasix_bool(caller, memory, ptr + 18)?,
        echo: p1_read_wasix_bool(caller, memory, ptr + 19)?,
        line_buffered: p1_read_wasix_bool(caller, memory, ptr + 20)?,
        line_feeds: p1_read_wasix_bool(caller, memory, ptr + 21)?,
    })
}

fn write_wasix_tty_state<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    state: WasixTtyState,
) -> i32 {
    p1_write_u32(caller, memory, ptr, state.cols)
        .max(p1_write_u32(caller, memory, ptr + 4, state.rows))
        .max(p1_write_u32(caller, memory, ptr + 8, state.width))
        .max(p1_write_u32(caller, memory, ptr + 12, state.height))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr + 16,
            state.stdin_tty,
        ))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr + 17,
            state.stdout_tty,
        ))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr + 18,
            state.stderr_tty,
        ))
        .max(p1_write_wasix_bool(caller, memory, ptr + 19, state.echo))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr + 20,
            state.line_buffered,
        ))
        .max(p1_write_wasix_bool(
            caller,
            memory,
            ptr + 21,
            state.line_feeds,
        ))
}

fn write_wasix_addr_ip4<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    address: crate::Ipv4Address,
) -> i32 {
    let octets = address.octets();
    p1_write_u8(caller, memory, ptr, WASIX_ADDRESS_FAMILY_IP_INET4).max(p1_write_memory(
        caller,
        memory,
        ptr + WASIX_ADDR_IP_UNION_OFFSET,
        &octets,
    ))
}

fn write_wasix_addr_port_ip4<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    address: crate::Ipv4Address,
    port: u16,
) -> i32 {
    let octets = address.octets();
    p1_write_u8(caller, memory, ptr, WASIX_ADDRESS_FAMILY_IP_INET4)
        .max(p1_write_u16(
            caller,
            memory,
            ptr + WASIX_ADDR_PORT_UNION_OFFSET,
            port,
        ))
        .max(p1_write_memory(
            caller,
            memory,
            ptr + WASIX_ADDR_PORT_IP4_ADDRESS_OFFSET,
            &octets,
        ))
}

fn write_wasix_addr_port_unspec<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> i32 {
    p1_write_u8(caller, memory, ptr, WASIX_ADDRESS_FAMILY_UNSPEC)
        .max(p1_write_u16(
            caller,
            memory,
            ptr + WASIX_ADDR_PORT_UNION_OFFSET,
            0,
        ))
        .max(p1_write_memory(
            caller,
            memory,
            ptr + WASIX_ADDR_PORT_IP4_ADDRESS_OFFSET,
            &[0, 0, 0, 0],
        ))
}

fn p1_sock_accept<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(_)) => p1::errno::NOTSUP,
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

fn p1_connected_tcp_stream<CpuImpl, HostFs>(
    caller: &Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
) -> Result<u64, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { stream, .. },
        ))) => Ok(*stream),
        Some(Preview1Descriptor::Socket(_)) => Err(p1::errno::INVAL),
        Some(_) => Err(p1::errno::NOTSOCK),
        None => Err(p1::errno::BADF),
    }
}

async fn p1_sock_recv<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    ri_data: u32,
    ri_data_len: u32,
    _ri_flags: u16,
    ro_datalen: u32,
    ro_flags: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let iovs = match p1_read_iovs(caller, memory, ri_data, ri_data_len) {
        Ok(iovs) => iovs,
        Err(errno) => return errno,
    };
    let capacity = iovs
        .iter()
        .try_fold(0u32, |sum, (_, len)| sum.checked_add(*len));
    let Some(capacity) = capacity else {
        return p1::errno::OVERFLOW;
    };
    let descriptor = caller.data().descriptors.get(fd).cloned();
    if let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { .. })) = descriptor {
        let bytes = match caller
            .data_mut()
            .read_socket_pair(fd, capacity as usize)
            .await
        {
            Ok(bytes) => bytes,
            Err(errno) => return errno,
        };
        let status = p1_write_iovs_from_bytes(caller, memory, iovs, &bytes, ro_datalen);
        if status != p1::errno::SUCCESS {
            return status;
        }
        return p1_write_u16(caller, memory, ro_flags, 0);
    }
    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Connected {
        stream,
        ..
    }))) = descriptor
    else {
        return p1_connected_tcp_stream(caller, fd)
            .err()
            .unwrap_or(p1::errno::INVAL);
    };
    let status = caller.data().require_tcp_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    let bytes = match service.tcp_read(stream, capacity, u64::MAX).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => Vec::new(),
        Err(error) => return p1_errno_from_tcp_error(error),
    };
    let status = p1_write_iovs_from_bytes(caller, memory, iovs, &bytes, ro_datalen);
    if status != p1::errno::SUCCESS {
        return status;
    }
    p1_write_u16(caller, memory, ro_flags, 0)
}

async fn p1_sock_send<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    si_data: u32,
    si_data_len: u32,
    _si_flags: u16,
    so_datalen: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let bytes = match p1_read_iovs_to_bytes(caller, memory, si_data, si_data_len) {
        Ok(bytes) => bytes,
        Err(errno) => return errno,
    };
    let descriptor = caller.data().descriptors.get(fd).cloned();
    if let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { writer, .. })) = descriptor
    {
        let written = match u32::try_from(bytes.len()) {
            Ok(written) => written,
            Err(_) => return p1::errno::OVERFLOW,
        };
        if writer.write(bytes).is_err() {
            return p1::errno::IO;
        }
        return p1_write_u32(caller, memory, so_datalen, written);
    }
    let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Connected {
        stream,
        ..
    }))) = descriptor
    else {
        return p1_connected_tcp_stream(caller, fd)
            .err()
            .unwrap_or(p1::errno::INVAL);
    };
    let status = caller.data().require_tcp_authority();
    if status != p1::errno::SUCCESS {
        return status;
    }
    let Some(service) = caller.data().runtime_state.network_service() else {
        return p1::errno::NETDOWN;
    };
    if let Err(error) = service.tcp_write_all(stream, &bytes, u64::MAX).await {
        return p1_errno_from_tcp_error(error);
    }
    let written = match u32::try_from(bytes.len()) {
        Ok(written) => written,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, so_datalen, written)
}

async fn p1_sock_shutdown<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    _how: u8,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let descriptor = caller.data().descriptors.get(fd).cloned();
    match descriptor {
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Connected { stream, .. },
        ))) => {
            let status = caller.data().require_tcp_authority();
            if status != p1::errno::SUCCESS {
                return status;
            }
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            service.tcp_close(stream).await;
            let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(slot))) =
                caller.data_mut().descriptors.get_mut(fd)
            else {
                return p1::errno::BADF;
            };
            *slot = WasixTcpSocket::Unconnected;
            p1::errno::SUCCESS
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(
            WasixTcpSocket::Unconnected,
        ))) => p1::errno::INVAL,
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
            socket,
            ..
        }))) => {
            let status = caller.data().require_udp_authority();
            if status != p1::errno::SUCCESS {
                return status;
            }
            let Some(service) = caller.data().runtime_state.network_service() else {
                return p1::errno::NETDOWN;
            };
            service.udp_close(socket).await;
            let Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(slot))) =
                caller.data_mut().descriptors.get_mut(fd)
            else {
                return p1::errno::BADF;
            };
            *slot = WasixUdpSocket::Unbound;
            p1::errno::SUCCESS
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound))) => {
            p1::errno::INVAL
        }
        Some(Preview1Descriptor::Socket(WasixSocketDescriptor::Pair { .. })) => p1::errno::NOTSUP,
        Some(_) => p1::errno::NOTSOCK,
        None => p1::errno::BADF,
    }
}

fn p1_environment_strings<CpuImpl, HostFs>(
    store: &Preview1ProgramStore<CpuImpl, HostFs>,
) -> Vec<String>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    store
        .environment
        .iter()
        .map(|(name, value)| {
            let mut entry = String::with_capacity(name.len() + value.len() + 1);
            entry.push_str(name);
            entry.push('=');
            entry.push_str(value);
            entry
        })
        .collect()
}

fn nul_terminated_list_size<'a>(mut values: impl Iterator<Item = &'a str>) -> Option<u32> {
    values.try_fold(0u32, |acc, value| {
        let len = u32::try_from(value.len()).ok()?;
        acc.checked_add(len)?.checked_add(1)
    })
}

fn p1_write_string_array<'a, CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    pointers: u32,
    buffer: u32,
    values: impl Iterator<Item = &'a str>,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let mut current = buffer;
    let mut status = p1::errno::SUCCESS;
    for (index, value) in values.enumerate() {
        let pointer = match u32::try_from(index)
            .ok()
            .and_then(|index| index.checked_mul(4))
            .and_then(|offset| pointers.checked_add(offset))
        {
            Some(pointer) => pointer,
            None => return p1::errno::OVERFLOW,
        };
        status = status.max(p1_write_u32(caller, memory, pointer, current));
        status = status.max(p1_write_memory(caller, memory, current, value.as_bytes()));
        current = match current
            .checked_add(u32::try_from(value.len()).unwrap_or(u32::MAX))
            .and_then(|value| value.checked_add(1))
        {
            Some(next) => next,
            None => return p1::errno::OVERFLOW,
        };
        status = status.max(p1_write_u8(caller, memory, current - 1, 0));
    }
    status
}

fn p1_memory<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
) -> Option<Preview1Memory>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Some(memory) = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
    {
        let data = memory.data(&mut *caller);
        return Some(Preview1Memory {
            base: data.as_ptr() as usize,
            len: data.len(),
        });
    }
    let shared_memory = caller.data().imported_memory.as_ref()?;
    let data = shared_memory.data();
    Some(Preview1Memory {
        base: data.as_ptr().cast::<u8>() as usize,
        len: data.len(),
    })
}

fn p1_memory_from_instance<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
) -> Option<Preview1Memory>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if let Some(memory) = instance.get_memory(&mut *store, "memory") {
        let data = memory.data(&mut *store);
        return Some(Preview1Memory {
            base: data.as_ptr() as usize,
            len: data.len(),
        });
    }
    let shared_memory = store.data().imported_memory.as_ref()?;
    let data = shared_memory.data();
    Some(Preview1Memory {
        base: data.as_ptr().cast::<u8>() as usize,
        len: data.len(),
    })
}

fn preview1_read_memory(
    memory: Preview1Memory,
    ptr: u32,
    len: usize,
) -> Result<Vec<u8>, ProgramExecError> {
    let start = ptr as usize;
    let end = start.checked_add(len).ok_or_else(|| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::GuestMemoryAccessOverflow,
    })?;
    if end > memory.len {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        });
    }
    let mut bytes = Vec::with_capacity(len);
    // SAFETY: preview1/WASIX host calls run synchronously on the owning
    // store task. The bounds check above proves the source range lies inside
    // the guest memory view captured for this host call.
    unsafe {
        bytes.set_len(len);
        core::ptr::copy_nonoverlapping(
            (memory.base as *const u8).add(start),
            bytes.as_mut_ptr(),
            len,
        );
    }
    Ok(bytes)
}

fn preview1_write_memory(memory: Preview1Memory, ptr: u32, bytes: &[u8]) -> i32 {
    let start = ptr as usize;
    let Some(end) = start.checked_add(bytes.len()) else {
        return p1::errno::FAULT;
    };
    if end > memory.len {
        return p1::errno::FAULT;
    }
    // SAFETY: the bounds check above proves the destination range lies inside
    // the guest memory view captured for this host call.
    unsafe {
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (memory.base as *mut u8).add(start),
            bytes.len(),
        );
    }
    p1::errno::SUCCESS
}

fn preview1_read_u32(memory: Preview1Memory, ptr: u32) -> Result<u32, ProgramExecError> {
    let bytes = preview1_read_memory(memory, ptr, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap_or_else(|_| {
        panic!("Preview1 raw u32 read must return exactly 4 bytes")
    })))
}

fn preview1_write_u32(memory: Preview1Memory, ptr: u32, value: u32) -> i32 {
    preview1_write_memory(memory, ptr, &value.to_le_bytes())
}

fn p1_read_iovs<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    iovs: u32,
    iovs_len: u32,
) -> Result<Vec<(u32, u32)>, i32> {
    let mut result = Vec::new();
    for index in 0..iovs_len {
        let offset = index.checked_mul(8).ok_or(p1::errno::OVERFLOW)?;
        let iov = iovs.checked_add(offset).ok_or(p1::errno::OVERFLOW)?;
        let ptr = p1_try_read_u32(caller, memory, iov).map_err(|_| p1::errno::FAULT)?;
        let len = p1_try_read_u32(caller, memory, iov + 4).map_err(|_| p1::errno::FAULT)?;
        result.push((ptr, len));
    }
    Ok(result)
}

fn p1_read_iovs_to_bytes<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    iovs: u32,
    iovs_len: u32,
) -> Result<Vec<u8>, i32> {
    let iovs = p1_read_iovs(caller, memory, iovs, iovs_len)?;
    let capacity = iovs.iter().try_fold(0usize, |sum, (_, len)| {
        sum.checked_add((*len).try_into().ok()?)
    });
    let Some(capacity) = capacity else {
        return Err(p1::errno::OVERFLOW);
    };
    let mut bytes = Vec::with_capacity(capacity);
    for (ptr, len) in iovs {
        let len = usize::try_from(len).map_err(|_| p1::errno::OVERFLOW)?;
        let chunk = p1_read_memory(caller, memory, ptr, len).map_err(|_| p1::errno::FAULT)?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn p1_read_memory<T>(
    _caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    len: usize,
) -> Result<Vec<u8>, ProgramExecError> {
    preview1_read_memory(memory, ptr, len)
}

fn p1_write_memory<T>(
    _caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    bytes: &[u8],
) -> i32 {
    preview1_write_memory(memory, ptr, bytes)
}

fn p1_try_read_u32<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<u32, ProgramExecError> {
    let bytes = p1_read_memory(caller, memory, ptr, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap_or_else(|_| {
        panic!("Preview1 u32 read must return exactly 4 bytes")
    })))
}

fn p1_try_read_u8<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<u8, ProgramExecError> {
    let bytes = p1_read_memory(caller, memory, ptr, 1)?;
    Ok(bytes[0])
}

fn p1_try_read_u16<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<u16, ProgramExecError> {
    let bytes = p1_read_memory(caller, memory, ptr, 2)?;
    Ok(u16::from_le_bytes(bytes.try_into().unwrap_or_else(|_| {
        panic!("Preview1 u16 read must return exactly 2 bytes")
    })))
}

fn p1_try_read_u64<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
) -> Result<u64, ProgramExecError> {
    let bytes = p1_read_memory(caller, memory, ptr, 8)?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap_or_else(|_| {
        panic!("Preview1 u64 read must return exactly 8 bytes")
    })))
}

fn p1_write_u8<T>(caller: &mut Caller<'_, T>, memory: Preview1Memory, ptr: u32, value: u8) -> i32 {
    p1_write_memory(caller, memory, ptr, &[value])
}

fn p1_write_u16<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    value: u16,
) -> i32 {
    p1_write_memory(caller, memory, ptr, &value.to_le_bytes())
}

fn p1_write_u32<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    value: u32,
) -> i32 {
    p1_write_memory(caller, memory, ptr, &value.to_le_bytes())
}

fn p1_write_u64<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    ptr: u32,
    value: u64,
) -> i32 {
    p1_write_memory(caller, memory, ptr, &value.to_le_bytes())
}

async fn p1_write_descriptor<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    bytes: &[u8],
) -> Result<u32, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Stdout) => {
            caller
                .data()
                .write_output(crate::ComponentOutputStreamKind::Stdout, bytes);
            u32::try_from(bytes.len()).map_err(|_| p1::errno::OVERFLOW)
        }
        Some(Preview1Descriptor::Stderr) => {
            caller
                .data()
                .write_output(crate::ComponentOutputStreamKind::Stderr, bytes);
            u32::try_from(bytes.len()).map_err(|_| p1::errno::OVERFLOW)
        }
        Some(Preview1Descriptor::PipeWrite { writer }) => {
            writer.write(bytes.to_vec()).map_err(|_| p1::errno::IO)?;
            u32::try_from(bytes.len()).map_err(|_| p1::errno::OVERFLOW)
        }
        Some(Preview1Descriptor::Event(event)) => {
            if bytes.len() != 8 {
                return Err(p1::errno::INVAL);
            }
            let increment = u64::from_le_bytes(
                bytes
                    .try_into()
                    .unwrap_or_else(|_| panic!("eventfd write length was checked")),
            );
            event.write(increment)?;
            Ok(8)
        }
        Some(Preview1Descriptor::File { .. }) => {
            let Some(Preview1Descriptor::File {
                descriptor,
                offset,
                fdflags,
            }) = caller.data().descriptors.get(fd)
            else {
                return Err(p1::errno::BADF);
            };
            let current_offset = *offset;
            let descriptor = descriptor.clone();
            let next_offset = current_offset.saturating_add(bytes.len() as u64);
            if let Some(host_path) =
                crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned)
            {
                if !descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
                    return Err(p1::errno::NOTCAPABLE);
                }
                let service = caller
                    .data()
                    .filesystem
                    .host_service()
                    .map_err(p1_errno_from_fs)?;
                let host_offset = if fdflags & P1_FDFLAG_APPEND != 0 {
                    service
                        .stat_path(&host_path)
                        .await
                        .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
                        .map_err(p1_errno_from_fs)?
                        .size
                } else {
                    current_offset
                };
                service
                    .write_file(&host_path, host_offset, bytes)
                    .await
                    .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
                    .map_err(p1_errno_from_fs)?;
                caller
                    .data_mut()
                    .filesystem
                    .invalidate_host_subtree(&descriptor.path);
                let Some(Preview1Descriptor::File { offset, .. }) =
                    caller.data_mut().descriptors.get_mut(fd)
                else {
                    panic!("Preview1 descriptor disappeared during host write");
                };
                *offset = next_offset;
                return u32::try_from(bytes.len()).map_err(|_| p1::errno::OVERFLOW);
            }
            let now_nanos = caller.data().now_nanos();
            let write_offset: usize = current_offset.try_into().map_err(|_| p1::errno::OVERFLOW)?;
            if fdflags & P1_FDFLAG_APPEND != 0 {
                caller
                    .data_mut()
                    .filesystem
                    .append(&descriptor, bytes, now_nanos)
                    .map_err(p1_errno_from_fs)?;
            } else {
                caller
                    .data_mut()
                    .filesystem
                    .write_at(&descriptor, write_offset, bytes, now_nanos)
                    .map_err(p1_errno_from_fs)?;
            }
            let Some(Preview1Descriptor::File { offset, .. }) =
                caller.data_mut().descriptors.get_mut(fd)
            else {
                panic!("Preview1 descriptor disappeared during write");
            };
            *offset = next_offset;
            u32::try_from(bytes.len()).map_err(|_| p1::errno::OVERFLOW)
        }
        Some(_) => Err(p1::errno::BADF),
        None => Err(p1::errno::BADF),
    }
}

async fn p1_read_descriptor<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    fd: i32,
    capacity: usize,
) -> Result<Vec<u8>, i32>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match caller.data().descriptors.get(fd) {
        Some(Preview1Descriptor::Stdin { .. }) => Ok(caller.data_mut().read_stdin(capacity).await),
        Some(Preview1Descriptor::PipeRead { .. }) => {
            caller.data_mut().read_pipe(fd, capacity).await
        }
        Some(Preview1Descriptor::Event(event)) => {
            if capacity < 8 {
                return Err(p1::errno::INVAL);
            }
            Ok(event.read().await.to_le_bytes().to_vec())
        }
        Some(Preview1Descriptor::File {
            descriptor, offset, ..
        }) => {
            let descriptor = descriptor.clone();
            let offset = *offset;
            let bytes = if let Some(host_path) =
                crate::guest_host_share_path(&descriptor.path).map(ToOwned::to_owned)
            {
                let service = caller
                    .data()
                    .filesystem
                    .host_service()
                    .map_err(p1_errno_from_fs)?;
                let max_bytes = u32::try_from(capacity).map_err(|_| p1::errno::OVERFLOW)?;
                service
                    .read_file_range(&host_path, offset, max_bytes)
                    .await
                    .map_err(crate::wasmtime_adapter::wasi::map_host_fs_error)
                    .map_err(p1_errno_from_fs)?
            } else {
                caller
                    .data()
                    .filesystem
                    .read_file_chunk(&descriptor, offset, capacity)
                    .map_err(p1_errno_from_fs)?
            };
            if let Some(Preview1Descriptor::File { offset, .. }) =
                caller.data_mut().descriptors.get_mut(fd)
            {
                *offset = offset.saturating_add(bytes.len() as u64);
            }
            Ok(bytes)
        }
        Some(_) => Err(p1::errno::BADF),
        None => Err(p1::errno::BADF),
    }
}

fn p1_path_flags(flags: u32) -> fs_types::PathFlags {
    let mut result = fs_types::PathFlags::empty();
    if flags & 1 != 0 {
        result |= fs_types::PathFlags::SYMLINK_FOLLOW;
    }
    result
}

fn p1_open_flags(flags: u16) -> fs_types::OpenFlags {
    let mut result = fs_types::OpenFlags::empty();
    if flags & 1 != 0 {
        result |= fs_types::OpenFlags::CREATE;
    }
    if flags & 2 != 0 {
        result |= fs_types::OpenFlags::DIRECTORY;
    }
    if flags & 4 != 0 {
        result |= fs_types::OpenFlags::EXCLUSIVE;
    }
    if flags & 8 != 0 {
        result |= fs_types::OpenFlags::TRUNCATE;
    }
    result
}

fn p1_descriptor_flags(rights: u64, fdflags: u16) -> fs_types::DescriptorFlags {
    let mut flags = fs_types::DescriptorFlags::empty();
    if rights & P1_RIGHT_FD_READ != 0 || rights & P1_RIGHT_FD_READDIR != 0 {
        flags |= fs_types::DescriptorFlags::READ;
    }
    if rights & P1_RIGHT_FD_WRITE != 0 {
        flags |= fs_types::DescriptorFlags::WRITE;
    }
    if rights & P1_RIGHT_PATH_MUTATE_MASK != 0 {
        flags |= fs_types::DescriptorFlags::MUTATE_DIRECTORY;
    }
    let _ = fdflags;
    flags
}

fn p1_descriptor_rights(descriptor: &Preview1Descriptor) -> u64 {
    match descriptor {
        Preview1Descriptor::Stdin { .. } => P1_RIGHT_FD_READ | P1_RIGHT_POLL_FD_READWRITE,
        Preview1Descriptor::Stdout | Preview1Descriptor::Stderr => {
            P1_RIGHT_FD_WRITE | P1_RIGHT_POLL_FD_READWRITE
        }
        Preview1Descriptor::PipeRead { .. } => P1_RIGHT_FD_READ | P1_RIGHT_POLL_FD_READWRITE,
        Preview1Descriptor::PipeWrite { .. } => P1_RIGHT_FD_WRITE | P1_RIGHT_POLL_FD_READWRITE,
        Preview1Descriptor::Event(_) => {
            P1_RIGHT_FD_READ | P1_RIGHT_FD_WRITE | P1_RIGHT_POLL_FD_READWRITE
        }
        Preview1Descriptor::Socket(_) => {
            P1_RIGHT_FD_READ | P1_RIGHT_FD_WRITE | P1_RIGHT_POLL_FD_READWRITE
        }
        Preview1Descriptor::Preopen { descriptor, .. }
        | Preview1Descriptor::File { descriptor, .. } => {
            let mut rights = P1_RIGHT_FD_ADVISE | P1_RIGHT_FD_FILESTAT_GET;
            if descriptor.flags.contains(fs_types::DescriptorFlags::READ) {
                rights |= P1_RIGHT_FD_READ
                    | P1_RIGHT_FD_SEEK
                    | P1_RIGHT_FD_TELL
                    | P1_RIGHT_FD_READDIR
                    | P1_RIGHT_POLL_FD_READWRITE
                    | P1_RIGHT_PATH_READ_MASK;
            }
            if descriptor.flags.contains(fs_types::DescriptorFlags::WRITE) {
                rights |= P1_RIGHT_FD_DATASYNC
                    | P1_RIGHT_FD_SYNC
                    | P1_RIGHT_FD_WRITE
                    | P1_RIGHT_FD_ALLOCATE
                    | P1_RIGHT_FD_FDSTAT_SET_FLAGS
                    | P1_RIGHT_FD_FILESTAT_SET_SIZE
                    | P1_RIGHT_PATH_FILE_WRITE_MASK;
            }
            if descriptor
                .flags
                .contains(fs_types::DescriptorFlags::MUTATE_DIRECTORY)
            {
                rights |= P1_RIGHT_PATH_MUTATE_MASK;
            }
            rights
        }
    }
}

fn p1_filetype(kind: FsNodeKind) -> u8 {
    match kind {
        FsNodeKind::Directory => 3,
        FsNodeKind::File => 4,
        FsNodeKind::Symlink => 7,
    }
}

fn p1_filetype_from_descriptor_type(type_: fs_types::DescriptorType) -> u8 {
    match type_ {
        fs_types::DescriptorType::Directory => 3,
        fs_types::DescriptorType::RegularFile => 4,
        fs_types::DescriptorType::SymbolicLink => 7,
        fs_types::DescriptorType::CharacterDevice => 2,
        fs_types::DescriptorType::BlockDevice => 1,
        _ => 0,
    }
}

fn p1_descriptor_path(descriptor: Option<&Preview1Descriptor>) -> Option<&str> {
    match descriptor {
        Some(Preview1Descriptor::Preopen { descriptor, .. })
        | Some(Preview1Descriptor::File { descriptor, .. }) => Some(&descriptor.path),
        _ => None,
    }
}

fn p1_directory_descriptor(descriptor: Option<&Preview1Descriptor>) -> Option<&FsDescriptor> {
    match descriptor {
        Some(Preview1Descriptor::Preopen { descriptor, .. })
        | Some(Preview1Descriptor::File { descriptor, .. })
            if descriptor.kind == FsNodeKind::Directory =>
        {
            Some(descriptor)
        }
        _ => None,
    }
}

fn p1_poll_descriptor(descriptor: Option<&Preview1Descriptor>, event_type: u8) -> Result<u64, i32> {
    match (descriptor, event_type) {
        (Some(Preview1Descriptor::Stdin { carry }), P1_EVENTTYPE_FD_READ) => Ok(carry.len() as u64),
        (Some(Preview1Descriptor::PipeRead { carry, .. }), P1_EVENTTYPE_FD_READ) => {
            Ok(carry.len() as u64)
        }
        (Some(Preview1Descriptor::Event(event)), P1_EVENTTYPE_FD_READ) => {
            Ok(u64::from(event.state.lock().value != 0) * 8)
        }
        (Some(Preview1Descriptor::Stdout), P1_EVENTTYPE_FD_WRITE)
        | (Some(Preview1Descriptor::Stderr), P1_EVENTTYPE_FD_WRITE)
        | (Some(Preview1Descriptor::PipeWrite { .. }), P1_EVENTTYPE_FD_WRITE)
        | (Some(Preview1Descriptor::Event(_)), P1_EVENTTYPE_FD_WRITE)
        | (Some(Preview1Descriptor::Socket(_)), P1_EVENTTYPE_FD_WRITE) => Ok(usize::MAX as u64),
        (
            Some(Preview1Descriptor::File { .. }) | Some(Preview1Descriptor::Socket(_)),
            P1_EVENTTYPE_FD_READ | P1_EVENTTYPE_FD_WRITE,
        ) => Ok(0),
        (Some(_), _) => Err(p1::errno::INVAL),
        (None, _) => Err(p1::errno::BADF),
    }
}

fn p1_descriptor_stat_from_host_metadata(
    metadata: crate::HostMetadata,
) -> fs_types::DescriptorStat {
    fs_types::DescriptorStat {
        type_: if metadata.qid_type & 0x80 != 0 {
            fs_types::DescriptorType::Directory
        } else {
            fs_types::DescriptorType::RegularFile
        },
        link_count: 1,
        size: metadata.size,
        data_access_timestamp: None,
        data_modification_timestamp: None,
        status_change_timestamp: None,
    }
}

fn p1_write_filestat<CpuImpl, HostFs>(
    caller: &mut Caller<'_, Preview1ProgramStore<CpuImpl, HostFs>>,
    stat: u32,
    value: fs_types::DescriptorStat,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let Some(memory) = p1_memory(caller) else {
        return p1::errno::FAULT;
    };
    let atim = value
        .data_access_timestamp
        .map(|datetime| {
            u64::try_from(datetime.seconds)
                .unwrap_or(0)
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(datetime.nanoseconds))
        })
        .unwrap_or(0);
    let mtim = value
        .data_modification_timestamp
        .map(|datetime| {
            u64::try_from(datetime.seconds)
                .unwrap_or(0)
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(datetime.nanoseconds))
        })
        .unwrap_or(0);
    let ctim = value
        .status_change_timestamp
        .map(|datetime| {
            u64::try_from(datetime.seconds)
                .unwrap_or(0)
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(datetime.nanoseconds))
        })
        .unwrap_or(0);
    p1_write_u64(caller, memory, stat, 0)
        .max(p1_write_u64(caller, memory, stat + 8, 0))
        .max(p1_write_u8(
            caller,
            memory,
            stat + 16,
            p1_filetype_from_descriptor_type(value.type_),
        ))
        .max(p1_write_u64(caller, memory, stat + 24, value.link_count))
        .max(p1_write_u64(caller, memory, stat + 32, value.size))
        .max(p1_write_u64(caller, memory, stat + 40, atim))
        .max(p1_write_u64(caller, memory, stat + 48, mtim))
        .max(p1_write_u64(caller, memory, stat + 56, ctim))
}

fn p1_timestamp_from_fstflags(
    fstflags: u16,
    value_flag: u16,
    now_flag: u16,
    value: u64,
    now_nanos: u64,
) -> Option<u64> {
    if fstflags & now_flag != 0 {
        Some(now_nanos)
    } else if fstflags & value_flag != 0 {
        Some(value)
    } else {
        None
    }
}

fn p1_write_iovs_from_bytes<T>(
    caller: &mut Caller<'_, T>,
    memory: Preview1Memory,
    iovs: Vec<(u32, u32)>,
    bytes: &[u8],
    written_out: u32,
) -> i32 {
    let mut copied = 0usize;
    for (ptr, len) in iovs {
        if copied >= bytes.len() {
            break;
        }
        let len = (len as usize).min(bytes.len() - copied);
        let status = p1_write_memory(caller, memory, ptr, &bytes[copied..copied + len]);
        if status != p1::errno::SUCCESS {
            return status;
        }
        copied += len;
    }
    let copied = match u32::try_from(copied) {
        Ok(copied) => copied,
        Err(_) => return p1::errno::OVERFLOW,
    };
    p1_write_u32(caller, memory, written_out, copied)
}

fn p1_errno_from_component_path(error: crate::ComponentFsPathError) -> i32 {
    match error {
        crate::ComponentFsPathError::InvalidBasePath => p1::errno::INVAL,
        crate::ComponentFsPathError::NotPermitted => p1::errno::PERM,
    }
}

fn p1_errno_from_fs(error: fs_types::ErrorCode) -> i32 {
    match error {
        fs_types::ErrorCode::Access => p1::errno::ACCES,
        fs_types::ErrorCode::Already => p1::errno::EXIST,
        fs_types::ErrorCode::Invalid => p1::errno::INVAL,
        fs_types::ErrorCode::Io => p1::errno::IO,
        fs_types::ErrorCode::IsDirectory => p1::errno::ISDIR,
        fs_types::ErrorCode::Loop => p1::errno::LOOP,
        fs_types::ErrorCode::NoEntry => p1::errno::NOENT,
        fs_types::ErrorCode::NotDirectory => p1::errno::NOTDIR,
        fs_types::ErrorCode::NotEmpty => p1::errno::NOTEMPTY,
        fs_types::ErrorCode::Unsupported => p1::errno::NOTSUP,
        fs_types::ErrorCode::Overflow => p1::errno::OVERFLOW,
        fs_types::ErrorCode::NotPermitted => p1::errno::PERM,
        fs_types::ErrorCode::ReadOnly => p1::errno::ROFS,
        fs_types::ErrorCode::CrossDevice => p1::errno::XDEV,
        _ => p1::errno::IO,
    }
}

fn p1_errno_from_dns_error(error: crate::DnsError) -> i32 {
    match error.kind {
        crate::DnsErrorKind::UnresolvedHost => p1::errno::HOSTUNREACH,
        crate::DnsErrorKind::Timeout => p1::errno::TIMEDOUT,
        crate::DnsErrorKind::Unavailable => p1::errno::NETDOWN,
        crate::DnsErrorKind::Internal => p1::errno::IO,
    }
}

fn p1_errno_from_tcp_error(error: crate::TcpError) -> i32 {
    match error.kind {
        crate::TcpErrorKind::UnresolvedHost => p1::errno::HOSTUNREACH,
        crate::TcpErrorKind::Timeout => p1::errno::TIMEDOUT,
        crate::TcpErrorKind::Unavailable => p1::errno::NETDOWN,
        crate::TcpErrorKind::Internal => p1::errno::IO,
    }
}

fn p1_errno_from_udp_error(error: crate::UdpError) -> i32 {
    match error.kind {
        crate::UdpErrorKind::UnresolvedHost => p1::errno::HOSTUNREACH,
        crate::UdpErrorKind::Timeout => p1::errno::TIMEDOUT,
        crate::UdpErrorKind::PermissionDenied => p1::errno::NOTCAPABLE,
        crate::UdpErrorKind::Unavailable => p1::errno::NETDOWN,
        crate::UdpErrorKind::Internal => p1::errno::IO,
    }
}

fn validate_preview1_program_module_imports(module: &Module) -> Result<(), ProgramExecError> {
    for import in module.imports() {
        if import.module() == "env" && import.name() == "memory" {
            match import.ty() {
                ExternType::Memory(memory) if memory.is_shared() => continue,
                _ => {}
            }
        }
        validate_preview1_program_import(import.module(), import.name())?;
    }
    Ok(())
}

fn validate_preview1_program_import(module_name: &str, name: &str) -> Result<(), ProgramExecError> {
    match module_name {
        "wasi_snapshot_preview1" | "wasi_unstable" => {
            if p1::PREVIEW1_FUNCTIONS.contains(&name)
                && PREVIEW1_PROGRAM_LINKED_IMPORTS.contains(&name)
            {
                return Ok(());
            }
        }
        WASIX_MODULE => {
            if WASIX_PROGRAM_LINKED_IMPORTS.contains(&name) {
                return Ok(());
            }
        }
        _ => {}
    }
    tracing::error!(
        module = module_name,
        import = name,
        "program core module imports unsupported host function"
    );
    Err(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::UnsupportedImport,
    })
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
const P1_RIGHT_PATH_SYMLINK: u64 = 1 << 23;
const P1_RIGHT_PATH_REMOVE_DIRECTORY: u64 = 1 << 24;
const P1_RIGHT_PATH_UNLINK_FILE: u64 = 1 << 25;
const P1_RIGHT_POLL_FD_READWRITE: u64 = 1 << 26;
const P1_FDFLAG_APPEND: u16 = 1 << 0;
const WASIX_FDFLAGSEXT_CLOEXEC: u16 = 1 << 0;
const WASIX_EVENTFDFLAG_SEMAPHORE: u32 = 1 << 0;
const WASIX_OPTION_NONE: u8 = 0;
const WASIX_OPTION_SOME: u8 = 1;
const WASIX_OPTION_UNION_U32_OFFSET: u32 = 4;
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
const WASIX_RIFLAGS_DATA_TRUNCATED: u16 = 1 << 2;
const WASIX_ADDRESS_FAMILY_UNSPEC: u8 = 0;
const WASIX_ADDRESS_FAMILY_IP_INET4: u8 = 1;
const WASIX_ADDRESS_FAMILY_IP_INET6: u8 = 2;
const WASIX_ADDRESS_FAMILY_UNIX: u8 = 3;
const WASIX_ADDR_IP_UNION_OFFSET: u32 = 2;
const WASIX_ADDR_IP_SIZE: u32 = 18;
const WASIX_ADDR_PORT_UNION_OFFSET: u32 = 2;
const WASIX_ADDR_PORT_IP4_ADDRESS_OFFSET: u32 = 4;
const P1_FSTFLAG_ATIM: u16 = 1 << 0;
const P1_FSTFLAG_ATIM_NOW: u16 = 1 << 1;
const P1_FSTFLAG_MTIM: u16 = 1 << 2;
const P1_FSTFLAG_MTIM_NOW: u16 = 1 << 3;
const P1_EVENTTYPE_CLOCK: u8 = 0;
const P1_EVENTTYPE_FD_READ: u8 = 1;
const P1_EVENTTYPE_FD_WRITE: u8 = 2;
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
const WASIX_PROGRAM_LINKED_IMPORTS: &[&str] = &[
    "args_sizes_get",
    "args_get",
    "environ_sizes_get",
    "environ_get",
    "clock_time_get",
    "fd_close",
    "fd_fdstat_get",
    "fd_fdstat_set_flags",
    "fd_filestat_get",
    "fd_prestat_get",
    "fd_prestat_dir_name",
    "fd_read",
    "fd_readdir",
    "fd_renumber",
    "fd_seek",
    "fd_write",
    "path_filestat_get",
    "path_open",
    "sched_yield",
    "proc_exit",
    "proc_exit2",
    "clock_time_set",
    "fd_dup",
    "fd_dup2",
    "fd_event",
    "fd_pipe",
    "tty_get",
    "tty_set",
    "getcwd",
    "chdir",
    "callback_signal",
    "thread_spawn_v2",
    "thread_sleep",
    "thread_id",
    "thread_join",
    "thread_parallelism",
    "thread_signal",
    "futex_wait",
    "futex_wake",
    "futex_wake_all",
    "thread_exit",
    "stack_checkpoint",
    "stack_restore",
    "path_open2",
    "fd_fdflags_get",
    "fd_fdflags_set",
    "proc_raise_interval",
    "proc_fork",
    "proc_exec",
    "proc_exec2",
    "proc_exec3",
    "proc_spawn",
    "proc_spawn2",
    "proc_id",
    "proc_parent",
    "proc_join",
    "proc_signal",
    "proc_signals_get",
    "proc_signals_sizes_get",
    "proc_snapshot",
    "port_bridge",
    "port_unbridge",
    "port_dhcp_acquire",
    "port_addr_add",
    "port_addr_remove",
    "port_addr_clear",
    "port_mac",
    "port_addr_list",
    "port_gateway_set",
    "port_route_add",
    "port_route_remove",
    "port_route_clear",
    "port_route_list",
    "sock_status",
    "sock_addr_local",
    "sock_addr_peer",
    "sock_open",
    "sock_pair",
    "sock_set_opt_flag",
    "sock_get_opt_flag",
    "sock_set_opt_time",
    "sock_get_opt_time",
    "sock_set_opt_size",
    "sock_get_opt_size",
    "sock_join_multicast_v4",
    "sock_leave_multicast_v4",
    "sock_join_multicast_v6",
    "sock_leave_multicast_v6",
    "sock_bind",
    "sock_listen",
    "sock_accept_v2",
    "sock_connect",
    "sock_recv_from",
    "sock_send_to",
    "sock_send_file",
    "resolve",
    "epoll_create",
    "epoll_ctl",
    "epoll_wait",
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

/// Decide whether a compile failure means the cached compiler-plugin
/// runtime is no longer usable. OOM kills come with non-deterministic
/// SharedMemory state because a worker thread may have aborted
/// mid-write; rebuilding from scratch is the safe path. Plain compile
/// errors (invalid input wasm, ABI mismatch) leave the plugin healthy.
fn plugin_runtime_should_be_recycled(error: &ProgramExecError) -> bool {
    matches!(
        error.kind,
        ProgramExecErrorKind::OutOfMemory | ProgramExecErrorKind::Internal
    )
}

fn map_program_runtime_error(error: wasmtime::Error) -> ProgramExecError {
    if error.is::<crate::ProgramOutOfMemory>() {
        tracing::error!(?error, "program runtime reported out of memory");
        return ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: ProgramExecErrorDetail::RuntimeFailure,
        };
    }
    if let Some(killed) = error.downcast_ref::<crate::InstanceKilled>() {
        tracing::error!(?error, reason = ?killed.reason, "program instance was killed");
        let kind = match killed.reason {
            crate::KillReason::OutOfMemory => ProgramExecErrorKind::OutOfMemory,
            crate::KillReason::SupervisorRestart => ProgramExecErrorKind::Internal,
        };
        return ProgramExecError {
            kind,
            detail: ProgramExecErrorDetail::RuntimeFailure,
        };
    }

    tracing::error!(?error, "program runtime operation failed");
    ProgramExecError {
        kind: ProgramExecErrorKind::Internal,
        detail: ProgramExecErrorDetail::RuntimeFailure,
    }
}

fn map_artifact_trust_error(error: ArtifactTrustError) -> ProgramExecError {
    tracing::error!(?error, "artifact trust check failed");
    ProgramExecError {
        kind: ProgramExecErrorKind::InvalidSignature,
        detail: ProgramExecErrorDetail::ArtifactSignatureInvalid,
    }
}

fn map_artifact_profile_error(error: ArtifactProfileError) -> ProgramExecError {
    tracing::error!(?error, "artifact profile check failed");
    ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::ArtifactProfileInvalid,
    }
}

fn trusted_bootfs_payload(bytes: &Bytes) -> Result<Bytes, ProgramExecError> {
    let trusted = cwasm::trust_bootfs_artifact(UntrustedCwasm::new(bytes))
        .map_err(map_artifact_trust_error)?;
    Ok(bytes.slice(..trusted.payload().len()))
}

fn trusted_signed_payload(bytes: &Bytes) -> Result<Bytes, ProgramExecError> {
    let trusted = cwasm::verify_signed_artifact(UntrustedCwasm::new(bytes))
        .map_err(map_artifact_trust_error)?;
    Ok(bytes.slice(..trusted.payload().len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview1_program_linked_imports_match_manifest() {
        assert_eq!(PREVIEW1_PROGRAM_LINKED_IMPORTS, p1::PREVIEW1_FUNCTIONS);
    }

    #[test]
    fn wasix_program_linked_imports_have_authority_mapping() {
        for import in WASIX_PROGRAM_LINKED_IMPORTS {
            assert!(
                crate::wasmtime_adapter::wasix::authority_for(import).is_some(),
                "WASIX linked import {import} has no capability mapping"
            );
        }
    }

    #[test]
    fn wasix_manifest_is_linked_by_core_adapter() {
        assert_eq!(
            WASIX_PROGRAM_LINKED_IMPORTS,
            crate::wasmtime_adapter::wasix::manifest().collect::<Vec<_>>()
        );
    }

    #[test]
    fn wasix_linked_imports_are_accepted_by_core_validator() {
        for import in WASIX_PROGRAM_LINKED_IMPORTS {
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
        let mut table = Preview1DescriptorTable {
            entries: vec![
                Some(Preview1DescriptorEntry::new(
                    Preview1Descriptor::Stdin { carry: Vec::new() },
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
            ],
        };

        assert_eq!(table.renumber(3, 1), p1::errno::SUCCESS);
        match table.get(1) {
            Some(Preview1Descriptor::File { descriptor, .. }) => {
                assert_eq!(descriptor.path, "/redirected");
            }
            _ => panic!("fd 1 should be redirected to the file descriptor"),
        }
        assert!(table.get(3).is_none());
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
        let mut table = Preview1DescriptorTable {
            entries: vec![
                Some(Preview1DescriptorEntry::new(
                    Preview1Descriptor::Stdin { carry: Vec::new() },
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
            ],
        };

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
    fn wasix_socket_operations_select_network_authority_by_socket_kind() {
        let tcp =
            Preview1Descriptor::Socket(WasixSocketDescriptor::Tcp(WasixTcpSocket::Connected {
                stream: 1,
                peer_address: crate::Ipv4Address::new([127, 0, 0, 1]),
                peer_port: 80,
            }));
        let udp_bound =
            Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Bound {
                socket: 2,
                local_port: 5353,
            }));
        let udp_unbound =
            Preview1Descriptor::Socket(WasixSocketDescriptor::Udp(WasixUdpSocket::Unbound));
        let (left_writer, right_reader) = crate::byte_channel();
        let pair = Preview1Descriptor::Socket(WasixSocketDescriptor::Pair {
            reader: right_reader,
            writer: left_writer,
            carry: Vec::new(),
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
            wasix_sock_recv_authority(Some(&udp_unbound)),
            Err(p1::errno::INVAL)
        );
        assert_eq!(
            wasix_sock_send_authority(Some(&nonsocket)),
            Err(p1::errno::NOTSOCK)
        );
        assert_eq!(wasix_sock_send_authority(None), Err(p1::errno::BADF));
    }
}
