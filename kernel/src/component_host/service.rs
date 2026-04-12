use super::*;

#[derive(Clone, Copy)]
pub enum ComponentRunMode {
    KernelExecutor,
    Concurrent,
}

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
    engine: Engine,
    compiler: ComputePool,
    compile_priority: ComputePriority,
    component_cache: Mutex<ComponentCache<crate::wasmtime_adapter::WasmtimeCompiledComponent>>,
    clock_cpu: CpuImpl,
    spawner: crate::Spawner<CpuImpl>,
    _marker: core::marker::PhantomData<fn() -> HostFs>,
}

struct ProgramExecRequest {
    name: String,
    args: Vec<String>,
    wasm: Arc<[u8]>,
    rights: WasiRights,
}

pub fn install_program_service<CpuImpl, HostFs>(
    kernel: &crate::Kernel<CpuImpl>,
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
) -> UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    install_program_service_with_config(
        kernel,
        cpu,
        debug_state,
        ProgramServiceConfig::new(1),
    )
}

pub fn install_component_host_program_service<CpuImpl, HostFs>(
    kernel: &crate::Kernel<CpuImpl>,
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
) -> Option<UserProgramService<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    if cpu.current_processor() != cpu.bootstrap_processor() {
        return None;
    }

    let worker_count = component_host_worker_count(cpu.processor_count(), cpu.bootstrap_processor());
    Some(install_program_service_with_config(
        kernel,
        cpu,
        debug_state,
        ProgramServiceConfig::new(worker_count.max(1)),
    ))
}

pub fn install_program_service_with_config<CpuImpl, HostFs>(
    kernel: &crate::Kernel<CpuImpl>,
    cpu: &CpuImpl,
    debug_state: &HostRuntimeState<CpuImpl, HostFs>,
    config: ProgramServiceConfig,
) -> UserProgramService<CpuImpl, HostFs>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
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
    let engine = build_component_engine_for_platform(cpu)
        .unwrap_or_else(|error| panic!("failed to create launched-program engine: {error:#}"));
    let compiler = ComputePool::new(config.worker_count(), WORKER_STACK_SIZE, compiler_budget)
        .unwrap_or_else(|error| panic!("failed to create launched-program compute pool: {error}"));
    let runtime = crate::wasmtime_adapter::WasmtimeComponentRuntime::new(cpu.clone());
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

pub fn run_embedded_component_forever<CpuImpl, HostFs>(
    component: EmbeddedComponent,
    world: ComponentBindingSet,
    cpu: CpuImpl,
    kernel: &crate::Kernel<CpuImpl>,
    debug_state: HostRuntimeState<CpuImpl, HostFs>,
    read_serial: fn(u32) -> Vec<u8>,
    write_serial: fn(&[u8]),
) -> !
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    run_embedded_component_in_mode_forever(
        component,
        world,
        cpu,
        kernel,
        debug_state,
        read_serial,
        write_serial,
        ComponentRunMode::KernelExecutor,
    )
}

pub fn run_embedded_component_in_mode_forever<CpuImpl, HostFs>(
    component: EmbeddedComponent,
    world: ComponentBindingSet,
    cpu: CpuImpl,
    kernel: &crate::Kernel<CpuImpl>,
    debug_state: HostRuntimeState<CpuImpl, HostFs>,
    read_serial: fn(u32) -> Vec<u8>,
    write_serial: fn(&[u8]),
    run_mode: ComponentRunMode,
) -> !
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    let component_name = component.name();
    super::emit_stage_marker(write_serial, "boot");
    tracing::info!(
        component = component_name,
        "launching embedded system component"
    );
    run_system_component(
        component,
        world,
        cpu.clone(),
        kernel,
        debug_state,
        read_serial,
        write_serial,
        run_mode,
    )
    .unwrap_or_else(|error| panic!("failed to exec embedded system component:\n{error:#}"));
    super::emit_stage_marker(write_serial, "done");
    tracing::info!(component = component_name, "embedded system component exited cleanly");
    cpu.shutdown()
}

pub fn run_program_workers_forever<CpuImpl, HostFs>(
    _cpu: CpuImpl,
    kernel: crate::Kernel<CpuImpl>,
    _debug_state: HostRuntimeState<CpuImpl, HostFs>,
) -> !
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    kernel.run();
}

pub fn run_component_host_processor_forever<CpuImpl, HostFs>(
    cpu: CpuImpl,
    kernel: crate::Kernel<CpuImpl>,
    debug_state: HostRuntimeState<CpuImpl, HostFs>,
    read_serial: fn(u32) -> Vec<u8>,
    write_serial: fn(&[u8]),
    run_mode: ComponentRunMode,
) -> !
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    match component_host_processor_role(
        cpu.current_processor(),
        cpu.processor_count(),
        cpu.bootstrap_processor(),
    ) {
        ComponentHostProcessorRole::Kernel => kernel.run(),
        ComponentHostProcessorRole::SystemComponent => {
            let component = crate::embedded_system_component()
                .unwrap_or_else(|| panic!("embedded init bootfs is missing the system component"));
            run_embedded_component_in_mode_forever(
                component,
                ComponentBindingSet::System,
                cpu,
                &kernel,
                debug_state,
                read_serial,
                write_serial,
                run_mode,
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
    pub(crate) async fn exec(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        name: impl Into<String>,
        args: Vec<String>,
        wasm: &[u8],
        rights: WasiRights,
    ) -> Result<ExecResult, ProgramExecError> {
        let request = ProgramExecRequest {
            name: name.into(),
            args,
            wasm: Arc::<[u8]>::from(wasm.to_vec()),
            rights,
        };
        let service = self.clone();
        let task = self
            .inner
            .spawner
            .spawn(async move { service.exec_on_kernel(exec_context, request).await });
        task.await
    }

    async fn exec_on_kernel(
        &self,
        exec_context: ProgramExecContext<CpuImpl, HostFs>,
        request: ProgramExecRequest,
    ) -> Result<ExecResult, ProgramExecError> {
        let component = self.compile_component(&request.wasm).await?;
        run_program_component(
            exec_context,
            request.name,
            request.args,
            request.rights,
            component,
            &self.inner.runtime,
        )
        .await
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
        let mut compiled = core::pin::pin!(self.inner.compiler.spawn(
            self.inner.compile_priority,
            {
                let engine = self.inner.engine.clone();
                let wasm = wasm.clone();
                move || {
                    let component = Component::from_binary(&engine, &wasm)?;
                    Ok(crate::wasmtime_adapter::WasmtimeCompiledComponent { component })
                }
            },
        ));
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

async fn run_program_component<CpuImpl, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, HostFs>,
    name: String,
    args: Vec<String>,
    _rights: WasiRights,
    compiled: Arc<crate::wasmtime_adapter::WasmtimeCompiledComponent>,
    runtime: &crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
) -> Result<ExecResult, ProgramExecError>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    HostFs: crate::HostFileSystem,
{
    use crate::{
        ComponentExecContext, ComponentExecutor, ComponentExitStatus, ComponentRuntimeFactory,
        ComponentWorld,
    };

    let started_at = exec_context.runtime_state.uptime_nanos(exec_context.cpu.now().ticks());
    let launched_instance = exec_context
        .instance_registry
        .register(name.clone(), started_at);
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
        Vec::new(),
        OutputMode::Capture,
        exec_context.read_serial,
        exec_context.write_serial,
    );

    let engine = <crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl> as ComponentRuntimeFactory<CpuImpl, HostRuntimeState<CpuImpl, HostFs>, HostFs>>::create_engine(runtime)
        .map_err(map_program_runtime_error)?;
    let executor = <crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl> as ComponentRuntimeFactory<CpuImpl, HostRuntimeState<CpuImpl, HostFs>, HostFs>>::instantiate(runtime, &engine, &compiled, ComponentWorld::Program, context)
        .map_err(map_program_runtime_error)?;

    let result = executor.run().await.map_err(map_program_runtime_error)?;

    Ok(ExecResult {
        instance_id: result.instance_id,
        exit_code: match result.status {
            ComponentExitStatus::Ok => 0,
            ComponentExitStatus::Failed => 1,
        },
        output: result.output,
    })
}

fn map_program_runtime_error(error: impl core::fmt::Display) -> ProgramExecError {
    ProgramExecError {
        kind: ProgramExecErrorKind::Internal,
        detail: format!("{error:#}"),
    }
}
