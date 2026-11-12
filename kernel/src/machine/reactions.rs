//! What the kernel does when the machine generation advances, or a
//! device that left state in the kernel was reset.
//!
//! A restored machine keeps the memory it had and nothing it learned from
//! the outside since: the calendar is behind by however long it was
//! stopped, the host share's server may never have heard of the session,
//! the DHCP lease may belong to another network. A reset device has
//! forgotten what the kernel gave it: the 9p server its session, the
//! balloon its frames and free-page reports. One kernel task waits on
//! [`MachineGeneration::reactions`] and sets each of those right, in a
//! fixed order:
//!
//! 1. the wall clock is re-read from the platform's real-time clock, when
//!    the generation advanced;
//! 2. the host-share session is reset, when the server lost it (an image
//!    restore, or the 9p device itself was reset), and its caches are
//!    flushed, whenever either happened;
//! 3. the DHCP lease is re-validated, where the interface is
//!    DHCP-configured, when the generation advanced;
//! 4. the memory balloon is resynchronised with its device, only when
//!    that device was reset.
//!
//! The root DRBG is not in this list. It rekeys on its own before it
//! produces a byte ([`crate::RootEntropy`]), which is why none of these
//! steps can observe a stream the restore left shared.
//!
//! # Concurrency contract
//!
//! There is one reactions task per boot, local to the processor that
//! installed it (the bootstrap processor), and it is the single consumer
//! of the generation's reaction [`Notify`](crate::Notify). It runs the
//! steps one after another and never concurrently with itself.
//!
//! The wait is armed before anything is read: `notified()` is created
//! first, an advance or a device reset that lands after that leaves a
//! stored permit, and the task reads [`MachineGeneration::take_unhandled`]
//! only after the wait resolved. One that lands while the steps run
//! therefore wakes the next wait at once, and the set runs again for what
//! is newest. A burst before the task runs is one wake and one pass, and
//! a wake that finds neither a new generation nor a new reset runs
//! nothing.
//!
//! Each step touches only state that is safe to touch from outside its
//! owner: the wall-clock offset is one atomic written by this task alone
//! after bring-up, the host share's session reset takes the client's own
//! session lock, the DHCP step locks the default network shard for the
//! length of one message, and the balloon step is a request to the task
//! that owns the inflated frames ([`crate::BalloonHandle::resynchronise`]),
//! which carries it out at its next turn without this task waiting.

use core::future::Future;
use core::pin::pin;

use helios_hal::cpu::Cpu;
use helios_hal::measured_boot::MeasuredBoot;
use helios_hal::rtc::RealTimeClock;
use helios_hal::watchdog::Watchdog;

use crate::machine::{DeviceResets, Generation, MachineGeneration};
use crate::{ComponentNetworkService, HostFileSystem, HostShareRestore, Kernel, RuntimeState};

/// The steps of one pass, so the loop that orders them is exercised
/// without the devices behind them.
pub(crate) trait ReactionSteps {
    /// Re-reads the real-time clock and moves the wall clock to it.
    fn reread_wall_clock(&mut self) -> impl Future<Output = ()> + '_;

    /// Drops what the host share's session and caches no longer have a
    /// right to.
    fn reset_host_share(&mut self, restore: HostShareRestore) -> impl Future<Output = ()> + '_;

    /// Asks the DHCP server whether the lease is still good.
    fn revalidate_lease(&mut self) -> impl Future<Output = ()> + '_;

    /// Asks the balloon service to bring its view in line with a device
    /// that was reset.
    fn resynchronise_balloon(&mut self) -> impl Future<Output = ()> + '_;
}

/// Spawns the task that reacts to machine-generation advances.
///
/// Called by the backend on the bootstrap processor, after the wall clock
/// is seeded and the machine generation installed. `rtc` is the clock the
/// backend seeded the wall clock from, handed over rather than dropped;
/// a machine without one passes `None`, and its wall clock then reads as
/// uptime before and after a restore. The host share and the
/// network service are read from `runtime_state` when a pass needs them,
/// because they are installed after this task starts and a machine may
/// have neither.
pub fn install_generation_reactions<
    CpuImpl,
    WatchdogImpl,
    MeasuredBootImpl,
    ProgramService,
    NetworkService,
    HostFs,
    Rtc,
>(
    kernel: &Kernel<CpuImpl, WatchdogImpl, MeasuredBootImpl>,
    runtime_state: RuntimeState<ProgramService, NetworkService, HostFs>,
    rtc: Option<Rtc>,
) where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    MeasuredBootImpl: MeasuredBoot,
    ProgramService: Clone + 'static,
    NetworkService: ComponentNetworkService,
    HostFs: HostFileSystem,
    Rtc: RealTimeClock + 'static,
{
    let generation = runtime_state.machine_generation_handle();
    let steps = MachineSteps {
        cpu: kernel.timer().cpu().clone(),
        runtime_state,
        rtc,
    };
    kernel.spawn_local_detached(async move {
        react_forever(&generation, steps).await;
    });
}

/// The production steps, over the runtime state the backend built.
struct MachineSteps<CpuImpl, ProgramService, NetworkService, HostFs, Rtc> {
    cpu: CpuImpl,
    runtime_state: RuntimeState<ProgramService, NetworkService, HostFs>,
    rtc: Option<Rtc>,
}

impl<CpuImpl, ProgramService, NetworkService, HostFs, Rtc> ReactionSteps
    for MachineSteps<CpuImpl, ProgramService, NetworkService, HostFs, Rtc>
where
    CpuImpl: Cpu,
    ProgramService: Clone,
    NetworkService: ComponentNetworkService,
    HostFs: HostFileSystem,
    Rtc: RealTimeClock,
{
    async fn reread_wall_clock(&mut self) {
        let Some(rtc) = &self.rtc else {
            return;
        };
        let wall = RuntimeState::<ProgramService, NetworkService, HostFs>::read_rtc(rtc);
        self.runtime_state
            .reseed_wall_clock(self.cpu.now().ticks(), wall);
        tracing::info!(
            source = Rtc::SOURCE,
            unix_seconds = wall.get(),
            "machine restored: wall clock re-read"
        );
    }

    async fn reset_host_share(&mut self, restore: HostShareRestore) {
        if let Some(host_fs) = self.runtime_state.host_fs_service() {
            host_fs.after_machine_restore(restore).await;
            tracing::info!(?restore, "machine restored: host share reset");
        }
    }

    async fn revalidate_lease(&mut self) {
        if let Some(network) = self.runtime_state.network_service() {
            network.revalidate_lease().await;
        }
    }

    async fn resynchronise_balloon(&mut self) {
        if let Some(balloon) = self.runtime_state.memory_balloon() {
            balloon.resynchronise();
            tracing::info!("memory balloon reset: resynchronisation requested");
        }
    }
}

/// Runs the steps once for every generation the machine reaches and
/// every device reset recorded, until the kernel stops.
pub(crate) async fn react_forever<Steps: ReactionSteps>(
    generation: &MachineGeneration,
    mut steps: Steps,
) {
    let mut handled = Generation::BOOT;
    let mut handled_resets = DeviceResets::BOOT;
    loop {
        // The wait is armed by creating it, before anything is read, so
        // an advance or a reset between the two is a stored permit.
        let woken = pin!(generation.reactions().notified());
        woken.await;
        let pending = generation.take_unhandled();
        let advanced = pending.generation != handled;
        let host_share_reset = pending.device_resets.host_share != handled_resets.host_share;
        let balloon_reset = pending.device_resets.memory_balloon != handled_resets.memory_balloon;
        if !advanced && !host_share_reset && !balloon_reset {
            continue;
        }
        tracing::info!(
            generation = pending.generation.get(),
            advanced,
            image_restored = pending.image_restored,
            host_share_reset,
            balloon_reset,
            "machine generation advanced or a device was reset; running the reactions"
        );
        if advanced {
            steps.reread_wall_clock().await;
        }
        if advanced || host_share_reset {
            steps
                .reset_host_share(if pending.image_restored || host_share_reset {
                    HostShareRestore::SessionLost
                } else {
                    HostShareRestore::ServerKept
                })
                .await;
        }
        if advanced {
            steps.revalidate_lease().await;
        }
        if balloon_reset {
            steps.resynchronise_balloon().await;
        }
        handled = pending.generation;
        handled_resets = pending.device_resets;
    }
}

#[cfg(test)]
mod tests {
    use alloc::rc::Rc;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use super::{ReactionSteps, react_forever};
    use crate::HostShareRestore;
    use crate::machine::{AdvanceCause, MachineGeneration, MachineGenerationHandle, ResetDevice};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Step {
        WallClock,
        HostShare(HostShareRestore),
        Lease,
        Balloon,
    }

    /// Records every step, and advances the generation once from inside
    /// the first wall-clock step when told to.
    struct Recording {
        log: Rc<RefCell<Vec<Step>>>,
        generation: MachineGenerationHandle,
        advance_during_first_pass: Option<AdvanceCause>,
    }

    impl ReactionSteps for Recording {
        fn reread_wall_clock(&mut self) -> impl Future<Output = ()> + '_ {
            self.log.borrow_mut().push(Step::WallClock);
            if let Some(cause) = self.advance_during_first_pass.take() {
                self.generation.advance(cause);
            }
            core::future::ready(())
        }

        fn reset_host_share(&mut self, restore: HostShareRestore) -> impl Future<Output = ()> + '_ {
            self.log.borrow_mut().push(Step::HostShare(restore));
            core::future::ready(())
        }

        fn revalidate_lease(&mut self) -> impl Future<Output = ()> + '_ {
            self.log.borrow_mut().push(Step::Lease);
            core::future::ready(())
        }

        fn resynchronise_balloon(&mut self) -> impl Future<Output = ()> + '_ {
            self.log.borrow_mut().push(Step::Balloon);
            core::future::ready(())
        }
    }

    fn restored(byte: u8) -> AdvanceCause {
        AdvanceCause::ImageRestored {
            image_id: [byte; 16],
        }
    }

    fn recording(
        generation: &MachineGenerationHandle,
        advance_during_first_pass: Option<AdvanceCause>,
    ) -> (Rc<RefCell<Vec<Step>>>, Recording) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let steps = Recording {
            log: log.clone(),
            generation: generation.clone(),
            advance_during_first_pass,
        };
        (log, steps)
    }

    const SERVER_KEPT_PASS: [Step; 3] = [
        Step::WallClock,
        Step::HostShare(HostShareRestore::ServerKept),
        Step::Lease,
    ];
    const SESSION_LOST_PASS: [Step; 3] = [
        Step::WallClock,
        Step::HostShare(HostShareRestore::SessionLost),
        Step::Lease,
    ];

    #[test]
    fn each_advance_runs_the_reactions_once_in_order() {
        let generation = MachineGeneration::new(None);
        let (log, steps) = recording(&generation, None);
        let mut context = Context::from_waker(Waker::noop());
        let mut task = pin!(react_forever(&generation, steps));

        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert!(log.borrow().is_empty(), "nothing runs before an advance");

        generation.advance(restored(1));
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(*log.borrow(), SESSION_LOST_PASS);

        // A poll with nothing new runs nothing.
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(log.borrow().len(), 3);

        generation.advance(AdvanceCause::GenerationId(
            helios_hal::generation::VmGenerationId::from_bytes([2; 16]),
        ));
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(log.borrow().len(), 6);
        assert_eq!(log.borrow()[3..], SERVER_KEPT_PASS);
    }

    #[test]
    fn a_second_advance_during_a_reaction_runs_the_set_again_afterwards() {
        let generation = MachineGeneration::new(None);
        let (log, steps) = recording(&generation, Some(restored(2)));
        let mut context = Context::from_waker(Waker::noop());
        let mut task = pin!(react_forever(&generation, steps));

        generation.advance(restored(1));
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);

        let log = log.borrow();
        assert_eq!(log.len(), 6, "the set ran twice: {log:?}");
        assert_eq!(log[..3], SESSION_LOST_PASS);
        assert_eq!(log[3..], SESSION_LOST_PASS);
        assert_eq!(generation.current().get(), 2);
    }

    #[test]
    fn a_lost_session_is_not_forgotten_when_a_later_advance_has_another_cause() {
        let generation = MachineGeneration::new(None);
        let (log, steps) = recording(&generation, None);
        let mut context = Context::from_waker(Waker::noop());
        let mut task = pin!(react_forever(&generation, steps));

        // Two advances land before the task runs: the restore, then a
        // VMM write. The one pass has to treat the session as lost.
        generation.advance(restored(1));
        generation.advance(AdvanceCause::GenerationId(
            helios_hal::generation::VmGenerationId::from_bytes([3; 16]),
        ));
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(*log.borrow(), SESSION_LOST_PASS);
    }

    /// A 9p device reset is not an advance: the clock and the lease are
    /// as good as they were, and only the session the server forgot is
    /// dropped.
    #[test]
    fn a_host_share_device_reset_drops_the_session_and_nothing_else() {
        let generation = MachineGeneration::new(None);
        let (log, steps) = recording(&generation, None);
        let mut context = Context::from_waker(Waker::noop());
        let mut task = pin!(react_forever(&generation, steps));

        generation.record_device_reset(ResetDevice::HostShare);
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(
            *log.borrow(),
            [Step::HostShare(HostShareRestore::SessionLost)]
        );

        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(log.borrow().len(), 1, "a reset is reacted to once");
        assert_eq!(generation.current().get(), 0, "a reset advances nothing");
    }

    /// A 9p reset that lands with an advance the server survived still
    /// means the session is gone.
    #[test]
    fn a_host_share_device_reset_with_a_kept_server_advance_loses_the_session() {
        let generation = MachineGeneration::new(None);
        let (log, steps) = recording(&generation, None);
        let mut context = Context::from_waker(Waker::noop());
        let mut task = pin!(react_forever(&generation, steps));

        generation.advance(AdvanceCause::GenerationId(
            helios_hal::generation::VmGenerationId::from_bytes([4; 16]),
        ));
        generation.record_device_reset(ResetDevice::HostShare);
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(*log.borrow(), SESSION_LOST_PASS);
    }

    /// The balloon is resynchronised only when its device was reset, and
    /// last: after everything a generation advance asks for.
    #[test]
    fn a_balloon_reset_runs_last_and_only_when_the_balloon_was_reset() {
        let generation = MachineGeneration::new(None);
        let (log, steps) = recording(&generation, None);
        let mut context = Context::from_waker(Waker::noop());
        let mut task = pin!(react_forever(&generation, steps));

        generation.advance(restored(1));
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(
            *log.borrow(),
            SESSION_LOST_PASS,
            "an advance alone leaves the balloon alone"
        );

        // A hibernate resume: every device reset, then the advance.
        generation.record_device_reset(ResetDevice::HostShare);
        generation.record_device_reset(ResetDevice::MemoryBalloon);
        generation.advance(restored(2));
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(
            log.borrow()[3..],
            [
                Step::WallClock,
                Step::HostShare(HostShareRestore::SessionLost),
                Step::Lease,
                Step::Balloon,
            ]
        );

        generation.record_device_reset(ResetDevice::MemoryBalloon);
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(log.borrow()[7..], [Step::Balloon]);
    }

    /// Resets arm the same wait advances do, so one recorded while a pass
    /// runs is reacted to by the next pass.
    #[test]
    fn a_reset_recorded_during_a_pass_runs_its_reaction_afterwards() {
        struct ResettingClock {
            inner: Recording,
        }

        impl ReactionSteps for ResettingClock {
            fn reread_wall_clock(&mut self) -> impl Future<Output = ()> + '_ {
                self.inner
                    .generation
                    .record_device_reset(ResetDevice::MemoryBalloon);
                self.inner.reread_wall_clock()
            }

            fn reset_host_share(
                &mut self,
                restore: HostShareRestore,
            ) -> impl Future<Output = ()> + '_ {
                self.inner.reset_host_share(restore)
            }

            fn revalidate_lease(&mut self) -> impl Future<Output = ()> + '_ {
                self.inner.revalidate_lease()
            }

            fn resynchronise_balloon(&mut self) -> impl Future<Output = ()> + '_ {
                self.inner.resynchronise_balloon()
            }
        }

        let generation = MachineGeneration::new(None);
        let (log, inner) = recording(&generation, None);
        let mut context = Context::from_waker(Waker::noop());
        let mut task = pin!(react_forever(&generation, ResettingClock { inner }));

        generation.advance(restored(1));
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        let log = log.borrow();
        assert_eq!(log[..3], SESSION_LOST_PASS);
        assert_eq!(
            log[3..],
            [Step::Balloon],
            "the pass that took the advance had already read the resets"
        );
    }
}
