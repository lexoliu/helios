//! Wasmtime `custom-virtual-memory` C ABI providers.
//!
//! When the kernel-side wasmtime build is compiled with the
//! `custom-virtual-memory` feature, it expects a set of
//! `extern "C"` symbols that wrap the host's mmap-equivalent
//! primitives. helios-kernel exposes those symbols here and routes
//! every call through a typed `CustomVmHooks` table that the active
//! backend installs at boot via [`install_hooks`].
//!
//! The hooks table is a struct of plain function pointers — no
//! `dyn Trait` (per AGENTS §3) — so call sites compile to a
//! straight indirect call after a single atomic load. The backend
//! creates a `&'static CustomVmHooks` value per the user's
//! "&'static / leak is fine" allowance and installs it once before
//! any guest code triggers the wasmtime allocator.
//!
//! Backends that have not installed hooks abort the kernel with a
//! clear diagnostic the first time wasmtime asks for memory; this is
//! the bare-metal equivalent of "no mmap wired up", and matches
//! AGENTS §3 by failing loudly rather than masking the missing
//! configuration with a fallback path.

use core::ffi::c_int;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

/// Opaque pointer type wasmtime uses for the COW image handle. The
/// kernel never inspects the inside; backends define their own
/// concrete struct cast through this pointer.
#[repr(C)]
pub struct WasmtimeMemoryImage {
    _opaque: [u8; 0],
}

/// Function-pointer table installed by the active backend.
pub struct CustomVmHooks {
    pub mmap_new: extern "C" fn(size: usize, prot_flags: u32, ret: &mut *mut u8) -> c_int,
    pub mmap_remap: extern "C" fn(addr: *mut u8, size: usize, prot_flags: u32) -> c_int,
    pub munmap: extern "C" fn(ptr: *mut u8, size: usize) -> c_int,
    pub mprotect: extern "C" fn(ptr: *mut u8, size: usize, prot_flags: u32) -> c_int,
    pub page_size: extern "C" fn() -> usize,
    pub memory_image_new:
        extern "C" fn(ptr: *const u8, len: usize, ret: &mut *mut WasmtimeMemoryImage) -> c_int,
    pub memory_image_free: extern "C" fn(image: *mut WasmtimeMemoryImage),
    pub memory_image_map_at:
        extern "C" fn(image: *mut WasmtimeMemoryImage, addr: *mut u8, len: usize) -> c_int,
}

pub type RuntimeMemoryHooks = CustomVmHooks;
pub type RuntimeMemoryImage = WasmtimeMemoryImage;

static HOOKS: AtomicPtr<CustomVmHooks> = AtomicPtr::new(ptr::null_mut());

/// Install the active backend's custom-virtual-memory hooks. Must be
/// called once during boot before any guest code reaches the
/// wasmtime allocator. Subsequent calls panic.
pub fn install_hooks(hooks: &'static CustomVmHooks) {
    let prev = HOOKS.swap(hooks as *const _ as *mut _, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "wasmtime custom-virtual-memory hooks installed more than once"
    );
}

fn hooks() -> &'static CustomVmHooks {
    let ptr = HOOKS.load(Ordering::Acquire);
    assert!(
        !ptr.is_null(),
        "wasmtime called the custom-virtual-memory ABI before any backend installed hooks"
    );
    unsafe { &*(ptr as *const CustomVmHooks) }
}

/// Default `memory_image_new` hook: report "no image possible" by
/// storing `NULL`. Wasmtime accepts this as a non-fatal opt-out and
/// falls back to per-instance memcpy of data segments. Backends that
/// do not yet implement COW images compose this with their own
/// mmap/mprotect/munmap entries.
pub extern "C" fn default_memory_image_new(
    _ptr: *const u8,
    _len: usize,
    ret: &mut *mut WasmtimeMemoryImage,
) -> c_int {
    *ret = ptr::null_mut();
    0
}

/// Default `memory_image_free` hook: paired with
/// [`default_memory_image_new`] which always stores `NULL`, so
/// wasmtime never calls this on a real image.
pub extern "C" fn default_memory_image_free(_image: *mut WasmtimeMemoryImage) {}

/// Default `memory_image_map_at` hook: paired with
/// [`default_memory_image_new`] which produces no real image, so
/// wasmtime should never reach this code path. A backend that does
/// support COW images must override `memory_image_new` and
/// `memory_image_map_at` together.
pub extern "C" fn default_memory_image_map_at(
    _image: *mut WasmtimeMemoryImage,
    _addr: *mut u8,
    _len: usize,
) -> c_int {
    panic!("wasmtime_memory_image_map_at called on a default (no-image) backend");
}

/// Default `page_size` hook: 4 KiB on every backend helios currently
/// supports.
pub extern "C" fn default_page_size() -> usize {
    4096
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_mmap_new(size: usize, prot_flags: u32, ret: &mut *mut u8) -> c_int {
    (hooks().mmap_new)(size, prot_flags, ret)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_mmap_remap(addr: *mut u8, size: usize, prot_flags: u32) -> c_int {
    (hooks().mmap_remap)(addr, size, prot_flags)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_munmap(ptr: *mut u8, size: usize) -> c_int {
    (hooks().munmap)(ptr, size)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_mprotect(ptr: *mut u8, size: usize, prot_flags: u32) -> c_int {
    (hooks().mprotect)(ptr, size, prot_flags)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_page_size() -> usize {
    (hooks().page_size)()
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_memory_image_new(
    ptr: *const u8,
    len: usize,
    ret: &mut *mut WasmtimeMemoryImage,
) -> c_int {
    (hooks().memory_image_new)(ptr, len, ret)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_memory_image_free(image: *mut WasmtimeMemoryImage) {
    (hooks().memory_image_free)(image)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn wasmtime_memory_image_map_at(
    image: *mut WasmtimeMemoryImage,
    addr: *mut u8,
    len: usize,
) -> c_int {
    (hooks().memory_image_map_at)(image, addr, len)
}
