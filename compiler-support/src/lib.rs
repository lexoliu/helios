use helios_artifact::{
    CWASM_MEMORY_GUARD_SIZE, CWASM_MEMORY_RESERVATION, CwasmCodeProtection, CwasmTarget,
    cwasm_target_code_protection, cwasm_target_guest_stack_limit_offset,
    cwasm_target_supports_wasm_simd,
};
use std::cell::{Cell, RefCell};
use std::env;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use thiserror::Error;
use wasmparser::{Encoding, Parser, Payload};
use wasmtime::{Config, Engine, ModuleVersionStrategy, OptLevel, StackLimitLocation, Strategy};

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
    /// Start of this worker's first translate or compile pass, relative to
    /// the start of the compile.
    pub first_pass_us: i64,
    /// End of this worker's last translate or compile pass, relative to the
    /// start of the compile.
    pub last_pass_us: i64,
    /// Time between this worker's consecutive translate/compile passes.
    pub between_passes_us: u64,
    /// The longest single stretch between two consecutive passes.
    pub longest_between_us: u64,
    /// When that stretch began, relative to the compile start.
    pub longest_between_start_us: i64,
}

/// One function's `compile` pass, timed on the worker that ran it.
#[derive(Clone, Debug)]
pub struct FunctionSpan {
    pub worker: usize,
    pub start_us: i64,
    pub duration_us: u64,
}

#[derive(Clone, Debug)]
pub struct CompileProfile {
    pub engine_us: u64,
    pub pool_us: u64,
    pub compile_us: u64,
    pub worker_count: usize,
    pub workers: Vec<WorkerStat>,
    /// The longest `compile` passes, longest first.
    pub longest: Vec<FunctionSpan>,
}

const PROFILE_LONGEST_FUNCTIONS: usize = 8;

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
    #[error("Wasmtime rejected artifact version {version:?}: {source}")]
    ModuleVersion {
        version: String,
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
    target: &CwasmTarget<'_>,
    hint: AotCompileHint,
) -> Result<PrecompiledArtifact> {
    Ok(precompile_artifact_inner(bytes, target, hint, false)?.0)
}

pub fn precompile_artifact_profiled(
    bytes: &[u8],
    target: &CwasmTarget<'_>,
    hint: AotCompileHint,
) -> Result<(PrecompiledArtifact, CompileProfile)> {
    precompile_artifact_inner(bytes, target, hint, true)
}

fn precompile_artifact_inner(
    bytes: &[u8],
    target: &CwasmTarget<'_>,
    hint: AotCompileHint,
    profile: bool,
) -> Result<(PrecompiledArtifact, CompileProfile)> {
    let worker_count = compiler_worker_count();
    let engine_started = Instant::now();
    let engine =
        Engine::new(&build_engine_config(target, hint, worker_count)?).map_err(|source| {
            CompileError::Engine {
                target: target.triple().to_owned(),
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
                longest: Vec::new(),
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
                WORKER_SPANS.with(|spans| spans.borrow_mut().clear());
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
                let spans = WORKER_SPANS.with(RefCell::take);
                worker_stats
                    .lock()
                    .expect("compiler worker profile lock was poisoned")
                    .push(RawWorkerStat {
                        index,
                        counters,
                        lifetime_us: elapsed_us(started),
                        spans,
                    });
            });
    }
    let pool = builder.build().map_err(CompileError::WorkerPool)?;
    let pool_us = elapsed_us(pool_started);
    let compile_started = Instant::now();
    let artifact = pool.install(|| precompile_artifact_with_engine(bytes, encoding, &engine))?;
    let compile_us = elapsed_us(compile_started);
    drop(pool);
    let raw_workers = worker_stats
        .map(|worker_stats| {
            while worker_stats
                .lock()
                .expect("compiler worker profile lock was poisoned")
                .len()
                < worker_count
            {
                std::thread::yield_now();
            }
            core::mem::take(
                &mut *worker_stats
                    .lock()
                    .expect("compiler worker profile lock was poisoned"),
            )
        })
        .unwrap_or_default();
    let (workers, longest) = summarize_workers(compile_started, raw_workers);
    Ok((
        artifact,
        CompileProfile {
            engine_us,
            pool_us,
            compile_us,
            worker_count,
            workers,
            longest,
        },
    ))
}

fn summarize_workers(
    compile_started: Instant,
    raw_workers: Vec<RawWorkerStat>,
) -> (Vec<WorkerStat>, Vec<FunctionSpan>) {
    let offset_us = |at: Instant| match at.checked_duration_since(compile_started) {
        Some(after) => i64::try_from(after.as_micros()).unwrap_or(i64::MAX),
        None => -i64::try_from((compile_started - at).as_micros()).unwrap_or(i64::MAX),
    };
    let mut longest = Vec::new();
    let workers = raw_workers
        .into_iter()
        .map(|raw| {
            let mut ordered: Vec<&PassSpan> = raw.spans.iter().collect();
            ordered.sort_by_key(|span| span.started);
            let gaps = ordered.windows(2).map(|pair| {
                let gap = pair[1].started.saturating_duration_since(pair[0].finished);
                (
                    pair[0].finished,
                    u64::try_from(gap.as_micros()).unwrap_or(u64::MAX),
                )
            });
            let mut between_passes_us = 0u64;
            let mut longest_between: Option<(Instant, u64)> = None;
            for (began, gap) in gaps {
                between_passes_us = between_passes_us.saturating_add(gap);
                if longest_between.is_none_or(|(_, longest)| gap > longest) {
                    longest_between = Some((began, gap));
                }
            }
            let (longest_between_start_us, longest_between_us) =
                longest_between.map_or((0, 0), |(began, gap)| (offset_us(began), gap));
            let first_pass_us = raw
                .spans
                .iter()
                .map(|span| span.started)
                .min()
                .map_or(0, offset_us);
            let last_pass_us = raw
                .spans
                .iter()
                .map(|span| span.finished)
                .max()
                .map_or(0, offset_us);
            longest.extend(raw.spans.iter().filter(|span| !span.translate).map(|span| {
                FunctionSpan {
                    worker: raw.index,
                    start_us: offset_us(span.started),
                    duration_us: u64::try_from((span.finished - span.started).as_micros())
                        .unwrap_or(u64::MAX),
                }
            }));
            WorkerStat {
                index: raw.index,
                translate_count: raw.counters.translate_count,
                translate_us: raw.counters.translate_ns / 1_000,
                compile_count: raw.counters.compile_count,
                compile_us: raw.counters.compile_ns / 1_000,
                lifetime_us: raw.lifetime_us,
                first_pass_us,
                last_pass_us,
                between_passes_us,
                longest_between_us,
                longest_between_start_us,
            }
        })
        .collect();
    longest.sort_by_key(|span| core::cmp::Reverse(span.duration_us));
    longest.truncate(PROFILE_LONGEST_FUNCTIONS);
    (workers, longest)
}

struct RawWorkerStat {
    index: usize,
    counters: WorkerCounters,
    lifetime_us: u64,
    spans: Vec<PassSpan>,
}

struct PassSpan {
    started: Instant,
    finished: Instant,
    translate: bool,
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
    static WORKER_SPANS: RefCell<Vec<PassSpan>> = const { RefCell::new(Vec::new()) };
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
        let finished = Instant::now();
        let elapsed = u64::try_from((finished - self.started).as_nanos()).unwrap_or(u64::MAX);
        let translate = match self.pass {
            cranelift_codegen::timing::Pass::wasm_translate_function => true,
            cranelift_codegen::timing::Pass::compile => false,
            _ => return,
        };
        WORKER_COUNTERS.with(|cell| {
            let mut counters = cell.get();
            if translate {
                counters.translate_count += 1;
                counters.translate_ns += elapsed;
            } else {
                counters.compile_count += 1;
                counters.compile_ns += elapsed;
            }
            cell.set(counters);
        });
        WORKER_SPANS.with(|spans| {
            spans.borrow_mut().push(PassSpan {
                started: self.started,
                finished,
                translate,
            });
        });
    }
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
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

/// The protection the machine code of a `hint` compile for `target` carries.
///
/// Cranelift's code carries what the target's ISA flags ask for, which is
/// the same at every ISA level. Winch's does not: its aarch64 frame setup is
/// `stp fp, lr` with no `paci*` or `auti*`
/// (`winch/codegen/src/isa/aarch64/masm.rs`), so a `Fast` compile is
/// unprotected even though `sign_return_address` is among its ISA flags.
fn code_protection(target: &CwasmTarget<'_>, hint: AotCompileHint) -> CwasmCodeProtection {
    match hint {
        AotCompileHint::Fast => CwasmCodeProtection::NONE,
        AotCompileHint::Balanced | AotCompileHint::Performance => {
            cwasm_target_code_protection(target.triple())
        }
    }
}

/// The engine configuration that decides the machine code a precompile for
/// `target` under `hint` produces: the target and its ISA flags, the compiler
/// strategy and optimisation level, the wasm features, and the memory
/// profile that bounds-check elision is compiled against.
///
/// Nothing here exists only because of the Helios kernel. The kernel's
/// compiler adds its own runtime mechanisms on top (`build_engine_config`),
/// and `helios-compiler-host` runs the compiler plugin's own module under
/// this configuration on Linux, so the plugin's code is the same on both.
pub fn codegen_config(target: &CwasmTarget<'_>, hint: AotCompileHint) -> Result<Config> {
    let triple = target.triple();
    let mut config = Config::new();
    config
        .target(triple)
        .map_err(|source| CompileError::Target {
            target: triple.to_owned(),
            source,
        })?;
    for flag in target.cranelift_flags() {
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
    // The version string records the protection the machine code carries,
    // and the kernel engine refuses an artifact whose string differs from
    // the protection it requires.
    if let Some(version) = code_protection(target, hint)
        .module_version(ModuleVersionStrategy::WasmtimeVersion.as_str())
    {
        config
            .module_version(ModuleVersionStrategy::Custom(version.clone()))
            .map_err(|source| CompileError::ModuleVersion { version, source })?;
    }
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_threading(true);
    let wasm_simd = cwasm_target_supports_wasm_simd(triple);
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

/// The wasm stack limit every Helios engine runs guests under.
///
/// The kernel's `kernel/src/component/runtime.rs` sets the same 8 MiB, and
/// `helios-compiler-host` takes it from here so the plugin meets one limit on
/// both hosts.
pub const MAX_WASM_STACK: usize = 8 * 1024 * 1024;

/// The native stack a thread that runs wasm under [`MAX_WASM_STACK`] gets:
/// the wasm limit plus 1 MiB of headroom for the host functions it calls.
pub const WASM_THREAD_STACK: usize = 9 * 1024 * 1024;

fn build_engine_config(
    target: &CwasmTarget<'_>,
    hint: AotCompileHint,
    worker_count: usize,
) -> Result<Config> {
    let triple = target.triple();
    let mut config = codegen_config(target, hint)?;
    config.parallel_compilation(worker_count > 1);
    config.stack_limit_interruption(true);
    // The kernel engine reads the limit from the same location (see
    // `cwasm_target_guest_stack_limit_offset`) and refuses a cwasm compiled
    // for the other one.
    if let Some(offset) = cwasm_target_guest_stack_limit_offset(triple) {
        config.stack_limit_location(StackLimitLocation::PerProcessor { offset });
    }
    // Wasmtime validates `max_wasm_stack <= async_stack_size`; keep the async
    // fiber stack comfortably above the wasm stack limit with host headroom.
    config.max_wasm_stack(MAX_WASM_STACK);
    config.async_stack_size(WASM_THREAD_STACK);
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::{
        AotCompileHint, CwasmCodeProtection, CwasmTarget, build_engine_config, code_protection,
        precompile_artifact,
    };
    use helios_artifact::cwasm_target_isa_levels;
    #[cfg(target_arch = "x86_64")]
    use wasmtime::{Engine, Module, ModuleVersionStrategy};

    const MINIMAL_MODULE: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // wasm header
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type
        0x03, 0x02, 0x01, 0x00, // function
        0x07, 0x05, 0x01, 0x01, 0x66, 0x00, 0x00, // export f
        0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b, // body
    ];

    /// One target of each architecture Helios defines levels for.
    const TARGETS: [&str; 3] = [
        "x86_64-unknown-none",
        "aarch64-unknown-none",
        "riscv64gc-unknown-none-elf",
    ];

    /// Every level of `triple`, as a compile target.
    fn every_level(triple: &str) -> impl Iterator<Item = CwasmTarget<'_>> {
        cwasm_target_isa_levels(triple)
            .iter()
            .map(move |level| CwasmTarget::new(triple, level.name()).expect("a defined level"))
    }

    /// The marker `CwasmCodeProtection::module_version` puts in the version
    /// string, which sits uncompressed in the artifact's engine section.
    const SIGNING_MARKER: &[u8] = b"+return-address-signing";

    fn contains_marker(bytes: &[u8]) -> bool {
        bytes
            .windows(SIGNING_MARKER.len())
            .any(|window| window == SIGNING_MARKER)
    }

    /// Cranelift's aarch64 output records that it signs return addresses, at
    /// every ISA level.
    #[test]
    fn aarch64_cranelift_artifacts_record_return_address_signing() {
        for target in every_level("aarch64-unknown-none") {
            for hint in [AotCompileHint::Balanced, AotCompileHint::Performance] {
                let artifact = precompile_artifact(MINIMAL_MODULE, &target, hint)
                    .expect("the minimal module precompiles for aarch64");
                assert!(
                    contains_marker(&artifact.bytes),
                    "{} {hint:?}",
                    target.level().name()
                );
            }
        }
    }

    /// Winch's aarch64 output does not sign, so a `Fast` compile records
    /// the protection of unprotected code whatever the target's flags say.
    #[test]
    fn a_winch_compile_records_no_protection() {
        for target in every_level("aarch64-unknown-none") {
            let level = target.level().name();
            assert_eq!(
                code_protection(&target, AotCompileHint::Fast),
                CwasmCodeProtection::NONE,
                "{level}"
            );
            for hint in [AotCompileHint::Balanced, AotCompileHint::Performance] {
                assert!(
                    code_protection(&target, hint).return_addresses_signed,
                    "{level} {hint:?}"
                );
            }
        }
    }

    #[test]
    fn targets_without_signing_record_no_protection() {
        for triple in ["x86_64-unknown-none", "riscv64gc-unknown-none-elf"] {
            for target in every_level(triple) {
                let artifact =
                    precompile_artifact(MINIMAL_MODULE, &target, AotCompileHint::Performance)
                        .expect("the minimal module precompiles");
                assert!(
                    !contains_marker(&artifact.bytes),
                    "{triple} {}",
                    target.level().name()
                );
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn an_engine_requiring_signed_code_refuses_an_unsigned_artifact() {
        let triple = "x86_64-unknown-linux-gnu";
        let target = CwasmTarget::new(triple, cwasm_target_isa_levels(triple).floor().name())
            .expect("a floor belongs to its own target");
        let artifact = precompile_artifact(MINIMAL_MODULE, &target, AotCompileHint::Performance)
            .expect("the minimal module precompiles");

        let same = build_engine_config(&target, AotCompileHint::Performance, 1)
            .expect("the engine config builds");
        let same_engine = Engine::new(&same).expect("the engine builds");
        unsafe { Module::deserialize(&same_engine, &artifact.bytes) }
            .expect("an artifact deserializes into an engine with the config it was built under");

        let mut requiring_signed = build_engine_config(&target, AotCompileHint::Performance, 1)
            .expect("the engine config builds");
        let version = CwasmCodeProtection {
            return_addresses_signed: true,
        }
        .module_version(ModuleVersionStrategy::WasmtimeVersion.as_str())
        .expect("signed code has its own version");
        requiring_signed
            .module_version(ModuleVersionStrategy::Custom(version))
            .expect("the version string is accepted");
        let engine = Engine::new(&requiring_signed).expect("the engine builds");
        let error = unsafe { Module::deserialize(&engine, &artifact.bytes) }
            .expect_err("unsigned code must not load into an engine that requires signing");
        let debug = format!("{error:?}");
        assert!(debug.contains("incompatible version"), "{debug}");
    }

    #[test]
    fn every_level_of_every_target_compiles() {
        for triple in TARGETS {
            for target in every_level(triple) {
                precompile_artifact(MINIMAL_MODULE, &target, AotCompileHint::Performance)
                    .unwrap_or_else(|error| {
                        panic!(
                            "{triple} level {} does not compile: {error}",
                            target.level().name()
                        )
                    });
            }
        }
    }

    /// Deserialization needs a target that is the host itself, so it runs only
    /// where the host is a platform Helios develops on; the other
    /// architectures' levels are covered by compilation alone.
    #[cfg(any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    mod deserialize {
        use std::cell::{Cell, RefCell};

        use wasmtime::{Engine, Module};

        use super::{AotCompileHint, CwasmTarget, MINIMAL_MODULE, build_engine_config};
        use super::{cwasm_target_isa_levels, precompile_artifact};

        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        const HOST_TARGET: &str = "x86_64-unknown-linux-gnu";
        #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
        const HOST_TARGET: &str = "aarch64-unknown-linux-gnu";
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        const HOST_TARGET: &str = "aarch64-apple-darwin";

        // `detect_host_feature` takes a function pointer, so the probe under
        // test reports through thread-local test state.
        thread_local! {
            static ASKED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
            static DENIED: Cell<Option<&'static str>> = const { Cell::new(None) };
        }

        fn recording_probe(name: &str) -> Option<bool> {
            ASKED.with(|asked| asked.borrow_mut().push(name.to_owned()));
            Some(DENIED.get() != Some(name))
        }

        /// Deserializes the artifact compiled for `target` on an engine whose
        /// probe is `recording_probe`, returning the names the probe was
        /// asked and the outcome.
        fn deserialize(target: &CwasmTarget<'_>) -> (Vec<String>, wasmtime::Result<Module>) {
            let artifact = precompile_artifact(MINIMAL_MODULE, target, AotCompileHint::Performance)
                .expect("the minimal module precompiles");
            let mut config = build_engine_config(target, AotCompileHint::Performance, 1)
                .expect("the engine config builds");
            unsafe {
                config.detect_host_feature(recording_probe);
            }
            let engine = Engine::new(&config).expect("the engine builds");
            ASKED.with(|asked| asked.borrow_mut().clear());
            let outcome = unsafe { Module::deserialize(&engine, &artifact.bytes) };
            (ASKED.with(|asked| asked.take()), outcome)
        }

        #[test]
        fn a_level_is_checked_against_exactly_its_probe_names() {
            DENIED.set(None);
            for level in cwasm_target_isa_levels(HOST_TARGET).iter() {
                let target = CwasmTarget::new(HOST_TARGET, level.name()).expect("a defined level");
                let (mut asked, outcome) = deserialize(&target);
                outcome.unwrap_or_else(|error| {
                    panic!("level {} does not deserialize: {error:?}", level.name())
                });
                asked.sort();
                asked.dedup();
                let mut expected: Vec<_> = level
                    .features()
                    .map(|feature| feature.probe_name().to_owned())
                    .collect();
                expected.sort();
                assert_eq!(asked, expected, "level {}", level.name());
            }
        }

        #[test]
        fn a_missing_feature_is_refused_naming_its_cranelift_flag() {
            for level in cwasm_target_isa_levels(HOST_TARGET).iter() {
                let target = CwasmTarget::new(HOST_TARGET, level.name()).expect("a defined level");
                for feature in level.features() {
                    DENIED.set(Some(feature.probe_name()));
                    let (_, outcome) = deserialize(&target);
                    let error = outcome.expect_err(&format!(
                        "level {} must not deserialize without {}",
                        level.name(),
                        feature.probe_name()
                    ));
                    let debug = format!("{error:?}");
                    assert!(
                        debug.contains(feature.cranelift_flag()),
                        "level {} without {}: {debug}",
                        level.name(),
                        feature.probe_name()
                    );
                    assert!(debug.contains("not available on the host"), "{debug}");
                }
            }
            DENIED.set(None);
        }
    }
}
