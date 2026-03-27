use core::cell::Cell;

super::std::thread_local! {
    static WASMTIME_TLS: Cell<*mut u8> = const { Cell::new(core::ptr::null_mut()) };
}

/// Hosted glue for Wasmtime's custom TLS ABI.
///
/// The no-std Wasmtime build asks the embedder for a single thread-local
/// pointer slot. Hosted Helios already executes on OS threads, so a plain Rust
/// thread-local has the exact semantics Wasmtime expects.
#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_get() -> *mut u8 {
    WASMTIME_TLS.with(Cell::get)
}

/// Hosted glue for Wasmtime's custom TLS ABI.
#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(ptr: *mut u8) {
    WASMTIME_TLS.with(|slot| slot.set(ptr));
}
