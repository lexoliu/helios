extern crate alloc;

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use core::future::{Future, poll_fn};
use core::task::Poll;

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use helios_hal::cpu::Cpu;
use helios_hal::resource::WasiRights;
use thiserror::Error;

use crate::program::CompileError;
use crate::{
    Blueprint, InstanceId, InstanceRegistry, Notify, ProgramRuntime, ProgramRuntimeDriver, Task,
    monotonic_nanos,
};

#[derive(Clone)]
pub struct ProgramService<CpuImpl: Cpu + Clone> {
    inner: Arc<ProgramServiceInner<CpuImpl>>,
}

struct ProgramServiceInner<CpuImpl: Cpu + Clone> {
    runtime: ProgramRuntime,
    compiler: ProgramRuntimeDriver,
    clock_cpu: CpuImpl,
    instance_registry: InstanceRegistry,
    run_queue: ConcurrentQueue<QueuedProgram>,
    run_ready: Notify,
    compile_ready: Arc<Notify>,
}

struct QueuedProgram {
    task: Task,
    instance: crate::RegisteredInstance,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ProgramExecErrorKind {
    #[error("the wasm binary is invalid")]
    InvalidBinary,
    #[error("the wasm binary exports no supported entry point")]
    MissingEntry,
    #[error("the wasm binary imports unsupported host functions")]
    UnsupportedImport,
    #[error("the exec queues are saturated")]
    QueueSaturated,
    #[error("program exec is unavailable on this platform")]
    Unavailable,
    #[error("the kernel rejected the program for an internal reason")]
    Internal,
}

#[derive(Debug, Error)]
#[error("{kind}: {detail}")]
pub struct ProgramExecError {
    pub kind: ProgramExecErrorKind,
    pub detail: String,
}

impl<CpuImpl: Cpu + Clone> ProgramService<CpuImpl> {
    pub fn new(
        runtime: ProgramRuntime,
        clock_cpu: CpuImpl,
        instance_registry: InstanceRegistry,
    ) -> Self {
        let compiler = runtime.driver();
        let compile_ready = compiler.ready_notify();
        Self {
            inner: Arc::new(ProgramServiceInner {
                runtime,
                compiler,
                clock_cpu,
                instance_registry,
                run_queue: ConcurrentQueue::unbounded(),
                run_ready: Notify::new(),
                compile_ready,
            }),
        }
    }

    pub fn compile_worker_count(&self) -> usize {
        self.inner.runtime.compile_worker_count()
    }

    pub async fn exec(
        &self,
        name: impl Into<String>,
        wasm: &[u8],
        rights: WasiRights,
    ) -> Result<InstanceId, ProgramExecError> {
        let name = name.into();
        let task = self
            .inner
            .runtime
            .compile(Blueprint::new_wasm(wasm, rights))
            .await
            .map_err(ProgramExecError::from)?;
        let started_at = monotonic_nanos(&self.inner.clock_cpu);
        let instance = self.inner.instance_registry.register(name, started_at);
        let id = instance.id();
        let queued = QueuedProgram { task, instance };
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

    pub fn run_next_on(&self, execution_cpu: &CpuImpl) -> bool {
        match self.inner.run_queue.pop() {
            Ok(queued) => {
                let instance_id = queued.instance.id().raw();
                let instance_name = queued.instance.name().to_string();
                match queued
                    .task
                    .run_tracked(execution_cpu.clone(), queued.instance)
                {
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

    pub async fn wait_for_activity(&self) {
        let run = self.inner.run_ready.notified();
        let compile = self.inner.compile_ready.notified();
        let mut run = core::pin::pin!(run);
        let mut compile = core::pin::pin!(compile);
        poll_fn(|cx| {
            if run.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
            if compile.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
            Poll::Pending
        })
        .await;
    }
}

impl From<CompileError> for ProgramExecError {
    fn from(error: CompileError) -> Self {
        match error {
            CompileError::QueueSaturated(error) => Self {
                kind: ProgramExecErrorKind::QueueSaturated,
                detail: error.to_string(),
            },
            CompileError::MissingEntry => Self {
                kind: ProgramExecErrorKind::MissingEntry,
                detail: error.to_string(),
            },
            CompileError::UnsupportedImportModule { .. } => Self {
                kind: ProgramExecErrorKind::UnsupportedImport,
                detail: error.to_string(),
            },
            CompileError::Wasmtime(error) => Self {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: error.to_string(),
            },
        }
    }
}
