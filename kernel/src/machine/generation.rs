//! The machine generation: a counter that advances whenever the machine's
//! state was duplicated or rolled back underneath the kernel.
//!
//! Two things do that. A virtual-machine monitor that restores a snapshot,
//! or clones the guest, writes a new VM generation ID into the window the
//! platform published ([`helios_hal::generation`]); and the kernel itself
//! restores from a hibernate image. Either way every copy of the machine
//! holds the same memory, and any state that was meant to be unique to one
//! running machine, a random stream above all, is now shared. The
//! generation is how the rest of the kernel finds out.
//!
//! # Concurrency contract
//!
//! - [`MachineGeneration::current`] is one `Acquire` load: lock-free from
//!   any processor and any context.
//! - [`MachineGeneration::observe`] is lock-free while the generation is
//!   unchanged: two volatile loads of the window, when there is one, and a
//!   comparison with the two words of the last identifier seen. On a
//!   mismatch it takes the advance lock, an [`IrqSafeMutex`] because a draw
//!   can happen in any context an allocator can; under it, the window is
//!   read again and compared again, the new identifier is recorded, the
//!   counter is bumped with `Release`, and the notifications are raised.
//! - A torn read of the 16-byte window can only cause one extra advance,
//!   never a missed one: the identifier read is different from the one
//!   seen, so the generation moves, and the next reading, once the VMM's
//!   write is complete, moves it once more. An extra advance costs one
//!   extra rekey.
//! - [`MachineGeneration::advance`] is the same slow path with no window.
//!   Restoring from an image uses it.
//! - The counter and the two words of the identifier last seen share one
//!   cache-padded block. It is written only by the processor holding the
//!   advance lock, rarely, and read by every draw.
//! - Two [`Notify`]s are fired on every advance, each with a single
//!   consumer: one for the reactions that follow a restore (wall clock,
//!   host share, network), one for the entropy reseed task. Both are
//!   level-triggered, so a burst of advances is one wake, and the consumer
//!   reads [`MachineGeneration::current`] after waking.

use core::sync::atomic::{AtomicU64, Ordering};

use crossbeam_utils::CachePadded;
use helios_hal::generation::{GenerationIdWindow, VmGenerationId};
use triomphe::Arc;

use crate::Notify;
use crate::memory::IrqSafeMutex;

/// The machine generation, shared by everything that has to notice a
/// restore. There is one per boot and the backend creates it.
pub type MachineGenerationHandle = Arc<MachineGeneration>;

/// A point in the machine's history. Generations only increase, so two
/// values are enough to tell whether a restore happened between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(u64);

impl Generation {
    /// The generation a machine boots in.
    pub const BOOT: Self = Self(0);

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Why the machine generation advanced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvanceCause {
    /// The VMM wrote a new VM generation ID.
    GenerationId(VmGenerationId),
    /// The kernel restored itself from a hibernate image.
    ImageRestored {
        /// Identifies the image that was restored.
        image_id: [u8; 16],
    },
}

impl AdvanceCause {
    /// The 16 bytes that tell this cause from every other.
    const fn bytes(&self) -> [u8; 16] {
        match self {
            Self::GenerationId(id) => *id.as_bytes(),
            Self::ImageRestored { image_id } => *image_id,
        }
    }
}

/// Written by the processor holding the advance lock, read by every draw.
struct GenerationWords {
    counter: AtomicU64,
    seen_low: AtomicU64,
    seen_high: AtomicU64,
}

/// What the advance lock guards: the cause of the latest advance, kept so
/// a consumer that wakes late can fold it in.
struct LatestAdvance {
    cause: [u8; 16],
}

pub struct MachineGeneration {
    window: Option<GenerationIdWindow>,
    words: CachePadded<GenerationWords>,
    advance_lock: IrqSafeMutex<LatestAdvance>,
    reactions: Notify,
    entropy_wake: Notify,
}

impl MachineGeneration {
    /// Starts the machine in [`Generation::BOOT`], with the identifier
    /// now in `window` as the one already seen.
    ///
    /// A platform whose VMM publishes no window passes `None`, and the
    /// generation then moves only through [`Self::advance`].
    pub fn new(window: Option<GenerationIdWindow>) -> MachineGenerationHandle {
        let [low, high] = window
            .as_ref()
            .map_or([0, 0], |window| window.read().to_words());
        Arc::new(Self {
            window,
            words: CachePadded::new(GenerationWords {
                counter: AtomicU64::new(0),
                seen_low: AtomicU64::new(low),
                seen_high: AtomicU64::new(high),
            }),
            advance_lock: IrqSafeMutex::new(LatestAdvance { cause: [0; 16] }),
            reactions: Notify::new(),
            entropy_wake: Notify::new(),
        })
    }

    /// The generation as of the last observation, without looking at the
    /// window.
    pub fn current(&self) -> Generation {
        Generation(self.words.counter.load(Ordering::Acquire))
    }

    /// Compares the window with the identifier last seen, advances if the
    /// VMM wrote a new one, and returns the generation.
    ///
    /// Every consumer calls this before it produces anything that must
    /// not be shared between two copies of the machine.
    pub fn observe(&self) -> Generation {
        if let Some(window) = &self.window {
            let [low, high] = window.read().to_words();
            if low != self.words.seen_low.load(Ordering::Acquire)
                || high != self.words.seen_high.load(Ordering::Acquire)
            {
                self.advance_for_window(window);
            }
        }
        self.current()
    }

    /// Advances the generation because of `cause`.
    ///
    /// A [`AdvanceCause::GenerationId`] is also recorded as the
    /// identifier seen, so the window showing it afterwards does not
    /// advance the generation a second time.
    pub fn advance(&self, cause: AdvanceCause) {
        self.advance_lock.with(|latest| {
            if let AdvanceCause::GenerationId(id) = cause {
                let [low, high] = id.to_words();
                self.words.seen_low.store(low, Ordering::Release);
                self.words.seen_high.store(high, Ordering::Release);
            }
            self.bump(latest, cause.bytes());
        });
        self.fire();
    }

    /// The latest generation together with the cause that produced it,
    /// read as one consistent pair.
    pub fn latest_advance(&self) -> (Generation, [u8; 16]) {
        self.advance_lock
            .with(|latest| (self.current(), latest.cause))
    }

    /// Completes when the machine generation has advanced since the
    /// reactions were last handled. The single consumer is the task that
    /// carries out the reactions to a restore.
    pub fn reactions(&self) -> &Notify {
        &self.reactions
    }

    /// Fired on every advance for the entropy reseed task, which takes
    /// fresh material at once instead of at its next interval.
    pub(crate) fn entropy_wake(&self) -> &Notify {
        &self.entropy_wake
    }

    fn advance_for_window(&self, window: &GenerationIdWindow) {
        let advanced = self.advance_lock.with(|latest| {
            let id = window.read();
            let [low, high] = id.to_words();
            if low == self.words.seen_low.load(Ordering::Acquire)
                && high == self.words.seen_high.load(Ordering::Acquire)
            {
                // Another processor saw the same write first.
                return false;
            }
            self.words.seen_low.store(low, Ordering::Release);
            self.words.seen_high.store(high, Ordering::Release);
            self.bump(latest, *id.as_bytes());
            true
        });
        if advanced {
            self.fire();
        }
    }

    /// Records `cause` and moves the counter. The advance lock is held.
    fn bump(&self, latest: &mut LatestAdvance, cause: [u8; 16]) {
        latest.cause = cause;
        self.words.counter.fetch_add(1, Ordering::Release);
    }

    fn fire(&self) {
        self.reactions.notify_one_coalesced();
        self.entropy_wake.notify_one_coalesced();
    }
}

#[cfg(test)]
pub(crate) mod test_window {
    use alloc::boxed::Box;
    use core::ptr::NonNull;
    use core::sync::atomic::{AtomicU64, Ordering};

    use helios_hal::generation::{GenerationIdWindow, VmGenerationId};

    /// A generation window the test writes as a VMM would.
    pub(crate) struct TestWindow {
        words: &'static [AtomicU64; 2],
    }

    impl TestWindow {
        pub(crate) fn new(id: VmGenerationId) -> (Self, GenerationIdWindow) {
            let [low, high] = id.to_words();
            let words: &'static [AtomicU64; 2] =
                Box::leak(Box::new([AtomicU64::new(low), AtomicU64::new(high)]));
            let ptr = NonNull::from(&words[0]).cast::<u64>();
            // SAFETY: the leaked array is 16 bytes aligned to 8 and lives for
            // the rest of the process; `AtomicU64` has the layout of `u64`.
            let window = unsafe { GenerationIdWindow::new(ptr) };
            (Self { words }, window)
        }

        pub(crate) fn write(&self, id: VmGenerationId) {
            let [low, high] = id.to_words();
            self.words[0].store(low, Ordering::SeqCst);
            self.words[1].store(high, Ordering::SeqCst);
        }

        /// Writes only the low half, as a VMM write seen half-done.
        pub(crate) fn write_low(&self, low: u64) {
            self.words[0].store(low, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use helios_hal::generation::VmGenerationId;

    use super::test_window::TestWindow;
    use super::{AdvanceCause, Generation, MachineGeneration};

    fn id(byte: u8) -> VmGenerationId {
        VmGenerationId::from_bytes([byte; 16])
    }

    #[test]
    fn a_machine_without_a_window_never_advances_by_observation() {
        let generation = MachineGeneration::new(None);
        for _ in 0..4 {
            assert_eq!(generation.observe(), Generation::BOOT);
        }
    }

    #[test]
    fn a_new_identifier_advances_once() {
        let (window, handle) = TestWindow::new(id(1));
        let generation = MachineGeneration::new(Some(handle));
        assert_eq!(generation.observe(), Generation::BOOT);

        window.write(id(2));
        let advanced = generation.observe();
        assert_eq!(advanced.get(), 1);
        assert_eq!(generation.observe(), advanced);
        assert_eq!(generation.latest_advance(), (advanced, [2; 16]));
    }

    #[test]
    fn a_torn_window_advances_once_and_then_stays_stable() {
        let (window, handle) = TestWindow::new(id(1));
        let generation = MachineGeneration::new(Some(handle));
        let seen_low = u64::from_le_bytes([1; 8]);

        window.write_low(seen_low ^ 0xff);
        let torn = generation.observe();
        assert_eq!(torn.get(), 1);
        assert_eq!(generation.observe(), torn, "a stable window is stable");
    }

    #[test]
    fn an_image_restore_advances_and_records_its_cause() {
        let generation = MachineGeneration::new(None);
        generation.advance(AdvanceCause::ImageRestored { image_id: [9; 16] });
        assert_eq!(generation.current().get(), 1);
        assert_eq!(generation.latest_advance(), (generation.current(), [9; 16]));
    }

    #[test]
    fn advancing_with_the_identifier_the_window_shows_does_not_advance_twice() {
        let (window, handle) = TestWindow::new(id(1));
        let generation = MachineGeneration::new(Some(handle));
        window.write(id(3));
        generation.advance(AdvanceCause::GenerationId(id(3)));
        assert_eq!(generation.observe().get(), 1);
    }

    #[test]
    fn every_advance_leaves_a_wake_for_each_consumer() {
        use core::future::Future;
        use core::pin::pin;
        use core::task::{Context, Poll, Waker};

        let generation = MachineGeneration::new(None);
        generation.advance(AdvanceCause::ImageRestored { image_id: [1; 16] });

        let mut context = Context::from_waker(Waker::noop());
        let mut reactions = pin!(generation.reactions().notified());
        assert_eq!(reactions.as_mut().poll(&mut context), Poll::Ready(()));
        let mut entropy = pin!(generation.entropy_wake().notified());
        assert_eq!(entropy.as_mut().poll(&mut context), Poll::Ready(()));
    }
}
