//! The compile session: the one task a compile runs in, whichever route
//! asked for it.
//!
//! A requester (a `programs.aot`, `exec`, `spawn` or WASIX `proc_exec` host
//! call) holds only the receiving half of a reply channel. The session task
//! owns the sending half, the compile slot's lease, the compile's instance and
//! its worker joins, so the compile's lifetime is the session's and never the
//! requester's:
//!
//! - the slot is released only after the compile's body has returned, which
//!   includes joining every worker task the plugin spawned, because a worker
//!   that outlives its lease runs on the plugin's memory while the next compile
//!   writes to it;
//! - a requester that goes away (its receiver is dropped: it was killed, its
//!   call was cancelled, its store went) condemns the compile's instance, and
//!   the body is *not* dropped: it ends by itself once every store has unwound
//!   through the kill path, and only then is the slot released;
//! - a requester that went away before the slot was acquired never starts the
//!   compile at all.
//!
//! # Concurrency
//!
//! The session is one task, polled by one processor at a time. The slot is the
//! async FIFO mutex of [`CompilerCompileSlot`]: waiting parks, and a waiter that
//! is dropped leaves the queue. Reply cancellation is observed through
//! [`Sender::poll_canceled`], which registers the task's waker before it tests
//! the receiver's closed flag, so a drop between the test and the park is not
//! lost and nothing is polled on a timer. Condemning the instance runs in task
//! context (the registry's `entries` spin lock and the kill table's, neither of
//! which an interrupt handler takes) and happens at most once per session.

use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::Poll;

use futures::channel::oneshot::Sender;

use super::CompilerCompileLease;
use super::CompilerCompileSlot;
use crate::wasmtime_adapter::preemption::GuestInterrupts;
use crate::{InstanceId, InstanceRegistry, KillOutcome, KillReason};

/// Runs one compile for the requester at the other end of `reply`.
///
/// Waits for the slot, `start`s the compile once it is held, and drives the
/// body it returns to its end with the lease held. `start` registers whatever
/// the compile needs (its instance, its stores' state) and returns the
/// condemnation to run if the requester leaves, with the body; it is not called
/// at all when the requester left before the slot was free. `finish` turns the
/// body's output into the reply, after the lease is released, and is skipped
/// when the requester is gone by then.
pub(super) async fn run_compile_session<Reply, Output, Condemn, Body>(
    slot: &CompilerCompileSlot,
    mut reply: Sender<Reply>,
    start: impl FnOnce() -> (Condemn, Body),
    finish: impl FnOnce(Output) -> Reply,
) where
    Condemn: FnOnce(),
    Body: Future<Output = Output>,
{
    let Some(lease) = acquire_unless_abandoned(slot, &mut reply).await else {
        return;
    };
    let (condemn, body) = start();
    let output = watch_requester(&mut reply, condemn, body).await;
    // The body joined every worker, so nothing runs on the plugin's memory.
    drop(lease);
    if reply.is_canceled() {
        return;
    }
    // A failed send is a requester that left since the check above.
    let _ = reply.send(finish(output));
}

/// Waits for the compile slot, unless the requester leaves first.
///
/// Both waits are armed by one poll, the requester's before the slot's, and a
/// requester that is gone wins over a slot that is free: the compile would
/// only be thrown away.
async fn acquire_unless_abandoned<'slot, Reply>(
    slot: &'slot CompilerCompileSlot,
    reply: &mut Sender<Reply>,
) -> Option<CompilerCompileLease<'slot>> {
    let mut acquire = pin!(slot.acquire());
    poll_fn(|cx| {
        if reply.poll_canceled(cx).is_ready() {
            return Poll::Ready(None);
        }
        acquire.as_mut().poll(cx).map(Some)
    })
    .await
}

/// Drives `body`, and runs `condemn` the first time the requester is found
/// gone, then keeps driving `body`.
///
/// The body is never dropped here. It holds stores suspended in the plugin and
/// owns the join of every worker task, and dropping it would release them
/// while workers still run on the plugin's memory; condemning the compile's
/// instance is what makes the body end.
async fn watch_requester<Reply, Output>(
    reply: &mut Sender<Reply>,
    condemn: impl FnOnce(),
    body: impl Future<Output = Output>,
) -> Output {
    let mut condemn = Some(condemn);
    let mut body = pin!(body);
    poll_fn(|cx| {
        if condemn.is_some()
            && reply.poll_canceled(cx).is_ready()
            && let Some(condemn) = condemn.take()
        {
            condemn();
        }
        body.as_mut().poll(cx)
    })
    .await
}

/// Condemns the compile's instance because its requester is gone, and
/// interrupts every store of it, so the kill reaches each one wherever it is:
/// running wasm, or parked in a call.
pub(super) fn condemn_abandoned_compile(
    registry: &InstanceRegistry,
    interrupts: &GuestInterrupts,
    instance: InstanceId,
    now_nanos: u64,
) -> KillOutcome {
    let outcome = registry.request_kill(instance, KillReason::RequesterGone, now_nanos);
    interrupts.interrupt_instance(instance);
    outcome
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use alloc::task::Wake;
    use core::future::Future;
    use core::pin::{Pin, pin};
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use core::task::{Context, Waker};

    use futures::channel::oneshot;
    use helios_hal::cpu::Timebase;

    use super::*;

    /// A waker that counts the wakes it receives.
    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl WakeCount {
        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Task {
        wakes: Arc<WakeCount>,
        waker: Waker,
    }

    impl Task {
        fn new() -> Self {
            let wakes = Arc::new(WakeCount::default());
            Self {
                waker: Waker::from(wakes.clone()),
                wakes,
            }
        }

        fn context(&self) -> Context<'_> {
            Context::from_waker(&self.waker)
        }
    }

    /// What a session needs from the platform: the registry the compile's
    /// instance lives in and the engine's interrupt routes.
    struct Platform {
        registry: InstanceRegistry,
        interrupts: GuestInterrupts,
    }

    impl Platform {
        fn new() -> Self {
            Self {
                registry: InstanceRegistry::new(),
                interrupts: GuestInterrupts::new(1, Timebase::new(1_000_000_000)),
            }
        }
    }

    /// A compile whose phases the test opens one by one: `compiled` ends the
    /// plugin call, and each worker is a task that has not ended yet.
    struct Compile {
        compiled: oneshot::Receiver<()>,
        workers: [oneshot::Receiver<()>; 2],
        body_polls: Arc<AtomicUsize>,
    }

    struct Gates {
        compiled: oneshot::Sender<()>,
        workers: [oneshot::Sender<()>; 2],
    }

    fn compile() -> (Compile, Gates, Arc<AtomicUsize>) {
        let (compiled_tx, compiled) = oneshot::channel();
        let (first_tx, first) = oneshot::channel();
        let (second_tx, second) = oneshot::channel();
        let body_polls = Arc::new(AtomicUsize::new(0));
        (
            Compile {
                compiled,
                workers: [first, second],
                body_polls: body_polls.clone(),
            },
            Gates {
                compiled: compiled_tx,
                workers: [first_tx, second_tx],
            },
            body_polls,
        )
    }

    impl Compile {
        /// The compile proper, then the join of every worker, as the real
        /// body does.
        async fn run(self) -> &'static str {
            let Self {
                compiled,
                workers: [first, second],
                body_polls,
            } = self;
            let _ = counted(&body_polls, compiled).await;
            let _ = counted(&body_polls, first).await;
            let _ = counted(&body_polls, second).await;
            "compiled"
        }
    }

    /// Awaits `gate`, counting every poll it is given.
    async fn counted(polls: &AtomicUsize, mut gate: oneshot::Receiver<()>) {
        let _ = core::future::poll_fn(|cx| {
            polls.fetch_add(1, Ordering::SeqCst);
            Pin::new(&mut gate).poll(cx)
        })
        .await;
    }

    /// A requester that goes away condemns its compile exactly once, through
    /// the ordinary kill path, and the compile keeps running: the body is
    /// still polled, and it is the body that ends the session.
    #[test]
    fn a_dropped_reply_condemns_its_compile() {
        let platform = Platform::new();
        let slot = CompilerCompileSlot::new();
        let (reply, requester) = oneshot::channel::<&'static str>();
        let instance = platform.registry.register("compiler-plugin", 0);
        let condemnations = Arc::new(AtomicUsize::new(0));
        let (body, gates, body_polls) = compile();
        let session = run_compile_session(
            &slot,
            reply,
            {
                let condemnations = condemnations.clone();
                let registry = platform.registry.clone();
                let interrupts = &platform.interrupts;
                let id = instance.id();
                move || {
                    (
                        move || {
                            condemnations.fetch_add(1, Ordering::SeqCst);
                            condemn_abandoned_compile(&registry, interrupts, id, 0);
                        },
                        body.run(),
                    )
                }
            },
            |output| output,
        );
        let mut session = pin!(session);
        let task = Task::new();

        assert!(session.as_mut().poll(&mut task.context()).is_pending());
        assert_eq!(condemnations.load(Ordering::SeqCst), 0);
        assert_eq!(instance.pending_kill(), None);

        drop(requester);
        assert_eq!(
            task.wakes.count(),
            1,
            "the requester leaving wakes the session: the watch is a signal, not a poll"
        );
        let polls_before = body_polls.load(Ordering::SeqCst);
        assert!(session.as_mut().poll(&mut task.context()).is_pending());
        assert_eq!(instance.pending_kill(), Some(KillReason::RequesterGone));
        assert_eq!(condemnations.load(Ordering::SeqCst), 1);
        assert!(
            body_polls.load(Ordering::SeqCst) > polls_before,
            "the compile body is still driven after the condemnation"
        );

        // Nothing condemns twice, and the session ends only with its body.
        assert!(session.as_mut().poll(&mut task.context()).is_pending());
        assert_eq!(condemnations.load(Ordering::SeqCst), 1);
        let Gates { compiled, workers } = gates;
        compiled.send(()).unwrap();
        for worker in workers {
            worker.send(()).unwrap();
        }
        assert!(session.as_mut().poll(&mut task.context()).is_ready());
    }

    /// The slot is the compile's until every worker has ended, also after the
    /// requester left and the plugin's call has returned: a second compile
    /// stays parked until the last worker is joined.
    #[test]
    fn the_compile_slot_outlives_every_worker() {
        let platform = Platform::new();
        let slot = CompilerCompileSlot::new();
        let (reply, requester) = oneshot::channel::<&'static str>();
        let instance = platform.registry.register("compiler-plugin", 0);
        let (body, gates, _) = compile();
        let session = run_compile_session(
            &slot,
            reply,
            {
                let registry = platform.registry.clone();
                let interrupts = &platform.interrupts;
                let id = instance.id();
                move || {
                    (
                        move || {
                            condemn_abandoned_compile(&registry, interrupts, id, 0);
                        },
                        body.run(),
                    )
                }
            },
            |output| output,
        );
        let mut session = pin!(session);
        let task = Task::new();
        let next_task = Task::new();

        assert!(session.as_mut().poll(&mut task.context()).is_pending());
        drop(requester);
        let Gates { compiled, workers } = gates;
        let [first, second] = workers;

        // The kill ends the plugin's call; both workers are still running.
        compiled.send(()).unwrap();
        assert!(session.as_mut().poll(&mut task.context()).is_pending());
        let mut next = pin!(slot.acquire());
        assert!(
            next.as_mut().poll(&mut next_task.context()).is_pending(),
            "the plugin's call returned, but its workers still run on the plugin's memory"
        );

        first.send(()).unwrap();
        assert!(session.as_mut().poll(&mut task.context()).is_pending());
        assert!(
            next.as_mut().poll(&mut next_task.context()).is_pending(),
            "one worker is still running"
        );

        second.send(()).unwrap();
        assert!(session.as_mut().poll(&mut task.context()).is_ready());
        assert_eq!(next_task.wakes.count(), 1, "the release wakes the waiter");
        assert!(next.as_mut().poll(&mut next_task.context()).is_ready());
    }

    /// A compile queued behind another whose requester leaves before the slot
    /// is free never starts: nothing is registered, no store is made, and the
    /// queue forgets it.
    #[test]
    fn a_queued_compile_whose_requester_left_never_starts() {
        let slot = CompilerCompileSlot::new();
        let holder = futures_lite::future::block_on(slot.acquire());
        let (reply, requester) = oneshot::channel::<&'static str>();
        let started = Arc::new(AtomicBool::new(false));
        let session = run_compile_session(
            &slot,
            reply,
            {
                let started = started.clone();
                move || {
                    started.store(true, Ordering::SeqCst);
                    (|| {}, core::future::ready("compiled"))
                }
            },
            |output| output,
        );
        let mut session = pin!(session);
        let task = Task::new();

        assert!(session.as_mut().poll(&mut task.context()).is_pending());
        drop(requester);
        assert_eq!(
            task.wakes.count(),
            1,
            "the departure wakes the queued session"
        );
        assert!(
            session.as_mut().poll(&mut task.context()).is_ready(),
            "a session whose requester left does not wait for the slot"
        );
        drop(holder);
        assert!(
            !started.load(Ordering::SeqCst),
            "the compile never started: no instance is registered and no store is made"
        );
        let mut again = pin!(slot.acquire());
        assert!(
            again.as_mut().poll(&mut task.context()).is_ready(),
            "the abandoned session left the slot's queue"
        );
    }
}
