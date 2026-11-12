use std::sync::Once;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use helios_artifact::CwasmTarget;
use helios_compiler_abi::{
    CompileHint, CompilerRequestHeader, CompilerResponseHeader, CompilerStatus,
    HELIOS_COMPILER_ABI_VERSION, HELIOS_COMPILER_REQUEST_PROFILE, OutputKind,
};
use helios_compiler_support::{
    AotCompileHint, CompileProfile, PrecompiledArtifactKind, precompile_artifact,
    precompile_artifact_profiled,
};
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

/// Bytes currently owned across the interface: what `helios_compiler_alloc`
/// and the response allocations added, less what `helios_compiler_free`
/// returned. The plugin's own state, read through
/// `helios_compiler_outstanding_bytes`.
static OUTSTANDING_BYTES: AtomicU32 = AtomicU32::new(0);

fn add_outstanding(len: usize) {
    // The plugin's memory is a 32-bit address space, so a live allocation's
    // length always fits.
    let len = u32::try_from(len).expect("compiler allocation exceeds the 32-bit address space");
    OUTSTANDING_BYTES.fetch_add(len, Ordering::Relaxed);
}

/// Allocates a buffer the caller owns until it frees it through
/// `helios_compiler_free` (see `helios_compiler_abi` for the contract).
/// Returns 0 when the plugin's memory is exhausted.
#[unsafe(no_mangle)]
pub extern "C" fn helios_compiler_alloc(len: usize, align: usize) -> *mut u8 {
    let layout = std::alloc::Layout::from_size_align(len, align)
        .unwrap_or_else(|error| panic!("invalid compiler allocation layout: {error}"));
    assert_ne!(
        layout.size(),
        0,
        "zero-length buffers are never allocated across the compiler ABI"
    );
    let ptr = unsafe { std::alloc::alloc(layout) };
    if !ptr.is_null() {
        add_outstanding(len);
    }
    ptr
}

/// Releases an allocation made by `helios_compiler_alloc` or handed over in a
/// compiler response.
///
/// # Safety
///
/// `ptr` must come from `helios_compiler_alloc` or a response, `len` and
/// `align` must be the ones the compiler ABI states for that buffer, and the
/// allocation must not have been freed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_compiler_free(ptr: *mut u8, len: usize, align: usize) {
    let layout = std::alloc::Layout::from_size_align(len, align)
        .unwrap_or_else(|error| panic!("invalid compiler free layout: {error}"));
    unsafe {
        std::alloc::dealloc(ptr, layout);
    }
    let len = u32::try_from(len).expect("compiler allocation exceeds the 32-bit address space");
    OUTSTANDING_BYTES.fetch_sub(len, Ordering::Relaxed);
}

/// Bytes currently owned across the interface; see `helios_compiler_abi`.
#[unsafe(no_mangle)]
pub extern "C" fn helios_compiler_outstanding_bytes() -> u32 {
    OUTSTANDING_BYTES.load(Ordering::Relaxed)
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
    target: CwasmTarget<'a>,
    hint: AotCompileHint,
    profile: bool,
}

unsafe fn read_request<'a>(
    request_ptr: u32,
    request_len: u32,
) -> Result<CompileRequest<'a>, String> {
    let request = unsafe { slice_from_abi(request_ptr, request_len) };
    let Some((header, _)) = request.split_first_chunk::<{ CompilerRequestHeader::SIZE }>() else {
        return Err(format!(
            "compiler request length {request_len} is smaller than header {}",
            CompilerRequestHeader::SIZE
        ));
    };
    let header = CompilerRequestHeader::from_bytes(header);
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
    let isa_level = unsafe { slice_from_abi(header.isa_level_ptr, header.isa_level_len) };
    let isa_level = core::str::from_utf8(isa_level)
        .map_err(|error| format!("compiler ISA level is not UTF-8: {error}"))?;
    let target = compile_target(target, isa_level)?;
    let hint = match CompileHint::try_from(header.hint).map_err(|error| error.to_string())? {
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

/// Names the level `isa_level` of `triple`'s chain; an unknown level's
/// diagnostic lists the levels the target defines.
fn compile_target<'a>(triple: &'a str, isa_level: &str) -> Result<CwasmTarget<'a>, String> {
    CwasmTarget::new(triple, isa_level)
        .map_err(|error| format!("invalid compiler ISA level for target {triple}: {error}"))
}

/// The bytes a `(ptr, len)` pair of the compiler ABI names; a zero length is
/// the empty slice whatever the pointer.
///
/// # Safety
///
/// A non-zero `len` names a readable range of this module's linear memory.
unsafe fn slice_from_abi<'a>(ptr: u32, len: u32) -> &'a [u8] {
    if len == 0 {
        return &[];
    }
    unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) }
}

fn compile(request: CompileRequest<'_>) -> u32 {
    if request.profile {
        enable_compiler_timing_log();
    }
    let started = request.profile.then(Instant::now);
    let memory_pages_before = request.profile.then(compiler_memory_pages);
    let result = if request.profile {
        precompile_artifact_profiled(request.wasm, &request.target, request.hint)
            .map(|(artifact, profile)| (artifact, Some(profile)))
    } else {
        precompile_artifact(request.wasm, &request.target, request.hint)
            .map(|artifact| (artifact, None))
    };
    match result {
        Ok((artifact, profile)) => {
            let output_kind = match artifact.kind {
                PrecompiledArtifactKind::CoreModule => OutputKind::CoreModule,
                PrecompiledArtifactKind::Component => OutputKind::Component,
            };
            let diagnostic = if let Some(started) = started {
                let report_started = Instant::now();
                let mut diagnostic =
                    compiler_timing_report(started, output_kind, artifact.bytes.len());
                let report_us = elapsed_us(report_started);
                let profile = profile.as_ref().expect("profile result missing");
                diagnostic.push_str(&compile_phase_report(
                    profile,
                    report_us,
                    memory_pages_before.expect("profile memory size missing"),
                    compiler_memory_pages(),
                ));
                diagnostic.into_bytes()
            } else {
                Vec::new()
            };
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
        elapsed.as_millis(),
    )
}

fn compile_phase_report(
    profile: &CompileProfile,
    report_us: u64,
    memory_pages_before: u32,
    memory_pages_after: u32,
) -> String {
    let mut report = format!(
        "compile-phases engine_us={} pool_us={} compile_us={} report_us={report_us} workers={} memory_pages_before={memory_pages_before} memory_pages_after={memory_pages_after}\n",
        profile.engine_us, profile.pool_us, profile.compile_us, profile.worker_count,
    );
    for worker in &profile.workers {
        report.push_str(&format!(
            "compile-worker index={} translate_n={} translate_us={} compile_n={} compile_us={} lifetime_us={} first_pass_us={} last_pass_us={} between_us={} longest_between_us={} longest_between_start_us={}\n",
            worker.index,
            worker.translate_count,
            worker.translate_us,
            worker.compile_count,
            worker.compile_us,
            worker.lifetime_us,
            worker.first_pass_us,
            worker.last_pass_us,
            worker.between_passes_us,
            worker.longest_between_us,
            worker.longest_between_start_us,
        ));
    }
    for span in &profile.longest {
        report.push_str(&format!(
            "compile-longest worker={} start_us={} duration_us={}\n",
            span.worker, span.start_us, span.duration_us,
        ));
    }
    report
}

#[inline]
fn compiler_memory_pages() -> u32 {
    #[cfg(target_arch = "wasm32")]
    {
        core::arch::wasm32::memory_size::<0>() as u32
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0
    }
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn response(
    status: CompilerStatus,
    output_kind: OutputKind,
    precompiled: Vec<u8>,
    diagnostic: Vec<u8>,
) -> u32 {
    let response_ptr =
        helios_compiler_alloc(CompilerResponseHeader::SIZE, CompilerResponseHeader::ALIGN);
    assert!(
        !response_ptr.is_null(),
        "compiler response allocation returned null"
    );
    let precompiled = hand_over(precompiled);
    let diagnostic = hand_over(diagnostic);
    let header = CompilerResponseHeader {
        abi_version: HELIOS_COMPILER_ABI_VERSION,
        status: status.into(),
        output_kind: output_kind.into(),
        precompiled_ptr: precompiled.0,
        precompiled_len: precompiled.1,
        diagnostic_ptr: diagnostic.0,
        diagnostic_len: diagnostic.1,
    };
    let bytes = header.to_bytes();
    // SAFETY: `response_ptr` is a live allocation of `SIZE` bytes just made
    // above, and a byte copy has no alignment requirement.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), response_ptr, bytes.len());
    }
    response_ptr as u32
}

/// Gives `bytes` to the caller as a `(ptr, len)` pair, to be returned through
/// `helios_compiler_free` with `align` 1.
///
/// The buffer is a boxed slice, so its allocation is exactly `len` bytes with
/// no spare capacity, which is the layout `helios_compiler_free` is told. An
/// empty buffer is `(0, 0)`: nothing is allocated and nothing is freed.
fn hand_over(bytes: Vec<u8>) -> (u32, u32) {
    if bytes.is_empty() {
        return (0, 0);
    }
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len();
    add_outstanding(len);
    let ptr = Box::into_raw(boxed) as *mut u8;
    (ptr as u32, len as u32)
}

#[cfg(test)]
mod tests {
    use super::compile_target;

    #[test]
    fn an_unknown_isa_level_is_rejected_naming_the_defined_levels() {
        let error = compile_target("x86_64-unknown-none", "x86-64-v9")
            .expect_err("a level the target does not define is refused");
        assert!(error.contains("x86-64-v9"), "{error}");
        assert!(
            error.contains("x86-64-v3, x86-64-v4, x86-64-v4+vbmi+bitalg"),
            "{error}"
        );
    }

    #[test]
    fn a_defined_isa_level_is_accepted() {
        let target = compile_target("aarch64-unknown-none", "aarch64-lse-fp16-dotprod")
            .expect("a defined level is accepted");
        assert_eq!(target.level().name(), "aarch64-lse-fp16-dotprod");
    }
}
