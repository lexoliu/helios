//! Hosts the Helios compiler plugin (`compiler-plugin/`) under Wasmtime on
//! an ordinary operating system.
//!
//! The kernel is the plugin's first host. This crate is the second: it runs
//! the same core module through the same call sequence the kernel does
//! (`invoke_compiler_inner` and `ensure_compiler_plugin` in
//! `kernel/src/wasmtime_adapter/component_host/service/mod.rs`), links the
//! same import set, and hands the plugin its worker count the same way, so a
//! compile timed here is the plugin's work on this system and nothing else.
//!
//! The plugin's own machine code is compiled with
//! [`helios_compiler_support::codegen_config`], the configuration the kernel's
//! precompile of it uses minus the kernel's runtime mechanisms (stack-limit
//! interruption, async fibers, the pooling allocator), which a process on
//! this system does not have.
//!
//! Concurrency: a [`CompilerHost`] is driven by one thread, which must have
//! a [`WASM_THREAD_STACK`] native stack ([`on_wasm_thread`] provides one).
//! Each `wasi.thread-spawn` the plugin makes is one more native thread with
//! its own store over the shared memory; those threads only ever run inside
//! a compile, and the driving thread joins every one of them before
//! [`CompilerHost::compile`] returns.

mod memory;
mod wasi;

use std::num::NonZeroU32;
use std::sync::atomic::AtomicI32;
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::Instant;

use helios_compiler_abi::{
    CompileHint, CompilerRequestHeader, CompilerResponseHeader, CompilerStatus,
    HELIOS_COMPILER_ABI_VERSION, HELIOS_COMPILER_ALLOC, HELIOS_COMPILER_COMPILE,
    HELIOS_COMPILER_INITIALIZE, HELIOS_COMPILER_PTHREAD_SELF_OFFSET, OutputKind,
};
use helios_compiler_support::{AotCompileHint, MAX_WASM_STACK, codegen_config};
use thiserror::Error;
use wasmtime::{
    Engine, Instance, InstancePre, Linker, MemoryType, Module, SharedMemory, Store, TypedFunc, Val,
};

pub use helios_compiler_support::WASM_THREAD_STACK;
pub use memory::GuestMemoryAccess;

/// The global the plugin's start publishes its main-thread TLS block in.
const TLS_BASE_GLOBAL: &str = "__tls_base";
/// The wasi-threads entry point every spawned thread runs.
const THREAD_START_EXPORT: &str = "wasi_thread_start";

/// Why the plugin could not be hosted or a compile did not produce output.
#[derive(Debug, Error)]
pub enum CompilerHostError {
    #[error("failed to configure the engine for the plugin's own code: {0}")]
    CodegenConfig(#[source] helios_compiler_support::CompileError),
    #[error("failed to build the Wasmtime engine for {target}: {source}")]
    Engine {
        target: String,
        #[source]
        source: wasmtime::Error,
    },
    #[error("failed to precompile the compiler plugin: {0}")]
    Precompile(#[source] wasmtime::Error),
    #[error("failed to load the precompiled compiler plugin: {0}")]
    Deserialize(#[source] wasmtime::Error),
    #[error(
        "the compiler plugin must import `env.memory` as a 32-bit shared memory with a declared maximum; it imports {found}"
    )]
    MemoryContract { found: String },
    #[error("failed to create the plugin's shared memory of {maximum_pages} pages: {source}")]
    SharedMemory {
        maximum_pages: u32,
        #[source]
        source: wasmtime::Error,
    },
    #[error("failed to link the compiler plugin's imports: {0}")]
    Link(#[source] wasmtime::Error),
    #[error("failed to instantiate the compiler plugin: {0}")]
    Instantiate(#[source] wasmtime::Error),
    #[error("the compiler plugin does not export `{name}` with the expected type: {source}")]
    Export {
        name: &'static str,
        #[source]
        source: wasmtime::Error,
    },
    #[error("the compiler plugin does not export the `{TLS_BASE_GLOBAL}` global")]
    MissingTlsBase,
    #[error("the compiler plugin's `{TLS_BASE_GLOBAL}` global holds {found}, not an i32")]
    TlsBaseType { found: String },
    #[error("failed to set the compiler plugin's `{TLS_BASE_GLOBAL}` global: {0}")]
    SetTlsBase(#[source] wasmtime::Error),
    #[error(
        "the compiler plugin's start did not publish a static TLS block: `{TLS_BASE_GLOBAL}` is 0"
    )]
    TlsBaseUnset,
    #[error(
        "the thread pointer overflows: TLS base {tls_base:#x} plus pthread_self offset {offset:#x}"
    )]
    ThreadPointerOverflow { tls_base: u32, offset: u32 },
    #[error("`{export}` trapped: {source}")]
    Call {
        export: &'static str,
        #[source]
        source: wasmtime::Error,
    },
    #[error("{size} bytes do not fit the plugin's 32-bit address space")]
    TooLarge { size: usize },
    #[error("`{HELIOS_COMPILER_ALLOC}` returned null for {len} bytes")]
    AllocationFailed { len: usize },
    #[error(transparent)]
    Memory(#[from] GuestMemoryAccess),
    #[error("the compiler plugin answered ABI version {found}, expected {expected}")]
    AbiMismatch { found: u32, expected: u32 },
    #[error("the compiler plugin answered an unknown {field} {value}")]
    UnknownResponseValue { field: &'static str, value: u32 },
    #[error("the compiler plugin rejected the request with {status:?}: {diagnostic}")]
    Rejected {
        status: CompilerStatus,
        diagnostic: String,
    },
    #[error("compiler plugin thread {thread_id} trapped: {source}")]
    ThreadTrap {
        thread_id: i32,
        #[source]
        source: wasmtime::Error,
    },
    #[error("compiler plugin thread {thread_id} panicked")]
    ThreadPanic { thread_id: i32 },
    #[error("failed to spawn the thread that drives the plugin: {0}")]
    DriverSpawn(#[source] std::io::Error),
    #[error("the thread that drives the plugin panicked")]
    DriverPanic,
}

/// One compile's answer from the plugin.
#[derive(Debug)]
pub struct Compiled {
    /// The precompiled artifact the plugin produced.
    pub output: Vec<u8>,
    pub kind: OutputKind,
    /// What the plugin wrote to its response's diagnostic buffer: empty
    /// unless the request asked for a profile.
    pub diagnostic: Vec<u8>,
}

/// The compiler plugin, precompiled for this host and, after the first
/// compile, resident with its shared memory and linked imports.
pub struct CompilerHost {
    engine: Engine,
    precompiled: Vec<u8>,
    workers: NonZeroU32,
    runtime: Option<PluginRuntime>,
}

/// The state the kernel's `CompilerPluginRuntime` caches across compiles.
struct PluginRuntime {
    instance_pre: InstancePre<PluginState>,
    shared: Arc<Shared>,
    threads: mpsc::Receiver<PluginThread>,
    /// The main-thread TLS block the instance that won the memory-init flag
    /// published; every per-compile instance adopts it, because later
    /// instances skip that initialisation and see `__tls_base == 0`.
    main_thread_tls_base: u32,
}

/// What every store of the plugin, the spawned threads' included, shares.
struct Shared {
    memory: SharedMemory,
    instance_pre: OnceLock<InstancePre<PluginState>>,
    next_thread_id: AtomicI32,
    threads: mpsc::Sender<PluginThread>,
    workers: NonZeroU32,
    /// The epoch of the monotonic clock the plugin reads.
    clock_epoch: Instant,
}

/// One store's data: the shared state plus its own stdio descriptors.
#[derive(Clone)]
struct PluginState {
    shared: Arc<Shared>,
    stdout_open: bool,
    stderr_open: bool,
}

impl PluginState {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            stdout_open: true,
            stderr_open: true,
        }
    }

    fn memory(&self) -> &SharedMemory {
        &self.shared.memory
    }
}

/// A thread the plugin spawned, joined after the compile that spawned it.
struct PluginThread {
    thread_id: i32,
    handle: JoinHandle<wasmtime::Result<()>>,
}

impl CompilerHost {
    /// Builds the engine and precompiles `plugin` for this host: the
    /// counterpart of the prebuild that puts the plugin's cwasm in the
    /// Helios bootfs, so it is paid before any compile is timed.
    ///
    /// `workers` is the worker count the plugin is told through
    /// `RAYON_NUM_THREADS`, as the kernel tells it its kernel-role
    /// processor count.
    pub fn new(plugin: &[u8], workers: NonZeroU32) -> Result<Self, CompilerHostError> {
        let target = target_lexicon::HOST.to_string();
        let mut config = codegen_config(&target, AotCompileHint::Performance)
            .map_err(CompilerHostError::CodegenConfig)?;
        config.parallel_compilation(true);
        config.max_wasm_stack(MAX_WASM_STACK);
        // No fiber runs on this engine; Wasmtime still requires the async
        // stack to hold the wasm stack limit.
        config.async_stack_size(WASM_THREAD_STACK);
        let engine =
            Engine::new(&config).map_err(|source| CompilerHostError::Engine { target, source })?;
        let precompiled = engine
            .precompile_module(plugin)
            .map_err(CompilerHostError::Precompile)?;
        Ok(Self {
            engine,
            precompiled,
            workers,
            runtime: None,
        })
    }

    /// The worker count the plugin is told.
    pub fn workers(&self) -> NonZeroU32 {
        self.workers
    }

    /// Compiles `wasm` for `target` under `hint`, through the kernel's call
    /// sequence, and joins every thread the plugin spawned for it.
    ///
    /// The first call also builds the resident runtime, as the kernel's
    /// first compile of a boot does. The calling thread runs the plugin and
    /// needs a [`WASM_THREAD_STACK`] stack.
    pub fn compile(
        &mut self,
        wasm: &[u8],
        target: &str,
        hint: CompileHint,
    ) -> Result<Compiled, CompilerHostError> {
        let runtime = match &mut self.runtime {
            Some(runtime) => runtime,
            slot @ None => slot.insert(PluginRuntime::build(
                &self.engine,
                &self.precompiled,
                self.workers,
            )?),
        };
        // A failed call can leave the plugin's workers parked on work that
        // never comes, so only a compile that returned is joined; the
        // failure ends the host.
        let compiled = runtime.invoke(wasm, target, hint)?;
        runtime.join_threads()?;
        Ok(compiled)
    }
}

impl PluginRuntime {
    /// `ensure_compiler_plugin`: deserialize the precompiled module, create
    /// its shared memory from the import's declared type, link the imports,
    /// and run one scratch instance whose start publishes the TLS block.
    fn build(
        engine: &Engine,
        precompiled: &[u8],
        workers: NonZeroU32,
    ) -> Result<Self, CompilerHostError> {
        // SAFETY: `precompiled` is the output of `precompile_module` on this
        // same engine, made in this process.
        let module = unsafe { Module::deserialize(engine, precompiled) }
            .map_err(CompilerHostError::Deserialize)?;
        let memory_type = imported_shared_memory_type(&module)?;
        let maximum_pages = memory_type
            .maximum()
            .and_then(|pages| u32::try_from(pages).ok())
            .ok_or_else(|| CompilerHostError::MemoryContract {
                found: format!("{memory_type:?}"),
            })?;
        let memory = SharedMemory::new(engine, memory_type).map_err(|source| {
            CompilerHostError::SharedMemory {
                maximum_pages,
                source,
            }
        })?;
        let (thread_tx, thread_rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            memory: memory.clone(),
            instance_pre: OnceLock::new(),
            next_thread_id: AtomicI32::new(0),
            threads: thread_tx,
            workers,
            clock_epoch: Instant::now(),
        });
        let mut linker = Linker::new(engine);
        wasi::add_imports(&mut linker).map_err(CompilerHostError::Link)?;
        // `Linker::define` takes a store only to check ownership; a shared
        // memory belongs to the engine, so the scratch store serves.
        let mut scratch = Store::new(engine, PluginState::new(shared.clone()));
        for import in module.imports() {
            if import.ty().memory().is_some() {
                linker
                    .define(&scratch, import.module(), import.name(), memory.clone())
                    .map_err(CompilerHostError::Link)?;
            }
        }
        let instance_pre = linker
            .instantiate_pre(&module)
            .map_err(CompilerHostError::Link)?;
        let instance = instance_pre
            .instantiate(&mut scratch)
            .map_err(CompilerHostError::Instantiate)?;
        let main_thread_tls_base = tls_base(&mut scratch, &instance)?;
        if main_thread_tls_base == 0 {
            return Err(CompilerHostError::TlsBaseUnset);
        }
        drop(scratch);
        // A fresh `OnceLock` set exactly once, before any thread can spawn.
        let _ = shared.instance_pre.set(instance_pre.clone());
        tracing::info!(
            workers = workers.get(),
            maximum_pages,
            "built the resident compiler plugin runtime"
        );
        Ok(Self {
            instance_pre,
            shared,
            threads: thread_rx,
            main_thread_tls_base,
        })
    }

    /// `invoke_compiler_inner`: a fresh store and instance, the adopted TLS
    /// block, `initialize`, three allocations, the request, and `compile`.
    fn invoke(
        &self,
        wasm: &[u8],
        target: &str,
        hint: CompileHint,
    ) -> Result<Compiled, CompilerHostError> {
        let mut store = Store::new(
            self.instance_pre.module().engine(),
            PluginState::new(self.shared.clone()),
        );
        let instance = self
            .instance_pre
            .instantiate(&mut store)
            .map_err(CompilerHostError::Instantiate)?;
        let tls_base = self.main_thread_tls_base;
        set_tls_base(&mut store, &instance, tls_base)?;
        let pthread_self_offset =
            typed_func::<(), i32>(&mut store, &instance, HELIOS_COMPILER_PTHREAD_SELF_OFFSET)?
                .call(&mut store, ())
                .map_err(|source| CompilerHostError::Call {
                    export: HELIOS_COMPILER_PTHREAD_SELF_OFFSET,
                    source,
                })? as u32;
        let thread_pointer = tls_base.checked_add(pthread_self_offset).ok_or(
            CompilerHostError::ThreadPointerOverflow {
                tls_base,
                offset: pthread_self_offset,
            },
        )?;
        typed_func::<i32, ()>(&mut store, &instance, HELIOS_COMPILER_INITIALIZE)?
            .call(&mut store, thread_pointer as i32)
            .map_err(|source| CompilerHostError::Call {
                export: HELIOS_COMPILER_INITIALIZE,
                source,
            })?;
        let alloc = typed_func::<(i32, i32), i32>(&mut store, &instance, HELIOS_COMPILER_ALLOC)?;
        let compile =
            typed_func::<(i32, i32), i32>(&mut store, &instance, HELIOS_COMPILER_COMPILE)?;

        let target = target.as_bytes();
        let request_len = size_of::<CompilerRequestHeader>();
        let wasm_ptr = guest_alloc(&mut store, &alloc, wasm.len(), 1)?;
        let target_ptr = guest_alloc(&mut store, &alloc, target.len(), 1)?;
        let request_ptr = guest_alloc(
            &mut store,
            &alloc,
            request_len,
            align_of::<CompilerRequestHeader>(),
        )?;
        let memory = self.shared.memory.clone();
        memory::write(&memory, wasm_ptr, wasm)?;
        memory::write(&memory, target_ptr, target)?;
        let request = encode_request(RequestFields {
            hint,
            wasm_ptr,
            wasm_len: guest_len(wasm.len())?,
            target_ptr,
            target_len: guest_len(target.len())?,
            // `profile: false`, as the bench's AOT request is.
            flags: 0,
        });
        memory::write(&memory, request_ptr, &request)?;

        let response_ptr = compile
            .call(&mut store, (request_ptr as i32, request_len as i32))
            .map_err(|source| CompilerHostError::Call {
                export: HELIOS_COMPILER_COMPILE,
                source,
            })? as u32;
        let response = decode_response(&memory::read(
            &memory,
            response_ptr,
            size_of::<CompilerResponseHeader>(),
        )?)?;
        let diagnostic = memory::read(
            &memory,
            response.diagnostic_ptr,
            response.diagnostic_len as usize,
        )?;
        if response.status != CompilerStatus::Ok {
            return Err(CompilerHostError::Rejected {
                status: response.status,
                diagnostic: String::from_utf8_lossy(&diagnostic).into_owned(),
            });
        }
        let output = memory::read(
            &memory,
            response.precompiled_ptr,
            response.precompiled_len as usize,
        )?;
        Ok(Compiled {
            output,
            kind: response.output_kind,
            diagnostic,
        })
    }

    /// Joins every thread the last compile spawned, the threads those
    /// spawned included, and fails on the first that trapped or panicked.
    fn join_threads(&self) -> Result<(), CompilerHostError> {
        // A thread is only ever spawned by a thread that is still running,
        // and every thread received here is joined before the queue is read
        // again, so an empty queue means no thread of this compile is left.
        let mut failure = None;
        loop {
            let spawned: Vec<PluginThread> = self.threads.try_iter().collect();
            if spawned.is_empty() {
                break;
            }
            for thread in spawned {
                let outcome = match thread.handle.join() {
                    Ok(Ok(())) => continue,
                    Ok(Err(source)) => CompilerHostError::ThreadTrap {
                        thread_id: thread.thread_id,
                        source,
                    },
                    Err(_) => CompilerHostError::ThreadPanic {
                        thread_id: thread.thread_id,
                    },
                };
                tracing::error!(thread_id = thread.thread_id, error = %outcome, "compiler plugin thread failed");
                failure.get_or_insert(outcome);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

/// Runs `work` on a thread with a [`WASM_THREAD_STACK`] stack and returns
/// what it returned: the thread a [`CompilerHost`] is driven from.
pub fn on_wasm_thread<R, F>(work: F) -> Result<R, CompilerHostError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    std::thread::Builder::new()
        .name("compiler-host".to_owned())
        .stack_size(WASM_THREAD_STACK)
        .spawn(work)
        .map_err(CompilerHostError::DriverSpawn)?
        .join()
        .map_err(|_| CompilerHostError::DriverPanic)
}

/// The type of the plugin's `env.memory` import, checked against the
/// contract the kernel holds it to.
fn imported_shared_memory_type(module: &Module) -> Result<MemoryType, CompilerHostError> {
    let import = module
        .imports()
        .find(|import| import.module() == "env" && import.name() == "memory");
    let memory_type = import.and_then(|import| import.ty().memory().cloned());
    match memory_type {
        Some(ty) if ty.is_shared() && !ty.is_64() && ty.maximum().is_some() => Ok(ty),
        other => Err(CompilerHostError::MemoryContract {
            found: format!("{other:?}"),
        }),
    }
}

fn tls_base(store: &mut Store<PluginState>, instance: &Instance) -> Result<u32, CompilerHostError> {
    let global = instance
        .get_global(&mut *store, TLS_BASE_GLOBAL)
        .ok_or(CompilerHostError::MissingTlsBase)?;
    match global.get(&mut *store) {
        Val::I32(value) => Ok(value as u32),
        value => Err(CompilerHostError::TlsBaseType {
            found: format!("{value:?}"),
        }),
    }
}

fn set_tls_base(
    store: &mut Store<PluginState>,
    instance: &Instance,
    tls_base: u32,
) -> Result<(), CompilerHostError> {
    instance
        .get_global(&mut *store, TLS_BASE_GLOBAL)
        .ok_or(CompilerHostError::MissingTlsBase)?
        .set(&mut *store, Val::I32(tls_base as i32))
        .map_err(CompilerHostError::SetTlsBase)
}

fn typed_func<Params, Results>(
    store: &mut Store<PluginState>,
    instance: &Instance,
    name: &'static str,
) -> Result<TypedFunc<Params, Results>, CompilerHostError>
where
    Params: wasmtime::WasmParams,
    Results: wasmtime::WasmResults,
{
    instance
        .get_typed_func(&mut *store, name)
        .map_err(|source| CompilerHostError::Export { name, source })
}

fn guest_alloc(
    store: &mut Store<PluginState>,
    alloc: &TypedFunc<(i32, i32), i32>,
    len: usize,
    align: usize,
) -> Result<u32, CompilerHostError> {
    let ptr = alloc
        .call(&mut *store, (guest_len(len)? as i32, align as i32))
        .map_err(|source| CompilerHostError::Call {
            export: HELIOS_COMPILER_ALLOC,
            source,
        })?;
    if ptr == 0 {
        return Err(CompilerHostError::AllocationFailed { len });
    }
    Ok(ptr as u32)
}

/// A length the plugin's `i32` ABI can carry.
fn guest_len(len: usize) -> Result<u32, CompilerHostError> {
    i32::try_from(len)
        .map(|len| len as u32)
        .map_err(|_| CompilerHostError::TooLarge { size: len })
}

struct RequestFields {
    hint: CompileHint,
    wasm_ptr: u32,
    wasm_len: u32,
    target_ptr: u32,
    target_len: u32,
    flags: u32,
}

/// The request header's bytes in the plugin's linear memory: the
/// `#[repr(C)]` `CompilerRequestHeader`, whose seven `u32` fields lie in
/// declaration order with no padding, each little-endian as wasm memory is.
fn encode_request(fields: RequestFields) -> [u8; size_of::<CompilerRequestHeader>()] {
    let words = [
        HELIOS_COMPILER_ABI_VERSION,
        fields.hint as u32,
        fields.wasm_ptr,
        fields.wasm_len,
        fields.target_ptr,
        fields.target_len,
        fields.flags,
    ];
    let mut bytes = [0; size_of::<CompilerRequestHeader>()];
    for (chunk, word) in bytes.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

const _: () = assert!(size_of::<CompilerRequestHeader>() == 7 * 4);
const _: () = assert!(size_of::<CompilerResponseHeader>() == 7 * 4);

struct Response {
    status: CompilerStatus,
    output_kind: OutputKind,
    precompiled_ptr: u32,
    precompiled_len: u32,
    diagnostic_ptr: u32,
    diagnostic_len: u32,
}

/// Decodes `CompilerResponseHeader` word by word, so a value the plugin
/// writes that no variant names is an error rather than an invalid enum.
fn decode_response(bytes: &[u8]) -> Result<Response, CompilerHostError> {
    let word = |index: usize| {
        let start = index * 4;
        u32::from_le_bytes(
            bytes[start..start + 4]
                .try_into()
                .expect("a response word is four bytes"),
        )
    };
    let abi_version = word(0);
    if abi_version != HELIOS_COMPILER_ABI_VERSION {
        return Err(CompilerHostError::AbiMismatch {
            found: abi_version,
            expected: HELIOS_COMPILER_ABI_VERSION,
        });
    }
    let status = match word(1) {
        0 => CompilerStatus::Ok,
        1 => CompilerStatus::InvalidRequest,
        2 => CompilerStatus::InvalidInput,
        3 => CompilerStatus::CompileFailed,
        value => {
            return Err(CompilerHostError::UnknownResponseValue {
                field: "status",
                value,
            });
        }
    };
    let output_kind = match word(2) {
        0 => OutputKind::CoreModule,
        1 => OutputKind::Component,
        value => {
            return Err(CompilerHostError::UnknownResponseValue {
                field: "output kind",
                value,
            });
        }
    };
    Ok(Response {
        status,
        output_kind,
        precompiled_ptr: word(3),
        precompiled_len: word(4),
        diagnostic_ptr: word(5),
        diagnostic_len: word(6),
    })
}
