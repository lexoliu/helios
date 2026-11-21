//! The Linux process host: a static Linux executable launched through the
//! program service, as a wasm program is, run natively through the
//! platform's [`helios_hal::native`] capability.
//!
//! Each thread of a Linux process runs on its own executor task with its own
//! [`UserContext`] and [`Process`] handle, sharing one [`UserSpace`] and one
//! underlying process state (descriptor table, futex queues, program break
//! and process attributes). Tasks spawned by `clone` / `clone3` enter the
//! instance's global home-node queue (`InstanceSpawner::try_spawn_detached`),
//! so online processors steal and execute threads of one process concurrently
//! across SMP. Its exit status, or the signal that ended it, reaches the
//! parent through the child's exit channel once every thread of the group has
//! exited and dropped its handles.
//!
//! # Concurrency
//!
//! - User slices and address-space mutations synchronize inside [`UserSpace`],
//!   which shoots down stale translations across active processors before
//!   reclaiming physical frames; multi-step VM compositions serialize on the
//!   process's shared address-space lock in `helios-linux`.
//! - Process-wide state in [`LinuxShared`] uses lock-free atomics for summed
//!   processor ticks, fatal-error latch and live spawned-task count, plus a
//!   bounded non-awaiting `spin::Mutex` for wall-clock offset updates.
//! - Each thread task checks instance condemnation, thread-group exit and
//!   process signals before every user slice and again after arming its
//!   `NotifyWaiter`s around every suspended system call, so `exit_group`, a
//!   fatal fault, OOM, a signal or a kill wakes every parked sibling at once
//!   and stops running slices at the next timer preemption.

use super::*;
use crate::{SetWallClockCap, TerminalInputCap, TerminalOutputCap};
use core::num::NonZeroU32;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use helios_hal::native::{
    FaultKind, NativeError, NativeExecution, NativeUnavailable, Stop, UserContext, UserFault,
    UserSpace,
};
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{PageFlags, VirtAddr, VirtRange};
use helios_linux::abi::{self, Abi};
use helios_linux::elf::{self, Image};
use helios_linux::syscall::{
    self, Call, Denied, EpollData, Exit, MapError, MemoryFault, MemoryTotals, Process, Region,
    Resource, Resources, SpawnError, SpawnedThread, Start, Stdio, Stream, System, Thread,
    UserMemory, VirtualMemory, WriteError,
};

/// The ELF identification bytes every ELF file starts with (System V gABI,
/// `e_ident[EI_MAG0..EI_MAG3]`).
const ELF_MAGIC: &[u8] = b"\x7fELF";

/// Whether `bytes` are an ELF file, which only the Linux host runs.
pub(super) fn is_linux_elf(bytes: &[u8]) -> bool {
    bytes.starts_with(ELF_MAGIC)
}

/// A validated static Linux executable, ready to map into a new process.
pub(super) struct LinuxExecutable {
    image: Bytes,
    abi: Abi,
}

impl LinuxExecutable {
    /// Validates `image` for this platform. Refused when the platform has
    /// no native execution or its processor lacks a protection the
    /// isolation contract requires, when no Linux ABI is decoded for its
    /// instruction set, or when the image is not one the loader maps.
    pub(super) fn load<CpuImpl: Cpu>(
        cpu: &CpuImpl,
        image: Bytes,
    ) -> Result<Self, ProgramExecError> {
        let native = cpu.native().map_err(native_unavailable)?;
        let abi = Abi::for_instruction_set(native.instruction_set()).ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::Unavailable,
            detail: ProgramExecErrorDetail::LinuxAbiUnsupported,
        })?;
        Image::parse(&image, abi, user_end(native)).map_err(image_error)?;
        Ok(Self { image, abi })
    }
}

fn native_unavailable(reason: NativeUnavailable) -> ProgramExecError {
    ProgramExecError {
        kind: ProgramExecErrorKind::Unavailable,
        detail: ProgramExecErrorDetail::NativeExecutionUnavailable(reason),
    }
}

fn user_end<Native: NativeExecution>(native: &Native) -> u64 {
    native.user_range().end().raw() as u64
}

/// What a Linux process's [`ProcessAuthority`] lets its system calls
/// reach, derived once from the authority its parent granted. Each
/// resource a later system call reaches (files, sockets, clocks,
/// processes, devices) derives its capability here the way the matching
/// WASIX import does, and a missing one is refused with Linux's errno.
/// User and group IDs are values a process reports and take no part here.
struct LinuxGrants {
    terminal_input: Option<TerminalInputCap>,
    terminal_output: Option<TerminalOutputCap>,
    /// What WASIX's `clock_time_set` derives, for `clock_settime`.
    set_wall_clock: Option<SetWallClockCap>,
}

impl LinuxGrants {
    fn derive(authority: &ProcessAuthority) -> Self {
        Self {
            terminal_input: authority.derive_terminal_input_cap().ok(),
            terminal_output: authority.derive_terminal_output_cap().ok(),
            set_wall_clock: authority.derive_set_wall_clock_cap().ok(),
        }
    }
}

fn image_error(error: elf::Error) -> ProgramExecError {
    let refusal = match error {
        elf::Error::Parse(_) => crate::LinuxImageRefusal::Malformed,
        elf::Error::UnsupportedImage => crate::LinuxImageRefusal::Unsupported,
        elf::Error::UnsupportedSegment => crate::LinuxImageRefusal::Dynamic,
        elf::Error::InvalidSegment | elf::Error::Overlap => crate::LinuxImageRefusal::Segment,
        elf::Error::InvalidEntry => crate::LinuxImageRefusal::Entry,
    };
    ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::LinuxImageInvalid(refusal),
    }
}

fn native_error(error: NativeError) -> ProgramExecError {
    match error {
        NativeError::OutOfMemory => ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: ProgramExecErrorDetail::NativeMemoryExhausted,
        },
        // The image was validated and the stack built inside the user
        // range, so any other refusal is a kernel defect.
        error => panic!("mapping a validated Linux image was refused: {error}"),
    }
}

/// The objects a Linux process's descriptors share with WASIX programs.
pub(super) struct LinuxResources<Net>(core::marker::PhantomData<fn() -> Net>);

impl<Net: ComponentHostNetwork> Resources for LinuxResources<Net> {
    type File = FsDescriptor;
    type Network = crate::UnixNetwork<Net>;
    type Data = EpollUserData;
}

/// A Linux interest's 64-bit `epoll_data` rides in the WASIX record's
/// 64-bit field; the rest of the record is a WASIX program's own.
impl EpollData for EpollUserData {
    fn from_linux(data: u64) -> Self {
        Self {
            ptr: 0,
            fd: 0,
            data1: 0,
            data2: data,
        }
    }
    fn to_linux(&self) -> u64 {
        self.data2
    }
}

/// A process's address space as the system-call decoder sees it.
struct LinuxMemory<'a, Space> {
    space: &'a Space,
    /// The platform's user range, whose start is the lowest mappable
    /// address.
    user: VirtRange,
}

impl<Space: UserSpace> UserMemory for LinuxMemory<'_, Space> {
    fn read(&self, address: u64, output: &mut [u8]) -> Result<(), MemoryFault> {
        self.space
            .read(VirtAddr::new(address as usize), output)
            .map_err(|_| MemoryFault)
    }

    fn check_writable(&self, address: u64, length: usize) -> Result<(), MemoryFault> {
        self.space
            .check_writable(VirtAddr::new(address as usize), length)
            .map_err(|_| MemoryFault)
    }

    fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), MemoryFault> {
        self.space
            .write(VirtAddr::new(address as usize), bytes)
            .map_err(|_| MemoryFault)
    }

    fn atomic_load_u32(&self, address: u64) -> Result<u32, MemoryFault> {
        self.space
            .atomic_load_u32(VirtAddr::new(address as usize))
            .map_err(|_| MemoryFault)
    }

    fn atomic_store_u32(&mut self, address: u64, value: u32) -> Result<(), MemoryFault> {
        self.space
            .atomic_store_u32(VirtAddr::new(address as usize), value)
            .map_err(|_| MemoryFault)
    }

    fn atomic_compare_exchange_u32(
        &mut self,
        address: u64,
        expected: u32,
        desired: u32,
    ) -> Result<Result<u32, u32>, MemoryFault> {
        self.space
            .atomic_compare_exchange_u32(VirtAddr::new(address as usize), expected, desired)
            .map_err(|_| MemoryFault)
    }
}

fn page_range(address: u64, length: u64) -> VirtRange {
    VirtRange::new(VirtAddr::new(address as usize), length as usize)
}

/// A refusal of a memory-map call the decoder already validated. Only the
/// address space's own limits reach the process; anything else means the
/// decoder and the space disagree on a range, a kernel defect.
fn map_error(error: NativeError) -> MapError {
    match error {
        NativeError::OutOfMemory => MapError::OutOfMemory,
        NativeError::Address => MapError::Address,
        NativeError::Overlap => MapError::Occupied,
        error @ (NativeError::NotMapped | NativeError::Register) => {
            panic!("a validated Linux memory-map call was refused: {error}")
        }
    }
}

impl<Space: UserSpace> VirtualMemory for LinuxMemory<'_, Space> {
    fn lowest_address(&self) -> u64 {
        self.user.start.raw() as u64
    }

    fn map_anywhere(&mut self, hint: u64, length: u64, access: PageFlags) -> Result<u64, MapError> {
        self.space
            .map_anywhere(VirtAddr::new(hint as usize), length as usize, access)
            .map(|range| range.start.raw() as u64)
            .map_err(map_error)
    }

    fn map_at(&mut self, address: u64, length: u64, access: PageFlags) -> Result<(), MapError> {
        self.space
            .map(page_range(address, length), &[], access)
            .map_err(map_error)
    }

    fn unmap(&mut self, address: u64, length: u64) {
        self.space
            .unmap(page_range(address, length))
            .unwrap_or_else(|error| panic!("unmapping a validated Linux range: {error}"));
    }

    fn protect(&mut self, address: u64, length: u64, access: PageFlags) -> Result<(), MapError> {
        self.space
            .protect(page_range(address, length), access)
            .map_err(map_error)
    }

    fn discard(&mut self, address: u64, length: u64) {
        self.space
            .discard(page_range(address, length))
            .unwrap_or_else(|error| panic!("discarding a validated Linux range: {error}"));
    }

    fn remap(&mut self, from: u64, length: u64, to: u64, new_length: u64) -> Result<(), MapError> {
        self.space
            .remap(page_range(from, length), page_range(to, new_length))
            .map_err(map_error)
    }

    fn region(&self, start: u64, end: u64) -> Option<Region> {
        if start >= end {
            return None;
        }
        self.space
            .mapping(page_range(start, end - start))
            .map(|mapping| Region {
                start: mapping.range.start.raw() as u64,
                end: mapping.range.end().raw() as u64,
                access: mapping.flags,
            })
    }

    fn free_range(&self, hint: u64, length: u64) -> Option<u64> {
        self.space
            .free_range(VirtAddr::new(hint as usize), length as usize)
            .map(|start| start.raw() as u64)
    }
}

/// Process-wide host state shared across every thread task of one Linux
/// process.
struct LinuxShared<CpuImpl, Net, StorageImpl, Space>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    cpu: CpuImpl,
    timer: crate::Timer<CpuImpl>,
    spawner: crate::InstanceSpawner<CpuImpl>,
    runtime_state: HostRuntimeState<CpuImpl, Net, StorageImpl>,
    read_serial: crate::SerialReader,
    write_serial: crate::DebugSerialWriter,
    output_mode: OutputMode,
    launched_instance: crate::RegisteredInstance,
    signal_state: WasixSignalState,
    grants: LinuxGrants,
    /// The process's view of wall time, which `clock_settime` moves as
    /// WASIX's `clock_time_set` moves a program's. Never held across `.await`.
    clock: spin::Mutex<crate::KernelClock<CpuImpl, HostRuntimeState<CpuImpl, Net, StorageImpl>>>,
    instances: crate::InstanceRegistry,
    abi: Abi,
    registers: abi::SystemCallRegisters,
    user: VirtRange,
    stack_top: u64,
    stack_base: usize,
    image_end: usize,
    space: Arc<Space>,
    /// Summed user-slice ticks across every thread of the process
    /// (`CLOCK_PROCESS_CPUTIME_ID`).
    process_cpu_ticks: AtomicU64,
    /// First host-level failure (user memory exhaustion or instance kill)
    /// observed on any thread of the process.
    fatal_error: spin::Mutex<Option<ProgramExecError>>,
    /// Detached thread tasks still executing or dropping their process/space
    /// handles.
    running_tasks: AtomicUsize,
    tasks_drained: helios_sync::Notify,
}

impl<CpuImpl, Net, StorageImpl, Space> LinuxShared<CpuImpl, Net, StorageImpl, Space>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    /// Where the initial stack may grow down into: from its initial pages
    /// down as far as the process's current `RLIMIT_STACK` allows, stopping
    /// above the highest ELF segment.
    fn stack_growth(&self, limit: u64) -> VirtRange {
        let lowest = (self.stack_top.saturating_sub(limit) as usize).max(self.image_end);
        VirtRange::new(
            VirtAddr::new(lowest),
            self.stack_base.saturating_sub(lowest),
        )
    }

    fn record_fatal(&self, error: ProgramExecError) {
        let mut slot = self.fatal_error.lock();
        if slot.is_none() {
            *slot = Some(error);
        }
    }

    fn take_fatal(&self) -> Option<ProgramExecError> {
        self.fatal_error.lock().take()
    }

    async fn wait_tasks_drained(&self) {
        loop {
            let mut waiter = self.tasks_drained.waiter();
            if self.running_tasks.load(Ordering::SeqCst) == 0 {
                return;
            }
            core::future::poll_fn(|cx| self.tasks_drained.poll_notified(cx, &mut waiter)).await;
        }
    }
}

/// Decrements [`LinuxShared::running_tasks`] after a spawned thread task has
/// finished [`drive_linux_thread`] and dropped its `Process`, `UserContext`
/// and `LinuxThreadPlatform`.
struct TaskDrainGuard<CpuImpl, Net, StorageImpl, Space>(
    Arc<LinuxShared<CpuImpl, Net, StorageImpl, Space>>,
)
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage;

impl<CpuImpl, Net, StorageImpl, Space> Drop for TaskDrainGuard<CpuImpl, Net, StorageImpl, Space>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn drop(&mut self) {
        if self.0.running_tasks.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.tasks_drained.notify_all();
        }
    }
}

/// Per-thread host state: thread-local serial buffer, per-thread DRBG stream,
/// and per-thread slice tick counter (`CLOCK_THREAD_CPUTIME_ID`).
struct LinuxThreadPlatform<CpuImpl, Net, StorageImpl, Space>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    shared: Arc<LinuxShared<CpuImpl, Net, StorageImpl, Space>>,
    serial_input: Vec<u8>,
    entropy: crate::EntropyPool,
    thread_cpu_ticks: u64,
}

impl<CpuImpl, Net, StorageImpl, Space> LinuxThreadPlatform<CpuImpl, Net, StorageImpl, Space>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn new(shared: Arc<LinuxShared<CpuImpl, Net, StorageImpl, Space>>, tid: NonZeroU32) -> Self {
        let personalization = shared.launched_instance.id().raw() ^ (u64::from(tid.get()) << 32);
        let entropy =
            crate::EntropyPool::derive(shared.runtime_state.root_entropy(), personalization);
        Self {
            shared,
            serial_input: Vec::new(),
            entropy,
            thread_cpu_ticks: 0,
        }
    }
}

impl<CpuImpl, Net, StorageImpl, Space: UserSpace> Stdio
    for LinuxThreadPlatform<CpuImpl, Net, StorageImpl, Space>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    async fn write(&mut self, stream: Stream, bytes: &[u8]) -> Result<usize, WriteError> {
        let kind = match stream {
            Stream::Stdout => crate::ComponentOutputStreamKind::Stdout,
            Stream::Stderr => crate::ComponentOutputStreamKind::Stderr,
        };
        let writer = match self
            .shared
            .output_mode
            .sink(kind, self.shared.launched_instance.id())
        {
            // The terminal is the one destination the stream reaches on
            // the process's own authority; a channel or a log belongs to
            // whoever routed the stream there.
            crate::ComponentOutputSink::Local(crate::LocalOutputSink::Serial)
                if self.shared.grants.terminal_output.is_none() =>
            {
                return Err(WriteError::Denied);
            }
            crate::ComponentOutputSink::Local(local) => loop {
                // Zero means another processor owns the debug UART now:
                // offer the same bytes again after a yield.
                let taken = local.write(
                    &self.shared.cpu,
                    &self.shared.runtime_state,
                    self.shared.write_serial,
                    bytes,
                );
                if taken != 0 || bytes.is_empty() {
                    return Ok(taken);
                }
                crate::yield_now().await;
            },
            crate::ComponentOutputSink::Child(writer) => writer.clone(),
        };
        match writer.write(Bytes::copy_from_slice(bytes)).await {
            Ok(()) => Ok(bytes.len()),
            Err(crate::ClosedPeer) => Err(WriteError::Io),
        }
    }

    async fn read_input(&mut self, limit: usize) -> Result<Bytes, Denied> {
        Ok(match self.shared.output_mode.stdin_route() {
            crate::ComponentInputRoute::Serial if self.shared.grants.terminal_input.is_none() => {
                return Err(Denied);
            }
            crate::ComponentInputRoute::Serial => loop {
                let wait = self.shared.write_serial.wait_for_debug_serial_input();
                (self.shared.read_serial)(
                    &mut self.serial_input,
                    u32::try_from(limit).unwrap_or(u32::MAX),
                );
                if !self.serial_input.is_empty() {
                    break Bytes::copy_from_slice(&self.serial_input);
                }
                wait.await;
            },
            crate::ComponentInputRoute::Closed => Bytes::new(),
            crate::ComponentInputRoute::Child(stdin_rx) => {
                stdin_rx.read_limit(limit).await.unwrap_or_default()
            }
        })
    }
}

impl<CpuImpl, Net, StorageImpl, Space: UserSpace> syscall::Clock
    for LinuxThreadPlatform<CpuImpl, Net, StorageImpl, Space>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    /// The uptime clock, the one WASIX's `clock_time_get` reads.
    fn now_nanos(&self) -> u64 {
        self.shared
            .runtime_state
            .uptime_nanos(self.shared.cpu.now().ticks())
    }

    async fn sleep_until(&self, deadline: u64) {
        let remaining = deadline.saturating_sub(self.now_nanos());
        self.shared
            .timer
            .sleep_for(Duration::from_nanos(remaining))
            .await;
    }
}

impl<CpuImpl, Net, StorageImpl, Space: UserSpace> System
    for LinuxThreadPlatform<CpuImpl, Net, StorageImpl, Space>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn realtime_nanos(&self) -> u64 {
        self.shared.clock.lock().system_time_nanos()
    }

    fn set_realtime_nanos(&mut self, nanos: u64) -> Result<(), Denied> {
        // The right WASIX's `clock_time_set` derives.
        let cap = self.shared.grants.set_wall_clock.as_ref().ok_or(Denied)?;
        self.shared.clock.lock().set_system_time_nanos(cap, nanos);
        Ok(())
    }

    fn cpu_nanos(&self) -> u64 {
        self.shared
            .cpu
            .timebase()
            .ticks_to_nanos(self.thread_cpu_ticks)
    }

    fn process_cpu_nanos(&self) -> u64 {
        self.shared
            .cpu
            .timebase()
            .ticks_to_nanos(self.shared.process_cpu_ticks.load(Ordering::Relaxed))
    }

    fn processor_count(&self) -> u32 {
        u32::try_from(self.shared.cpu.processor_count().max(1)).unwrap_or(u32::MAX)
    }

    fn fill_random(&mut self, output: &mut [u8]) {
        self.entropy
            .fill_secure(self.shared.runtime_state.root_entropy(), output);
    }

    fn memory(&self) -> MemoryTotals {
        let machine = crate::machine_memory();
        MemoryTotals {
            total_bytes: machine.usable_bytes as u64,
            free_bytes: machine.free_bytes as u64,
        }
    }

    fn process_count(&self) -> u32 {
        u32::try_from(self.shared.instances.count()).unwrap_or(u32::MAX)
    }

    fn kernel_version(&self) -> &str {
        concat!("#1 Helios ", env!("CARGO_PKG_VERSION"))
    }
}

/// Calling thread's context view for `arch_prctl` and `clone` / `clone3`.
struct ContextThread<'a, CpuImpl, Net, StorageImpl, Context: UserContext>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    context: &'a mut Context,
    shared: &'a Arc<LinuxShared<CpuImpl, Net, StorageImpl, Context::Space>>,
}

impl<CpuImpl, Net, StorageImpl, Context: UserContext> Thread<LinuxResources<Net>>
    for ContextThread<'_, CpuImpl, Net, StorageImpl, Context>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn thread_pointer(&self) -> u64 {
        self.context.thread_pointer().raw() as u64
    }

    fn set_thread_pointer(&mut self, address: u64) -> Result<(), Denied> {
        self.context
            .set_thread_pointer(VirtAddr::new(address as usize))
            .map_err(|_| Denied)
    }

    fn spawn_thread(
        &mut self,
        child: Process<LinuxResources<Net>>,
        state: SpawnedThread,
    ) -> Result<(), SpawnError> {
        let mut child_context = self
            .context
            .try_clone()
            .map_err(|_| SpawnError::OutOfMemory)?;
        if let Some(tls) = state.tls {
            child_context
                .set_thread_pointer(VirtAddr::new(tls as usize))
                .map_err(|_| SpawnError::InvalidTls)?;
        }
        if let Some(stack) = state.stack {
            child_context
                .set_register(self.shared.abi.stack_pointer_register(), stack)
                .expect("the ABI names registers of this instruction set");
        }
        child_context
            .set_register(self.shared.registers.result, 0)
            .expect("the ABI names registers of this instruction set");
        let child_platform = LinuxThreadPlatform::new(self.shared.clone(), child.tid());
        self.shared.running_tasks.fetch_add(1, Ordering::SeqCst);
        let guard = TaskDrainGuard(self.shared.clone());
        self.shared
            .spawner
            .try_spawn_detached(async move {
                drive_linux_thread(child_context, child, child_platform).await;
                drop(guard);
            })
            .map_err(|error| {
                tracing::warn!(
                    target: "helios_kernel::linux",
                    instance = ?self.shared.launched_instance.id(),
                    %error,
                    "refused a Linux thread: the executor's instance share is full"
                );
                SpawnError::Busy
            })
    }
}

/// What ended a system call a thread was suspended in.
enum CallEnd {
    Done(ControlFlow<Exit, u64>),
    /// Thread-group exit, signal, or instance kill arrived; the loop's next
    /// check acts on it.
    Stopped,
}

fn check_thread_stop<CpuImpl, Net, StorageImpl, Space>(
    process: &mut Process<LinuxResources<Net>>,
    shared: &LinuxShared<CpuImpl, Net, StorageImpl, Space>,
) -> bool
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    if let Some(reason) = shared.launched_instance.pending_kill() {
        match reason {
            crate::KillReason::Operator | crate::KillReason::RequesterGone => {
                process.terminate_group(helios_unix::ChildTermination::Killed);
            }
            crate::KillReason::OutOfMemory
            | crate::KillReason::SupervisorRestart
            | crate::KillReason::StoreFailed => {
                shared.record_fatal(map_program_runtime_error(wasmtime::Error::from(
                    crate::InstanceKilled { reason },
                )));
                process.terminate_group(helios_unix::ChildTermination::Killed);
            }
        }
        return true;
    }
    if process.group_ended() {
        return true;
    }
    if let Some(signal) = shared.signal_state.take_pending() {
        process.terminate_group(helios_unix::ChildTermination::Signaled(
            helios_unix::SignalTermination {
                signal: signal.raw(),
                core_dumped: false,
            },
        ));
        return true;
    }
    false
}

fn terminate_on_fault<Net: ComponentHostNetwork>(
    process: &mut Process<LinuxResources<Net>>,
    instance_id: crate::InstanceId,
    fault: UserFault,
) {
    tracing::debug!(
        target: "helios_kernel::linux",
        instance = ?instance_id,
        tid = process.tid().get(),
        ?fault,
        "Linux thread faulted"
    );
    process.terminate_group(helios_unix::ChildTermination::Signaled(
        helios_unix::SignalTermination {
            signal: abi::fault_signal(fault.kind),
            core_dumped: false,
        },
    ));
}

/// Drives one Linux thread (`main` or a `clone`-spawned worker) until that
/// thread exits (`exit`), condemns the group (`exit_group`, unhandled fault,
/// OOM), or observes condemnation from a sibling, signal or kill.
async fn drive_linux_thread<CpuImpl, Net, StorageImpl, Context>(
    mut context: Context,
    mut process: Process<LinuxResources<Net>>,
    mut platform: LinuxThreadPlatform<CpuImpl, Net, StorageImpl, Context::Space>,
) where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
    Context: UserContext,
{
    let shared = platform.shared.clone();
    let observer = process.observer();
    let instance_id = shared.launched_instance.id();
    let registers = shared.registers;
    loop {
        if check_thread_stop(&mut process, &shared) {
            break;
        }
        let slice_began = shared.cpu.now().ticks();
        let stop = context.resume(&shared.space);
        let elapsed = shared.cpu.now().ticks().saturating_sub(slice_began);
        platform.thread_cpu_ticks += elapsed;
        shared
            .process_cpu_ticks
            .fetch_add(elapsed, Ordering::Relaxed);
        match stop {
            Stop::SystemCall => {
                let call = {
                    let read = |register| {
                        context
                            .register(register)
                            .expect("the ABI names registers of this instruction set")
                    };
                    Call {
                        number: read(registers.number),
                        arguments: registers.arguments.map(read),
                    }
                };
                // Arm every stop source before testing them so an `exit_group`,
                // signal or kill raised during the user slice cannot be missed
                // before `dispatch` suspends.
                let mut group_wait = observer.waiter();
                let mut signal_wait = shared.signal_state.wait_state();
                let mut kill_wait = shared.launched_instance.arm_kill_wait();
                if check_thread_stop(&mut process, &shared) {
                    break;
                }
                let ended = {
                    let mut memory = LinuxMemory {
                        space: &*shared.space,
                        user: shared.user,
                    };
                    let mut thread = ContextThread {
                        context: &mut context,
                        shared: &shared,
                    };
                    let mut call = core::pin::pin!(syscall::dispatch(
                        &mut process,
                        call,
                        &mut memory,
                        &mut thread,
                        &mut platform,
                    ));
                    core::future::poll_fn(|cx| {
                        if let Poll::Ready(flow) = call.as_mut().poll(cx) {
                            return Poll::Ready(CallEnd::Done(flow));
                        }
                        let group_stopped = observer.poll_changed(cx, &mut group_wait).is_ready();
                        let signalled = shared
                            .signal_state
                            .poll_changed(cx, &mut signal_wait)
                            .is_ready();
                        let killed = shared
                            .launched_instance
                            .poll_kill_wait(cx, &mut kill_wait)
                            .is_ready();
                        if group_stopped || signalled || killed {
                            Poll::Ready(CallEnd::Stopped)
                        } else {
                            Poll::Pending
                        }
                    })
                    .await
                };
                match ended {
                    CallEnd::Done(ControlFlow::Continue(result)) => context
                        .set_register(registers.result, result)
                        .expect("the ABI names registers of this instruction set"),
                    CallEnd::Done(ControlFlow::Break(_)) => break,
                    CallEnd::Stopped => {}
                }
            }
            Stop::Preempted | Stop::Interrupted => crate::yield_now().await,
            Stop::Fault(fault) => match fault.kind {
                // A discarded page's first touch, a translation older than the
                // page's current access, or growth of the initial stack:
                // resolved atomically against concurrent sibling VM ops.
                FaultKind::Page {
                    address,
                    access,
                    present,
                } if shared.user.contains(address) => {
                    match shared.space.resolve_fault(address, access) {
                        Ok(()) => {}
                        Err(NativeError::OutOfMemory) => {
                            shared.record_fatal(native_error(NativeError::OutOfMemory));
                            process.terminate_group(helios_unix::ChildTermination::Killed);
                            break;
                        }
                        Err(NativeError::NotMapped)
                            if !present
                                && !access.contains(PageFlags::EXECUTE)
                                && shared
                                    .stack_growth(process.limit(Resource::Stack).current)
                                    .contains(address) =>
                        {
                            let page = VirtRange::new(address.page_floor(), PhysFrame::SIZE);
                            match shared
                                .space
                                .map(page, &[], PageFlags::READ | PageFlags::WRITE)
                            {
                                Ok(()) => {}
                                // A sibling thread mapped or grew this stack
                                // page concurrently; verify the installed page
                                // satisfies `access`.
                                Err(NativeError::Overlap) => {
                                    match shared.space.resolve_fault(address, access) {
                                        Ok(()) => {}
                                        Err(NativeError::OutOfMemory) => {
                                            shared.record_fatal(native_error(
                                                NativeError::OutOfMemory,
                                            ));
                                            process.terminate_group(
                                                helios_unix::ChildTermination::Killed,
                                            );
                                            break;
                                        }
                                        Err(NativeError::NotMapped) => {
                                            terminate_on_fault(&mut process, instance_id, fault);
                                            break;
                                        }
                                        Err(error) => panic!(
                                            "resolving a concurrent stack fault on {page:?}: {error}"
                                        ),
                                    }
                                }
                                Err(NativeError::OutOfMemory) => {
                                    shared.record_fatal(native_error(NativeError::OutOfMemory));
                                    process.terminate_group(helios_unix::ChildTermination::Killed);
                                    break;
                                }
                                Err(error) => {
                                    panic!("growing a Linux stack into {page:?}: {error}")
                                }
                            }
                        }
                        Err(NativeError::NotMapped) => {
                            terminate_on_fault(&mut process, instance_id, fault);
                            break;
                        }
                        Err(error) => panic!("resolving a page fault at {address:?}: {error}"),
                    }
                }
                _ => {
                    terminate_on_fault(&mut process, instance_id, fault);
                    break;
                }
            },
        }
    }
}

/// Runs a Linux process to its end: maps `executable`, builds its initial
/// stack from `argv` and `env`, drives `main` and every thread spawned via
/// `clone` / `clone3`, and returns once every thread task has exited and
/// released the process's resources.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_linux_process<CpuImpl, Net, StorageImpl>(
    exec_context: ProgramExecContext<CpuImpl, Net, StorageImpl>,
    argv: ProgramArgv,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    descriptors: Option<Preview1DescriptorTable<Net>>,
    signal_state: WasixSignalState,
    spawner: crate::InstanceSpawner<CpuImpl>,
    launched_instance: crate::RegisteredInstance,
    output_mode: OutputMode,
    executable: LinuxExecutable,
    timeline: &phases::Timeline,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let instance_id = launched_instance.id();
    timeline.set_instance(instance_id);
    timeline.record(&exec_context.cpu, LaunchPhase::TaskBegin);
    let cpu = exec_context.cpu.clone();
    let native = cpu.native().map_err(native_unavailable)?;
    let image =
        Image::parse(&executable.image, executable.abi, user_end(native)).map_err(image_error)?;
    let space = Arc::new(native.create_space().map_err(native_error)?);
    // The end of the highest segment: the stack never grows down past it,
    // whatever its limit, so a wild pointer into the unmapped space below
    // the stack stays a fault.
    let mut image_end = 0;
    for segment in image.segments() {
        let mut flags = PageFlags::READ;
        flags.set(PageFlags::WRITE, segment.writable);
        flags.set(PageFlags::EXECUTE, segment.executable);
        let range = VirtRange::new(
            VirtAddr::new(segment.address as usize),
            segment.memory_size as usize,
        );
        image_end = image_end.max(range.end().raw().next_multiple_of(PhysFrame::SIZE));
        space
            .map(range, segment.bytes, flags)
            .map_err(native_error)?;
    }

    let argv = argv.into_vec();
    let environment: Vec<String> = env
        .into_iter()
        .map(|(name, value)| {
            let mut variable = String::with_capacity(name.len() + 1 + value.len());
            variable.push_str(&name);
            variable.push('=');
            variable.push_str(&value);
            variable
        })
        .collect();
    let arguments: Vec<&[u8]> = argv.iter().map(String::as_bytes).collect();
    let variables: Vec<&[u8]> = environment.iter().map(String::as_bytes).collect();
    let mut random = [0; 16];
    exec_context.runtime_state.root_entropy().fill(&mut random);
    let user = native.user_range();
    // One unmapped guard page above the stack.
    let top = (user.end().raw() - PhysFrame::SIZE) as u64;
    let stack = helios_linux::stack::build(
        &helios_linux::stack::Startup {
            entry: image.entry(),
            page_size: PhysFrame::SIZE as u64,
            headers: image.program_headers().map_err(image_error)?,
            random,
            arguments: &arguments,
            environment: &variables,
            // The name the program was invoked under; the resolved path
            // does not travel with a launch.
            executable: arguments[0],
        },
        top,
    )
    .map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidPath,
        detail: ProgramExecErrorDetail::InvalidProgramPath,
    })?;
    space
        .map(
            VirtRange::new(VirtAddr::new(stack.base as usize), stack.bytes.len()),
            &stack.bytes,
            PageFlags::READ | PageFlags::WRITE,
        )
        .map_err(native_error)?;
    let context = native
        .create_context(
            VirtAddr::new(image.entry() as usize),
            VirtAddr::new(stack.pointer as usize),
        )
        .map_err(native_error)?;

    let pid = u32::try_from(instance_id.raw())
        .ok()
        .and_then(NonZeroU32::new)
        .expect("an instance id is a nonzero process id below 2^32");
    let table = match descriptors {
        Some(table) => table.into_slots(),
        None => syscall::standard_streams::<LinuxResources<Net>>(),
    };
    // The launch hands a process no user or group IDs, so it reports zero
    // ones; the machine's memory sizes the limits Linux derives from it.
    let process = Process::<LinuxResources<Net>>::with_descriptors(
        executable.abi,
        pid,
        image.program_break(PhysFrame::SIZE as u64),
        table,
        Start {
            memory_bytes: crate::machine_memory().usable_bytes as u64,
            ..Start::default()
        },
    );
    let observer = process.observer();
    let shared = Arc::new(LinuxShared {
        cpu: cpu.clone(),
        timer: exec_context.timer.clone(),
        spawner,
        runtime_state: exec_context.runtime_state.clone(),
        read_serial: exec_context.read_serial,
        write_serial: exec_context.write_serial,
        output_mode,
        launched_instance,
        signal_state,
        grants: LinuxGrants::derive(&authority),
        clock: spin::Mutex::new(crate::KernelClock::new(
            cpu.clone(),
            exec_context.runtime_state.clone(),
        )),
        instances: exec_context.instance_registry.clone(),
        abi: executable.abi,
        registers: executable.abi.system_call_registers(),
        user,
        stack_top: top,
        stack_base: stack.base as usize,
        image_end,
        space,
        process_cpu_ticks: AtomicU64::new(0),
        fatal_error: spin::Mutex::new(None),
        running_tasks: AtomicUsize::new(0),
        tasks_drained: helios_sync::Notify::new(),
    });
    let main_platform = LinuxThreadPlatform::new(shared.clone(), pid);

    timeline.record(&cpu, LaunchPhase::GuestBegin);
    // `drive_linux_thread` consumes `main`'s `Process` and `Context`, so if
    // `main` calls `SYS_exit` (`pthread_exit`) while worker threads continue,
    // `main`'s thread handle withdraws immediately without holding the group
    // open.
    drive_linux_thread(context, process, main_platform).await;

    // Wait for every live sibling thread to exit, still honouring external
    // signals and instance kills while `main` is already gone.
    loop {
        let mut group_wait = observer.waiter();
        let mut signal_wait = shared.signal_state.wait_state();
        let mut kill_wait = shared.launched_instance.arm_kill_wait();
        if let Some(reason) = shared.launched_instance.pending_kill() {
            match reason {
                crate::KillReason::Operator | crate::KillReason::RequesterGone => {
                    observer.terminate_group(helios_unix::ChildTermination::Killed);
                }
                crate::KillReason::OutOfMemory
                | crate::KillReason::SupervisorRestart
                | crate::KillReason::StoreFailed => {
                    shared.record_fatal(map_program_runtime_error(wasmtime::Error::from(
                        crate::InstanceKilled { reason },
                    )));
                    observer.terminate_group(helios_unix::ChildTermination::Killed);
                }
            }
        } else if !observer.group_ended()
            && let Some(signal) = shared.signal_state.take_pending()
        {
            observer.terminate_group(helios_unix::ChildTermination::Signaled(
                helios_unix::SignalTermination {
                    signal: signal.raw(),
                    core_dumped: false,
                },
            ));
        }
        if observer.all_threads_exited() {
            break;
        }
        core::future::poll_fn(|cx| {
            let group_changed = observer.poll_changed(cx, &mut group_wait).is_ready();
            let signalled = shared
                .signal_state
                .poll_changed(cx, &mut signal_wait)
                .is_ready();
            let killed = shared
                .launched_instance
                .poll_kill_wait(cx, &mut kill_wait)
                .is_ready();
            if group_changed || signalled || killed {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    // Wait for every detached thread task to drop its `Process` (descriptor
    // table) and `Arc<Space>` before reporting `ChildExit` to the parent.
    shared.wait_tasks_drained().await;
    timeline.record(&cpu, LaunchPhase::GuestEnd);

    let fatal = shared.take_fatal();
    let termination = observer.termination();
    drop(observer);
    drop(shared);

    match fatal {
        Some(error) => Err(error),
        None => Ok(ChildExit {
            instance_id,
            termination,
            filesystem: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{
        NativeSpace, UserMemoryPool, UserTranslation, install_user_memory_pool,
        installed_user_memory_pool,
    };
    use core::alloc::Layout;
    use core::ptr::NonNull;

    fn ensure_pool() {
        const POOL_BYTES: usize = 4 * 1024 * 1024;
        if installed_user_memory_pool().is_some() {
            return;
        }
        let layout = Layout::from_size_align(POOL_BYTES, PhysFrame::SIZE).expect("pool layout");
        // SAFETY: nonzero size, leaked for the lifetime of the test process.
        let base = unsafe { alloc::alloc::alloc_zeroed(layout) } as usize;
        assert!(base != 0, "pool backing");
        install_user_memory_pool(UserMemoryPool::single_node(1, &[(base, base + POOL_BYTES)]));
    }

    struct DummyTables;

    // SAFETY: host unit test uses `NativeSpace`'s direct-map operations only
    // and never enters hardware user mode.
    unsafe impl UserTranslation for DummyTables {
        fn install(&mut self, _: u64, _: u64, _: PageFlags) -> Result<(), NativeError> {
            Ok(())
        }
        fn uninstall(&mut self, _: u64) {}
        fn protect(&mut self, _: u64, _: PageFlags) {}
        fn invalidate(&self, _: u64, _: u64) {}
        fn physical(&self, frame: NonNull<u8>) -> u64 {
            frame.as_ptr() as u64
        }
    }

    #[test]
    fn linux_memory_atomic_u32_ops_commit_discarded_pages_and_enforce_permissions() {
        ensure_pool();
        let start = PhysFrame::SIZE;
        let end = 1 << 32;
        let space = NativeSpace::new(DummyTables, start as u64, end as u64);
        let user = VirtRange::new(VirtAddr::new(start), end - start);
        let mut memory = LinuxMemory {
            space: &space,
            user,
        };

        let rw_addr = 0x10_000u64;
        memory
            .map_at(
                rw_addr,
                PhysFrame::SIZE as u64,
                PageFlags::READ | PageFlags::WRITE,
            )
            .unwrap();
        assert_eq!(memory.atomic_load_u32(rw_addr + 2), Err(MemoryFault));
        assert_eq!(memory.atomic_load_u32(0x20_000), Err(MemoryFault));
        assert_eq!(memory.atomic_load_u32(rw_addr), Ok(0));

        // Discard leaves a zero-fill-on-demand page; atomic store commits a
        // fresh frame and writes the word.
        memory.discard(rw_addr, PhysFrame::SIZE as u64);
        assert_eq!(memory.atomic_load_u32(rw_addr), Ok(0));
        memory.atomic_store_u32(rw_addr, 0x1234_5678).unwrap();
        assert_eq!(memory.atomic_load_u32(rw_addr), Ok(0x1234_5678));
        assert_eq!(
            memory.atomic_compare_exchange_u32(rw_addr, 9, 10),
            Ok(Err(0x1234_5678))
        );
        assert_eq!(
            memory.atomic_compare_exchange_u32(rw_addr, 0x1234_5678, 0xdead_beef),
            Ok(Ok(0x1234_5678))
        );
        assert_eq!(memory.atomic_load_u32(rw_addr), Ok(0xdead_beef));

        // Narrowing to read-only keeps atomic loads working and refuses atomic
        // stores and CAS.
        memory
            .protect(rw_addr, PhysFrame::SIZE as u64, PageFlags::READ)
            .unwrap();
        assert_eq!(memory.atomic_load_u32(rw_addr), Ok(0xdead_beef));
        assert_eq!(memory.atomic_store_u32(rw_addr, 1), Err(MemoryFault));
        assert_eq!(
            memory.atomic_compare_exchange_u32(rw_addr, 0xdead_beef, 1),
            Err(MemoryFault)
        );
    }
}
