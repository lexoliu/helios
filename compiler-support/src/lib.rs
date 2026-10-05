use helios_artifact::{
    CWASM_MEMORY_GUARD_SIZE, CWASM_MEMORY_RESERVATION, cwasm_target_cranelift_flags,
    cwasm_target_supports_wasm_simd,
};
use std::cell::Cell;
use std::env;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use thiserror::Error;
use wasmparser::{Encoding, Parser, Payload};
use wasmtime::{Config, Engine, OptLevel, Strategy};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AotCompileHint {
    Fast,
    Balanced,
    Performance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrecompiledArtifactKind {
    CoreModule,
    Component,
}

pub struct PrecompiledArtifact {
    pub bytes: Vec<u8>,
    pub kind: PrecompiledArtifactKind,
}

#[derive(Clone, Debug)]
pub struct WorkerStat {
    pub index: usize,
    pub translate_count: u64,
    pub translate_us: u64,
    pub compile_count: u64,
    pub compile_us: u64,
    pub lifetime_us: u64,
}

#[derive(Clone, Debug)]
pub struct CompileProfile {
    pub engine_us: u64,
    pub pool_us: u64,
    pub compile_us: u64,
    pub worker_count: usize,
    pub workers: Vec<WorkerStat>,
}

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("failed to create Wasmtime engine for target {target}: {source}")]
    Engine {
        target: String,
        source: wasmtime::Error,
    },
    #[error("Wasmtime rejected target {target}: {source}")]
    Target {
        target: String,
        source: wasmtime::Error,
    },
    #[error("failed to precompile core module: {0}")]
    CoreModule(wasmtime::Error),
    #[error("failed to precompile component: {0}")]
    Component(wasmtime::Error),
    #[error("failed to build compiler worker pool: {0}")]
    WorkerPool(rayon::ThreadPoolBuildError),
    #[error("failed to parse wasm artifact header: {0}")]
    WasmHeader(wasmparser::BinaryReaderError),
    #[error("wasm artifact did not contain a module or component header")]
    MissingWasmHeader,
}

pub type Result<T> = core::result::Result<T, CompileError>;

pub fn precompile_artifact(
    bytes: &[u8],
    target: &str,
    hint: AotCompileHint,
) -> Result<PrecompiledArtifact> {
    Ok(precompile_artifact_inner(bytes, target, hint, false)?.0)
}

pub fn precompile_artifact_profiled(
    bytes: &[u8],
    target: &str,
    hint: AotCompileHint,
) -> Result<(PrecompiledArtifact, CompileProfile)> {
    precompile_artifact_inner(bytes, target, hint, true)
}

fn precompile_artifact_inner(
    bytes: &[u8],
    target: &str,
    hint: AotCompileHint,
    profile: bool,
) -> Result<(PrecompiledArtifact, CompileProfile)> {
    let worker_count = compiler_worker_count();
    let engine_started = Instant::now();
    let engine =
        Engine::new(&build_engine_config(target, hint, worker_count)?).map_err(|source| {
            CompileError::Engine {
                target: target.to_owned(),
                source,
            }
        })?;
    let encoding = wasm_encoding(bytes)?;
    let engine_us = elapsed_us(engine_started);
    if worker_count <= 1 {
        let compile_started = Instant::now();
        let artifact = precompile_artifact_with_engine(bytes, encoding, &engine)?;
        return Ok((
            artifact,
            CompileProfile {
                engine_us,
                pool_us: 0,
                compile_us: elapsed_us(compile_started),
                worker_count,
                workers: Vec::new(),
            },
        ));
    }

    let worker_stats = profile.then(|| Arc::new(Mutex::new(Vec::with_capacity(worker_count))));
    let pool_started = Instant::now();
    let mut builder = rayon::ThreadPoolBuilder::new().num_threads(worker_count);
    if let Some(worker_stats) = worker_stats.clone() {
        builder = builder
            .start_handler(|_| {
                WORKER_COUNTERS.with(|counters| counters.set(WorkerCounters::default()));
                WORKER_LIFETIME_START.with(|started| started.set(Some(Instant::now())));
                cranelift_codegen::timing::set_thread_profiler(Box::new(WorkerProfiler));
            })
            .exit_handler(move |index| {
                let started = WORKER_LIFETIME_START.with(|started| {
                    started
                        .take()
                        .expect("rayon worker exited without a profiling start")
                });
                let counters = WORKER_COUNTERS.with(Cell::take);
                worker_stats
                    .lock()
                    .expect("compiler worker profile lock was poisoned")
                    .push(WorkerStat {
                        index,
                        translate_count: counters.translate_count,
                        translate_us: counters.translate_ns / 1_000,
                        compile_count: counters.compile_count,
                        compile_us: counters.compile_ns / 1_000,
                        lifetime_us: elapsed_us(started),
                    });
            });
    }
    let pool = builder.build().map_err(CompileError::WorkerPool)?;
    let pool_us = elapsed_us(pool_started);
    let compile_started = Instant::now();
    let artifact = pool.install(|| precompile_artifact_with_engine(bytes, encoding, &engine))?;
    let compile_us = elapsed_us(compile_started);
    drop(pool);
    let workers = worker_stats
        .map(|worker_stats| {
            while worker_stats
                .lock()
                .expect("compiler worker profile lock was poisoned")
                .len()
                < worker_count
            {
                std::thread::yield_now();
            }
            worker_stats
                .lock()
                .expect("compiler worker profile lock was poisoned")
                .clone()
        })
        .unwrap_or_default();
    Ok((
        artifact,
        CompileProfile {
            engine_us,
            pool_us,
            compile_us,
            worker_count,
            workers,
        },
    ))
}

fn precompile_artifact_with_engine(
    bytes: &[u8],
    encoding: Encoding,
    engine: &Engine,
) -> Result<PrecompiledArtifact> {
    match encoding {
        Encoding::Module => {
            let bytes = engine
                .precompile_module(bytes)
                .map_err(CompileError::CoreModule)?;
            Ok(PrecompiledArtifact {
                bytes,
                kind: PrecompiledArtifactKind::CoreModule,
            })
        }
        Encoding::Component => {
            let bytes = engine
                .precompile_component(bytes)
                .map_err(CompileError::Component)?;
            Ok(PrecompiledArtifact {
                bytes,
                kind: PrecompiledArtifactKind::Component,
            })
        }
    }
}

#[derive(Clone, Copy, Default)]
struct WorkerCounters {
    translate_count: u64,
    translate_ns: u64,
    compile_count: u64,
    compile_ns: u64,
}

thread_local! {
    static WORKER_COUNTERS: Cell<WorkerCounters> = const { Cell::new(WorkerCounters {
        translate_count: 0,
        translate_ns: 0,
        compile_count: 0,
        compile_ns: 0,
    }) };
    static WORKER_LIFETIME_START: Cell<Option<Instant>> = const { Cell::new(None) };
}

struct WorkerProfiler;

impl cranelift_codegen::timing::Profiler for WorkerProfiler {
    fn start_pass(&self, pass: cranelift_codegen::timing::Pass) -> Box<dyn std::any::Any> {
        let started = Instant::now();
        let inner = cranelift_codegen::timing::DefaultProfiler.start_pass(pass);
        Box::new(WorkerTimingToken {
            _inner: inner,
            pass,
            started,
        })
    }
}

struct WorkerTimingToken {
    _inner: Box<dyn std::any::Any>,
    pass: cranelift_codegen::timing::Pass,
    started: Instant,
}

impl Drop for WorkerTimingToken {
    fn drop(&mut self) {
        let elapsed = elapsed_ns(self.started);
        WORKER_COUNTERS.with(|cell| {
            let mut counters = cell.get();
            match self.pass {
                cranelift_codegen::timing::Pass::wasm_translate_function => {
                    counters.translate_count += 1;
                    counters.translate_ns += elapsed;
                }
                cranelift_codegen::timing::Pass::compile => {
                    counters.compile_count += 1;
                    counters.compile_ns += elapsed;
                }
                _ => {}
            }
            cell.set(counters);
        });
    }
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

pub fn wasm_encoding(bytes: &[u8]) -> Result<Encoding> {
    for payload in Parser::new(0).parse_all(bytes) {
        if let Payload::Version { encoding, .. } = payload.map_err(CompileError::WasmHeader)? {
            return Ok(encoding);
        }
    }
    Err(CompileError::MissingWasmHeader)
}

fn compiler_worker_count() -> usize {
    env::var("RAYON_NUM_THREADS")
        .ok()
        .and_then(|value| value.parse::<NonZeroUsize>().ok())
        .or_else(|| std::thread::available_parallelism().ok())
        .map_or(1, NonZeroUsize::get)
}

fn build_engine_config(target: &str, hint: AotCompileHint, worker_count: usize) -> Result<Config> {
    let mut config = Config::new();
    config
        .target(target)
        .map_err(|source| CompileError::Target {
            target: target.to_owned(),
            source,
        })?;
    for flag in cwasm_target_cranelift_flags(target) {
        unsafe {
            config.cranelift_flag_enable(flag);
        }
    }
    match hint {
        AotCompileHint::Fast => {
            config.strategy(Strategy::Winch);
        }
        AotCompileHint::Balanced => {
            config.strategy(Strategy::Cranelift);
            config.cranelift_opt_level(OptLevel::None);
        }
        AotCompileHint::Performance => {
            config.strategy(Strategy::Cranelift);
            config.cranelift_opt_level(OptLevel::Speed);
        }
    }
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_threading(true);
    let wasm_simd = cwasm_target_supports_wasm_simd(target);
    config.wasm_simd(wasm_simd);
    config.wasm_relaxed_simd(wasm_simd);
    config.relaxed_simd_deterministic(false);
    // 128-bit integer arithmetic is supported on every backend, including
    // riscv64gc where i128 lowers to register pairs and split i64 stores.
    config.wasm_wide_arithmetic(true);
    config.wasm_multi_memory(true);
    config.wasm_memory64(true);
    config.wasm_tail_call(true);
    config.wasm_threads(true);
    config.shared_memory(true);
    config.gc_support(true);
    config.wasm_reference_types(true);
    config.wasm_function_references(true);
    // The only profile channel Cranelift has. With this off the
    // `metadata.code.branch_hint` section a module carries is skipped and
    // the unlikely successor of a hinted branch is laid out inline like
    // every other; with it on, `code_translator.rs` marks that successor
    // cold and the block-order pass moves it out of line. A module without
    // the section is unaffected, so it is on for every compile.
    config.wasm_branch_hinting(true);
    config.concurrency_support(true);
    config.parallel_compilation(worker_count > 1);
    config.epoch_interruption(true);
    // Wasmtime validates `max_wasm_stack <= async_stack_size`; keep the async
    // fiber stack comfortably above the wasm stack limit with host headroom.
    config.max_wasm_stack(8 * 1024 * 1024);
    config.async_stack_size(9 * 1024 * 1024);
    // Every helios target runs the lazy-commit memory profile, and the
    // reservation and guard sizes compiled into a cwasm artifact are the ones
    // the kernel engine configures for it. Compiling against a different
    // profile would emit bounds checks (or elide checks the runtime cannot
    // back with a guard region), so both sides read the same constants.
    config.memory_init_cow(true);
    config.memory_may_move(false);
    config.memory_reservation(CWASM_MEMORY_RESERVATION);
    config.memory_guard_size(CWASM_MEMORY_GUARD_SIZE);
    Ok(config)
}
