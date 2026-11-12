use super::*;
use crate::{RawMutex, RawMutexLease};
use helios_compiler_abi::{CompilerStatus, OutputKind, UnknownDiscriminant};

/// Cached state of the compiler kernel plugin. Building it costs one
/// `Module::deserialize` + one `SharedMemory::new(8192 pages)` + one
/// `Linker::instantiate_pre`; the result is reused across compile
/// calls. Per-call work is reduced to a fresh `wasmtime::Store`,
/// `instance_pre.instantiate`, then `initialize` / `alloc` / `compile`.
pub(super) struct CompilerPluginRuntime<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    pub(super) instance_pre: Arc<InstancePre<CompilerCoreStore<CpuImpl, Net, HostFs>>>,
    pub(super) shared: Arc<CompilerCoreShared<CompilerCoreStore<CpuImpl, Net, HostFs>>>,
    /// The linker-reserved main-thread TLS block, initialised once by the
    /// instance that wins the memory-init flag; every per-compile instance
    /// adopts it because later instances skip that init and see `__tls_base == 0`.
    pub(super) main_thread_tls_base: u32,
    /// Names the plugin's code to the instruction sampler while the
    /// plugin is resident.
    pub(super) _code: crate::wasmtime_adapter::code_map::CodeRegistration,
}

/// The one compile the plugin runs at a time.
///
/// The cached `SharedMemory` is the plugin's only scratch surface, so a
/// second compile racing the first would corrupt its request and response
/// buffers. The slot is an async mutex rather than a hand-built `Notify`
/// permit queue: `RawMutex` parks a waiter, arms its wait before it tests
/// the lock, wakes exactly one waiter when a lease drops, and is FIFO, which
/// is the whole of what a slot needs. A waiter costs nothing while parked.
///
/// The lease is held across every await of the compile on purpose. It is the
/// slot itself, the permission to use the plugin's memory, and guards no
/// data, so §4's "release the guard before an `.await`" (which keeps a data
/// lock from pinning other tasks across a suspension) does not describe it.
///
/// Concurrency contract: any processor may wait for the slot and any
/// processor may drop the lease (the lease is `Send`); both are lock-free
/// apart from the mutex's own state. Interrupt handlers never touch it.
pub(super) struct CompilerCompileSlot {
    lock: RawMutex,
}

/// Ownership of the compile slot; releasing it wakes one waiting compile.
pub(super) struct CompilerCompileLease<'a> {
    _lease: RawMutexLease<'a>,
}

impl CompilerCompileSlot {
    pub(super) fn new() -> Self {
        Self {
            lock: RawMutex::new(),
        }
    }

    /// Parks until the slot is free, then takes it.
    pub(super) async fn acquire(&self) -> CompilerCompileLease<'_> {
        CompilerCompileLease {
            _lease: self.lock.lock().await,
        }
    }
}

/// The compiler plugin's exports one call uses.
pub(super) struct CompilerExports {
    pub(super) alloc: wasmtime::TypedFunc<(i32, i32), i32>,
    pub(super) free: wasmtime::TypedFunc<(i32, i32, i32), ()>,
    pub(super) compile: wasmtime::TypedFunc<(i32, i32), i32>,
    pub(super) outstanding: wasmtime::TypedFunc<(), i32>,
}

/// The bytes the plugin counts as owned across the interface right now.
pub(super) async fn compiler_outstanding_bytes<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, Net, HostFs>>,
    outstanding: &wasmtime::TypedFunc<(), i32>,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    outstanding
        .call_async(&mut *store, ())
        .await
        .map(|bytes| bytes as u32)
        .map_err(map_program_runtime_error)
}

/// Holds the interface to its accounting once a compile's buffers have been
/// released: the plugin's count is back at the level it had before the
/// request was allocated, or the call is an ABI failure. The error is
/// `Internal`, so the supervisor discards a runtime whose memory no longer
/// matches the contract instead of compiling on with it.
pub(super) fn ensure_compiler_buffers_returned(
    before: u32,
    after: u32,
) -> Result<(), ProgramExecError> {
    if after == before {
        return Ok(());
    }
    tracing::error!(
        before,
        after,
        "compiler plugin buffers were not all returned after a compile"
    );
    Err(ProgramExecError {
        kind: ProgramExecErrorKind::Internal,
        detail: ProgramExecErrorDetail::CompilerBuffersOutstanding,
    })
}

/// How a compile call ended, as far as the plugin's memory is concerned.
///
/// The outer `Err` is a failed call into the plugin (a trap, a kill, an
/// allocation the plugin refused): its allocator and memory are in an unknown
/// state, the runtime is discarded or about to be, and nothing is freed
/// because the memory dies with it. Every other end, the success and the
/// kernel's own failures (a bad response, a rejected input), is the inner
/// `Result`, and means every call into the plugin returned, so the call's
/// [`CompilerAllocations`] are released. The two are told apart by which side
/// of the interface produced the error, never by a flag, and the nesting
/// (rather than a wrapper enum) keeps the error within the size budget of a
/// program `Err`.
pub(super) type CompilerCallOutcome = Result<Result<Vec<u8>, ProgramExecError>, ProgramExecError>;

/// A buffer in the plugin's memory, with the layout it was allocated with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PluginBuffer {
    ptr: u32,
    len: usize,
    align: usize,
}

/// The plugin allocations of one compile call: the three request buffers the
/// kernel allocates and the three response buffers the plugin hands over (the
/// ownership table is in `helios_compiler_abi`).
///
/// They are freed together by [`Self::release`], which is reached only when
/// every call into the plugin returned. It cannot be a `Drop`, because
/// freeing is an async call into the plugin; a call that trapped simply never
/// reaches `release`, and its allocations die with the runtime.
#[must_use = "the plugin's allocations leak unless released"]
pub(super) struct CompilerAllocations {
    /// Three request buffers and three response buffers.
    buffers: SmallVec<[PluginBuffer; 6]>,
}

impl CompilerAllocations {
    pub(super) fn new() -> Self {
        Self {
            buffers: SmallVec::new(),
        }
    }

    /// Allocates a request buffer through the plugin and records it. A
    /// zero-length buffer is pointer 0 and is not allocated.
    pub(super) async fn alloc<CpuImpl, Net, HostFs>(
        &mut self,
        store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, Net, HostFs>>,
        alloc: &wasmtime::TypedFunc<(i32, i32), i32>,
        len: usize,
        align: usize,
    ) -> Result<u32, ProgramExecError>
    where
        CpuImpl: Cpu + Clone,
        Net: ComponentHostNetwork,
        HostFs: crate::HostFileSystem,
    {
        if len == 0 {
            return Ok(0);
        }
        let ptr = compiler_alloc(store, alloc, len, align).await?;
        self.buffers.push(PluginBuffer { ptr, len, align });
        Ok(ptr)
    }

    /// Takes ownership of a buffer the plugin handed over in its response. A
    /// zero-length buffer is pointer 0 and was never allocated.
    pub(super) fn adopt(&mut self, ptr: u32, len: usize, align: usize) {
        if len == 0 {
            return;
        }
        self.buffers.push(PluginBuffer { ptr, len, align });
    }

    /// Returns every recorded buffer to the plugin.
    pub(super) async fn release<CpuImpl, Net, HostFs>(
        self,
        store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, Net, HostFs>>,
        free: &wasmtime::TypedFunc<(i32, i32, i32), ()>,
    ) -> Result<(), ProgramExecError>
    where
        CpuImpl: Cpu + Clone,
        Net: ComponentHostNetwork,
        HostFs: crate::HostFileSystem,
    {
        for buffer in self.buffers {
            free.call_async(
                &mut *store,
                (buffer.ptr as i32, buffer.len as i32, buffer.align as i32),
            )
            .await
            .map_err(map_program_runtime_error)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct CompilerCoreStore<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    pub(super) cpu: CpuImpl,
    pub(super) spawner: crate::Spawner<CpuImpl>,
    pub(super) runtime_state: HostRuntimeState<CpuImpl, Net, HostFs>,
    pub(super) instance: Arc<crate::RegisteredInstance>,
    pub(super) shared: Arc<CompilerCoreShared<CompilerCoreStore<CpuImpl, Net, HostFs>>>,
    pub(super) preview1_descriptors: CompilerPreview1Descriptors,
    pub(super) write_serial: crate::DebugSerialWriter,
    pub(super) _marker: core::marker::PhantomData<fn() -> HostFs>,
}

#[derive(Clone)]
pub(super) struct CompilerPreview1Descriptors {
    pub(super) stdout_open: bool,
    pub(super) stderr_open: bool,
}

impl CompilerPreview1Descriptors {
    pub(super) const fn new() -> Self {
        Self {
            stdout_open: true,
            stderr_open: true,
        }
    }

    pub(super) fn can_write(&self, fd: i32) -> bool {
        match fd {
            1 => self.stdout_open,
            2 => self.stderr_open,
            _ => false,
        }
    }

    pub(super) fn close(&mut self, fd: i32) -> i32 {
        match fd {
            1 if self.stdout_open => {
                self.stdout_open = false;
                p1::errno::SUCCESS
            }
            2 if self.stderr_open => {
                self.stderr_open = false;
                p1::errno::SUCCESS
            }
            _ => p1::errno::BADF,
        }
    }
}

pub(super) struct CompilerCoreShared<T> {
    pub(super) memory: SharedMemory,
    pub(super) entropy: Mutex<crate::EntropyPool>,
    pub(super) instance_pre: spin::Once<Arc<InstancePre<T>>>,
    pub(super) next_thread_id: AtomicI32,
    pub(super) thread_tasks: Mutex<Vec<crate::JoinHandle<CompilerThreadReport>>>,
    /// The interrupt routes of the engine every compiler store runs on,
    /// so a kill reaches each of them, the threads' included.
    pub(super) interrupts: Arc<crate::wasmtime_adapter::preemption::GuestInterrupts>,
}

#[derive(Clone, Copy)]
pub(super) struct CompilerThreadReport {
    pub(super) thread_id: i32,
    pub(super) processor: u16,
    pub(super) spawned_ns: u64,
    pub(super) started_ns: u64,
    pub(super) instantiated_ns: u64,
    pub(super) finished_ns: u64,
}

impl<CpuImpl, Net, HostFs> CompilerCoreStore<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    pub(super) fn memory(&self) -> &SharedMemory {
        &self.shared.memory
    }

    pub(super) fn record_user_ticks(&self, ticks: u64) {
        if self.runtime_state.profiling_enabled() {
            self.runtime_state.record_profile_stack_parts(
                ProfileScope::User,
                "user;",
                self.instance.name(),
                ticks,
            );
        }
    }
}

pub(super) fn compiler_shared_memory(
    engine: &wasmtime::Engine,
    module: &Module,
) -> Result<SharedMemory, ProgramExecError> {
    imported_shared_memory_with_declared_maximum(engine, module)?.ok_or(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::CompilerMemoryContractInvalid,
    })
}

pub(super) fn define_compiler_shared_memory<T>(
    linker: &mut CoreLinker<T>,
    store: &wasmtime::Store<T>,
    module: &Module,
    memory: SharedMemory,
) -> Result<(), ProgramExecError> {
    define_imported_shared_memory(linker, store, module, memory)
}

pub(super) fn add_compiler_core_imports<CpuImpl, Net, HostFs>(
    linker: &mut CoreLinker<CompilerCoreStore<CpuImpl, Net, HostFs>>,
    memory: SharedMemory,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    add_wasi_p1_imports(linker)?;
    add_wasi_thread_spawn(linker)?;
    let _ = memory;
    Ok(())
}

pub(super) fn compiler_plugin_worker_threads<CpuImpl: Cpu>(cpu: &CpuImpl) -> u32 {
    let kernel_processors = super::component_host_kernel_processor_count(
        cpu.processor_count(),
        cpu.bootstrap_processor(),
    );
    let worker_count = kernel_processors.max(1);
    u32::try_from(worker_count)
        .unwrap_or_else(|_| panic!("compiler plugin processor count exceeds u32"))
}

pub(super) fn compiler_rayon_env_len(thread_count: u32) -> u32 {
    RAYON_NUM_THREADS_ENV.len() as u32 + decimal_len(thread_count) + 1
}

pub(super) fn compiler_environ_get<CpuImpl, Net, HostFs>(
    caller: Caller<'_, CompilerCoreStore<CpuImpl, Net, HostFs>>,
    environ: u32,
    buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let thread_count = compiler_plugin_worker_threads(&caller.data().cpu);
    let memory = caller.data().memory();
    let environ = GuestAddress::wasm32(environ);
    let pointer_status = write_u32(memory, environ, buf);
    let bytes_status = write_rayon_threads_env(memory, GuestAddress::wasm32(buf), thread_count);
    pointer_status.max(bytes_status)
}

pub(super) fn write_rayon_threads_env(
    memory: &SharedMemory,
    ptr: GuestAddress,
    thread_count: u32,
) -> i32 {
    let prefix_status = write_shared_memory(memory, ptr, RAYON_NUM_THREADS_ENV).map_or(28, |_| 0);
    if prefix_status != 0 {
        return prefix_status;
    }
    let digits_start = ptr.offset(RAYON_NUM_THREADS_ENV.len() as u64);
    let digits_len = write_decimal(memory, digits_start, thread_count);
    if digits_len < 0 {
        return 28;
    }
    let nul_ptr = digits_start.offset(digits_len as u64);
    write_shared_memory(memory, nul_ptr, &[0]).map_or(28, |_| 0)
}

pub(super) fn decimal_len(mut value: u32) -> u32 {
    let mut len = 1;
    while value >= 10 {
        value /= 10;
        len += 1;
    }
    len
}

pub(super) fn write_decimal(memory: &SharedMemory, ptr: GuestAddress, mut value: u32) -> i32 {
    let mut digits = [0_u8; 10];
    let len = decimal_len(value) as usize;
    for index in (0..len).rev() {
        digits[index] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    write_shared_memory(memory, ptr, &digits[..len]).map_or(-1, |_| len as i32)
}

/// Installs the compiler store's interrupt callback and registers the
/// store for kills, for as long as the returned registration lives.
///
/// The compiler is never preempted by a slice: its calls are not driven
/// through
/// [`crate::wasmtime_adapter::preemption::StoreInterrupts::drive`], so no
/// processor's running slot ever names it. Two things interrupt it: a kill
/// of its instance, which the callback traps, and a freeze request
/// (`GuestInterrupts::interrupt_running` reaches undriven stores through
/// the kill table), on which the callback yields. Every compile call is a
/// `call_async` made from a task, so the yield returns the task to the
/// executor and its processor reaches the freeze barrier's boundary; the
/// compile resumes when the machine thaws. An interrupt that is neither
/// resumes the compile.
pub(super) fn configure_compiler_core_store<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, Net, HostFs>>,
) -> crate::wasmtime_adapter::preemption::StoreInterrupts<CpuImpl>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    store.interrupt_callback(|caller| {
        if let Some(reason) = caller.data().instance.pending_kill() {
            return Err(wasmtime::Error::from(crate::InstanceKilled { reason }));
        }
        if caller.data().shared.interrupts.freeze_requested() {
            Ok(wasmtime::InterruptAction::Yield)
        } else {
            Ok(wasmtime::InterruptAction::Continue)
        }
    });
    let interrupts = store.data().shared.interrupts.clone();
    interrupts.register_undriven(store.data().instance.id(), store, store.data().cpu.clone())
}

pub(super) fn compiler_tls_base<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, Net, HostFs>>,
    instance: &wasmtime::Instance,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let global = instance
        .get_global(&mut *store, "__tls_base")
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::CompilerPluginInvalid,
        })?;
    match global.get(&mut *store) {
        Val::I32(value) => Ok(value as u32),
        value => Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: {
                tracing::error!(?value, "compiler __tls_base has invalid value type");
                ProgramExecErrorDetail::CompilerPluginInvalid
            },
        }),
    }
}

pub(super) fn set_compiler_tls_base<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, Net, HostFs>>,
    instance: &wasmtime::Instance,
    tls_base: u32,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let global = instance
        .get_global(&mut *store, "__tls_base")
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::CompilerPluginInvalid,
        })?;
    global
        .set(&mut *store, Val::I32(tls_base as i32))
        .map_err(map_program_runtime_error)
}

pub(super) async fn compiler_alloc<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, Net, HostFs>>,
    alloc: &wasmtime::TypedFunc<(i32, i32), i32>,
    len: usize,
    align: usize,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let ptr = alloc
        .call_async(&mut *store, (len as i32, align as i32))
        .await
        .map_err(map_program_runtime_error)?;
    if ptr == 0 {
        let memory = store.data().memory();
        tracing::error!(
            len,
            pages = memory.size(),
            bytes = memory.data_size(),
            "compiler allocation returned null"
        );
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: ProgramExecErrorDetail::CompilerAllocationFailed,
        });
    }
    Ok(ptr as u32)
}

/// Reads the response header the plugin returned. The header is all `u32`
/// fields, so every byte pattern in plugin memory decodes to a header; what
/// its enumerated fields mean is checked by [`decode_compiler_response`].
pub(super) fn read_compiler_response(
    memory: &SharedMemory,
    ptr: GuestAddress,
) -> Result<CompilerResponseHeader, ProgramExecError> {
    let bytes = read_shared_memory(memory, ptr, CompilerResponseHeader::SIZE as u32)?;
    let bytes: &[u8; CompilerResponseHeader::SIZE] =
        bytes.as_slice().try_into().map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::Internal,
            detail: ProgramExecErrorDetail::InternalInvariant,
        })?;
    Ok(CompilerResponseHeader::from_bytes(bytes))
}

/// The typed error for a wire value the plugin sent that names no variant.
pub(super) fn compiler_abi_value_error(error: UnknownDiscriminant) -> ProgramExecError {
    tracing::error!(
        field = error.field,
        value = error.value,
        "compiler plugin sent an unknown ABI value"
    );
    ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::CompilerAbiValueInvalid(error),
    }
}

/// Decodes the enumerated fields of a response header; an unknown value is
/// the typed ABI error rather than an invalid enum.
pub(super) fn decode_compiler_response(
    header: &CompilerResponseHeader,
) -> Result<(CompilerStatus, OutputKind), ProgramExecError> {
    let status = CompilerStatus::try_from(header.status).map_err(compiler_abi_value_error)?;
    let output_kind = OutputKind::try_from(header.output_kind).map_err(compiler_abi_value_error)?;
    Ok((status, output_kind))
}

/// Everything the kernel does with a response once `compile` returned, which
/// is host-side work only: no call into the plugin is made here, so every
/// failure leaves the instance intact and the call's allocations are freed by
/// the caller. The response buffers are adopted into `allocations` as soon as
/// the header's layout is known to be this ABI's, before anything else can
/// fail, so a rejected or malformed response is freed too.
pub(super) fn accept_compiler_response<CpuImpl, Net, HostFs>(
    store: &CompilerCoreStore<CpuImpl, Net, HostFs>,
    allocations: &mut CompilerAllocations,
    response_ptr: u32,
    profile: bool,
    phases: &mut CompilerPhaseTimestamps,
) -> Result<Vec<u8>, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let memory = store.memory();
    let header = read_compiler_response(memory, GuestAddress::wasm32(response_ptr))?;
    if header.abi_version != HELIOS_COMPILER_ABI_VERSION {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::CompilerAbiMismatch,
        });
    }
    allocations.adopt(
        response_ptr,
        CompilerResponseHeader::SIZE,
        CompilerResponseHeader::ALIGN,
    );
    allocations.adopt(header.diagnostic_ptr, header.diagnostic_len as usize, 1);
    allocations.adopt(header.precompiled_ptr, header.precompiled_len as usize, 1);
    let (status, _output_kind) = decode_compiler_response(&header)?;
    let diagnostic = read_shared_memory(
        memory,
        GuestAddress::wasm32(header.diagnostic_ptr),
        header.diagnostic_len,
    )?;
    if status != CompilerStatus::Ok {
        if !diagnostic.is_empty() {
            tracing::error!(
                diagnostic = %core::str::from_utf8(&diagnostic).unwrap_or("<non-utf8 compiler diagnostic>"),
                "compiler plugin rejected input"
            );
        }
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::CompilerRejectedInput,
        });
    }
    if !diagnostic.is_empty() {
        store.write_serial.emit(&diagnostic);
    }
    phases.diagnostic_done_ns = if profile {
        monotonic_nanos(&store.cpu)
    } else {
        0
    };
    let output = read_shared_memory(
        memory,
        GuestAddress::wasm32(header.precompiled_ptr),
        header.precompiled_len,
    )?;
    phases.response_done_ns = if profile {
        monotonic_nanos(&store.cpu)
    } else {
        0
    };
    Ok(output)
}

pub(super) fn compiler_fd_fdstat_get<CpuImpl, Net, HostFs>(
    caller: Caller<'_, CompilerCoreStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    stat: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let descriptor = match fd {
        1 if caller.data().preview1_descriptors.stdout_open => Preview1Descriptor::<Net>::Stdout,
        2 if caller.data().preview1_descriptors.stderr_open => Preview1Descriptor::<Net>::Stderr,
        _ => return p1::errno::BADF,
    };
    let bytes = p1_fdstat_bytes(2, 0, p1_descriptor_rights(&descriptor));
    write_shared_memory(caller.data().memory(), GuestAddress::wasm32(stat), &bytes)
        .map_or(p1::errno::FAULT, |_| p1::errno::SUCCESS)
}

pub(super) fn fd_write<CpuImpl, Net, HostFs>(
    caller: Caller<'_, CompilerCoreStore<CpuImpl, Net, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    nwritten: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    if !caller.data().preview1_descriptors.can_write(fd) {
        return p1::errno::BADF;
    }
    let memory = caller.data().memory();
    let iovs = GuestAddress::wasm32(iovs);
    let mut written = 0u32;
    for index in 0..iovs_len {
        let iov = iovs.offset(u64::from(index) * 8);
        let Ok(ptr) = try_read_u32(memory, iov) else {
            return p1::errno::FAULT;
        };
        let Ok(len) = try_read_u32(memory, iov.offset(4)) else {
            return p1::errno::FAULT;
        };
        let Ok(bytes) = read_shared_memory(memory, GuestAddress::wasm32(ptr), len) else {
            return p1::errno::FAULT;
        };
        // The compiler plugin is bootfs-provisioned and trusted (§3.3),
        // and this host function is synchronous, so its diagnostics go
        // to the console the way a kernel record does: handed to the
        // port's owner rather than waiting for the port.
        caller.data().write_serial.emit(&bytes);
        written = written.saturating_add(len);
    }
    write_u32(memory, GuestAddress::wasm32(nwritten), written)
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc as SharedArc;
    use alloc::task::Wake;
    use core::pin::pin;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context, Waker};

    use futures_lite::future::block_on;

    use super::*;

    /// A waker that counts the wakes it receives.
    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl WakeCount {
        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl Wake for WakeCount {
        fn wake(self: SharedArc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn response_with(status: u32, output_kind: u32) -> CompilerResponseHeader {
        CompilerResponseHeader {
            abi_version: HELIOS_COMPILER_ABI_VERSION,
            status,
            output_kind,
            precompiled_ptr: 0,
            precompiled_len: 0,
            diagnostic_ptr: 0,
            diagnostic_len: 0,
        }
    }

    #[test]
    fn a_known_status_and_output_kind_decode() {
        assert_eq!(
            decode_compiler_response(&response_with(
                CompilerStatus::CompileFailed.into(),
                OutputKind::Component.into()
            )),
            Ok((CompilerStatus::CompileFailed, OutputKind::Component))
        );
    }

    #[test]
    fn an_unknown_status_is_the_typed_abi_error() {
        assert_eq!(
            decode_compiler_response(&response_with(7, 0)),
            Err(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::CompilerAbiValueInvalid(UnknownDiscriminant {
                    field: "status",
                    value: 7
                }),
            })
        );
    }

    #[test]
    fn an_unknown_output_kind_is_the_typed_abi_error() {
        assert_eq!(
            decode_compiler_response(&response_with(0, u32::MAX)),
            Err(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::CompilerAbiValueInvalid(UnknownDiscriminant {
                    field: "output_kind",
                    value: u32::MAX
                }),
            })
        );
    }

    #[test]
    fn a_count_off_its_starting_level_is_the_typed_abi_error() {
        assert_eq!(ensure_compiler_buffers_returned(4096, 4096), Ok(()));
        assert_eq!(
            ensure_compiler_buffers_returned(0, 28),
            Err(ProgramExecError {
                kind: ProgramExecErrorKind::Internal,
                detail: ProgramExecErrorDetail::CompilerBuffersOutstanding,
            })
        );
    }

    /// A zero-length buffer is pointer 0 and was never allocated, so it is
    /// never owned and never freed.
    #[test]
    fn a_zero_length_response_buffer_is_not_adopted() {
        let mut allocations = CompilerAllocations::new();
        allocations.adopt(0, 0, 1);
        allocations.adopt(64, 10, 1);
        assert_eq!(
            allocations.buffers.as_slice(),
            &[PluginBuffer {
                ptr: 64,
                len: 10,
                align: 1
            }]
        );
    }

    /// A compile that arrives while another holds the slot parks, and the
    /// release wakes exactly one waiter, the first, without either being
    /// polled in between.
    #[test]
    fn a_waiting_compile_is_woken_by_the_release() {
        let slot = CompilerCompileSlot::new();
        let first = block_on(slot.acquire());

        let second_wakes = SharedArc::new(WakeCount::default());
        let third_wakes = SharedArc::new(WakeCount::default());
        let second_waker = Waker::from(second_wakes.clone());
        let third_waker = Waker::from(third_wakes.clone());
        let mut second_context = Context::from_waker(&second_waker);
        let mut third_context = Context::from_waker(&third_waker);
        let mut second = pin!(slot.acquire());
        let mut third = pin!(slot.acquire());

        assert!(second.as_mut().poll(&mut second_context).is_pending());
        assert!(third.as_mut().poll(&mut third_context).is_pending());
        assert_eq!(second_wakes.count() + third_wakes.count(), 0);

        drop(first);

        assert_eq!(second_wakes.count(), 1, "the first waiter is woken");
        assert_eq!(third_wakes.count(), 0, "only one waiter is woken");
        assert!(second.as_mut().poll(&mut second_context).is_ready());
    }
}
