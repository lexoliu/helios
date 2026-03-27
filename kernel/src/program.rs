extern crate alloc;

use alloc::vec::Vec;

use helios_hal::resource::WasiRights;
use wasmtime::{Config, Engine, Instance, Module, Store};

use crate::{ComputePool, ComputePriority, ComputeSpawnError};

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
    engine: Engine,
    compiler: ComputePool,
    compile_priority: ComputePriority,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramRuntimeInitError {
    InvalidComputeConfig,
}

#[derive(Debug)]
pub enum ProgramRuntimeError {
    Init(ProgramRuntimeInitError),
    Wasmtime(wasmtime::Error),
}

#[derive(Debug)]
pub enum CompileError {
    QueueSaturated(ComputeSpawnError),
    ImportedModule { import_count: usize },
    MissingEntry,
    Wasmtime(wasmtime::Error),
}

#[derive(Debug)]
pub enum RunError {
    Wasmtime(wasmtime::Error),
}

/// Blueprint for a user-mode task that has not been compiled yet.
///
/// Helios user mode is pure wasm. The blueprint therefore only needs the wasm
/// bytes plus the initial capability mask that will become the task's root
/// authority.
pub struct Blueprint<'a> {
    wasm: &'a [u8],
    rights: WasiRights,
}

/// Per-task kernel-visible resource namespace.
///
/// The initial pure-wasm runtime has no host imports, so the table starts empty
/// and only tracks ownership/lifetime. Host bindings will extend this type
/// later without changing the compiled task representation.
#[derive(Default)]
pub struct ResourceTable {
    slots: Vec<()>,
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

impl ProgramRuntime {
    pub fn new(config: ProgramRuntimeConfig) -> Result<Self, ProgramRuntimeError> {
        let compiler = ComputePool::new(crate::compute_pool::ComputePoolConfig {
            worker_count: config.compute_workers,
            worker_stack_size: config.worker_stack_size,
            max_memory_bytes: config.max_memory_bytes,
        })
        .map_err(|_| ProgramRuntimeError::Init(ProgramRuntimeInitError::InvalidComputeConfig))?;

        Ok(Self {
            engine: build_engine().map_err(ProgramRuntimeError::Wasmtime)?,
            compiler,
            compile_priority: config.compile_priority,
        })
    }

    pub async fn compile(&self, blueprint: Blueprint<'_>) -> Result<Task, CompileError> {
        let wasm = blueprint.wasm.to_vec();
        let rights = blueprint.rights;
        let engine = self.engine.clone();

        self.compiler
            .spawn(self.compile_priority, move || {
                compile_task(engine, wasm, rights)
            })
            .await
            .map_err(CompileError::QueueSaturated)?
    }
}

impl<'a> Blueprint<'a> {
    pub const fn new_wasm(wasm: &'a [u8], rights: WasiRights) -> Self {
        Self { wasm, rights }
    }

    /// Launches kernel compute work that JIT-compiles this wasm blueprint.
    pub async fn compile(self, runtime: &ProgramRuntime) -> Result<Task, CompileError> {
        runtime.compile(self).await
    }
}

impl ResourceTable {
    pub const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
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
        let mut store = Store::new(
            self.module.engine(),
            TaskState {
                rights: self.rights,
                resources: self.resources,
            },
        );
        let instance = Instance::new(&mut store, &self.module, &[]).map_err(RunError::Wasmtime)?;

        if self.entry.returns_exit_code {
            let main = instance
                .get_typed_func::<(), i32>(&mut store, self.entry.name)
                .map_err(RunError::Wasmtime)?;
            let code = main.call(&mut store, ()).map_err(RunError::Wasmtime)?;
            return Ok(code as u32);
        }

        let start = instance
            .get_typed_func::<(), ()>(&mut store, self.entry.name)
            .map_err(RunError::Wasmtime)?;
        start.call(&mut store, ()).map_err(RunError::Wasmtime)?;
        let TaskState { rights, resources } = store.into_data();
        let _ = rights;
        let _ = resources;
        Ok(0)
    }
}

fn build_engine() -> Result<Engine, wasmtime::Error> {
    let mut config = Config::new();
    config.target(env!("HELIOS_BUILD_TARGET"))?;
    Engine::new(&config)
}

fn compile_task(engine: Engine, wasm: Vec<u8>, rights: WasiRights) -> Result<Task, CompileError> {
    let module = Module::from_binary(&engine, &wasm).map_err(CompileError::Wasmtime)?;
    let import_count = module.imports().len();
    if import_count != 0 {
        return Err(CompileError::ImportedModule { import_count });
    }

    let entry = detect_entry(&module)?;

    Ok(Task {
        module,
        rights,
        resources: ResourceTable::new(),
        entry,
    })
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

    use alloc::vec::Vec;
    use futures_lite::future::block_on;

    use super::{
        Blueprint, CompileError, ProgramRuntime, ProgramRuntimeConfig, ProgramRuntimeError,
    };
    use crate::ComputePriority;
    use helios_hal::resource::WasiRights;

    fn test_runtime() -> Result<ProgramRuntime, ProgramRuntimeError> {
        ProgramRuntime::new(
            ProgramRuntimeConfig::new(1, 64 * 1024, 512 * 1024)
                .with_compile_priority(ComputePriority::HIGH),
        )
    }

    fn parse_wat(source: &str) -> Vec<u8> {
        wat::parse_str(source).expect("test module must be valid wat")
    }

    #[test]
    fn compiles_and_runs_start_module() {
        let runtime = match test_runtime() {
            Ok(runtime) => runtime,
            Err(ProgramRuntimeError::Wasmtime(_)) => return,
            Err(error) => panic!("program runtime initialization failed: {error:?}"),
        };
        let compiler = runtime.compiler.clone();
        let worker = std::thread::spawn(move || {
            while !compiler.run_next() {
                std::thread::yield_now();
            }
        });
        let wasm = parse_wat(include_str!("program_start_test.wat"));

        let task = block_on(Blueprint::new_wasm(&wasm, WasiRights::empty()).compile(&runtime))
            .expect("pure wasm module should compile");
        let exit_code = block_on(task.run()).expect("compiled task should run");

        worker.join().expect("compile worker should complete");
        assert_eq!(exit_code, 0);
    }

    #[test]
    fn rejects_imported_modules() {
        let runtime = match test_runtime() {
            Ok(runtime) => runtime,
            Err(ProgramRuntimeError::Wasmtime(_)) => return,
            Err(error) => panic!("program runtime initialization failed: {error:?}"),
        };
        let compiler = runtime.compiler.clone();
        let worker = std::thread::spawn(move || {
            while !compiler.run_next() {
                std::thread::yield_now();
            }
        });
        let wasm = parse_wat(include_str!("program_import_test.wat"));

        let error =
            match block_on(Blueprint::new_wasm(&wasm, WasiRights::empty()).compile(&runtime)) {
                Ok(_) => panic!("imported module must be rejected"),
                Err(error) => error,
            };

        worker.join().expect("compile worker should complete");
        assert!(matches!(
            error,
            CompileError::ImportedModule { import_count: 1 }
        ));
    }
}
