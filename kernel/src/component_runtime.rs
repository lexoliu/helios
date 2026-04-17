extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use crate::child_io::{ByteReader, ByteWriter, ClosedPeer, TryRead};
use crate::{
    InstanceExecutionTransition, InstanceRegistry, RegisteredInstance, record_instance_transition,
};
use helios_hal::cpu::Cpu;
use wasmtime::component::ResourceTable;

/// Where `wasi:cli/{stdin,stdout,stderr}` traffic flows for a component.
///
/// Three concrete routings are supported:
///
/// - `Serial`: debugger component path — stdout/stderr are written to the
///   serial debug port; stdin reads drain it.
/// - `Trace`: diagnostic path — stdout/stderr bytes are recorded as
///   observer console text; stdin is always empty.
/// - `Child { … }`: spawned-child path — stdin, stdout, and stderr are
///   connected to byte channels the parent controls.
pub enum ComponentOutputMode {
    Serial,
    Trace,
    Child {
        stdin_rx: ByteReader,
        stdout_tx: ByteWriter,
        stderr_tx: ByteWriter,
    },
}

impl ComponentOutputMode {
    /// Obtain a cloneable writer for the requested child stream, when
    /// this mode has one. Returns `None` for `Serial`/`Trace`; callers
    /// should dispatch through `ComponentStoreData::write_output` for
    /// those routes.
    pub fn child_writer(&self, kind: ComponentOutputStreamKind) -> Option<ByteWriter> {
        match (self, kind) {
            (ComponentOutputMode::Child { stdout_tx, .. }, ComponentOutputStreamKind::Stdout) => {
                Some(stdout_tx.clone())
            }
            (ComponentOutputMode::Child { stderr_tx, .. }, ComponentOutputStreamKind::Stderr) => {
                Some(stderr_tx.clone())
            }
            _ => None,
        }
    }

    /// Obtain a reference to the child-stdin reader, when this mode has one.
    pub fn child_stdin(&self) -> Option<&ByteReader> {
        match self {
            ComponentOutputMode::Child { stdin_rx, .. } => Some(stdin_rx),
            _ => None,
        }
    }
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
}

pub struct ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem> {
    pub table: ResourceTable,
    pub cpu: CpuImpl,
    pub runtime_state: RuntimeStateImpl,
    pub instance_registry: InstanceRegistry,
    execution_context: ComponentExecutionContext<FileSystem>,
    serial_reader: fn(u32) -> Vec<u8>,
    serial_writer: fn(&[u8]),
    /// Set by `wasi:cli/exit.{exit,exit-with-code}` before the guest
    /// traps; the executor reads it to distinguish a clean requested
    /// exit (turn into an exit code) from an actual runtime error.
    requested_exit: Option<u8>,
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
            requested_exit: None,
        }
    }

    /// Record the exit code requested by `wasi:cli/exit` so the
    /// executor can report it cleanly after the guest trap.
    pub fn request_exit(&mut self, code: u8) {
        self.requested_exit = Some(code);
    }

    /// Consume any exit code recorded before the guest trapped.
    pub fn take_requested_exit(&mut self) -> Option<u8> {
        self.requested_exit.take()
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

    pub fn output_mode(&self) -> &ComponentOutputMode {
        &self.execution_context.output_mode
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

    /// Deliver a chunk of stdout/stderr bytes to whatever sink is
    /// configured for this component. Never blocks.
    pub fn write_output(&self, stream: ComponentOutputStreamKind, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        match &self.execution_context.output_mode {
            ComponentOutputMode::Serial => (self.serial_writer)(bytes),
            ComponentOutputMode::Trace => {
                let text = core::str::from_utf8(bytes).unwrap_or_else(|error| {
                    panic!("guest attempted to write non-utf8 stdout/stderr bytes: {error}")
                });
                self.runtime_state
                    .record_console_text(self.cpu.now().ticks(), text);
            }
            ComponentOutputMode::Child {
                stdout_tx,
                stderr_tx,
                ..
            } => {
                let writer = match stream {
                    ComponentOutputStreamKind::Stdout => stdout_tx,
                    ComponentOutputStreamKind::Stderr => stderr_tx,
                };
                // Ignore ClosedPeer: the parent dropped the reader, so
                // further writes are simply discarded. This matches POSIX
                // behavior when writing to a closed pipe with SIGPIPE
                // suppressed.
                let _: Result<(), ClosedPeer> = writer.write(bytes.to_vec());
            }
        }
    }

    /// Non-blocking drain of whatever stdin bytes are currently buffered,
    /// up to `max_bytes`.  Returns an empty `Vec` when the port is idle so
    /// callers can yield to the kernel executor.  For `Child` mode, the
    /// reader half of the parent-provided channel is polled once.
    pub fn try_read_stdin(&self, max_bytes: u32) -> Vec<u8> {
        match &self.execution_context.output_mode {
            ComponentOutputMode::Serial => (self.serial_reader)(max_bytes),
            ComponentOutputMode::Trace => Vec::new(),
            ComponentOutputMode::Child { stdin_rx, .. } => match stdin_rx.try_read() {
                TryRead::Ready(mut bytes) => {
                    let cap = max_bytes as usize;
                    if bytes.len() > cap {
                        bytes.truncate(cap);
                    }
                    bytes
                }
                TryRead::Pending | TryRead::Eof => Vec::new(),
            },
        }
    }

    /// Await the next stdin chunk, yielding the executor between polls.
    /// Returns `None` on EOF (Child mode after parent closes stdin, or
    /// for Trace mode which never produces bytes).
    pub async fn await_stdin_chunk(&self) -> Option<Vec<u8>> {
        match &self.execution_context.output_mode {
            ComponentOutputMode::Serial => {
                // Busy-poll with yield_now between polls — the serial
                // port is a raw hardware reader without async wakeup.
                loop {
                    let bytes = (self.serial_reader)(u32::MAX);
                    if !bytes.is_empty() {
                        return Some(bytes);
                    }
                    crate::yield_now().await;
                }
            }
            ComponentOutputMode::Trace => None,
            ComponentOutputMode::Child { stdin_rx, .. } => stdin_rx.read().await,
        }
    }

    pub fn write_serial(&self, bytes: &[u8]) {
        (self.serial_writer)(bytes);
    }

    /// Raw read of the serial debug port, regardless of this component's
    /// configured stdio routing. Used by `helios:system/serial.read` and
    /// the debugger shell to drain user input directly from hardware.
    pub fn try_read_serial_port(&self, max_bytes: u32) -> Vec<u8> {
        (self.serial_reader)(max_bytes)
    }

    pub fn record_transition(&mut self, transition: InstanceExecutionTransition) {
        record_instance_transition(self.instance(), transition, self.now_nanos());
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

    pub fn uptime_nanos(&self) -> u64 {
        self.runtime_state.uptime_nanos(self.cpu.now().ticks())
    }

    pub fn deadline_nanos(&self) -> u64 {
        self.deadline_nanos
    }
}

// Wasmtime trait impls (Pollable, OutputStream, ResourceLimiter, IoView)
// live in wasmtime_adapter/store.rs.
