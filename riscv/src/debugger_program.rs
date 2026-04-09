extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::pin::Pin;
use core::task::{Context, Poll};

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use futures::channel::oneshot;
use helios_hal::cpu::Cpu;
use helios_hal::resource::WasiRights;
use helios_kernel::{
    ComponentCache, ComputePool, ComputePriority, EmbeddedDebugger, ExecOutput, ExecResult,
    InstanceExecutionTransition, InstanceRegistry, Notify, ProgramExecError, ProgramExecErrorKind,
    RawMutex, RawMutexGuardResource, RawMutexResource, RawRwLock, RawRwLockReadGuardResource,
    RawRwLockResource, RawRwLockWriteGuardResource, RegisteredInstance, SerialPortResource,
    TcpStreamResource, build_target_engine_config, elapsed_millis, embedded_debugger, heap_stats,
    monotonic_nanos,
};
use spin::Mutex;
use thiserror::Error;
use wasmtime::component::{
    Accessor, Destination, HasSelf, Linker, Resource, ResourceTable, ResourceType, StreamProducer,
    StreamReader, StreamResult,
};
use wasmtime::{
    CallHook, CustomCodeMemory, Engine, OptLevel, RegallocAlgorithm, ResourceLimiter, Store,
    StoreContextMut,
};
use wasmtime_wasi_io::IoView;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{OutputStream, StreamError};

use crate::debug_state::RuntimeState;
use crate::debugger_bindings::bindings;
use crate::{RiscvCpu, try_read_debug_serial_byte, write_debug_serial_bytes};
use helios_kernel::{StatsSample, TraceEvent, TraceField, TraceFilter, TraceLevel, TraceValue};

const WASMTIME_TARGET: &str = "riscv64gc-unknown-none-elf";
const SERIAL_INSTANCE: &str = "helios:system/serial@0.1.0";
const SYNC_INSTANCE: &str = "helios:system/sync@0.1.0";
const STATS_INSTANCE: &str = "helios:system/stats@0.1.0";
const PROGRAMS_INSTANCE: &str = "helios:system/programs@0.1.0";
const NET_INSTANCE: &str = "helios:system/net@0.1.0";
const TRACING_INSTANCE: &str = "helios:system/tracing@0.1.0";
const INSTANCES_INSTANCE: &str = "helios:system/instances@0.1.0";
const WORKER_STACK_SIZE: usize = 256 * 1024;
const COMPONENT_CACHE_FRACTION: usize = 8;

struct RiscvCodeMemory;

impl CustomCodeMemory for RiscvCodeMemory {
    fn required_alignment(&self) -> usize {
        1
    }

    fn publish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime::Result<()> {
        unsafe {
            core::arch::asm!("fence.i", options(nostack, preserves_flags));
        }
        Ok(())
    }

    fn unpublish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime::Result<()> {
        Ok(())
    }
}

pub struct SbiSerialPort {
    _resource: SerialPortResource,
}

pub struct SbiTcpStream {
    resource: TcpStreamResource<RiscvTcpStream>,
}

impl SbiTcpStream {
    fn new(backend: RiscvTcpStream) -> Self {
        Self {
            resource: TcpStreamResource::new(backend),
        }
    }
}

#[derive(Clone)]
pub struct RiscvTcpStream {
    service: crate::net::NetworkService,
    stream: crate::net::TcpStreamId,
}
#[derive(Clone)]
struct GuestOutput {
    mode: OutputMode,
    debug_state: RuntimeState,
    cpu: RiscvCpu,
    captured_stdout: Arc<Mutex<Vec<u8>>>,
    captured_stderr: Arc<Mutex<Vec<u8>>>,
}
pub(crate) struct DebugSerialOutputStream {
    sink: GuestOutput,
    stream: OutputStreamKind,
}
pub(crate) struct DeadlinePollable {
    pub(crate) cpu: RiscvCpu,
    pub(crate) deadline_nanos: u64,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputMode {
    Serial,
    #[allow(dead_code)]
    Trace,
    Capture,
}

#[derive(Clone, Copy)]
pub(crate) enum SystemWorld {
    Debugger,
    Program,
}

pub struct SbiRawMutex {
    resource: RawMutexResource,
}

pub struct SbiRawMutexGuard {
    _resource: RawMutexGuardResource,
}

pub struct SbiRawRwLock {
    resource: RawRwLockResource,
}

pub struct SbiRawRwLockReadGuard {
    _resource: RawRwLockReadGuardResource,
}

pub struct SbiRawRwLockWriteGuard {
    _resource: RawRwLockWriteGuardResource,
}

#[derive(Clone)]
pub(crate) struct UserProgramService {
    inner: Arc<UserProgramServiceInner>,
}

struct UserProgramServiceInner {
    engine: Engine,
    compiler: ComputePool,
    compile_priority: ComputePriority,
    component_cache: Mutex<ComponentCache<wasmtime::component::Component>>,
    clock_cpu: RiscvCpu,
    debug_state: RuntimeState,
    instance_registry: InstanceRegistry,
    run_queue: ConcurrentQueue<QueuedProgram>,
    run_ready: Notify,
}

struct QueuedProgram {
    name: String,
    args: Vec<String>,
    component: Arc<wasmtime::component::Component>,
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
    let engine = build_engine().unwrap_or_else(|error| {
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
    ) -> Result<Arc<wasmtime::component::Component>, ProgramExecError> {
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
                move || wasmtime::component::Component::from_binary(&engine, &wasm)
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

fn run_debugger(debugger: EmbeddedDebugger, cpu: RiscvCpu) -> Result<(), DebuggerError> {
    emit_stage_marker("engine:new");
    tracing::info!("debugger hart: creating wasmtime engine");
    let engine = build_engine().map_err(DebuggerError::CreateEngine)?;
    emit_stage_marker("engine:ok");
    tracing::info!("debugger hart: compiling embedded debugger component");
    let component =
        wasmtime::component::Component::from_binary(&engine, debugger.component().bytes())
            .map_err(DebuggerError::CompileComponent)?;
    emit_stage_marker("component:ok");

    emit_stage_marker("linker:new");
    tracing::info!("debugger hart: preparing component linker");
    let linker =
        component_linker(&engine, SystemWorld::Debugger).map_err(DebuggerError::LinkComponent)?;
    emit_stage_marker("linker:ok");

    emit_stage_marker("store:new");
    let debug_state = cpu.debug_state();
    let filesystem = crate::debugger_wasi::DebugFileSystem::new(debug_state.clone());
    let instance_registry = cpu.instance_registry();
    let instance =
        instance_registry.register("debugger", debug_state.uptime_nanos(cpu.now().ticks()));
    let mut store = store_with_state(
        &engine,
        StoreData {
            table: ResourceTable::new(),
            debug_state,
            instance_registry,
            instance,
            cpu,
            debug_port: Some(()),
            filesystem,
            arguments: Vec::new(),
            environment: Vec::new(),
            output_mode: OutputMode::Serial,
            captured_stdout: Arc::new(Mutex::new(Vec::new())),
            captured_stderr: Arc::new(Mutex::new(Vec::new())),
        },
    );
    emit_stage_marker("store:ok");

    emit_stage_marker("pre:begin");
    tracing::info!("debugger hart: instantiating embedded debugger component");
    if let Err(error) = linker.instantiate_pre(&component) {
        emit_error_marker("pre:error", &format!("{error:#}"));
        log_wasmtime_error_chain("debugger hart: instantiate_pre failed", &error);
        return Err(DebuggerError::InstantiateComponent(error));
    }
    emit_stage_marker("pre:ok");
    emit_stage_marker("instantiate:begin");
    let instance = helios_kernel::block_on(bindings::Debugger::instantiate_async(
        &mut store, &component, &linker,
    ))
    .map_err(|error| {
        emit_error_marker("instantiate:error", &format!("{error:#}"));
        DebuggerError::InstantiateComponent(error)
    })?;
    emit_stage_marker("instantiate:ok");
    tracing::info!("debugger hart: entering wasi:cli/run");
    emit_stage_marker("run:begin");
    let result =
        helios_kernel::block_on(store.run_concurrent(async move |accessor| {
            instance.wasi_cli_run().call_run(accessor).await
        }))
        .map_err(|error| {
            emit_error_marker("run:error", &format!("{error:#}"));
            DebuggerError::RunComponent(error)
        })?;
    emit_stage_marker("run:ok");
    tracing::info!("debugger hart: wasi:cli/run returned");
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(())) => Err(DebuggerError::GuestFailed),
        Err(error) => Err(DebuggerError::RunComponent(error)),
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

pub(crate) fn build_engine() -> wasmtime::Result<Engine> {
    let mut config = build_target_engine_config(WASMTIME_TARGET);
    config.cranelift_opt_level(OptLevel::None);
    config.cranelift_regalloc_algorithm(RegallocAlgorithm::SinglePass);
    config.cranelift_debug_verifier(false);
    config.with_custom_code_memory(Some(Arc::new(RiscvCodeMemory)));
    config.signals_based_traps(false);
    config.memory_guard_size(0);
    config.memory_reservation(0);
    config.memory_reservation_for_growth(1 << 20);
    config.memory_init_cow(false);
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config)
}

pub(crate) fn component_linker(
    engine: &Engine,
    world: SystemWorld,
) -> wasmtime::Result<Linker<StoreData>> {
    let mut linker = Linker::<StoreData>::new(engine);
    wasmtime_wasi_io::add_to_linker_async(&mut linker)?;
    crate::debugger_wasi_p2::add_to_linker(&mut linker)?;
    crate::debugger_wasi::add_to_linker(&mut linker)?;
    add_serial_to_linker(&mut linker)?;
    add_sync_to_linker(&mut linker)?;
    match world {
        SystemWorld::Debugger => add_system_to_linker(&mut linker)?,
        SystemWorld::Program => add_program_world_to_linker(&mut linker)?,
    }
    Ok(linker)
}

pub(crate) fn store_with_state(engine: &Engine, state: StoreData) -> Store<StoreData> {
    let mut store = Store::new(engine, state);
    store.limiter(|state| state);
    store.call_hook(|mut caller: StoreContextMut<'_, StoreData>, hook| {
        caller.data_mut().record_call_hook(hook);
        Ok(())
    });
    store
}

fn add_system_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    add_programs_to_linker(linker)?;
    add_net_to_linker(linker)?;
    add_stats_to_linker(linker)?;
    add_instances_to_linker(linker)?;
    add_tracing_to_linker(linker)?;
    Ok(())
}

pub(crate) fn add_program_world_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    add_programs_to_program_linker(linker)?;
    add_net_to_program_linker(linker)?;
    add_stats_to_program_linker(linker)?;
    add_tracing_to_program_linker(linker)?;
    Ok(())
}

pub(crate) fn add_serial_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance(SERIAL_INSTANCE)?;
    instance.resource_concurrent(
        "serial-port",
        ResourceType::host::<SbiSerialPort>(),
        |accessor, rep| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let resource = Resource::<SbiSerialPort>::new_own(rep);
                    let _ = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.func_wrap(
        "debug-port",
        |mut caller: StoreContextMut<'_, StoreData>, (): ()| {
            let resource = match caller.data().debug_port {
                Some(()) => Some(caller.data_mut().table.push(SbiSerialPort {
                    _resource: SerialPortResource,
                })?),
                None => None,
            };
            Ok((resource,))
        },
    )?;
    instance.func_wrap(
        "[method]serial-port.rights",
        |mut caller: StoreContextMut<'_, StoreData>, (resource,): (Resource<SbiSerialPort>,)| {
            let _ = caller.data_mut().table.get(&resource)?;
            Ok((bindings::helios::system::serial::SerialRights::READ
                | bindings::helios::system::serial::SerialRights::WRITE
                | bindings::helios::system::serial::SerialRights::FLUSH,))
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]serial-port.read",
        |accessor: &Accessor<StoreData>, (resource, max_bytes): (Resource<SbiSerialPort>, u32)| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let _ = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((read_serial(max_bytes),))
                })
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]serial-port.write",
        |accessor: &Accessor<StoreData>, (resource, bytes): (Resource<SbiSerialPort>, Vec<u8>)| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let _ = access.get().table.get(&resource)?;
                    write_serial(&bytes);
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]serial-port.flush",
        |accessor: &Accessor<StoreData>, (resource,): (Resource<SbiSerialPort>,)| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let _ = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    Ok(())
}

pub(crate) fn add_sync_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance(SYNC_INSTANCE)?;
    instance.resource_concurrent(
        "raw-mutex",
        ResourceType::host::<SbiRawMutex>(),
        |accessor, rep| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let resource = Resource::<SbiRawMutex>::new_own(rep);
                    let _ = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.resource_concurrent(
        "raw-mutex-guard",
        ResourceType::host::<SbiRawMutexGuard>(),
        |accessor, rep| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let resource = Resource::<SbiRawMutexGuard>::new_own(rep);
                    let _ = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.resource_concurrent(
        "raw-rw-lock",
        ResourceType::host::<SbiRawRwLock>(),
        |accessor, rep| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let resource = Resource::<SbiRawRwLock>::new_own(rep);
                    let _ = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.resource_concurrent(
        "raw-rw-lock-read-guard",
        ResourceType::host::<SbiRawRwLockReadGuard>(),
        |accessor, rep| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let resource = Resource::<SbiRawRwLockReadGuard>::new_own(rep);
                    let _ = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.resource_concurrent(
        "raw-rw-lock-write-guard",
        ResourceType::host::<SbiRawRwLockWriteGuard>(),
        |accessor, rep| {
            Box::pin(async move {
                accessor.with(|mut access| {
                    let resource = Resource::<SbiRawRwLockWriteGuard>::new_own(rep);
                    let _ = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(())
                })
            })
        },
    )?;
    instance.func_wrap(
        "[constructor]raw-mutex",
        |mut caller: StoreContextMut<'_, StoreData>, (): ()| {
            let resource = caller.data_mut().table.push(SbiRawMutex {
                resource: RawMutexResource {
                    inner: Arc::new(RawMutex::new()),
                },
            })?;
            Ok((resource,))
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]raw-mutex.lock",
        |accessor: &Accessor<StoreData>, (resource,): (Resource<SbiRawMutex>,)| {
            Box::pin(async move {
                let mutex = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(
                        access.get().table.get(&resource)?.resource.inner.clone(),
                    )
                })?;
                let lease = mutex.lock_owned().await;
                accessor.with(|mut access| {
                    let guard = access.get().table.push(SbiRawMutexGuard {
                        _resource: RawMutexGuardResource { _lease: lease },
                    })?;
                    Ok::<_, wasmtime::Error>((guard,))
                })
            })
        },
    )?;
    instance.func_wrap(
        "[constructor]raw-rw-lock",
        |mut caller: StoreContextMut<'_, StoreData>, (): ()| {
            let resource = caller.data_mut().table.push(SbiRawRwLock {
                resource: RawRwLockResource {
                    inner: Arc::new(RawRwLock::new()),
                },
            })?;
            Ok((resource,))
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]raw-rw-lock.read",
        |accessor: &Accessor<StoreData>, (resource,): (Resource<SbiRawRwLock>,)| {
            Box::pin(async move {
                let rwlock = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(
                        access.get().table.get(&resource)?.resource.inner.clone(),
                    )
                })?;
                let lease = rwlock.read_owned().await;
                accessor.with(|mut access| {
                    let guard = access.get().table.push(SbiRawRwLockReadGuard {
                        _resource: RawRwLockReadGuardResource { _lease: lease },
                    })?;
                    Ok::<_, wasmtime::Error>((guard,))
                })
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]raw-rw-lock.write",
        |accessor: &Accessor<StoreData>, (resource,): (Resource<SbiRawRwLock>,)| {
            Box::pin(async move {
                let rwlock = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(
                        access.get().table.get(&resource)?.resource.inner.clone(),
                    )
                })?;
                let lease = rwlock.write_owned().await;
                accessor.with(|mut access| {
                    let guard = access.get().table.push(SbiRawRwLockWriteGuard {
                        _resource: RawRwLockWriteGuardResource { _lease: lease },
                    })?;
                    Ok::<_, wasmtime::Error>((guard,))
                })
            })
        },
    )?;
    Ok(())
}

fn add_stats_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance(STATS_INSTANCE)?;
    instance.func_wrap("snapshot", |caller, (): ()| {
        Ok((snapshot_sample(caller.data()),))
    })?;
    instance.func_wrap("subscribe", |mut caller, (period,): (u64,)| {
        let reader = StreamReader::new(&mut caller, StatsStreamProducer::new(period))?;
        Ok((reader,))
    })?;
    Ok(())
}

fn add_programs_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance(PROGRAMS_INSTANCE)?;
    // `programs.exec` is intentionally a synchronous WIT call. The guest
    // blocks on exec completion while the host internally awaits the
    // compile/queue path on Wasmtime's async fiber.
    instance.func_wrap_async(
        "exec",
        |caller: StoreContextMut<'_, StoreData>,
         (request,): (bindings::helios::system::programs::ExecRequest,)| {
            let service = caller.data().debug_state.program_service().or_else(|| {
                install_program_service(&caller.data().cpu, &caller.data().debug_state)
            });
            Box::new(async move {
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(
                        bindings::helios::system::programs::ExecError {
                            kind: bindings::helios::system::programs::ExecErrorKind::Unavailable,
                            detail: "program exec is unavailable on this machine".to_owned(),
                        },
                    ),));
                };
                let response = service
                    .exec(
                        request.name,
                        request.args,
                        &request.wasm,
                        WasiRights::empty(),
                    )
                    .await
                    .map(|result| bindings::helios::system::programs::ExecResult {
                        instance_id: result.instance_id.raw(),
                        exit_code: result.exit_code,
                        output: bindings::helios::system::programs::ExecOutput {
                            stdout: result.output.stdout,
                            stderr: result.output.stderr,
                        },
                    })
                    .map_err(convert_launch_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    Ok(())
}

fn add_programs_to_program_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance(PROGRAMS_INSTANCE)?;
    instance.func_wrap_async(
        "exec",
        |caller: StoreContextMut<'_, StoreData>,
         (request,): (crate::program_bindings::bindings::helios::system::programs::ExecRequest,)| {
            let service = caller.data().debug_state.program_service().or_else(|| {
                install_program_service(
                    &caller.data().cpu,
                    &caller.data().debug_state,
                )
            });
            Box::new(async move {
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(
                        crate::program_bindings::bindings::helios::system::programs::ExecError {
                            kind: crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::Unavailable,
                            detail: "program exec is unavailable on this machine".to_owned(),
                        },
                    ),));
                };
                let response = service
                    .exec(
                        request.name,
                        request.args,
                        &request.wasm,
                        WasiRights::empty(),
                    )
                    .await
                    .map(|result| crate::program_bindings::bindings::helios::system::programs::ExecResult {
                        instance_id: result.instance_id.raw(),
                        exit_code: result.exit_code,
                        output: crate::program_bindings::bindings::helios::system::programs::ExecOutput {
                            stdout: result.output.stdout,
                            stderr: result.output.stderr,
                        },
                    })
                    .map_err(convert_program_launch_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    Ok(())
}

fn add_net_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance(NET_INSTANCE)?;
    instance.resource_concurrent(
        "tcp-stream",
        ResourceType::host::<SbiTcpStream>(),
        |accessor, rep| {
            Box::pin(async move {
                let stream = accessor.with(|mut access| {
                    let resource = Resource::<SbiTcpStream>::new_own(rep);
                    let stream = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(stream)
                })?;
                stream
                    .resource
                    .backend
                    .service
                    .tcp_close(stream.resource.backend.stream)
                    .await;
                Ok::<_, wasmtime::Error>(())
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "ping",
        |accessor: &Accessor<StoreData>, (host, timeout): (String, u64)| {
            Box::pin(async move {
                let service = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().debug_state.network_service())
                })?;
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(
                        bindings::helios::system::net::PingError {
                            kind: bindings::helios::system::net::PingErrorKind::Unavailable,
                            detail: "network service is unavailable on this machine".to_owned(),
                        },
                    ),));
                };
                let response = service
                    .ping(&host, timeout)
                    .await
                    .map(convert_ping_reply)
                    .map_err(convert_ping_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "tcp-connect",
        |accessor: &Accessor<StoreData>, (host, port, timeout): (String, u16, u64)| {
            Box::pin(async move {
                let service = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().debug_state.network_service())
                })?;
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(unavailable_tcp_error()),));
                };
                let connected = service.tcp_connect(&host, port, timeout).await;
                let response = match connected {
                    Ok(stream) => {
                        let resource = accessor.with(|mut access| {
                            access.get().table.push(SbiTcpStream::new(RiscvTcpStream {
                                service: service.clone(),
                                stream,
                            }))
                        })?;
                        Ok(resource)
                    }
                    Err(error) => Err(convert_tcp_error(error)),
                };
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.read",
        |accessor: &Accessor<StoreData>,
         (resource, max_bytes, timeout): (Resource<SbiTcpStream>, u32, u64)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.stream,
                    ))
                })?;
                let response = socket
                    .0
                    .tcp_read(socket.1, max_bytes, timeout)
                    .await
                    .map_err(convert_tcp_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.write",
        |accessor: &Accessor<StoreData>,
         (resource, bytes, timeout): (Resource<SbiTcpStream>, Vec<u8>, u64)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.stream,
                    ))
                })?;
                let response = socket
                    .0
                    .tcp_write_all(socket.1, &bytes, timeout)
                    .await
                    .map(|()| bytes.len() as u64)
                    .map_err(convert_tcp_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.close",
        |accessor: &Accessor<StoreData>, (resource,): (Resource<SbiTcpStream>,)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.stream,
                    ))
                })?;
                socket.0.tcp_close(socket.1).await;
                Ok::<_, wasmtime::Error>((Ok::<(), bindings::helios::system::net::TcpError>(()),))
            })
        },
    )?;
    Ok(())
}

fn add_net_to_program_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance(NET_INSTANCE)?;
    instance.resource_concurrent(
        "tcp-stream",
        ResourceType::host::<SbiTcpStream>(),
        |accessor, rep| {
            Box::pin(async move {
                let stream = accessor.with(|mut access| {
                    let resource = Resource::<SbiTcpStream>::new_own(rep);
                    let stream = access.get().table.delete(resource)?;
                    Ok::<_, wasmtime::Error>(stream)
                })?;
                stream
                    .resource
                    .backend
                    .service
                    .tcp_close(stream.resource.backend.stream)
                    .await;
                Ok::<_, wasmtime::Error>(())
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "ping",
        |accessor: &Accessor<StoreData>, (host, timeout): (String, u64)| {
            Box::pin(async move {
                let service = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().debug_state.network_service())
                })?;
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(
                        crate::program_bindings::bindings::helios::system::net::PingError {
                            kind: crate::program_bindings::bindings::helios::system::net::PingErrorKind::Unavailable,
                            detail: "network service is unavailable on this machine".to_owned(),
                        },
                    ),));
                };
                let response = service
                    .ping(&host, timeout)
                    .await
                    .map(convert_program_ping_reply)
                    .map_err(convert_program_ping_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "tcp-connect",
        |accessor: &Accessor<StoreData>, (host, port, timeout): (String, u16, u64)| {
            Box::pin(async move {
                let service = accessor.with(|mut access| {
                    Ok::<_, wasmtime::Error>(access.get().debug_state.network_service())
                })?;
                let Some(service) = service else {
                    return Ok::<_, wasmtime::Error>((Err(unavailable_program_tcp_error()),));
                };
                let connected = service.tcp_connect(&host, port, timeout).await;
                let response = match connected {
                    Ok(stream) => {
                        let resource = accessor.with(|mut access| {
                            access.get().table.push(SbiTcpStream::new(RiscvTcpStream {
                                service: service.clone(),
                                stream,
                            }))
                        })?;
                        Ok(resource)
                    }
                    Err(error) => Err(convert_program_tcp_error(error)),
                };
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.read",
        |accessor: &Accessor<StoreData>,
         (resource, max_bytes, timeout): (Resource<SbiTcpStream>, u32, u64)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.stream,
                    ))
                })?;
                let response = socket
                    .0
                    .tcp_read(socket.1, max_bytes, timeout)
                    .await
                    .map_err(convert_program_tcp_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.write",
        |accessor: &Accessor<StoreData>,
         (resource, bytes, timeout): (Resource<SbiTcpStream>, Vec<u8>, u64)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.stream,
                    ))
                })?;
                let response = socket
                    .0
                    .tcp_write_all(socket.1, &bytes, timeout)
                    .await
                    .map(|()| bytes.len() as u64)
                    .map_err(convert_program_tcp_error);
                Ok::<_, wasmtime::Error>((response,))
            })
        },
    )?;
    instance.func_wrap_concurrent(
        "[method]tcp-stream.close",
        |accessor: &Accessor<StoreData>, (resource,): (Resource<SbiTcpStream>,)| {
            Box::pin(async move {
                let socket = accessor.with(|mut access| {
                    let socket = access.get().table.get(&resource)?;
                    Ok::<_, wasmtime::Error>((
                        socket.resource.backend.service.clone(),
                        socket.resource.backend.stream,
                    ))
                })?;
                socket.0.tcp_close(socket.1).await;
                Ok::<_, wasmtime::Error>((Ok::<
                    (),
                    crate::program_bindings::bindings::helios::system::net::TcpError,
                >(()),))
            })
        },
    )?;
    Ok(())
}

fn add_tracing_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance(TRACING_INSTANCE)?;
    instance.func_wrap(
        "recent",
        |caller, (filter, limit): (bindings::helios::system::tracing::Filter, u32)| {
            let filter = convert_filter(filter);
            let events = caller
                .data()
                .debug_state
                .recent(&filter, limit)
                .into_iter()
                .map(convert_event)
                .collect::<Vec<_>>();
            Ok((events,))
        },
    )?;
    instance.func_wrap(
        "subscribe",
        |mut caller, (filter,): (bindings::helios::system::tracing::Filter,)| {
            let reader = StreamReader::new(
                &mut caller,
                TracingStreamProducer::new(convert_filter(filter)),
            )?;
            Ok((reader,))
        },
    )?;
    Ok(())
}

fn add_stats_to_program_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance(STATS_INSTANCE)?;
    instance.func_wrap("snapshot", |caller, (): ()| {
        Ok((snapshot_program_sample(caller.data()),))
    })?;
    instance.func_wrap("subscribe", |mut caller, (period,): (u64,)| {
        let reader = StreamReader::new(&mut caller, ProgramStatsStreamProducer::new(period))?;
        Ok((reader,))
    })?;
    Ok(())
}

fn add_tracing_to_program_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance(TRACING_INSTANCE)?;
    instance.func_wrap(
        "recent",
        |caller,
         (filter, limit): (
            crate::program_bindings::bindings::helios::system::tracing::Filter,
            u32,
        )| {
            let filter = convert_program_filter(filter);
            let events = caller
                .data()
                .debug_state
                .recent(&filter, limit)
                .into_iter()
                .map(convert_program_event)
                .collect::<Vec<_>>();
            Ok((events,))
        },
    )?;
    instance.func_wrap(
        "subscribe",
        |mut caller,
         (filter,): (crate::program_bindings::bindings::helios::system::tracing::Filter,)| {
            let reader = StreamReader::new(
                &mut caller,
                ProgramTracingStreamProducer::new(convert_program_filter(filter)),
            )?;
            Ok((reader,))
        },
    )?;
    Ok(())
}

fn convert_launch_error(
    error: helios_kernel::ProgramExecError,
) -> bindings::helios::system::programs::ExecError {
    bindings::helios::system::programs::ExecError {
        kind: match error.kind {
            ProgramExecErrorKind::InvalidBinary => {
                bindings::helios::system::programs::ExecErrorKind::InvalidBinary
            }
            ProgramExecErrorKind::MissingEntry => {
                bindings::helios::system::programs::ExecErrorKind::MissingEntry
            }
            ProgramExecErrorKind::UnsupportedImport => {
                bindings::helios::system::programs::ExecErrorKind::UnsupportedImport
            }
            ProgramExecErrorKind::QueueSaturated => {
                bindings::helios::system::programs::ExecErrorKind::QueueSaturated
            }
            ProgramExecErrorKind::Unavailable => {
                bindings::helios::system::programs::ExecErrorKind::Unavailable
            }
            ProgramExecErrorKind::Internal => {
                bindings::helios::system::programs::ExecErrorKind::Internal
            }
        },
        detail: error.detail,
    }
}

fn convert_program_launch_error(
    error: helios_kernel::ProgramExecError,
) -> crate::program_bindings::bindings::helios::system::programs::ExecError {
    crate::program_bindings::bindings::helios::system::programs::ExecError {
        kind: match error.kind {
            ProgramExecErrorKind::InvalidBinary => {
                crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::InvalidBinary
            }
            ProgramExecErrorKind::MissingEntry => {
                crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::MissingEntry
            }
            ProgramExecErrorKind::UnsupportedImport => {
                crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::UnsupportedImport
            }
            ProgramExecErrorKind::QueueSaturated => {
                crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::QueueSaturated
            }
            ProgramExecErrorKind::Unavailable => {
                crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::Unavailable
            }
            ProgramExecErrorKind::Internal => {
                crate::program_bindings::bindings::helios::system::programs::ExecErrorKind::Internal
            }
        },
        detail: error.detail,
    }
}

fn convert_ping_reply(reply: crate::net::PingReply) -> bindings::helios::system::net::PingReply {
    let octets = reply.address.octets();
    bindings::helios::system::net::PingReply {
        address: bindings::helios::system::net::IpAddress::Ipv4((
            octets[0], octets[1], octets[2], octets[3],
        )),
        round_trip: reply.round_trip_nanos,
        payload_bytes: reply.payload_bytes,
    }
}

fn convert_program_ping_reply(
    reply: crate::net::PingReply,
) -> crate::program_bindings::bindings::helios::system::net::PingReply {
    let octets = reply.address.octets();
    crate::program_bindings::bindings::helios::system::net::PingReply {
        address: crate::program_bindings::bindings::helios::system::net::IpAddress::Ipv4((
            octets[0], octets[1], octets[2], octets[3],
        )),
        round_trip: reply.round_trip_nanos,
        payload_bytes: reply.payload_bytes,
    }
}

fn convert_ping_error(error: crate::net::PingError) -> bindings::helios::system::net::PingError {
    bindings::helios::system::net::PingError {
        kind: match error.kind {
            crate::net::PingErrorKind::UnresolvedHost => {
                bindings::helios::system::net::PingErrorKind::UnresolvedHost
            }
            crate::net::PingErrorKind::Timeout => {
                bindings::helios::system::net::PingErrorKind::Timeout
            }
            crate::net::PingErrorKind::Unavailable => {
                bindings::helios::system::net::PingErrorKind::Unavailable
            }
            crate::net::PingErrorKind::Internal => {
                bindings::helios::system::net::PingErrorKind::Internal
            }
        },
        detail: error.detail,
    }
}

fn convert_program_ping_error(
    error: crate::net::PingError,
) -> crate::program_bindings::bindings::helios::system::net::PingError {
    crate::program_bindings::bindings::helios::system::net::PingError {
        kind: match error.kind {
            crate::net::PingErrorKind::UnresolvedHost => {
                crate::program_bindings::bindings::helios::system::net::PingErrorKind::UnresolvedHost
            }
            crate::net::PingErrorKind::Timeout => {
                crate::program_bindings::bindings::helios::system::net::PingErrorKind::Timeout
            }
            crate::net::PingErrorKind::Unavailable => {
                crate::program_bindings::bindings::helios::system::net::PingErrorKind::Unavailable
            }
            crate::net::PingErrorKind::Internal => {
                crate::program_bindings::bindings::helios::system::net::PingErrorKind::Internal
            }
        },
        detail: error.detail,
    }
}

fn unavailable_tcp_error() -> bindings::helios::system::net::TcpError {
    bindings::helios::system::net::TcpError {
        kind: bindings::helios::system::net::TcpErrorKind::Unavailable,
        detail: "network service is unavailable on this machine".to_owned(),
    }
}

fn unavailable_program_tcp_error()
-> crate::program_bindings::bindings::helios::system::net::TcpError {
    crate::program_bindings::bindings::helios::system::net::TcpError {
        kind: crate::program_bindings::bindings::helios::system::net::TcpErrorKind::Unavailable,
        detail: "network service is unavailable on this machine".to_owned(),
    }
}

fn convert_tcp_error(error: crate::net::TcpError) -> bindings::helios::system::net::TcpError {
    bindings::helios::system::net::TcpError {
        kind: match error.kind {
            crate::net::TcpErrorKind::UnresolvedHost => {
                bindings::helios::system::net::TcpErrorKind::UnresolvedHost
            }
            crate::net::TcpErrorKind::Timeout => {
                bindings::helios::system::net::TcpErrorKind::Timeout
            }
            crate::net::TcpErrorKind::Unavailable => {
                bindings::helios::system::net::TcpErrorKind::Unavailable
            }
            crate::net::TcpErrorKind::Internal => {
                bindings::helios::system::net::TcpErrorKind::Internal
            }
        },
        detail: error.detail,
    }
}

fn convert_program_tcp_error(
    error: crate::net::TcpError,
) -> crate::program_bindings::bindings::helios::system::net::TcpError {
    crate::program_bindings::bindings::helios::system::net::TcpError {
        kind: match error.kind {
            crate::net::TcpErrorKind::UnresolvedHost => {
                crate::program_bindings::bindings::helios::system::net::TcpErrorKind::UnresolvedHost
            }
            crate::net::TcpErrorKind::Timeout => {
                crate::program_bindings::bindings::helios::system::net::TcpErrorKind::Timeout
            }
            crate::net::TcpErrorKind::Unavailable => {
                crate::program_bindings::bindings::helios::system::net::TcpErrorKind::Unavailable
            }
            crate::net::TcpErrorKind::Internal => {
                crate::program_bindings::bindings::helios::system::net::TcpErrorKind::Internal
            }
        },
        detail: error.detail,
    }
}

fn add_instances_to_linker(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    let mut instance = linker.instance(INSTANCES_INSTANCE)?;
    instance.func_wrap("snapshot", |caller, (): ()| {
        Ok((caller
            .data()
            .instance_registry
            .snapshot(caller.data().now_nanos())
            .into_iter()
            .map(convert_instance)
            .collect::<Vec<_>>(),))
    })?;
    Ok(())
}

pub(crate) struct StoreData {
    pub(crate) table: ResourceTable,
    pub(crate) cpu: RiscvCpu,
    pub(crate) debug_state: RuntimeState,
    pub(crate) instance_registry: InstanceRegistry,
    pub(crate) instance: RegisteredInstance,
    pub(crate) debug_port: Option<()>,
    pub(crate) filesystem: crate::debugger_wasi::DebugFileSystem,
    pub(crate) arguments: Vec<String>,
    pub(crate) environment: Vec<(String, String)>,
    pub(crate) output_mode: OutputMode,
    pub(crate) captured_stdout: Arc<Mutex<Vec<u8>>>,
    pub(crate) captured_stderr: Arc<Mutex<Vec<u8>>>,
}

impl StoreData {
    pub(crate) fn now_nanos(&self) -> u64 {
        self.debug_state.uptime_nanos(self.cpu.now().ticks())
    }

    pub(crate) fn write_output(&mut self, stream: OutputStreamKind, bytes: &[u8]) {
        GuestOutput::from_store(self).write(stream, bytes);
    }

    pub(crate) fn take_captured_output(&self) -> helios_kernel::ExecOutput {
        helios_kernel::ExecOutput {
            stdout: self.captured_stdout.lock().clone(),
            stderr: self.captured_stderr.lock().clone(),
        }
    }

    pub(crate) fn record_call_hook(&mut self, hook: CallHook) {
        let transition = match hook {
            CallHook::CallingWasm | CallHook::ReturningFromHost => {
                InstanceExecutionTransition::Resume
            }
            CallHook::ReturningFromWasm | CallHook::CallingHost => {
                InstanceExecutionTransition::Pause
            }
        };
        self.instance.transition(transition, self.now_nanos());
    }
}

impl ResourceLimiter for StoreData {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allow = maximum.is_none_or(|maximum| desired <= maximum);
        if allow {
            self.instance
                .set_memory_bytes(u64::try_from(desired).expect("desired memory exceeds u64"));
        }
        Ok(allow)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(maximum.is_none_or(|maximum| desired <= maximum))
    }
}

impl IoView for StoreData {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

#[derive(Clone, Copy)]
pub(crate) enum OutputStreamKind {
    Stdout,
    Stderr,
}

impl GuestOutput {
    fn from_store(store: &StoreData) -> Self {
        Self {
            mode: store.output_mode,
            debug_state: store.debug_state.clone(),
            cpu: store.cpu.clone(),
            captured_stdout: store.captured_stdout.clone(),
            captured_stderr: store.captured_stderr.clone(),
        }
    }

    fn write(&self, stream: OutputStreamKind, bytes: &[u8]) {
        match self.mode {
            OutputMode::Serial => write_serial(bytes),
            OutputMode::Trace => {
                let text = core::str::from_utf8(bytes).unwrap_or_else(|error| {
                    panic!("guest attempted to write non-utf8 stdout/stderr bytes: {error}")
                });
                self.debug_state
                    .record_console_text(self.cpu.now().ticks(), text);
            }
            OutputMode::Capture => match stream {
                OutputStreamKind::Stdout => self.captured_stdout.lock().extend_from_slice(bytes),
                OutputStreamKind::Stderr => self.captured_stderr.lock().extend_from_slice(bytes),
            },
        }
    }
}

impl DebugSerialOutputStream {
    pub(crate) fn from_store(store: &StoreData, stream: OutputStreamKind) -> Self {
        Self {
            sink: GuestOutput::from_store(store),
            stream,
        }
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for DebugSerialOutputStream {
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl OutputStream for DebugSerialOutputStream {
    fn write(&mut self, bytes: Bytes) -> Result<(), StreamError> {
        self.sink.write(self.stream, bytes.as_ref());
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StreamError> {
        Ok(())
    }

    fn check_write(&mut self) -> Result<usize, StreamError> {
        Ok(4096)
    }
}

#[wasmtime_wasi_io::async_trait]
impl Pollable for DeadlinePollable {
    async fn ready(&mut self) {
        while self
            .cpu
            .debug_state()
            .ticks_to_nanos(self.cpu.now().ticks())
            < self.deadline_nanos
        {
            core::hint::spin_loop();
        }
    }
}

impl bindings::helios::system::serial::Host for StoreData {}

impl bindings::helios::system::serial::HostWithStore for HasSelf<StoreData> {
    async fn debug_port<T: Send>(
        accessor: &Accessor<T, Self>,
    ) -> wasmtime::Result<Option<Resource<SbiSerialPort>>> {
        accessor.with(|mut access| match access.get().debug_port {
            Some(()) => Ok(Some(access.get().table.push(SbiSerialPort {
                _resource: SerialPortResource,
            })?)),
            None => Ok(None),
        })
    }
}

impl bindings::helios::system::serial::HostSerialPort for StoreData {}

impl bindings::helios::system::serial::HostSerialPortWithStore for HasSelf<StoreData> {
    async fn rights<T: Send>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiSerialPort>,
    ) -> wasmtime::Result<bindings::helios::system::serial::SerialRights> {
        accessor.with(|mut access| {
            let _ = access.get().table.get(&resource)?;
            Ok::<_, wasmtime::Error>(
                bindings::helios::system::serial::SerialRights::READ
                    | bindings::helios::system::serial::SerialRights::WRITE
                    | bindings::helios::system::serial::SerialRights::FLUSH,
            )
        })
    }

    async fn drop<T>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiSerialPort>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }

    async fn read<T: 'static + Send>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiSerialPort>,
        max_bytes: u32,
    ) -> wasmtime::Result<Vec<u8>> {
        accessor.with(|mut access| {
            let _ = access.get().table.get(&resource)?;
            Ok::<_, wasmtime::Error>(read_serial(max_bytes))
        })
    }

    async fn write<T: 'static + Send>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiSerialPort>,
        bytes: Vec<u8>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.get(&resource)?;
            write_serial(&bytes);
            Ok::<_, wasmtime::Error>(())
        })?;
        Ok(())
    }

    async fn flush<T: 'static + Send>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiSerialPort>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.get(&resource)?;
            Ok::<_, wasmtime::Error>(())
        })?;
        Ok(())
    }
}

impl bindings::helios::system::sync::Host for StoreData {}

impl bindings::helios::system::sync::HostRawMutex for StoreData {}

impl bindings::helios::system::sync::HostRawMutexWithStore for HasSelf<StoreData> {
    async fn new<T: Send>(accessor: &Accessor<T, Self>) -> wasmtime::Result<Resource<SbiRawMutex>> {
        accessor.with(|mut access| {
            Ok(access.get().table.push(SbiRawMutex {
                resource: RawMutexResource {
                    inner: Arc::new(RawMutex::new()),
                },
            })?)
        })
    }

    async fn drop<T>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiRawMutex>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }

    async fn lock<T: 'static>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiRawMutex>,
    ) -> wasmtime::Result<Resource<SbiRawMutexGuard>> {
        let mutex = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.resource.inner.clone())
        })?;
        let lease = mutex.lock_owned().await;
        accessor.with(|mut access| {
            Ok(access.get().table.push(SbiRawMutexGuard {
                _resource: RawMutexGuardResource { _lease: lease },
            })?)
        })
    }
}

impl bindings::helios::system::sync::HostRawMutexGuard for StoreData {}

impl bindings::helios::system::sync::HostRawMutexGuardWithStore for HasSelf<StoreData> {
    async fn drop<T>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiRawMutexGuard>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }
}

impl bindings::helios::system::sync::HostRawRwLock for StoreData {}

impl bindings::helios::system::sync::HostRawRwLockWithStore for HasSelf<StoreData> {
    async fn new<T: Send>(
        accessor: &Accessor<T, Self>,
    ) -> wasmtime::Result<Resource<SbiRawRwLock>> {
        accessor.with(|mut access| {
            Ok(access.get().table.push(SbiRawRwLock {
                resource: RawRwLockResource {
                    inner: Arc::new(RawRwLock::new()),
                },
            })?)
        })
    }

    async fn drop<T>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiRawRwLock>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }

    async fn read<T: 'static>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiRawRwLock>,
    ) -> wasmtime::Result<Resource<SbiRawRwLockReadGuard>> {
        let rwlock = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.resource.inner.clone())
        })?;
        let lease = rwlock.read_owned().await;
        accessor.with(|mut access| {
            Ok(access.get().table.push(SbiRawRwLockReadGuard {
                _resource: RawRwLockReadGuardResource { _lease: lease },
            })?)
        })
    }

    async fn write<T: 'static>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiRawRwLock>,
    ) -> wasmtime::Result<Resource<SbiRawRwLockWriteGuard>> {
        let rwlock = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.resource.inner.clone())
        })?;
        let lease = rwlock.write_owned().await;
        accessor.with(|mut access| {
            Ok(access.get().table.push(SbiRawRwLockWriteGuard {
                _resource: RawRwLockWriteGuardResource { _lease: lease },
            })?)
        })
    }
}

impl bindings::helios::system::sync::HostRawRwLockReadGuard for StoreData {}

impl bindings::helios::system::sync::HostRawRwLockReadGuardWithStore for HasSelf<StoreData> {
    async fn drop<T>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiRawRwLockReadGuard>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }
}

impl bindings::helios::system::sync::HostRawRwLockWriteGuard for StoreData {}

impl bindings::helios::system::sync::HostRawRwLockWriteGuardWithStore for HasSelf<StoreData> {
    async fn drop<T>(
        accessor: &Accessor<T, Self>,
        resource: Resource<SbiRawRwLockWriteGuard>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let _ = access.get().table.delete(resource)?;
            Ok::<_, wasmtime::Error>(())
        })
    }
}

struct StatsStreamProducer {
    period_nanos: u64,
    next_due: Option<u64>,
}

impl StatsStreamProducer {
    fn new(period_nanos: u64) -> Self {
        assert!(period_nanos != 0, "stats subscribe period must be non-zero");
        Self {
            period_nanos,
            next_due: None,
        }
    }
}

impl StreamProducer<StoreData> for StatsStreamProducer {
    type Item = bindings::helios::system::stats::Sample;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<'_, StoreData>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        let now = store.data().now_nanos();
        let period_nanos = self.period_nanos;
        let due = self
            .next_due
            .get_or_insert_with(|| now.saturating_add(period_nanos));
        if now < *due {
            return Poll::Pending;
        }

        *due = now.saturating_add(period_nanos);
        destination.set_buffer(Some(snapshot_sample(store.data())));
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

struct ProgramStatsStreamProducer {
    period_nanos: u64,
    next_due: Option<u64>,
}

impl ProgramStatsStreamProducer {
    fn new(period_nanos: u64) -> Self {
        assert!(period_nanos != 0, "stats subscribe period must be non-zero");
        Self {
            period_nanos,
            next_due: None,
        }
    }
}

impl StreamProducer<StoreData> for ProgramStatsStreamProducer {
    type Item = crate::program_bindings::bindings::helios::system::stats::Sample;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<'_, StoreData>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        let now = store.data().now_nanos();
        let period_nanos = self.period_nanos;
        let due = self
            .next_due
            .get_or_insert_with(|| now.saturating_add(period_nanos));
        if now < *due {
            return Poll::Pending;
        }

        *due = now.saturating_add(period_nanos);
        destination.set_buffer(Some(snapshot_program_sample(store.data())));
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

struct TracingStreamProducer {
    filter: TraceFilter,
    cursor: u64,
}

impl TracingStreamProducer {
    fn new(filter: TraceFilter) -> Self {
        Self { filter, cursor: 0 }
    }
}

impl StreamProducer<StoreData> for TracingStreamProducer {
    type Item = bindings::helios::system::tracing::Event;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<'_, StoreData>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        match store
            .data()
            .debug_state
            .next_after(self.cursor, &self.filter)
        {
            Some((seq, event)) => {
                self.cursor = seq;
                destination.set_buffer(Some(convert_event(event)));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            None => Poll::Pending,
        }
    }
}

struct ProgramTracingStreamProducer {
    filter: TraceFilter,
    cursor: u64,
}

impl ProgramTracingStreamProducer {
    fn new(filter: TraceFilter) -> Self {
        Self { filter, cursor: 0 }
    }
}

impl StreamProducer<StoreData> for ProgramTracingStreamProducer {
    type Item = crate::program_bindings::bindings::helios::system::tracing::Event;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        store: StoreContextMut<'_, StoreData>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        match store
            .data()
            .debug_state
            .next_after(self.cursor, &self.filter)
        {
            Some((seq, event)) => {
                self.cursor = seq;
                destination.set_buffer(Some(convert_program_event(event)));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            None => Poll::Pending,
        }
    }
}

fn snapshot_sample(store: &StoreData) -> bindings::helios::system::stats::Sample {
    convert_sample(store.debug_state.snapshot(store.cpu.now().ticks()))
}

fn snapshot_program_sample(
    store: &StoreData,
) -> crate::program_bindings::bindings::helios::system::stats::Sample {
    convert_program_sample(store.debug_state.snapshot(store.cpu.now().ticks()))
}

fn convert_sample(sample: StatsSample) -> bindings::helios::system::stats::Sample {
    let heap = heap_stats();
    let total_bytes =
        u64::try_from(heap.total_bytes).expect("kernel heap total bytes do not fit into u64");
    let available_bytes = u64::try_from(heap.available_bytes())
        .expect("kernel heap available bytes do not fit into u64");
    bindings::helios::system::stats::Sample {
        timestamp: sample.timestamp,
        uptime: sample.uptime,
        processors: bindings::helios::system::stats::Processors {
            configured: sample.configured_processors,
            online: sample.online_processors,
            utilization: (0..sample.configured_processors)
                .map(|id| bindings::helios::system::stats::Processor { id, busy: 0 })
                .collect(),
        },
        memory: bindings::helios::system::stats::Memory {
            total_bytes,
            available_bytes,
            pressure: convert_memory_pressure(total_bytes, available_bytes),
        },
    }
}

fn convert_program_sample(
    sample: StatsSample,
) -> crate::program_bindings::bindings::helios::system::stats::Sample {
    let heap = heap_stats();
    let total_bytes =
        u64::try_from(heap.total_bytes).expect("kernel heap total bytes do not fit into u64");
    let available_bytes = u64::try_from(heap.available_bytes())
        .expect("kernel heap available bytes do not fit into u64");
    crate::program_bindings::bindings::helios::system::stats::Sample {
        timestamp: sample.timestamp,
        uptime: sample.uptime,
        processors: crate::program_bindings::bindings::helios::system::stats::Processors {
            configured: sample.configured_processors,
            online: sample.online_processors,
            utilization: (0..sample.configured_processors)
                .map(
                    |id| crate::program_bindings::bindings::helios::system::stats::Processor {
                        id,
                        busy: 0,
                    },
                )
                .collect(),
        },
        memory: crate::program_bindings::bindings::helios::system::stats::Memory {
            total_bytes,
            available_bytes,
            pressure: convert_program_memory_pressure(total_bytes, available_bytes),
        },
    }
}

fn convert_instance(
    instance: helios_kernel::InstanceSnapshot,
) -> bindings::helios::system::instances::Instance {
    bindings::helios::system::instances::Instance {
        id: instance.id.raw(),
        name: instance.name,
        started_at: instance.started_at,
        uptime: instance.uptime,
        memory_bytes: instance.memory_bytes,
        cpu_busy: instance.cpu_busy,
    }
}

fn convert_memory_pressure(
    total_bytes: u64,
    available_bytes: u64,
) -> bindings::helios::system::stats::MemoryPressure {
    if total_bytes == 0 {
        return bindings::helios::system::stats::MemoryPressure::Nominal;
    }

    let used_permille =
        ((total_bytes.saturating_sub(available_bytes.min(total_bytes))) * 1_000) / total_bytes;

    match used_permille {
        0..=699 => bindings::helios::system::stats::MemoryPressure::Nominal,
        700..=849 => bindings::helios::system::stats::MemoryPressure::Elevated,
        850..=949 => bindings::helios::system::stats::MemoryPressure::High,
        _ => bindings::helios::system::stats::MemoryPressure::Critical,
    }
}

fn convert_program_memory_pressure(
    total_bytes: u64,
    available_bytes: u64,
) -> crate::program_bindings::bindings::helios::system::stats::MemoryPressure {
    if total_bytes == 0 {
        return crate::program_bindings::bindings::helios::system::stats::MemoryPressure::Nominal;
    }

    let used_permille =
        ((total_bytes.saturating_sub(available_bytes.min(total_bytes))) * 1_000) / total_bytes;

    match used_permille {
        0..=699 => {
            crate::program_bindings::bindings::helios::system::stats::MemoryPressure::Nominal
        }
        700..=849 => {
            crate::program_bindings::bindings::helios::system::stats::MemoryPressure::Elevated
        }
        850..=949 => crate::program_bindings::bindings::helios::system::stats::MemoryPressure::High,
        _ => crate::program_bindings::bindings::helios::system::stats::MemoryPressure::Critical,
    }
}

fn convert_filter(filter: bindings::helios::system::tracing::Filter) -> TraceFilter {
    TraceFilter {
        min_level: filter.min_level.map(convert_level_to_local),
        target_prefixes: filter.target_prefixes,
    }
}

fn convert_program_filter(
    filter: crate::program_bindings::bindings::helios::system::tracing::Filter,
) -> TraceFilter {
    TraceFilter {
        min_level: filter.min_level.map(convert_program_level_to_local),
        target_prefixes: filter.target_prefixes,
    }
}

fn convert_event(event: TraceEvent) -> bindings::helios::system::tracing::Event {
    bindings::helios::system::tracing::Event {
        timestamp: event.timestamp,
        level: convert_level_from_local(event.level),
        target: event.target,
        fields: event.fields.into_iter().map(convert_field).collect(),
    }
}

fn convert_program_event(
    event: TraceEvent,
) -> crate::program_bindings::bindings::helios::system::tracing::Event {
    crate::program_bindings::bindings::helios::system::tracing::Event {
        timestamp: event.timestamp,
        level: convert_program_level_from_local(event.level),
        target: event.target,
        fields: event
            .fields
            .into_iter()
            .map(convert_program_field)
            .collect(),
    }
}

fn convert_field(field: TraceField) -> bindings::helios::system::tracing::Field {
    bindings::helios::system::tracing::Field {
        key: field.key,
        value: convert_value(field.value),
    }
}

fn convert_program_field(
    field: TraceField,
) -> crate::program_bindings::bindings::helios::system::tracing::Field {
    crate::program_bindings::bindings::helios::system::tracing::Field {
        key: field.key,
        value: convert_program_value(field.value),
    }
}

fn convert_value(value: TraceValue) -> bindings::helios::system::tracing::Value {
    match value {
        TraceValue::Boolean(value) => bindings::helios::system::tracing::Value::Boolean(value),
        TraceValue::Signed64(value) => bindings::helios::system::tracing::Value::Signed64(value),
        TraceValue::Unsigned64(value) => {
            bindings::helios::system::tracing::Value::Unsigned64(value)
        }
        TraceValue::Float64(value) => bindings::helios::system::tracing::Value::Float64(value),
        TraceValue::Text(value) => bindings::helios::system::tracing::Value::Text(value),
        TraceValue::Blob(value) => bindings::helios::system::tracing::Value::Blob(value),
    }
}

fn convert_program_value(
    value: TraceValue,
) -> crate::program_bindings::bindings::helios::system::tracing::Value {
    match value {
        TraceValue::Boolean(value) => {
            crate::program_bindings::bindings::helios::system::tracing::Value::Boolean(value)
        }
        TraceValue::Signed64(value) => {
            crate::program_bindings::bindings::helios::system::tracing::Value::Signed64(value)
        }
        TraceValue::Unsigned64(value) => {
            crate::program_bindings::bindings::helios::system::tracing::Value::Unsigned64(value)
        }
        TraceValue::Float64(value) => {
            crate::program_bindings::bindings::helios::system::tracing::Value::Float64(value)
        }
        TraceValue::Text(value) => {
            crate::program_bindings::bindings::helios::system::tracing::Value::Text(value)
        }
        TraceValue::Blob(value) => {
            crate::program_bindings::bindings::helios::system::tracing::Value::Blob(value)
        }
    }
}

fn convert_level_from_local(level: TraceLevel) -> bindings::helios::system::tracing::Level {
    match level {
        TraceLevel::Error => bindings::helios::system::tracing::Level::Error,
        TraceLevel::Warn => bindings::helios::system::tracing::Level::Warn,
        TraceLevel::Info => bindings::helios::system::tracing::Level::Info,
        TraceLevel::Debug => bindings::helios::system::tracing::Level::Debug,
        TraceLevel::Trace => bindings::helios::system::tracing::Level::Trace,
    }
}

fn convert_level_to_local(level: bindings::helios::system::tracing::Level) -> TraceLevel {
    match level {
        bindings::helios::system::tracing::Level::Error => TraceLevel::Error,
        bindings::helios::system::tracing::Level::Warn => TraceLevel::Warn,
        bindings::helios::system::tracing::Level::Info => TraceLevel::Info,
        bindings::helios::system::tracing::Level::Debug => TraceLevel::Debug,
        bindings::helios::system::tracing::Level::Trace => TraceLevel::Trace,
    }
}

fn convert_program_level_from_local(
    level: TraceLevel,
) -> crate::program_bindings::bindings::helios::system::tracing::Level {
    match level {
        TraceLevel::Error => {
            crate::program_bindings::bindings::helios::system::tracing::Level::Error
        }
        TraceLevel::Warn => crate::program_bindings::bindings::helios::system::tracing::Level::Warn,
        TraceLevel::Info => crate::program_bindings::bindings::helios::system::tracing::Level::Info,
        TraceLevel::Debug => {
            crate::program_bindings::bindings::helios::system::tracing::Level::Debug
        }
        TraceLevel::Trace => {
            crate::program_bindings::bindings::helios::system::tracing::Level::Trace
        }
    }
}

fn convert_program_level_to_local(
    level: crate::program_bindings::bindings::helios::system::tracing::Level,
) -> TraceLevel {
    match level {
        crate::program_bindings::bindings::helios::system::tracing::Level::Error => {
            TraceLevel::Error
        }
        crate::program_bindings::bindings::helios::system::tracing::Level::Warn => TraceLevel::Warn,
        crate::program_bindings::bindings::helios::system::tracing::Level::Info => TraceLevel::Info,
        crate::program_bindings::bindings::helios::system::tracing::Level::Debug => {
            TraceLevel::Debug
        }
        crate::program_bindings::bindings::helios::system::tracing::Level::Trace => {
            TraceLevel::Trace
        }
    }
}

fn read_serial(max_bytes: u32) -> Vec<u8> {
    let max_bytes = max_bytes as usize;
    let mut bytes = Vec::with_capacity(max_bytes);

    loop {
        if let Some(byte) = try_read_debug_serial_byte() {
            bytes.push(byte);
            break;
        }
        core::hint::spin_loop();
    }

    while bytes.len() < max_bytes {
        let Some(byte) = try_read_debug_serial_byte() else {
            break;
        };
        bytes.push(byte);
    }

    bytes
}

fn write_serial(bytes: &[u8]) {
    write_debug_serial_bytes(bytes);
}

fn emit_stage_marker(stage: &str) {
    write_debug_serial_bytes(b"\n[KDBG ");
    write_debug_serial_bytes(stage.as_bytes());
    write_debug_serial_bytes(b"]\n");
}

fn emit_error_marker(label: &str, message: &str) {
    write_debug_serial_bytes(b"\n[KDBG ");
    write_debug_serial_bytes(label.as_bytes());
    write_debug_serial_bytes(b": ");
    for byte in message.bytes() {
        match byte {
            b'\n' | b'\r' => write_debug_serial_bytes(b" "),
            b']' => write_debug_serial_bytes(b")"),
            other => write_debug_serial_bytes(&[other]),
        }
    }
    write_debug_serial_bytes(b"]\n");
}

fn log_wasmtime_error_chain(prefix: &str, error: &wasmtime::Error) {
    tracing::error!("{prefix}: {error:?}");
}

#[derive(Debug, Error)]
enum DebuggerError {
    #[error("failed to initialize Wasmtime engine: {0}")]
    CreateEngine(wasmtime::Error),
    #[error("failed to JIT-compile embedded debugger component: {0}")]
    CompileComponent(wasmtime::Error),
    #[error("failed to add debugger host bindings: {0}")]
    LinkComponent(wasmtime::Error),
    #[error("failed to instantiate debugger component: {0}")]
    InstantiateComponent(wasmtime::Error),
    #[error("debugger component trapped: {0}")]
    RunComponent(wasmtime::Error),
    #[error("debugger component returned a non-zero result")]
    GuestFailed,
}
