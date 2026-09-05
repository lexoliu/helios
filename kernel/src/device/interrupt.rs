//! Forwarding one granted device's interrupts to its owner.
//!
//! The kernel-side handler does the least it can: it holds the source
//! off at the controller, records that the source fired, and wakes the
//! owner. Every decision about what the device meant by it is the
//! owner's, and is taken in user memory.
//!
//! # Why the source is masked before it is forwarded
//!
//! A level-triggered device keeps its line asserted until its driver
//! clears the condition in a register, and that driver is a wasm
//! instance that has not been scheduled yet. Leaving the source enabled
//! would re-enter the handler as fast as the controller can deliver it
//! and no other task would ever run. Masking on delivery bounds the
//! kernel's work at one handler entry per interrupt, and hands the owner
//! the responsibility it already has: unmask when the device has been
//! serviced.
//!
//! Masking also makes the pending set bounded by construction. At most
//! one delivery per source can be outstanding, so a relay that can hold
//! one event per source can never fail to queue one, and no interrupt is
//! ever dropped for want of room.
//!
//! # Concurrency contract
//!
//! [`InterruptRelay::forward`] runs in interrupt context on whichever
//! processor the controller picked; it takes no lock and allocates
//! nothing. [`InterruptRelay::next_event`] is awaited by the owner's
//! task on whichever processor is running it, and arms its wake-up
//! before it inspects the queue, so an interrupt taken between the
//! inspection and the park is not lost.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use arrayvec::ArrayVec;
use concurrent_queue::ConcurrentQueue;

use super::grant::{GrantError, GrantInterrupt, MAX_GRANT_INTERRUPTS};
use super::platform::device_interrupt_hooks;
use crate::Notify;

/// One delivery of one of a granted device's interrupts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptEvent {
    /// Which of the grant's interrupts fired, as an index into
    /// [`DeviceGrant::interrupts`](super::DeviceGrant::interrupts). The
    /// owner never sees the platform's own numbering.
    pub index: u32,
    /// How many deliveries of this interrupt the relay has forwarded,
    /// this one included. A gap between two events the owner sees is
    /// coalescing it can measure rather than infer.
    pub sequence: u64,
}

/// What one granted device's interrupts have done, for the stats panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterruptStats {
    /// Deliveries handed to the owner.
    pub forwarded: u64,
    /// Deliveries that arrived while the owner still had the previous
    /// one outstanding. The device re-asserted before its driver
    /// acknowledged, which the driver sees as a sequence gap.
    pub coalesced: u64,
    /// Sources currently held off at the controller.
    pub masked: u32,
}

/// The kernel side of one granted device's interrupts.
///
/// Built when discovery publishes the grant, so it outlives every owner
/// the device has: a backend registers its interrupt route against the
/// relay once, at boot, and the route stays valid across an owner's
/// death and restart.
pub struct InterruptRelay {
    sources: ArrayVec<GrantInterrupt, MAX_GRANT_INTERRUPTS>,
    /// At most one event per source, which is what masking on delivery
    /// guarantees.
    pending: ConcurrentQueue<InterruptEvent>,
    /// One permit per queued event, for the single owner.
    ready: Notify,
    /// Set from the moment a source's event is queued until the owner
    /// acknowledges it.
    outstanding: [AtomicBool; MAX_GRANT_INTERRUPTS],
    /// Whether the controller is currently holding the source off.
    masked: [AtomicBool; MAX_GRANT_INTERRUPTS],
    /// Deliveries per source, which is what an event's sequence carries.
    deliveries: [AtomicU64; MAX_GRANT_INTERRUPTS],
    forwarded: AtomicU64,
    coalesced: AtomicU64,
}

impl InterruptRelay {
    /// A relay over `sources`, with every source masked.
    ///
    /// A device nobody owns raises nothing: the relay starts masked and
    /// the first owner unmasks what it wants when it is ready to be
    /// interrupted.
    pub fn new(sources: &[GrantInterrupt]) -> Self {
        let sources: ArrayVec<GrantInterrupt, MAX_GRANT_INTERRUPTS> =
            sources.iter().copied().collect();
        let capacity = sources.len().max(1);
        Self {
            sources,
            pending: ConcurrentQueue::bounded(capacity),
            ready: Notify::new(),
            outstanding: [const { AtomicBool::new(false) }; MAX_GRANT_INTERRUPTS],
            masked: [const { AtomicBool::new(true) }; MAX_GRANT_INTERRUPTS],
            deliveries: [const { AtomicU64::new(0) }; MAX_GRANT_INTERRUPTS],
            forwarded: AtomicU64::new(0),
            coalesced: AtomicU64::new(0),
        }
    }

    pub fn sources(&self) -> &[GrantInterrupt] {
        &self.sources
    }

    /// The index `source` occupies in this grant, if it belongs to it.
    pub fn index_of(&self, source: GrantInterrupt) -> Option<usize> {
        self.sources.iter().position(|held| *held == source)
    }

    /// Hold `source` off at the controller and hand its delivery to the
    /// owner.
    ///
    /// Called from interrupt context. Returns false when the source does
    /// not belong to this grant, which lets a backend fail fast with
    /// controller context in the message rather than swallowing a
    /// misrouted line.
    #[must_use]
    pub fn forward(&self, source: GrantInterrupt) -> bool {
        let Some(index) = self.index_of(source) else {
            return false;
        };
        self.mask_index(index);
        let sequence = self.deliveries[index].fetch_add(1, Ordering::AcqRel) + 1;
        if self.outstanding[index].swap(true, Ordering::AcqRel) {
            // The owner has not acknowledged the previous delivery. The
            // source is masked either way, so nothing is lost but the
            // separate wake-up; the sequence the owner eventually reads
            // records how many deliveries the one event stands for.
            self.coalesced.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        let event = InterruptEvent {
            index: index as u32,
            sequence,
        };
        self.pending.push(event).unwrap_or_else(|_| {
            panic!(
                "device interrupt relay had no room for source {source}, \
                 which masking on delivery should have made impossible"
            )
        });
        self.forwarded.fetch_add(1, Ordering::Relaxed);
        self.ready.notify_one();
        true
    }

    /// Await the next delivery.
    ///
    /// The wake-up is armed before the queue is inspected, so an
    /// interrupt taken on another processor between the inspection and
    /// the park is not lost.
    pub async fn next_event(&self) -> InterruptEvent {
        loop {
            let ready = self.ready.notified();
            if let Ok(event) = self.pending.pop() {
                return event;
            }
            ready.await;
        }
    }

    /// The next delivery if one is already queued, without parking.
    pub fn try_next_event(&self) -> Option<InterruptEvent> {
        self.pending.pop().ok()
    }

    /// Acknowledge the delivery of `index`: the owner has read whatever
    /// the device had to say, and a further assertion is a new event.
    ///
    /// Acknowledging does not unmask. A driver that has read the device
    /// but is not ready to be interrupted again keeps the source held
    /// off, which is the whole reason the two are separate calls.
    pub fn ack(&self, index: usize) -> Result<(), GrantError> {
        self.check(index)?;
        self.outstanding[index].store(false, Ordering::Release);
        Ok(())
    }

    /// Hold `index` off at the controller.
    pub fn mask(&self, index: usize) -> Result<(), GrantError> {
        self.check(index)?;
        self.mask_index(index);
        Ok(())
    }

    /// Let the controller deliver `index` again.
    pub fn unmask(&self, index: usize) -> Result<(), GrantError> {
        self.check(index)?;
        if self.masked[index].swap(false, Ordering::AcqRel) {
            (device_interrupt_hooks().unmask)(self.sources[index].raw());
        }
        Ok(())
    }

    /// Hold every source off and forget every pending delivery.
    ///
    /// This is what an owner's death runs: the device keeps asserting
    /// whatever it was asserting, and nothing in the kernel reacts to it
    /// until a new owner unmasks.
    pub fn quiesce(&self) {
        for index in 0..self.sources.len() {
            self.mask_index(index);
            self.outstanding[index].store(false, Ordering::Release);
        }
        while self.pending.pop().is_ok() {}
    }

    pub fn stats(&self) -> InterruptStats {
        InterruptStats {
            forwarded: self.forwarded.load(Ordering::Relaxed),
            coalesced: self.coalesced.load(Ordering::Relaxed),
            masked: self
                .masked
                .iter()
                .take(self.sources.len())
                .filter(|masked| masked.load(Ordering::Relaxed))
                .count() as u32,
        }
    }

    fn mask_index(&self, index: usize) {
        if !self.masked[index].swap(true, Ordering::AcqRel) {
            (device_interrupt_hooks().mask)(self.sources[index].raw());
        }
    }

    fn check(&self, index: usize) -> Result<(), GrantError> {
        if index >= self.sources.len() {
            return Err(GrantError::NoSuchInterrupt);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{GrantInterrupt, InterruptRelay};
    use crate::device::platform::test_hooks;
    use futures_lite::future::block_on;

    fn relay() -> InterruptRelay {
        test_hooks::install();
        InterruptRelay::new(&[GrantInterrupt::new(33), GrantInterrupt::new(34)])
    }

    #[test]
    fn a_delivery_masks_its_source_and_reaches_the_owner() {
        let relay = relay();
        relay.unmask(0).expect("the source exists");

        assert!(relay.forward(GrantInterrupt::new(33)));

        let event = relay.try_next_event().expect("the delivery is queued");
        assert_eq!(event.index, 0);
        assert_eq!(event.sequence, 1);
        assert_eq!(relay.stats().forwarded, 1);
        assert_eq!(relay.stats().masked, 2, "the untouched source stays masked");
        // The controller saw both halves: the unmask that armed the
        // source and the mask the delivery imposed.
        assert!(test_hooks::unmasked().contains(&33));
        assert!(test_hooks::masked().contains(&33));
    }

    #[test]
    fn a_source_that_belongs_to_another_device_is_refused() {
        let relay = relay();

        assert!(!relay.forward(GrantInterrupt::new(99)));
    }

    #[test]
    fn a_re_assertion_before_the_owner_acknowledges_is_counted_not_dropped() {
        let relay = relay();
        relay.unmask(0).expect("the source exists");

        assert!(relay.forward(GrantInterrupt::new(33)));
        assert!(relay.forward(GrantInterrupt::new(33)));

        let event = relay.try_next_event().expect("one event stands for both");
        assert_eq!(event.sequence, 1);
        assert!(relay.try_next_event().is_none());
        assert_eq!(relay.stats().coalesced, 1);

        // Once acknowledged, the next assertion is a fresh delivery and
        // its sequence records that two deliveries came before it.
        relay.ack(0).expect("the source exists");
        assert!(relay.forward(GrantInterrupt::new(33)));
        assert_eq!(
            relay.try_next_event().expect("a fresh delivery").sequence,
            3
        );
    }

    /// The delivery happens on whichever processor the controller
    /// picked, and the owner is parked on another. The wake-up has to
    /// survive the gap between the owner's inspection and its park,
    /// which is what arming first buys.
    #[test]
    fn an_interrupt_taken_while_the_owner_parks_still_wakes_it() {
        let relay = relay();

        block_on(async {
            let owner = async { relay.next_event().await };
            let controller = async {
                // Yields first, so the owner has inspected the empty
                // queue and armed before this runs.
                crate::yield_now().await;
                assert!(relay.forward(GrantInterrupt::new(34)));
            };
            let (event, ()) = futures::future::join(owner, controller).await;
            assert_eq!(event.index, 1);
        });
    }

    #[test]
    fn quiescing_masks_every_source_and_forgets_every_pending_delivery() {
        let relay = relay();
        relay.unmask(0).expect("the source exists");
        relay.unmask(1).expect("the source exists");
        assert!(relay.forward(GrantInterrupt::new(33)));

        relay.quiesce();

        assert!(relay.try_next_event().is_none());
        assert_eq!(relay.stats().masked, 2);
    }

    #[test]
    fn an_interrupt_index_the_grant_does_not_have_is_refused() {
        let relay = relay();

        assert!(relay.ack(7).is_err());
        assert!(relay.mask(7).is_err());
        assert!(relay.unmask(7).is_err());
    }
}
