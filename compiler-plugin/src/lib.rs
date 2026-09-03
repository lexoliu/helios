use core::mem::{align_of, size_of};
use core::ptr;
use std::sync::Once;
use std::time::Instant;

use helios_compiler_abi::{
    CompileHint, CompilerRequestHeader, CompilerResponseHeader, CompilerStatus,
    HELIOS_COMPILER_ABI_VERSION, HELIOS_COMPILER_REQUEST_PROFILE, OutputKind,
};
use helios_compiler_support::{AotCompileHint, PrecompiledArtifactKind, precompile_artifact};
use tracing_log::LogTracer;
use tracing_subscriber::fmt::Subscriber;

unsafe extern "C" {
    static mut __wasilibc_pthread_self: u8;

    fn __init_tp(thread_pointer: *mut core::ffi::c_void) -> i32;
    fn __wasm_call_ctors();
}

#[unsafe(no_mangle)]
pub extern "C" fn helios_compiler_pthread_self_offset() -> u32 {
    core::ptr::addr_of_mut!(__wasilibc_pthread_self).addr() as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn helios_compiler_initialize(thread_pointer: u32) {
    let status = unsafe { __init_tp(thread_pointer as *mut core::ffi::c_void) };
    assert_eq!(status, 0, "failed to initialize wasi-libc thread pointer");
    unsafe {
        __wasm_call_ctors();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn helios_compiler_alloc(len: usize, align: usize) -> *mut u8 {
    let layout = std::alloc::Layout::from_size_align(len, align)
        .unwrap_or_else(|error| panic!("invalid compiler allocation layout: {error}"));
    unsafe { std::alloc::alloc(layout) }
}

/// Releases an allocation made by `helios_compiler_alloc`.
///
/// # Safety
///
/// `ptr` must come from `helios_compiler_alloc`, `len` and `align`
/// must be the ones it was allocated with, and the allocation must not
/// have been freed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_compiler_free(ptr: *mut u8, len: usize, align: usize) {
    let layout = std::alloc::Layout::from_size_align(len, align)
        .unwrap_or_else(|error| panic!("invalid compiler free layout: {error}"));
    unsafe {
        std::alloc::dealloc(ptr, layout);
    }
}

/// Compiles the request encoded at `request_ptr` and returns the
/// address of the response in the plugin's own linear memory.
///
/// # Safety
///
/// `request_ptr..request_ptr + request_len` must be a readable range of
/// this module's linear memory holding an encoded compile request.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_compiler_compile(request_ptr: u32, request_len: u32) -> u32 {
    let request = unsafe { read_request(request_ptr, request_len) };
    match request {
        Ok(request) => compile(request),
        Err(diagnostic) => response(
            CompilerStatus::InvalidRequest,
            OutputKind::CoreModule,
            Vec::new(),
            diagnostic.into_bytes(),
        ),
    }
}

struct CompileRequest<'a> {
    wasm: &'a [u8],
    target: &'a str,
    hint: AotCompileHint,
    profile: bool,
}

unsafe fn read_request<'a>(
    request_ptr: u32,
    request_len: u32,
) -> Result<CompileRequest<'a>, String> {
    let request_len = request_len as usize;
    if request_len < size_of::<CompilerRequestHeader>() {
        return Err(format!(
            "compiler request length {request_len} is smaller than header {}",
            size_of::<CompilerRequestHeader>()
        ));
    }
    let header = unsafe { ptr::read_unaligned(request_ptr as *const CompilerRequestHeader) };
    if header.abi_version != HELIOS_COMPILER_ABI_VERSION {
        return Err(format!(
            "unsupported compiler ABI version {}, expected {}",
            header.abi_version, HELIOS_COMPILER_ABI_VERSION
        ));
    }
    let wasm = unsafe { slice_from_abi(header.wasm_ptr, header.wasm_len) };
    let target = unsafe { slice_from_abi(header.target_ptr, header.target_len) };
    let target = core::str::from_utf8(target)
        .map_err(|error| format!("compiler target triple is not UTF-8: {error}"))?;
    let hint = match header.hint {
        CompileHint::Fast => AotCompileHint::Fast,
        CompileHint::Balanced => AotCompileHint::Balanced,
        CompileHint::Performance => AotCompileHint::Performance,
    };
    Ok(CompileRequest {
        wasm,
        target,
        hint,
        profile: header.flags & HELIOS_COMPILER_REQUEST_PROFILE != 0,
    })
}

unsafe fn slice_from_abi<'a>(ptr: u32, len: u32) -> &'a [u8] {
    unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) }
}

fn compile(request: CompileRequest<'_>) -> u32 {
    if request.profile {
        enable_compiler_timing_log();
    }
    let started = request.profile.then(Instant::now);
    let result = precompile_artifact(request.wasm, request.target, request.hint);
    match result {
        Ok(artifact) => {
            let output_kind = match artifact.kind {
                PrecompiledArtifactKind::CoreModule => OutputKind::CoreModule,
                PrecompiledArtifactKind::Component => OutputKind::Component,
            };
            let diagnostic = started
                .map(|started| {
                    compiler_timing_report(started, output_kind, artifact.bytes.len()).into_bytes()
                })
                .unwrap_or_default();
            response(CompilerStatus::Ok, output_kind, artifact.bytes, diagnostic)
        }
        Err(error) => response(
            CompilerStatus::CompileFailed,
            OutputKind::CoreModule,
            Vec::new(),
            format!("{error:#}").into_bytes(),
        ),
    }
}

fn enable_compiler_timing_log() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let subscriber = Subscriber::builder()
            .with_writer(std::io::stderr)
            .with_target(true)
            .with_level(true)
            .without_time()
            .finish();
        if tracing::subscriber::set_global_default(subscriber).is_err() {
            return;
        }
        let _ = LogTracer::init();
    });
}

fn compiler_timing_report(started: Instant, output_kind: OutputKind, output_len: usize) -> String {
    let elapsed = started.elapsed();
    // Intentionally skip `publish_current()`: the kernel caches this
    // plugin's wasmtime Module + SharedMemory across compile()
    // invocations, which makes the main thread's cranelift TLS region
    // stable across calls. `publish_current` asserts that
    // `CURRENT_PASS == Pass::None` and aborts (panic = "abort" in this
    // workspace) when stale state is left behind from a previous
    // compile. Worker-thread contributions are already published into
    // the cranelift global by their Drop guards; `take_global` reads
    // those without touching the main thread's TLS, which is enough
    // for the diagnostic breakdown — main-thread passes (Translate
    // WASM function, etc.) are a small fraction of total compile time.
    let pass_times = cranelift_codegen::timing::take_global();
    format!(
        "INFO [helios_compiler_plugin] profile total_ms={} output_kind={output_kind:?} output_len={output_len}\n{pass_times}",
        elapsed.as_millis()
    )
}

fn response(
    status: CompilerStatus,
    output_kind: OutputKind,
    precompiled: Vec<u8>,
    diagnostic: Vec<u8>,
) -> u32 {
    let response_len = size_of::<CompilerResponseHeader>();
    let response_ptr = helios_compiler_alloc(response_len, align_of::<CompilerResponseHeader>());
    assert!(
        !response_ptr.is_null(),
        "compiler response allocation returned null"
    );
    let precompiled = leak_vec(precompiled);
    let diagnostic = leak_vec(diagnostic);
    let header = CompilerResponseHeader {
        abi_version: HELIOS_COMPILER_ABI_VERSION,
        status,
        output_kind,
        precompiled_ptr: precompiled.0,
        precompiled_len: precompiled.1,
        diagnostic_ptr: diagnostic.0,
        diagnostic_len: diagnostic.1,
    };
    unsafe {
        ptr::write_unaligned(response_ptr as *mut CompilerResponseHeader, header);
    }
    response_ptr as u32
}

fn leak_vec(bytes: Vec<u8>) -> (u32, u32) {
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len();
    let ptr = Box::into_raw(boxed) as *mut u8;
    (ptr as u32, len as u32)
}
