extern crate alloc;

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use futures::channel::oneshot;
use helios_hal::cpu::Cpu;
use helios_hal::resource::WasiRights;
use helios_kernel::{
    ComputePool, ComputePriority, InstanceId, InstanceRegistry, Notify, ProgramExecError,
    ProgramExecErrorKind, RegisteredInstance, heap_stats,
};
use lru::LruCache;
use spin::Mutex;
use wasmtime::Engine;
use wasmtime::component::{Component, ResourceTable};

use crate::debugger_program::{self, OutputMode, StoreData};
use crate::{RiscvCpu, debug_state::RuntimeState, program_bindings};

const WORKER_STACK_SIZE: usize = 256 * 1024;
const COMPONENT_CACHE_FRACTION: usize = 8;

#[derive(Clone, Debug)]
pub(crate) struct ExecOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct ExecResult {
    pub(crate) instance_id: InstanceId,
    pub(crate) exit_code: u32,
    pub(crate) output: ExecOutput,
}

#[derive(Clone)]
pub(crate) struct UserProgramService {
    inner: Arc<UserProgramServiceInner>,
}

struct UserProgramServiceInner {
    engine: Engine,
    compiler: ComputePool,
    compile_priority: ComputePriority,
    component_cache: Mutex<ComponentCache>,
    timebase_frequency: u64,
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

struct ComponentCache {
    budget_bytes: usize,
    resident_bytes: usize,
    entries: LruCache<Arc<[u8]>, Arc<Component>>,
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
    let engine = debugger_program::build_engine().unwrap_or_else(|error| {
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
            timebase_frequency: cpu.timer_frequency(),
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
            && !crate::debugger_program::should_run_on(
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

pub(crate) fn should_run_on(hart_id: u16, hart_count: usize, bootstrap_hart: u16) -> bool {
    hart_count > 2
        && hart_id != bootstrap_hart
        && !crate::debugger_program::should_run_on(hart_id, hart_count, bootstrap_hart)
}

pub(crate) fn run_forever(cpu: RiscvCpu, kernel: helios_kernel::Kernel<RiscvCpu>) -> ! {
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
        let started_at = monotonic_nanos(self.inner.timebase_frequency);
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
                let result = run_component(
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

    async fn compile_component(&self, wasm: &[u8]) -> Result<Arc<Component>, ProgramExecError> {
        let started_at = monotonic_nanos(self.inner.timebase_frequency);
        if let Some(component) = self.inner.component_cache.lock().get(wasm) {
            tracing::info!(
                target: "helios_riscv::program_host",
                phase = "compile-component",
                cache = "hit",
                wasm_bytes = wasm.len(),
                elapsed_ms = elapsed_millis(started_at, self.inner.timebase_frequency),
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
        tracing::info!(
            target: "helios_riscv::program_host",
            phase = "compile-component",
            cache = "miss",
            wasm_bytes = wasm.len(),
            elapsed_ms = elapsed_millis(started_at, self.inner.timebase_frequency),
            "program component compiled"
        );
        Ok(self
            .inner
            .component_cache
            .lock()
            .insert_if_missing(wasm, component))
    }
}

impl ComponentCache {
    fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            resident_bytes: 0,
            entries: LruCache::unbounded(),
        }
    }

    fn get(&mut self, wasm: &[u8]) -> Option<Arc<Component>> {
        self.entries.get(wasm).cloned()
    }

    fn insert_if_missing(&mut self, wasm: Arc<[u8]>, component: Arc<Component>) -> Arc<Component> {
        if let Some(existing) = self.entries.get(wasm.as_ref()).cloned() {
            return existing;
        }

        self.resident_bytes = self
            .resident_bytes
            .checked_add(wasm.len())
            .expect("component cache byte accounting overflow");
        let replaced = self.entries.put(wasm, component.clone());
        assert!(
            replaced.is_none(),
            "component cache replaced an entry after miss revalidation"
        );
        self.evict_to_budget();
        component
    }

    fn evict_to_budget(&mut self) {
        while self.resident_bytes > self.budget_bytes {
            let Some((wasm, _component)) = self.entries.pop_lru() else {
                panic!("component cache accounting lost track of resident bytes");
            };
            self.resident_bytes = self
                .resident_bytes
                .checked_sub(wasm.len())
                .expect("component cache byte accounting underflow");
        }
    }
}

fn worker_hart_count(hart_count: usize, bootstrap_hart: u16) -> usize {
    (0..hart_count)
        .filter(|hart| {
            let hart = *hart as u16;
            should_run_on(hart, hart_count, bootstrap_hart)
        })
        .count()
}

fn run_component(
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
    let timebase_frequency = cpu.timer_frequency();
    let store_started_at = monotonic_nanos(timebase_frequency);
    let linker = debugger_program::component_linker(
        component.engine(),
        debugger_program::SystemWorld::Program,
    )?;

    let mut store = debugger_program::store_with_state(
        component.engine(),
        StoreData {
            table: ResourceTable::new(),
            cpu,
            debug_state,
            instance_registry,
            instance,
            debug_port: None,
            filesystem,
            arguments: argv,
            environment: Vec::new(),
            output_mode,
            captured_stdout: Arc::new(Mutex::new(Vec::new())),
            captured_stderr: Arc::new(Mutex::new(Vec::new())),
        },
    );
    tracing::info!(
        target: "helios_riscv::program_host",
        phase = "prepare-store",
        instance = store.data().instance.id().raw(),
        elapsed_ms = elapsed_millis(store_started_at, timebase_frequency),
        "program store prepared"
    );

    let instantiate_started_at = monotonic_nanos(timebase_frequency);
    let program = debugger_program::block_on(program_bindings::bindings::Init::instantiate_async(
        &mut store, &component, &linker,
    ))?;
    tracing::info!(
        target: "helios_riscv::program_host",
        phase = "instantiate",
        instance = store.data().instance.id().raw(),
        elapsed_ms = elapsed_millis(instantiate_started_at, timebase_frequency),
        "program component instantiated"
    );
    let run_started_at = monotonic_nanos(timebase_frequency);
    let result =
        debugger_program::block_on(store.run_concurrent(async move |accessor| {
            program.wasi_cli_run().call_run(accessor).await
        }))?;
    tracing::info!(
        target: "helios_riscv::program_host",
        phase = "call-run",
        instance = store.data().instance.id().raw(),
        elapsed_ms = elapsed_millis(run_started_at, timebase_frequency),
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

fn monotonic_nanos(timebase_frequency: u64) -> u64 {
    let ticks = riscv::register::time::read64();
    ticks.saturating_mul(1_000_000_000) / timebase_frequency
}

fn elapsed_millis(started_at: u64, timebase_frequency: u64) -> u64 {
    monotonic_nanos(timebase_frequency).saturating_sub(started_at) / 1_000_000
}
