use super::*;
use crate::wasmtime_adapter::config::AotCompileHint;
use ed25519_dalek::SigningKey;
use helios_artifact::sign_payload_with_key;
use helios_hal::watchdog::Watchdog;
use wasmparser::Parser;
use wasmtime::Engine;
use wasmtime::component::Component;

const HELIOS_PROCESS_ID_ENV: &str = "HELIOS_PROCESS_ID";
const PROGRAM_PHASE_HEARTBEAT_INTERVAL_NANOS: u64 = 5_000_000_000;

#[derive(Clone, Copy)]
pub struct ProgramServiceConfig;

impl ProgramServiceConfig {
    pub const fn new(_compile_workers: usize) -> Self {
        Self
    }

    pub const fn with_inline_compile_driver(self, _drive_compile_inline: bool) -> Self {
        self
    }
}

#[derive(Clone)]
pub struct UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    inner: Arc<UserProgramServiceInner<CpuImpl, HostFs>>,
}

#[derive(Clone)]
pub(crate) struct ProgramExecContext<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    runtime: crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    engine: crate::wasmtime_adapter::WasmtimeEngine,
    component_cache: Mutex<ComponentCache<crate::wasmtime_adapter::WasmtimeCompiledComponent>>,
    clock_cpu: CpuImpl,
    progress: helios_hal::watchdog::ProgressCounter,
    spawner: crate::Spawner<CpuImpl>,
    _marker: core::marker::PhantomData<fn() -> HostFs>,
}

struct ProgramSpawnRequest {
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    rights: WasiRights,
}

pub(crate) enum ProgramSource {
    RawWasm(Vec<u8>),
    SignedArtifact(Vec<u8>),
    BootfsArtifact(Vec<u8>),
}

fn spawn_program_phase_heartbeat<CpuImpl>(
    spawner: &crate::Spawner<CpuImpl>,
    cpu: &CpuImpl,
    progress: &helios_hal::watchdog::ProgressCounter,
    write_serial: fn(&[u8]),
    phase: &'static str,
    started_at: u64,
    done: &Arc<core::sync::atomic::AtomicBool>,
) where
    CpuImpl: Cpu + Clone,
{
    spawner.spawn_detached({
        let done = done.clone();
        let cpu = cpu.clone();
        let progress = progress.clone();
        async move {
            let mut next_heartbeat =
                started_at.saturating_add(PROGRAM_PHASE_HEARTBEAT_INTERVAL_NANOS);
            loop {
                if done.load(core::sync::atomic::Ordering::Acquire) {
                    return;
                }

                let now = monotonic_nanos(&cpu);
                if now >= next_heartbeat {
                    progress.record_progress();
                    let elapsed_ms = elapsed_millis(started_at, now);
                    let message =
                        format!("\n[KDBG program:{phase}-progress elapsed_ms={elapsed_ms}]\n");
                    write_serial(message.as_bytes());
                    next_heartbeat = now.saturating_add(PROGRAM_PHASE_HEARTBEAT_INTERVAL_NANOS);
                }

                crate::yield_now().await;
            }
        }
    });
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
    kernel: &crate::Kernel<CpuImpl, WatchdogImpl>,
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
) -> UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    install_program_service_with_config(kernel, cpu, debug_state, ProgramServiceConfig::new(1))
}

pub fn install_component_host_program_service<CpuImpl, HostFs, WatchdogImpl>(
    kernel: &crate::Kernel<CpuImpl, WatchdogImpl>,
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
) -> Option<UserProgramService<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    let topology = kernel.topology();
    if cpu.current_processor() != topology.bootstrap_processor {
        return None;
    }

    let worker_count =
        component_host_worker_count(topology.configured_processors, topology.bootstrap_processor);
    Some(install_program_service_with_config(
        kernel,
        cpu,
        debug_state,
        ProgramServiceConfig::new(worker_count.max(1))
            .with_inline_compile_driver(worker_count == 0),
    ))
}

pub fn install_program_service_with_config<CpuImpl, HostFs, WatchdogImpl>(
    kernel: &crate::Kernel<CpuImpl, WatchdogImpl>,
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
    config: ProgramServiceConfig,
) -> UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    if let Some(service) = debug_state.program_service() {
        return service;
    }

    let available_bytes = heap_stats().available_bytes();
    let _ = config;
    let cache_budget = available_bytes / COMPONENT_CACHE_FRACTION;
    let runtime = crate::wasmtime_adapter::WasmtimeComponentRuntime::new(cpu.clone());
    let engine = <crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl> as crate::ComponentRuntimeFactory<CpuImpl, HostRuntimeState<CpuImpl, HostFs>, HostFs>>::create_engine(&runtime)
        .unwrap_or_else(|error| panic!("failed to create launched-program engine: {error:#}"));
    let service = UserProgramService {
        inner: Arc::new(UserProgramServiceInner {
            runtime,
            engine,
            component_cache: Mutex::new(ComponentCache::new(cache_budget)),
            clock_cpu: cpu.clone(),
            progress: kernel.spawner().progress_counter(),
            spawner: kernel.spawner(),
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    let component_name = component.name();
    super::emit_stage_marker(write_serial, "boot");
    tracing::info!(
        component = component_name,
        "launching embedded system component"
    );
    kernel
        .run_local_future(run_system_component(
            component,
            world,
            cpu.clone(),
            kernel.spawner(),
            debug_state,
            read_serial,
            write_serial,
        ))
        .unwrap_or_else(|error| panic!("failed to exec embedded system component:\n{error:#}"));
    super::emit_stage_marker(write_serial, "done");
    tracing::info!(
        component = component_name,
        "embedded system component exited cleanly"
    );
    cpu.shutdown()
}

pub fn run_program_workers_forever<CpuImpl, HostFs, WatchdogImpl>(
    _cpu: CpuImpl,
    kernel: crate::Kernel<CpuImpl, WatchdogImpl>,
    _debug_state: HostRuntimeState<CpuImpl, HostFs>,
) -> !
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
    WatchdogImpl: Watchdog + Clone,
{
    let topology = kernel.topology();
    match component_host_processor_role(
        cpu.current_processor(),
        topology.configured_processors,
        topology.bootstrap_processor,
    ) {
        ComponentHostProcessorRole::Kernel => kernel.run(),
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

impl<CpuImpl, HostFs> UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
        mut env: Vec<(String, String)>,
        source: ProgramSource,
        hint: Option<AotCompileHint>,
        rights: WasiRights,
    ) -> Result<ChildHandle, ProgramExecError> {
        super::emit_stage_marker(exec_context.write_serial, "program:spawn-begin");
        let component = self
            .load_component(&source, hint, exec_context.write_serial)
            .await?;

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
        let mut child = self
            .spawn(exec_context, name.into(), args, env, source, hint, rights)
            .await?;

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

    pub(crate) fn aot(
        &self,
        wasm: &[u8],
        hint: AotCompileHint,
    ) -> Result<Vec<u8>, ProgramExecError> {
        self.compile_raw_component_to_signed_artifact(wasm, hint)
    }

    async fn load_component(
        &self,
        source: &ProgramSource,
        hint: Option<AotCompileHint>,
        write_serial: fn(&[u8]),
    ) -> Result<Arc<crate::wasmtime_adapter::WasmtimeCompiledComponent>, ProgramExecError> {
        let started_at = monotonic_nanos(&self.inner.clock_cpu);
        let trusted = match source {
            ProgramSource::SignedArtifact(bytes) => {
                let trusted = crate::verify_signed_artifact(crate::UntrustedWasmc::new(bytes))
                    .map_err(map_artifact_trust_error)?;
                if hint.is_some() {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidHint,
                        detail: "exec hint is not allowed for signed wasmc inputs".into(),
                    });
                }
                trusted
            }
            ProgramSource::BootfsArtifact(bytes) => {
                let trusted = crate::trust_bootfs_artifact(crate::UntrustedWasmc::new(bytes))
                    .map_err(map_artifact_trust_error)?;
                if hint.is_some() {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidHint,
                        detail: "exec hint is not allowed for signed wasmc inputs".into(),
                    });
                }
                trusted
            }
            ProgramSource::RawWasm(wasm) => {
                let hint = hint.unwrap_or(AotCompileHint::Balanced);
                let signed = self.compile_raw_component_to_signed_artifact(wasm, hint)?;
                crate::verify_signed_artifact(crate::UntrustedWasmc::new(&signed))
                    .map_err(map_artifact_trust_error)?
            }
        };
        let payload = trusted.payload();
        if let Some(component) = self.inner.component_cache.lock().get(payload) {
            super::emit_stage_marker(write_serial, "program:compile-cache-hit");
            let now = monotonic_nanos(&self.inner.clock_cpu);
            tracing::info!(
                target: "helios_component_host::program_host",
                phase = "compile-component",
                cache = "hit",
                wasm_bytes = payload.len(),
                elapsed_ms = elapsed_millis(started_at, now),
                "program component cache hit"
            );
            return Ok(component);
        }

        let payload = Arc::<[u8]>::from(payload.to_vec());
        super::emit_stage_marker(write_serial, "program:compile-begin");
        tracing::info!(
            target: "helios_component_host::program_host",
            phase = "compile-component",
            cache = "miss",
            wasm_bytes = payload.len(),
            "program component deserialization started"
        );
        let compiled = self.deserialize_component(payload.as_ref())?;
        super::emit_stage_marker(write_serial, "program:compile-end");
        let component = Arc::new(compiled);
        let now = monotonic_nanos(&self.inner.clock_cpu);
        tracing::info!(
            target: "helios_component_host::program_host",
            phase = "compile-component",
            cache = "miss",
            wasm_bytes = payload.len(),
            elapsed_ms = elapsed_millis(started_at, now),
            "program component compiled"
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
    ) -> Result<crate::wasmtime_adapter::WasmtimeCompiledComponent, ProgramExecError> {
        let component = unsafe { Component::deserialize(self.inner.engine.raw(), payload) }
            .map_err(|error| ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: format!("{error:#}"),
            })?;
        Ok(crate::wasmtime_adapter::WasmtimeCompiledComponent { component })
    }

    fn compile_raw_component_to_signed_artifact(
        &self,
        wasm: &[u8],
        hint: AotCompileHint,
    ) -> Result<Vec<u8>, ProgramExecError> {
        if !Parser::is_component(wasm) {
            return Err(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: "raw core wasm programs are not supported on the component program service"
                    .into(),
            });
        }
        let config = crate::wasmtime_adapter::config::build_component_aot_engine_config(
            env!("HELIOS_BUILD_TARGET"),
            hint,
        );
        let engine = Engine::new(&config).map_err(|error| ProgramExecError {
            kind: ProgramExecErrorKind::Internal,
            detail: format!("failed to create component AOT engine: {error:#}"),
        })?;
        let payload = engine
            .precompile_component(wasm)
            .map_err(|error| ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: format!("{error:#}"),
            })?;
        sign_payload_with_key(&payload, &trusted_root_signing_key()).map_err(|error| {
            ProgramExecError {
                kind: ProgramExecErrorKind::Internal,
                detail: error.to_string(),
            }
        })
    }
}

impl<CpuImpl, HostFs> ProgramExecContext<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
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
    spawn_program_phase_heartbeat(
        &spawner,
        &run_cpu,
        &progress,
        exec_context.write_serial,
        "run",
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

fn map_program_runtime_error(error: impl core::fmt::Display) -> ProgramExecError {
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

fn trusted_root_signing_key() -> SigningKey {
    SigningKey::from_bytes(&generated::TRUSTED_ROOT_SIGNING_KEY)
}

mod generated {
    include!(concat!(env!("OUT_DIR"), "/trusted_signing_key.rs"));
}
