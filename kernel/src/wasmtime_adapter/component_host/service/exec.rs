use super::*;

/// A component that is compiled, linked and ready to instantiate, shared by
/// every launch of the same executable.
pub(super) type PreparedComponent<CpuImpl, Net, HostFs> =
    Arc<ComponentInstancePre<StoreData<CpuImpl, Net, HostFs>>>;

pub(super) enum ProgramExecutable<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    Component(PreparedComponent<CpuImpl, Net, HostFs>),
    CoreModule(Arc<WasmtimeCompiledCoreModule>),
    ForkedCoreModule {
        compiled: Arc<WasmtimeCompiledCoreModule>,
        /// Boxed because a fork's restore state dwarfs the two
        /// refcounted handles the other variants carry, and the
        /// executable itself is moved through every launch path.
        restore: Box<CoreModuleRestore<Net>>,
    },
}

pub(super) struct CoreModuleRestore<Net: ComponentHostNetwork> {
    pub(super) memory: SharedMemory,
    pub(super) memory_spec: SharedMemorySpec,
    pub(super) descriptors: Preview1DescriptorTable<Net>,
    pub(super) signal_dispositions: Vec<WasixSignalDisposition>,
    pub(super) rewind: WasixRewind,
}

/// The asyncify stacks and the stack window a rewind restores.
///
/// Asyncify unwinds a guest by copying its shadow stack out and its
/// rewind records in; both buffers and the window they belong to are
/// produced together and consumed together. The window is held in
/// charged user pages shared with whatever else needs it (the stored
/// checkpoint, a fork's child), never copied again.
pub(super) struct WasixRewind {
    pub(super) stack_lower: u32,
    pub(super) stack_upper: u32,
    pub(super) stack_pointer: u32,
    pub(super) memory_stack: SharedPages,
    pub(super) rewind_stack: WasixRewindRecords,
    /// The value the rewound call returns, or `None` when the guest
    /// resumes without one.
    pub(super) value: Option<u64>,
    /// The errno the rewound call returns instead of its value, when the
    /// user pool could not hold what the call needed to keep.
    pub(super) failure: Option<i32>,
}

/// Where a rewind finds the records the guest's unwind wrote.
pub(super) enum WasixRewindRecords {
    /// A copy in charged pages, which outlives the guest's own memory
    /// being reused: a checkpoint or a fork child restores from one.
    Captured(SharedPages),
    /// The records are still where the unwind wrote them, `len` bytes from
    /// `stack_lower + WASIX_ASYNCIFY_DATA_SIZE`, because the pool could
    /// not hold a copy and the rewind is about to run at once.
    InPlace { len: u32 },
}

/// Pre-instantiated core modules shared across launches.
pub(super) type CoreModuleInstancePreCache<CpuImpl, Net, HostFs> =
    Arc<Mutex<ComponentCache<InstancePre<Preview1ProgramStore<CpuImpl, Net, HostFs>>>>>;

pub(super) struct WasixExecReplacement<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    pub(super) argv: ProgramArgv,
    pub(super) env: Vec<(String, String)>,
    pub(super) executable: ProgramExecutable<CpuImpl, Net, HostFs>,
    pub(super) authority: ProcessAuthority,
    pub(super) filesystem: Option<Namespace<HostFs>>,
    pub(super) descriptors: Option<Preview1DescriptorTable<Net>>,
    pub(super) signal_state: WasixSignalState,
    pub(super) signal_dispositions: Vec<WasixSignalDisposition>,
    /// The replacement launch's phase timeline — the calling task runs
    /// the new program itself, so the handle emits from that task.
    pub(super) timeline: phases::Timeline,
}

/// The thread table of a core-module process, whose spawned threads can
/// hand main the image an exec of theirs asked for.
pub(super) type WasixImageThreads<CpuImpl, Net, HostFs> =
    WasixProcessThreads<Box<WasixExecReplacement<CpuImpl, Net, HostFs>>>;

pub(super) struct CoreModuleReplacementState<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    pub(super) exec_context: ProgramExecContext<CpuImpl, Net, HostFs>,
    pub(super) instance: crate::RegisteredInstance,
    pub(super) output_mode: OutputMode,
    pub(super) core_linker: CoreLinker<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
}

pub(super) enum CoreModuleRunCompletion<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    Exit(Result<ChildExit, ProgramExecError>),
    /// Boxed: an exec replacement carries the whole next program,
    /// and the common completion is a plain exit code.
    Exec(Box<WasixExecReplacement<CpuImpl, Net, HostFs>>),
}

pub(super) struct WasixAsyncifyState<Net: ComponentHostNetwork> {
    pub(super) snapshots: Vec<WasixStackSnapshot>,
    pub(super) process_snapshots: Vec<WasixProcessSnapshot<Net>>,
    pub(super) phase: WasixAsyncifyPhase,
    pub(super) rewind_value: Option<u64>,
    /// The errno the call being rewound returns in place of its value, set
    /// with [`Self::rewind_value`] by a rewind that could not keep what
    /// the call needed.
    pub(super) rewind_failure: Option<i32>,
    pub(super) process_snapshot_rewinding: bool,
}

/// The asyncify state machine's phase.
///
/// Every phase but `Idle` owns the stack window its call captured in
/// charged user pages, taken before the unwind started. A call whose
/// capture is refused never leaves `Idle`.
pub(super) enum WasixAsyncifyPhase {
    Idle,
    Capturing {
        snapshot: u32,
        ret_value: u32,
        stack_lower: u32,
        stack_upper: u32,
        unwind_stack_begin: u32,
        memory_stack: ChargedPages,
        stack_pointer: u32,
    },
    Restoring {
        hash: u128,
        value: u64,
        stack_lower: u32,
    },
    Forking {
        stack_lower: u32,
        stack_upper: u32,
        unwind_stack_begin: u32,
        memory_stack: ChargedPages,
        stack_pointer: u32,
    },
    ProcessSnapshot {
        stack_lower: u32,
        stack_upper: u32,
        unwind_stack_begin: u32,
        memory_stack: ChargedPages,
        stack_pointer: u32,
    },
}

/// Why a call could not start its unwind.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum UnwindRefusal {
    /// The call answers this errno.
    Errno(i32),
    /// The user pool could not hold the window: the call answers `NOMEM`
    /// once the OOM killer has been asked for this many bytes.
    OutOfMemory { requested_bytes: u64 },
}

/// The shadow-stack window `[stack_pointer, stack_upper)` of guest
/// `memory` an unwind captures.
#[derive(Clone, Copy)]
pub(super) struct StackWindow {
    pub(super) memory: Preview1Memory,
    pub(super) stack_pointer: u32,
    pub(super) stack_upper: u32,
}

/// Copies the stack window `[stack_pointer, stack_upper)` into charged
/// pages, runs `prepare` (the writes into guest memory that set the unwind
/// up) and only then leaves `Idle` for the phase `armed` builds from the
/// copy.
///
/// The copy comes first, so a pool that cannot hold it refuses the call
/// with the phase still `Idle` and nothing written to the guest. `prepare`
/// answers an errno, and a failure of it drops the copy and leaves the
/// phase alone too.
pub(super) fn arm_unwind(
    phase: &mut WasixAsyncifyPhase,
    pool: &'static crate::UserMemoryPool,
    instance: &crate::RegisteredInstance,
    window: StackWindow,
    prepare: impl FnOnce() -> i32,
    armed: impl FnOnce(ChargedPages) -> WasixAsyncifyPhase,
) -> Result<(), UnwindRefusal> {
    debug_assert!(
        matches!(phase, WasixAsyncifyPhase::Idle),
        "an unwind starts from Idle"
    );
    let len = usize::try_from(window.stack_upper - window.stack_pointer)
        .map_err(|_| UnwindRefusal::Errno(p1::errno::OVERFLOW))?;
    let window = capture_window(
        pool,
        instance,
        window.memory,
        GuestAddress::wasm32(window.stack_pointer),
        len,
    )
    .map_err(|error| match error {
        PageCaptureError::OutOfBounds(_) => UnwindRefusal::Errno(p1::errno::FAULT),
        PageCaptureError::OutOfMemory {
            requested_bytes, ..
        } => UnwindRefusal::OutOfMemory { requested_bytes },
    })?;
    let status = prepare();
    if status != p1::errno::SUCCESS {
        return Err(UnwindRefusal::Errno(status));
    }
    *phase = armed(window);
    Ok(())
}

/// Stores a checkpoint: shares of the pages the rewind that captured it
/// holds, never copies of them.
pub(super) fn store_checkpoint(
    snapshots: &mut Vec<WasixStackSnapshot>,
    hash: u128,
    memory_stack: &SharedPages,
    rewind_stack: &SharedPages,
    stack_pointer: u32,
) {
    snapshots.push(WasixStackSnapshot {
        hash,
        memory_stack: memory_stack.clone(),
        rewind_stack: rewind_stack.clone(),
        stack_pointer,
    });
}

/// The checkpoint with `hash`, as the pages a rewind restores from:
/// shares of the stored copies, never copies of them.
pub(super) fn find_checkpoint(
    snapshots: &[WasixStackSnapshot],
    hash: u128,
) -> Option<(SharedPages, SharedPages, u32)> {
    let snapshot = snapshots.iter().find(|snapshot| snapshot.hash == hash)?;
    Some((
        snapshot.memory_stack.clone(),
        snapshot.rewind_stack.clone(),
        snapshot.stack_pointer,
    ))
}

/// A stack checkpoint a guest can restore: its window and the asyncify
/// records, in charged pages the stored entry shares with the rewind that
/// captured it and with every restore.
pub(super) struct WasixStackSnapshot {
    pub(super) hash: u128,
    pub(super) memory_stack: SharedPages,
    pub(super) rewind_stack: SharedPages,
    pub(super) stack_pointer: u32,
}

/// A process snapshot: linear memory in charged pages with its all-zero
/// pages left as holes, and the window and records its call captured,
/// shared with the rewind that resumed the guest.
pub(super) struct WasixProcessSnapshot<Net: ComponentHostNetwork> {
    pub(super) memory: ChargedPages,
    pub(super) memory_pages: u32,
    pub(super) descriptors: Preview1DescriptorTable<Net>,
    pub(super) filesystem: MemoryFs,
    pub(super) authority: ProcessAuthority,
    pub(super) cwd: Option<Preview1Cwd>,
    pub(super) arguments: Vec<String>,
    pub(super) environment: Vec<(String, String)>,
    pub(super) signal_dispositions: Vec<WasixSignalDisposition>,
    pub(super) stack_lower: u32,
    pub(super) stack_upper: u32,
    pub(super) stack_pointer: u32,
    pub(super) memory_stack: SharedPages,
    pub(super) rewind_stack: SharedPages,
}

/// The kernel-provisioned artifact at `path`, read from the boot payload
/// itself rather than from a filesystem a guest can write to.
pub(super) fn read_bootfs_artifact<CpuImpl, Net, HostFs>(
    runtime_state: &HostRuntimeState<CpuImpl, Net, HostFs>,
    path: &str,
) -> Option<BootfsBytes>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let inside = path
        .strip_prefix('/')
        .expect("a boot artifact is named by an absolute guest path");
    let path = helios_hal::fs::FsPath::new(inside)
        .expect("a boot artifact path is a normalized filesystem path");
    runtime_state
        .root_namespace()
        .table()
        .boot()
        .file_contents(path)
        .ok()
}

impl<Net: ComponentHostNetwork> WasixAsyncifyState<Net> {
    pub(super) const fn new() -> Self {
        Self {
            snapshots: Vec::new(),
            process_snapshots: Vec::new(),
            phase: WasixAsyncifyPhase::Idle,
            rewind_value: None,
            rewind_failure: None,
            process_snapshot_rewinding: false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_program_executable<CpuImpl, Net, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, Net, HostFs>,
    argv: ProgramArgv,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    filesystem: Option<Namespace<HostFs>>,
    descriptors: Option<Preview1DescriptorTable<Net>>,
    signal_state: WasixSignalState,
    signal_dispositions: Vec<WasixSignalDisposition>,
    spawner: crate::InstanceSpawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    executable: ProgramExecutable<CpuImpl, Net, HostFs>,
    engine: &crate::wasmtime_adapter::WasmtimeEngine,
    runtime: &crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    preview1_core_linker: CoreLinker<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    shared_memory_pool: Arc<Mutex<SharedMemoryPool>>,
    core_module_instance_pre_cache: CoreModuleInstancePreCache<CpuImpl, Net, HostFs>,
    launched_instance: crate::RegisteredInstance,
    output_mode: OutputMode,
    timeline: phases::Timeline,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    // The line's owner: marks the run's terminal and emits when this
    // run ends — on any exit — but only when the caller marked the
    // handle `for_task`.
    let task_trace = timeline.task_guard(exec_context.cpu.clone());
    let outcome = match executable {
        ProgramExecutable::Component(compiled) => {
            run_program_component(
                exec_context,
                argv,
                env,
                authority,
                filesystem,
                signal_state,
                spawner,
                progress,
                compiled,
                engine,
                runtime,
                launched_instance,
                output_mode,
                &timeline,
            )
            .await
        }
        ProgramExecutable::CoreModule(compiled) => {
            run_program_core_module(
                exec_context,
                argv,
                env,
                authority,
                filesystem,
                descriptors,
                signal_state,
                signal_dispositions,
                spawner,
                progress,
                compiled,
                engine,
                runtime,
                preview1_core_linker,
                shared_memory_pool,
                core_module_instance_pre_cache,
                launched_instance,
                output_mode,
                &timeline,
            )
            .await
        }
        ProgramExecutable::ForkedCoreModule { compiled, restore } => {
            run_program_core_module_with_restore(
                exec_context,
                argv,
                env,
                authority,
                filesystem,
                signal_state,
                spawner,
                progress,
                compiled,
                restore,
                engine,
                runtime,
                preview1_core_linker,
                shared_memory_pool,
                core_module_instance_pre_cache,
                launched_instance,
                output_mode,
                &timeline,
            )
            .await
        }
    };
    // The terminal the line ends with: `completed` on any child exit,
    // `failed` with the error's kind on a guest trap or host failure.
    task_trace.finish(&outcome);
    outcome
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_program_core_module<CpuImpl, Net, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, Net, HostFs>,
    argv: ProgramArgv,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    filesystem: Option<Namespace<HostFs>>,
    descriptors: Option<Preview1DescriptorTable<Net>>,
    signal_state: WasixSignalState,
    signal_dispositions: Vec<WasixSignalDisposition>,
    spawner: crate::InstanceSpawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    compiled: Arc<WasmtimeCompiledCoreModule>,
    engine: &crate::wasmtime_adapter::WasmtimeEngine,
    runtime: &crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    preview1_core_linker: CoreLinker<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    shared_memory_pool: Arc<Mutex<SharedMemoryPool>>,
    core_module_instance_pre_cache: CoreModuleInstancePreCache<CpuImpl, Net, HostFs>,
    launched_instance: crate::RegisteredInstance,
    output_mode: OutputMode,
    timeline: &phases::Timeline,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    // The name feeds only the named kernel-profile records, which are
    // gated on `profiling_enabled` — the `String` it owns exists only
    // when that diagnostic is on.
    let profile_name = if exec_context.runtime_state.profiling_enabled() {
        argv.program_name().to_owned()
    } else {
        String::new()
    };
    let argv = argv.into_vec();
    let run_cpu = exec_context.cpu.clone();
    let run_timer = exec_context.timer.clone();
    let profile_runtime_state = exec_context.runtime_state.clone();
    let write_serial = exec_context.write_serial;
    let instance_id = launched_instance.id();
    let run_started_at = monotonic_nanos(&exec_context.cpu);
    timeline.set_instance(instance_id);
    timeline.record(&run_cpu, LaunchPhase::TaskBegin);
    let wasix_abi = core_module_imports_wasix(&compiled.module);
    let replacement_state = wasix_abi.then(|| CoreModuleReplacementState {
        exec_context: exec_context.clone(),
        instance: launched_instance.clone(),
        output_mode: output_mode.clone(),
        core_linker: preview1_core_linker.clone(),
    });
    let shared_memory_prepare_profile =
        start_program_kernel_profile(&profile_runtime_state, &run_cpu);
    let imported_memory_spec = imported_shared_memory_spec_with_user_budget(&compiled.module)?;
    let imported_memory = match imported_memory_spec {
        Some(spec) => {
            Some(acquire_or_wait_for_recycle(&shared_memory_pool, engine.raw(), spec).await?)
        }
        None => None,
    };
    record_program_kernel_profile_sample(
        shared_memory_prepare_profile,
        "core-shared-memory-prepare",
    );
    timeline.record(&run_cpu, LaunchPhase::SharedMemory);
    let recycle = imported_memory_spec.zip(imported_memory.clone());
    let threads = WasixProcessThreads::new();
    let image = async {
        let store_prepare_profile = start_program_kernel_profile(&profile_runtime_state, &run_cpu);
        let mut store = wasmtime::Store::new(
            engine.raw(),
            Preview1ProgramStore::<CpuImpl, Net, HostFs>::new(
                exec_context.cpu,
                exec_context.timer,
                spawner.clone(),
                exec_context.runtime_state,
                engine.interrupts().clone(),
                launched_instance,
                exec_context.parent_instance_id,
                argv,
                env,
                authority,
                output_mode,
                exec_context.read_serial,
                write_serial,
                imported_memory.clone(),
                filesystem,
                descriptors,
                signal_state,
                threads.main_mailbox(),
                signal_dispositions,
                Some(compiled.clone()),
                wasix_abi,
                threads.clone(),
            ),
        );
        let interrupts = configure_preview1_program_store(&mut store);
        record_program_kernel_profile_sample(store_prepare_profile, "core-store-prepare");
        timeline.record(&run_cpu, LaunchPhase::StorePrepare);

        let instance = if let Some(memory) = imported_memory {
            let mut linker = preview1_core_linker;
            let imported_memory_define_profile =
                start_program_kernel_profile(&profile_runtime_state, &run_cpu);
            define_imported_shared_memory(&mut linker, &store, &compiled.module, memory)?;
            record_program_kernel_profile_sample(
                imported_memory_define_profile,
                "core-shared-memory-define",
            );

            super::emit_program_stage_marker(write_serial, "program:instantiate-core-begin");
            let instantiate_started = run_cpu.now().ticks();
            let turns = store.data().guest_turns();
            let instance = interrupts
                .drive(turns.drive(linker.instantiate_async(&mut store, &compiled.module)))
                .await;
            record_named_program_kernel_profile(
                &profile_runtime_state,
                &run_cpu,
                "instantiate-core",
                &profile_name,
                instantiate_started,
            );
            record_program_kernel_profile(
                &profile_runtime_state,
                &run_cpu,
                "instantiate-core",
                instantiate_started,
            );
            instance
        } else {
            let cache_lookup_profile =
                start_program_kernel_profile(&profile_runtime_state, &run_cpu);
            let cached_instance_pre = core_module_instance_pre_cache
                .lock()
                .get(&compiled.cache_key);
            record_program_kernel_profile_sample(
                cache_lookup_profile,
                "core-instance-pre-cache-lookup",
            );
            let instance_pre_hit = cached_instance_pre.is_some();
            let instance_pre = if let Some(instance_pre) = cached_instance_pre {
                super::emit_program_stage_marker(
                    write_serial,
                    "program:instantiate-core-pre-cache-hit",
                );
                instance_pre
            } else {
                super::emit_program_stage_marker(
                    write_serial,
                    "program:instantiate-core-pre-begin",
                );
                let instantiate_pre_profile =
                    start_program_kernel_profile(&profile_runtime_state, &run_cpu);
                let instance_pre = Arc::new(
                    preview1_core_linker
                        .instantiate_pre(&compiled.module)
                        .map_err(map_program_runtime_error)?,
                );
                record_program_kernel_profile_sample(
                    instantiate_pre_profile,
                    "instantiate-core-pre",
                );
                super::emit_program_stage_marker(write_serial, "program:instantiate-core-pre-end");
                let cache_insert_profile =
                    start_program_kernel_profile(&profile_runtime_state, &run_cpu);
                let inserted = core_module_instance_pre_cache
                    .lock()
                    .insert_if_missing(compiled.cache_key.clone(), instance_pre);
                record_program_kernel_profile_sample(
                    cache_insert_profile,
                    "core-instance-pre-cache-insert",
                );
                inserted
            };
            timeline.record_hit(&run_cpu, LaunchPhase::InstantiatePre, instance_pre_hit);

            super::emit_program_stage_marker(write_serial, "program:instantiate-core-begin");
            let instantiate_started = run_cpu.now().ticks();
            let turns = store.data().guest_turns();
            let instance = interrupts
                .drive(turns.drive(instance_pre.instantiate_async(&mut store)))
                .await;
            record_named_program_kernel_profile(
                &profile_runtime_state,
                &run_cpu,
                "instantiate-core",
                &profile_name,
                instantiate_started,
            );
            record_program_kernel_profile(
                &profile_runtime_state,
                &run_cpu,
                "instantiate-core",
                instantiate_started,
            );
            instance
        };
        let instance = instance.map_err(map_program_runtime_error)?;
        super::emit_program_stage_marker(write_serial, "program:instantiate-core-ok");
        timeline.record(&run_cpu, LaunchPhase::Instantiate);

        let resolve_start_profile = start_program_kernel_profile(&profile_runtime_state, &run_cpu);
        let start = resolve_core_module_start(&mut store, &instance)?;
        record_program_kernel_profile_sample(resolve_start_profile, "resolve-core-start");
        timeline.record(&run_cpu, LaunchPhase::Start);

        let completion = run_core_module_image(
            &mut store,
            &interrupts,
            &instance,
            start,
            CoreModuleImageRun {
                spawner: &spawner,
                progress: &progress,
                cpu: &run_cpu,
                timer: &run_timer,
                write_serial,
                runtime_state: &profile_runtime_state,
                profile_name: &profile_name,
                timeline,
                started_at: run_started_at,
                instance_id,
                run_phase: "run-core",
                complete_phase: "complete-core",
            },
        )
        .await?;
        Ok::<_, ProgramExecError>(CoreModuleImageEnd {
            completion,
            store_teardown_profile: start_program_kernel_profile(&profile_runtime_state, &run_cpu),
        })
    }
    .await;
    finish_core_module_process(
        threads,
        image,
        "core-store-teardown",
        recycle,
        CoreModuleSuccession {
            replacement_state,
            spawner,
            progress,
            engine,
            runtime,
            shared_memory_pool,
            core_module_instance_pre_cache,
        },
        timeline,
        &run_cpu,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_program_core_module_with_restore<CpuImpl, Net, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, Net, HostFs>,
    argv: ProgramArgv,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    filesystem: Option<Namespace<HostFs>>,
    signal_state: WasixSignalState,
    spawner: crate::InstanceSpawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    compiled: Arc<WasmtimeCompiledCoreModule>,
    restore: Box<CoreModuleRestore<Net>>,
    engine: &crate::wasmtime_adapter::WasmtimeEngine,
    runtime: &crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    preview1_core_linker: CoreLinker<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    shared_memory_pool: Arc<Mutex<SharedMemoryPool>>,
    core_module_instance_pre_cache: CoreModuleInstancePreCache<CpuImpl, Net, HostFs>,
    launched_instance: crate::RegisteredInstance,
    output_mode: OutputMode,
    timeline: &phases::Timeline,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    // The name feeds only the named kernel-profile records, which are
    // gated on `profiling_enabled` — the `String` it owns exists only
    // when that diagnostic is on.
    let profile_name = if exec_context.runtime_state.profiling_enabled() {
        argv.program_name().to_owned()
    } else {
        String::new()
    };
    let argv = argv.into_vec();
    let run_started_at = monotonic_nanos(&exec_context.cpu);
    let run_cpu = exec_context.cpu.clone();
    let run_timer = exec_context.timer.clone();
    let profile_runtime_state = exec_context.runtime_state.clone();
    let write_serial = exec_context.write_serial;
    let instance_id = launched_instance.id();
    timeline.set_instance(instance_id);
    timeline.record(&run_cpu, LaunchPhase::TaskBegin);
    let wasix_abi = core_module_imports_wasix(&compiled.module);
    let replacement_state = wasix_abi.then(|| CoreModuleReplacementState {
        exec_context: exec_context.clone(),
        instance: launched_instance.clone(),
        output_mode: output_mode.clone(),
        core_linker: preview1_core_linker.clone(),
    });
    let recycle = Some((restore.memory_spec, restore.memory.clone()));
    let threads = WasixProcessThreads::new();
    let image = async {
        let store_prepare_profile = start_program_kernel_profile(&profile_runtime_state, &run_cpu);
        let mut store = wasmtime::Store::new(
            engine.raw(),
            Preview1ProgramStore::<CpuImpl, Net, HostFs>::new(
                exec_context.cpu,
                exec_context.timer,
                spawner.clone(),
                exec_context.runtime_state,
                engine.interrupts().clone(),
                launched_instance,
                exec_context.parent_instance_id,
                argv,
                env,
                authority,
                output_mode,
                exec_context.read_serial,
                write_serial,
                Some(restore.memory.clone()),
                filesystem,
                Some(restore.descriptors),
                signal_state,
                threads.main_mailbox(),
                restore.signal_dispositions,
                Some(compiled.clone()),
                wasix_abi,
                threads.clone(),
            ),
        );
        let interrupts = configure_preview1_program_store(&mut store);
        record_program_kernel_profile_sample(store_prepare_profile, "core-store-prepare-rewind");
        timeline.record(&run_cpu, LaunchPhase::StorePrepare);

        let mut linker = preview1_core_linker;
        let imported_memory_define_profile =
            start_program_kernel_profile(&profile_runtime_state, &run_cpu);
        define_imported_shared_memory(&mut linker, &store, &compiled.module, restore.memory)?;
        record_program_kernel_profile_sample(
            imported_memory_define_profile,
            "core-shared-memory-define-rewind",
        );
        timeline.record(&run_cpu, LaunchPhase::SharedMemory);

        super::emit_program_stage_marker(write_serial, "program:instantiate-core-begin");
        let instantiate_started = run_cpu.now().ticks();
        let turns = store.data().guest_turns();
        let instance = interrupts
            .drive(turns.drive(linker.instantiate_async(&mut store, &compiled.module)))
            .await;
        record_named_program_kernel_profile(
            &profile_runtime_state,
            &run_cpu,
            "instantiate-core-rewind",
            &profile_name,
            instantiate_started,
        );
        record_program_kernel_profile(
            &profile_runtime_state,
            &run_cpu,
            "instantiate-core-rewind",
            instantiate_started,
        );
        let instance = instance.map_err(map_program_runtime_error)?;
        timeline.record(&run_cpu, LaunchPhase::Instantiate);
        super::emit_program_stage_marker(write_serial, "program:instantiate-core-ok");

        let rewind_profile = start_program_kernel_profile(&profile_runtime_state, &run_cpu);
        wasix_begin_rewind(&mut store, &interrupts, &instance, restore.rewind).await?;
        record_program_kernel_profile_sample(rewind_profile, "core-begin-rewind");

        let resolve_start_profile = start_program_kernel_profile(&profile_runtime_state, &run_cpu);
        let start = resolve_core_module_start(&mut store, &instance)?;
        record_program_kernel_profile_sample(resolve_start_profile, "resolve-core-start-rewind");
        timeline.record(&run_cpu, LaunchPhase::Start);

        let completion = run_core_module_image(
            &mut store,
            &interrupts,
            &instance,
            start,
            CoreModuleImageRun {
                spawner: &spawner,
                progress: &progress,
                cpu: &run_cpu,
                timer: &run_timer,
                write_serial,
                runtime_state: &profile_runtime_state,
                profile_name: &profile_name,
                timeline,
                started_at: run_started_at,
                instance_id,
                run_phase: "run-core-rewind",
                complete_phase: "complete-core-rewind",
            },
        )
        .await?;
        Ok::<_, ProgramExecError>(CoreModuleImageEnd {
            completion,
            store_teardown_profile: start_program_kernel_profile(&profile_runtime_state, &run_cpu),
        })
    }
    .await;
    finish_core_module_process(
        threads,
        image,
        "core-store-teardown-rewind",
        recycle,
        CoreModuleSuccession {
            replacement_state,
            spawner,
            progress,
            engine,
            runtime,
            shared_memory_pool,
            core_module_instance_pre_cache,
        },
        timeline,
        &run_cpu,
    )
    .await
}

/// The `_start` export of a core-module image.
fn resolve_core_module_start<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    instance: &wasmtime::Instance,
) -> Result<wasmtime::TypedFunc<(), ()>, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    instance
        .get_typed_func::<(), ()>(store, "_start")
        .map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::InvalidEntryPoint,
        })
}

/// What a core-module image's run reports through, and under which
/// kernel-profile phases: a fresh image and a rewound fork record under
/// names of their own.
struct CoreModuleImageRun<'a, CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    spawner: &'a crate::InstanceSpawner<CpuImpl>,
    progress: &'a helios_hal::watchdog::ProgressCounter,
    cpu: &'a CpuImpl,
    timer: &'a crate::Timer<CpuImpl>,
    write_serial: crate::DebugSerialWriter,
    runtime_state: &'a HostRuntimeState<CpuImpl, Net, HostFs>,
    profile_name: &'a str,
    timeline: &'a phases::Timeline,
    started_at: u64,
    instance_id: crate::InstanceId,
    run_phase: &'static str,
    complete_phase: &'static str,
}

/// Runs an instantiated core-module image's `_start` on the process's
/// main thread to its end, through every asyncify unwind and rewind, and
/// turns that end into the image's completion: an exit status, or the
/// next image an exec asked for.
async fn run_core_module_image<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    interrupts: &StoreInterrupts<CpuImpl>,
    instance: &wasmtime::Instance,
    start: wasmtime::TypedFunc<(), ()>,
    run: CoreModuleImageRun<'_, CpuImpl, Net, HostFs>,
) -> Result<CoreModuleRunCompletion<CpuImpl, Net, HostFs>, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let run_heartbeat = super::spawn_component_phase_heartbeat(
        run.spawner,
        run.cpu,
        run.timer,
        run.progress,
        run.write_serial,
        super::ComponentPhase {
            name: "program:run-core",
            started_at: run.started_at,
        },
    )
    .map_err(map_task_capacity_error)?;
    super::emit_program_stage_marker(run.write_serial, "program:run-core-begin");
    let run_phase_started = run.cpu.now().ticks();
    run.timeline.record(run.cpu, LaunchPhase::GuestBegin);
    let result = loop {
        let turns = store.data().guest_turns();
        let result = interrupts
            .drive(turns.drive(start.call_async(&mut *store, ())))
            .await;
        // A spawned thread ended the process: no unwind is completed.
        if result.as_ref().is_err_and(WasixThreadEnded::is) {
            break result;
        }
        match handle_wasix_asyncify_completion(store, interrupts, instance).await {
            Ok(true) => continue,
            Ok(false) => break result,
            // A spawned thread ended the process while main was between
            // an unwind and its rewind; the rewind's call met the end.
            Err(_) if store.data().thread_signals.thread_ended() => {
                break Err(wasmtime::Error::new(WasixThreadEnded));
            }
            Err(error) => return Err(error),
        }
    };
    run.timeline.record(run.cpu, LaunchPhase::GuestEnd);
    record_named_program_kernel_profile(
        run.runtime_state,
        run.cpu,
        run.run_phase,
        run.profile_name,
        run_phase_started,
    );
    record_program_kernel_profile(run.runtime_state, run.cpu, run.run_phase, run_phase_started);
    drop(run_heartbeat);
    super::emit_program_stage_marker(run.write_serial, "program:run-core-end");

    let completion_profile = start_program_kernel_profile(run.runtime_state, run.cpu);
    let completion = match result {
        Ok(()) => CoreModuleRunCompletion::Exit(Ok(ChildExit {
            instance_id: run.instance_id,
            exit_code: 0,
            filesystem: Some(store.data().filesystem.root_layer()),
        })),
        // Main's call stopped because a spawned thread ended the process;
        // the process ends the way that thread's call ended it: it exits
        // with its status, runs the image its exec asked for, or fails
        // with the failure a trap on main would have reported.
        Err(error) if WasixThreadEnded::is(&error) => {
            match store.data().threads.take_process_end().expect(
                "main's image is ended only by a spawned thread that recorded the process's end first",
            ) {
                WasixProcessEnd::Exit(code) => CoreModuleRunCompletion::Exit(Ok(ChildExit {
                    instance_id: run.instance_id,
                    exit_code: code,
                    filesystem: Some(store.data().filesystem.root_layer()),
                })),
                WasixProcessEnd::Exec(replacement) => CoreModuleRunCompletion::Exec(replacement),
                WasixProcessEnd::Fail(failure) => CoreModuleRunCompletion::Exit(Err(failure)),
            }
        }
        Err(error) => {
            if let Some(replacement) = store.data_mut().take_exec_replacement() {
                CoreModuleRunCompletion::Exec(Box::new(replacement))
            } else {
                let end = store.data_mut().take_process_end(error);
                CoreModuleRunCompletion::Exit(end.map(|exit_code| ChildExit {
                    instance_id: run.instance_id,
                    exit_code,
                    filesystem: Some(store.data().filesystem.root_layer()),
                }))
            }
        }
    };
    record_program_kernel_profile_sample(completion_profile, run.complete_phase);
    Ok(completion)
}

/// How a core-module image ended: its completion, and the kernel-profile
/// sample that times the teardown of its store, started before the store
/// drops.
struct CoreModuleImageEnd<CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    completion: CoreModuleRunCompletion<CpuImpl, Net, HostFs>,
    store_teardown_profile: Option<ProgramKernelProfile<CpuImpl, Net, HostFs>>,
}

/// What a core-module process hands on when its image ends: the runtime
/// an exec's next image launches on, and the pool its memory returns to.
struct CoreModuleSuccession<'a, CpuImpl, Net, HostFs>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    replacement_state: Option<CoreModuleReplacementState<CpuImpl, Net, HostFs>>,
    spawner: crate::InstanceSpawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    engine: &'a crate::wasmtime_adapter::WasmtimeEngine,
    runtime: &'a crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    shared_memory_pool: Arc<Mutex<SharedMemoryPool>>,
    core_module_instance_pre_cache: CoreModuleInstancePreCache<CpuImpl, Net, HostFs>,
}

/// Ends a core-module process once its image has: stops every thread the
/// process still has, then returns its memory to the pool and reports
/// its completion, or runs the image an exec asked for.
///
/// The threads end on every way the image can end, a failure included,
/// and before anything is reported, so no thread outlives its process:
/// none keeps a fiber stack, a store, an instance registration or a
/// clone of the process's stdio, and none still uses the memory the
/// pool takes back. The image's own completion is decided before the
/// threads are stopped, so their end never changes the process's exit
/// status.
async fn finish_core_module_process<CpuImpl, Net, HostFs>(
    threads: WasixImageThreads<CpuImpl, Net, HostFs>,
    image: Result<CoreModuleImageEnd<CpuImpl, Net, HostFs>, ProgramExecError>,
    teardown_phase: &'static str,
    recycle: Option<(SharedMemorySpec, SharedMemory)>,
    succession: CoreModuleSuccession<'_, CpuImpl, Net, HostFs>,
    timeline: &phases::Timeline,
    run_cpu: &CpuImpl,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let image = image.map(|end| {
        record_program_kernel_profile_sample(end.store_teardown_profile, teardown_phase);
        timeline.record(run_cpu, LaunchPhase::StoreTeardown);
        end.completion
    });
    threads.end().await;
    let completion = image?;

    let CoreModuleSuccession {
        replacement_state,
        spawner,
        progress,
        engine,
        runtime,
        shared_memory_pool,
        core_module_instance_pre_cache,
    } = succession;
    if let Some((spec, memory)) = recycle {
        spawn_scrubbed_recycle(&spawner, shared_memory_pool.clone(), spec, memory).await;
    }
    let outcome = match completion {
        CoreModuleRunCompletion::Exit(result) => result,
        CoreModuleRunCompletion::Exec(replacement) => {
            let replacement_state = replacement_state
                .expect("core module without WASIX imports requested exec replacement");
            Box::pin(run_program_executable(
                replacement_state.exec_context,
                replacement.argv,
                replacement.env,
                replacement.authority,
                replacement.filesystem,
                replacement.descriptors,
                replacement.signal_state,
                replacement.signal_dispositions,
                spawner,
                progress,
                replacement.executable,
                engine,
                runtime,
                replacement_state.core_linker,
                shared_memory_pool,
                core_module_instance_pre_cache,
                replacement_state.instance,
                replacement_state.output_mode,
                replacement.timeline,
            ))
            .await
        }
    };
    timeline.record(run_cpu, LaunchPhase::Completion);
    outcome
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_program_component<CpuImpl, Net, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, Net, HostFs>,
    argv: ProgramArgv,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    filesystem: Option<Namespace<HostFs>>,
    _signal_state: WasixSignalState,
    spawner: crate::InstanceSpawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    instance_pre: PreparedComponent<CpuImpl, Net, HostFs>,
    engine: &crate::wasmtime_adapter::WasmtimeEngine,
    _runtime: &crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    launched_instance: crate::RegisteredInstance,
    output_mode: OutputMode,
    timeline: &phases::Timeline,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    use crate::ComponentExecutor;

    let instance_id = launched_instance.id();

    let argv = argv.into_vec();
    let run_started_at = monotonic_nanos(&exec_context.cpu);
    let run_cpu = exec_context.cpu.clone();
    let run_timer = exec_context.timer.clone();
    let profile_cpu = exec_context.cpu.clone();
    let profile_runtime_state = exec_context.runtime_state.clone();
    timeline.set_instance(instance_id);
    timeline.record(&run_cpu, LaunchPhase::TaskBegin);

    // Use the engine that compiled the component — Wasmtime requires
    // component and store to share the same engine instance.
    super::emit_program_stage_marker(exec_context.write_serial, "program:instantiate-begin");
    let instantiate_started = profile_cpu.now().ticks();
    let store_prepare_started = profile_runtime_state
        .profiling_enabled()
        .then(|| profile_cpu.now().ticks());
    let filesystem =
        filesystem.unwrap_or_else(|| exec_context.runtime_state.root_namespace().descend());
    let (mut store, interrupts) = crate::wasmtime_adapter::store_with_state(
        engine,
        StoreData::<CpuImpl, Net, HostFs>::new(
            wasmtime::component::ResourceTable::new(),
            exec_context.cpu,
            exec_context.timer,
            spawner.clone(),
            exec_context.runtime_state.clone(),
            exec_context.instance_registry,
            launched_instance,
            None,
            filesystem,
            argv,
            env,
            authority,
            output_mode,
            exec_context.read_serial,
            exec_context.write_serial,
        ),
    );
    if let Some(store_prepare_started) = store_prepare_started {
        record_program_kernel_profile(
            &profile_runtime_state,
            &profile_cpu,
            "component-store-prepare",
            store_prepare_started,
        );
    }
    let instantiate_instance_started = profile_runtime_state
        .profiling_enabled()
        .then(|| profile_cpu.now().ticks());
    timeline.record(&run_cpu, LaunchPhase::StorePrepare);
    let turns = store.data().guest_turns();
    let instance = interrupts
        .drive(turns.drive(instance_pre.instantiate_async(&mut store)))
        .await;
    timeline.record(&run_cpu, LaunchPhase::Instantiate);
    if let Some(instantiate_instance_started) = instantiate_instance_started {
        record_program_kernel_profile(
            &profile_runtime_state,
            &profile_cpu,
            "instantiate-component-instance",
            instantiate_instance_started,
        );
    }
    let instance = instance.inspect(|instance| {
        // Every call that shows a view checks its span against this
        // instance's linear memory, so the store records the memory's
        // handle here, the first moment the runtime can name it.
        crate::wasmtime_adapter::component_host::record_linear_memory(&mut store, instance);
    });
    let executor = instance.and_then(|instance| {
        let resolve_started = profile_runtime_state
            .profiling_enabled()
            .then(|| profile_cpu.now().ticks());
        let resolved = crate::wasmtime_adapter::engine::resolve_wasi_cli_run(
            instance_pre.component(),
            &instance,
            &mut store,
        );
        if let Some(resolve_started) = resolve_started {
            record_program_kernel_profile(
                &profile_runtime_state,
                &profile_cpu,
                "resolve-component-run",
                resolve_started,
            );
        }
        resolved.map(|run_func| crate::wasmtime_adapter::WasmtimeExecutor {
            store,
            interrupts,
            run_func,
        })
    });
    record_program_kernel_profile(
        &profile_runtime_state,
        &profile_cpu,
        "instantiate-component",
        instantiate_started,
    );
    let executor = executor.map_err(map_program_runtime_error)?;
    super::emit_program_stage_marker(exec_context.write_serial, "program:instantiate-ok");
    timeline.record(&run_cpu, LaunchPhase::Start);

    let run_heartbeat = super::spawn_component_phase_heartbeat(
        &spawner,
        &run_cpu,
        &run_timer,
        &progress,
        exec_context.write_serial,
        super::ComponentPhase {
            name: "program:run",
            started_at: run_started_at,
        },
    )
    .map_err(map_task_capacity_error)?;
    super::emit_program_stage_marker(exec_context.write_serial, "program:run-begin");
    let run_phase_started = profile_cpu.now().ticks();
    timeline.record(&run_cpu, LaunchPhase::GuestBegin);
    let result = executor.run().await;
    timeline.record(&run_cpu, LaunchPhase::GuestEnd);
    record_program_kernel_profile(
        &profile_runtime_state,
        &profile_cpu,
        "run-component",
        run_phase_started,
    );
    drop(run_heartbeat);
    let result = result.map_err(map_program_runtime_error)?;
    super::emit_program_stage_marker(exec_context.write_serial, "program:run-end");
    timeline.record(&run_cpu, LaunchPhase::Completion);

    Ok(ChildExit {
        instance_id: result.instance_id,
        exit_code: result.exit_code,
        filesystem: None,
    })
}

pub(super) async fn handle_wasix_asyncify_completion<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    interrupts: &StoreInterrupts<CpuImpl>,
    instance: &wasmtime::Instance,
) -> Result<bool, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let phase = core::mem::replace(
        &mut store.data_mut().asyncify.phase,
        WasixAsyncifyPhase::Idle,
    );
    match phase {
        WasixAsyncifyPhase::Idle => Ok(false),
        WasixAsyncifyPhase::Capturing {
            snapshot,
            ret_value,
            stack_lower,
            stack_upper,
            unwind_stack_begin,
            mut memory_stack,
            stack_pointer,
        } => {
            let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
            })?;
            let unwind =
                wasix_unwind_records(memory, stack_lower, unwind_stack_begin, stack_pointer)?;
            let rewind_stack = capture_rewind_records(store.data(), memory, unwind)?;
            wasix_call_instance_func0(store, interrupts, instance, "asyncify_stop_unwind").await?;
            let Some(rewind_stack) = rewind_stack else {
                // The pool could not hold the records. They are still where
                // the unwind wrote them, so the guest resumes from them at
                // once and `stack_checkpoint` returns `NOMEM` with nothing
                // stored.
                wasix_begin_rewind(
                    store,
                    interrupts,
                    instance,
                    WasixRewind {
                        stack_lower,
                        stack_upper,
                        stack_pointer,
                        memory_stack: SharedPages::new(memory_stack),
                        rewind_stack: WasixRewindRecords::InPlace { len: unwind.len },
                        value: Some(0),
                        failure: Some(p1::errno::NOMEM),
                    },
                )
                .await?;
                return Ok(true);
            };

            let hash = wasix_next_stack_hash(store);
            let snapshot_bytes = wasix_stack_snapshot_bytes(ret_value, hash);
            if snapshot >= stack_pointer
                && snapshot.saturating_add(WASIX_STACK_SNAPSHOT_SIZE as u32) <= stack_upper
            {
                let offset = u64::from(snapshot - stack_pointer);
                let end = offset + snapshot_bytes.len() as u64;
                if end <= memory_stack.data.len() {
                    // The checkpoint is still this call's alone, so it can
                    // take the hash; once it is shared nothing writes it.
                    memory_stack.patch(
                        Preview1ProgramStore::<CpuImpl, Net, HostFs>::user_pool(),
                        offset,
                        &snapshot_bytes,
                    );
                }
            } else {
                let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
                })?;
                if preview1_write_memory(memory, GuestAddress::wasm32(snapshot), &snapshot_bytes)
                    != p1::errno::SUCCESS
                {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
                    });
                }
            }

            // The stored checkpoint and the rewind that resumes the guest
            // share one copy of each buffer.
            let memory_stack = SharedPages::new(memory_stack);
            let rewind_stack = SharedPages::new(rewind_stack);
            store_checkpoint(
                &mut store.data_mut().asyncify.snapshots,
                hash,
                &memory_stack,
                &rewind_stack,
                stack_pointer,
            );
            wasix_begin_rewind(
                store,
                interrupts,
                instance,
                WasixRewind {
                    stack_lower,
                    stack_upper,
                    stack_pointer,
                    memory_stack,
                    rewind_stack: WasixRewindRecords::Captured(rewind_stack),
                    value: Some(0),
                    failure: None,
                },
            )
            .await?;
            Ok(true)
        }
        WasixAsyncifyPhase::Restoring {
            hash,
            value,
            stack_lower,
        } => {
            wasix_call_instance_func0(store, interrupts, instance, "asyncify_stop_unwind").await?;
            let (memory_stack, rewind_stack, stack_pointer) =
                find_checkpoint(&store.data().asyncify.snapshots, hash).ok_or(
                    ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::StackSnapshotMissing,
                    },
                )?;
            let stack_upper = wasix_global_u32_from_instance(store, instance, "__heap_base")?;
            wasix_begin_rewind(
                store,
                interrupts,
                instance,
                WasixRewind {
                    stack_lower,
                    stack_upper,
                    stack_pointer,
                    memory_stack,
                    rewind_stack: WasixRewindRecords::Captured(rewind_stack),
                    value: Some(value),
                    failure: None,
                },
            )
            .await?;
            Ok(true)
        }
        WasixAsyncifyPhase::Forking {
            stack_lower,
            stack_upper,
            unwind_stack_begin,
            memory_stack,
            stack_pointer,
        } => {
            let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
            })?;
            let unwind =
                wasix_unwind_records(memory, stack_lower, unwind_stack_begin, stack_pointer)?;
            let rewind_stack = capture_rewind_records(store.data(), memory, unwind)?;
            wasix_call_instance_func0(store, interrupts, instance, "asyncify_stop_unwind").await?;
            let memory_stack = SharedPages::new(memory_stack);
            let Some(rewind_stack) = rewind_stack else {
                // The pool could not hold the records: no child is made, the
                // parent resumes from the records where they are, and
                // `proc_fork` returns `NOMEM`.
                wasix_begin_rewind(
                    store,
                    interrupts,
                    instance,
                    WasixRewind {
                        stack_lower,
                        stack_upper,
                        stack_pointer,
                        memory_stack,
                        rewind_stack: WasixRewindRecords::InPlace { len: unwind.len },
                        value: Some(0),
                        failure: Some(p1::errno::NOMEM),
                    },
                )
                .await?;
                return Ok(true);
            };
            let rewind_stack = SharedPages::new(rewind_stack);
            // The child restores from the parent's pages and the parent's own
            // rewind reads the same ones: nothing writes them. The parent's
            // `proc_fork` writes the child's pid into `ret_pid` itself when
            // it is re-entered, so no copy is patched with it.
            let child_pid = spawn_wasix_fork_child(
                store,
                stack_lower,
                stack_upper,
                stack_pointer,
                memory_stack.clone(),
                rewind_stack.clone(),
            )
            .await?;
            wasix_begin_rewind(
                store,
                interrupts,
                instance,
                WasixRewind {
                    stack_lower,
                    stack_upper,
                    stack_pointer,
                    memory_stack,
                    rewind_stack: WasixRewindRecords::Captured(rewind_stack),
                    value: Some(u64::from(child_pid)),
                    failure: None,
                },
            )
            .await?;
            Ok(true)
        }
        WasixAsyncifyPhase::ProcessSnapshot {
            stack_lower,
            stack_upper,
            unwind_stack_begin,
            memory_stack,
            stack_pointer,
        } => {
            let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
            })?;
            let unwind =
                wasix_unwind_records(memory, stack_lower, unwind_stack_begin, stack_pointer)?;
            let rewind_stack = capture_rewind_records(store.data(), memory, unwind)?;
            wasix_call_instance_func0(store, interrupts, instance, "asyncify_stop_unwind").await?;
            let memory_stack = SharedPages::new(memory_stack);
            let Some(rewind_stack) = rewind_stack else {
                // The pool could not hold the records: the guest resumes
                // from them where they are and the call returns `NOMEM`.
                store.data_mut().asyncify.process_snapshot_rewinding = true;
                wasix_begin_rewind(
                    store,
                    interrupts,
                    instance,
                    WasixRewind {
                        stack_lower,
                        stack_upper,
                        stack_pointer,
                        memory_stack,
                        rewind_stack: WasixRewindRecords::InPlace { len: unwind.len },
                        value: None,
                        failure: Some(p1::errno::NOMEM),
                    },
                )
                .await?;
                return Ok(true);
            };
            let rewind_stack = SharedPages::new(rewind_stack);
            // Guest memory swapped out cannot be read from kernel code, so
            // every page of it is read back first.
            let imported = wasix_imported_memory(store.data())?;
            wasix_ensure_memory_present(imported).await?;
            let snapshot = wasix_capture_process_snapshot(
                store,
                stack_lower,
                stack_upper,
                stack_pointer,
                memory_stack.clone(),
                rewind_stack.clone(),
            )?;
            store.data_mut().asyncify.process_snapshot_rewinding = true;
            let failure = match snapshot {
                Some(snapshot) => {
                    store.data_mut().asyncify.process_snapshots.push(snapshot);
                    let snapshot_count = store.data().asyncify.process_snapshots.len();
                    if let Some(snapshot) = store.data().asyncify.process_snapshots.last() {
                        trace_wasix_process_snapshot(snapshot, snapshot_count);
                    }
                    None
                }
                // The pool could not hold the memory: the guest resumes
                // from the copied records and the call returns `NOMEM`.
                None => Some(p1::errno::NOMEM),
            };
            wasix_begin_rewind(
                store,
                interrupts,
                instance,
                WasixRewind {
                    stack_lower,
                    stack_upper,
                    stack_pointer,
                    memory_stack,
                    rewind_stack: WasixRewindRecords::Captured(rewind_stack),
                    value: None,
                    failure,
                },
            )
            .await?;
            Ok(true)
        }
    }
}

pub(super) async fn wasix_begin_rewind<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    interrupts: &StoreInterrupts<CpuImpl>,
    instance: &wasmtime::Instance,
    rewind: WasixRewind,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let WasixRewind {
        stack_lower,
        stack_upper,
        stack_pointer,
        memory_stack,
        rewind_stack,
        value,
        failure,
    } = rewind;
    if stack_lower >= stack_pointer || stack_pointer > stack_upper {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::StackBoundsInvalid,
        });
    }
    let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
    })?;
    // The window's page runs go straight from the frames into the guest.
    restore_pages(
        &memory_stack.data,
        memory,
        GuestAddress::wasm32(stack_pointer),
    )?;
    wasix_set_global_u32_from_instance(store, instance, "__stack_pointer", stack_pointer)?;

    let rewind_stack_begin =
        stack_lower
            .checked_add(WASIX_ASYNCIFY_DATA_SIZE)
            .ok_or(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::StackBoundsInvalid,
            })?;
    let rewind_len = match &rewind_stack {
        WasixRewindRecords::Captured(records) => {
            u32::try_from(records.data.len()).map_err(|_| ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::StackBoundsInvalid,
            })?
        }
        WasixRewindRecords::InPlace { len } => *len,
    };
    let rewind_stack_end = rewind_stack_begin
        .checked_add(rewind_len)
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::StackBoundsInvalid,
        })?;
    if rewind_stack_end > stack_upper {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::StackBoundsInvalid,
        });
    }
    let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
    })?;
    let stack_lower_address = GuestAddress::wasm32(stack_lower);
    let status = preview1_write_u32(memory, stack_lower_address, rewind_stack_end).max(
        preview1_write_u32(memory, stack_lower_address.offset(4), stack_upper),
    );
    if status != p1::errno::SUCCESS {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        });
    }
    // Records that were not copied are still where the unwind wrote them.
    if let WasixRewindRecords::Captured(records) = &rewind_stack {
        restore_pages(
            &records.data,
            memory,
            GuestAddress::wasm32(rewind_stack_begin),
        )?;
    }
    store.data_mut().asyncify.rewind_value = value;
    store.data_mut().asyncify.rewind_failure = failure;
    wasix_call_instance_func1(
        store,
        interrupts,
        instance,
        "asyncify_start_rewind",
        stack_lower,
    )
    .await?;
    Ok(())
}

/// The view of the guest's imported linear memory a process snapshot copies.
pub(super) fn wasix_imported_memory<CpuImpl, Net, HostFs>(
    data: &Preview1ProgramStore<CpuImpl, Net, HostFs>,
) -> Result<Preview1Memory, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let memory = data.imported_memory.as_ref().ok_or(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::ImportedSharedMemoryContractInvalid,
    })?;
    let data = memory.data();
    Ok(Preview1Memory {
        base: data.as_ptr().cast::<u8>() as usize,
        len: data.len(),
    })
}

/// Reads back every swapped-out page of `memory`, which kernel code is
/// about to read directly: a fault taken outside a guest fiber is fatal.
///
/// Platforms without swap have nothing to read back.
pub(super) async fn wasix_ensure_memory_present(
    memory: Preview1Memory,
) -> Result<(), ProgramExecError> {
    let Some(swap) = crate::installed_swap_handle() else {
        return Ok(());
    };
    swap.ensure_present(helios_hal::vmm::VirtRange::new(
        helios_hal::vmm::VirtAddr::new(memory.base),
        memory.len,
    ))
    .await
    .map_err(|error| {
        tracing::warn!(
            target: "helios_kernel::program",
            %error,
            "could not read guest memory back from swap for a process snapshot"
        );
        ProgramExecError {
            kind: ProgramExecErrorKind::Internal,
            detail: ProgramExecErrorDetail::HostOperationFailed,
        }
    })
}

/// Where the records a guest's unwind wrote begin and how long they are,
/// from the end it stored at `stack_lower`, once checked against the stack
/// window the unwind began in.
#[derive(Clone, Copy)]
pub(super) struct UnwindRecords {
    pub(super) begin: u32,
    pub(super) len: u32,
}

pub(super) fn wasix_unwind_records(
    memory: Preview1Memory,
    stack_lower: u32,
    unwind_stack_begin: u32,
    stack_pointer: u32,
) -> Result<UnwindRecords, ProgramExecError> {
    let unwind_stack_finish = preview1_read_u32(memory, GuestAddress::wasm32(stack_lower))?;
    if unwind_stack_finish < unwind_stack_begin || unwind_stack_finish > stack_pointer {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::StackBoundsInvalid,
        });
    }
    Ok(UnwindRecords {
        begin: unwind_stack_begin,
        len: unwind_stack_finish - unwind_stack_begin,
    })
}

/// Copies the records the guest's unwind wrote into pages charged to the
/// store's instance, or, when the user pool cannot hold them, asks the OOM
/// killer for the memory and answers `None`: the unwind has already
/// happened, so the caller rewinds in place and fails the call.
pub(super) fn capture_rewind_records<CpuImpl, Net, HostFs>(
    data: &Preview1ProgramStore<CpuImpl, Net, HostFs>,
    memory: Preview1Memory,
    unwind: UnwindRecords,
) -> Result<Option<ChargedPages>, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    match capture_window(
        Preview1ProgramStore::<CpuImpl, Net, HostFs>::user_pool(),
        data.instance(),
        memory,
        GuestAddress::wasm32(unwind.begin),
        unwind.len as usize,
    ) {
        Ok(records) => Ok(Some(records)),
        Err(PageCaptureError::OutOfBounds(error)) => Err(error),
        Err(PageCaptureError::OutOfMemory {
            requested_bytes, ..
        }) => {
            data.request_user_memory_oom_kill(requested_bytes);
            Ok(None)
        }
    }
}

/// Copies the guest's linear memory into charged pages, leaving all-zero
/// pages as holes, and keeps the process state beside it.
///
/// Answers `None` when the user pool cannot hold the pages, after asking
/// the OOM killer for the memory.
pub(super) fn wasix_capture_process_snapshot<CpuImpl, Net, HostFs>(
    store: &wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    stack_lower: u32,
    stack_upper: u32,
    stack_pointer: u32,
    memory_stack: SharedPages,
    rewind_stack: SharedPages,
) -> Result<Option<WasixProcessSnapshot<Net>>, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let snapshot_started = store
        .data()
        .runtime_state
        .profiling_enabled()
        .then(|| store.data().cpu.now().ticks());
    let memory = wasix_imported_memory(store.data())?;
    let memory_pages = memory.len.div_ceil(WASM_PAGE_SIZE);
    let memory_pages = u32::try_from(memory_pages).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::OutOfMemory,
        detail: ProgramExecErrorDetail::ImportedSharedMemoryBudgetExceeded,
    })?;
    let memory_bytes = match capture_memory_sparse(
        Preview1ProgramStore::<CpuImpl, Net, HostFs>::user_pool(),
        store.data().instance(),
        memory,
    ) {
        Ok(pages) => pages,
        Err(PageCaptureError::OutOfBounds(error)) => return Err(error),
        Err(PageCaptureError::OutOfMemory {
            requested_bytes, ..
        }) => {
            store.data().request_user_memory_oom_kill(requested_bytes);
            return Ok(None);
        }
    };
    if let Some(snapshot_started) = snapshot_started {
        record_program_kernel_profile(
            &store.data().runtime_state,
            &store.data().cpu,
            "asyncify-process-snapshot-memory-copy",
            snapshot_started,
        );
    }
    Ok(Some(WasixProcessSnapshot {
        memory: memory_bytes,
        memory_pages,
        descriptors: store.data().descriptors.clone(),
        filesystem: store.data().filesystem.root_layer(),
        authority: store.data().authority.clone(),
        cwd: store.data().cwd.clone(),
        arguments: store.data().arguments.clone(),
        environment: store.data().environment.clone(),
        signal_dispositions: store.data().signal_dispositions.clone(),
        stack_lower,
        stack_upper,
        stack_pointer,
        memory_stack,
        rewind_stack,
    }))
}

pub(super) fn trace_wasix_process_snapshot<Net: ComponentHostNetwork>(
    snapshot: &WasixProcessSnapshot<Net>,
    snapshot_count: usize,
) {
    let cwd = snapshot
        .cwd
        .as_ref()
        .map(|cwd| cwd.guest_name.as_str())
        .unwrap_or("");
    let _filesystem = &snapshot.filesystem;
    tracing::debug!(
        snapshot_count,
        memory_bytes = snapshot.memory.data.len(),
        held_bytes = snapshot.memory.charge.bytes(),
        memory_pages = snapshot.memory_pages,
        descriptors = snapshot.descriptors.entries.len(),
        directory_preopens = snapshot.authority.directory_preopens().len(),
        cwd,
        args = snapshot.arguments.len(),
        env = snapshot.environment.len(),
        signal_dispositions = snapshot.signal_dispositions.len(),
        stack_lower = snapshot.stack_lower,
        stack_upper = snapshot.stack_upper,
        stack_pointer = snapshot.stack_pointer,
        memory_stack_bytes = snapshot.memory_stack.data.len(),
        rewind_stack_bytes = snapshot.rewind_stack.data.len(),
        "captured explicit WASIX process snapshot"
    );
}

pub(super) async fn spawn_wasix_fork_child<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    stack_lower: u32,
    stack_upper: u32,
    stack_pointer: u32,
    memory_stack: SharedPages,
    rewind_stack: SharedPages,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let memory = store
        .data()
        .imported_memory
        .as_ref()
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::ImportedSharedMemoryContractInvalid,
        })?;
    // Clone the parent memory handle and re-derive its data view after
    // the acquire below: the raw view is not Send across the await.
    let parent_memory = memory.clone();
    let current_bytes = parent_memory.data_size();
    let current_pages = current_bytes.div_ceil(WASM_PAGE_SIZE);
    let current_pages = u32::try_from(current_pages).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::OutOfMemory,
        detail: ProgramExecErrorDetail::ImportedSharedMemoryBudgetExceeded,
    })?;
    let service = store
        .data()
        .runtime_state
        .program_service()
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::Unavailable,
            detail: ProgramExecErrorDetail::HostOperationFailed,
        })?;
    // A fork gets its own shared memory, so it has to be sized against
    // what the user pool can still commit rather than against the fixed
    // per-program ceiling: on backends that commit the whole maximum up
    // front the ceiling is unallocatable once the parent holds one.
    let maximum_pages = user_shared_memory_budget_pages();
    if maximum_pages < current_pages {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: ProgramExecErrorDetail::ImportedSharedMemoryBudgetExceeded,
        });
    }
    let memory_spec = SharedMemorySpec {
        initial_pages: current_pages,
        maximum_pages,
    };
    // Fork bursts in shell pipelines outrun the background scrub just
    // like exec spawns do; wait for an in-flight recycle before treating
    // the shortage as a real OOM.
    let fork_memory = acquire_or_wait_for_recycle(
        &service.inner.shared_memory_pool,
        service.inner.engine.raw(),
        memory_spec,
    )
    .await?;
    let fork_data = fork_memory.data();
    if fork_data.len() < current_bytes {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: ProgramExecErrorDetail::ImportedSharedMemoryBudgetExceeded,
        });
    }
    let copy_started = store
        .data()
        .runtime_state
        .profiling_enabled()
        .then(|| store.data().cpu.now().ticks());
    unsafe {
        core::ptr::copy_nonoverlapping(
            parent_memory.data().as_ptr().cast::<u8>(),
            fork_data.as_ptr().cast::<u8>().cast_mut(),
            current_bytes,
        );
    }
    if let Some(copy_started) = copy_started {
        record_program_kernel_profile(
            &store.data().runtime_state,
            &store.data().cpu,
            "asyncify-fork-memory-copy",
            copy_started,
        );
    }
    let compiled = store
        .data()
        .current_core_module
        .clone()
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::ImageReplacementUnavailable,
        })?;
    let restore = Box::new(CoreModuleRestore {
        memory: fork_memory,
        memory_spec,
        descriptors: store.data().descriptors.clone(),
        signal_dispositions: store.data().signal_dispositions.clone(),
        rewind: WasixRewind {
            stack_lower,
            stack_upper,
            stack_pointer,
            memory_stack,
            rewind_stack: WasixRewindRecords::Captured(rewind_stack),
            value: Some(0),
            failure: None,
        },
    });
    // A fork inherits the parent's argument vector verbatim.
    let argv = ProgramArgv::inherited(store.data().arguments.clone()).ok_or(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::InvalidEntryPoint,
    })?;
    let environment = store.data().environment.clone();
    let (_, stdin_reader) = crate::byte_channel();
    let output_mode = OutputMode::RoutedChild {
        stdin_rx: stdin_reader,
        stdout: store
            .data()
            .output_route(crate::ComponentOutputStreamKind::Stdout),
        stderr: store
            .data()
            .output_route(crate::ComponentOutputStreamKind::Stderr),
    };
    let mut child = service.spawn_loaded_with_output_mode(
        store.data().exec_context(),
        ProgramExecutable::ForkedCoreModule { compiled, restore },
        ProgramLaunch::new(
            argv,
            environment,
            store.data().authority.clone(),
            Some(store.data().filesystem.clone()),
        ),
        output_mode,
        ChildStdio {
            stdin: None,
            stdout: None,
            stderr: None,
        },
    )?;
    let pid = u32::try_from(child.instance_id.raw()).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::Internal,
        detail: ProgramExecErrorDetail::InternalInvariant,
    })?;
    let child_signal_state = child.signal_state();
    let exit = child.take_wait().ok_or(ProgramExecError {
        kind: ProgramExecErrorKind::Internal,
        detail: ProgramExecErrorDetail::ChildExitAlreadyConsumed,
    })?;
    store.data_mut().insert_child(pid, child_signal_state, exit);
    Ok(pid)
}

pub(super) fn wasix_next_stack_hash<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
) -> u128
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let data = store.data_mut();
    let lower = data.entropy.insecure_u64(data.runtime_state.root_entropy());
    let upper = data.entropy.insecure_u64(data.runtime_state.root_entropy());
    let hash = (u128::from(upper) << 64) | u128::from(lower);
    if hash == 0 { 1 } else { hash }
}

pub(super) fn wasix_stack_snapshot_bytes(user: u32, hash: u128) -> [u8; WASIX_STACK_SNAPSHOT_SIZE] {
    let mut bytes = [0_u8; WASIX_STACK_SNAPSHOT_SIZE];
    bytes[0..8].copy_from_slice(&u64::from(user).to_le_bytes());
    bytes[8..16].copy_from_slice(&(hash as u64).to_le_bytes());
    bytes[16..24].copy_from_slice(&((hash >> 64) as u64).to_le_bytes());
    bytes
}

pub(super) async fn wasix_call_instance_func0<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    interrupts: &StoreInterrupts<CpuImpl>,
    instance: &wasmtime::Instance,
    name: &str,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let function = instance
        .get_typed_func::<(), ()>(&mut *store, name)
        .map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::UnwindExportInvalid,
        })?;
    let turns = store.data().guest_turns();
    interrupts
        .drive(turns.drive(function.call_async(&mut *store, ())))
        .await
        .map_err(map_program_runtime_error)
}

pub(super) async fn wasix_call_instance_func1<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    interrupts: &StoreInterrupts<CpuImpl>,
    instance: &wasmtime::Instance,
    name: &str,
    value: u32,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let function = instance
        .get_typed_func::<i32, ()>(&mut *store, name)
        .map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::UnwindExportInvalid,
        })?;
    let turns = store.data().guest_turns();
    interrupts
        .drive(turns.drive(function.call_async(&mut *store, value as i32)))
        .await
        .map_err(map_program_runtime_error)
}

pub(super) fn wasix_global_u32_from_instance<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    instance: &wasmtime::Instance,
    name: &str,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let global = instance
        .get_global(&mut *store, name)
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::UnwindExportInvalid,
        })?;
    match global.get(&mut *store) {
        Val::I32(value) => Ok(value as u32),
        _ => Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryTypeMismatch,
        }),
    }
}

pub(super) fn wasix_set_global_u32_from_instance<CpuImpl, Net, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, Net, HostFs>>,
    instance: &wasmtime::Instance,
    name: &str,
    value: u32,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let global = instance
        .get_global(&mut *store, name)
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::UnwindExportInvalid,
        })?;
    global
        .set(&mut *store, Val::I32(value as i32))
        .map_err(map_program_runtime_error)
}

pub(super) fn validate_preview1_program_module_imports(
    module: &Module,
) -> Result<(), ProgramExecError> {
    for import in module.imports() {
        if import.module() == "env" && import.name() == "memory" {
            match import.ty() {
                ExternType::Memory(memory) if memory.is_shared() => continue,
                _ => {}
            }
        }
        validate_preview1_program_import(import.module(), import.name())?;
    }
    Ok(())
}

pub(super) fn core_module_imports_wasix(module: &Module) -> bool {
    module
        .imports()
        .any(|import| import.module() == WASIX_MODULE)
}

pub(super) fn validate_preview1_program_import(
    module_name: &str,
    name: &str,
) -> Result<(), ProgramExecError> {
    match module_name {
        "wasi_snapshot_preview1" | "wasi_unstable" => {
            if p1::PREVIEW1_FUNCTIONS.contains(&name)
                && PREVIEW1_PROGRAM_LINKED_IMPORTS.contains(&name)
            {
                return Ok(());
            }
        }
        WASIX_MODULE if crate::wasmtime_adapter::wasix::LINKED_IMPORTS.contains(&name) => {
            return Ok(());
        }
        _ => {}
    }
    tracing::error!(
        module = module_name,
        import = name,
        "program core module imports unsupported host function"
    );
    Err(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::UnsupportedImport,
    })
}

/// Decide whether a compile failure means the cached compiler-plugin
/// runtime is no longer usable. OOM kills come with non-deterministic
/// SharedMemory state because a worker thread may have aborted
/// mid-write; rebuilding from scratch is the safe path. Plain compile
/// errors (invalid input wasm, ABI mismatch) leave the plugin healthy.
pub(super) fn plugin_runtime_should_be_recycled(error: &ProgramExecError) -> bool {
    matches!(
        error.kind,
        ProgramExecErrorKind::OutOfMemory | ProgramExecErrorKind::Internal
    )
}

/// Whether `error` is, or was caused by, the user pool refusing an allocation.
///
/// `Error::is` only inspects the outermost error, and wasmtime wraps whatever
/// the memory creator returned in its own context, so an out-of-memory that
/// arrives through `SharedMemory::new` was being classified as an internal
/// failure. Walking the chain is what tells a caller it ran out of memory
/// rather than hitting a bug (#62).
fn caused_by_out_of_memory(error: &wasmtime::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<crate::ProgramOutOfMemory>())
}

/// A task the executor refused to place for this instance.
///
/// The instance asked for more executor capacity than the machine has
/// left for user-mode work; it takes the failure, the kernel does not.
pub(super) fn map_task_capacity_error(error: crate::TaskCapacityError) -> ProgramExecError {
    tracing::warn!(
        target: "helios_kernel::program",
        %error,
        "refused an instance task: the executor's instance share is full"
    );
    ProgramExecError {
        kind: ProgramExecErrorKind::OutOfMemory,
        detail: ProgramExecErrorDetail::ExecutorTaskCapacityExhausted,
    }
}

pub(super) fn map_program_runtime_error(error: wasmtime::Error) -> ProgramExecError {
    if caused_by_out_of_memory(&error) {
        tracing::error!(?error, "program runtime reported out of memory");
        return ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: ProgramExecErrorDetail::RuntimeFailure(RuntimeMessage::of(&error)),
        };
    }
    if let Some(killed) = error.downcast_ref::<crate::InstanceKilled>() {
        tracing::error!(?error, reason = ?killed.reason, "program instance was killed");
        let kind = match killed.reason {
            crate::KillReason::OutOfMemory => ProgramExecErrorKind::OutOfMemory,
            // Both are somebody outside the program deciding it should
            // stop, which is nothing the program's own caller did wrong.
            crate::KillReason::SupervisorRestart | crate::KillReason::Operator => {
                ProgramExecErrorKind::Internal
            }
        };
        return ProgramExecError {
            kind,
            detail: ProgramExecErrorDetail::RuntimeFailure(RuntimeMessage::of(&error)),
        };
    }

    tracing::error!(?error, "program runtime operation failed");
    ProgramExecError {
        kind: ProgramExecErrorKind::Internal,
        detail: ProgramExecErrorDetail::RuntimeFailure(RuntimeMessage::of(&error)),
    }
}

pub(super) fn map_artifact_trust_error(error: ArtifactTrustError) -> ProgramExecError {
    tracing::error!(?error, "artifact trust check failed");
    ProgramExecError {
        kind: ProgramExecErrorKind::InvalidSignature,
        detail: ProgramExecErrorDetail::ArtifactSignatureInvalid,
    }
}

pub(super) fn map_artifact_profile_error(error: ArtifactProfileError) -> ProgramExecError {
    tracing::error!(?error, "artifact profile check failed");
    ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::ArtifactProfileInvalid,
    }
}

pub(super) fn trusted_bootfs_payload(bytes: &BootfsBytes) -> Result<Bytes, ProgramExecError> {
    let trusted = cwasm::trust_bootfs_artifact(bytes).map_err(map_artifact_trust_error)?;
    Ok(bytes.to_bytes().slice(..trusted.payload().len()))
}

pub(super) fn trusted_signed_payload(bytes: &Bytes) -> Result<Bytes, ProgramExecError> {
    let trusted = cwasm::verify_signed_artifact(UntrustedCwasm::new(bytes))
        .map_err(map_artifact_trust_error)?;
    Ok(bytes.slice(..trusted.payload().len()))
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::vec;
    use core::alloc::Layout;

    use helios_hal::cpu::current_processor;
    use helios_hal::pmm::PhysFrame;

    use super::*;

    const PAGE: usize = PhysFrame::SIZE;
    /// What a pool's memory holds before anything uses it, so a byte a
    /// capture failed to set shows at once.
    const GARBAGE: u8 = 0xAA;

    /// A pool over `frames` frames of leaked host memory, filled with
    /// garbage.
    fn pool(frames: usize) -> &'static crate::UserMemoryPool {
        let bytes = frames * PAGE;
        let layout = Layout::from_size_align(bytes, PAGE).expect("pool layout");
        // SAFETY: a non-zero layout.
        let start = unsafe { alloc::alloc::alloc(layout) };
        assert!(!start.is_null(), "host allocation for the test pool failed");
        // SAFETY: `bytes` bytes just allocated.
        unsafe { start.write_bytes(GARBAGE, bytes) };
        let pool = Box::leak(Box::new(crate::UserMemoryPool::empty()));
        pool.initialize(&[(start as usize, start as usize + bytes)]);
        pool.configure_processors(1);
        pool
    }

    fn allocated(pool: &crate::UserMemoryPool) -> usize {
        pool.stats().allocated_bytes
    }

    /// Takes every frame the pool has but `spare`, and answers them so the
    /// test can hand them back.
    fn leave_frames(
        pool: &'static crate::UserMemoryPool,
        spare: usize,
    ) -> Vec<core::ptr::NonNull<u8>> {
        let layout = Layout::from_size_align(PAGE, PAGE).expect("frame layout");
        let mut held = Vec::new();
        while let Ok((frame, _)) = pool.allocate_uninit_on(current_processor(), layout) {
            held.push(frame);
        }
        for _ in 0..spare {
            let frame = held.pop().expect("the pool had frames");
            pool.deallocate_bytes_on(current_processor(), frame, layout);
        }
        held
    }

    /// Guest memory the test owns: `pages` pages with the listed ones
    /// non-zero, each holding bytes that differ from page to page.
    fn guest_memory(pages: usize, non_zero: &[usize]) -> Vec<u8> {
        let mut bytes = vec![0_u8; pages * PAGE];
        for &page in non_zero {
            for (offset, byte) in bytes[page * PAGE..(page + 1) * PAGE].iter_mut().enumerate() {
                *byte = (page * 31 + offset * 7 + 1) as u8;
            }
        }
        bytes
    }

    fn view(bytes: &[u8]) -> Preview1Memory {
        Preview1Memory {
            base: bytes.as_ptr() as usize,
            len: bytes.len(),
        }
    }

    /// The view a restore writes through.
    fn view_mut(bytes: &mut [u8]) -> Preview1Memory {
        Preview1Memory {
            base: bytes.as_mut_ptr() as usize,
            len: bytes.len(),
        }
    }

    /// The charge of a process snapshot is the frames of its non-zero pages
    /// and the one index node above them: the zero pages stay holes and
    /// cost nothing.
    #[test]
    fn a_snapshot_is_charged_for_its_non_zero_pages_alone() {
        let pool = pool(64);
        let registry = crate::InstanceRegistry::new();
        let instance = registry.register("snapshotter", 0);
        let guest = guest_memory(16, &[1, 2, 9]);
        let before = allocated(pool);

        let snapshot =
            capture_memory_sparse(pool, &instance, view(&guest)).expect("the pool holds it");

        let non_zero_pages = 3;
        let index_nodes = 1;
        let held = (non_zero_pages + index_nodes) * PAGE;
        assert_eq!(
            snapshot.data.frames(),
            (non_zero_pages + index_nodes) as u64
        );
        assert_eq!(snapshot.charge.bytes(), held as u64);
        assert_eq!(
            allocated(pool) - before,
            held,
            "the pool holds what is charged"
        );
        assert_eq!(instance.user_pool_bytes(), held as u64);
        assert_eq!(snapshot.data.len(), guest.len() as u64);

        drop(snapshot);
        assert_eq!(allocated(pool), before);
        assert_eq!(instance.user_pool_bytes(), 0);
    }

    /// A restore writes back the exact bytes captured, holes included, into
    /// memory that held something else.
    #[test]
    fn a_restore_writes_back_the_exact_bytes() {
        let pool = pool(64);
        let registry = crate::InstanceRegistry::new();
        let instance = registry.register("restorer", 0);

        // A window that starts and ends inside pages.
        let guest = guest_memory(5, &[0, 1, 2, 3, 4]);
        let (start, len) = (PAGE / 2 + 5, 3 * PAGE + 77);
        let window = capture_window(
            pool,
            &instance,
            view(&guest),
            GuestAddress::wasm32(start as u32),
            len,
        )
        .expect("the pool holds it");
        assert_eq!(window.data.len(), len as u64);
        let mut elsewhere = vec![GARBAGE; guest.len()];
        restore_pages(
            &window.data,
            view_mut(&mut elsewhere),
            GuestAddress::wasm32(start as u32),
        )
        .expect("inside memory");
        assert_eq!(elsewhere[start..start + len], guest[start..start + len]);
        assert!(elsewhere[..start].iter().all(|byte| *byte == GARBAGE));
        assert!(elsewhere[start + len..].iter().all(|byte| *byte == GARBAGE));

        // A sparse snapshot restores its holes as zeros.
        let sparse = guest_memory(16, &[0, 7, 15]);
        let snapshot =
            capture_memory_sparse(pool, &instance, view(&sparse)).expect("the pool holds it");
        elsewhere = vec![GARBAGE; sparse.len()];
        restore_pages(
            &snapshot.data,
            view_mut(&mut elsewhere),
            GuestAddress::wasm32(0),
        )
        .expect("inside memory");
        assert_eq!(elsewhere, sparse);
    }

    /// A pool that cannot hold the window refuses the call before it
    /// leaves `Idle` or touches the guest, holds nothing and charges
    /// nothing; one that can arms the phase with the copy.
    #[test]
    fn an_exhausted_pool_answers_nomem_with_the_phase_idle() {
        let pool = pool(64);
        let registry = crate::InstanceRegistry::new();
        let instance = registry.register("caller", 0);
        let guest = guest_memory(8, &[0, 1, 2, 3, 4, 5, 6, 7]);
        let window = StackWindow {
            memory: view(&guest),
            stack_pointer: 0,
            stack_upper: guest.len() as u32,
        };
        let held = leave_frames(pool, 4);
        let before = allocated(pool);

        let mut phase = WasixAsyncifyPhase::Idle;
        let mut prepared = false;
        let refused = arm_unwind(
            &mut phase,
            pool,
            &instance,
            window,
            || {
                prepared = true;
                p1::errno::SUCCESS
            },
            |memory_stack| WasixAsyncifyPhase::ProcessSnapshot {
                stack_lower: 0,
                stack_upper: window.stack_upper,
                unwind_stack_begin: 0,
                memory_stack,
                stack_pointer: 0,
            },
        );

        // Eight data pages and their index node, against four free frames.
        assert_eq!(
            refused,
            Err(UnwindRefusal::OutOfMemory {
                requested_bytes: 9 * PAGE as u64
            })
        );
        assert!(
            matches!(phase, WasixAsyncifyPhase::Idle),
            "the phase stays Idle"
        );
        assert!(!prepared, "nothing was written to the guest");
        assert_eq!(allocated(pool), before, "the partial reservation went back");
        assert_eq!(instance.user_pool_bytes(), 0, "nothing is charged");

        // Half the window fits, and arms.
        let half = StackWindow {
            stack_upper: (2 * PAGE) as u32,
            ..window
        };
        let armed = arm_unwind(
            &mut phase,
            pool,
            &instance,
            half,
            || p1::errno::SUCCESS,
            |memory_stack| WasixAsyncifyPhase::ProcessSnapshot {
                stack_lower: 0,
                stack_upper: half.stack_upper,
                unwind_stack_begin: 0,
                memory_stack,
                stack_pointer: 0,
            },
        );
        assert_eq!(armed, Ok(()));
        let WasixAsyncifyPhase::ProcessSnapshot { memory_stack, .. } = &phase else {
            panic!("the window was captured, so the phase is armed");
        };
        assert_eq!(memory_stack.charge.bytes(), 3 * PAGE as u64);
        assert_eq!(instance.user_pool_bytes(), 3 * PAGE as u64);

        drop(phase);
        let layout = Layout::from_size_align(PAGE, PAGE).expect("frame layout");
        for frame in held {
            pool.deallocate_bytes_on(current_processor(), frame, layout);
        }
    }

    /// The stored checkpoint, the rewind that captured it and every restore
    /// hold one copy: storing and restoring clone the `Arc` and add nothing
    /// to the charge.
    #[test]
    fn a_checkpoint_and_its_restore_share_one_copy() {
        let pool = pool(64);
        let registry = crate::InstanceRegistry::new();
        let instance = registry.register("checkpointer", 0);
        let guest = guest_memory(3, &[0, 1, 2]);
        let capture = |len| {
            SharedPages::new(
                capture_window(pool, &instance, view(&guest), GuestAddress::wasm32(0), len)
                    .expect("the pool holds it"),
            )
        };
        let memory_stack = capture(2 * PAGE);
        let rewind_stack = capture(PAGE);
        let held = instance.user_pool_bytes();

        let mut snapshots = Vec::new();
        store_checkpoint(&mut snapshots, 7, &memory_stack, &rewind_stack, 64);
        assert_eq!(
            SharedPages::count(&memory_stack),
            2,
            "the rewind and the store"
        );
        let (restored_stack, restored_records, stack_pointer) =
            find_checkpoint(&snapshots, 7).expect("stored");
        assert!(SharedPages::ptr_eq(&restored_stack, &memory_stack));
        assert!(SharedPages::ptr_eq(&restored_records, &rewind_stack));
        assert_eq!(
            SharedPages::count(&memory_stack),
            3,
            "the restore shares it"
        );
        assert_eq!(stack_pointer, 64);
        assert_eq!(
            instance.user_pool_bytes(),
            held,
            "a restore commits no pages"
        );
        assert!(find_checkpoint(&snapshots, 8).is_none());
    }
}
