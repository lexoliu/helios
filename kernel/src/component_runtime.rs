extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::boxed::Box;

use spin::Mutex;

use crate::{
    ExecOutput, InstanceRegistry, RegisteredInstance, allow_instance_resource_growth,
    record_instance_call_hook,
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

pub struct ComponentStoreData<CpuImpl, RuntimeState, FileSystem> {
    pub table: ResourceTable,
    pub cpu: CpuImpl,
    pub runtime_state: RuntimeState,
    pub instance_registry: InstanceRegistry,
    pub instance: RegisteredInstance,
    pub debug_port: Option<()>,
    pub filesystem: FileSystem,
    pub arguments: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub output_mode: ComponentOutputMode,
    pub captured_stdout: Arc<Mutex<Vec<u8>>>,
    pub captured_stderr: Arc<Mutex<Vec<u8>>>,
    serial_writer: fn(&[u8]),
    uptime_nanos: fn(&RuntimeState, u64) -> u64,
    record_console_text: fn(&RuntimeState, u64, &str),
}

#[derive(Clone)]
struct ComponentOutput<CpuImpl, RuntimeState> {
    mode: ComponentOutputMode,
    runtime_state: RuntimeState,
    cpu: CpuImpl,
    captured_stdout: Arc<Mutex<Vec<u8>>>,
    captured_stderr: Arc<Mutex<Vec<u8>>>,
    serial_writer: fn(&[u8]),
    record_console_text: fn(&RuntimeState, u64, &str),
}

pub struct ComponentOutputStream<CpuImpl, RuntimeState> {
    sink: ComponentOutput<CpuImpl, RuntimeState>,
    stream: ComponentOutputStreamKind,
}

pub struct DeadlinePollable<CpuImpl, RuntimeState> {
    cpu: CpuImpl,
    runtime_state: RuntimeState,
    deadline_nanos: u64,
    uptime_nanos: fn(&RuntimeState, u64) -> u64,
}

impl<CpuImpl, RuntimeState, FileSystem> ComponentStoreData<CpuImpl, RuntimeState, FileSystem>
where
    CpuImpl: Cpu + Clone,
    RuntimeState: Clone + Send + 'static,
    FileSystem: Send,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cpu: CpuImpl,
        runtime_state: RuntimeState,
        instance_registry: InstanceRegistry,
        instance: RegisteredInstance,
        debug_port: Option<()>,
        filesystem: FileSystem,
        arguments: Vec<String>,
        environment: Vec<(String, String)>,
        output_mode: ComponentOutputMode,
        serial_writer: fn(&[u8]),
        uptime_nanos: fn(&RuntimeState, u64) -> u64,
        record_console_text: fn(&RuntimeState, u64, &str),
    ) -> Self {
        Self {
            table: ResourceTable::new(),
            cpu,
            runtime_state,
            instance_registry,
            instance,
            debug_port,
            filesystem,
            arguments,
            environment,
            output_mode,
            captured_stdout: Arc::new(Mutex::new(Vec::new())),
            captured_stderr: Arc::new(Mutex::new(Vec::new())),
            serial_writer,
            uptime_nanos,
            record_console_text,
        }
    }

    pub fn now_nanos(&self) -> u64 {
        (self.uptime_nanos)(&self.runtime_state, self.cpu.now().ticks())
    }

    pub fn write_output(&mut self, stream: ComponentOutputStreamKind, bytes: &[u8]) {
        ComponentOutput::from_store(self).write(stream, bytes);
    }

    pub fn take_captured_output(&self) -> ExecOutput {
        ExecOutput {
            stdout: self.captured_stdout.lock().clone(),
            stderr: self.captured_stderr.lock().clone(),
        }
    }

    pub fn record_call_hook(&mut self, hook: CallHook) {
        record_instance_call_hook(&self.instance, hook, self.now_nanos());
    }
}

impl<CpuImpl, RuntimeState, FileSystem> ResourceLimiter
    for ComponentStoreData<CpuImpl, RuntimeState, FileSystem>
where
    CpuImpl: Cpu + Clone,
    RuntimeState: Clone + Send,
    FileSystem: Send,
{
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(allow_instance_resource_growth(
            &self.instance,
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

impl<CpuImpl, RuntimeState, FileSystem> IoView for ComponentStoreData<CpuImpl, RuntimeState, FileSystem>
where
    CpuImpl: Cpu + Clone,
    RuntimeState: Clone + Send,
{
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl<CpuImpl, RuntimeState> ComponentOutput<CpuImpl, RuntimeState>
where
    CpuImpl: Cpu + Clone,
    RuntimeState: Clone + Send + 'static,
{
    fn from_store<FileSystem>(
        store: &ComponentStoreData<CpuImpl, RuntimeState, FileSystem>,
    ) -> Self {
        Self {
            mode: store.output_mode,
            runtime_state: store.runtime_state.clone(),
            cpu: store.cpu.clone(),
            captured_stdout: store.captured_stdout.clone(),
            captured_stderr: store.captured_stderr.clone(),
            serial_writer: store.serial_writer,
            record_console_text: store.record_console_text,
        }
    }

    fn write(&self, stream: ComponentOutputStreamKind, bytes: &[u8]) {
        match self.mode {
            ComponentOutputMode::Serial => (self.serial_writer)(bytes),
            ComponentOutputMode::Trace => {
                let text = core::str::from_utf8(bytes).unwrap_or_else(|error| {
                    panic!("guest attempted to write non-utf8 stdout/stderr bytes: {error}")
                });
                (self.record_console_text)(&self.runtime_state, self.cpu.now().ticks(), text);
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

impl<CpuImpl, RuntimeState> ComponentOutputStream<CpuImpl, RuntimeState>
where
    CpuImpl: Cpu + Clone,
    RuntimeState: Clone + Send + 'static,
{
    pub fn from_store<FileSystem>(
        store: &ComponentStoreData<CpuImpl, RuntimeState, FileSystem>,
        stream: ComponentOutputStreamKind,
    ) -> Self {
        Self {
            sink: ComponentOutput::from_store(store),
            stream,
        }
    }
}

impl<CpuImpl, RuntimeState> DeadlinePollable<CpuImpl, RuntimeState>
where
    CpuImpl: Cpu + Clone,
    RuntimeState: Clone + Send + 'static,
{
    pub fn new(
        cpu: CpuImpl,
        runtime_state: RuntimeState,
        deadline_nanos: u64,
        uptime_nanos: fn(&RuntimeState, u64) -> u64,
    ) -> Self {
        Self {
            cpu,
            runtime_state,
            deadline_nanos,
            uptime_nanos,
        }
    }
}

#[wasmtime_wasi_io::async_trait]
impl<CpuImpl, RuntimeState> Pollable for ComponentOutputStream<CpuImpl, RuntimeState>
where
    CpuImpl: Cpu + Clone,
    RuntimeState: Clone + Send + 'static,
{
    async fn ready(&mut self) {}
}

#[wasmtime_wasi_io::async_trait]
impl<CpuImpl, RuntimeState> OutputStream for ComponentOutputStream<CpuImpl, RuntimeState>
where
    CpuImpl: Cpu + Clone,
    RuntimeState: Clone + Send + 'static,
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
impl<CpuImpl, RuntimeState> Pollable for DeadlinePollable<CpuImpl, RuntimeState>
where
    CpuImpl: Cpu + Clone,
    RuntimeState: Clone + Send + 'static,
{
    async fn ready(&mut self) {
        while (self.uptime_nanos)(&self.runtime_state, self.cpu.now().ticks()) < self.deadline_nanos
        {
            core::hint::spin_loop();
        }
    }
}
