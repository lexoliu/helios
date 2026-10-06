//! The Wasmtime parking hooks.
//!
//! `memory.atomic.wait`, and so every futex-backed lock and condition
//! variable a wasm thread uses, reaches `wasmtime_parking_wait` after
//! Wasmtime's `ParkingSpot` has queued the waiter and dropped its table
//! lock; `ParkingSpot::notify` calls `wasmtime_parking_unpark` once for
//! every waiter it dequeues, with that lock held.
//!
//! A waiter is a wasm thread: a Wasmtime fiber running inside a kernel
//! task. The wait blocks that fiber through
//! `wasmtime::block_on_current_fiber`, so the task returns `Pending` and
//! its processor runs other tasks until an unpark or the timeout. The hook
//! names no waiter, so an unpark bumps one generation and releases every
//! parked fiber. Each one returns into Wasmtime's loop, which reads its own
//! `notified` flag and parks again when the unpark was for another waiter.
//!
//! The generation a fiber last returned at is kept in its fiber-stack
//! slot. An unpark that lands after Wasmtime dropped its lock and before
//! this hook armed has already moved the generation past that value, so
//! the wait returns at once instead of losing the wake.

use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use futures::future::select;
use helios_hal::cpu::Cpu;
use wasmtime::BlockOnCurrentFiberError;

use crate::{Notify, Timer};

static UNPARK_GENERATION: AtomicU64 = AtomicU64::new(0);
static UNPARKED: Notify = Notify::new();

/// Body of `wasmtime_parking_unpark`. Lock-free and allocation-free, as
/// a call made under Wasmtime's parking-table lock must be.
pub fn unpark_wasmtime_waiters() {
    UNPARK_GENERATION.fetch_add(1, Ordering::AcqRel);
    UNPARKED.notify_all();
}

/// Body of `wasmtime_parking_wait`: gives the processor back until an
/// unpark or until `timeout_nanos` pass. `timer` is the calling
/// processor's timer; `u64::MAX` means no timeout.
pub fn park_wasmtime_waiter<CpuImpl>(timer: &Timer<CpuImpl>, timeout_nanos: u64)
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
{
    if timeout_nanos == 0 {
        return;
    }
    let frame = 0u8;
    let returned_at = crate::memory::fiber_parked_generation(core::ptr::addr_of!(frame) as usize);
    let unparked = UNPARKED.notified();
    let generation = UNPARK_GENERATION.load(Ordering::Acquire);
    if generation != returned_at.load(Ordering::Relaxed) {
        returned_at.store(generation, Ordering::Relaxed);
        return;
    }
    let wait = async move {
        if timeout_nanos == u64::MAX {
            unparked.await;
        } else {
            let timeout = core::pin::pin!(timer.sleep_for(Duration::from_nanos(timeout_nanos)));
            select(unparked, timeout).await;
        }
    };
    // SAFETY: Wasmtime calls this hook from host code on the waiting
    // fiber with its parking-table lock released and no store borrow
    // live across the call, and the future borrows nothing on the fiber.
    match unsafe { wasmtime::block_on_current_fiber(wait) } {
        Ok(()) => {}
        Err(BlockOnCurrentFiberError::NoFiber) => panic!(
            "Wasmtime parking wait outside a Wasmtime fiber: every wasm thread that can wait \
             must run under call_async"
        ),
        Err(BlockOnCurrentFiberError::Unavailable) => panic!(
            "Wasmtime parking wait on a fiber that cannot block: the engine lacks \
             block_on_current_fiber or the fiber's future context is gone"
        ),
        Err(BlockOnCurrentFiberError::Cancelled(_)) => panic!(
            "fiber cancelled inside a Wasmtime parking wait: ParkingSpot's wait loop has no \
             cancellation exit, so the fiber cannot unwind"
        ),
    }
    returned_at.store(UNPARK_GENERATION.load(Ordering::Acquire), Ordering::Relaxed);
}
