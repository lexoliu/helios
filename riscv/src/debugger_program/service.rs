use super::*;

#[derive(Clone)]
pub(crate) struct UserProgramService {
    inner: Arc<UserProgramServiceInner>,
}

struct UserProgramServiceInner {
    engine: Engine,
    compiler: ComputePool,
    compile_priority: ComputePriority,
    component_cache: Mutex<ComponentCache<Component>>,
    clock_cpu: RiscvCpu,
    debug_state: RuntimeState,
    instance_registry: InstanceRegistry,
    run_queue: ConcurrentQueue<QueuedProgram>,
    run_ready: Notify,
}

struct QueuedProgram {
    name: String,
    args: Vec<String>,
    component: Arc<Component>,
    instance: RegisteredInstance,
    output_mode: OutputMode,
    completion: Option<oneshot::Sender<Result<ExecResult, ProgramExecError>>>,
}

pub fn should_run_on(hart_id: u16, hart_count: usize, bootstrap_hart: u16) -> bool {
    assert!(
        hart_count > 1,
        "embedded debugger requires at least two processors so one can be dedicated to shell I/O"
    );
    hart_id != bootstrap_hart && hart_id == debug_processor(bootstrap_hart, hart_count)
}

pub(crate) fn install_program_service(
    cpu: &RiscvCpu,
    debug_state: &crate::debug_state::RuntimeState,
) -> Option<UserProgramService> {
    if let Some(service) = debug_state.program_service() {
        return Some(service);
    }

    let worker_count = worker_hart_count(cpu.processor_count(), cpu.bootstrap_processor().id());
    if worker_count == 0 {
        tracing::warn!(
            "program exec is unavailable: no worker harts remain after reserving the debugger hart"
        );
        return None;
    }

    let available_bytes = heap_stats().available_bytes();
    let reserved_stack_bytes = worker_count * WORKER_STACK_SIZE;
    let cache_budget =
        available_bytes.saturating_sub(reserved_stack_bytes) / COMPONENT_CACHE_FRACTION;
    let compiler_budget = available_bytes
        .saturating_sub(cache_budget)
        .max(reserved_stack_bytes);
    let engine = build_engine(cpu).unwrap_or_else(|error| {
        panic!("failed to create RISC-V component engine for user programs: {error:#}")
    });
    let compiler = ComputePool::new(worker_count, WORKER_STACK_SIZE, compiler_budget)
        .unwrap_or_else(|error| panic!("failed to create user-program compute pool: {error}"));
    let service = UserProgramService {
        inner: Arc::new(UserProgramServiceInner {
            engine,
            compiler,
            compile_priority: ComputePriority::NORMAL,
            component_cache: Mutex::new(ComponentCache::new(cache_budget)),
            clock_cpu: cpu.clone(),
            debug_state: debug_state.clone(),
            instance_registry: debug_state.instance_registry(),
            run_queue: ConcurrentQueue::unbounded(),
            run_ready: Notify::new(),
        }),
    };
    debug_state.install_program_service(service.clone());

    for hart in 0..cpu.processor_count() {
        let hart = helios_hal::cpu::ProcessorId::new(hart as u16);
        if hart != cpu.bootstrap_processor()
            && !should_run_on(
                hart.id(),
                cpu.processor_count(),
                cpu.bootstrap_processor().id(),
            )
        {
            cpu.start_processor(hart);
        }
    }

    Some(service)
}

pub(crate) fn program_worker_should_run_on(
    hart_id: u16,
    hart_count: usize,
    bootstrap_hart: u16,
) -> bool {
    hart_count > 2
        && hart_id != bootstrap_hart
        && !should_run_on(hart_id, hart_count, bootstrap_hart)
}

pub(crate) fn run_program_workers_forever(
    cpu: RiscvCpu,
    kernel: helios_kernel::Kernel<RiscvCpu>,
) -> ! {
    let debug_state = crate::global_debug_state();
    let worker_cpu = cpu.clone();
    kernel.spawn_local_detached(async move {
        let service = debug_state.wait_for_program_service().await;
        loop {
            if service.run_next_on(&worker_cpu) {
                continue;
            }
            service.wait_for_activity().await;
        }
    });
    kernel.run();
}

pub fn run_forever(cpu: RiscvCpu) -> ! {
    let debugger = embedded_debugger()
        .unwrap_or_else(|| panic!("no embedded debugger program found; set HELIOS_DEBUGGER_WASM"));
    emit_stage_marker("boot");
    tracing::info!("debugger hart: launching embedded debugger component");
    run_debugger(debugger, cpu.clone())
        .unwrap_or_else(|error| panic!("failed to exec embedded debugger component:\n{error:#}"));
    emit_stage_marker("done");
    tracing::info!("debugger hart: embedded debugger component exited cleanly");
    cpu.shutdown()
}

fn debug_processor(bootstrap_hart: u16, hart_count: usize) -> u16 {
    assert!(
        usize::from(bootstrap_hart) < hart_count,
        "bootstrap hart {} is outside detected hart count {}",
        bootstrap_hart,
        hart_count
    );

    if bootstrap_hart == 0 {
        return 1;
    }

    0
}

impl UserProgramService {
    pub(crate) async fn exec(
        &self,
        name: impl Into<String>,
        args: Vec<String>,
        wasm: &[u8],
        _rights: WasiRights,
    ) -> Result<ExecResult, ProgramExecError> {
        let name = name.into();
        let component = self.compile_component(wasm).await?;
        let started_at = monotonic_nanos(&self.inner.clock_cpu);
        let instance = self
            .inner
            .instance_registry
            .register(name.clone(), started_at);
        let (tx, rx) = oneshot::channel();
        self.inner
            .run_queue
            .push(QueuedProgram {
                name,
                args,
                component,
                instance,
                output_mode: OutputMode::Capture,
                completion: Some(tx),
            })
            .unwrap_or_else(|error| match error {
                PushError::Full(_) => unreachable!("program run queue reported full"),
                PushError::Closed(_) => panic!("program run queue was closed unexpectedly"),
            });
        self.inner.run_ready.notify_one();
        rx.await.unwrap_or_else(|_| {
            Err(ProgramExecError {
                kind: ProgramExecErrorKind::Internal,
                detail: "program worker dropped exec result before completion".to_string(),
            })
        })
    }

    pub(crate) fn run_next_on(&self, execution_cpu: &RiscvCpu) -> bool {
        match self.inner.run_queue.pop() {
            Ok(mut queued) => {
                let instance_id = queued.instance.id();
                let instance_name = queued.instance.name().to_string();
                let completion = queued.completion.take();
                let result = run_program_component(
                    queued,
                    execution_cpu.clone(),
                    self.inner.debug_state.clone(),
                    self.inner.instance_registry.clone(),
                );
                if let Some(completion) = completion {
                    let response = result
                        .as_ref()
                        .map(|(exit_code, output)| ExecResult {
                            instance_id,
                            exit_code: *exit_code,
                            output: output.clone(),
                        })
                        .map_err(|error| ProgramExecError {
                            kind: ProgramExecErrorKind::Internal,
                            detail: error.to_string(),
                        });
                    completion.send(response).unwrap_or_else(|_| {
                        panic!("program exec waiter dropped before receiving result")
                    });
                }
                match result {
                    Ok((exit_code, _output)) => {
                        tracing::info!(
                            "Program exited instance={} name={} code={}",
                            instance_id.raw(),
                            instance_name,
                            exit_code
                        );
                    }
                    Err(error) => {
                        tracing::error!(
                            "Program trapped instance={} name={} error={}",
                            instance_id.raw(),
                            instance_name,
                            error
                        );
                    }
                }
                true
            }
            Err(PopError::Empty | PopError::Closed) => self.inner.compiler.run_next(),
        }
    }

    pub(crate) async fn wait_for_activity(&self) {
        let run = self.inner.run_ready.notified();
        let compile = self.inner.compiler.wait_for_work();
        let mut run = core::pin::pin!(run);
        let mut compile = core::pin::pin!(compile);
        core::future::poll_fn(|cx| {
            if run.as_mut().poll(cx).is_ready() {
                return core::task::Poll::Ready(());
            }
            if compile.as_mut().poll(cx).is_ready() {
                return core::task::Poll::Ready(());
            }
            core::task::Poll::Pending
        })
        .await;
    }

    async fn compile_component(
        &self,
        wasm: &[u8],
    ) -> Result<Arc<Component>, ProgramExecError> {
        let started_at = monotonic_nanos(&self.inner.clock_cpu);
        if let Some(component) = self.inner.component_cache.lock().get(wasm) {
            let now = monotonic_nanos(&self.inner.clock_cpu);
            tracing::info!(
                target: "helios_riscv::program_host",
                phase = "compile-component",
                cache = "hit",
                wasm_bytes = wasm.len(),
                elapsed_ms = elapsed_millis(started_at, now),
                "program component cache hit"
            );
            return Ok(component);
        }

        let engine = self.inner.engine.clone();
        let wasm = Arc::<[u8]>::from(wasm.to_vec());
        let compiled = self
            .inner
            .compiler
            .spawn(self.inner.compile_priority, {
                let wasm = wasm.clone();
                move || Component::from_binary(&engine, &wasm)
            })
            .await
            .map_err(|error| ProgramExecError {
                kind: ProgramExecErrorKind::QueueSaturated,
                detail: error.to_string(),
            })?
            .map_err(|error| ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: error.to_string(),
            })?;

        let component = Arc::new(compiled);
        let now = monotonic_nanos(&self.inner.clock_cpu);
        tracing::info!(
            target: "helios_riscv::program_host",
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

fn worker_hart_count(hart_count: usize, bootstrap_hart: u16) -> usize {
    (0..hart_count)
        .filter(|hart| {
            let hart = *hart as u16;
            program_worker_should_run_on(hart, hart_count, bootstrap_hart)
        })
        .count()
}

fn run_program_component(
    queued: QueuedProgram,
    cpu: RiscvCpu,
    debug_state: RuntimeState,
    instance_registry: InstanceRegistry,
) -> Result<(u32, ExecOutput), wasmtime::Error> {
    let QueuedProgram {
        name,
        args,
        component,
        instance,
        output_mode,
        completion: _,
    } = queued;
    let filesystem = crate::debugger_wasi::DebugFileSystem::new(debug_state.clone());
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(name);
    argv.extend(args);
    let store_started_at = monotonic_nanos(&cpu);
    let linker = component_linker(component.engine(), SystemWorld::Program)?;

    let mut store = store_with_state(
        component.engine(),
        StoreData::new(
            cpu,
            debug_state,
            instance_registry,
            instance,
            None,
            filesystem,
            argv,
            Vec::new(),
            output_mode,
            write_serial
        ),
    );
    tracing::info!(
        target: "helios_riscv::program_host",
        phase = "prepare-store",
        instance = store.data().instance.id().raw(),
        elapsed_ms = elapsed_millis(store_started_at, monotonic_nanos(&store.data().cpu)),
        "program store prepared"
    );

    let instantiate_started_at = monotonic_nanos(&store.data().cpu);
    let program = helios_kernel::block_on(crate::program_bindings::bindings::Init::instantiate_async(
        &mut store, &component, &linker,
    ))?;
    tracing::info!(
        target: "helios_riscv::program_host",
        phase = "instantiate",
        instance = store.data().instance.id().raw(),
        elapsed_ms = elapsed_millis(instantiate_started_at, monotonic_nanos(&store.data().cpu)),
        "program component instantiated"
    );
    let run_started_at = monotonic_nanos(&store.data().cpu);
    let result =
        helios_kernel::block_on(store.run_concurrent(async move |accessor| {
            program.wasi_cli_run().call_run(accessor).await
        }))?;
    tracing::info!(
        target: "helios_riscv::program_host",
        phase = "call-run",
        instance = store.data().instance.id().raw(),
        elapsed_ms = elapsed_millis(run_started_at, monotonic_nanos(&store.data().cpu)),
        "program call_run completed"
    );
    let exit_code = match result {
        Ok(Ok(())) => 0,
        Ok(Err(())) => 1,
        Err(error) => return Err(error),
    };
    let output = store.data().take_captured_output();
    Ok((exit_code, output))
}
