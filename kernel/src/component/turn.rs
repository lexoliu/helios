//! The boundary at which a store gains and loses a processor.
//!
//! A guest's processor time, the user-memory owner of the processor it
//! runs on, its `user;` profile samples, and the closes its dead
//! sockets queued all change hands where the store does: when the
//! executor polls the future that drives the store's guest code, inside
//! which the runtime resumes the guest's fiber, and when that poll
//! returns because the fiber suspended or finished. [`OnProcessor`] is
//! that future's wrapper, so a synchronous host call, which neither
//! suspends the fiber nor leaves the poll, does no accounting at all.
//!
//! It is also where a stop reaches a guest that is not running. A guest
//! suspended in a host future meets no interrupt check, and the future may
//! never resolve: a sleep, a socket read with no peer, a channel nobody
//! sends on. So while the call is suspended its turn waits on the
//! instance's condemnation and on the store's own stop beside the call,
//! and when either is raised it drops the call, which is how Wasmtime
//! cancels a suspended fiber: the fiber is resumed only to unwind, its
//! host future is dropped on the way, and its stack goes back to the
//! pool.
//!
//! The wrapper goes around the outermost call that drives a store, never
//! around a call a host function makes back into its own guest: the
//! outer turn already holds the processor for it, and a nested turn of
//! the same store is refused (see [`crate::StoreTurns::begin`]).

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use helios_hal::cpu::Cpu;
use pin_project_lite::pin_project;

use super::{ComponentRuntimeState, InstanceKilled, SocketRetirementQueue};
use crate::{NotifyWaiter, ProfileScope, StoreTurns};

/// How a store's own stop, one that is not its instance's condemnation,
/// wakes a turn whose guest is suspended.
///
/// # Concurrency
///
/// The stop may be raised from any processor while the turn that waits
/// for it is parked on another, so raising it is a broadcast that stores
/// nothing. The turn arms its wait before it reads the stop
/// ([`TurnEpilogue::take_stop`]), so a stop raised between the read and
/// the park still completes the wait.
pub trait StopWake {
    /// One turn's armed wait.
    type Wait;

    /// Arm a wait that completes on a stop raised from this point on.
    fn arm_stop_wait(&self) -> Self::Wait;

    /// Poll a wait armed by [`Self::arm_stop_wait`]. `Ready` means a
    /// stop may have been raised since the wait was armed, and re-arms
    /// the wait.
    fn poll_stop_wait(&self, cx: &mut Context<'_>, wait: &mut Self::Wait) -> Poll<()>;
}

/// What a store does when one of its turns ends, and when the call it
/// is driving completes or is stopped, beyond the accounting every
/// store does.
pub trait TurnEpilogue<RuntimeStateImpl, Error>: StopWake {
    fn end_turn(&self, runtime_state: &RuntimeStateImpl);

    /// The stop the store owes its guest, if any, taken so it is
    /// delivered once. It is read before the call is resumed, after
    /// each wake of a suspended call, and when the call completes, and
    /// it ends the call ahead of a condemnation and ahead of whatever
    /// the call returned, as an event the guest met on its way back to
    /// the host would have.
    fn take_stop(&self) -> Option<Error>;
}

/// A store with no stop of its own: its wait never completes.
impl StopWake for () {
    type Wait = ();

    fn arm_stop_wait(&self) {}

    fn poll_stop_wait(&self, _: &mut Context<'_>, (): &mut ()) -> Poll<()> {
        Poll::Pending
    }
}

/// A store with nothing of its own to do at the end of a turn.
impl<RuntimeStateImpl, Error> TurnEpilogue<RuntimeStateImpl, Error> for () {
    fn end_turn(&self, _: &RuntimeStateImpl) {}

    fn take_stop(&self) -> Option<Error> {
        None
    }
}

/// A component store has no stop of its own: only its instance's
/// condemnation ends a suspended call.
impl StopWake for SocketRetirementQueue {
    type Wait = ();

    fn arm_stop_wait(&self) {}

    fn poll_stop_wait(&self, _: &mut Context<'_>, (): &mut ()) -> Poll<()> {
        Poll::Pending
    }
}

/// A component store closes the sockets its guest let go of during the
/// turn. Draining when the store gives up the processor bounds the
/// delay between a socket's death and its close by one turn: the next
/// time the guest blocks, yields at the end of a time slice, or finishes.
impl<RuntimeStateImpl, Error> TurnEpilogue<RuntimeStateImpl, Error> for SocketRetirementQueue
where
    RuntimeStateImpl: ComponentRuntimeState,
{
    fn end_turn(&self, runtime_state: &RuntimeStateImpl) {
        if !self.is_empty() {
            runtime_state.retire_network_handles(self);
        }
    }

    fn take_stop(&self) -> Option<Error> {
        None
    }
}

/// Everything the driver of one store needs to account its turns,
/// taken from the store before the store is borrowed for the call.
pub struct GuestTurns<CpuImpl, RuntimeStateImpl, Epilogue> {
    turns: StoreTurns,
    cpu: CpuImpl,
    runtime_state: RuntimeStateImpl,
    epilogue: Epilogue,
}

impl<CpuImpl, RuntimeStateImpl, Epilogue> GuestTurns<CpuImpl, RuntimeStateImpl, Epilogue>
where
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
    Epilogue: StopWake,
{
    pub fn new(
        turns: StoreTurns,
        cpu: CpuImpl,
        runtime_state: RuntimeStateImpl,
        epilogue: Epilogue,
    ) -> Self {
        Self {
            turns,
            cpu,
            runtime_state,
            epilogue,
        }
    }

    /// Drive `call`, the outermost call into the store, one turn per
    /// poll.
    ///
    /// The waits for a condemnation and for the store's own stop are
    /// armed here, before anything reads either, so a stop raised at
    /// any point after this call reaches the call.
    pub fn drive<Call>(self, call: Call) -> OnProcessor<Call, CpuImpl, RuntimeStateImpl, Epilogue> {
        OnProcessor {
            kill_wait: self.turns.arm_kill_wait(),
            stop_wait: self.epilogue.arm_stop_wait(),
            call: Some(call),
            turns: self,
        }
    }

    fn now_nanos(&self) -> u64 {
        self.runtime_state.uptime_nanos(self.cpu.now().ticks())
    }

    /// Run `work` as one turn of the store on the executing processor.
    fn on_processor<Error, Output>(&self, work: impl FnOnce() -> Output) -> Output
    where
        Epilogue: TurnEpilogue<RuntimeStateImpl, Error>,
    {
        let turn = self.turns.begin(self.now_nanos());
        let output = work();
        let elapsed = turn.end(self.now_nanos());
        if self.runtime_state.profiling_enabled() {
            self.runtime_state.record_profile_stack_parts_nanos(
                ProfileScope::User,
                "user;",
                self.turns.instance().name(),
                elapsed,
            );
        }
        self.epilogue.end_turn(&self.runtime_state);
        output
    }

    /// The stop the call owes its guest now, if any: the store's own
    /// first, then its instance's condemnation.
    fn stop<Error>(&self) -> Option<Error>
    where
        Error: From<InstanceKilled>,
        Epilogue: TurnEpilogue<RuntimeStateImpl, Error>,
    {
        self.epilogue.take_stop().or_else(|| {
            self.turns
                .pending_kill()
                .map(|reason| Error::from(InstanceKilled { reason }))
        })
    }
}

pin_project! {
    /// A call into a store's guest, accounted one processor turn per
    /// poll.
    ///
    /// A call ends at the first of three things: its own completion, the
    /// store's own stop ([`TurnEpilogue::take_stop`], a pending signal,
    /// for one), and its instance's condemnation. Those two stops are
    /// read before the call is resumed, after every wake while it is
    /// suspended, and when it completes, in that precedence, and a stop
    /// ends the call ahead of its result.
    ///
    /// A guest that is running meets both stops at its next interrupt
    /// check, which raising either makes immediate. A guest suspended in a host
    /// future meets none, so the turn waits for both beside the call,
    /// and a stop raised while the call is suspended drops the call in a
    /// turn of its own: Wasmtime resumes the fiber only so it can
    /// unwind, and the unwinding is the instance's processor time and
    /// memory. A call that returns to the host before it reaches an
    /// interrupt check, its last host call having been where the stop
    /// arrived, meets it where it completes.
    ///
    /// # Concurrency
    ///
    /// One task on one processor polls this at a time. Stops are raised
    /// from any processor, as broadcasts that store nothing; the waits
    /// for them are armed in [`GuestTurns::drive`] and re-armed by every
    /// wake, each time before the stops are read again.
    pub struct OnProcessor<Call, CpuImpl, RuntimeStateImpl, Epilogue>
    where
        Epilogue: StopWake,
    {
        #[pin]
        call: Option<Call>,
        turns: GuestTurns<CpuImpl, RuntimeStateImpl, Epilogue>,
        kill_wait: NotifyWaiter,
        stop_wait: Epilogue::Wait,
    }
}

impl<Call, Value, Error, CpuImpl, RuntimeStateImpl, Epilogue> Future
    for OnProcessor<Call, CpuImpl, RuntimeStateImpl, Epilogue>
where
    Call: Future<Output = Result<Value, Error>>,
    Error: From<InstanceKilled>,
    CpuImpl: Cpu + Clone,
    RuntimeStateImpl: ComponentRuntimeState,
    Epilogue: TurnEpilogue<RuntimeStateImpl, Error>,
{
    type Output = Result<Value, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let turns = &*this.turns;
        // A stop raised since the last poll ends the call without
        // resuming it: the guest would only meet it at its next
        // interrupt check, and a host future that has not resolved may never let
        // it get there.
        if let Some(stop) = turns.stop() {
            turns.on_processor::<Error, _>(|| this.call.set(None));
            return Poll::Ready(Err(stop));
        }
        let call = this
            .call
            .as_mut()
            .as_pin_mut()
            .expect("a guest call was polled after it ended");
        let polled = turns.on_processor::<Error, _>(|| call.poll(cx));
        if let Poll::Ready(result) = polled {
            this.call.set(None);
            return Poll::Ready(match turns.stop() {
                Some(stop) => Err(stop),
                None => result,
            });
        }
        // Suspended. Both waits were armed before the stops were read
        // above, so a stop raised since then completes one of them here.
        loop {
            let killed = turns.turns.poll_kill_wait(cx, this.kill_wait);
            let stopped = turns.epilogue.poll_stop_wait(cx, this.stop_wait);
            if killed.is_pending() && stopped.is_pending() {
                return Poll::Pending;
            }
            if let Some(stop) = turns.stop() {
                turns.on_processor::<Error, _>(|| this.call.set(None));
                return Poll::Ready(Err(stop));
            }
        }
    }
}

#[cfg(all(test, feature = "wasmtime-runtime"))]
mod tests {
    use alloc::boxed::Box;
    use core::future::{Future, poll_fn};
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use helios_hal::cpu::{Cpu, current_processor};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::GuestTurns;
    use crate::component::{InstanceKilled, RetiredNetworkHandle, StoreSocketRetirement};
    use crate::test_support::{ManualClockCpu, TestCpu, TestNetworkService, TestRuntimeState};
    use crate::wasmtime_adapter::engine::build_component_engine_for_platform;
    use crate::{
        InstanceActivity, InstanceRegistry, KillOutcome, KillReason, MemoryOwner,
        configure_user_memory_owner_processors, current_user_memory_owner,
    };

    fn poll_once<Call: Future>(call: core::pin::Pin<&mut Call>) -> Poll<Call::Output> {
        call.poll(&mut Context::from_waker(Waker::noop()))
    }

    /// A host call that does not suspend is part of the turn around it;
    /// the time the fiber spends suspended is no turn at all.
    #[test]
    fn a_turn_counts_its_host_calls_and_not_its_suspensions() {
        let registry = InstanceRegistry::new();
        let instance = registry.register("hostcall-loop", 0);
        let activity = InstanceActivity::new(instance.clone(), 0);
        let cpu = ManualClockCpu::new();
        let guest_cpu = cpu.clone();
        let mut polls = 0;
        let call = poll_fn(move |_| {
            polls += 1;
            match polls {
                1 => {
                    // Guest code, then a synchronous host call, then
                    // guest code again, then a host call that suspends.
                    guest_cpu.advance(10);
                    guest_cpu.advance(25);
                    guest_cpu.advance(5);
                    Poll::Pending
                }
                _ => {
                    guest_cpu.advance(7);
                    Poll::Ready(Ok::<u32, InstanceKilled>(3))
                }
            }
        });
        let turns = GuestTurns::new(
            activity.turns(),
            cpu.clone(),
            TestRuntimeState::default(),
            (),
        );
        let mut driven = pin!(turns.drive(call));

        assert!(poll_once(driven.as_mut()).is_pending());
        assert_eq!(
            registry.active_totals(cpu.now().ticks())[0].active_nanos,
            40
        );
        assert_eq!(
            registry.idle_instances(cpu.now().ticks(), 0),
            [instance.id()],
            "a suspended guest is off the processor"
        );

        // Suspended: the clock moves, the instance's time does not.
        cpu.advance(1_000);
        assert_eq!(
            registry.active_totals(cpu.now().ticks())[0].active_nanos,
            40
        );

        assert_eq!(poll_once(driven.as_mut()), Poll::Ready(Ok(3)));
        assert_eq!(
            registry.active_totals(cpu.now().ticks())[0].active_nanos,
            47
        );
    }

    /// The processor's user-memory owner is the instance for the whole
    /// turn, synchronous host calls included, and whatever it was before
    /// once the turn ends. A turn of another store inside it — a host
    /// call that instantiates a child — names the child and hands the
    /// processor back to the parent.
    #[test]
    fn a_turn_names_its_instance_as_the_memory_owner() {
        configure_user_memory_owner_processors(1);
        let processor = current_processor();
        let registry = InstanceRegistry::new();
        let parent = registry.register("parent", 0);
        let child = registry.register("child", 0);
        let parent_activity = InstanceActivity::new(parent.clone(), 0);
        let child_activity = InstanceActivity::new(child.clone(), 0);
        let cpu = ManualClockCpu::new();
        let parent_owner = MemoryOwner::new(parent.id().raw());
        let child_owner = MemoryOwner::new(child.id().raw());

        let child_turns = GuestTurns::new(
            child_activity.turns(),
            cpu.clone(),
            TestRuntimeState::default(),
            (),
        );
        let mut child_turns = Some(child_turns);
        let call = poll_fn(move |_| {
            assert_eq!(current_user_memory_owner(processor), parent_owner);
            let child_call = poll_fn(|_| {
                assert_eq!(current_user_memory_owner(processor), child_owner);
                Poll::Ready(Ok::<(), InstanceKilled>(()))
            });
            let child_turns = child_turns.take().expect("the child is instantiated once");
            let mut child_call = pin!(child_turns.drive(child_call));
            assert_eq!(poll_once(child_call.as_mut()), Poll::Ready(Ok(())));
            assert_eq!(current_user_memory_owner(processor), parent_owner);
            Poll::Ready(Ok::<(), InstanceKilled>(()))
        });
        let turns = GuestTurns::new(
            parent_activity.turns(),
            cpu.clone(),
            TestRuntimeState::default(),
            (),
        );
        assert_eq!(current_user_memory_owner(processor), MemoryOwner::NONE);
        let mut driven = pin!(turns.drive(call));
        assert_eq!(poll_once(driven.as_mut()), Poll::Ready(Ok(())));
        assert_eq!(current_user_memory_owner(processor), MemoryOwner::NONE);
    }

    /// A kill that lands while the guest is suspended in a host call is
    /// reported when that call completes, even if the guest returns to
    /// the host without reaching another interrupt check.
    #[test]
    fn a_kill_during_a_suspended_host_call_ends_the_call_killed() {
        let registry = InstanceRegistry::new();
        let instance = registry.register("victim", 0);
        let activity = InstanceActivity::new(instance.clone(), 0);
        let cpu = ManualClockCpu::new();
        let mut polls = 0;
        let call = poll_fn(move |_| {
            polls += 1;
            if polls == 1 {
                Poll::Pending
            } else {
                Poll::Ready(Ok::<u32, InstanceKilled>(0))
            }
        });
        let turns = GuestTurns::new(
            activity.turns(),
            cpu.clone(),
            TestRuntimeState::default(),
            (),
        );
        let mut driven = pin!(turns.drive(call));

        assert!(poll_once(driven.as_mut()).is_pending());
        assert_eq!(
            registry.request_kill(instance.id(), KillReason::Operator, 5),
            KillOutcome::Requested
        );
        assert_eq!(
            poll_once(driven.as_mut()),
            Poll::Ready(Err(InstanceKilled {
                reason: KillReason::Operator
            }))
        );
        assert_eq!(
            registry.idle_instances(10, 0),
            [],
            "a condemned instance is not a swap candidate"
        );
    }

    /// A kill that lands while the guest is in a synchronous host call,
    /// after which the export returns without another interrupt check, ends
    /// the call killed: the turn meets the condemnation where the call
    /// completes, where the call hook used to meet it on the way back
    /// from the host call.
    #[test]
    fn a_kill_during_the_last_synchronous_host_call_ends_the_call_killed() {
        let registry = InstanceRegistry::new();
        let instance = registry.register("victim", 0);
        let activity = InstanceActivity::new(instance.clone(), 0);
        let condemner = registry.clone();
        let call = poll_fn(move |_| {
            assert_eq!(
                condemner.request_kill(instance.id(), KillReason::OutOfMemory, 0),
                KillOutcome::Requested
            );
            Poll::Ready(Ok::<u32, InstanceKilled>(0))
        });
        let turns = GuestTurns::new(
            activity.turns(),
            ManualClockCpu::new(),
            TestRuntimeState::default(),
            (),
        );
        let mut driven = pin!(turns.drive(call));

        assert_eq!(
            poll_once(driven.as_mut()),
            Poll::Ready(Err(InstanceKilled {
                reason: KillReason::OutOfMemory
            }))
        );
    }

    /// A socket that dies during a turn is closed when the turn ends,
    /// without waiting for the guest's next host call.
    #[test]
    fn a_turn_retires_the_sockets_that_died_in_it() {
        let registry = InstanceRegistry::new();
        let instance = registry.register("client", 0);
        let activity = InstanceActivity::new(instance, 0);
        let service = TestNetworkService::new();
        let closed = service.closed();
        let retirement =
            StoreSocketRetirement::new(TestRuntimeState::with_network(service.clone()));
        let sender = retirement.sender();
        let cpu = ManualClockCpu::new();
        let call = poll_fn(move |_| {
            sender.push(RetiredNetworkHandle::TcpStream(11));
            Poll::<Result<(), InstanceKilled>>::Pending
        });
        let turns = GuestTurns::new(
            activity.turns(),
            cpu,
            TestRuntimeState::with_network(service),
            retirement.turn_queue(),
        );
        let mut driven = pin!(turns.drive(call));

        assert!(poll_once(driven.as_mut()).is_pending());
        assert_eq!(closed.count(), 1);
        assert_eq!(closed.last(), 11);
    }

    /// Counts the wakes a turn receives.
    #[derive(Default)]
    struct CountingWaker {
        wakes: AtomicUsize,
    }

    impl CountingWaker {
        fn wakes(&self) -> usize {
            self.wakes.load(Ordering::Acquire)
        }
    }

    impl std::task::Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Records that the value holding it was dropped.
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    /// A guest suspended in a host future that never resolves is woken
    /// by its instance's condemnation, and the turn ends the call with
    /// the kill and drops it, rather than waiting for a completion that
    /// never comes.
    #[test]
    fn a_kill_ends_a_call_suspended_in_a_host_future_that_never_resolves() {
        let registry = InstanceRegistry::new();
        let instance = registry.register("parked", 0);
        let activity = InstanceActivity::new(instance.clone(), 0);
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(dropped.clone());
        let call = async move {
            let _guard = guard;
            core::future::pending::<Result<u32, InstanceKilled>>().await
        };
        let cpu = ManualClockCpu::new();
        let turns = GuestTurns::new(
            activity.turns(),
            cpu.clone(),
            TestRuntimeState::default(),
            (),
        );
        let wakes = Arc::new(CountingWaker::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let mut driven = pin!(turns.drive(call));

        assert!(driven.as_mut().poll(&mut cx).is_pending());
        assert_eq!(wakes.wakes(), 0);
        assert!(!dropped.load(Ordering::Acquire));

        assert_eq!(
            registry.request_kill(instance.id(), KillReason::OutOfMemory, 5),
            KillOutcome::Requested
        );
        assert_eq!(wakes.wakes(), 1, "the condemnation wakes the turn");
        assert_eq!(
            driven.as_mut().poll(&mut cx),
            Poll::Ready(Err(InstanceKilled {
                reason: KillReason::OutOfMemory
            }))
        );
        assert!(
            dropped.load(Ordering::Acquire),
            "the call is dropped when the kill ends it"
        );
        assert_eq!(
            registry.idle_instances(cpu.now().ticks(), 0),
            [],
            "the turn that dropped the call left the processor"
        );
    }

    /// An instance condemned before its call is first polled never runs
    /// the call: the turn ends it with the kill and drops it unpolled.
    #[test]
    fn a_call_of_a_condemned_instance_ends_before_it_runs() {
        let registry = InstanceRegistry::new();
        let instance = registry.register("condemned", 0);
        let activity = InstanceActivity::new(instance.clone(), 0);
        assert_eq!(
            registry.request_kill(instance.id(), KillReason::SupervisorRestart, 0),
            KillOutcome::Requested
        );
        let call = poll_fn(|_| -> Poll<Result<u32, InstanceKilled>> {
            panic!("a condemned instance's call is not resumed")
        });
        let turns = GuestTurns::new(
            activity.turns(),
            ManualClockCpu::new(),
            TestRuntimeState::default(),
            (),
        );
        let mut driven = pin!(turns.drive(call));

        assert_eq!(
            poll_once(driven.as_mut()),
            Poll::Ready(Err(InstanceKilled {
                reason: KillReason::SupervisorRestart
            }))
        );
    }

    /// The same, through Wasmtime: a guest call suspended in an async
    /// host function whose future never resolves is ended by a kill.
    /// Dropping the call is Wasmtime's cancellation: the fiber is resumed
    /// only to unwind, the host future is dropped on it, and the fiber's
    /// stack, the store and the instance's registry entry all go back
    /// with their owners.
    #[test]
    fn a_kill_unwinds_a_fiber_suspended_in_a_host_future_that_never_resolves() {
        let engine = build_component_engine_for_platform(&TestCpu::without_entropy())
            .expect("the component engine builds on the test platform");
        let live_stacks = || {
            engine
                .pooling_allocator_metrics()
                .expect("the component engine uses the pooling allocator")
                .stacks()
        };
        let registry = InstanceRegistry::new();
        let instance = registry.register("parked-fiber", 0);
        let activity = InstanceActivity::new(instance.clone(), 0);
        let turns = GuestTurns::new(
            activity.turns(),
            ManualClockCpu::new(),
            TestRuntimeState::default(),
            (),
        );
        let mut store = wasmtime::Store::new(&engine, activity);
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = dropped.clone();
        let parked = wasmtime::Func::wrap_async(
            &mut store,
            move |_: wasmtime::Caller<'_, InstanceActivity>, (): ()| {
                let guard = DropFlag(flag.clone());
                Box::new(async move {
                    let _guard = guard;
                    core::future::pending::<()>().await;
                })
            },
        );
        let wakes = Arc::new(CountingWaker::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);

        {
            let mut driven = pin!(turns.drive(parked.call_async(&mut store, &[], &mut [])));
            assert!(driven.as_mut().poll(&mut cx).is_pending());
            assert_eq!(
                live_stacks(),
                1,
                "the suspended guest holds its fiber stack"
            );

            assert_eq!(
                registry.request_kill(instance.id(), KillReason::Operator, 0),
                KillOutcome::Requested
            );
            assert_eq!(wakes.wakes(), 1, "the condemnation wakes the turn");
            match driven.as_mut().poll(&mut cx) {
                Poll::Ready(Err(error)) => assert_eq!(
                    error.downcast_ref::<InstanceKilled>(),
                    Some(&InstanceKilled {
                        reason: KillReason::Operator
                    })
                ),
                other => panic!("the kill ends the call, got {other:?}"),
            }
            assert!(
                dropped.load(Ordering::Acquire),
                "the fiber unwound past the host future"
            );
        }

        drop(store);
        assert_eq!(live_stacks(), 0, "the fiber stack is back in the pool");
        drop(instance);
        assert_eq!(registry.snapshot(0), [], "the registry entry is reclaimed");
    }
    /// A guest parked the way `memory.atomic.wait` parks on bare metal,
    /// blocked in `wasmtime::block_on_current_fiber` from a synchronous
    /// host frame, is ended by a kill too. The turn drops the call, the
    /// blocked frame sees `Cancelled` and unwinds with the error the
    /// parking hook's cancellation turns into in Wasmtime, and the call
    /// still reports the kill. The fiber's stack, the store and the
    /// registry entry are reclaimed as for a host future.
    #[test]
    fn a_kill_unwinds_a_fiber_blocked_on_its_own_stack() {
        let engine = build_component_engine_for_platform(&TestCpu::without_entropy())
            .expect("the component engine builds on the test platform");
        let live_stacks = || {
            engine
                .pooling_allocator_metrics()
                .expect("the component engine uses the pooling allocator")
                .stacks()
        };
        let registry = InstanceRegistry::new();
        let instance = registry.register("parked-waiter", 0);
        let activity = InstanceActivity::new(instance.clone(), 0);
        let turns = GuestTurns::new(
            activity.turns(),
            ManualClockCpu::new(),
            TestRuntimeState::default(),
            (),
        );
        let mut store = wasmtime::Store::new(&engine, activity);
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let parked = wasmtime::Func::wrap(
            &mut store,
            move |_: wasmtime::Caller<'_, InstanceActivity>| -> wasmtime::Result<()> {
                // SAFETY: this frame runs on the guest call's fiber, holds
                // no store borrow across the block, and the future borrows
                // nothing on the fiber.
                match unsafe { wasmtime::block_on_current_fiber(core::future::pending::<()>()) } {
                    Err(wasmtime::BlockOnCurrentFiberError::Cancelled(_)) => {
                        flag.store(true, Ordering::Release);
                        Err(wasmtime::Error::new(wasmtime::AtomicWaitCancelled))
                    }
                    other => panic!("the blocked fiber is only ever cancelled, got {other:?}"),
                }
            },
        );
        let wakes = Arc::new(CountingWaker::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);

        {
            let mut driven = pin!(turns.drive(parked.call_async(&mut store, &[], &mut [])));
            assert!(driven.as_mut().poll(&mut cx).is_pending());
            assert_eq!(live_stacks(), 1, "the blocked guest holds its fiber stack");

            assert_eq!(
                registry.request_kill(instance.id(), KillReason::Operator, 0),
                KillOutcome::Requested
            );
            assert_eq!(wakes.wakes(), 1, "the condemnation wakes the turn");
            match driven.as_mut().poll(&mut cx) {
                Poll::Ready(Err(error)) => assert_eq!(
                    error.downcast_ref::<InstanceKilled>(),
                    Some(&InstanceKilled {
                        reason: KillReason::Operator
                    }),
                    "the call reports the kill, not the unwinding wait"
                ),
                other => panic!("the kill ends the call, got {other:?}"),
            }
            assert!(
                cancelled.load(Ordering::Acquire),
                "the blocked frame saw the cancellation and unwound"
            );
        }

        drop(store);
        assert_eq!(live_stacks(), 0, "the fiber stack is back in the pool");
        drop(instance);
        assert_eq!(registry.snapshot(0), [], "the registry entry is reclaimed");
    }
}
