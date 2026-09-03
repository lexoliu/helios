//! Reusable nesting-aware critical-section primitive shared by every backend.
//!
//! Each backend supplies architecture-specific interrupt and identity hooks
//! through [`InterruptOps`]; the bookkeeping (owner CAS, nesting depth, token
//! encoding) lives here so the three backends do not duplicate ~50 lines each.

use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicUsize, Ordering, compiler_fence};

const RESTORE_INTERRUPTS_BIT: usize = 1;
const OUTERMOST_BIT: usize = 1 << 1;

/// Marks a [`ProcessorIdentity`] that still carries a hardware id rather than
/// the address of an installed per-processor runtime.
const BOOTSTRAPPING_TAG: usize = 1;

/// The identity a processor answers with while it competes for a critical
/// section.
///
/// A processor has an identity from its very first instruction, long before it
/// has built the per-processor runtime whose address later serves as that
/// identity. Both forms share one word: a runtime is at least two-byte aligned,
/// so the low bit distinguishes a tagged hardware id from a runtime address.
/// Every value is non-zero and distinct per processor, which is exactly what
/// [`CriticalSectionState`] needs to tell a nested re-acquire from a second
/// processor contending for the same section.
///
/// Backends keep this word in whichever register is processor-local — `tp` on
/// RISC-V, `tpidr_el1` on AArch64, `IA32_FS_BASE` on x86-64 — or rebuild the
/// bootstrapping form on demand from a hardware id register.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProcessorIdentity(NonZeroUsize);

impl ProcessorIdentity {
    /// The identity of a processor that has not installed its runtime yet,
    /// derived from a hardware id that is unique across processors.
    ///
    /// # Panics
    ///
    /// Panics if `hardware_id` uses the top bit, which the tag displaces.
    pub const fn bootstrapping(hardware_id: usize) -> Self {
        assert!(
            hardware_id <= usize::MAX >> 1,
            "processor hardware id does not fit alongside the bootstrapping tag"
        );
        match NonZeroUsize::new((hardware_id << 1) | BOOTSTRAPPING_TAG) {
            Some(word) => Self(word),
            None => unreachable!(),
        }
    }

    /// The identity of a processor whose runtime lives at `runtime`.
    ///
    /// # Panics
    ///
    /// Panics if `Runtime` is not at least two-byte aligned, because the low
    /// bit would then collide with the bootstrapping tag.
    pub fn installed<Runtime>(runtime: &Runtime) -> Self {
        assert!(
            align_of::<Runtime>().is_multiple_of(2),
            "a per-processor runtime must be at least two-byte aligned"
        );
        let address = core::ptr::from_ref(runtime) as usize;
        Self(NonZeroUsize::new(address).expect("a reference is never null"))
    }

    /// Rebuilds an identity from the word a backend keeps in its
    /// processor-local register.
    pub const fn from_raw(word: NonZeroUsize) -> Self {
        Self(word)
    }

    /// The word to store in the processor-local register.
    pub const fn raw(self) -> usize {
        self.0.get()
    }

    /// The address of the installed per-processor runtime, or `None` while the
    /// processor still carries its bootstrapping identity.
    pub const fn runtime_address(self) -> Option<NonZeroUsize> {
        if self.0.get() & BOOTSTRAPPING_TAG != 0 {
            return None;
        }

        Some(self.0)
    }
}

/// Backend-specific operations the generic critical section delegates to.
///
/// Implementations must be zero-cost, infallible, and safe for the static
/// lifetime; the [`CriticalSectionState`] uses `I::current_identity` as the CAS
/// key, so two processors must never answer with the same value — a shared
/// value makes one processor's acquire look like the other's nested
/// re-acquire and silently voids mutual exclusion.
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
    /// Must be distinct on every processor and available from the processor's
    /// first instruction, which is what [`ProcessorIdentity`] guarantees.
    fn current_identity() -> ProcessorIdentity;
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

        let owner = I::current_identity().raw();
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

#[cfg(test)]
mod tests {
    use super::{CriticalSectionState, InterruptOps, ProcessorIdentity, decode_is_outermost};
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[repr(align(8))]
    struct Runtime(#[allow(dead_code)] usize);

    static FIRST_RUNTIME: Runtime = Runtime(0);
    static SECOND_RUNTIME: Runtime = Runtime(0);

    #[test]
    fn two_bootstrapping_processors_never_share_an_identity() {
        let processors = (0..64).map(ProcessorIdentity::bootstrapping);
        let mut seen = alloc::vec::Vec::new();
        for processor in processors {
            assert!(
                !seen.contains(&processor.raw()),
                "hardware ids collapsed onto one bootstrapping identity"
            );
            seen.push(processor.raw());
        }
    }

    #[test]
    fn a_bootstrapping_identity_never_looks_like_an_installed_runtime() {
        for hardware_id in 0..64 {
            assert_eq!(
                ProcessorIdentity::bootstrapping(hardware_id).runtime_address(),
                None
            );
        }
    }

    #[test]
    fn an_installed_identity_reports_the_runtime_it_was_built_from() {
        let identity = ProcessorIdentity::installed(&FIRST_RUNTIME);
        let address = identity
            .runtime_address()
            .expect("an installed identity carries its runtime address");
        assert_eq!(address.get(), core::ptr::from_ref(&FIRST_RUNTIME) as usize);
        assert_ne!(
            identity,
            ProcessorIdentity::installed(&SECOND_RUNTIME),
            "two runtimes must not share an identity"
        );
    }

    #[test]
    fn a_raw_round_trip_preserves_the_identity() {
        let identity = ProcessorIdentity::bootstrapping(3);
        let word = core::num::NonZeroUsize::new(identity.raw()).expect("identities are non-zero");
        assert_eq!(ProcessorIdentity::from_raw(word), identity);
    }
    /// The identity `TestInterruptOps` answers with, so a test can stand in
    /// for several processors in turn.
    static CURRENT_IDENTITY: AtomicUsize = AtomicUsize::new(0);

    fn run_as(processor: ProcessorIdentity) {
        CURRENT_IDENTITY.store(processor.raw(), Ordering::Relaxed);
    }

    struct TestInterruptOps;

    impl InterruptOps for TestInterruptOps {
        fn interrupts_enabled() -> bool {
            false
        }

        fn disable_interrupts() {}

        unsafe fn enable_interrupts() {}

        fn current_identity() -> ProcessorIdentity {
            let word = core::num::NonZeroUsize::new(CURRENT_IDENTITY.load(Ordering::Relaxed))
                .expect("the test set an identity before acquiring");
            ProcessorIdentity::from_raw(word)
        }
    }

    #[test]
    fn a_processor_re_entering_the_section_is_recognised_as_nested() {
        let state = CriticalSectionState::new();
        run_as(ProcessorIdentity::bootstrapping(0));

        let outer = unsafe { state.acquire::<TestInterruptOps>() };
        assert!(decode_is_outermost(outer));
        let inner = unsafe { state.acquire::<TestInterruptOps>() };
        assert!(
            !decode_is_outermost(inner),
            "a processor re-entering its own section must nest"
        );

        unsafe { state.release::<TestInterruptOps>(inner) };
        unsafe { state.release::<TestInterruptOps>(outer) };
    }

    /// Regression for #62: every bootstrapping processor must take the section
    /// as an outermost owner.
    ///
    /// Before the fix each backend answered with the constant `1` until it
    /// installed its per-processor runtime, so a second processor acquiring in
    /// that window read a failed CAS whose current value equalled its own
    /// owner, took the nested branch, and entered the section alongside the
    /// first. The release path then panicked with "critical section depth
    /// underflowed" — or, worse, did not, and two processors ran inside one
    /// section.
    #[test]
    fn a_second_bootstrapping_processor_is_never_mistaken_for_a_nested_owner() {
        let state = CriticalSectionState::new();

        for hardware_id in 0..8 {
            run_as(ProcessorIdentity::bootstrapping(hardware_id));
            let token = unsafe { state.acquire::<TestInterruptOps>() };
            assert!(
                decode_is_outermost(token),
                "processor {hardware_id} was mistaken for a nested re-acquire                  by whichever processor held the section before it"
            );
            unsafe { state.release::<TestInterruptOps>(token) };
        }
    }

    /// The corruption #62 was made of, kept as a test so the shape is on
    /// record rather than only in a CI log.
    ///
    /// Two processors answering with one identity is not a benign
    /// mis-accounting: the second acquire is read as the first's nested
    /// re-acquire, so both run inside the section at once. Which assertion
    /// notices depends only on the interleaving — this one trips the outermost
    /// release, and the interleaving where the freed owner lets a third
    /// processor restart the depth at 1 trips "critical section depth
    /// underflowed", which is what the riscv64 smoke guest printed.
    #[test]
    #[should_panic(expected = "outermost critical section release observed nested depth 2")]
    fn a_shared_identity_puts_two_processors_inside_one_section() {
        let state = CriticalSectionState::new();
        let shared = ProcessorIdentity::bootstrapping(0);

        run_as(shared);
        let first = unsafe { state.acquire::<TestInterruptOps>() };
        assert!(decode_is_outermost(first));

        // A different processor, indistinguishable because it answers with the
        // same identity. It enters the section the first one still holds.
        let second = unsafe { state.acquire::<TestInterruptOps>() };
        assert!(!decode_is_outermost(second));

        unsafe { state.release::<TestInterruptOps>(first) };
    }

    #[test]
    fn a_processor_that_installed_its_runtime_still_owns_the_section_outright() {
        let state = CriticalSectionState::new();

        run_as(ProcessorIdentity::bootstrapping(1));
        let bootstrapping = unsafe { state.acquire::<TestInterruptOps>() };
        assert!(decode_is_outermost(bootstrapping));
        unsafe { state.release::<TestInterruptOps>(bootstrapping) };

        run_as(ProcessorIdentity::installed(&FIRST_RUNTIME));
        let installed = unsafe { state.acquire::<TestInterruptOps>() };
        assert!(
            decode_is_outermost(installed),
            "installing a runtime must not make a processor look like the              bootstrapping owner that came before it"
        );
        unsafe { state.release::<TestInterruptOps>(installed) };
    }
}
