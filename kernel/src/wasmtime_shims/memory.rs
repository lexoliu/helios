use core::ptr;

use libc::{MAP_FIXED, MAP_PRIVATE, c_int, c_void};

const PROT_READ: u32 = 1 << 0;
const PROT_WRITE: u32 = 1 << 1;
const PROT_EXEC: u32 = 1 << 2;

#[cfg(any(
    target_vendor = "apple",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
const MAP_ANON_FLAG: c_int = libc::MAP_ANON;

#[cfg(not(any(
    target_vendor = "apple",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
const MAP_ANON_FLAG: c_int = libc::MAP_ANONYMOUS;

#[repr(C)]
pub struct wasmtime_memory_image {
    _private: [u8; 0],
}

fn host_prot_flags(flags: u32) -> c_int {
    let mut prot = 0;

    if flags & PROT_READ != 0 {
        prot |= libc::PROT_READ;
    }
    if flags & PROT_WRITE != 0 {
        prot |= libc::PROT_WRITE;
    }
    if flags & PROT_EXEC != 0 {
        prot |= libc::PROT_EXEC;
    }

    prot
}

fn last_errno() -> i32 {
    super::std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EINVAL)
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_mmap_new(size: usize, prot_flags: u32, ret: &mut *mut u8) -> i32 {
    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            host_prot_flags(prot_flags),
            MAP_PRIVATE | MAP_ANON_FLAG,
            -1,
            0,
        )
    };

    if ptr == libc::MAP_FAILED {
        return last_errno();
    }

    *ret = ptr.cast();
    0
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_mmap_remap(addr: *mut u8, size: usize, prot_flags: u32) -> i32 {
    let ptr = unsafe {
        libc::mmap(
            addr.cast::<c_void>(),
            size,
            host_prot_flags(prot_flags),
            MAP_FIXED | MAP_PRIVATE | MAP_ANON_FLAG,
            -1,
            0,
        )
    };

    if ptr == libc::MAP_FAILED {
        return last_errno();
    }

    0
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_munmap(ptr: *mut u8, size: usize) -> i32 {
    let rc = unsafe { libc::munmap(ptr.cast::<c_void>(), size) };
    if rc == 0 {
        return 0;
    }

    last_errno()
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_mprotect(ptr: *mut u8, size: usize, prot_flags: u32) -> i32 {
    let rc = unsafe { libc::mprotect(ptr.cast::<c_void>(), size, host_prot_flags(prot_flags)) };
    if rc == 0 {
        return 0;
    }

    last_errno()
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_page_size() -> usize {
    let rc = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if rc <= 0 {
        panic!("sysconf(_SC_PAGESIZE) failed");
    }

    rc as usize
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_memory_image_new(
    ptr: *const u8,
    len: usize,
    ret: &mut *mut wasmtime_memory_image,
) -> i32 {
    let _ = ptr;
    let _ = len;
    *ret = ptr::null_mut();
    0
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_memory_image_map_at(
    image: *mut wasmtime_memory_image,
    addr: *mut u8,
    len: usize,
) -> i32 {
    panic!(
        "hosted Wasmtime requested memory image mapping for image={:?} addr={:?} len={len}",
        image, addr
    );
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_memory_image_free(image: *mut wasmtime_memory_image) {
    panic!("hosted Wasmtime requested memory image free for image={image:?}");
}
