//! Reusable nesting-aware critical-section primitive shared by every backend.
//!
//! Each backend supplies architecture-specific interrupt and identity hooks
//! through [`InterruptOps`]; the bookkeeping (owner CAS, nesting depth, token
//! encoding) lives here so the three backends do not duplicate ~50 lines each.

use core::sync::atomic::{AtomicUsize, Ordering, compiler_fence};

const RESTORE_INTERRUPTS_BIT: usize = 1;
const OUTERMOST_BIT: usize = 1 << 1;

/// Backend-specific operations the generic critical section delegates to.
///
/// Implementations must be zero-cost, infallible, and safe for the static
/// lifetime; the [`CriticalSectionState`] uses `I::current_owner` as the CAS
/// key, so it must be unique per processor running concurrent acquires.
pub trait InterruptOps {
    /// Returns `true` if interrupts are currently enabled on this processor.
    fn interrupts_enabled() -> bool;

    /// Disables interrupts on this processor.
    fn disable_interrupts();

    /// Re-enables interrupts on this processor.
    ///
    /// # Safety
    ///
    /// Caller guarantees this only undoes a previous [`disable_interrupts`].
    unsafe fn enable_interrupts();

    /// Identity of the current acquirer.
    ///
    /// Must be unique per processor; the value `0` is reserved as "free".
    fn current_owner() -> usize;
}

/// Shared state for a `critical_section::Impl`.
///
/// Holds the current owner (or `0` when free) and recursive depth. A backend
/// owns one `static` instance and threads it through its `acquire`/`release`
/// hooks via [`CriticalSectionState::acquire`] and
/// [`CriticalSectionState::release`].
pub struct CriticalSectionState {
    owner: AtomicUsize,
    depth: AtomicUsize,
}

impl CriticalSectionState {
    /// Creates a fresh, unowned critical section.
    pub const fn new() -> Self {
        Self {
            owner: AtomicUsize::new(0),
            depth: AtomicUsize::new(0),
        }
    }

    /// Acquires the critical section, blocking on contention via
    /// `core::hint::spin_loop`.
    ///
    /// Returns an opaque token that must be passed back to
    /// [`CriticalSectionState::release`].
    ///
    /// # Safety
    ///
    /// Must be paired with exactly one matching `release` call. The caller
    /// must not invoke this from a context where `I::disable_interrupts`
    /// would corrupt invariants (e.g. from inside the interrupt vector).
    #[inline]
    pub unsafe fn acquire<I: InterruptOps>(&self) -> usize {
        let interrupts_were_enabled = I::interrupts_enabled();
        I::disable_interrupts();
        compiler_fence(Ordering::SeqCst);

        let owner = I::current_owner();
        loop {
            match self
                .owner
                .compare_exchange(0, owner, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => {
                    self.depth.store(1, Ordering::Relaxed);
                    return encode_token(interrupts_were_enabled, true);
                }
                Err(current) if current == owner => {
                    let depth = self.depth.fetch_add(1, Ordering::Relaxed);
                    assert!(depth != usize::MAX, "critical section nesting overflowed");
                    return encode_token(interrupts_were_enabled, false);
                }
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    /// Releases the critical section, paired with a prior [`acquire`].
    ///
    /// # Safety
    ///
    /// `restore_state` must be the token returned by the matching `acquire`
    /// on the same `CriticalSectionState`.
    #[inline]
    pub unsafe fn release<I: InterruptOps>(&self, restore_state: usize) {
        compiler_fence(Ordering::SeqCst);
        let interrupts_were_enabled = decode_restore_interrupts(restore_state);
        let outermost = decode_is_outermost(restore_state);
        let previous_depth = self.depth.fetch_sub(1, Ordering::Relaxed);
        assert!(previous_depth != 0, "critical section depth underflowed");

        if outermost {
            assert!(
                previous_depth == 1,
                "outermost critical section release observed nested depth {previous_depth}"
            );
            self.owner.store(0, Ordering::Release);
        }

        if interrupts_were_enabled {
            unsafe { I::enable_interrupts() };
        }
    }
}

impl Default for CriticalSectionState {
    fn default() -> Self {
        Self::new()
    }
}

const fn encode_token(interrupts_were_enabled: bool, outermost: bool) -> usize {
    (interrupts_were_enabled as usize) | ((outermost as usize) << 1)
}

const fn decode_restore_interrupts(token: usize) -> bool {
    token & RESTORE_INTERRUPTS_BIT != 0
}

const fn decode_is_outermost(token: usize) -> bool {
    token & OUTERMOST_BIT != 0
}
