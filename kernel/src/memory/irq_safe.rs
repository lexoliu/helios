//! The lock an interrupt handler is allowed to take.
//!
//! # Why the kernel's allocators need one
//!
//! Every allocator in this module is reached from two contexts that
//! share one processor: an ordinary task, and the interrupt handler
//! that preempts it. The handler really does allocate. A virtio
//! interrupt ends in [`crate::Notify::notify_all`], which is
//! `event_listener::Event::notify`; that call allocates its shared
//! state the first time it runs, and waking a listener calls
//! `Waker::wake`, whose `async_task` vtable frees the task when the
//! wake drops the last reference to a finished one.
//!
//! A plain spin lock cannot survive that. A task holding the heap lock
//! on processor N, interrupted on N by a handler that allocates, spins
//! on a word only it can clear; every other processor then stops behind
//! the same word at its next allocation. The guest goes silent on every
//! processor with no panic and no console line, which is what #206
//! recorded.
//!
//! # Concurrency contract
//!
//! One spin lock, nothing lock-free, and interrupts masked on the
//! holder's processor for exactly as long as the lock is held. The
//! guarded value is reachable only inside [`IrqSafeMutex::with`], so
//! neither half can be forgotten by a later edit. A guard never crosses
//! an `.await`: `with` takes a plain closure and there is no async
//! form.
//!
//! The two halves answer two different questions, and it matters which
//! is which:
//!
//! - The **spin lock** keeps other processors out. That is the same job
//!   it had before, and it is unchanged.
//! - The **local interrupt mask** keeps this processor's own interrupt
//!   handler from re-entering the lock the processor already holds.
//!   That is the deadlock in #206, and it is a purely local problem, so
//!   it takes a purely local remedy:
//!   [`helios_hal::critical_section::with_local_interrupts_masked`].
//!
//! An earlier version of this fix used `critical_section::with`, which
//! is correct but far too strong: this kernel's critical section is
//! machine-wide — [`helios_hal::critical_section::CriticalSectionState`]
//! holds one `owner` word for the whole machine — so every allocation
//! took a global lock on top of the spin lock and serialised against
//! every unrelated critical section in the tree. It measured 28–38%
//! slower on the allocation- and wake-heavy bench workloads. The local
//! mask fixes the same deadlock and adds no cross-processor
//! serialisation at all.
//!
//! A guarded region runs to completion on its processor and must stay
//! short: no `.await`, no wait on another processor, and no work that
//! is not the lock's own. Growing the kernel heap out of the user pool
//! is the case that shows why — the lend runs between two guarded
//! regions, never inside one.

use helios_hal::critical_section::with_local_interrupts_masked;
use spin::Mutex;

/// A spin lock whose guarded region runs with interrupts masked on the
/// processor that holds it.
///
/// See the module documentation for the contract. `T` is the state the
/// lock protects; the lock is only useful for state an interrupt
/// handler can reach, which in this kernel means the allocators and
/// their bookkeeping.
pub(crate) struct IrqSafeMutex<T> {
    value: Mutex<T>,
}

impl<T> IrqSafeMutex<T> {
    /// Wraps `value` in a lock no interrupt handler can deadlock on.
    pub(crate) const fn new(value: T) -> Self {
        Self {
            value: Mutex::new(value),
        }
    }

    /// Runs `act` with exclusive access to the guarded value, with other
    /// processors held off by the spin lock and this processor's
    /// interrupts masked.
    ///
    /// The mask goes outside the lock, not inside it: taking the lock
    /// first would leave a window in which an interrupt arrives with the
    /// lock already held, which is the deadlock itself.
    pub(crate) fn with<R>(&self, act: impl FnOnce(&mut T) -> R) -> R {
        with_local_interrupts_masked(|| act(&mut self.value.lock()))
    }

    /// Runs `act` when the lock is free, and answers `None` when it is
    /// not, without ever spinning.
    ///
    /// This is the form a page-fault handler is allowed to use. A fault
    /// arrives inside whatever the interrupted code was doing, and that
    /// code may be holding this very lock on this very processor;
    /// [`Self::with`] would then spin on a word only the interrupted
    /// context can clear. Answering `None` lets the handler fall back on
    /// state it owns alone instead of deadlocking against itself.
    ///
    /// The mask goes outside the attempt for the same reason it does in
    /// [`Self::with`]: an interrupt that arrives between taking the lock
    /// and running `act` would find the lock held.
    pub(crate) fn try_with<R>(&self, act: impl FnOnce(&mut T) -> R) -> Option<R> {
        with_local_interrupts_masked(|| self.value.try_lock().map(|mut value| act(&mut value)))
    }
}

#[cfg(test)]
mod tests {
    use super::IrqSafeMutex;
    use helios_hal::critical_section::with_local_interrupts_masked;

    /// Nesting the mask is correct without a depth counter, and the
    /// guarded value stays reachable throughout.
    ///
    /// On a real backend the inner `mask` finds interrupts already
    /// disabled, answers `false`, and its restore leaves them that way,
    /// so only the outermost restore re-enables them. A hosted target
    /// has no interrupts, so what this test can observe is only that
    /// nested regions run and return their values; the ordering of the
    /// hardware flag is not visible here and is the backend's
    /// `InterruptOps` to get right.
    #[test]
    fn a_masked_region_nests_and_returns() {
        let lock = IrqSafeMutex::new(1usize);

        let total = with_local_interrupts_masked(|| {
            let outer = lock.with(|value| {
                *value += 1;
                *value
            });
            let inner = with_local_interrupts_masked(|| lock.with(|value| *value * 10));
            outer + inner
        });

        assert_eq!(total, 2 + 20);
        lock.with(|value| assert_eq!(*value, 2));
    }

    /// The mask is released even when the guarded region unwinds, so a
    /// failing host test cannot leave the processor masked for every
    /// test that runs after it.
    #[test]
    fn a_panicking_region_still_restores_the_mask() {
        let lock = IrqSafeMutex::new(0usize);
        let outcome = std::panic::catch_unwind(|| {
            with_local_interrupts_masked(|| panic!("the guarded region failed"));
        });
        assert!(outcome.is_err());

        // The mask was restored, so the lock is still usable.
        lock.with(|value| *value += 1);
        lock.with(|value| assert_eq!(*value, 1));
    }
}
