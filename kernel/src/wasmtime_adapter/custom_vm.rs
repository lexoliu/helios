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

/// `prot_flags` bits, matching the runtime's `WASMTIME_PROT_*` constants.
const PROT_READ: u32 = 1 << 0;
const PROT_WRITE: u32 = 1 << 1;
const PROT_EXEC: u32 = 1 << 2;

/// Make a just-written code range executable.
///
/// The runtime allocates compiled code through [`wasmtime_mmap_new`] with
/// read/write protection and then hands the text range to the platform's
/// `Cpu::publish_executable` instead of changing the protection itself, so
/// flipping the range to read/execute is the backend's job. Every backend
/// does it the same way — through its own `mprotect` hook — so the walk lives
/// here rather than once per architecture; what stays architecture-specific
/// is the cache and pipeline maintenance the backend does around this call.
///
/// # Panics
///
/// The range must start on a page boundary (the runtime guarantees this for
/// the text section, see `CustomCodeMemory::required_alignment`), and the
/// backend must be able to protect it. A backend that cannot is misconfigured
/// and there is no correct weaker permission to fall back to.
pub fn publish_code_memory(ptr: *const u8, len: usize) {
    protect_code_memory(ptr, len, PROT_READ | PROT_EXEC, "publish");
}

/// Return a published code range to read/write so it can be edited again.
///
/// The inverse of [`publish_code_memory`]; the runtime calls it through
/// `Cpu::unpublish_executable` before patching code in place.
pub fn unpublish_code_memory(ptr: *const u8, len: usize) {
    protect_code_memory(ptr, len, PROT_READ | PROT_WRITE, "unpublish");
}

fn protect_code_memory(ptr: *const u8, len: usize, prot_flags: u32, operation: &str) {
    if len == 0 {
        return;
    }
    let page_size = (hooks().page_size)();
    let start = ptr as usize;
    assert!(
        start.is_multiple_of(page_size),
        "code-memory range start {start:#x} is not page-aligned"
    );
    // The runtime asks for the text section's exact byte length, which the
    // linker does not pad to a page. Protection is a whole-page operation, so
    // the tail page is included; the runtime has finished writing the range
    // before it publishes, and everything past the text section in the same
    // page is read-only data either way.
    let len = len
        .checked_next_multiple_of(page_size)
        .unwrap_or_else(|| panic!("code-memory range length {len:#x} overflows a page boundary"));
    let result = (hooks().mprotect)(ptr.cast_mut(), len, prot_flags);
    assert!(
        result == 0,
        "code-memory {operation} of {len:#x} bytes at {start:#x} failed: errno={result}"
    );
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
