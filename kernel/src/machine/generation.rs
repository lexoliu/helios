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
//! - [`spawn_generation_probe`] runs one detached task that sleeps
//!   [`GENERATION_PROBE_INTERVAL`] on the kernel timer between calls to
//!   [`MachineGeneration::observe`]. It takes no lock of its own, never
//!   busy-waits, and exists only on a machine that has a window.
//! - [`MachineGeneration::take_unhandled`] takes the advance lock, reads
//!   the counter and clears the restore memory as one step. It has one
//!   caller, the reactions task.
//! - Two [`Notify`]s are fired on every advance, each with a single
//!   consumer: one for the reactions that follow a restore (wall clock,
//!   host share, network), one for the entropy reseed task. Both are
//!   level-triggered, so a burst of advances is one wake, and the consumer
//!   reads [`MachineGeneration::current`] after waking.
//!
//! # Device resets
//!
//! A device that was reset and brought back by its driver has forgotten
//! state a kernel service keeps elsewhere: the 9p server its session, the
//! balloon the frames it was given. That is not a generation advance —
//! nothing was duplicated, and no random stream needs a rekey — but its
//! consumers are the same reactions task, so the machine generation
//! records it beside the advances.
//!
//! - [`MachineGeneration::record_device_reset`] is one `Release`
//!   increment of that device's reset count, then the reactions
//!   [`Notify`]; it takes no lock and is callable from any processor and
//!   any context. Its caller is whoever re-initialised the device, which
//!   does so on one processor while the machine is frozen.
//! - [`MachineGeneration::device_resets`] is one `Acquire` load per
//!   device: lock-free, for a service that must not talk to a device it
//!   has not yet resynchronised with.
//! - The counts live in their own cache-padded block, written only by a
//!   recording processor, rarely, so a draw's block is never invalidated
//!   by one.
//! - The counts only increase, so a consumer compares them with the ones
//!   it last handled, exactly as it does the generation, and a burst of
//!   resets is one wake and one reaction.

use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use crossbeam_utils::CachePadded;
use helios_hal::cpu::Cpu;
use helios_hal::generation::{GenerationIdWindow, VmGenerationId};
use helios_hal::measured_boot::MeasuredBoot;
use helios_hal::watchdog::Watchdog;
use triomphe::Arc;

use crate::memory::IrqSafeMutex;
use crate::{Kernel, Notify};

/// How long a restore can go unnoticed by the reactions that do not draw
/// entropy (wall clock, host share, network). Entropy needs no probe: it
/// observes the generation on every draw.
pub const GENERATION_PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// Starts the periodic probe, on a machine whose platform published a
/// window. A machine with none has nothing the probe could ever see, so it
/// gets no task.
///
/// # Concurrency contract
///
/// The probe is one detached task on the kernel executor. Between
/// observations it sleeps on the kernel timer, so it never busy-waits and
/// holds no lock across the sleep. Each step is
/// [`MachineGeneration::observe`], which is lock-free while the identifier
/// is unchanged and otherwise takes the advance lock, so the task can run
/// on any processor alongside the entropy draws that observe as well.
pub fn spawn_generation_probe<CpuImpl, WatchdogImpl, MeasuredBootImpl>(
    kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
    generation: &MachineGenerationHandle,
) where
    CpuImpl: Cpu + Clone,
    WatchdogImpl: Watchdog + Clone,
    MeasuredBootImpl: MeasuredBoot,
{
    if !generation.has_window() {
        return;
    }
    let generation = generation.clone();
    let timer = kernel.timer();
    kernel.spawn_detached(async move {
        loop {
            timer.sleep_for(GENERATION_PROBE_INTERVAL).await;
            generation.observe();
        }
    });
}

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

/// A device whose reset a kernel service reacts to beyond its driver.
///
/// Only devices that leave state behind in the kernel are named: every
/// other driver restores what its device lost on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetDevice {
    /// The virtio 9p device carrying the host share. Its server has
    /// forgotten the session and every fid in it.
    HostShare,
    /// The memory balloon. It holds no frame the guest inflated into it
    /// and remembers no free page the guest reported.
    MemoryBalloon,
}

/// How many times each [`ResetDevice`] has been reset since boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceResets {
    pub host_share: u64,
    pub memory_balloon: u64,
}

impl DeviceResets {
    /// The counts a machine boots with.
    pub const BOOT: Self = Self {
        host_share: 0,
        memory_balloon: 0,
    };
}

/// Written by a processor recording a reset, read by the reactions task
/// and the balloon service.
struct DeviceResetWords {
    host_share: AtomicU64,
    memory_balloon: AtomicU64,
}

impl DeviceResetWords {
    fn count(&self, device: ResetDevice) -> &AtomicU64 {
        match device {
            ResetDevice::HostShare => &self.host_share,
            ResetDevice::MemoryBalloon => &self.memory_balloon,
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
    /// Whether an [`AdvanceCause::ImageRestored`] advance has happened
    /// since [`MachineGeneration::take_unhandled`] last cleared it.
    /// Sticky across advances, because the latest cause alone would let
    /// a later advance hide an earlier restore from a consumer that woke
    /// late.
    image_restored_unhandled: bool,
}

/// What a reactions consumer has not yet dealt with, as
/// [`MachineGeneration::take_unhandled`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnhandledAdvances {
    /// The generation the machine is in now.
    pub generation: Generation,
    /// The kernel restored itself from a hibernate image since the last
    /// call, whatever advanced the generation after that.
    pub image_restored: bool,
    /// Every device reset recorded so far.
    pub device_resets: DeviceResets,
}

pub struct MachineGeneration {
    window: Option<GenerationIdWindow>,
    words: CachePadded<GenerationWords>,
    resets: CachePadded<DeviceResetWords>,
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
            resets: CachePadded::new(DeviceResetWords {
                host_share: AtomicU64::new(0),
                memory_balloon: AtomicU64::new(0),
            }),
            advance_lock: IrqSafeMutex::new(LatestAdvance {
                cause: [0; 16],
                image_restored_unhandled: false,
            }),
            reactions: Notify::new(),
            entropy_wake: Notify::new(),
        })
    }

    /// Whether the VMM publishes a generation ID window, which is what
    /// tells a restored copy of the machine that it is one.
    pub fn has_window(&self) -> bool {
        self.window.is_some()
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
            self.bump(
                latest,
                cause.bytes(),
                matches!(cause, AdvanceCause::ImageRestored { .. }),
            );
        });
        self.fire();
    }

    /// The latest generation together with the cause that produced it,
    /// read as one consistent pair.
    pub fn latest_advance(&self) -> (Generation, [u8; 16]) {
        self.advance_lock
            .with(|latest| (self.current(), latest.cause))
    }

    /// The generation now, with what has advanced it since the last call
    /// folded into one answer, and the memory of it cleared.
    ///
    /// The reactions task calls it after every wake. Two advances that
    /// land before it runs are one answer, so nothing an earlier advance
    /// said is lost to the later one.
    pub fn take_unhandled(&self) -> UnhandledAdvances {
        self.advance_lock.with(|latest| UnhandledAdvances {
            generation: self.current(),
            image_restored: core::mem::take(&mut latest.image_restored_unhandled),
            device_resets: self.device_resets(),
        })
    }

    /// Records that `device` was reset and brought back, and wakes the
    /// reactions task.
    ///
    /// Called by whatever re-initialised the device, with the proof its
    /// driver returned, before the machine thaws: a service that reads
    /// [`Self::device_resets`] before it next talks to the device then
    /// sees the reset before anything it sends could reach the device
    /// that forgot.
    pub fn record_device_reset(&self, device: ResetDevice) {
        self.resets.count(device).fetch_add(1, Ordering::Release);
        self.reactions.notify_one_coalesced();
    }

    /// Every device reset recorded so far.
    pub fn device_resets(&self) -> DeviceResets {
        DeviceResets {
            host_share: self.resets.host_share.load(Ordering::Acquire),
            memory_balloon: self.resets.memory_balloon.load(Ordering::Acquire),
        }
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
            self.bump(latest, *id.as_bytes(), false);
            true
        });
        if advanced {
            self.fire();
        }
    }

    /// Records `cause` and moves the counter. The advance lock is held.
    fn bump(&self, latest: &mut LatestAdvance, cause: [u8; 16], image_restored: bool) {
        latest.cause = cause;
        latest.image_restored_unhandled |= image_restored;
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
    fn only_a_machine_with_a_window_has_something_to_probe() {
        assert!(!MachineGeneration::new(None).has_window());
        let (_window, handle) = TestWindow::new(id(1));
        assert!(MachineGeneration::new(Some(handle)).has_window());
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

    /// A device reset is counted and wakes the reactions, but nothing
    /// was duplicated: the generation stays and no random stream rekeys.
    #[test]
    fn a_device_reset_wakes_the_reactions_and_advances_nothing() {
        use core::future::Future;
        use core::pin::pin;
        use core::task::{Context, Poll, Waker};

        use super::{DeviceResets, ResetDevice};

        let generation = MachineGeneration::new(None);
        generation.record_device_reset(ResetDevice::MemoryBalloon);
        generation.record_device_reset(ResetDevice::MemoryBalloon);
        generation.record_device_reset(ResetDevice::HostShare);

        let mut context = Context::from_waker(Waker::noop());
        let mut reactions = pin!(generation.reactions().notified());
        assert_eq!(reactions.as_mut().poll(&mut context), Poll::Ready(()));
        let mut entropy = pin!(generation.entropy_wake().notified());
        assert_eq!(entropy.as_mut().poll(&mut context), Poll::Pending);

        let pending = generation.take_unhandled();
        assert_eq!(pending.generation, Generation::BOOT);
        assert!(!pending.image_restored);
        assert_eq!(
            pending.device_resets,
            DeviceResets {
                host_share: 1,
                memory_balloon: 2,
            }
        );
        assert_eq!(generation.device_resets(), pending.device_resets);
    }
}
