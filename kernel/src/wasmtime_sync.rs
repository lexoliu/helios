use core::hint::spin_loop;
use core::sync::atomic::{AtomicUsize, Ordering};

const MUTEX_UNLOCKED: usize = 0;
const MUTEX_LOCKED: usize = 1;
const RWLOCK_WRITER: usize = usize::MAX;

#[inline]
unsafe fn atomic_from_storage(storage: *mut usize) -> &'static AtomicUsize {
    unsafe { &*storage.cast::<AtomicUsize>() }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_sync_lock_acquire(lock: *mut usize) {
    let lock = unsafe { atomic_from_storage(lock) };
    while lock
        .compare_exchange(
            MUTEX_UNLOCKED,
            MUTEX_LOCKED,
            Ordering::Acquire,
            Ordering::Relaxed,
        )
        .is_err()
    {
        spin_loop();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_sync_lock_release(lock: *mut usize) {
    let lock = unsafe { atomic_from_storage(lock) };
    lock.store(MUTEX_UNLOCKED, Ordering::Release);
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_sync_lock_free(_lock: *mut usize) {}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_sync_rwlock_read(lock: *mut usize) {
    let lock = unsafe { atomic_from_storage(lock) };
    loop {
        let state = lock.load(Ordering::Acquire);
        if state != RWLOCK_WRITER
            && lock
                .compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            return;
        }
        spin_loop();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_sync_rwlock_read_release(lock: *mut usize) {
    let lock = unsafe { atomic_from_storage(lock) };
    lock.fetch_sub(1, Ordering::Release);
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_sync_rwlock_write(lock: *mut usize) {
    let lock = unsafe { atomic_from_storage(lock) };
    while lock
        .compare_exchange(0, RWLOCK_WRITER, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        spin_loop();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_sync_rwlock_write_release(lock: *mut usize) {
    let lock = unsafe { atomic_from_storage(lock) };
    lock.store(0, Ordering::Release);
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_sync_rwlock_free(_lock: *mut usize) {}
