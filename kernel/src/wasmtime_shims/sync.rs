use core::sync::atomic::{AtomicUsize, Ordering};

use super::std::{boxed::Box, mem, ptr};
use libc::{pthread_mutex_t, pthread_rwlock_t};

unsafe fn storage_atomic(storage: *mut usize) -> *const AtomicUsize {
    storage.cast::<AtomicUsize>()
}

fn init_mutex(storage: *mut usize) -> *mut pthread_mutex_t {
    let atomic = unsafe { &*storage_atomic(storage) };
    let current = atomic.load(Ordering::Acquire);
    if current != 0 {
        return current as *mut pthread_mutex_t;
    }

    let mut mutex = Box::new(unsafe { mem::zeroed::<pthread_mutex_t>() });
    let rc = unsafe { libc::pthread_mutex_init(&mut *mutex, ptr::null()) };
    if rc != 0 {
        panic!("pthread_mutex_init failed with error {rc}");
    }

    let mutex_ptr = Box::into_raw(mutex);
    match atomic.compare_exchange(0, mutex_ptr as usize, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => mutex_ptr,
        Err(existing) => {
            unsafe {
                let rc = libc::pthread_mutex_destroy(mutex_ptr);
                if rc != 0 {
                    panic!("pthread_mutex_destroy failed with error {rc}");
                }
                drop(Box::from_raw(mutex_ptr));
            }
            existing as *mut pthread_mutex_t
        }
    }
}

fn init_rwlock(storage: *mut usize) -> *mut pthread_rwlock_t {
    let atomic = unsafe { &*storage_atomic(storage) };
    let current = atomic.load(Ordering::Acquire);
    if current != 0 {
        return current as *mut pthread_rwlock_t;
    }

    let mut rwlock = Box::new(unsafe { mem::zeroed::<pthread_rwlock_t>() });
    let rc = unsafe { libc::pthread_rwlock_init(&mut *rwlock, ptr::null()) };
    if rc != 0 {
        panic!("pthread_rwlock_init failed with error {rc}");
    }

    let rwlock_ptr = Box::into_raw(rwlock);
    match atomic.compare_exchange(0, rwlock_ptr as usize, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => rwlock_ptr,
        Err(existing) => {
            unsafe {
                let rc = libc::pthread_rwlock_destroy(rwlock_ptr);
                if rc != 0 {
                    panic!("pthread_rwlock_destroy failed with error {rc}");
                }
                drop(Box::from_raw(rwlock_ptr));
            }
            existing as *mut pthread_rwlock_t
        }
    }
}

fn free_mutex(storage: *mut usize) {
    let atomic = unsafe { &*storage_atomic(storage) };
    let ptr = atomic.swap(0, Ordering::AcqRel);
    if ptr == 0 {
        return;
    }

    let mutex = ptr as *mut pthread_mutex_t;
    unsafe {
        let rc = libc::pthread_mutex_destroy(mutex);
        if rc != 0 {
            panic!("pthread_mutex_destroy failed with error {rc}");
        }
        drop(Box::from_raw(mutex));
    }
}

fn free_rwlock(storage: *mut usize) {
    let atomic = unsafe { &*storage_atomic(storage) };
    let ptr = atomic.swap(0, Ordering::AcqRel);
    if ptr == 0 {
        return;
    }

    let rwlock = ptr as *mut pthread_rwlock_t;
    unsafe {
        let rc = libc::pthread_rwlock_destroy(rwlock);
        if rc != 0 {
            panic!("pthread_rwlock_destroy failed with error {rc}");
        }
        drop(Box::from_raw(rwlock));
    }
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_sync_lock_free(lock: *mut usize) {
    free_mutex(lock);
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_sync_lock_acquire(lock: *mut usize) {
    let mutex = init_mutex(lock);
    let rc = unsafe { libc::pthread_mutex_lock(mutex) };
    if rc != 0 {
        panic!("pthread_mutex_lock failed with error {rc}");
    }
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_sync_lock_release(lock: *mut usize) {
    let mutex = init_mutex(lock);
    let rc = unsafe { libc::pthread_mutex_unlock(mutex) };
    if rc != 0 {
        panic!("pthread_mutex_unlock failed with error {rc}");
    }
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_sync_rwlock_read(lock: *mut usize) {
    let rwlock = init_rwlock(lock);
    let rc = unsafe { libc::pthread_rwlock_rdlock(rwlock) };
    if rc != 0 {
        panic!("pthread_rwlock_rdlock failed with error {rc}");
    }
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_sync_rwlock_read_release(lock: *mut usize) {
    let rwlock = init_rwlock(lock);
    let rc = unsafe { libc::pthread_rwlock_unlock(rwlock) };
    if rc != 0 {
        panic!("pthread_rwlock_unlock(read) failed with error {rc}");
    }
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_sync_rwlock_write(lock: *mut usize) {
    let rwlock = init_rwlock(lock);
    let rc = unsafe { libc::pthread_rwlock_wrlock(rwlock) };
    if rc != 0 {
        panic!("pthread_rwlock_wrlock failed with error {rc}");
    }
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_sync_rwlock_write_release(lock: *mut usize) {
    let rwlock = init_rwlock(lock);
    let rc = unsafe { libc::pthread_rwlock_unlock(rwlock) };
    if rc != 0 {
        panic!("pthread_rwlock_unlock(write) failed with error {rc}");
    }
}

#[unsafe(no_mangle)]
extern "C" fn wasmtime_sync_rwlock_free(lock: *mut usize) {
    free_rwlock(lock);
}
