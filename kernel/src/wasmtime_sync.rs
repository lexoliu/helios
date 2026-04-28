use core::hint::spin_loop;
use core::sync::atomic::{AtomicUsize, Ordering};

const MUTEX_UNLOCKED: usize = 0;
const MUTEX_LOCKED: usize = 1;
const RWLOCK_WRITER: usize = usize::MAX;

/// Iteration cap for kernel-side wasmtime synchronisation primitives.
///
/// Genuine contention on these locks is single-digit-microsecond
/// territory because callers do nothing but flip a flag and continue.
/// A spin that stretches into seconds is always a bug — typically a
/// PTE / cache-coherence mismatch under the `custom-virtual-memory`
/// path or a deadlock in upper-layer wasmtime code. Crashing with a
/// clear panic is strictly better than the silent 4-core CPU burn the
/// previous unbounded-spin form produced; AGENTS §3 demands explicit
/// failure over silent deadlock.
///
/// At 4 GHz, 1 << 32 iterations is ~1 second of pure-CPU work; the
/// real-world ceiling on contention is microseconds, so this cap is
/// six orders of magnitude beyond the legitimate range.
const SYNC_SPIN_LIMIT: u64 = 1u64 << 32;

#[cold]
#[inline(never)]
fn sync_deadlock(primitive: &'static str) -> ! {
    panic!("kernel wasmtime {primitive} spin lock exceeded SYNC_SPIN_LIMIT — deadlock detected");
}

#[inline]
unsafe fn atomic_from_storage(storage: *mut usize) -> &'static AtomicUsize {
    unsafe { &*storage.cast::<AtomicUsize>() }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_sync_lock_acquire(lock: *mut usize) {
    let lock = unsafe { atomic_from_storage(lock) };
    let mut iterations = 0u64;
    while lock
        .compare_exchange(
            MUTEX_UNLOCKED,
            MUTEX_LOCKED,
            Ordering::Acquire,
            Ordering::Relaxed,
        )
        .is_err()
    {
        iterations += 1;
        if iterations >= SYNC_SPIN_LIMIT {
            sync_deadlock("mutex");
        }
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
    let mut iterations = 0u64;
    loop {
        let state = lock.load(Ordering::Acquire);
        if state != RWLOCK_WRITER
            && lock
                .compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            return;
        }
        iterations += 1;
        if iterations >= SYNC_SPIN_LIMIT {
            sync_deadlock("rwlock-read");
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
    let mut iterations = 0u64;
    while lock
        .compare_exchange(0, RWLOCK_WRITER, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        iterations += 1;
        if iterations >= SYNC_SPIN_LIMIT {
            sync_deadlock("rwlock-write");
        }
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
