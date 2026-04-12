extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use spin::Mutex;

use crate::{
    ExecOutput, InstanceExecutionTransition, InstanceRegistry, RegisteredInstance,
    allow_instance_resource_growth, record_instance_transition, yield_now,
};
use helios_hal::cpu::Cpu;
use wasmtime::component::ResourceTable;
use wasmtime::{CallHook, ResourceLimiter};
use wasmtime_wasi_io::IoView;
use wasmtime_wasi_io::bytes::Bytes;
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{OutputStream, StreamError};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ComponentOutputMode {
    Serial,
    Trace,
    Capture,
}

#[derive(Clone, Copy)]
pub enum ComponentOutputStreamKind {
    Stdout,
    Stderr,
}

pub trait ComponentRuntimeState: Clone + Send + 'static {
    fn uptime_nanos(&self, current_ticks: u64) -> u64;

    fn record_console_text(&self, current_ticks: u64, text: &str);
}

pub(crate) struct ComponentExecutionContext<FileSystem> {
    instance: RegisteredInstance,
    debug_port: Option<()>,
    filesystem: FileSystem,
    arguments: Vec<String>,
    environment: Vec<(String, String)>,
    output_mode: ComponentOutputMode,
    captured_stdout: Arc<Mutex<Vec<u8>>>,
    captured_stderr: Arc<Mutex<Vec<u8>>>,
}

pub struct ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem> {
    pub table: ResourceTable,
    pub cpu: CpuImpl,
    pub runtime_state: RuntimeStateImpl,
    pub instance_registry: InstanceRegistry,
    execution_context: ComponentExecutionContext<FileSystem>,
    serial_reader: fn(u32) -> Vec<u8>,
    serial_writer: fn(&[u8]),
}

#[derive(Clone)]
struct ComponentOutput<CpuImpl, RuntimeStateImpl> {
    mode: ComponentOutputMode,
    runtime_state: RuntimeStateImpl,
    cpu: CpuImpl,
    captured_stdout: Arc<Mutex<Vec<u8>>>,
    captured_stderr: Arc<Mutex<Vec<u8>>>,
    serial_writer: fn(&[u8]),
}

pub struct ComponentOutputStream<CpuImpl, RuntimeStateImpl> {
    sink: ComponentOutput<CpuImpl, RuntimeStateImpl>,
    stream: ComponentOutputStreamKind,
}

pub struct DeadlinePollable<CpuImpl, RuntimeStateImpl> {
    cpu: CpuImpl,
    runtime_state: RuntimeStateImpl,
    deadline_nanos: u64,
}

impl<FileSystem> ComponentExecutionContext<FileSystem> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        instance: RegisteredInstance,
        debug_port: Option<()>,
        filesystem: FileSystem,
        arguments: Vec<String>,
        environment: Vec<(String, String)>,
        output_mode: ComponentOutputMode,
    ) -> Self {
        Self {
            instance,
            debug_port,
            filesystem,
            arguments,
            environment,
            output_mode,
            captured_stdout: Arc::new(Mutex::new(Vec::new())),
            captured_stderr: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn take_captured_output(&self) -> ExecOutput {
        ExecOutput {
            stdout: self.captured_stdout.lock().clone(),
            stderr: self.captured_stderr.lock().clone(),
        }
    }

}

impl<CpuImpl, RuntimeStateImpl, FileSystem>
    ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
    FileSystem: Send,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cpu: CpuImpl,
        runtime_state: RuntimeStateImpl,
        instance_registry: InstanceRegistry,
        instance: RegisteredInstance,
        debug_port: Option<()>,
        filesystem: FileSystem,
        arguments: Vec<String>,
        environment: Vec<(String, String)>,
        output_mode: ComponentOutputMode,
        serial_reader: fn(u32) -> Vec<u8>,
        serial_writer: fn(&[u8]),
    ) -> Self {
        Self {
            table: ResourceTable::new(),
            cpu,
            runtime_state,
            instance_registry,
            execution_context: ComponentExecutionContext::new(
                instance,
                debug_port,
                filesystem,
                arguments,
                environment,
                output_mode,
            ),
            serial_reader,
            serial_writer,
        }
    }

    pub(crate) fn instance(&self) -> &RegisteredInstance {
        &self.execution_context.instance
    }

    pub(crate) fn debug_port(&self) -> Option<()> {
        self.execution_context.debug_port
    }

    pub(crate) fn filesystem(&self) -> &FileSystem {
        &self.execution_context.filesystem
    }

    pub(crate) fn filesystem_mut(&mut self) -> &mut FileSystem {
        &mut self.execution_context.filesystem
    }

    pub(crate) fn arguments(&self) -> &[String] {
        &self.execution_context.arguments
    }

    pub(crate) fn environment(&self) -> &[(String, String)] {
        &self.execution_context.environment
    }

    pub(crate) fn take_captured_output(&self) -> ExecOutput {
        self.execution_context.take_captured_output()
    }

    pub(crate) fn serial_reader_fn(&self) -> fn(u32) -> Vec<u8> {
        self.serial_reader
    }

    pub(crate) fn serial_writer_fn(&self) -> fn(&[u8]) {
        self.serial_writer
    }

    pub fn now_nanos(&self) -> u64 {
        self.runtime_state.uptime_nanos(self.cpu.now().ticks())
    }

    pub fn write_output(&mut self, stream: ComponentOutputStreamKind, bytes: &[u8]) {
        ComponentOutput::from_store(self).write(stream, bytes);
    }

    pub fn read_serial(&self, max_bytes: u32) -> Vec<u8> {
        (self.serial_reader)(max_bytes)
    }

    pub fn write_serial(&self, bytes: &[u8]) {
        (self.serial_writer)(bytes);
    }

    pub fn record_call_hook(&mut self, hook: CallHook) {
        let transition = match hook {
            CallHook::CallingWasm | CallHook::ReturningFromHost => {
                InstanceExecutionTransition::Resume
            }
            CallHook::ReturningFromWasm | CallHook::CallingHost => {
                InstanceExecutionTransition::Pause
            }
        };
        record_instance_transition(self.instance(), transition, self.now_nanos());
    }
}

impl<CpuImpl, RuntimeStateImpl, FileSystem> ResourceLimiter
    for ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
    FileSystem: Send,
{
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(allow_instance_resource_growth(
            self.instance(),
            desired,
            maximum,
        ))
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

impl<CpuImpl, RuntimeStateImpl, FileSystem> IoView
    for ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl<CpuImpl, RuntimeStateImpl> ComponentOutput<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    fn from_store<FileSystem>(
        store: &ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem>,
    ) -> Self
    where
        FileSystem: Send,
    {
        let context = &store.execution_context;
        Self {
            mode: context.output_mode,
            runtime_state: store.runtime_state.clone(),
            cpu: store.cpu.clone(),
            captured_stdout: context.captured_stdout.clone(),
            captured_stderr: context.captured_stderr.clone(),
            serial_writer: store.serial_writer,
        }
    }

    fn write(&self, stream: ComponentOutputStreamKind, bytes: &[u8]) {
        match self.mode {
            ComponentOutputMode::Serial => (self.serial_writer)(bytes),
            ComponentOutputMode::Trace => {
                let text = core::str::from_utf8(bytes).unwrap_or_else(|error| {
                    panic!("guest attempted to write non-utf8 stdout/stderr bytes: {error}")
                });
                self.runtime_state
                    .record_console_text(self.cpu.now().ticks(), text);
            }
            ComponentOutputMode::Capture => match stream {
                ComponentOutputStreamKind::Stdout => {
                    self.captured_stdout.lock().extend_from_slice(bytes)
                }
                ComponentOutputStreamKind::Stderr => {
                    self.captured_stderr.lock().extend_from_slice(bytes)
                }
            },
        }
    }
}

impl<CpuImpl, RuntimeStateImpl> ComponentOutputStream<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    pub fn from_store<FileSystem>(
        store: &ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem>,
        stream: ComponentOutputStreamKind,
    ) -> Self
    where
        FileSystem: Send,
    {
        Self {
            sink: ComponentOutput::from_store(store),
            stream,
        }
    }
}

impl<CpuImpl, RuntimeStateImpl> DeadlinePollable<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    pub fn new(cpu: CpuImpl, runtime_state: RuntimeStateImpl, deadline_nanos: u64) -> Self {
        Self {
            cpu,
            runtime_state,
            deadline_nanos,
        }
    }
}

#[wasmtime_wasi_io::async_trait]
impl<CpuImpl, RuntimeStateImpl> Pollable for ComponentOutputStream<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl<CpuImpl, RuntimeStateImpl> OutputStream for ComponentOutputStream<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
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
impl<CpuImpl, RuntimeStateImpl> Pollable for DeadlinePollable<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + crate::CodegenPlatform + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    async fn ready(&mut self) {
        while self.runtime_state.uptime_nanos(self.cpu.now().ticks()) < self.deadline_nanos {
            yield_now().await;
        }
    }
}
