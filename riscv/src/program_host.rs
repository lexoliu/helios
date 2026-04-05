extern crate alloc;

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use helios_hal::cpu::Cpu;
use helios_hal::resource::WasiRights;
use helios_kernel::{
    ComputePool, ComputePriority, InstanceId, InstanceRegistry, Notify, ProgramExecError,
    ProgramExecErrorKind, RegisteredInstance, heap_stats,
};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};

use crate::debugger_program::{self, OutputMode, StoreData};
use crate::{RiscvCpu, debug_state::RuntimeState, program_bindings};

const WORKER_STACK_SIZE: usize = 256 * 1024;

#[derive(Clone)]
pub(crate) struct UserProgramService {
    inner: Arc<UserProgramServiceInner>,
}

struct UserProgramServiceInner {
    engine: Engine,
    compiler: ComputePool,
    compile_priority: ComputePriority,
    timebase_frequency: u64,
    debug_state: RuntimeState,
    instance_registry: InstanceRegistry,
    run_queue: ConcurrentQueue<QueuedProgram>,
    run_ready: Notify,
}

struct QueuedProgram {
    name: String,
    args: Vec<String>,
    component: Component,
    instance: RegisteredInstance,
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

    let memory_budget = heap_stats()
        .available_bytes()
        .max(worker_count * WORKER_STACK_SIZE);
    let engine = debugger_program::build_engine().unwrap_or_else(|error| {
        panic!("failed to create RISC-V component engine for user programs: {error:#}")
    });
    let compiler = ComputePool::new(worker_count, WORKER_STACK_SIZE, memory_budget)
        .unwrap_or_else(|error| panic!("failed to create user-program compute pool: {error}"));
    let service = UserProgramService {
        inner: Arc::new(UserProgramServiceInner {
            engine,
            compiler,
            compile_priority: ComputePriority::NORMAL,
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
    ) -> Result<InstanceId, ProgramExecError> {
        let name = name.into();
        let component = self.compile_component(wasm).await?;
        let started_at = monotonic_nanos(self.inner.timebase_frequency);
        let instance = self
            .inner
            .instance_registry
            .register(name.clone(), started_at);
        let id = instance.id();
        let queued = QueuedProgram {
            name,
            args,
            component,
            instance,
        };
        match self.inner.run_queue.push(queued) {
            Ok(()) => {
                self.inner.run_ready.notify_one();
                Ok(id)
            }
            Err(PushError::Full(_)) => unreachable!("unbounded program queue reported full"),
            Err(PushError::Closed(_)) => Err(ProgramExecError {
                kind: ProgramExecErrorKind::Unavailable,
                detail: "program worker queue was closed unexpectedly".to_string(),
            }),
        }
    }

    pub(crate) fn run_next_on(&self, execution_cpu: &RiscvCpu) -> bool {
        match self.inner.run_queue.pop() {
            Ok(queued) => {
                let instance_id = queued.instance.id().raw();
                let instance_name = queued.instance.name().to_string();
                match run_component(
                    queued,
                    execution_cpu.clone(),
                    self.inner.debug_state.clone(),
                    self.inner.instance_registry.clone(),
                ) {
                    Ok(exit_code) => {
                        tracing::info!(
                            "Program exited instance={} name={} code={}",
                            instance_id,
                            instance_name,
                            exit_code
                        );
                    }
                    Err(error) => {
                        tracing::error!(
                            "Program trapped instance={} name={} error={}",
                            instance_id,
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

    async fn compile_component(&self, wasm: &[u8]) -> Result<Component, ProgramExecError> {
        let engine = self.inner.engine.clone();
        let wasm = wasm.to_vec();
        self.inner
            .compiler
            .spawn(self.inner.compile_priority, move || {
                Component::from_binary(&engine, &wasm)
            })
            .await
            .map_err(|error| ProgramExecError {
                kind: ProgramExecErrorKind::QueueSaturated,
                detail: error.to_string(),
            })?
            .map_err(|error| ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: error.to_string(),
            })
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
) -> Result<u32, wasmtime::Error> {
    let QueuedProgram {
        name,
        args,
        component,
        instance,
    } = queued;
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(name);
    argv.extend(args);
    let mut linker = Linker::<StoreData>::new(component.engine());
    wasmtime_wasi_io::add_to_linker_async(&mut linker)?;
    crate::debugger_wasi_p2::add_to_linker(&mut linker)?;
    crate::debugger_wasi::add_to_linker(&mut linker)?;
    debugger_program::add_serial_to_linker(&mut linker)?;
    debugger_program::add_sync_to_linker(&mut linker)?;
    debugger_program::add_program_world_to_linker(&mut linker)?;

    let mut store = Store::new(
        component.engine(),
        StoreData {
            table: ResourceTable::new(),
            cpu,
            debug_state,
            instance_registry,
            instance,
            debug_port: None,
            filesystem: crate::debugger_wasi::DebugFileSystem::new(),
            arguments: argv,
            environment: Vec::new(),
            output_mode: OutputMode::Trace,
        },
    );
    store.limiter(|state| state);
    store.call_hook(|mut caller, hook| {
        caller.data_mut().record_call_hook(hook);
        Ok(())
    });

    let program = debugger_program::block_on(program_bindings::bindings::Init::instantiate_async(
        &mut store, &component, &linker,
    ))?;
    let result =
        debugger_program::block_on(store.run_concurrent(async move |accessor| {
            program.wasi_cli_run().call_run(accessor).await
        }))?;
    match result {
        Ok(Ok(())) => Ok(0),
        Ok(Err(())) => Ok(1),
        Err(error) => Err(error),
    }
}

fn monotonic_nanos(timebase_frequency: u64) -> u64 {
    let ticks = riscv::register::time::read64();
    ticks.saturating_mul(1_000_000_000) / timebase_frequency
}
