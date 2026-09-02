extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use bytes::Bytes;

use crate::io::{ByteReader, ByteWriter, TryRead};
use crate::{
    EntropyPool, InstanceExecutionTransition, InstanceRegistry, KernelClock, KillReason,
    ProcessAuthority, RegisteredInstance, SetWallClockCap, Sleep, Timer,
    nanos_to_ticks_ceil_saturating, record_instance_transition,
};
use helios_hal::cpu::{Cpu, Instant};
use thiserror::Error;

/// Error returned through the runtime call hook when a kill was
/// requested for the running instance — by the OOM killer or by a
/// kernel-plugin supervisor. The runtime adapter turns this into a trap and the
/// component executor surfaces it as a typed `ProgramExecError`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("instance terminated: {reason:?}")]
pub struct InstanceKilled {
    pub reason: KillReason,
}

/// Where stdin/stdout/stderr traffic flows for a component.
///
/// Three concrete routings are supported:
///
/// - `Serial`: debugger component path — stdout/stderr are written to the
///   serial debug port; stdin reads drain it.
/// - `Trace`: diagnostic path — stdout/stderr bytes are recorded as
///   observer console text; stdin is always empty.
/// - `Child { … }`: spawned-child path — stdin, stdout, and stderr are
///   connected to byte channels the parent controls.
#[derive(Clone)]
pub enum ComponentOutputRoute {
    Serial,
    Trace,
    Child(ByteWriter),
    Discard,
}

impl ComponentOutputRoute {
    /// Borrow this route as a sink so every writer path matches on one
    /// exhaustive set of destinations.
    pub fn sink(&self) -> ComponentOutputSink<'_> {
        match self {
            Self::Serial => ComponentOutputSink::Local(LocalOutputSink::Serial),
            Self::Trace => ComponentOutputSink::Local(LocalOutputSink::Trace),
            Self::Child(writer) => ComponentOutputSink::Child(writer),
            Self::Discard => ComponentOutputSink::Local(LocalOutputSink::Discard),
        }
    }
}

/// Borrowed resolution of where one stdio stream's bytes go.
///
/// The split is exactly the flow-control boundary: a [`LocalOutputSink`]
/// accepts bytes unconditionally and never blocks, while `Child` is a
/// bounded byte channel that applies backpressure — callers must either
/// await [`ByteWriter::write`] or drive [`ByteWriter::poll_write`] from a
/// poll context.
pub enum ComponentOutputSink<'a> {
    Local(LocalOutputSink),
    Child(&'a ByteWriter),
}

/// Output destinations that can always take one more chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalOutputSink {
    Serial,
    Trace,
    Discard,
}

impl LocalOutputSink {
    pub(crate) fn write<CpuImpl, RuntimeStateImpl>(
        self,
        cpu: &CpuImpl,
        runtime_state: &RuntimeStateImpl,
        serial_writer: fn(&[u8]),
        bytes: &[u8],
    ) where
        CpuImpl: Cpu + Clone,
        RuntimeStateImpl: ComponentRuntimeState,
    {
        match self {
            Self::Serial => serial_writer(bytes),
            Self::Trace => {
                let text = core::str::from_utf8(bytes).unwrap_or_else(|error| {
                    panic!("guest attempted to write non-utf8 stdout/stderr bytes: {error}")
                });
                runtime_state.record_console_text(cpu.now().ticks(), text);
            }
            Self::Discard => {}
        }
    }
}

#[derive(Clone)]
pub enum ComponentOutputMode {
    Serial,
    Trace,
    Child {
        stdin_rx: ByteReader,
        stdout_tx: ByteWriter,
        stderr_tx: ByteWriter,
    },
    RoutedChild {
        stdin_rx: ByteReader,
        stdout: ComponentOutputRoute,
        stderr: ComponentOutputRoute,
    },
}

impl ComponentOutputMode {
    /// Obtain a cloneable writer for the requested child stream, when
    /// this mode has one. Returns `None` for `Serial`/`Trace`; callers
    /// that need every route should use [`Self::sink`] instead.
    pub fn child_writer(&self, kind: ComponentOutputStreamKind) -> Option<ByteWriter> {
        match (self, kind) {
            (ComponentOutputMode::Child { stdout_tx, .. }, ComponentOutputStreamKind::Stdout) => {
                Some(stdout_tx.clone())
            }
            (ComponentOutputMode::Child { stderr_tx, .. }, ComponentOutputStreamKind::Stderr) => {
                Some(stderr_tx.clone())
            }
            (
                ComponentOutputMode::RoutedChild {
                    stdout: ComponentOutputRoute::Child(writer),
                    ..
                },
                ComponentOutputStreamKind::Stdout,
            )
            | (
                ComponentOutputMode::RoutedChild {
                    stderr: ComponentOutputRoute::Child(writer),
                    ..
                },
                ComponentOutputStreamKind::Stderr,
            ) => Some(writer.clone()),
            _ => None,
        }
    }

    /// Resolve where `kind` goes without cloning a writer handle.
    pub fn sink(&self, kind: ComponentOutputStreamKind) -> ComponentOutputSink<'_> {
        match (self, kind) {
            (ComponentOutputMode::Serial, _) => ComponentOutputSink::Local(LocalOutputSink::Serial),
            (ComponentOutputMode::Trace, _) => ComponentOutputSink::Local(LocalOutputSink::Trace),
            (ComponentOutputMode::Child { stdout_tx, .. }, ComponentOutputStreamKind::Stdout) => {
                ComponentOutputSink::Child(stdout_tx)
            }
            (ComponentOutputMode::Child { stderr_tx, .. }, ComponentOutputStreamKind::Stderr) => {
                ComponentOutputSink::Child(stderr_tx)
            }
            (
                ComponentOutputMode::RoutedChild { stdout, .. },
                ComponentOutputStreamKind::Stdout,
            ) => stdout.sink(),
            (
                ComponentOutputMode::RoutedChild { stderr, .. },
                ComponentOutputStreamKind::Stderr,
            ) => stderr.sink(),
        }
    }

    /// Obtain a reference to the child-stdin reader, when this mode has one.
    pub fn child_stdin(&self) -> Option<&ByteReader> {
        match self {
            ComponentOutputMode::Child { stdin_rx, .. }
            | ComponentOutputMode::RoutedChild { stdin_rx, .. } => Some(stdin_rx),
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

    /// Nanoseconds between the monotonic clock and wall time, as the
    /// platform's real-time clock set them at boot.
    ///
    /// Zero on a machine whose kernel found no real-time clock, where
    /// wall time cannot be told apart from uptime.
    fn wall_clock_offset_nanos(&self) -> i128;

    fn record_console_text(&self, current_ticks: u64, text: &str);

    /// The kernel's root DRBG, from which this instance's pool is
    /// derived.
    fn root_entropy(&self) -> &crate::RootEntropy;

    fn profiling_enabled(&self) -> bool;

    fn record_profile_stack_nanos(
        &self,
        scope: crate::ProfileScope,
        stack: String,
        weight_nanos: u64,
    );

    fn record_profile_stack_parts_nanos(
        &self,
        scope: crate::ProfileScope,
        prefix: &str,
        suffix: &str,
        weight_nanos: u64,
    );

    fn record_perf_metric_parts_nanos(
        &self,
        scope: crate::ProfileScope,
        prefix: &str,
        suffix: &str,
        elapsed_nanos: u64,
        counters: helios_hal::cpu::HardwarePerfCounterDelta,
        bytes: u64,
    );

    fn record_perf_metric_parts_events_nanos(
        &self,
        scope: crate::ProfileScope,
        prefix: &str,
        suffix: &str,
        events: u64,
        elapsed_nanos: u64,
        counters: helios_hal::cpu::HardwarePerfCounterDelta,
        bytes: u64,
    );
}

pub(crate) struct ComponentExecutionContext<FileSystem> {
    instance: RegisteredInstance,
    debug_port: Option<()>,
    filesystem: FileSystem,
    arguments: Vec<String>,
    environment: Vec<(String, String)>,
    process_authority: ProcessAuthority,
    output_mode: ComponentOutputMode,
}

pub struct ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem, ResourceTableImpl>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    pub table: ResourceTableImpl,
    pub cpu: CpuImpl,
    timer: Timer<CpuImpl>,
    spawner: crate::Spawner<CpuImpl>,
    pub runtime_state: RuntimeStateImpl,
    pub instance_registry: InstanceRegistry,
    entropy: EntropyPool,
    clock: KernelClock<CpuImpl, RuntimeStateImpl>,
    execution_context: ComponentExecutionContext<FileSystem>,
    serial_reader: crate::SerialReader,
    serial_writer: fn(&[u8]),
    /// Set by the runtime exit interface before the guest
    /// traps; the executor reads it to distinguish a clean requested
    /// exit (turn into an exit code) from an actual runtime error.
    requested_exit: Option<u8>,
}

pub struct DeadlinePollable<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    cpu: CpuImpl,
    timer: Timer<CpuImpl>,
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
        process_authority: ProcessAuthority,
        output_mode: ComponentOutputMode,
    ) -> Self {
        Self {
            instance,
            debug_port,
            filesystem,
            arguments,
            environment,
            process_authority,
            output_mode,
        }
    }
}

impl<CpuImpl, RuntimeStateImpl, FileSystem, ResourceTableImpl>
    ComponentStoreData<CpuImpl, RuntimeStateImpl, FileSystem, ResourceTableImpl>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
    FileSystem: Send,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        table: ResourceTableImpl,
        cpu: CpuImpl,
        timer: Timer<CpuImpl>,
        spawner: crate::Spawner<CpuImpl>,
        runtime_state: RuntimeStateImpl,
        instance_registry: InstanceRegistry,
        instance: RegisteredInstance,
        debug_port: Option<()>,
        filesystem: FileSystem,
        arguments: Vec<String>,
        environment: Vec<(String, String)>,
        process_authority: ProcessAuthority,
        output_mode: ComponentOutputMode,
        serial_reader: crate::SerialReader,
        serial_writer: fn(&[u8]),
    ) -> Self {
        let entropy = EntropyPool::derive(runtime_state.root_entropy(), instance.id().raw());
        let clock = KernelClock::new(cpu.clone(), runtime_state.clone());
        Self {
            table,
            cpu,
            timer,
            spawner,
            runtime_state,
            instance_registry,
            entropy,
            clock,
            execution_context: ComponentExecutionContext::new(
                instance,
                debug_port,
                filesystem,
                arguments,
                environment,
                process_authority,
                output_mode,
            ),
            serial_reader,
            serial_writer,
            requested_exit: None,
        }
    }

    /// Record the exit code requested by the runtime exit interface so the
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

    pub(crate) fn process_authority(&self) -> &ProcessAuthority {
        &self.execution_context.process_authority
    }

    pub fn output_mode(&self) -> &ComponentOutputMode {
        &self.execution_context.output_mode
    }

    pub(crate) fn serial_reader_fn(&self) -> crate::SerialReader {
        self.serial_reader
    }

    pub(crate) fn serial_writer_fn(&self) -> fn(&[u8]) {
        self.serial_writer
    }

    pub fn now_nanos(&self) -> u64 {
        self.clock.monotonic_nanos()
    }

    pub fn system_time_nanos(&self) -> u64 {
        self.clock.system_time_nanos()
    }

    pub fn set_system_time_nanos(&mut self, cap: &SetWallClockCap, nanos: u64) {
        self.clock.set_system_time_nanos(cap, nanos);
    }

    pub fn timer(&self) -> Timer<CpuImpl> {
        self.timer.clone()
    }

    pub fn sleep_for(&self, duration: core::time::Duration) -> Sleep<CpuImpl> {
        self.timer.sleep_for(duration)
    }

    pub fn spawner(&self) -> &crate::Spawner<CpuImpl> {
        &self.spawner
    }

    pub(crate) fn fill_secure_random(&mut self, buffer: &mut [u8]) {
        self.entropy.fill_secure(buffer);
    }

    pub(crate) fn secure_random_u64(&mut self) -> u64 {
        self.entropy.secure_u64()
    }

    pub(crate) fn insecure_random_bytes(&mut self, len: usize) -> Vec<u8> {
        self.entropy.insecure_bytes(len)
    }

    pub(crate) fn insecure_random_u64(&mut self) -> u64 {
        self.entropy.insecure_u64()
    }

    pub(crate) fn insecure_seed(&mut self) -> (u64, u64) {
        self.entropy.insecure_seed()
    }

    /// Deliver an owned stdout/stderr chunk from a synchronous poll
    /// context, honouring child-channel backpressure.
    ///
    /// Serial, trace, and discard sinks complete immediately. A child pipe
    /// that is at capacity parks the caller: `pending` keeps the chunk and
    /// `wait` keeps the registration, so the next poll retries the very
    /// same bytes once the parent has drained. Nothing is dropped and a
    /// full pipe is never reported as an error — a vanished reader is
    /// swallowed like a POSIX write to a closed pipe with SIGPIPE
    /// suppressed.
    pub fn poll_write_output_bytes(
        &self,
        stream: ComponentOutputStreamKind,
        cx: &mut core::task::Context<'_>,
        wait: &mut Option<crate::ByteWriteWait>,
        pending: &mut Option<Bytes>,
    ) -> core::task::Poll<()> {
        let Some(bytes) = pending.as_ref() else {
            return core::task::Poll::Ready(());
        };
        if bytes.is_empty() {
            *pending = None;
            return core::task::Poll::Ready(());
        }
        match self.execution_context.output_mode.sink(stream) {
            ComponentOutputSink::Local(local) => {
                let bytes = pending.take().expect("the chunk was present a line ago");
                local.write(&self.cpu, &self.runtime_state, self.serial_writer, &bytes);
                core::task::Poll::Ready(())
            }
            ComponentOutputSink::Child(writer) => {
                let wait = wait.get_or_insert_with(|| writer.wait_state());
                writer.poll_write(cx, wait, pending).map(|_closed_or_ok| ())
            }
        }
    }

    /// Non-blocking drain of whatever stdin bytes are currently buffered,
    /// up to `max_bytes`.  Returns an empty `Vec` when the port is idle so
    /// callers can yield to the kernel executor.  For `Child` mode, the
    /// reader half of the parent-provided channel is polled once.
    pub fn try_read_stdin(&self, max_bytes: u32) -> Vec<u8> {
        match &self.execution_context.output_mode {
            ComponentOutputMode::Serial => {
                let mut bytes = Vec::new();
                (self.serial_reader)(&mut bytes, max_bytes);
                bytes
            }
            ComponentOutputMode::Trace => Vec::new(),
            ComponentOutputMode::Child { stdin_rx, .. }
            | ComponentOutputMode::RoutedChild { stdin_rx, .. } => match stdin_rx.try_read() {
                TryRead::Ready(mut bytes) => {
                    let cap = max_bytes as usize;
                    if bytes.len() > cap {
                        bytes = bytes.slice(..cap);
                    }
                    bytes.to_vec()
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
                    let mut bytes = Vec::new();
                    (self.serial_reader)(&mut bytes, u32::MAX);
                    if !bytes.is_empty() {
                        return Some(bytes);
                    }
                    crate::yield_now().await;
                }
            }
            ComponentOutputMode::Trace => None,
            ComponentOutputMode::Child { stdin_rx, .. }
            | ComponentOutputMode::RoutedChild { stdin_rx, .. } => {
                stdin_rx.read().await.map(|bytes| bytes.to_vec())
            }
        }
    }

    pub fn write_serial(&self, bytes: &[u8]) {
        (self.serial_writer)(bytes);
    }

    /// Raw read of the serial debug port, regardless of this component's
    /// configured stdio routing. Used by `helios:system/serial.read` and
    /// the debugger shell to drain user input directly from hardware.
    pub fn try_read_serial_port(&self, max_bytes: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        (self.serial_reader)(&mut bytes, max_bytes);
        bytes
    }

    pub fn record_transition(&mut self, transition: InstanceExecutionTransition) {
        let now_nanos = self.now_nanos();
        let elapsed = record_instance_transition(self.instance(), transition, now_nanos);
        if let Some(elapsed) = elapsed
            && self.runtime_state.profiling_enabled()
        {
            self.runtime_state.record_profile_stack_parts_nanos(
                crate::ProfileScope::User,
                "user;",
                self.instance().name(),
                elapsed,
            );
        }
    }

    /// Returns the recorded kill reason if the OOM killer or a
    /// supervisor flagged this instance for termination since the last
    /// host-call boundary. The component executor calls this from
    /// `call_hook` and surfaces the value as an `InstanceKilled` trap.
    pub fn check_pending_kill(&self) -> Option<KillReason> {
        self.instance().pending_kill()
    }
}

impl<CpuImpl, RuntimeStateImpl> DeadlinePollable<CpuImpl, RuntimeStateImpl>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    pub fn new(
        cpu: CpuImpl,
        timer: Timer<CpuImpl>,
        runtime_state: RuntimeStateImpl,
        deadline_nanos: u64,
    ) -> Self {
        Self {
            cpu,
            timer,
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

    pub async fn ready(&mut self) {
        let timer = self.timer.clone();
        let cpu = self.cpu.clone();
        let runtime_state = self.runtime_state.clone();
        wait_until_runtime_deadline(timer, cpu, runtime_state, self.deadline_nanos).await;
    }
}

pub async fn wait_until_runtime_deadline<CpuImpl, RuntimeStateImpl>(
    timer: Timer<CpuImpl>,
    cpu: CpuImpl,
    runtime_state: RuntimeStateImpl,
    deadline_nanos: u64,
) where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
{
    loop {
        let now_ticks = cpu.now().ticks();
        let now_nanos = runtime_state.uptime_nanos(now_ticks);
        if now_nanos >= deadline_nanos {
            return;
        }

        let remaining_nanos = deadline_nanos.saturating_sub(now_nanos);
        let remaining_ticks =
            nanos_to_ticks_ceil_saturating(remaining_nanos, cpu.timer_frequency()).max(1);
        timer
            .sleep_until(Instant::new(now_ticks.saturating_add(remaining_ticks)))
            .await;
    }
}

// Runtime-adapter trait impls live in the concrete adapter module.
