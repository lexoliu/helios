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
use crate::{ProfileScope, StoreTurns};

/// What a store does when one of its turns ends, and when the call it
/// is driving completes, beyond the accounting every store does.
pub trait TurnEpilogue<RuntimeStateImpl, Error> {
    fn end_turn(&self, runtime_state: &RuntimeStateImpl);

    /// The stop the store owes its guest at the moment the call
    /// completes, if any. It ends the call ahead of a condemnation and
    /// ahead of whatever the call returned, as an event the guest met
    /// on its way back to the host would have.
    fn end_call(&self) -> Option<Error>;
}

/// A store with nothing of its own to do at the end of a turn.
impl<RuntimeStateImpl, Error> TurnEpilogue<RuntimeStateImpl, Error> for () {
    fn end_turn(&self, _: &RuntimeStateImpl) {}

    fn end_call(&self) -> Option<Error> {
        None
    }
}

/// A component store closes the sockets its guest let go of during the
/// turn. Draining when the store gives up the processor bounds the
/// delay between a socket's death and its close by one turn: the next
/// time the guest blocks, yields at an epoch tick, or finishes.
impl<RuntimeStateImpl, Error> TurnEpilogue<RuntimeStateImpl, Error> for SocketRetirementQueue
where
    RuntimeStateImpl: ComponentRuntimeState,
{
    fn end_turn(&self, runtime_state: &RuntimeStateImpl) {
        if !self.is_empty() {
            runtime_state.retire_network_handles(self);
        }
    }

    fn end_call(&self) -> Option<Error> {
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
    pub fn drive<Call>(self, call: Call) -> OnProcessor<Call, CpuImpl, RuntimeStateImpl, Epilogue> {
        OnProcessor { call, turns: self }
    }

    fn now_nanos(&self) -> u64 {
        self.runtime_state.uptime_nanos(self.cpu.now().ticks())
    }
}

pin_project! {
    /// A call into a store's guest, accounted one processor turn per
    /// poll.
    ///
    /// A completing call first meets the store's own stop
    /// ([`TurnEpilogue::end_call`], a pending signal, for one),
    /// then its instance's condemnation, and only then reports its
    /// result. A guest that is still running meets both at its next
    /// epoch check, which raising either makes immediate; a call that
    /// returns to the host before it reaches one, its last host call
    /// having been where the signal or the kill arrived, would
    /// otherwise end as if nothing had happened.
    pub struct OnProcessor<Call, CpuImpl, RuntimeStateImpl, Epilogue> {
        #[pin]
        call: Call,
        turns: GuestTurns<CpuImpl, RuntimeStateImpl, Epilogue>,
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
        let this = self.project();
        let turns = &*this.turns;
        let turn = turns.turns.begin(turns.now_nanos());
        let polled = this.call.poll(cx);
        let elapsed = turn.end(turns.now_nanos());
        if turns.runtime_state.profiling_enabled() {
            turns.runtime_state.record_profile_stack_parts_nanos(
                ProfileScope::User,
                "user;",
                turns.turns.instance().name(),
                elapsed,
            );
        }
        turns.epilogue.end_turn(&turns.runtime_state);
        let Poll::Ready(result) = polled else {
            return Poll::Pending;
        };
        if let Some(stop) = turns.epilogue.end_call() {
            return Poll::Ready(Err(stop));
        }
        Poll::Ready(match turns.turns.pending_kill() {
            Some(reason) => Err(Error::from(InstanceKilled { reason })),
            None => result,
        })
    }
}

#[cfg(all(test, feature = "wasmtime-runtime"))]
mod tests {
    use core::future::{Future, poll_fn};
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use helios_hal::cpu::{Cpu, current_processor};

    use super::GuestTurns;
    use crate::component::{InstanceKilled, RetiredNetworkHandle, StoreSocketRetirement};
    use crate::test_support::{ManualClockCpu, TestNetworkService, TestRuntimeState};
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
    /// the host without reaching another epoch check.
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
    /// after which the export returns without another epoch check, ends
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
}
