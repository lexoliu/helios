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
//! Concurrency contract: the table is a lock-free-to-the-caller spin
//! mutex over a fixed slot array. It is never held across an await, and
//! it is deliberately independent of the queue lock so a task that
//! failed to take the queue can still observe a completion another task
//! drained for it.

use core::future::Future;

use async_lock::Mutex as AsyncMutex;
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
}

impl<const N: usize> InFlight<N> {
    pub(crate) const fn new() -> Self {
        Self {
            slots: Mutex::new([Slot::Idle; N]),
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
    #[should_panic(expected = "which no request was waiting on")]
    fn a_completion_without_a_waiter_is_a_device_fault() {
        let inflight: InFlight<2> = InFlight::new();
        inflight.complete(1, 0);
    }
}
