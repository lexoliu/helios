extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use bytes::Bytes;

use crate::io::{ByteReader, ByteWriter, TryRead};
use crate::{
    ActivityChange, EntropyPool, InstanceActivity, InstanceExecutionTransition, InstanceRegistry,
    KernelClock, KillReason, ProcessAuthority, RegisteredInstance, SetWallClockCap, Sleep, Timer,
    nanos_to_ticks_ceil_saturating,
};
use helios_hal::cpu::{Cpu, Instant, current_processor};

use crate::memory::{MemoryOwner, set_user_memory_owner, user_mapping_kernel_heap_bytes};
use thiserror::Error;

/// The stack a component instance runs on: both the guest wasm call
/// stack and the host Rust async frames driving it.
///
/// CPython's class construction recurses deeply and does not fit
/// Wasmtime's 512 KiB default. This is kernel policy rather than a
/// runtime knob — the kernel is what pays for the stack, in user pages
/// for the stack itself and in kernel heap for the page tables that
/// address it — so the runtime adapter's engine configuration reads it
/// from here.
pub const COMPONENT_ASYNC_STACK_SIZE: usize = 8 * 1024 * 1024;

/// How many component instances the kernel serves at once.
///
/// This is kernel policy and belongs here for the same reason
/// [`COMPONENT_ASYNC_STACK_SIZE`] does: the kernel is what pays for a
/// live instance, and the runtime adapter reads the number rather than
/// choosing it. Until #284 the number was Wasmtime's own default — one
/// value, 1000, that its `InstanceLimits` assigns to every total it has,
/// including the *core* instance count, which is not the same thing as a
/// program: `instance-startup-500` never once completed on any bench run
/// because 500 programs draw more than 1000 core instances between them.
///
/// The cap the runtime is told is not the real limit. What actually
/// bounds concurrency is user memory, and the instance that cannot get
/// its pages dies as any other user OOM does (AGENTS §3.1); this number
/// exists so that the refusal comes from the memory the kernel accounts
/// for rather than from a pool sized by a default nobody chose.
pub const MAX_CONCURRENT_INSTANCES: u32 = 1024;

/// The most core instances one component may instantiate.
///
/// Measured across every component this tree builds: `hello` and the
/// curl program instantiate three core modules each — the component
/// tooling's shim, the main module and the preview1 adapter — and
/// `python3` four. Eight leaves room for a component that carries more
/// without letting one draw the pool down.
pub const MAX_CORE_INSTANCES_PER_COMPONENT: u32 = 8;

/// The most linear memories one component may hold.
///
/// Every component in the tree defines exactly one, including the
/// threaded ones: a guest's threads share its single memory rather than
/// adding a second. Two is the multi-memory headroom, and it is what
/// each unit of [`MAX_CONCURRENT_INSTANCES`] reserves address space for,
/// so it is deliberately tight.
pub const MAX_MEMORIES_PER_COMPONENT: u32 = 2;

/// The most tables one component may hold.
///
/// Measured at exactly two for every component this tree builds: the
/// main module's function table and the shim's. There is no headroom
/// here on purpose — the table pool is the one pool the runtime commits
/// up front rather than reserving, so this number is paid in user pages
/// per live slot whether or not any component uses it. See
/// [`MAX_POOLED_USER_MEMORY`].
pub const MAX_TABLES_PER_COMPONENT: u32 = 2;

/// The virtual address space the kernel will let the runtime's instance
/// pools reserve.
///
/// A pooled linear-memory slot reserves
/// `CWASM_MEMORY_RESERVATION + CWASM_MEMORY_GUARD_SIZE` of address space
/// whether or not anything ever runs in it, so the instance budget is
/// spent in address space long before it is spent in pages. Every
/// bare-metal backend hands the runtime a 32 TiB window
/// (`USER_VA_BASE..USER_VA_END` in `x86/src/vmm.rs`,
/// `aarch64/src/vmm.rs` and `riscv/src/vmm.rs`), and the kernel keeps
/// its pools inside half of it so that everything else mapped out of the
/// same window — compiled code, the fiber stacks, a guest's own growth —
/// still has somewhere to go. The pools the budget below implies reserve
/// about 8.1 TiB of that.
///
/// This is the number to check before widening
/// [`MAX_CONCURRENT_INSTANCES`] or [`MAX_MEMORIES_PER_COMPONENT`]; the
/// runtime adapter asserts the derived pools fit.
pub const MAX_POOLED_ADDRESS_SPACE: u64 = 16 << 40;

/// The user memory one engine's instance pools may commit up front.
///
/// Reserved address space is not the only thing a pool costs, and the
/// exception is what made the first draft of the instance budget a
/// regression rather than a fix. The linear-memory, stack and GC-heap
/// pools all take their space as they use it — the memory pool maps its
/// whole slab inaccessible, the bare-metal stack pool is a counter — but
/// the **table pool commits its entire mapping when the engine is
/// built**, one page-rounded `table_elements`-sized slot per
/// `total_tables`. On the bench lane that is user memory gone before a
/// program runs: run 34224973216 refused `cpython-json` its 40 MiB with
/// `available=34942976 of 1510993920`, because a fourfold table budget
/// had committed 1.3 GiB of the 1.44 GiB the guest had.
///
/// It is per *engine*, and the kernel builds one per component-host
/// consumer — two today, the host and its service path — so the figure
/// to compare against a guest's budget is twice this.
pub const MAX_POOLED_USER_MEMORY: u64 = 128 << 20;

/// The kernel heap one wasm store costs the kernel.
///
/// Two terms, and both are per *store* rather than per instance, which
/// is what makes an instance's kernel-heap footprint track the stores
/// it holds — a guest thread gets its own store over the same instance
/// — instead of only the linear memory it grew:
///
/// - `store_bytes`, the store value itself. Every service the guest can
///   reach hangs off it: the resource and descriptor tables, preopened
///   directories, signal state, stdio plumbing, the entropy pool. The
///   kernel owns the whole thing on its own heap.
/// - The page tables and reservation records for the fiber stack the
///   store runs on. The stack's pages come from the user pool and
///   arrive one page-fault at a time, but the kernel heap pays to
///   address the whole span the moment the stack is created: the
///   arena's `prepare_demand_commit` builds every page-table level over
///   [`COMPONENT_ASYNC_STACK_SIZE`] up front, because a fault-time
///   commit cannot build one. So this term is the store's true
///   kernel-heap cost rather than a stand-in for resident user pages,
///   which are now only the pages the store actually touched.
pub fn store_kernel_heap_bytes(store_bytes: usize) -> u64 {
    let bytes =
        store_bytes.saturating_add(user_mapping_kernel_heap_bytes(COMPONENT_ASYNC_STACK_SIZE));
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

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
/// never parks the caller, while `Child` is a bounded byte channel that
/// applies backpressure — callers must either await
/// [`ByteWriter::write`] or drive [`ByteWriter::poll_write`] from a poll
/// context.
///
/// Neither half takes bytes unconditionally. A local serial sink is the
/// machine's one debug UART, whose owner may be another processor
/// mid-segment (#103), so it reports how much of the chunk it took and
/// the caller yields for the rest.
pub enum ComponentOutputSink<'a> {
    Local(LocalOutputSink),
    Child(&'a ByteWriter),
}

/// Output destinations the kernel serves itself, without a channel
/// between the guest and the bytes' destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalOutputSink {
    Serial,
    Trace,
    Discard,
}

impl LocalOutputSink {
    /// Takes what it can of `bytes` and reports how many it took.
    ///
    /// Zero means the debug UART is owned by another processor right
    /// now: the caller yields and offers the same bytes again. A short
    /// count means the console cut the stream at a line boundary, which
    /// is the granularity at which a kernel console record may reach
    /// the wire between the guest's own lines.
    #[must_use]
    pub(crate) fn write<CpuImpl, RuntimeStateImpl>(
        self,
        cpu: &CpuImpl,
        runtime_state: &RuntimeStateImpl,
        serial_writer: crate::DebugSerialWriter,
        bytes: &[u8],
    ) -> usize
    where
        CpuImpl: Cpu + Clone,
        RuntimeStateImpl: ComponentRuntimeState,
    {
        match self {
            Self::Serial => serial_writer.write_stream(bytes),
            Self::Trace => {
                let text = core::str::from_utf8(bytes).unwrap_or_else(|error| {
                    panic!("guest attempted to write non-utf8 stdout/stderr bytes: {error}")
                });
                runtime_state.record_console_text(cpu.now().ticks(), text);
                bytes.len()
            }
            Self::Discard => bytes.len(),
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

    /// The memory balloon the host resizes this guest through, if the
    /// platform has one. The out-of-memory path asks it to give its
    /// memory back before it condemns an instance.
    fn memory_balloon(&self) -> Option<crate::memory::BalloonHandle>;

    /// The devices discovery is willing to hand to a user-mode driver.
    ///
    /// Empty on a machine whose backend found nothing outside the
    /// hardware it drives itself, which is every machine until a
    /// backend publishes a grant.
    fn device_grants(&self) -> &crate::device::DeviceGrantRegistry;

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

    fn record_perf_metric_parts(
        &self,
        scope: crate::ProfileScope,
        prefix: &str,
        suffix: &str,
        sample: crate::PerfSample,
    );

    /// Retires the network handles that a dying socket resource could
    /// not retire itself, and wakes the packet pump if any did.
    ///
    /// The runtime state is the one value that holds the machine's
    /// network service, so it is where a queue of bare handle ids turns
    /// back into closes. See [`super::SocketRetirementQueue`] for why
    /// a socket resource cannot do it itself.
    fn retire_network_handles(&self, retired: &super::SocketRetirementQueue);
}

pub(crate) struct ComponentExecutionContext<FileSystem> {
    activity: InstanceActivity,
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
    /// The handles of socket resources that have died, and the runtime
    /// state that closes them.
    ///
    /// Declared after `table` on purpose: the table is torn down first,
    /// so every socket resource still in it has already pushed what it
    /// owned by the time this field's `Drop` drains the queue.
    pub retirement: super::StoreSocketRetirement<RuntimeStateImpl>,
    pub cpu: CpuImpl,
    timer: Timer<CpuImpl>,
    spawner: crate::InstanceSpawner<CpuImpl>,
    pub runtime_state: RuntimeStateImpl,
    pub instance_registry: InstanceRegistry,
    entropy: EntropyPool,
    clock: KernelClock<CpuImpl, RuntimeStateImpl>,
    execution_context: ComponentExecutionContext<FileSystem>,
    serial_reader: crate::SerialReader,
    serial_writer: crate::DebugSerialWriter,
    /// Where this instance's linear memory sits and the device it
    /// holds, if any. Empty on every instance that never asks for one,
    /// which is every instance that is not a driver.
    pub device: crate::device::DeviceOwnership,
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
        store_kernel_heap_bytes: u64,
        debug_port: Option<()>,
        filesystem: FileSystem,
        arguments: Vec<String>,
        environment: Vec<(String, String)>,
        process_authority: ProcessAuthority,
        output_mode: ComponentOutputMode,
    ) -> Self {
        Self {
            activity: InstanceActivity::new(instance, store_kernel_heap_bytes),
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
        spawner: crate::InstanceSpawner<CpuImpl>,
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
        serial_writer: crate::DebugSerialWriter,
    ) -> Self {
        let entropy = EntropyPool::derive(runtime_state.root_entropy(), instance.id().raw());
        let clock = KernelClock::new(cpu.clone(), runtime_state.clone());
        Self {
            table,
            retirement: super::StoreSocketRetirement::new(runtime_state.clone()),
            cpu,
            timer,
            spawner,
            runtime_state,
            instance_registry,
            entropy,
            clock,
            execution_context: ComponentExecutionContext::new(
                instance,
                store_kernel_heap_bytes(size_of::<Self>()),
                debug_port,
                filesystem,
                arguments,
                environment,
                process_authority,
                output_mode,
            ),
            serial_reader,
            serial_writer,
            device: crate::device::DeviceOwnership::new(),
            requested_exit: None,
        }
    }

    /// Retires every network handle a dying socket resource queued
    /// since the last drain.
    ///
    /// Called at the entry to every host call, and again right after a
    /// socket resource is deleted so that the common case — a guest
    /// closing its own connection — retires on that very call.
    pub fn retire_sockets(&self) {
        self.retirement.drain();
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
        self.execution_context.activity.instance()
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

    /// The kernel's writer for the machine's debug UART.
    pub(crate) fn serial_writer(&self) -> crate::DebugSerialWriter {
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

    pub fn spawner(&self) -> &crate::InstanceSpawner<CpuImpl> {
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
                let chunk = pending.take().expect("the chunk was present a line ago");
                let taken = local.write(&self.cpu, &self.runtime_state, self.serial_writer, &chunk);
                if taken == chunk.len() {
                    return core::task::Poll::Ready(());
                }
                // The debug UART's owner is another processor. Keep what
                // is left and come back for it: there is no readiness
                // signal to register on a port that is written to, so
                // the wake is this task's own, which is what
                // `yield_now` does from a poll context.
                *pending = Some(chunk.slice(taken..));
                cx.waker().wake_by_ref();
                core::task::Poll::Pending
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

    /// Raw read of the serial debug port, regardless of this component's
    /// configured stdio routing. Used by `helios:system/serial.read` and
    /// the debugger shell to drain user input directly from hardware.
    pub fn try_read_serial_port(&self, max_bytes: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        (self.serial_reader)(&mut bytes, max_bytes);
        bytes
    }

    /// Report the transition wasmtime's call hook delivered, and name
    /// the instance whose guest code this processor is running so pages
    /// the runtime commits while it runs — a `memory.grow`, most of all
    /// — are charged to it.
    ///
    /// A call hook cannot hold a scope guard across the stretch of
    /// guest code it opens; the guard is the store's own
    /// [`InstanceActivity`], which owns the pairing between the
    /// transitions and hands back what changed.
    ///
    /// Returns the kill reason when the OOM killer or a supervisor has
    /// condemned this instance — and in that case the activation has
    /// already been ended, so the caller's only remaining job is to
    /// raise the `InstanceKilled` trap. The two cannot be separated:
    /// see [`InstanceActivity::record`].
    pub fn record_transition(
        &mut self,
        transition: InstanceExecutionTransition,
    ) -> Option<KillReason> {
        let now_nanos = self.now_nanos();
        let step = self
            .execution_context
            .activity
            .record(transition, now_nanos);
        self.apply_activity_change(step.change);
        step.killed
    }

    /// End this store's activation and report the condemnation when the
    /// instance has one. The epoch callback uses this: a CPU-bound
    /// guest never reaches a call hook, and the trap raised there
    /// unwinds through the same hooks a call-hook kill does.
    pub fn end_on_pending_kill(&mut self) -> Option<KillReason> {
        let reason = self.execution_context.activity.pending_kill()?;
        let now_nanos = self.now_nanos();
        let change = self.execution_context.activity.end(now_nanos);
        self.apply_activity_change(change);
        Some(reason)
    }

    fn apply_activity_change(&self, change: ActivityChange) {
        name_user_memory_owner(self.instance(), change);
        if let ActivityChange::Left {
            instance_elapsed: Some(elapsed),
        } = change
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

/// Charges pages committed on this processor to `instance` while its
/// guest code runs, and to nobody once this store leaves it.
fn name_user_memory_owner(instance: &crate::RegisteredInstance, change: ActivityChange) {
    let owner = match change {
        ActivityChange::Entered => MemoryOwner::new(instance.id().raw()),
        ActivityChange::Left { .. } => MemoryOwner::NONE,
        ActivityChange::Unchanged => return,
    };
    set_user_memory_owner(current_processor(), owner);
}
