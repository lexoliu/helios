extern crate std;

use core::cell::Cell;

std::thread_local! {
    static WASMTIME_TLS: Cell<*mut u8> = const { Cell::new(core::ptr::null_mut()) };
}

/// Hosted glue for the no-std Wasmtime fork's custom TLS ABI.
///
/// The fork's `sys/custom` backend expects the embedder to provide one
/// thread-local pointer slot. Hosted tests and tools already have OS threads,
/// so a normal Rust thread-local is the exact semantics Wasmtime requests.
#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_get() -> *mut u8 {
    WASMTIME_TLS.with(Cell::get)
}

/// Hosted glue for the no-std Wasmtime fork's custom TLS ABI.
#[unsafe(no_mangle)]
extern "C" fn wasmtime_tls_set(ptr: *mut u8) {
    WASMTIME_TLS.with(|slot| slot.set(ptr));
}
