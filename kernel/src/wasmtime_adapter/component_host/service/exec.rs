use super::*;

pub(super) enum ProgramExecutable<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    Component(Arc<ComponentInstancePre<StoreData<CpuImpl, HostFs>>>),
    CoreModule(Arc<WasmtimeCompiledCoreModule>),
    ForkedCoreModule {
        compiled: Arc<WasmtimeCompiledCoreModule>,
        restore: CoreModuleRestore,
    },
}

pub(super) struct CoreModuleRestore {
    pub(super) memory: SharedMemory,
    pub(super) memory_spec: SharedMemorySpec,
    pub(super) descriptors: Preview1DescriptorTable,
    pub(super) signal_dispositions: Vec<WasixSignalDisposition>,
    pub(super) stack_lower: u32,
    pub(super) stack_upper: u32,
    pub(super) stack_pointer: u32,
    pub(super) memory_stack: Vec<u8>,
    pub(super) rewind_stack: Vec<u8>,
    pub(super) value: u64,
}

pub(super) struct WasixExecReplacement<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) name: String,
    pub(super) args: Vec<String>,
    pub(super) env: Vec<(String, String)>,
    pub(super) executable: ProgramExecutable<CpuImpl, HostFs>,
    pub(super) authority: ProcessAuthority,
    pub(super) filesystem: Option<DebugFileSystemSnapshot>,
    pub(super) descriptors: Option<Preview1DescriptorTable>,
    pub(super) signal_state: WasixSignalState,
    pub(super) signal_dispositions: Vec<WasixSignalDisposition>,
}

pub(super) struct CoreModuleReplacementState<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) exec_context: ProgramExecContext<CpuImpl, HostFs>,
    pub(super) instance: crate::RegisteredInstance,
    pub(super) output_mode: OutputMode,
    pub(super) core_linker: CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
}

pub(super) enum CoreModuleRunCompletion<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    Exit(Result<ChildExit, ProgramExecError>),
    Exec(WasixExecReplacement<CpuImpl, HostFs>),
}

pub(super) struct WasixAsyncifyState {
    pub(super) snapshots: Vec<WasixStackSnapshot>,
    pub(super) process_snapshots: Vec<WasixProcessSnapshot>,
    pub(super) phase: WasixAsyncifyPhase,
    pub(super) rewind_value: Option<u64>,
    pub(super) process_snapshot_rewinding: bool,
}

pub(super) enum WasixAsyncifyPhase {
    Idle,
    Capturing {
        snapshot: u32,
        ret_value: u32,
        stack_lower: u32,
        stack_upper: u32,
        unwind_stack_begin: u32,
        memory_stack: Vec<u8>,
        stack_pointer: u32,
    },
    Restoring {
        hash: u128,
        value: u64,
        stack_lower: u32,
    },
    Forking {
        ret_pid: u32,
        stack_lower: u32,
        stack_upper: u32,
        unwind_stack_begin: u32,
        memory_stack: Vec<u8>,
        stack_pointer: u32,
    },
    ProcessSnapshot {
        stack_lower: u32,
        stack_upper: u32,
        unwind_stack_begin: u32,
        memory_stack: Vec<u8>,
        stack_pointer: u32,
    },
}

#[derive(Clone)]
pub(super) struct WasixStackSnapshot {
    pub(super) hash: u128,
    pub(super) memory_stack: Vec<u8>,
    pub(super) rewind_stack: Vec<u8>,
    pub(super) stack_pointer: u32,
}

#[derive(Clone)]
pub(super) struct WasixProcessSnapshot {
    pub(super) memory: Vec<u8>,
    pub(super) memory_pages: u32,
    pub(super) descriptors: Preview1DescriptorTable,
    pub(super) filesystem: DebugFileSystemSnapshot,
    pub(super) authority: ProcessAuthority,
    pub(super) cwd: Option<Preview1Cwd>,
    pub(super) arguments: Vec<String>,
    pub(super) environment: Vec<(String, String)>,
    pub(super) signal_dispositions: Vec<WasixSignalDisposition>,
    pub(super) stack_lower: u32,
    pub(super) stack_upper: u32,
    pub(super) stack_pointer: u32,
    pub(super) memory_stack: Vec<u8>,
    pub(super) rewind_stack: Vec<u8>,
}

pub(super) fn read_bootfs_artifact<CpuImpl, HostFs>(
    runtime_state: &HostRuntimeState<CpuImpl, HostFs>,
    path: &str,
) -> Option<Bytes>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let filesystem = crate::wasmtime_adapter::wasi::DebugFileSystem::<
        HostRuntimeState<CpuImpl, HostFs>,
        HostFs,
    >::new(runtime_state.clone());
    filesystem.read_program_file_bytes(path).ok()
}

impl WasixAsyncifyState {
    pub(super) const fn new() -> Self {
        Self {
            snapshots: Vec::new(),
            process_snapshots: Vec::new(),
            phase: WasixAsyncifyPhase::Idle,
            rewind_value: None,
            process_snapshot_rewinding: false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_program_executable<CpuImpl, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, HostFs>,
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    filesystem: Option<DebugFileSystemSnapshot>,
    descriptors: Option<Preview1DescriptorTable>,
    signal_state: WasixSignalState,
    signal_dispositions: Vec<WasixSignalDisposition>,
    spawner: crate::Spawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    executable: ProgramExecutable<CpuImpl, HostFs>,
    engine: &crate::wasmtime_adapter::WasmtimeEngine,
    runtime: &crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    preview1_core_linker: CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
    shared_memory_pool: Arc<Mutex<SharedMemoryPool>>,
    core_module_instance_pre_cache: Arc<
        Mutex<ComponentCache<InstancePre<Preview1ProgramStore<CpuImpl, HostFs>>>>,
    >,
    launched_instance: crate::RegisteredInstance,
    output_mode: OutputMode,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    match executable {
        ProgramExecutable::Component(compiled) => {
            run_program_component(
                exec_context,
                name,
                args,
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
            )
            .await
        }
        ProgramExecutable::CoreModule(compiled) => {
            run_program_core_module(
                exec_context,
                name,
                args,
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
            )
            .await
        }
        ProgramExecutable::ForkedCoreModule { compiled, restore } => {
            run_program_core_module_with_restore(
                exec_context,
                name,
                args,
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
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_program_core_module<CpuImpl, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, HostFs>,
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    filesystem: Option<DebugFileSystemSnapshot>,
    descriptors: Option<Preview1DescriptorTable>,
    signal_state: WasixSignalState,
    signal_dispositions: Vec<WasixSignalDisposition>,
    spawner: crate::Spawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    compiled: Arc<WasmtimeCompiledCoreModule>,
    engine: &crate::wasmtime_adapter::WasmtimeEngine,
    runtime: &crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    preview1_core_linker: CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
    shared_memory_pool: Arc<Mutex<SharedMemoryPool>>,
    core_module_instance_pre_cache: Arc<
        Mutex<ComponentCache<InstancePre<Preview1ProgramStore<CpuImpl, HostFs>>>>,
    >,
    launched_instance: crate::RegisteredInstance,
    output_mode: OutputMode,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let profile_name = name.clone();
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(name);
    argv.extend(args);
    let run_started_at = monotonic_nanos(&exec_context.cpu);
    let run_cpu = exec_context.cpu.clone();
    let run_timer = exec_context.timer.clone();
    let profile_cpu = exec_context.cpu.clone();
    let profile_runtime_state = exec_context.runtime_state.clone();
    let instance_id = launched_instance.id();
    let wasix_abi = core_module_imports_wasix(&compiled.module);
    let replacement_state = wasix_abi.then(|| CoreModuleReplacementState {
        exec_context: exec_context.clone(),
        instance: launched_instance.clone(),
        output_mode: output_mode.clone(),
        core_linker: preview1_core_linker.clone(),
    });
    let recycle_spawner = exec_context.spawner.clone();
    let shared_memory_prepare_profile =
        start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
    let imported_memory_spec = imported_shared_memory_spec_with_user_budget(&compiled.module)?;
    let imported_memory = match imported_memory_spec {
        Some(spec) => Some(shared_memory_pool.lock().acquire(engine.raw(), spec)?),
        None => None,
    };
    record_program_kernel_profile_sample(
        shared_memory_prepare_profile,
        "core-shared-memory-prepare",
    );
    let recycle_memory = imported_memory.clone();
    let store_teardown_profile: Option<ProgramKernelProfile<CpuImpl, HostFs>>;
    let (completion, recycle_allowed) = {
        let store_prepare_profile =
            start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
        let mut store = wasmtime::Store::new(
            engine.raw(),
            Preview1ProgramStore::<CpuImpl, HostFs>::new(
                exec_context.cpu,
                exec_context.timer,
                exec_context.spawner.clone(),
                exec_context.runtime_state,
                launched_instance,
                exec_context.parent_instance_id,
                argv,
                env,
                authority,
                output_mode,
                exec_context.read_serial,
                exec_context.write_serial,
                imported_memory.clone(),
                filesystem,
                descriptors,
                signal_state,
                signal_dispositions,
                Some(compiled.clone()),
                wasix_abi,
            ),
        );
        configure_preview1_program_store(&mut store);
        record_program_kernel_profile_sample(store_prepare_profile, "core-store-prepare");

        let instance = if let Some(memory) = imported_memory {
            let mut linker = preview1_core_linker;
            let imported_memory_define_profile =
                start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
            define_imported_shared_memory(&mut linker, &store, &compiled.module, memory)?;
            record_program_kernel_profile_sample(
                imported_memory_define_profile,
                "core-shared-memory-define",
            );

            super::emit_program_stage_marker(
                exec_context.write_serial,
                "program:instantiate-core-begin",
            );
            let instantiate_started = profile_cpu.now().ticks();
            let instance = linker.instantiate_async(&mut store, &compiled.module).await;
            record_named_program_kernel_profile(
                &profile_runtime_state,
                &profile_cpu,
                "instantiate-core",
                &profile_name,
                instantiate_started,
            );
            record_program_kernel_profile(
                &profile_runtime_state,
                &profile_cpu,
                "instantiate-core",
                instantiate_started,
            );
            instance
        } else {
            let cache_lookup_profile =
                start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
            let cached_instance_pre = core_module_instance_pre_cache
                .lock()
                .get(&compiled.cache_key);
            record_program_kernel_profile_sample(
                cache_lookup_profile,
                "core-instance-pre-cache-lookup",
            );
            let instance_pre = if let Some(instance_pre) = cached_instance_pre {
                super::emit_program_stage_marker(
                    exec_context.write_serial,
                    "program:instantiate-core-pre-cache-hit",
                );
                instance_pre
            } else {
                super::emit_program_stage_marker(
                    exec_context.write_serial,
                    "program:instantiate-core-pre-begin",
                );
                let instantiate_pre_profile =
                    start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
                let instance_pre = Arc::new(
                    preview1_core_linker
                        .instantiate_pre(&compiled.module)
                        .map_err(map_program_runtime_error)?,
                );
                record_program_kernel_profile_sample(
                    instantiate_pre_profile,
                    "instantiate-core-pre",
                );
                super::emit_program_stage_marker(
                    exec_context.write_serial,
                    "program:instantiate-core-pre-end",
                );
                let cache_insert_profile =
                    start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
                let inserted = core_module_instance_pre_cache
                    .lock()
                    .insert_if_missing(compiled.cache_key.clone(), instance_pre);
                record_program_kernel_profile_sample(
                    cache_insert_profile,
                    "core-instance-pre-cache-insert",
                );
                inserted
            };

            super::emit_program_stage_marker(
                exec_context.write_serial,
                "program:instantiate-core-begin",
            );
            let instantiate_started = profile_cpu.now().ticks();
            let instance = instance_pre.instantiate_async(&mut store).await;
            record_named_program_kernel_profile(
                &profile_runtime_state,
                &profile_cpu,
                "instantiate-core",
                &profile_name,
                instantiate_started,
            );
            record_program_kernel_profile(
                &profile_runtime_state,
                &profile_cpu,
                "instantiate-core",
                instantiate_started,
            );
            instance
        };
        let instance = instance.map_err(map_program_runtime_error)?;
        super::emit_program_stage_marker(exec_context.write_serial, "program:instantiate-core-ok");

        let resolve_start_profile =
            start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
        let start = instance
            .get_typed_func::<(), ()>(&mut store, "_start")
            .map_err(|_| ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::InvalidEntryPoint,
            })?;
        record_program_kernel_profile_sample(resolve_start_profile, "resolve-core-start");

        let run_done = Arc::new(core::sync::atomic::AtomicBool::new(false));
        super::spawn_component_phase_heartbeat(
            &spawner,
            &run_cpu,
            &run_timer,
            &progress,
            exec_context.write_serial,
            "program:run-core",
            run_started_at,
            &run_done,
        );
        super::emit_program_stage_marker(exec_context.write_serial, "program:run-core-begin");
        let run_phase_started = profile_cpu.now().ticks();
        let result = loop {
            let result = start.call_async(&mut store, ()).await;
            if handle_wasix_asyncify_completion(&mut store, &instance).await? {
                continue;
            }
            break result;
        };
        record_named_program_kernel_profile(
            &profile_runtime_state,
            &profile_cpu,
            "run-core",
            &profile_name,
            run_phase_started,
        );
        record_program_kernel_profile(
            &profile_runtime_state,
            &profile_cpu,
            "run-core",
            run_phase_started,
        );
        run_done.store(true, core::sync::atomic::Ordering::Release);
        super::emit_program_stage_marker(exec_context.write_serial, "program:run-core-end");

        let completion_profile = start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
        let completion = match result {
            Ok(()) => CoreModuleRunCompletion::Exit(Ok(ChildExit {
                instance_id,
                exit_code: 0,
                filesystem: Some(store.data().filesystem.snapshot()),
            })),
            Err(error) => {
                if let Some(replacement) = store.data_mut().take_exec_replacement() {
                    CoreModuleRunCompletion::Exec(replacement)
                } else {
                    CoreModuleRunCompletion::Exit(match store.data_mut().take_requested_exit() {
                        Some(code) => Ok(ChildExit {
                            instance_id,
                            exit_code: code,
                            filesystem: Some(store.data().filesystem.snapshot()),
                        }),
                        None => Err(map_program_runtime_error(error)),
                    })
                }
            }
        };
        record_program_kernel_profile_sample(completion_profile, "complete-core");
        store_teardown_profile = start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
        (completion, store.data().threads.is_empty())
    };
    record_program_kernel_profile_sample(store_teardown_profile, "core-store-teardown");

    if recycle_allowed {
        if let (Some(spec), Some(memory)) = (imported_memory_spec, recycle_memory) {
            spawn_scrubbed_recycle(&recycle_spawner, shared_memory_pool.clone(), spec, memory);
        }
    }
    match completion {
        CoreModuleRunCompletion::Exit(result) => result,
        CoreModuleRunCompletion::Exec(replacement) => {
            let replacement_state = replacement_state
                .expect("core module without WASIX imports requested exec replacement");
            Box::pin(run_program_executable(
                replacement_state.exec_context,
                replacement.name,
                replacement.args,
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
            ))
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_program_core_module_with_restore<CpuImpl, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, HostFs>,
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    filesystem: Option<DebugFileSystemSnapshot>,
    signal_state: WasixSignalState,
    spawner: crate::Spawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    compiled: Arc<WasmtimeCompiledCoreModule>,
    restore: CoreModuleRestore,
    engine: &crate::wasmtime_adapter::WasmtimeEngine,
    runtime: &crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    preview1_core_linker: CoreLinker<Preview1ProgramStore<CpuImpl, HostFs>>,
    shared_memory_pool: Arc<Mutex<SharedMemoryPool>>,
    core_module_instance_pre_cache: Arc<
        Mutex<ComponentCache<InstancePre<Preview1ProgramStore<CpuImpl, HostFs>>>>,
    >,
    launched_instance: crate::RegisteredInstance,
    output_mode: OutputMode,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let profile_name = name.clone();
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(name);
    argv.extend(args);
    let run_started_at = monotonic_nanos(&exec_context.cpu);
    let run_cpu = exec_context.cpu.clone();
    let run_timer = exec_context.timer.clone();
    let profile_cpu = exec_context.cpu.clone();
    let profile_runtime_state = exec_context.runtime_state.clone();
    let instance_id = launched_instance.id();
    let wasix_abi = core_module_imports_wasix(&compiled.module);
    let replacement_state = wasix_abi.then(|| CoreModuleReplacementState {
        exec_context: exec_context.clone(),
        instance: launched_instance.clone(),
        output_mode: output_mode.clone(),
        core_linker: preview1_core_linker.clone(),
    });
    let imported_memory = Some(restore.memory.clone());
    let recycle_memory = restore.memory.clone();
    let memory_spec = restore.memory_spec;
    let store_teardown_profile: Option<ProgramKernelProfile<CpuImpl, HostFs>>;
    let (completion, recycle_allowed) = {
        let store_prepare_profile =
            start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
        let mut store = wasmtime::Store::new(
            engine.raw(),
            Preview1ProgramStore::<CpuImpl, HostFs>::new(
                exec_context.cpu,
                exec_context.timer,
                exec_context.spawner.clone(),
                exec_context.runtime_state,
                launched_instance,
                exec_context.parent_instance_id,
                argv,
                env,
                authority,
                output_mode,
                exec_context.read_serial,
                exec_context.write_serial,
                imported_memory.clone(),
                filesystem,
                Some(restore.descriptors),
                signal_state,
                restore.signal_dispositions,
                Some(compiled.clone()),
                wasix_abi,
            ),
        );
        configure_preview1_program_store(&mut store);
        record_program_kernel_profile_sample(store_prepare_profile, "core-store-prepare-rewind");

        let mut linker = preview1_core_linker;
        let imported_memory_define_profile =
            start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
        define_imported_shared_memory(
            &mut linker,
            &store,
            &compiled.module,
            restore.memory.clone(),
        )?;
        record_program_kernel_profile_sample(
            imported_memory_define_profile,
            "core-shared-memory-define-rewind",
        );

        super::emit_program_stage_marker(
            exec_context.write_serial,
            "program:instantiate-core-begin",
        );
        let instantiate_started = profile_cpu.now().ticks();
        let instance = linker.instantiate_async(&mut store, &compiled.module).await;
        record_named_program_kernel_profile(
            &profile_runtime_state,
            &profile_cpu,
            "instantiate-core-rewind",
            &profile_name,
            instantiate_started,
        );
        record_program_kernel_profile(
            &profile_runtime_state,
            &profile_cpu,
            "instantiate-core-rewind",
            instantiate_started,
        );
        let instance = instance.map_err(map_program_runtime_error)?;
        super::emit_program_stage_marker(exec_context.write_serial, "program:instantiate-core-ok");

        let rewind_profile = start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
        wasix_begin_rewind(
            &mut store,
            &instance,
            restore.stack_lower,
            restore.stack_upper,
            restore.stack_pointer,
            restore.memory_stack,
            restore.rewind_stack,
            Some(restore.value),
        )
        .await?;
        record_program_kernel_profile_sample(rewind_profile, "core-begin-rewind");

        let resolve_start_profile =
            start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
        let start = instance
            .get_typed_func::<(), ()>(&mut store, "_start")
            .map_err(|_| ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::InvalidEntryPoint,
            })?;
        record_program_kernel_profile_sample(resolve_start_profile, "resolve-core-start-rewind");

        let run_done = Arc::new(core::sync::atomic::AtomicBool::new(false));
        super::spawn_component_phase_heartbeat(
            &spawner,
            &run_cpu,
            &run_timer,
            &progress,
            exec_context.write_serial,
            "program:run-core",
            run_started_at,
            &run_done,
        );
        super::emit_program_stage_marker(exec_context.write_serial, "program:run-core-begin");
        let run_phase_started = profile_cpu.now().ticks();
        let result = loop {
            let result = start.call_async(&mut store, ()).await;
            if handle_wasix_asyncify_completion(&mut store, &instance).await? {
                continue;
            }
            break result;
        };
        record_named_program_kernel_profile(
            &profile_runtime_state,
            &profile_cpu,
            "run-core-rewind",
            &profile_name,
            run_phase_started,
        );
        record_program_kernel_profile(
            &profile_runtime_state,
            &profile_cpu,
            "run-core-rewind",
            run_phase_started,
        );
        run_done.store(true, core::sync::atomic::Ordering::Release);
        super::emit_program_stage_marker(exec_context.write_serial, "program:run-core-end");

        let completion_profile = start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
        let completion = match result {
            Ok(()) => CoreModuleRunCompletion::Exit(Ok(ChildExit {
                instance_id,
                exit_code: 0,
                filesystem: Some(store.data().filesystem.snapshot()),
            })),
            Err(error) => {
                if let Some(replacement) = store.data_mut().take_exec_replacement() {
                    CoreModuleRunCompletion::Exec(replacement)
                } else {
                    CoreModuleRunCompletion::Exit(match store.data_mut().take_requested_exit() {
                        Some(code) => Ok(ChildExit {
                            instance_id,
                            exit_code: code,
                            filesystem: Some(store.data().filesystem.snapshot()),
                        }),
                        None => Err(map_program_runtime_error(error)),
                    })
                }
            }
        };
        record_program_kernel_profile_sample(completion_profile, "complete-core-rewind");
        store_teardown_profile = start_program_kernel_profile(&profile_runtime_state, &profile_cpu);
        (completion, store.data().threads.is_empty())
    };
    record_program_kernel_profile_sample(store_teardown_profile, "core-store-teardown-rewind");

    if recycle_allowed {
        spawn_scrubbed_recycle(
            &spawner,
            shared_memory_pool.clone(),
            memory_spec,
            recycle_memory,
        );
    }
    match completion {
        CoreModuleRunCompletion::Exit(result) => result,
        CoreModuleRunCompletion::Exec(replacement) => {
            let replacement_state = replacement_state
                .expect("core module without WASIX imports requested exec replacement");
            Box::pin(run_program_executable(
                replacement_state.exec_context,
                replacement.name,
                replacement.args,
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
            ))
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_program_component<CpuImpl, HostFs>(
    exec_context: ProgramExecContext<CpuImpl, HostFs>,
    name: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    authority: ProcessAuthority,
    filesystem: Option<DebugFileSystemSnapshot>,
    _signal_state: WasixSignalState,
    spawner: crate::Spawner<CpuImpl>,
    progress: helios_hal::watchdog::ProgressCounter,
    instance_pre: Arc<ComponentInstancePre<StoreData<CpuImpl, HostFs>>>,
    engine: &crate::wasmtime_adapter::WasmtimeEngine,
    _runtime: &crate::wasmtime_adapter::WasmtimeComponentRuntime<CpuImpl>,
    launched_instance: crate::RegisteredInstance,
    output_mode: OutputMode,
) -> Result<ChildExit, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    use crate::ComponentExecutor;

    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(name);
    argv.extend(args);
    let run_started_at = monotonic_nanos(&exec_context.cpu);
    let run_cpu = exec_context.cpu.clone();
    let run_timer = exec_context.timer.clone();
    let profile_cpu = exec_context.cpu.clone();
    let profile_runtime_state = exec_context.runtime_state.clone();

    // Use the engine that compiled the component — Wasmtime requires
    // component and store to share the same engine instance.
    super::emit_program_stage_marker(exec_context.write_serial, "program:instantiate-begin");
    let instantiate_started = profile_cpu.now().ticks();
    let store_prepare_started = profile_runtime_state
        .profiling_enabled()
        .then(|| profile_cpu.now().ticks());
    let filesystem = match filesystem {
        Some(snapshot) => {
            DebugFileSystem::from_snapshot(exec_context.runtime_state.clone(), snapshot)
        }
        None => DebugFileSystem::new(exec_context.runtime_state.clone()),
    };
    let mut store = crate::wasmtime_adapter::store_with_state(
        engine.raw(),
        StoreData::<CpuImpl, HostFs>::new(
            wasmtime::component::ResourceTable::new(),
            exec_context.cpu,
            exec_context.timer,
            exec_context.spawner.clone(),
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
    let instance = instance_pre.instantiate_async(&mut store).await;
    if let Some(instantiate_instance_started) = instantiate_instance_started {
        record_program_kernel_profile(
            &profile_runtime_state,
            &profile_cpu,
            "instantiate-component-instance",
            instantiate_instance_started,
        );
    }
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
        resolved.map(|run_func| crate::wasmtime_adapter::WasmtimeExecutor { store, run_func })
    });
    record_program_kernel_profile(
        &profile_runtime_state,
        &profile_cpu,
        "instantiate-component",
        instantiate_started,
    );
    let executor = executor.map_err(map_program_runtime_error)?;
    super::emit_program_stage_marker(exec_context.write_serial, "program:instantiate-ok");

    let run_done = Arc::new(core::sync::atomic::AtomicBool::new(false));
    super::spawn_component_phase_heartbeat(
        &spawner,
        &run_cpu,
        &run_timer,
        &progress,
        exec_context.write_serial,
        "program:run",
        run_started_at,
        &run_done,
    );
    super::emit_program_stage_marker(exec_context.write_serial, "program:run-begin");
    let run_phase_started = profile_cpu.now().ticks();
    let result = executor.run().await;
    record_program_kernel_profile(
        &profile_runtime_state,
        &profile_cpu,
        "run-component",
        run_phase_started,
    );
    run_done.store(true, core::sync::atomic::Ordering::Release);
    let result = result.map_err(map_program_runtime_error)?;
    super::emit_program_stage_marker(exec_context.write_serial, "program:run-end");

    Ok(ChildExit {
        instance_id: result.instance_id,
        exit_code: result.exit_code,
        filesystem: None,
    })
}

pub(super) async fn handle_wasix_asyncify_completion<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
) -> Result<bool, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
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
            let unwind_stack_finish = preview1_read_u32(memory, stack_lower)?;
            if unwind_stack_finish < unwind_stack_begin || unwind_stack_finish > stack_pointer {
                return Err(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::StackBoundsInvalid,
                });
            }
            let unwind_len =
                usize::try_from(unwind_stack_finish - unwind_stack_begin).map_err(|_| {
                    ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::StackBoundsInvalid,
                    }
                })?;
            let rewind_stack = preview1_read_memory(memory, unwind_stack_begin, unwind_len)?;
            wasix_call_instance_func0(store, instance, "asyncify_stop_unwind").await?;

            let hash = wasix_next_stack_hash(store);
            let snapshot_bytes = wasix_stack_snapshot_bytes(ret_value, hash);
            if snapshot >= stack_pointer
                && snapshot.saturating_add(WASIX_STACK_SNAPSHOT_SIZE as u32) <= stack_upper
            {
                let offset =
                    usize::try_from(snapshot - stack_pointer).map_err(|_| ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::StackBoundsInvalid,
                    })?;
                let end = offset + snapshot_bytes.len();
                if end <= memory_stack.len() {
                    memory_stack[offset..end].copy_from_slice(&snapshot_bytes);
                }
            } else {
                let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
                })?;
                if preview1_write_memory(memory, snapshot, &snapshot_bytes) != p1::errno::SUCCESS {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
                    });
                }
            }

            store
                .data_mut()
                .asyncify
                .snapshots
                .push(WasixStackSnapshot {
                    hash,
                    memory_stack: memory_stack.clone(),
                    rewind_stack: rewind_stack.clone(),
                    stack_pointer,
                });
            wasix_begin_rewind(
                store,
                instance,
                stack_lower,
                stack_upper,
                stack_pointer,
                memory_stack,
                rewind_stack,
                Some(0),
            )
            .await?;
            Ok(true)
        }
        WasixAsyncifyPhase::Restoring {
            hash,
            value,
            stack_lower,
        } => {
            wasix_call_instance_func0(store, instance, "asyncify_stop_unwind").await?;
            let snapshot = store
                .data()
                .asyncify
                .snapshots
                .iter()
                .find(|snapshot| snapshot.hash == hash)
                .cloned()
                .ok_or(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::StackSnapshotMissing,
                })?;
            let stack_upper = wasix_global_u32_from_instance(store, instance, "__heap_base")?;
            wasix_begin_rewind(
                store,
                instance,
                stack_lower,
                stack_upper,
                snapshot.stack_pointer,
                snapshot.memory_stack,
                snapshot.rewind_stack,
                Some(value),
            )
            .await?;
            Ok(true)
        }
        WasixAsyncifyPhase::Forking {
            ret_pid,
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
            let unwind_stack_finish = preview1_read_u32(memory, stack_lower)?;
            if unwind_stack_finish < unwind_stack_begin || unwind_stack_finish > stack_pointer {
                return Err(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::StackBoundsInvalid,
                });
            }
            let unwind_len =
                usize::try_from(unwind_stack_finish - unwind_stack_begin).map_err(|_| {
                    ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::StackBoundsInvalid,
                    }
                })?;
            let rewind_stack = preview1_read_memory(memory, unwind_stack_begin, unwind_len)?;
            wasix_call_instance_func0(store, instance, "asyncify_stop_unwind").await?;
            let child_pid = spawn_wasix_fork_child(
                store,
                stack_lower,
                stack_upper,
                stack_pointer,
                memory_stack.clone(),
                rewind_stack.clone(),
            )?;
            let snapshot_bytes = child_pid.to_le_bytes();
            if ret_pid >= stack_pointer && ret_pid.saturating_add(4) <= stack_upper {
                let offset =
                    usize::try_from(ret_pid - stack_pointer).map_err(|_| ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::StackBoundsInvalid,
                    })?;
                let end = offset + snapshot_bytes.len();
                if end <= memory_stack.len() {
                    memory_stack[offset..end].copy_from_slice(&snapshot_bytes);
                }
            } else {
                let memory = p1_memory_from_instance(store, instance).ok_or(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
                })?;
                if preview1_write_memory(memory, ret_pid, &snapshot_bytes) != p1::errno::SUCCESS {
                    return Err(ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
                    });
                }
            }
            wasix_begin_rewind(
                store,
                instance,
                stack_lower,
                stack_upper,
                stack_pointer,
                memory_stack,
                rewind_stack,
                Some(u64::from(child_pid)),
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
            let unwind_stack_finish = preview1_read_u32(memory, stack_lower)?;
            if unwind_stack_finish < unwind_stack_begin || unwind_stack_finish > stack_pointer {
                return Err(ProgramExecError {
                    kind: ProgramExecErrorKind::InvalidBinary,
                    detail: ProgramExecErrorDetail::StackBoundsInvalid,
                });
            }
            let unwind_len =
                usize::try_from(unwind_stack_finish - unwind_stack_begin).map_err(|_| {
                    ProgramExecError {
                        kind: ProgramExecErrorKind::InvalidBinary,
                        detail: ProgramExecErrorDetail::StackBoundsInvalid,
                    }
                })?;
            let rewind_stack = preview1_read_memory(memory, unwind_stack_begin, unwind_len)?;
            wasix_call_instance_func0(store, instance, "asyncify_stop_unwind").await?;
            let snapshot = wasix_capture_process_snapshot(
                store,
                stack_lower,
                stack_upper,
                stack_pointer,
                memory_stack.clone(),
                rewind_stack.clone(),
            )?;
            store.data_mut().asyncify.process_snapshots.push(snapshot);
            let snapshot_count = store.data().asyncify.process_snapshots.len();
            if let Some(snapshot) = store.data().asyncify.process_snapshots.last() {
                trace_wasix_process_snapshot(snapshot, snapshot_count);
            }
            store.data_mut().asyncify.process_snapshot_rewinding = true;
            wasix_begin_rewind(
                store,
                instance,
                stack_lower,
                stack_upper,
                stack_pointer,
                memory_stack,
                rewind_stack,
                None,
            )
            .await?;
            Ok(true)
        }
    }
}

pub(super) async fn wasix_begin_rewind<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
    stack_lower: u32,
    stack_upper: u32,
    stack_pointer: u32,
    memory_stack: Vec<u8>,
    rewind_stack: Vec<u8>,
    value: Option<u64>,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
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
    if preview1_write_memory(memory, stack_pointer, &memory_stack) != p1::errno::SUCCESS {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        });
    }
    wasix_set_global_u32_from_instance(store, instance, "__stack_pointer", stack_pointer)?;

    let rewind_stack_begin =
        stack_lower
            .checked_add(WASIX_ASYNCIFY_DATA_SIZE)
            .ok_or(ProgramExecError {
                kind: ProgramExecErrorKind::InvalidBinary,
                detail: ProgramExecErrorDetail::StackBoundsInvalid,
            })?;
    let rewind_len = u32::try_from(rewind_stack.len()).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::StackBoundsInvalid,
    })?;
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
    let status = preview1_write_u32(memory, stack_lower, rewind_stack_end)
        .max(preview1_write_u32(memory, stack_lower + 4, stack_upper))
        .max(preview1_write_memory(
            memory,
            rewind_stack_begin,
            &rewind_stack,
        ));
    if status != p1::errno::SUCCESS {
        return Err(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::GuestMemoryAccessOutOfBounds,
        });
    }
    store.data_mut().asyncify.rewind_value = value;
    wasix_call_instance_func1(store, instance, "asyncify_start_rewind", stack_lower).await?;
    Ok(())
}

pub(super) fn wasix_capture_process_snapshot<CpuImpl, HostFs>(
    store: &wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    stack_lower: u32,
    stack_upper: u32,
    stack_pointer: u32,
    memory_stack: Vec<u8>,
    rewind_stack: Vec<u8>,
) -> Result<WasixProcessSnapshot, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let snapshot_started = store
        .data()
        .runtime_state
        .profiling_enabled()
        .then(|| store.data().cpu.now().ticks());
    let memory = store
        .data()
        .imported_memory
        .as_ref()
        .ok_or(ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::ImportedSharedMemoryContractInvalid,
        })?;
    let memory = memory.data();
    let memory_len = memory.len();
    let memory_pages = memory_len.div_ceil(WASM_PAGE_SIZE);
    let memory_pages = u32::try_from(memory_pages).map_err(|_| ProgramExecError {
        kind: ProgramExecErrorKind::OutOfMemory,
        detail: ProgramExecErrorDetail::ImportedSharedMemoryBudgetExceeded,
    })?;
    let mut memory_bytes = Vec::with_capacity(memory_len);
    unsafe {
        memory_bytes.set_len(memory_len);
        core::ptr::copy_nonoverlapping(
            memory.as_ptr().cast::<u8>(),
            memory_bytes.as_mut_ptr(),
            memory_len,
        );
    }
    if let Some(snapshot_started) = snapshot_started {
        record_program_kernel_profile(
            &store.data().runtime_state,
            &store.data().cpu,
            "asyncify-process-snapshot-memory-copy",
            snapshot_started,
        );
    }
    Ok(WasixProcessSnapshot {
        memory: memory_bytes,
        memory_pages,
        descriptors: store.data().descriptors.clone(),
        filesystem: store.data().filesystem.snapshot(),
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
    })
}

pub(super) fn trace_wasix_process_snapshot(snapshot: &WasixProcessSnapshot, snapshot_count: usize) {
    let cwd = snapshot
        .cwd
        .as_ref()
        .map(|cwd| cwd.guest_name.as_str())
        .unwrap_or("");
    let _filesystem = &snapshot.filesystem;
    tracing::debug!(
        snapshot_count,
        memory_bytes = snapshot.memory.len(),
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
        memory_stack_bytes = snapshot.memory_stack.len(),
        rewind_stack_bytes = snapshot.rewind_stack.len(),
        "captured explicit WASIX process snapshot"
    );
}

pub(super) fn spawn_wasix_fork_child<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    stack_lower: u32,
    stack_upper: u32,
    stack_pointer: u32,
    memory_stack: Vec<u8>,
    rewind_stack: Vec<u8>,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
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
    let data = memory.data();
    let current_bytes = data.len();
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
    let memory_spec = SharedMemorySpec {
        initial_pages: current_pages,
        maximum_pages: PROGRAM_SHARED_MEMORY_MAX_PAGES,
    };
    let fork_memory = service
        .inner
        .shared_memory_pool
        .lock()
        .acquire(service.inner.engine.raw(), memory_spec)?;
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
            data.as_ptr().cast::<u8>(),
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
    let restore = CoreModuleRestore {
        memory: fork_memory,
        memory_spec,
        descriptors: store.data().descriptors.clone(),
        signal_dispositions: store.data().signal_dispositions.clone(),
        stack_lower,
        stack_upper,
        stack_pointer,
        memory_stack,
        rewind_stack,
        value: 0,
    };
    let argv = store.data().arguments.clone();
    let name = argv.first().cloned().ok_or(ProgramExecError {
        kind: ProgramExecErrorKind::InvalidBinary,
        detail: ProgramExecErrorDetail::InvalidEntryPoint,
    })?;
    let args = argv.into_iter().skip(1).collect();
    let mut environment = store.data().environment.clone();
    environment.retain(|(name, _)| name.as_str() != HELIOS_PROCESS_ID_ENV);
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
        name,
        args,
        environment,
        ProgramExecutable::ForkedCoreModule { compiled, restore },
        store.data().authority.clone(),
        Some(store.data().filesystem.snapshot()),
        None,
        Vec::new(),
        output_mode,
        None,
        None,
        None,
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

pub(super) fn wasix_next_stack_hash<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
) -> u128
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let lower = store.data_mut().entropy.insecure_u64();
    let upper = store.data_mut().entropy.insecure_u64();
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

pub(super) async fn wasix_call_instance_func0<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
    name: &str,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let function = instance
        .get_typed_func::<(), ()>(&mut *store, name)
        .map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::UnwindExportInvalid,
        })?;
    function
        .call_async(&mut *store, ())
        .await
        .map_err(map_program_runtime_error)
}

pub(super) async fn wasix_call_instance_func1<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
    name: &str,
    value: u32,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let function = instance
        .get_typed_func::<i32, ()>(&mut *store, name)
        .map_err(|_| ProgramExecError {
            kind: ProgramExecErrorKind::InvalidBinary,
            detail: ProgramExecErrorDetail::UnwindExportInvalid,
        })?;
    function
        .call_async(&mut *store, value as i32)
        .await
        .map_err(map_program_runtime_error)
}

pub(super) fn wasix_global_u32_from_instance<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
    name: &str,
) -> Result<u32, ProgramExecError>
where
    CpuImpl: Cpu + Clone,
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

pub(super) fn wasix_set_global_u32_from_instance<CpuImpl, HostFs>(
    store: &mut wasmtime::Store<Preview1ProgramStore<CpuImpl, HostFs>>,
    instance: &wasmtime::Instance,
    name: &str,
    value: u32,
) -> Result<(), ProgramExecError>
where
    CpuImpl: Cpu + Clone,
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
        WASIX_MODULE => {
            if crate::wasmtime_adapter::wasix::LINKED_IMPORTS.contains(&name) {
                return Ok(());
            }
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

pub(super) fn map_program_runtime_error(error: wasmtime::Error) -> ProgramExecError {
    if error.is::<crate::ProgramOutOfMemory>() {
        tracing::error!(?error, "program runtime reported out of memory");
        return ProgramExecError {
            kind: ProgramExecErrorKind::OutOfMemory,
            detail: ProgramExecErrorDetail::RuntimeFailure,
        };
    }
    if let Some(killed) = error.downcast_ref::<crate::InstanceKilled>() {
        tracing::error!(?error, reason = ?killed.reason, "program instance was killed");
        let kind = match killed.reason {
            crate::KillReason::OutOfMemory => ProgramExecErrorKind::OutOfMemory,
            crate::KillReason::SupervisorRestart => ProgramExecErrorKind::Internal,
        };
        return ProgramExecError {
            kind,
            detail: ProgramExecErrorDetail::RuntimeFailure,
        };
    }

    tracing::error!(?error, "program runtime operation failed");
    ProgramExecError {
        kind: ProgramExecErrorKind::Internal,
        detail: ProgramExecErrorDetail::RuntimeFailure,
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

pub(super) fn trusted_bootfs_payload(bytes: &Bytes) -> Result<Bytes, ProgramExecError> {
    let trusted = cwasm::trust_bootfs_artifact(UntrustedCwasm::new(bytes))
        .map_err(map_artifact_trust_error)?;
    Ok(bytes.slice(..trusted.payload().len()))
}

pub(super) fn trusted_signed_payload(bytes: &Bytes) -> Result<Bytes, ProgramExecError> {
    let trusted = cwasm::verify_signed_artifact(UntrustedCwasm::new(bytes))
        .map_err(map_artifact_trust_error)?;
    Ok(bytes.slice(..trusted.payload().len()))
}
