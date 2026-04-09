extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use helios_hal::cpu::Cpu;
use helios_hal::fs::DirectoryHandle;
use helios_hal::resource::{SerialRights, WasiRights};
use thiserror::Error;
use wasmtime::{CallHook, Config, Engine, Linker, Module, ResourceLimiter, Store};

use crate::{
    BootDirectory, ComputePool, ComputePriority, ComputeSpawnError, EmbeddedProgram,
    InstanceExecutionTransition, RegisteredInstance, build_target_engine_config,
};

/// Immutable compilation settings for the user-mode runtime.
///
/// The caller owns these limits instead of relying on hidden defaults, so the
/// kernel can size JIT resources explicitly for each platform bring-up.
#[derive(Clone, Copy)]
pub struct ProgramRuntimeConfig {
    compute_workers: usize,
    worker_stack_size: usize,
    max_memory_bytes: usize,
    compile_priority: ComputePriority,
}

impl ProgramRuntimeConfig {
    pub const fn new(
        compute_workers: usize,
        worker_stack_size: usize,
        max_memory_bytes: usize,
    ) -> Self {
        Self {
            compute_workers,
            worker_stack_size,
            max_memory_bytes,
            compile_priority: ComputePriority::NORMAL,
        }
    }

    pub const fn with_compile_priority(mut self, compile_priority: ComputePriority) -> Self {
        self.compile_priority = compile_priority;
        self
    }
}

#[derive(Clone)]
pub struct ProgramRuntime {
    engine_config: Config,
    compiler: ComputePool,
    compile_priority: ComputePriority,
    compile_worker_count: usize,
}

#[derive(Clone)]
pub struct ProgramRuntimeDriver {
    compiler: ComputePool,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ProgramRuntimeInitError {
    #[error("compute runtime configuration is invalid")]
    InvalidComputeConfig,
}

#[derive(Debug, Error)]
pub enum ProgramRuntimeError {
    #[error(transparent)]
    Init(ProgramRuntimeInitError),
    #[error("failed to initialize Wasmtime engine: {0}")]
    Wasmtime(wasmtime::Error),
}

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("compute queue rejected the compile job: {0}")]
    QueueSaturated(ComputeSpawnError),
    #[error("module exports neither _start nor main")]
    MissingEntry,
    #[error("unsupported import {module}::{name}")]
    UnsupportedImportModule { module: String, name: String },
    #[error("Wasmtime compilation failed: {0}")]
    Wasmtime(wasmtime::Error),
}

#[derive(Debug, Error)]
pub enum RunError {
    #[error("Wasmtime execution failed: {0}")]
    Wasmtime(wasmtime::Error),
}

/// Blueprint for a user-mode task that has not been compiled yet.
///
/// Helios user mode is pure wasm. The blueprint therefore only needs the wasm
/// bytes plus the initial capability mask that will become the task's root
/// authority.
pub struct Blueprint<'a> {
    source: BlueprintSource<'a>,
    rights: WasiRights,
    boot_directory: Option<DirectoryHandle<BootDirectory>>,
    debug_serial: Option<SerialRights>,
}

enum BlueprintSource<'a> {
    Wasm(&'a [u8]),
    Embedded(&'a EmbeddedProgram),
}

/// Per-task kernel-visible resource namespace.
///
/// The table currently only tracks ownership/lifetime. Host bindings will move
/// concrete kernel objects into this namespace as the user-mode runtime grows.
#[derive(Default)]
pub struct ResourceTable {
    boot_directory: Option<DirectoryHandle<BootDirectory>>,
    debug_serial: Option<SerialRights>,
}

#[derive(Clone, Copy)]
struct EntryPoint {
    name: &'static str,
    returns_exit_code: bool,
}

/// User-mode task, which has been JIT-compiled and is ready to execute.
pub struct Task {
    module: Module,
    rights: WasiRights,
    resources: ResourceTable,
    entry: EntryPoint,
}

pub type ExitCode = u32;

struct TaskState {
    rights: WasiRights,
    resources: ResourceTable,
}

struct TrackedTaskState<CpuImpl: Cpu + Clone> {
    rights: WasiRights,
    resources: ResourceTable,
    cpu: CpuImpl,
    instance: RegisteredInstance,
}

impl ProgramRuntime {
    pub fn new(config: ProgramRuntimeConfig) -> Result<Self, ProgramRuntimeError> {
        let compiler = ComputePool::new(
            config.compute_workers,
            config.worker_stack_size,
            config.max_memory_bytes,
        )
        .map_err(|_| ProgramRuntimeError::Init(ProgramRuntimeInitError::InvalidComputeConfig))?;
        let engine_config = build_engine_config();
        Engine::new(&engine_config).map_err(ProgramRuntimeError::Wasmtime)?;

        Ok(Self {
            engine_config,
            compiler,
            compile_priority: config.compile_priority,
            compile_worker_count: config.compute_workers,
        })
    }

    pub fn compile_worker_count(&self) -> usize {
        self.compile_worker_count
    }

    pub fn driver(&self) -> ProgramRuntimeDriver {
        ProgramRuntimeDriver {
            compiler: self.compiler.clone(),
        }
    }

    pub async fn compile(&self, blueprint: Blueprint<'_>) -> Result<Task, CompileError> {
        let source = blueprint.source;
        let rights = blueprint.rights;
        let boot_directory = blueprint.boot_directory;
        let debug_serial = blueprint.debug_serial;
        let engine_config = self.engine_config.clone();

        match source {
            BlueprintSource::Wasm(wasm) => {
                let wasm = wasm.to_vec();
                self.compiler
                    .spawn(self.compile_priority, move || {
                        let engine = Engine::new(&engine_config).map_err(CompileError::Wasmtime)?;
                        compile_wasm_task(engine, wasm, rights, boot_directory, debug_serial)
                    })
                    .await
                    .map_err(CompileError::QueueSaturated)?
            }
            BlueprintSource::Embedded(program) => {
                let engine = Engine::new(&engine_config).map_err(CompileError::Wasmtime)?;
                compile_embedded_task(engine, program, rights, boot_directory, debug_serial)
            }
        }
    }
}

impl ProgramRuntimeDriver {
    pub fn run_next(&self) -> bool {
        self.compiler.run_next()
    }

    pub async fn wait_for_work(&self) {
        self.compiler.wait_for_work().await;
    }

    pub(crate) fn ready_notify(&self) -> alloc::sync::Arc<crate::sync::Notify> {
        self.compiler.ready_notify()
    }
}

impl<'a> Blueprint<'a> {
    pub const fn new_wasm(wasm: &'a [u8], rights: WasiRights) -> Self {
        Self {
            source: BlueprintSource::Wasm(wasm),
            rights,
            boot_directory: None,
            debug_serial: None,
        }
    }

    pub const fn new_embedded(program: &'a EmbeddedProgram, rights: WasiRights) -> Self {
        Self {
            source: BlueprintSource::Embedded(program),
            rights,
            boot_directory: None,
            debug_serial: None,
        }
    }

    pub fn with_boot_directory(mut self, boot_directory: DirectoryHandle<BootDirectory>) -> Self {
        self.boot_directory = Some(boot_directory);
        self
    }

    pub fn with_debug_serial(mut self, rights: SerialRights) -> Self {
        self.debug_serial = Some(rights);
        self
    }

    /// Launches kernel compute work that JIT-compiles this wasm blueprint.
    pub async fn compile(self, runtime: &ProgramRuntime) -> Result<Task, CompileError> {
        runtime.compile(self).await
    }
}

impl ResourceTable {
    pub const fn new() -> Self {
        Self {
            boot_directory: None,
            debug_serial: None,
        }
    }

    pub fn with_boot_directory(boot_directory: DirectoryHandle<BootDirectory>) -> Self {
        Self {
            boot_directory: Some(boot_directory),
            debug_serial: None,
        }
    }

    pub fn with_debug_serial(mut self, rights: SerialRights) -> Self {
        self.debug_serial = Some(rights);
        self
    }

    pub fn len(&self) -> usize {
        usize::from(self.boot_directory.is_some()) + usize::from(self.debug_serial.is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.boot_directory.is_none() && self.debug_serial.is_none()
    }

    pub fn boot_directory(&self) -> Option<&DirectoryHandle<BootDirectory>> {
        self.boot_directory.as_ref()
    }

    pub fn debug_serial(&self) -> Option<SerialRights> {
        self.debug_serial
    }
}

impl Task {
    pub fn rights(&self) -> WasiRights {
        self.rights
    }

    pub fn resource_table(&self) -> &ResourceTable {
        &self.resources
    }

    pub async fn run(self) -> Result<ExitCode, RunError> {
        let Task {
            module,
            rights,
            resources,
            entry,
        } = self;

        let mut store = Store::new(module.engine(), TaskState { rights, resources });
        let instance = instantiate_task(&module, &mut store).map_err(RunError::Wasmtime)?;
        let output = run_entry(&instance, &mut store, entry);
        let TaskState { rights, resources } = store.into_data();
        let _ = rights;
        let _ = resources;
        output
    }

    pub fn run_tracked<CpuImpl: Cpu + Clone>(
        self,
        cpu: CpuImpl,
        instance: RegisteredInstance,
    ) -> Result<ExitCode, RunError> {
        let Task {
            module,
            rights,
            resources,
            entry,
        } = self;

        let mut store = Store::new(
            module.engine(),
            TrackedTaskState {
                rights,
                resources,
                cpu,
                instance,
            },
        );
        store.limiter(|state| state);
        store.call_hook(|mut caller, hook| {
            caller.data_mut().record_call_hook(hook);
            Ok(())
        });
        let instance = instantiate_tracked_task(&module, &mut store).map_err(RunError::Wasmtime)?;
        if let Some(memory) = instance.get_memory(&mut store, "memory") {
            let memory_bytes = u64::try_from(memory.data_size(&store))
                .expect("wasm memory size exceeds u64 during task start");
            store.data().instance.set_memory_bytes(memory_bytes);
        }
        let output = run_entry(&instance, &mut store, entry);
        let TrackedTaskState {
            rights, resources, ..
        } = store.into_data();
        let _ = rights;
        let _ = resources;
        output
    }
}

fn instantiate_task(
    module: &Module,
    store: &mut Store<TaskState>,
) -> Result<wasmtime::Instance, wasmtime::Error> {
    let linker = Linker::<TaskState>::new(module.engine());
    linker.instantiate(store, module)
}

fn instantiate_tracked_task<CpuImpl: Cpu + Clone>(
    module: &Module,
    store: &mut Store<TrackedTaskState<CpuImpl>>,
) -> Result<wasmtime::Instance, wasmtime::Error> {
    let linker = Linker::<TrackedTaskState<CpuImpl>>::new(module.engine());
    linker.instantiate(store, module)
}

impl<CpuImpl: Cpu + Clone> TrackedTaskState<CpuImpl> {
    fn now_nanos(&self) -> u64 {
        let ticks = self.cpu.now().ticks();
        ticks.saturating_mul(1_000_000_000) / self.cpu.timer_frequency()
    }

    fn record_call_hook(&mut self, hook: CallHook) {
        let transition = match hook {
            CallHook::CallingWasm | CallHook::ReturningFromHost => {
                InstanceExecutionTransition::Resume
            }
            CallHook::ReturningFromWasm | CallHook::CallingHost => {
                InstanceExecutionTransition::Pause
            }
        };
        self.instance.transition(transition, self.now_nanos());
    }
}

impl<CpuImpl: Cpu + Clone> ResourceLimiter for TrackedTaskState<CpuImpl> {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allow = maximum.is_none_or(|maximum| desired <= maximum);
        if allow {
            let memory_bytes =
                u64::try_from(desired).expect("wasm memory size exceeds u64 during growth");
            self.instance.set_memory_bytes(memory_bytes);
        }
        Ok(allow)
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

fn run_entry<T>(
    instance: &wasmtime::Instance,
    store: &mut Store<T>,
    entry: EntryPoint,
) -> Result<ExitCode, RunError> {
    if entry.returns_exit_code {
        let main = instance
            .get_typed_func::<(), i32>(&mut *store, entry.name)
            .map_err(RunError::Wasmtime)?;
        let code = main.call(&mut *store, ()).map_err(RunError::Wasmtime)?;
        return Ok(code as u32);
    }

    let start = instance
        .get_typed_func::<(), ()>(&mut *store, entry.name)
        .map_err(RunError::Wasmtime)?;
    start.call(&mut *store, ()).map_err(RunError::Wasmtime)?;
    Ok(0)
}

fn build_engine_config() -> Config {
    build_target_engine_config(env!("HELIOS_BUILD_TARGET"))
}

fn compile_wasm_task(
    engine: Engine,
    wasm: Vec<u8>,
    rights: WasiRights,
    boot_directory: Option<DirectoryHandle<BootDirectory>>,
    debug_serial: Option<SerialRights>,
) -> Result<Task, CompileError> {
    let module = Module::from_binary(&engine, &wasm).map_err(CompileError::Wasmtime)?;
    finish_task(module, rights, boot_directory, debug_serial)
}

fn compile_embedded_task(
    engine: Engine,
    program: &EmbeddedProgram,
    rights: WasiRights,
    boot_directory: Option<DirectoryHandle<BootDirectory>>,
    debug_serial: Option<SerialRights>,
) -> Result<Task, CompileError> {
    let module = Module::from_binary(&engine, program.bytes()).map_err(CompileError::Wasmtime)?;
    finish_task(module, rights, boot_directory, debug_serial)
}

fn finish_task(
    module: Module,
    rights: WasiRights,
    boot_directory: Option<DirectoryHandle<BootDirectory>>,
    debug_serial: Option<SerialRights>,
) -> Result<Task, CompileError> {
    validate_imports(&module)?;

    let entry = detect_entry(&module)?;
    let mut resources = match boot_directory {
        Some(boot_directory) => ResourceTable::with_boot_directory(boot_directory),
        None => ResourceTable::new(),
    };
    if let Some(debug_serial) = debug_serial {
        resources = resources.with_debug_serial(debug_serial);
    }

    Ok(Task {
        module,
        rights,
        resources,
        entry,
    })
}

fn validate_imports(module: &Module) -> Result<(), CompileError> {
    if let Some(import) = module.imports().next() {
        return Err(CompileError::UnsupportedImportModule {
            module: import.module().into(),
            name: import.name().into(),
        });
    }

    Ok(())
}

fn detect_entry(module: &Module) -> Result<EntryPoint, CompileError> {
    if module.exports().any(|export| export.name() == "_start") {
        return Ok(EntryPoint {
            name: "_start",
            returns_exit_code: false,
        });
    }

    if module.exports().any(|export| export.name() == "main") {
        return Ok(EntryPoint {
            name: "main",
            returns_exit_code: true,
        });
    }

    Err(CompileError::MissingEntry)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use futures_lite::future::block_on;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wasm_encoder::{
        CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection,
        ImportSection, Instruction, MemorySection, MemoryType, Module, TypeSection,
    };

    use super::{Blueprint, CompileError, ProgramRuntimeConfig, ProgramRuntimeError};
    use crate::{ComputePriority, EmbeddedProgram};
    use helios_hal::resource::WasiRights;

    fn test_runtime() -> Result<super::ProgramRuntime, ProgramRuntimeError> {
        super::ProgramRuntime::new(
            ProgramRuntimeConfig::new(2, 64 * 1024, 1024 * 1024)
                .with_compile_priority(ComputePriority::HIGH),
        )
    }

    fn spawn_compile_workers(
        runtime: &super::ProgramRuntime,
        worker_count: usize,
        expected_jobs: usize,
    ) -> Vec<std::thread::JoinHandle<()>> {
        let compiler = runtime.compiler.clone();
        let completed = Arc::new(AtomicUsize::new(0));

        (0..worker_count)
            .map(|_| {
                let compiler = compiler.clone();
                let completed = completed.clone();
                std::thread::spawn(move || {
                    while completed.load(Ordering::Acquire) < expected_jobs {
                        if compiler.run_next() {
                            completed.fetch_add(1, Ordering::AcqRel);
                            continue;
                        }

                        std::thread::yield_now();
                    }
                })
            })
            .collect()
    }

    fn join_workers(workers: Vec<std::thread::JoinHandle<()>>) {
        for worker in workers {
            worker
                .join()
                .unwrap_or_else(|_| panic!("compile worker panicked unexpectedly"));
        }
    }

    fn start_module() -> Vec<u8> {
        let mut types = TypeSection::new();
        types.ty().function([], []);

        let mut functions = FunctionSection::new();
        functions.function(0);

        let mut exports = ExportSection::new();
        exports.export("_start", ExportKind::Func, 0);

        let mut code = CodeSection::new();
        let mut function = Function::new([]);
        function.instruction(&Instruction::End);
        code.function(&function);

        let mut module = Module::new();
        module.section(&types);
        module.section(&functions);
        module.section(&exports);
        module.section(&code);
        module.finish()
    }

    fn memory_main_module() -> Vec<u8> {
        let mut types = TypeSection::new();
        types.ty().function([], [wasm_encoder::ValType::I32]);

        let mut functions = FunctionSection::new();
        functions.function(0);

        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: 1,
            maximum: Some(2),
            memory64: false,
            shared: false,
            page_size_log2: None,
        });

        let mut exports = ExportSection::new();
        exports.export("main", ExportKind::Func, 0);
        exports.export("memory", ExportKind::Memory, 0);

        let mut code = CodeSection::new();
        let mut function = Function::new([]);
        function.instruction(&Instruction::I32Const(0));
        function.instruction(&Instruction::End);
        code.function(&function);

        let mut module = Module::new();
        module.section(&types);
        module.section(&functions);
        module.section(&memories);
        module.section(&exports);
        module.section(&code);
        module.finish()
    }

    fn imported_module() -> Vec<u8> {
        let mut types = TypeSection::new();
        types.ty().function([], []);

        let mut imports = ImportSection::new();
        imports.import("env", "noop", EntityType::Function(0));

        let mut functions = FunctionSection::new();
        functions.function(0);

        let mut exports = ExportSection::new();
        exports.export("_start", ExportKind::Func, 1);

        let mut code = CodeSection::new();
        let mut function = Function::new([]);
        function.instruction(&Instruction::End);
        code.function(&function);

        let mut module = Module::new();
        module.section(&types);
        module.section(&imports);
        module.section(&functions);
        module.section(&exports);
        module.section(&code);
        module.finish()
    }

    fn embedded_start_program() -> EmbeddedProgram {
        let bytes = Box::leak(start_module().into_boxed_slice());
        EmbeddedProgram::new("program-start-test", bytes)
    }

    #[test]
    fn compiles_and_runs_start_module() {
        let runtime =
            test_runtime().unwrap_or_else(|error| panic!("program runtime init failed: {error:?}"));
        let workers = spawn_compile_workers(&runtime, 1, 1);
        let wasm = start_module();

        let task = block_on(Blueprint::new_wasm(&wasm, WasiRights::empty()).compile(&runtime))
            .expect("pure wasm module should compile");
        let exit_code = block_on(task.run()).expect("compiled task should run");
        join_workers(workers);
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn rejects_imported_modules() {
        let runtime =
            test_runtime().unwrap_or_else(|error| panic!("program runtime init failed: {error:?}"));
        let workers = spawn_compile_workers(&runtime, 1, 1);
        let wasm = imported_module();

        let error =
            match block_on(Blueprint::new_wasm(&wasm, WasiRights::empty()).compile(&runtime)) {
                Ok(_) => panic!("imported module must be rejected"),
                Err(error) => error,
            };
        join_workers(workers);
        assert!(matches!(
            error,
            CompileError::UnsupportedImportModule { .. }
        ));
    }

    #[test]
    fn compiles_memory_module() {
        let runtime =
            test_runtime().unwrap_or_else(|error| panic!("program runtime init failed: {error:?}"));
        let workers = spawn_compile_workers(&runtime, 1, 1);
        let wasm = memory_main_module();

        let task = block_on(Blueprint::new_wasm(&wasm, WasiRights::empty()).compile(&runtime))
            .expect("memory module should compile");
        let exit_code = block_on(task.run()).expect("memory module should run");
        join_workers(workers);
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn compiles_multiple_modules_in_parallel() {
        let runtime = super::ProgramRuntime::new(
            ProgramRuntimeConfig::new(2, 64 * 1024, 1024 * 1024)
                .with_compile_priority(ComputePriority::HIGH),
        )
        .unwrap_or_else(|error| panic!("program runtime init failed: {error:?}"));
        let workers = spawn_compile_workers(&runtime, 2, 2);
        let first = start_module();
        let second = memory_main_module();

        let (task_a, task_b) = block_on(async {
            futures_lite::future::zip(
                Blueprint::new_wasm(&first, WasiRights::empty()).compile(&runtime),
                Blueprint::new_wasm(&second, WasiRights::empty()).compile(&runtime),
            )
            .await
        });

        let task_a = task_a.expect("first module should compile");
        let task_b = task_b.expect("second module should compile");

        assert_eq!(block_on(task_a.run()).expect("first module should run"), 0);
        assert_eq!(block_on(task_b.run()).expect("second module should run"), 0);
        join_workers(workers);
    }

    #[test]
    fn loads_embedded_jit_module() {
        let runtime =
            test_runtime().unwrap_or_else(|error| panic!("program runtime init failed: {error:?}"));
        let program = embedded_start_program();
        let task =
            block_on(Blueprint::new_embedded(&program, WasiRights::empty()).compile(&runtime))
                .expect("embedded module should load");

        let exit_code = block_on(task.run()).expect("embedded module should run");
        assert_eq!(exit_code, 0);
    }
}
