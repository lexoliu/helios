use super::*;

/// Cached state of the compiler kernel plugin. Building it costs one
/// `Module::deserialize` + one `SharedMemory::new(8192 pages)` + one
/// `Linker::instantiate_pre`; the result is reused across compile
/// calls. Per-call work is reduced to a fresh `wasmtime::Store`,
/// `instance_pre.instantiate`, then `initialize` / `alloc` / `compile`.
pub(super) struct CompilerPluginRuntime<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) instance_pre: Arc<InstancePre<CompilerCoreStore<CpuImpl, HostFs>>>,
    pub(super) shared: Arc<CompilerCoreShared<CompilerCoreStore<CpuImpl, HostFs>>>,
}

pub(super) struct CompilerCompileSlot<'a> {
    pub(super) occupied: &'a AtomicBool,
}

impl Drop for CompilerCompileSlot<'_> {
    fn drop(&mut self) {
        self.occupied.store(false, AtomicOrdering::Release);
    }
}

#[derive(Clone)]
pub(super) struct CompilerCoreStore<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) cpu: CpuImpl,
    pub(super) spawner: crate::Spawner<CpuImpl>,
    pub(super) runtime_state: HostRuntimeState<CpuImpl, HostFs>,
    pub(super) instance: Arc<crate::RegisteredInstance>,
    pub(super) shared: Arc<CompilerCoreShared<CompilerCoreStore<CpuImpl, HostFs>>>,
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
    pub(super) thread_tasks: Mutex<Vec<crate::JoinHandle<()>>>,
}

impl<CpuImpl, HostFs> CompilerCoreStore<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
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

pub(super) fn add_compiler_core_imports<CpuImpl, HostFs>(
    linker: &mut CoreLinker<CompilerCoreStore<CpuImpl, HostFs>>,
    memory: SharedMemory,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
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

pub(super) fn compiler_environ_get<CpuImpl, HostFs>(
    caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
    environ: u32,
    buf: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let thread_count = compiler_plugin_worker_threads(&caller.data().cpu);
    let memory = caller.data().memory();
    let pointer_status = write_u32(memory, environ, buf);
    let bytes_status = write_rayon_threads_env(memory, buf, thread_count);
    pointer_status.max(bytes_status)
}

pub(super) fn write_rayon_threads_env(memory: &SharedMemory, ptr: u32, thread_count: u32) -> i32 {
    let prefix_status = write_shared_memory(memory, ptr, RAYON_NUM_THREADS_ENV).map_or(28, |_| 0);
    if prefix_status != 0 {
        return prefix_status;
    }
    let digits_start = ptr + RAYON_NUM_THREADS_ENV.len() as u32;
    let digits_len = write_decimal(memory, digits_start, thread_count);
    if digits_len < 0 {
        return 28;
    }
    let nul_ptr = digits_start + digits_len as u32;
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

pub(super) fn write_decimal(memory: &SharedMemory, ptr: u32, mut value: u32) -> i32 {
    let mut digits = [0_u8; 10];
    let len = decimal_len(value) as usize;
    for index in (0..len).rev() {
        digits[index] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    write_shared_memory(memory, ptr, &digits[..len]).map_or(-1, |_| len as i32)
}

pub(super) fn configure_compiler_core_store<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, HostFs>>,
) where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    // The compiler must not be preempted by scheduler epoch ticks, but
    // epoch bumps are also how the OOM killer wakes a CPU-bound victim,
    // so extend the deadline tick by tick after checking for a pending
    // kill. A u64::MAX delta is not "never": wasmtime computes
    // `deadline = current_epoch + delta`, which wraps into the past as
    // soon as the engine's epoch has ever advanced and then traps the
    // very first epoch check with a bare interrupt.
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|caller| {
        if let Some(reason) = caller.data().instance.pending_kill() {
            return Err(wasmtime::Error::from(crate::InstanceKilled { reason }));
        }
        Ok(wasmtime::UpdateDeadline::Continue(1))
    });
}

pub(super) fn compiler_tls_base<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
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

pub(super) fn compiler_alloc<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<CompilerCoreStore<CpuImpl, HostFs>>,
    alloc: &wasmtime::TypedFunc<(i32, i32), i32>,
    len: usize,
    align: usize,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let ptr = alloc
        .call(&mut *store, (len as i32, align as i32))
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

pub(super) fn read_compiler_response(
    memory: &SharedMemory,
    ptr: u32,
) -> Result<CompilerResponseHeader, ProgramExecError> {
    let bytes = read_shared_memory(
        memory,
        ptr,
        core::mem::size_of::<CompilerResponseHeader>() as u32,
    )?;
    Ok(unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<CompilerResponseHeader>()) })
}

pub(super) fn compiler_fd_fdstat_get<CpuImpl, HostFs>(
    caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
    fd: i32,
    stat: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let descriptor = match fd {
        1 if caller.data().preview1_descriptors.stdout_open => Preview1Descriptor::Stdout,
        2 if caller.data().preview1_descriptors.stderr_open => Preview1Descriptor::Stderr,
        _ => return p1::errno::BADF,
    };
    let bytes = p1_fdstat_bytes(2, 0, p1_descriptor_rights(&descriptor));
    write_shared_memory(caller.data().memory(), stat, &bytes)
        .map_or(p1::errno::FAULT, |_| p1::errno::SUCCESS)
}

pub(super) fn fd_write<CpuImpl, HostFs>(
    caller: Caller<'_, CompilerCoreStore<CpuImpl, HostFs>>,
    fd: i32,
    iovs: u32,
    iovs_len: u32,
    nwritten: u32,
) -> i32
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    if !caller.data().preview1_descriptors.can_write(fd) {
        return p1::errno::BADF;
    }
    let memory = caller.data().memory();
    let mut written = 0u32;
    for index in 0..iovs_len {
        let iov = iovs + index * 8;
        let Ok(ptr) = try_read_u32(memory, iov) else {
            return p1::errno::FAULT;
        };
        let Ok(len) = try_read_u32(memory, iov + 4) else {
            return p1::errno::FAULT;
        };
        let Ok(bytes) = read_shared_memory(memory, ptr, len) else {
            return p1::errno::FAULT;
        };
        // The compiler plugin is bootfs-provisioned and trusted (§3.3),
        // and this host function is synchronous, so its diagnostics go
        // to the console the way a kernel record does: handed to the
        // port's owner rather than waiting for the port.
        caller.data().write_serial.emit(&bytes);
        written = written.saturating_add(len);
    }
    write_u32(memory, nwritten, written)
}
