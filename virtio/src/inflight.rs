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
use helios_hal::iommu::PhysicalRange;
use spin::Mutex;

use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::resume::{QuiesceError, ReinitError, Resumed, settle};
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
    /// The device finished the request, writing this many bytes.
    Complete(u32),
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
                    Slot::Idle | Slot::Complete(_) => None,
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

    /// Lets the table serve requests again after the queue it fronts was
    /// re-programmed on a reset device, which is what clears a fault.
    fn recover(&self) {
        let _slots = self.slots.lock();
        self.faulted.store(false, Ordering::SeqCst);
    }

    /// Applies how a re-initialisation ended: the table serves requests
    /// again only when the device is back, and otherwise fails every
    /// request, parked or yet to come.
    pub(crate) fn resume(&self, resumed: Resumed) {
        match resumed {
            Resumed::Back => self.recover(),
            Resumed::Failed => self.fail(),
        }
    }

    /// Finishes every request still pending with zero bytes written and
    /// wakes its waiter.
    ///
    /// For a queue whose chains are buffers posted for the device to
    /// answer whenever it chooses (a balloon's statistics buffer): when
    /// the device is reset those chains are gone and will never be
    /// answered, so their waiters are released to post a fresh one
    /// instead of parking for ever.
    pub(crate) fn abandon_pending(&self) {
        for token in 0..N {
            let waker = {
                let mut slots = self.slots.lock();
                match &mut slots[token] {
                    slot @ Slot::Pending(_) => match core::mem::replace(slot, Slot::Complete(0)) {
                        Slot::Pending(waker) => waker,
                        Slot::Idle | Slot::Complete(_) => None,
                    },
                    Slot::Idle | Slot::Complete(_) => None,
                }
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
        self.available.notify_all();
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
            match core::mem::replace(slot, Slot::Complete(len)) {
                Slot::Pending(waker) => waker,
                Slot::Idle | Slot::Complete(_) => panic!(
                    "virtio device completed descriptor {token}, which no request was waiting on"
                ),
            }
        };
        // The slot lock is released first: a waker runs executor code,
        // and no executor may be entered from inside this table.
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Takes the completion for `token` if it has arrived.
    pub(crate) fn take(&self, token: u16) -> Option<u32> {
        let mut slots = self.slots.lock();
        let slot = Self::slot(&mut slots, token);
        match slot {
            Slot::Complete(len) => {
                let len = *len;
                *slot = Slot::Idle;
                Some(len)
            }
            Slot::Idle | Slot::Pending(_) => None,
        }
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
            Slot::Idle => panic!("virtio descriptor {token} was awaited while not in flight"),
        }
    }

    fn slot(slots: &mut [Slot; N], token: u16) -> &mut Slot {
        slots.get_mut(usize::from(token)).unwrap_or_else(|| {
            panic!("virtio descriptor {token} is outside the {N}-slot completion table")
        })
    }
}

/// Reaps `queue` on behalf of every waiter until the device owns none of
/// its chains, for a quiesce.
///
/// Each completion is recorded in `inflight` and its waiter woken, exactly
/// as the wait path does, so nothing is lost between the reap and the
/// waiter's next poll: the slot holds the answer until the waiter takes
/// it, and the waker has already been run. See [`crate::resume`] for why
/// the queue lock can only be taken here and never waited for.
pub(crate) fn drain_to_idle<T, const N: usize>(
    inflight: &InFlight<N>,
    queue: &AsyncMutex<VirtQueue<T>>,
    transport: &T,
    queue_index: u16,
    expired: &mut impl FnMut() -> bool,
) -> Result<(), QuiesceError>
where
    T: VirtioTransport,
{
    let device = transport.device_type();
    settle(device, queue_index, expired, || {
        let mut ring = queue.try_lock().ok_or(QuiesceError::QueueBusy {
            device,
            queue: queue_index,
        })?;
        let drained = ring
            .drain_used(|completed, len| inflight.complete(completed, len))
            .map_err(|_| {
                inflight.fail();
                QuiesceError::DeviceFault {
                    device,
                    queue: queue_index,
                }
            })?;
        let remaining = ring.in_flight_chains();
        drop(ring);
        inflight.note_released(drained);
        Ok(remaining)
    })
}

/// Re-programs the request queue `queue` fronts on a reset device.
///
/// The queue holds no chain: a quiesce drained it and nothing was
/// submitted since. Completions already recorded in the driver's
/// [`InFlight`] stay there for their waiters, and the table serves
/// requests again only when the driver reports the whole
/// re-initialisation through [`InFlight::resume`].
pub(crate) fn reinitialize_requests<T>(
    queue: &AsyncMutex<VirtQueue<T>>,
    transport: &T,
    queue_index: u16,
) -> Result<(), ReinitError>
where
    T: VirtioTransport,
{
    let mut ring = queue.try_lock().ok_or(ReinitError::QueueBusy {
        device: transport.device_type(),
        queue: queue_index,
    })?;
    ring.reinitialize(transport)?;
    Ok(())
}

/// Re-programs a queue whose chains are buffers posted for the device to
/// answer whenever it chooses, on a reset device.
///
/// The chains are gone with the device, so their waiters are released
/// with nothing written, to post a fresh buffer instead of parking for
/// ever on one that will never be answered.
pub(crate) fn reinitialize_posted<T, const N: usize>(
    inflight: &InFlight<N>,
    queue: &AsyncMutex<VirtQueue<T>>,
    transport: &T,
    queue_index: u16,
) -> Result<(), ReinitError>
where
    T: VirtioTransport,
{
    let mut ring = queue.try_lock().ok_or(ReinitError::QueueBusy {
        device: transport.device_type(),
        queue: queue_index,
    })?;
    ring.reinitialize_discarding(transport)?;
    drop(ring);
    inflight.abandon_pending();
    Ok(())
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
pub(crate) async fn submit_chain<T, const N: usize>(
    inflight: &InFlight<N>,
    queue: &AsyncMutex<VirtQueue<T>>,
    transport: &T,
    inputs: &[&[u8]],
    outputs: &mut [&mut [u8]],
) -> IoResult<u16>
where
    T: VirtioTransport,
{
    submit_chain_with_payload(inflight, queue, transport, inputs, None, outputs).await
}

/// [`submit_chain`], with a run of memory the driver holds no pointer to
/// carried between the read-only buffers and the writable ones.
///
/// The payload is somebody else's pages — a guest's own pinned command
/// buffer — named by the physical run they occupy. Nothing is copied
/// and nothing is allocated: the device reads them where they are.
pub(crate) async fn submit_chain_with_payload<T, const N: usize>(
    inflight: &InFlight<N>,
    queue: &AsyncMutex<VirtQueue<T>>,
    transport: &T,
    inputs: &[&[u8]],
    payload: Option<PhysicalRange>,
    outputs: &mut [&mut [u8]],
) -> IoResult<u16>
where
    T: VirtioTransport,
{
    let buffers = inputs.len() + usize::from(payload.is_some()) + outputs.len();
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
                match queue.submit_with_payload(transport, inputs, payload, outputs) {
                    Ok(token) => {
                        queue.notify(transport);
                        inflight.register(token);
                        break Ok(token);
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
pub(crate) async fn await_completion<T, Wait, WaitFuture, const N: usize>(
    inflight: &InFlight<N>,
    queue: &AsyncMutex<VirtQueue<T>>,
    token: u16,
    mut wait: Wait,
) -> IoResult<u32>
where
    T: VirtioTransport,
    Wait: FnMut() -> WaitFuture,
    WaitFuture: Future<Output = ()>,
{
    loop {
        // The device wait is created before the slot and the ring are
        // tested, so a completion that arrives in between is observed
        // by this future rather than slept through.
        let mut notified = pin!(wait());

        if let Some(len) = inflight.take(token) {
            // This request's chain went back to the ring when its
            // completion was drained, and its slot is idle again now.
            // A submitter parked on either has nothing else to wake it.
            inflight.note_released(1);
            return Ok(len);
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
    fn an_abandoned_request_is_released_with_nothing_written() {
        let inflight: InFlight<4> = InFlight::new();
        let waker = Waker::noop();

        inflight.register(0);
        inflight.register(2);
        assert!(!inflight.arm(2, waker));
        inflight.complete(0, 9);

        inflight.abandon_pending();

        assert_eq!(
            inflight.take(0),
            Some(9),
            "an answer that already arrived is kept"
        );
        assert_eq!(
            inflight.take(2),
            Some(0),
            "a request the device will never answer completes empty"
        );
        assert!(inflight.is_idle(1), "an unused slot is untouched");
    }

    #[test]
    fn a_table_serves_requests_again_once_its_queue_was_re_programmed() {
        let inflight: InFlight<2> = InFlight::new();
        inflight.fail();
        assert!(inflight.is_faulted());

        inflight.recover();

        assert!(!inflight.is_faulted());
    }

    #[test]
    #[should_panic(expected = "which no request was waiting on")]
    fn a_completion_without_a_waiter_is_a_device_fault() {
        let inflight: InFlight<2> = InFlight::new();
        inflight.complete(1, 0);
    }
}
