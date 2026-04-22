use super::*;
use helios_hal::watchdog::Watchdog;

const HELIOS_PROCESS_ID_ENV: &str = "HELIOS_PROCESS_ID";

#[derive(Clone, Copy)]
pub struct ProgramServiceConfig {
    compile_workers: usize,
}

impl ProgramServiceConfig {
    pub const fn new(compile_workers: usize) -> Self {
        Self { compile_workers }
    }

    fn worker_count(self) -> usize {
        self.compile_workers
    }

    fn reserved_stack_bytes(self) -> usize {
        self.compile_workers
            .checked_mul(WORKER_STACK_SIZE)
            .unwrap_or_else(|| panic!("program service reserved stack bytes overflow"))
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
    compiler: ComputePool,
    compile_priority: ComputePriority,
    component_cache: Mutex<ComponentCache<crate::wasmtime_adapter::WasmtimeCompiledComponent>>,
    clock_cpu: CpuImpl,
    spawner: crate::Spawner<CpuImpl>,
    _marker: core::marker::PhantomData<fn() -> HostFs>,
}

struct ProgramSpawnRequest {
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    rights: WasiRights,
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
        ProgramServiceConfig::new(worker_count.max(1)),
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

    assert!(
        config.worker_count() != 0,
        "program service requires at least one compile worker slot"
    );

    let available_bytes = heap_stats().available_bytes();
    let reserved_stack_bytes = config.reserved_stack_bytes();
    let cache_budget =
        available_bytes.saturating_sub(reserved_stack_bytes) / COMPONENT_CACHE_FRACTION;
    let compiler_budget = available_bytes
        .saturating_sub(cache_budget)
        .max(reserved_stack_bytes);
    let compiler = ComputePool::new(config.worker_count(), WORKER_STACK_SIZE, compiler_budget)
        .unwrap_or_else(|error| panic!("failed to create launched-program compute pool: {error}"));
    let runtime = crate::wasmtime_adapter::WasmtimeComponentRuntime::new(cpu.clone());
    let engine = <crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl> as crate::ComponentRuntimeFactory<CpuImpl, HostRuntimeState<CpuImpl, HostFs>, HostFs>>::create_engine(&runtime)
        .unwrap_or_else(|error| panic!("failed to create launched-program engine: {error:#}"));
    let service = UserProgramService {
        inner: Arc::new(UserProgramServiceInner {
            runtime,
            engine,
            compiler,
            compile_priority: ComputePriority::NORMAL,
            component_cache: Mutex::new(ComponentCache::new(cache_budget)),
            clock_cpu: cpu.clone(),
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
    kernel.run();
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
        ComponentHostProcessorRole::ProgramWorker => {
            run_program_workers_forever(cpu, kernel, debug_state);
        }
    }
}

impl<CpuImpl, HostFs> UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    /// Spawn a new child program. The returned handle gives the caller
    /// direct access to the child's stdin/stdout/stderr channels and a
    /// future resolving with its exit status.
    pub(crate) async fn spawn(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        name: String,
        args: Vec<String>,
        mut env: Vec<(String, String)>,
        wasm: alloc::vec::Vec<u8>,
        rights: WasiRights,
    ) -> Result<ChildHandle, ProgramExecError> {
        let component = self.compile_component(&wasm).await?;

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

        self.inner.spawner.spawn_detached(async move {
            let result = run_program_component(
                exec_context,
                request.name,
                request.args,
                request.env,
                request.rights,
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
        wasm: &[u8],
        stdin: Vec<u8>,
        rights: WasiRights,
    ) -> Result<ExecResult, ProgramExecError> {
        let mut child = self
            .spawn(exec_context, name.into(), args, env, wasm.to_vec(), rights)
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

    async fn compile_component(
        &self,
        wasm: &[u8],
    ) -> Result<Arc<crate::wasmtime_adapter::WasmtimeCompiledComponent>, ProgramExecError> {
        let started_at = monotonic_nanos(&self.inner.clock_cpu);
        if let Some(component) = self.inner.component_cache.lock().get(wasm) {
            let now = monotonic_nanos(&self.inner.clock_cpu);
            tracing::info!(
                target: "helios_component_host::program_host",
                phase = "compile-component",
                cache = "hit",
                wasm_bytes = wasm.len(),
                elapsed_ms = elapsed_millis(started_at, now),
                "program component cache hit"
            );
            return Ok(component);
        }

        let wasm = Arc::<[u8]>::from(wasm.to_vec());
        let mut compiled =
            core::pin::pin!(self.inner.compiler.spawn(self.inner.compile_priority, {
                let engine = self.inner.engine.clone();
                let wasm = wasm.clone();
                move || {
                    use crate::ComponentRuntimeEngine;
                    engine.compile(&wasm)
                }
            },));
        let compiled = core::future::poll_fn(|cx| match compiled.as_mut().poll(cx) {
            core::task::Poll::Ready(result) => core::task::Poll::Ready(result),
            core::task::Poll::Pending => {
                if self.inner.compiler.run_next() {
                    cx.waker().wake_by_ref();
                }
                core::task::Poll::Pending
            }
        })
        .await
        .map_err(|error| ProgramExecError {
            kind: ProgramExecErrorKind::QueueSaturated,
            detail: error.to_string(),
        })?
        .map_err(|error: wasmtime::Error| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: format!("{error:#}"),
        })?;

        let component = Arc::new(compiled);
        let now = monotonic_nanos(&self.inner.clock_cpu);
        tracing::info!(
            target: "helios_component_host::program_host",
            phase = "compile-component",
            cache = "miss",
            wasm_bytes = wasm.len(),
            elapsed_ms = elapsed_millis(started_at, now),
            "program component compiled"
        );
        Ok(self
            .inner
            .component_cache
            .lock()
            .insert_if_missing(wasm, component))
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

    let context = ComponentExecContext::new(
        exec_context.cpu,
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
    let executor =
        <crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl> as ComponentRuntimeFactory<
            CpuImpl,
            HostRuntimeState<CpuImpl, HostFs>,
            HostFs,
        >>::instantiate(runtime, engine, &compiled, ComponentWorld::Program, context)
        .await
        .map_err(map_program_runtime_error)?;

    let result = executor.run().await.map_err(map_program_runtime_error)?;

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
