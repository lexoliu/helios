use super::*;
use crate::wasmtime_adapter::WasmtimeCompiledComponent;
use crate::wasmtime_adapter::config::AotCompileHint;
use crate::wasmtime_adapter::{WasmtimePrecompiledKind, wasi::WasiImportSet};
use bytes::Bytes;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicI32, Ordering as AtomicOrdering};
use core::task::{Context, Poll};
use helios_compiler_abi::{
    CompileHint as CompilerAbiHint, CompilerRequestHeader, CompilerResponseHeader, CompilerStatus,
    HELIOS_COMPILER_ABI_VERSION, HELIOS_COMPILER_ALLOC, HELIOS_COMPILER_COMPILE,
    HELIOS_COMPILER_INITIALIZE, HELIOS_COMPILER_PTHREAD_SELF_OFFSET,
};
use helios_hal::watchdog::Watchdog;
use wasmtime::component::Component;
use wasmtime::{Caller, InstancePre, Linker as CoreLinker, MemoryType, Module, SharedMemory, Val};

const COMPILER_PLUGIN_PATH: &str = "/bin/compiler.cwasm";
const HELIOS_PROCESS_ID_ENV: &str = "HELIOS_PROCESS_ID";
const RAYON_NUM_THREADS_ENV: &[u8] = b"RAYON_NUM_THREADS=";

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
    spawner: crate::Spawner<CpuImpl>,
    runtime_state: HostRuntimeState<CpuImpl, HostFs>,
    instance_registry: crate::InstanceRegistry,
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
    compiler_artifact: Option<Bytes>,
    clock_cpu: CpuImpl,
    _marker: core::marker::PhantomData<fn() -> HostFs>,
}

struct ProgramSpawnRequest {
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    rights: WasiRights,
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
    write_serial: fn(&[u8]),
    _marker: core::marker::PhantomData<fn() -> HostFs>,
}

struct CompilerCoreShared<T> {
    memory: SharedMemory,
    instance_pre: spin::Once<Arc<InstancePre<T>>>,
    next_thread_id: AtomicI32,
}

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
                    detail: "child exit channel dropped before signalling completion".into(),
                }),
            },
            None => Err(ProgramExecError {
                kind: ProgramExecErrorKind::Internal,
                detail: "child exit was already consumed".into(),
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
    let service = UserProgramService {
        inner: Arc::new(UserProgramServiceInner {
            runtime,
            engine,
            component_cache: Mutex::new(ComponentCache::new(cache_budget)),
            compiler_artifact,
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
        rights: WasiRights,
    ) -> Result<ChildHandle, ProgramExecError> {
        super::emit_stage_marker(exec_context.write_serial, "program:spawn-begin");
        let component = self
            .load_component(&exec_context, &source, hint, exec_context.write_serial)
            .await?;
        self.spawn_loaded(exec_context, name, args, env, component, rights)
    }

    fn spawn_loaded(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        name: String,
        args: Vec<String>,
        mut env: Vec<(String, String)>,
        component: Arc<crate::wasmtime_adapter::WasmtimeCompiledComponent>,
        rights: WasiRights,
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
            rights,
        };

        let (exit_tx, exit_rx) = futures::channel::oneshot::channel();
        let runtime = self.inner.runtime.clone();
        let engine = self.inner.engine.clone();
        let spawner = exec_context.spawner.clone();
        let run_spawner = spawner.clone();
        let progress = spawner.progress_counter();

        spawner.spawn_detached(async move {
            let result = run_program_component(
                exec_context,
                request.name,
                request.args,
                request.env,
                request.rights,
                run_spawner,
                progress,
                component,
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
        rights: WasiRights,
    ) -> Result<ExecResult, ProgramExecError> {
        let component = self
            .load_component(&exec_context, &source, hint, exec_context.write_serial)
            .await?;
        self.exec_loaded_buffered(
            exec_context,
            name.into(),
            args,
            env,
            component,
            stdin,
            rights,
        )
        .await
    }

    async fn exec_loaded_buffered(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        name: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        component: Arc<crate::wasmtime_adapter::WasmtimeCompiledComponent>,
        stdin: Vec<u8>,
        rights: WasiRights,
    ) -> Result<ExecResult, ProgramExecError> {
        let mut child = self.spawn_loaded(exec_context, name, args, env, component, rights)?;

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

    async fn load_component(
        &self,
        exec_context: &ProgramExecContext<CpuImpl, HostFs>,
        source: &ProgramSource,
        hint: Option<AotCompileHint>,
        write_serial: fn(&[u8]),
    ) -> Result<Arc<crate::wasmtime_adapter::WasmtimeCompiledComponent>, ProgramExecError> {
        let started_at = monotonic_nanos(&self.inner.clock_cpu);
        let payload = match source {
            ProgramSource::SignedArtifact(bytes) => {
                if hint.is_some() {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidHint,
                        detail: "exec hint is not allowed for signed cwasm inputs".into(),
                    });
                }
                trusted_signed_payload(bytes)?
            }
            ProgramSource::BootfsArtifact(bytes) => {
                if hint.is_some() {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidHint,
                        detail: "exec hint is not allowed for signed cwasm inputs".into(),
                    });
                }
                trusted_bootfs_payload(bytes)?
            }
            ProgramSource::RawWasm(wasm) => {
                let profile = crate::classify_raw_wasm(wasm).map_err(map_artifact_profile_error)?;
                if profile.kind != crate::ArtifactKind::Component {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: format!(
                            "raw artifact is {:?}/{:?}; kernel program execution currently requires a Preview2 component artifact",
                            profile.kind, profile.profile
                        ),
                    });
                }
                let hint = hint.unwrap_or(AotCompileHint::Balanced);
                let signed = self
                    .compile_raw_component_to_signed_artifact(exec_context, wasm, hint, false)
                    .await?;
                let signed = Bytes::from(signed);
                trusted_signed_payload(&signed)?
            }
        };
        self.load_precompiled_component(payload, write_serial, started_at)
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
                    detail: "precompiled artifact is a core module; use the core-module runtime path, not the Preview2 component runtime".into(),
                });
            }
            None => {
                return Err(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: "precompiled artifact is not a Wasmtime cwasm module or component"
                        .into(),
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

    fn deserialize_component(
        &self,
        payload: &[u8],
    ) -> Result<WasmtimeCompiledComponent, ProgramExecError> {
        let component = unsafe { Component::deserialize(self.inner.engine.raw(), payload) }
            .map_err(|error| ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: format!("{error:#}"),
            })?;
        let imports = WasiImportSet::from_component(self.inner.engine.raw(), &component);
        for import in imports.names() {
            crate::validate_component_import_name(import).map_err(map_artifact_profile_error)?;
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
            crate::sign_trusted_artifact_payload(&payload).map_err(map_artifact_trust_error)?;
        crate::verify_signed_artifact(crate::UntrustedCwasm::new(&signed))
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
        match WasmtimePrecompiledKind::detect(&compiler_payload) {
            Some(WasmtimePrecompiledKind::CoreModule) => {}
            Some(WasmtimePrecompiledKind::Component) => {
                return Err(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail:
                        "compiler plugin is a Preview2 component; expected a Preview1 core module"
                            .into(),
                });
            }
            None => {
                return Err(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: "compiler plugin is not a Wasmtime cwasm core module".into(),
                });
            }
        }
        let worker_threads = compiler_plugin_worker_threads(&exec_context.cpu);
        tracing::info!(
            worker_threads,
            "invoking compiler plugin with Rayon worker threads"
        );
        let engine = self.inner.engine.raw();
        let module = unsafe { Module::deserialize(engine, compiler_payload.as_ref()) }
            .map_err(map_program_runtime_error)?;
        let mut linker: CoreLinker<CompilerCoreStore<CpuImpl, HostFs>> = CoreLinker::new(engine);
        let shared_memory = compiler_shared_memory(engine, &module)?;
        let shared = Arc::new(CompilerCoreShared {
            memory: shared_memory.clone(),
            instance_pre: spin::Once::new(),
            next_thread_id: AtomicI32::new(0),
        });
        add_compiler_core_imports(&mut linker, shared_memory.clone())?;
        let started_at = exec_context
            .runtime_state
            .uptime_nanos(exec_context.cpu.now().ticks());
        let compiler_instance = exec_context
            .instance_registry
            .register("compiler-plugin", started_at);
        let store_data = CompilerCoreStore {
            cpu: exec_context.cpu.clone(),
            spawner: exec_context.spawner.clone(),
            runtime_state: exec_context.runtime_state.clone(),
            instance: Arc::new(compiler_instance),
            shared: shared.clone(),
            write_serial: exec_context.write_serial,
            _marker: core::marker::PhantomData,
        };
        let mut store = wasmtime::Store::new(engine, store_data);
        configure_compiler_core_store(&mut store);
        define_compiler_shared_memory(&mut linker, &store, &module, shared_memory)?;
        let instance_pre = Arc::new(
            linker
                .instantiate_pre(&module)
                .map_err(map_program_runtime_error)?,
        );
        shared.instance_pre.call_once(|| instance_pre.clone());
        let instance = instance_pre
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
                    detail: format!(
                        "compiler pthread self pointer overflow: tls_base={tls_base}, offset={pthread_self_offset}"
                    ),
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
                detail: format!(
                    "compiler response ABI version {} does not match expected {}",
                    response.abi_version, HELIOS_COMPILER_ABI_VERSION
                ),
            });
        }
        let diagnostic = read_shared_memory(
            store.data().memory(),
            response.diagnostic_ptr,
            response.diagnostic_len,
        )?;
        if response.status != CompilerStatus::Ok {
            return Err(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: String::from_utf8_lossy(&diagnostic).into_owned(),
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
                detail: format!("compiler plugin {COMPILER_PLUGIN_PATH} is not provisioned"),
            })
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

fn compiler_shared_memory(
    engine: &wasmtime::Engine,
    module: &Module,
) -> Result<SharedMemory, ProgramExecError> {
    let mut memory_type = None;
    for import in module.imports() {
        if import.module() == "env" && import.name() == "memory" {
            memory_type = import.ty().memory().cloned();
            break;
        }
    }
    let memory_type = memory_type.ok_or_else(|| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: "compiler plugin does not import env.memory".into(),
    })?;
    if !memory_type.is_shared() {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: "compiler plugin env.memory is not shared".into(),
        });
    }
    let maximum_pages = memory_type.maximum().ok_or_else(|| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: "compiler plugin shared memory does not declare a maximum".into(),
    })?;
    let initial_pages = u32::try_from(memory_type.minimum()).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: format!(
            "compiler plugin minimum memory pages {} exceeds wasm32",
            memory_type.minimum()
        ),
    })?;
    let maximum_pages = u32::try_from(maximum_pages).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: format!("compiler plugin maximum memory pages {maximum_pages} exceeds wasm32"),
    })?;
    SharedMemory::new(engine, MemoryType::shared(initial_pages, maximum_pages))
        .map_err(map_program_runtime_error)
}

fn define_compiler_shared_memory<T>(
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
                let now = caller.data().cpu.now().ticks();
                fill_random(caller.data().memory(), ptr as u32, len as u32, now)
            },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap("wasi_snapshot_preview1", "sched_yield", || -> i32 { 0 })
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
             -> i32 { 44 },
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
        .func_wrap("wasi_snapshot_preview1", "fd_close", |_fd: i32| -> i32 {
            0
        })
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_prestat_get",
            |_fd: i32, _buf: i32| -> i32 { 8 },
        )
        .map_err(map_program_runtime_error)?;
    linker
        .func_wrap(
            "wasi_snapshot_preview1",
            "fd_prestat_dir_name",
            |_fd: i32, _path: i32, _len: i32| -> i32 { 8 },
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
            detail: "compiler plugin does not export __tls_base".into(),
        })?;
    match global.get(&mut *store) {
        Val::I32(value) => Ok(value as u32),
        value => Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: format!("compiler __tls_base has non-i32 value {value:?}"),
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
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: format!(
                "compiler allocation of {len} bytes returned null with shared memory size {} pages ({} bytes)",
                memory.size(),
                memory.data_size()
            ),
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
    if fd != 1 && fd != 2 {
        return 8;
    }
    let memory = caller.data().memory();
    let mut written = 0u32;
    for index in 0..iovs_len {
        let iov = iovs + index * 8;
        let ptr = read_u32(memory, iov);
        let len = read_u32(memory, iov + 4);
        let Ok(bytes) = read_shared_memory(memory, ptr, len) else {
            return 28;
        };
        (caller.data().write_serial)(&bytes);
        written = written.saturating_add(len);
    }
    write_u32(memory, nwritten, written)
}

fn fill_random(memory: &SharedMemory, ptr: u32, len: u32, seed: u64) -> i32 {
    let mut bytes = Vec::with_capacity(len as usize);
    let mut state = seed;
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.push(state as u8);
    }
    write_shared_memory(memory, ptr, &bytes).map_or(28, |_| 0)
}

fn read_u32(memory: &SharedMemory, ptr: u32) -> u32 {
    let bytes = read_shared_memory(memory, ptr, 4).unwrap_or_else(|error| {
        panic!("compiler plugin attempted invalid u32 memory read at {ptr}: {error:?}")
    });
    u32::from_le_bytes(
        bytes
            .try_into()
            .unwrap_or_else(|_| panic!("u32 read must return 4 bytes")),
    )
}

fn write_u32(memory: &SharedMemory, ptr: u32, value: u32) -> i32 {
    write_shared_memory(memory, ptr, &value.to_le_bytes()).map_or(28, |_| 0)
}

fn write_u64(memory: &SharedMemory, ptr: u32, value: u64) -> i32 {
    write_shared_memory(memory, ptr, &value.to_le_bytes()).map_or(28, |_| 0)
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
        detail: "compiler plugin memory read overflow".into(),
    })?;
    if end > data.len() {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: format!(
                "compiler plugin memory read [{}..{}) exceeds memory size {}",
                start,
                end,
                data.len()
            ),
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
            detail: "compiler plugin memory write overflow".into(),
        })?;
    if end > data.len() {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: format!(
                "compiler plugin memory write [{}..{}) exceeds memory size {}",
                start,
                end,
                data.len()
            ),
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
            spawner: store.spawner().clone(),
            runtime_state: store.runtime_state.clone(),
            instance_registry: store.instance_registry.clone(),
            read_serial: store.serial_reader_fn(),
            write_serial: store.serial_writer_fn(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_program_component<CpuImpl, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, HostFs>,
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    _rights: WasiRights,
    spawner: crate::Spawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    compiled: Arc<crate::wasmtime_adapter::WasmtimeCompiledComponent>,
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
        exec_context.spawner.clone(),
        exec_context.runtime_state.clone(),
        exec_context.instance_registry,
        launched_instance,
        false,
        exec_context.runtime_state,
        argv,
        env,
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

fn map_program_runtime_error(error: wasmtime::Error) -> ProgramExecError {
    if error.is::<crate::ProgramOutOfMemory>() {
        return ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: format!("{error:#}"),
        };
    }

    ProgramExecError {
        kind: ProgramExecErrorKind::Internal,
        detail: format!("{error:#}"),
    }
}

fn map_artifact_trust_error(error: crate::ArtifactTrustError) -> ProgramExecError {
    ProgramExecError {
        kind: ProgramExecErrorKind::InvalidSignature,
        detail: error.to_string(),
    }
}

fn map_artifact_profile_error(error: crate::ArtifactProfileError) -> ProgramExecError {
    ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: error.to_string(),
    }
}

fn trusted_bootfs_payload(bytes: &Bytes) -> Result<Bytes, ProgramExecError> {
    let trusted = crate::trust_bootfs_artifact(crate::UntrustedCwasm::new(bytes))
        .map_err(map_artifact_trust_error)?;
    Ok(bytes.slice(..trusted.payload().len()))
}

fn trusted_signed_payload(bytes: &Bytes) -> Result<Bytes, ProgramExecError> {
    let trusted = crate::verify_signed_artifact(crate::UntrustedCwasm::new(bytes))
        .map_err(map_artifact_trust_error)?;
    Ok(bytes.slice(..trusted.payload().len()))
}
