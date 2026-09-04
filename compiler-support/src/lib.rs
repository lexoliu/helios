use helios_artifact::{
    CWASM_MEMORY_GUARD_SIZE, CWASM_MEMORY_RESERVATION, cwasm_target_supports_wasm_simd,
};
use std::env;
use std::num::NonZeroUsize;
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
    let worker_count = compiler_worker_count();
    let engine =
        Engine::new(&build_engine_config(target, hint, worker_count)?).map_err(|source| {
            CompileError::Engine {
                target: target.to_owned(),
                source,
            }
        })?;
    if worker_count <= 1 {
        return precompile_artifact_with_engine(bytes, &engine);
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count)
        .build()
        .map_err(CompileError::WorkerPool)?;
    pool.install(|| precompile_artifact_with_engine(bytes, &engine))
}

fn precompile_artifact_with_engine(bytes: &[u8], engine: &Engine) -> Result<PrecompiledArtifact> {
    let encoding = wasm_encoding(bytes)?;
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
        .map(NonZeroUsize::get)
        .or_else(|| {
            std::thread::available_parallelism()
                .ok()
                .map(NonZeroUsize::get)
        })
        .unwrap_or(1)
}

fn build_engine_config(target: &str, hint: AotCompileHint, worker_count: usize) -> Result<Config> {
    let mut config = Config::new();
    config
        .target(target)
        .map_err(|source| CompileError::Target {
            target: target.to_owned(),
            source,
        })?;
    configure_target_isa_flags(&mut config, target);
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

fn configure_target_isa_flags(config: &mut Config, target: &str) {
    for flag in helios_artifact::cwasm_target_cranelift_flags(target) {
        unsafe {
            config.cranelift_flag_enable(flag);
        }
    }
}
