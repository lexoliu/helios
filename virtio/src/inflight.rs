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
//! The table also carries the back-pressure signal for submitters. A
//! descriptor identifier goes back to the queue's free pool the moment
//! its completion is drained, which is before its waiter has read the
//! reply out of the slot, so a new request may not take an identifier
//! whose slot is still occupied. A submitter blocked on that — or on a
//! ring with no room left — is woken by whichever task frees the chain
//! or collects the completion.
//!
//! Concurrency contract: the table is a lock-free-to-the-caller spin
//! mutex over a fixed slot array. It is never held across an await, and
//! it is deliberately independent of the queue lock so a task that
//! failed to take the queue can still observe a completion another task
//! drained for it. Waiters park on the device interrupt, submitters on
//! this table's own notification, so neither can consume the other's
//! wake-up.

use core::future::Future;
use core::sync::atomic::{AtomicUsize, Ordering};

use async_lock::Mutex as AsyncMutex;
use helios_hal::io::IoResult;
use spin::Mutex;

use crate::notify::Notify;
use crate::queue::VirtQueue;
use crate::transport::VirtioTransport;

/// State of one descriptor identifier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Slot {
    /// No request is using this identifier.
    #[default]
    Idle,
    /// A request is in flight and its submitter is waiting.
    Pending,
    /// The device finished the request, writing this many bytes.
    Complete(u32),
}

/// Completion slots for up to `N` concurrently in-flight requests.
pub(crate) struct InFlight<const N: usize> {
    slots: Mutex<[Slot; N]>,
    /// Submitters parked because the ring is full or because the
    /// identifier they would be handed still owes a completion.
    blocked: AtomicUsize,
    /// Wakes those submitters. A device interrupt means "a completion
    /// arrived" and belongs to the waiters; this means "a chain or a
    /// slot was freed" and belongs to the submitters.
    available: Notify,
}

impl<const N: usize> InFlight<N> {
    pub(crate) const fn new() -> Self {
        Self {
            slots: Mutex::new([Slot::Idle; N]),
            blocked: AtomicUsize::new(0),
            available: Notify::new(),
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
        *Self::slot(&mut slots, token) == Slot::Idle
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
    /// Each freed resource admits one blocked submitter, so each gets
    /// its own wake-up. Nothing is published when no submitter is
    /// parked: an unclaimed notification would leave a permit behind
    /// for a later submitter to spin through.
    fn note_released(&self, count: usize) {
        if count == 0 || self.blocked.load(Ordering::SeqCst) == 0 {
            return;
        }
        for _ in 0..count {
            self.available.notify_all();
        }
    }

    /// Claims `token` for the caller. Must be called while the queue
    /// lock that produced the token is still held, so no drain can
    /// observe the completion before the slot exists.
    pub(crate) fn register(&self, token: u16) {
        let mut slots = self.slots.lock();
        let slot = Self::slot(&mut slots, token);
        assert_eq!(
            *slot,
            Slot::Idle,
            "virtio descriptor {token} was submitted while still in flight"
        );
        *slot = Slot::Pending;
    }

    /// Records a completion the caller drained from the queue.
    pub(crate) fn complete(&self, token: u16, len: u32) {
        let mut slots = self.slots.lock();
        let slot = Self::slot(&mut slots, token);
        assert_eq!(
            *slot,
            Slot::Pending,
            "virtio device completed descriptor {token}, which no request was waiting on"
        );
        *slot = Slot::Complete(len);
    }

    /// Takes the completion for `token` if it has arrived.
    pub(crate) fn take(&self, token: u16) -> Option<u32> {
        let mut slots = self.slots.lock();
        let slot = Self::slot(&mut slots, token);
        match *slot {
            Slot::Complete(len) => {
                *slot = Slot::Idle;
                Some(len)
            }
            _ => None,
        }
    }

    fn slot(slots: &mut [Slot; N], token: u16) -> &mut Slot {
        slots.get_mut(usize::from(token)).unwrap_or_else(|| {
            panic!("virtio descriptor {token} is outside the {N}-slot completion table")
        })
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
/// has already published, which is what recycles descriptors, and then
/// parks on the table's own notification until a waiter frees the chain
/// or collects the completion it needs.
pub(crate) async fn submit_chain<T, const N: usize>(
    inflight: &InFlight<N>,
    queue: &AsyncMutex<VirtQueue<T>>,
    interrupts: &Notify,
    transport: &T,
    inputs: &[&[u8]],
    outputs: &mut [&mut [u8]],
) -> IoResult<u16>
where
    T: VirtioTransport,
{
    let buffers = inputs.len() + outputs.len();
    let mut announced = false;
    let outcome = loop {
        let drained = {
            let mut queue = queue.lock().await;
            if queue.has_room_for(buffers) && inflight.is_idle(queue.next_free_descriptor()) {
                match queue.submit(transport, inputs, outputs) {
                    Ok(token) => {
                        queue.notify(transport);
                        inflight.register(token);
                        break Ok(token);
                    }
                    Err(error) => break Err(error),
                }
            }
            queue.drain_used(|completed, len| inflight.complete(completed, len))
        };
        // A notification is handed to a single claimant, so a drain that
        // finished other tasks' requests owes one wake-up per task.
        for _ in 0..drained {
            interrupts.notify_all();
        }
        inflight.note_released(drained);
        if drained != 0 {
            continue;
        }
        if !announced {
            inflight.announce_blocked();
            announced = true;
            continue;
        }
        inflight.available.notified().await;
    };
    if announced {
        inflight.release_blocked();
    }
    outcome
}

/// Waits for `token`'s completion, draining the queue on behalf of every
/// waiter whenever this task can take it.
///
/// This is the shape every single-request driver uses: a task parks on
/// the device's interrupt notification, and whichever task wakes first
/// reaps the whole used ring into the completion table rather than
/// looking for its own descriptor. `wait` is a parameter so callers can
/// park on something other than the raw interrupt — a deadline, say —
/// without duplicating the loop.
pub(crate) async fn await_completion<T, Wait, WaitFuture, const N: usize>(
    inflight: &InFlight<N>,
    queue: &AsyncMutex<VirtQueue<T>>,
    interrupts: &Notify,
    token: u16,
    mut wait: Wait,
) -> u32
where
    T: VirtioTransport,
    Wait: FnMut() -> WaitFuture,
    WaitFuture: Future<Output = ()>,
{
    loop {
        if let Some(len) = inflight.take(token) {
            // This request's chain went back to the ring when its
            // completion was drained, and its slot is idle again now.
            // A submitter parked on either has nothing else to wake it.
            inflight.note_released(1);
            return len;
        }

        let mut others = 0_usize;
        let drained = match queue.try_lock() {
            Some(mut queue) => queue.drain_used(|completed, len| {
                if completed != token {
                    others += 1;
                }
                inflight.complete(completed, len);
            }),
            None => 0,
        };
        // A notification is handed to a single claimant, so a drain that
        // finished several other tasks' requests owes one wake-up per
        // task rather than one for the whole batch.
        for _ in 0..others {
            interrupts.notify_all();
        }
        inflight.note_released(drained);
        if drained != 0 {
            continue;
        }

        wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::InFlight;

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
    #[should_panic(expected = "which no request was waiting on")]
    fn a_completion_without_a_waiter_is_a_device_fault() {
        let inflight: InFlight<2> = InFlight::new();
        inflight.complete(1, 0);
    }
}
