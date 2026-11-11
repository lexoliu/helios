//! Completion slots for drivers whose requests are reaped by whichever
//! task happens to win the queue lock.
//!
//! A virtqueue completion carries the descriptor identifier of the chain
//! it finishes, and with VIRTIO_F_RING_EVENT_IDX or VIRTIO_F_IN_ORDER a
//! device is free to finish chains in an order the submitting tasks
//! cannot predict. A waiter therefore cannot assume the completion it
//! finds is its own: it drains everything the device published into this
//! table and then looks up its own slot.
//!
//! Because a completion may be drained by a task other than the one
//! waiting on it, each slot carries that waiter's waker. The drainer
//! wakes the tasks whose requests it finished directly, rather than
//! going back through the device notification: an interrupt is a
//! broadcast that only tells a task to look, and routing a completion
//! through it would make one task's wake-up depend on another task not
//! having consumed the notification first.
//!
//! The table also carries the back-pressure signal for submitters. A
//! descriptor identifier goes back to the queue's free pool the moment
//! its completion is drained, which is before its waiter has read the
//! reply out of the slot, so a new request may not take an identifier
//! whose slot is still occupied. A submitter blocked on that — or on a
//! ring with no room left — is woken by whichever task frees the chain
//! or collects the completion.
//!
//! A request that is dropped while its chain is in flight does not
//! free the identifier: the device may still read and write the chain's
//! buffers and the control slot that carries its header and status. The
//! slot goes to `Abandoned` and the device's completion, not the dead
//! future, returns it to `Idle`.
//!
//! Concurrency contract: the table is a spin mutex over a fixed slot
//! array, never held across an await and never held while a waker runs.
//! It is deliberately independent of the queue lock so a task that
//! failed to take the queue can still observe a completion another task
//! drained for it. A waiter is woken through its own slot, a submitter
//! through this table's notification, so neither can consume the other's
//! wake-up.

use core::future::{Future, poll_fn};
use core::pin::pin;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};

use async_lock::Mutex as AsyncMutex;
use helios_hal::io::{IoError, IoResult};
use spin::Mutex;

use crate::notify::Notify;
use crate::queue::{ControlSlots, Part, ResponseSpan, ResponseWindow, TransportBuffer, VirtQueue};
use crate::transport::VirtioTransport;

/// State of one descriptor identifier.
#[derive(Debug, Default)]
enum Slot {
    /// No request is using this identifier.
    #[default]
    Idle,
    /// A request is in flight, together with the waker of the task
    /// waiting on it once that task has parked.
    Pending(Option<Waker>),
    /// The device finished the request, writing this many bytes. The slot
    /// stays here until the [`Reply`] that reads it is dropped.
    Complete(u32),
    /// The request that submitted this chain was dropped before the
    /// device finished it. The identifier, and the control slot it names,
    /// stay held until the device completes the chain: it may still be
    /// reading and writing both.
    Abandoned,
}

/// Completion slots for up to `N` concurrently in-flight requests.
pub(crate) struct InFlight<const N: usize> {
    slots: Mutex<[Slot; N]>,
    /// Submitters parked because the ring is full or because the
    /// identifier they would be handed still owes a completion.
    blocked: AtomicUsize,
    /// Wakes those submitters. A completed slot wakes its own waiter
    /// directly; this means "a chain or a slot was freed" and belongs to
    /// the submitters.
    available: Notify,
    /// Set, under the slot lock, once the device broke the used-ring
    /// contract on the queue this table serves. Every parked waiter and
    /// submitter is woken to see it, and none parks again.
    faulted: AtomicBool,
}

impl<const N: usize> InFlight<N> {
    pub(crate) const fn new() -> Self {
        Self {
            slots: Mutex::new([const { Slot::Idle }; N]),
            blocked: AtomicUsize::new(0),
            available: Notify::new(),
            faulted: AtomicBool::new(false),
        }
    }

    /// Whether `token` can be handed to a new request.
    ///
    /// A descriptor identifier goes back to the queue's free pool as
    /// soon as its completion is drained, which is before the waiter
    /// has read the reply out of the slot. Submitting over that
    /// identifier would overwrite a completion nobody has collected.
    pub(crate) fn is_idle(&self, token: u16) -> bool {
        let mut slots = self.slots.lock();
        matches!(Self::slot(&mut slots, token), Slot::Idle)
    }

    /// Announces that a submitter is parked waiting for a chain or a
    /// completion slot to be freed.
    ///
    /// Callers announce their interest *before* re-testing what they
    /// are waiting for, so a task that frees it in between cannot read
    /// this counter as zero and leave them asleep.
    fn announce_blocked(&self) {
        self.blocked.fetch_add(1, Ordering::SeqCst);
    }

    fn release_blocked(&self) {
        self.blocked.fetch_sub(1, Ordering::SeqCst);
    }

    /// Reports that `count` chains or completion slots became free.
    ///
    /// The notification is a broadcast, so one is enough however many
    /// were freed: every parked submitter re-tests the ring and its own
    /// identifier for itself, and the ones that still do not fit park
    /// again. Nothing is published when no submitter is parked.
    fn note_released(&self, count: usize) {
        if count == 0 || self.blocked.load(Ordering::SeqCst) == 0 {
            return;
        }
        self.available.notify_all();
    }

    /// Records that the queue faulted and wakes everyone waiting on it.
    ///
    /// Completions already delivered stay collectable; a request whose
    /// completion never arrives fails instead of waiting for a queue that
    /// is no longer driven.
    pub(crate) fn fail(&self) {
        {
            let _slots = self.slots.lock();
            self.faulted.store(true, Ordering::SeqCst);
        }
        for token in 0..N {
            let waker = {
                let mut slots = self.slots.lock();
                match &mut slots[token] {
                    Slot::Pending(waker) => waker.take(),
                    Slot::Idle | Slot::Complete(_) | Slot::Abandoned => None,
                }
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
        self.available.notify_all();
    }

    fn is_faulted(&self) -> bool {
        self.faulted.load(Ordering::SeqCst)
    }

    /// Claims `token` for the caller. Must be called while the queue
    /// lock that produced the token is still held, so no drain can
    /// observe the completion before the slot exists.
    pub(crate) fn register(&self, token: u16) {
        let mut slots = self.slots.lock();
        let slot = Self::slot(&mut slots, token);
        assert!(
            matches!(slot, Slot::Idle),
            "virtio descriptor {token} was submitted while still in flight"
        );
        *slot = Slot::Pending(None);
    }

    /// Records a completion the caller drained from the queue and wakes
    /// the task waiting on it, which may not be the caller.
    pub(crate) fn complete(&self, token: u16, len: u32) {
        let waker = {
            let mut slots = self.slots.lock();
            let slot = Self::slot(&mut slots, token);
            match slot {
                // Nobody will read the reply of an abandoned request, so
                // the identifier is free the moment the device is done.
                Slot::Abandoned => {
                    *slot = Slot::Idle;
                    return;
                }
                Slot::Pending(_) => {}
                Slot::Idle | Slot::Complete(_) => panic!(
                    "virtio device completed descriptor {token}, which no request was waiting on"
                ),
            }
            match core::mem::replace(slot, Slot::Complete(len)) {
                Slot::Pending(waker) => waker,
                _ => unreachable!("the slot was pending"),
            }
        };
        // The slot lock is released first: a waker runs executor code,
        // and no executor may be entered from inside this table.
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// The completion for `token`, if it has arrived. The slot stays
    /// occupied until [`InFlight::release`].
    fn completion(&self, token: u16) -> Option<u32> {
        let mut slots = self.slots.lock();
        match Self::slot(&mut slots, token) {
            Slot::Complete(len) => Some(*len),
            Slot::Idle | Slot::Pending(_) | Slot::Abandoned => None,
        }
    }

    /// Lets go of a collected completion, which frees the identifier, and
    /// with it the control slot, for the next request.
    fn release(&self, token: u16) {
        {
            let mut slots = self.slots.lock();
            let slot = Self::slot(&mut slots, token);
            assert!(
                matches!(slot, Slot::Complete(_)),
                "virtio descriptor {token} was released without a collected completion"
            );
            *slot = Slot::Idle;
        }
        self.note_released(1);
    }

    /// Records that the request waiting on `token` was dropped.
    ///
    /// A chain still in flight keeps its identifier until the device
    /// completes it; one whose completion had already arrived is freed
    /// now, since nobody will read it.
    fn abandon(&self, token: u16) {
        let freed = {
            let mut slots = self.slots.lock();
            let slot = Self::slot(&mut slots, token);
            match slot {
                Slot::Pending(_) => {
                    *slot = Slot::Abandoned;
                    false
                }
                Slot::Complete(_) => {
                    *slot = Slot::Idle;
                    true
                }
                Slot::Idle | Slot::Abandoned => {
                    panic!("virtio descriptor {token} was abandoned while no request owned it")
                }
            }
        };
        if freed {
            self.note_released(1);
        }
    }

    #[cfg(test)]
    pub(crate) fn take(&self, token: u16) -> Option<u32> {
        let len = self.completion(token)?;
        self.release(token);
        Some(len)
    }

    /// Parks `waker` on `token`, reporting whether the completion is
    /// already there, or the queue faulted, and the caller should not park at
    /// all.
    fn arm(&self, token: u16, waker: &Waker) -> bool {
        let mut slots = self.slots.lock();
        if self.is_faulted() {
            return true;
        }
        let slot = Self::slot(&mut slots, token);
        match slot {
            Slot::Complete(_) => true,
            Slot::Pending(parked) => {
                match parked {
                    Some(parked) if parked.will_wake(waker) => {}
                    parked => *parked = Some(waker.clone()),
                }
                false
            }
            Slot::Idle | Slot::Abandoned => {
                panic!("virtio descriptor {token} was awaited while not in flight")
            }
        }
    }

    fn slot(slots: &mut [Slot; N], token: u16) -> &mut Slot {
        slots.get_mut(usize::from(token)).unwrap_or_else(|| {
            panic!("virtio descriptor {token} is outside the {N}-slot completion table")
        })
    }
}

/// A request's claim on the descriptor identifier its chain was headed by.
///
/// Dropping it, whether the request is cancelled or fails before the
/// device answers, abandons the chain: the identifier, and with it the
/// control slot, is held until the device completes the chain, and freed
/// by that completion rather than by a future that no longer exists.
#[must_use = "a dropped ticket abandons its request"]
pub(crate) struct Ticket<'a, const N: usize> {
    inflight: &'a InFlight<N>,
    token: u16,
    response: ResponseWindow<'a>,
}

impl<const N: usize> Drop for Ticket<'_, N> {
    fn drop(&mut self) {
        self.inflight.abandon(self.token);
    }
}

/// A finished request, with the response bytes the device wrote.
///
/// The control slot stays held until this is dropped, so the bytes cannot
/// be overwritten by a later request while they are read.
#[must_use = "the slot stays held until the reply is dropped"]
pub(crate) struct Reply<'a, const N: usize> {
    inflight: &'a InFlight<N>,
    token: u16,
    len: u32,
    response: ResponseWindow<'a>,
}

impl<const N: usize> Reply<'_, N> {
    /// Bytes the device reported writing into the chain.
    pub(crate) fn written(&self) -> u32 {
        self.len
    }

    /// The bytes the device wrote to the chain's slot response area.
    pub(crate) fn response(&self) -> &[u8] {
        // SAFETY: the completion arrived and this reply holds the slot.
        unsafe { self.response.bytes() }
    }
}

impl<const N: usize> Drop for Reply<'_, N> {
    fn drop(&mut self) {
        self.inflight.release(self.token);
    }
}

/// Places one chain in the ring and claims its completion slot.
///
/// This is the submission half of the shape every request/response
/// driver uses. The queue lock is taken only long enough to publish the
/// chain and register the completion — registering under that lock is
/// what makes the completion reachable, since no other task can drain
/// the token before its waiter exists — and is released before the
/// caller awaits, so several requests are in flight at once.
///
/// Two conditions make a submission wait, and neither is a device
/// fault: the ring has no room for the chain, or the identifier the
/// queue would hand out next still holds a completion its waiter has
/// not collected. In both cases the submitter drains what the device
/// has already published, which is what recycles descriptors and wakes
/// the waiters those completions belong to, and then parks on the
/// table's own notification until a waiter frees the chain or collects
/// the completion it needs.
///
/// This variant names the caller's own buffers in the descriptors, so
/// the memory has to outlive the chain on the caller's side. Requests
/// whose bytes the driver owns go through [`submit_control`].
pub(crate) async fn submit_chain<'a, T, const N: usize>(
    inflight: &'a InFlight<N>,
    queue: &AsyncMutex<VirtQueue<T>>,
    transport: &T,
    inputs: &[&[u8]],
    outputs: &mut [&mut [u8]],
) -> IoResult<Ticket<'a, N>>
where
    T: VirtioTransport,
{
    let buffers = inputs.len() + outputs.len();
    let (token, _) = submit_with(inflight, queue, transport, buffers, |queue| {
        let token = queue.submit(transport, inputs, outputs)?;
        Ok((token, ResponseSpan::NONE))
    })
    .await?;
    Ok(Ticket {
        inflight,
        token,
        response: ResponseWindow::EMPTY,
    })
}

/// Places the chain `parts` describes, with its control bytes staged in
/// the slot of the descriptor that heads it.
///
/// The slot is chosen under the queue lock, once the completion table has
/// said the identifier the chain will take is idle, so no other request
/// can be using it. That is the whole of the slot's synchronisation; see
/// the contract in [`crate::queue`].
pub(crate) async fn submit_control<'a, T, const N: usize>(
    inflight: &'a InFlight<N>,
    queue: &AsyncMutex<VirtQueue<T>>,
    transport: &T,
    slots: &'a ControlSlots<TransportBuffer<T>>,
    parts: &[Part<'_>],
) -> IoResult<Ticket<'a, N>>
where
    T: VirtioTransport,
{
    let (token, span) = submit_with(inflight, queue, transport, parts.len(), |queue| {
        // SAFETY: `submit_with` calls this under the queue lock, after
        // the completion table reported the identifier the chain will be
        // headed by idle, which is the ownership `submit_parts` asks for.
        unsafe { queue.submit_parts(transport, slots, parts) }
    })
    .await?;
    Ok(Ticket {
        inflight,
        token,
        response: slots.window(token, span),
    })
}

/// The loop every submission shares: wait for room and for an idle
/// identifier, then let `submit` place the chain.
///
/// `submit` runs under the queue lock, once, with the identifier the
/// chain will be headed by known to be idle.
async fn submit_with<T, const N: usize>(
    inflight: &InFlight<N>,
    queue: &AsyncMutex<VirtQueue<T>>,
    transport: &T,
    buffers: usize,
    mut submit: impl FnMut(&mut VirtQueue<T>) -> IoResult<(u16, ResponseSpan)>,
) -> IoResult<(u16, ResponseSpan)>
where
    T: VirtioTransport,
{
    let mut announced = false;
    let outcome = loop {
        // Armed before the ring is tested, for the same reason the
        // completion wait is: a release that lands in between must not
        // be slept through.
        let notified = inflight.available.notified();
        let drained = {
            let mut queue = queue.lock().await;
            if inflight.is_faulted() {
                break Err(IoError::DeviceFault);
            }
            if queue.has_room_for(buffers) && inflight.is_idle(queue.next_free_descriptor()) {
                match submit(&mut queue) {
                    Ok((token, span)) => {
                        queue.notify(transport);
                        inflight.register(token);
                        break Ok((token, span));
                    }
                    Err(error) => break Err(error),
                }
            }
            match queue.drain_used(|completed, len| inflight.complete(completed, len)) {
                Ok(drained) => drained,
                Err(error) => {
                    inflight.fail();
                    break Err(error);
                }
            }
        };
        inflight.note_released(drained);
        if drained != 0 {
            continue;
        }
        if !announced {
            inflight.announce_blocked();
            announced = true;
            continue;
        }
        notified.await;
    };
    if announced {
        inflight.release_blocked();
    }
    outcome
}

/// Waits for `token`'s completion, draining the queue on behalf of every
/// waiter whenever this task can take it.
///
/// This is the shape every single-request driver uses. A parked task is
/// woken by either of two things: the device notification, which means
/// there may be work in the used ring for this task to drain on
/// everyone's behalf, or its own slot being completed by whichever task
/// drained the ring first. `wait` is a parameter so callers can park on
/// something other than the raw interrupt — a deadline, say — without
/// duplicating the loop.
///
/// A device that breaks the used-ring contract fails every waiter on the
/// queue with [`IoError::DeviceFault`]; the queue is not driven again.
///
/// The ticket is consumed. Dropping the request while it waits, or
/// failing it, abandons the chain instead: the identifier and its control
/// slot stay held until the device completes it. What comes back is a
/// [`Reply`], which keeps the slot held until the caller has read the
/// response bytes out of it.
pub(crate) async fn await_completion<'a, T, Wait, WaitFuture, const N: usize>(
    queue: &AsyncMutex<VirtQueue<T>>,
    ticket: Ticket<'a, N>,
    mut wait: Wait,
) -> IoResult<Reply<'a, N>>
where
    T: VirtioTransport,
    Wait: FnMut() -> WaitFuture,
    WaitFuture: Future<Output = ()>,
{
    let inflight = ticket.inflight;
    let token = ticket.token;
    loop {
        // The device wait is created before the slot and the ring are
        // tested, so a completion that arrives in between is observed
        // by this future rather than slept through.
        let mut notified = pin!(wait());

        if let Some(len) = inflight.completion(token) {
            // The reply takes over the slot. Its drop frees the
            // identifier, which is also what wakes a submitter parked
            // on it.
            let reply = Reply {
                inflight,
                token,
                len,
                response: ticket.response,
            };
            core::mem::forget(ticket);
            return Ok(reply);
        }
        if inflight.is_faulted() {
            return Err(IoError::DeviceFault);
        }

        let drained = match queue.try_lock() {
            Some(mut queue) => match queue.drain_used(|completed, len| {
                inflight.complete(completed, len);
            }) {
                Ok(drained) => drained,
                Err(error) => {
                    inflight.fail();
                    return Err(error);
                }
            },
            None => 0,
        };
        inflight.note_released(drained);
        if drained != 0 {
            continue;
        }

        poll_fn(|context: &mut Context<'_>| {
            if inflight.arm(token, context.waker()) {
                return Poll::Ready(());
            }
            notified.as_mut().poll(context)
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::InFlight;
    use core::task::Waker;

    #[test]
    fn a_completion_is_delivered_to_the_registered_token() {
        let inflight: InFlight<4> = InFlight::new();

        inflight.register(2);
        assert_eq!(inflight.take(2), None, "nothing has completed yet");

        inflight.complete(2, 96);
        assert_eq!(inflight.take(2), Some(96));
        assert_eq!(inflight.take(2), None, "a completion is delivered once");
    }

    #[test]
    fn completions_are_routed_by_token_not_by_arrival_order() {
        let inflight: InFlight<4> = InFlight::new();

        inflight.register(0);
        inflight.register(3);
        // The device finishes the second request first.
        inflight.complete(3, 7);
        inflight.complete(0, 11);

        assert_eq!(inflight.take(3), Some(7));
        assert_eq!(inflight.take(0), Some(11));
    }

    #[test]
    fn an_identifier_stays_busy_until_its_completion_is_collected() {
        let inflight: InFlight<4> = InFlight::new();

        assert!(inflight.is_idle(1));
        inflight.register(1);
        assert!(!inflight.is_idle(1), "the request is still in flight");
        inflight.complete(1, 4);
        assert!(
            !inflight.is_idle(1),
            "the queue has recycled the identifier, but the reply is still unread"
        );

        assert_eq!(inflight.take(1), Some(4));
        assert!(inflight.is_idle(1));
    }

    #[test]
    fn a_parked_waiter_is_woken_by_whoever_drains_its_completion() {
        let inflight: InFlight<2> = InFlight::new();
        let waker = Waker::noop();

        inflight.register(1);
        assert!(!inflight.arm(1, waker), "the request is still in flight");
        inflight.complete(1, 5);
        assert!(
            inflight.arm(1, waker),
            "an armed waiter sees its completion without parking again"
        );
        assert_eq!(inflight.take(1), Some(5));
    }

    #[test]
    fn an_abandoned_chain_keeps_its_identifier_until_the_device_completes_it() {
        let inflight: InFlight<4> = InFlight::new();

        inflight.register(2);
        inflight.abandon(2);
        assert!(
            !inflight.is_idle(2),
            "the device may still write the chain's slot"
        );
        inflight.complete(2, 9);
        assert!(inflight.is_idle(2), "its late completion frees it");
        assert_eq!(inflight.take(2), None, "nobody collects an abandoned reply");
    }

    #[test]
    fn abandoning_a_completed_chain_frees_it_at_once() {
        let inflight: InFlight<4> = InFlight::new();

        inflight.register(1);
        inflight.complete(1, 4);
        inflight.abandon(1);
        assert!(inflight.is_idle(1));
    }

    #[test]
    #[should_panic(expected = "which no request was waiting on")]
    fn a_completion_without_a_waiter_is_a_device_fault() {
        let inflight: InFlight<2> = InFlight::new();
        inflight.complete(1, 0);
    }
}
