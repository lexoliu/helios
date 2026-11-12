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
//! # Parked buffers
//!
//! The data buffers a request names are not borrowed from the request's
//! future: the request moves them into the table ([`Lend`]) when it
//! submits, and they stay in the slot of the chain's head descriptor until
//! the device has finished with them. This is the ownership
//! [`helios_hal::dma`] states for every request that transfers data, and
//! the reason the slot table is typed by what its requests lend:
//!
//! - The request that collects a completion takes its buffers back with
//!   the [`Reply`] and hands them to its caller.
//! - A request dropped after submission leaves them parked with its
//!   `Abandoned` slot; the device's completion drops them, which is the
//!   first moment their memory may be reused.
//! - A request dropped after its completion arrived but before it was
//!   collected drops them at once: the device is done.
//! - A request still in flight when the queue faults keeps them parked,
//!   since the device that broke the ring contract may still be using
//!   them, until the driver is dropped, which resets the queue first.
//!
//! The storage is the slot array itself, one slot per descriptor, so
//! lending allocates nothing.
//!
//! # Concurrency contract
//!
//! The table is a spin mutex over a fixed slot array, never held across
//! an await and never held while a waker runs or a parked buffer is
//! dropped: both are taken out under the lock and released after it. It
//! is deliberately independent of the queue lock so a task that failed to
//! take the queue can still observe a completion another task drained for
//! it. A waiter is woken through its own slot, a submitter through this
//! table's notification, so neither can consume the other's wake-up.
//!
//! A parked buffer is touched by exactly two parties, both from task
//! context on any processor and never from an interrupt handler (the
//! handlers only acknowledge the device and notify): the submitter, which
//! moves it in under the queue lock as it registers the chain, and then
//! either the waiter that collects the completion or the task that drains
//! an abandoned chain's completion, which moves it out under the table
//! lock. Nobody reads or writes a parked buffer's bytes while it is
//! parked; the device owns them.

use core::future::{Future, poll_fn};
use core::pin::pin;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};

use async_lock::Mutex as AsyncMutex;
use helios_hal::dma::BufError;
use helios_hal::io::{IoError, IoResult};
use spin::Mutex;

use crate::notify::Notify;
use crate::queue::{ControlSlots, Lend, Part, ResponseWindow, TransportBuffer, VirtQueue};
use crate::resume::{QuiesceError, ReinitError, Resumed, settle};
use crate::transport::VirtioTransport;

/// State of one descriptor identifier, with the buffers its request lent.
enum Slot<L> {
    /// No request is using this identifier.
    Idle,
    /// A request is in flight, together with the waker of the task
    /// waiting on it once that task has parked, and what it lent the
    /// device.
    Pending { waker: Option<Waker>, lent: L },
    /// The device finished the request, writing this many bytes. What the
    /// request lent stays here until its waiter collects it.
    Complete { len: u32, lent: L },
    /// The waiter collected the completion and took its buffers back. The
    /// slot stays held until the [`Reply`] that reads it is dropped.
    Collected,
    /// The request that submitted this chain was dropped before the
    /// device finished it. The identifier, the control slot it names and
    /// what the request lent stay held until the device completes the
    /// chain: it may still be reading and writing all of them.
    Abandoned(L),
}

/// Completion slots for up to `N` concurrently in-flight requests, each of
/// which lends the device an `L`.
pub(crate) struct InFlight<const N: usize, L = ()> {
    slots: Mutex<[Slot<L>; N]>,
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

impl<const N: usize, L: Lend> InFlight<N, L> {
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
    /// is no longer driven, and leaves what it lent parked.
    pub(crate) fn fail(&self) {
        {
            let _slots = self.slots.lock();
            self.faulted.store(true, Ordering::SeqCst);
        }
        for token in 0..N {
            let waker = {
                let mut slots = self.slots.lock();
                match &mut slots[token] {
                    Slot::Pending { waker, .. } => waker.take(),
                    Slot::Idle | Slot::Complete { .. } | Slot::Collected | Slot::Abandoned(_) => {
                        None
                    }
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
    /// wakes its waiter, which takes back what it lent.
    ///
    /// For a queue whose chains are buffers posted for the device to
    /// answer whenever it chooses (a balloon's statistics buffer): when
    /// the device is reset those chains are gone and will never be
    /// answered, so their waiters are released to post a fresh one
    /// instead of parking for ever. A chain whose request was dropped
    /// would have been freed by its completion, which will not come
    /// either: the reset device holds none of its buffers, so they are
    /// dropped and the identifier is free again.
    pub(crate) fn abandon_pending(&self) {
        for token in 0..N {
            let (waker, freed) = {
                let mut slots = self.slots.lock();
                let slot = &mut slots[token];
                match core::mem::replace(slot, Slot::Idle) {
                    Slot::Pending { waker, lent } => {
                        *slot = Slot::Complete { len: 0, lent };
                        (waker, None)
                    }
                    Slot::Abandoned(lent) => (None, Some(lent)),
                    held @ (Slot::Idle | Slot::Complete { .. } | Slot::Collected) => {
                        *slot = held;
                        (None, None)
                    }
                }
            };
            // As in `complete`: neither a waker nor a buffer's drop runs
            // under the table lock.
            drop(freed);
            if let Some(waker) = waker {
                waker.wake();
            }
        }
        self.available.notify_all();
    }

    /// Claims `token` for the caller and parks what it lent there. Must
    /// be called while the queue lock that produced the token is still
    /// held, so no drain can observe the completion before the slot
    /// exists.
    pub(crate) fn register(&self, token: u16, lent: L) {
        let mut slots = self.slots.lock();
        let slot = Self::slot(&mut slots, token);
        assert!(
            matches!(slot, Slot::Idle),
            "virtio descriptor {token} was submitted while still in flight"
        );
        *slot = Slot::Pending { waker: None, lent };
    }

    /// Records a completion the caller drained from the queue and wakes
    /// the task waiting on it, which may not be the caller. An abandoned
    /// chain's completion frees its slot and drops what it lent.
    pub(crate) fn complete(&self, token: u16, len: u32) {
        let (waker, freed) = {
            let mut slots = self.slots.lock();
            let slot = Self::slot(&mut slots, token);
            match core::mem::replace(slot, Slot::Idle) {
                // Nobody will read the reply of an abandoned request, so
                // the identifier and the buffers are free the moment the
                // device is done.
                Slot::Abandoned(lent) => (None, Some(lent)),
                Slot::Pending { waker, lent } => {
                    *slot = Slot::Complete { len, lent };
                    (waker, None)
                }
                Slot::Idle | Slot::Complete { .. } | Slot::Collected => panic!(
                    "virtio device completed descriptor {token}, which no request was waiting on"
                ),
            }
        };
        // The slot lock is released first: a waker runs executor code and
        // a buffer's drop runs its allocator, and neither may be entered
        // from inside this table.
        drop(freed);
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// The completion for `token` and what its request lent, if it has
    /// arrived. The slot stays occupied until [`InFlight::release`].
    fn collect(&self, token: u16) -> Option<(u32, L)> {
        let mut slots = self.slots.lock();
        let slot = Self::slot(&mut slots, token);
        if !matches!(slot, Slot::Complete { .. }) {
            return None;
        }
        match core::mem::replace(slot, Slot::Collected) {
            Slot::Complete { len, lent } => Some((len, lent)),
            _ => unreachable!("the slot was complete"),
        }
    }

    /// Lets go of a collected completion, which frees the identifier, and
    /// with it the control slot, for the next request.
    fn release(&self, token: u16) {
        {
            let mut slots = self.slots.lock();
            let slot = Self::slot(&mut slots, token);
            assert!(
                matches!(slot, Slot::Collected),
                "virtio descriptor {token} was released without a collected completion"
            );
            *slot = Slot::Idle;
        }
        self.note_released(1);
    }

    /// Records that the request waiting on `token` was dropped.
    ///
    /// A chain still in flight keeps its identifier, and what it lent,
    /// until the device completes it; one whose completion had already
    /// arrived is freed now, buffers included, since nobody will read it.
    fn abandon(&self, token: u16) {
        let freed = {
            let mut slots = self.slots.lock();
            let slot = Self::slot(&mut slots, token);
            match core::mem::replace(slot, Slot::Idle) {
                Slot::Pending { lent, .. } => {
                    *slot = Slot::Abandoned(lent);
                    None
                }
                Slot::Complete { lent, .. } => Some(lent),
                Slot::Idle | Slot::Collected | Slot::Abandoned(_) => {
                    panic!("virtio descriptor {token} was abandoned while no request owned it")
                }
            }
        };
        if let Some(lent) = freed {
            drop(lent);
            self.note_released(1);
        }
    }

    #[cfg(test)]
    pub(crate) fn take(&self, token: u16) -> Option<u32> {
        let (len, _lent) = self.collect(token)?;
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
            Slot::Complete { .. } => true,
            Slot::Pending { waker: parked, .. } => {
                match parked {
                    Some(parked) if parked.will_wake(waker) => {}
                    parked => *parked = Some(waker.clone()),
                }
                false
            }
            Slot::Idle | Slot::Collected | Slot::Abandoned(_) => {
                panic!("virtio descriptor {token} was awaited while not in flight")
            }
        }
    }

    fn slot(slots: &mut [Slot<L>; N], token: u16) -> &mut Slot<L> {
        slots.get_mut(usize::from(token)).unwrap_or_else(|| {
            panic!("virtio descriptor {token} is outside the {N}-slot completion table")
        })
    }
}

/// A request's claim on the descriptor identifier its chain was headed by.
///
/// Dropping it, whether the request is cancelled or fails before the
/// device answers, abandons the chain: the identifier, with it the
/// control slot, and what the request lent are held until the device
/// completes the chain, and freed by that completion rather than by a
/// future that no longer exists.
#[must_use = "a dropped ticket abandons its request"]
pub(crate) struct Ticket<'a, const N: usize, L: Lend = ()> {
    inflight: &'a InFlight<N, L>,
    token: u16,
    response: ResponseWindow<'a>,
}

impl<const N: usize, L: Lend> Ticket<'_, N, L> {
    /// The descriptor identifier the chain was headed by.
    pub(crate) fn token(&self) -> u16 {
        self.token
    }
}

impl<const N: usize, L: Lend> Drop for Ticket<'_, N, L> {
    fn drop(&mut self) {
        self.inflight.abandon(self.token);
    }
}

/// A finished request, with the response bytes the device wrote and the
/// buffers the request lent it.
///
/// The control slot stays held until this is dropped, so the bytes cannot
/// be overwritten by a later request while they are read.
#[must_use = "the slot stays held until the reply is dropped"]
pub(crate) struct Reply<'a, const N: usize, L: Lend = ()> {
    inflight: &'a InFlight<N, L>,
    token: u16,
    len: u32,
    response: ResponseWindow<'a>,
    /// What the request lent, back from the device. `None` once
    /// [`Reply::into_lent`] has taken it.
    lent: Option<L>,
}

impl<const N: usize, L: Lend> Reply<'_, N, L> {
    /// Bytes the device reported writing into the chain.
    pub(crate) fn written(&self) -> u32 {
        self.len
    }

    /// The bytes the device wrote to the chain's slot response area.
    pub(crate) fn response(&self) -> &[u8] {
        // SAFETY: the completion arrived and this reply holds the slot.
        unsafe { self.response.bytes() }
    }

    /// The buffers the request lent the device, which it has finished
    /// with. Releases the slot.
    pub(crate) fn into_lent(mut self) -> L {
        self.lent
            .take()
            .expect("a reply holds what its request lent until it is taken")
    }
}

impl<const N: usize, L: Lend> Drop for Reply<'_, N, L> {
    fn drop(&mut self) {
        self.inflight.release(self.token);
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
pub(crate) fn drain_to_idle<T, L, const N: usize>(
    inflight: &InFlight<N, L>,
    queue: &AsyncMutex<VirtQueue<T>>,
    transport: &T,
    queue_index: u16,
    expired: &mut impl FnMut() -> bool,
) -> Result<(), QuiesceError>
where
    T: VirtioTransport,
    L: Lend,
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
pub(crate) fn reinitialize_posted<T, L, const N: usize>(
    inflight: &InFlight<N, L>,
    queue: &AsyncMutex<VirtQueue<T>>,
    transport: &T,
    queue_index: u16,
) -> Result<(), ReinitError>
where
    T: VirtioTransport,
    L: Lend,
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

/// Places the chain `parts` describes, with its control bytes staged in
/// the slot of the descriptor that heads it.
///
/// This is the submission half of the shape every request/response
/// driver uses. The queue lock is taken only long enough to publish the
/// chain and register the completion — registering under that lock is
/// what makes the completion reachable, since no other task can drain
/// the token before its waiter exists — and is released before the
/// caller awaits, so several requests are in flight at once.
///
/// The slot is chosen under the queue lock, once the completion table has
/// said the identifier the chain will take is idle, so no other request
/// can be using it. That is the whole of the slot's synchronisation; see
/// the contract in [`crate::queue`].
///
/// Two conditions make a submission wait, and neither is a device
/// fault: the ring has no room for the chain, or the identifier the
/// queue would hand out next still holds a completion its waiter has
/// not collected. In both cases the submitter drains what the device
/// has already published, which is what recycles descriptors and wakes
/// the waiters those completions belong to, and then parks on the
/// table's own notification until a waiter frees the chain or collects
/// the completion it needs.
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
    submit_lent(inflight, queue, transport, slots, parts, ())
        .await
        .map_err(BufError::into_error)
}

/// [`submit_control`] for a request that lends the device `lent`, which
/// the [`Part::LentRead`] and [`Part::LentWrite`] parts name.
///
/// `lent` parks in the table with the chain (see "Parked buffers" above).
/// A submission that fails before the chain is published hands it
/// straight back in the error.
pub(crate) async fn submit_lent<'a, T, L, const N: usize>(
    inflight: &'a InFlight<N, L>,
    queue: &AsyncMutex<VirtQueue<T>>,
    transport: &T,
    slots: &'a ControlSlots<TransportBuffer<T>>,
    parts: &[Part<'_>],
    mut lent: L,
) -> Result<Ticket<'a, N, L>, BufError<L>>
where
    T: VirtioTransport,
    L: Lend,
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
                break Err(BufError::returned(IoError::DeviceFault, lent));
            }
            if queue.has_room_for(parts.len()) && inflight.is_idle(queue.next_free_descriptor()) {
                // SAFETY: the queue lock is held and the completion table
                // reported the identifier the chain will be headed by
                // idle, which is the ownership `submit_parts` asks for.
                match unsafe { queue.submit_parts(transport, slots, parts, &mut lent) } {
                    Ok((token, span)) => {
                        queue.notify(transport);
                        inflight.register(token, lent);
                        break Ok((token, span));
                    }
                    Err(error) => break Err(BufError::returned(error, lent)),
                }
            }
            match queue.drain_used(|completed, len| inflight.complete(completed, len)) {
                Ok(drained) => drained,
                Err(error) => {
                    inflight.fail();
                    break Err(BufError::returned(error, lent));
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
    let (token, span) = outcome?;
    Ok(Ticket {
        inflight,
        token,
        response: slots.window(token, span),
    })
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
/// failing it, abandons the chain instead: the identifier, its control
/// slot and what the request lent stay held until the device completes
/// it. What comes back is a [`Reply`], which keeps the slot held until
/// the caller has read the response bytes out of it and carries the lent
/// buffers back.
pub(crate) async fn await_completion<'a, T, Wait, WaitFuture, L, const N: usize>(
    queue: &AsyncMutex<VirtQueue<T>>,
    ticket: Ticket<'a, N, L>,
    mut wait: Wait,
) -> IoResult<Reply<'a, N, L>>
where
    T: VirtioTransport,
    Wait: FnMut() -> WaitFuture,
    WaitFuture: Future<Output = ()>,
    L: Lend,
{
    let inflight = ticket.inflight;
    let token = ticket.token;
    loop {
        // The device wait is created before the slot and the ring are
        // tested, so a completion that arrives in between is observed
        // by this future rather than slept through.
        let mut notified = pin!(wait());

        if let Some((len, lent)) = inflight.collect(token) {
            // The reply takes over the slot. Its drop frees the
            // identifier, which is also what wakes a submitter parked
            // on it.
            let reply = Reply {
                inflight,
                token,
                len,
                response: ticket.response,
                lent: Some(lent),
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
    use super::{AsyncMutex, InFlight, Part, VirtQueue, await_completion, submit_control};
    use crate::features::{RING_FEATURES, negotiate};
    use crate::notify::Notify;
    use crate::queue::control_slots;
    use crate::testing::{FakeTransport, FakeTransportConfig};
    use crate::transport::{DeviceType, VirtioFeatures};
    use core::pin::pin;
    use core::task::Waker;
    use futures_lite::future::{block_on, poll_once};

    #[test]
    fn a_completion_is_delivered_to_the_registered_token() {
        let inflight: InFlight<4> = InFlight::new();

        inflight.register(2, ());
        assert_eq!(inflight.take(2), None, "nothing has completed yet");

        inflight.complete(2, 96);
        assert_eq!(inflight.take(2), Some(96));
        assert_eq!(inflight.take(2), None, "a completion is delivered once");
    }

    #[test]
    fn completions_are_routed_by_token_not_by_arrival_order() {
        let inflight: InFlight<4> = InFlight::new();

        inflight.register(0, ());
        inflight.register(3, ());
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
        inflight.register(1, ());
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

        inflight.register(1, ());
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

        inflight.register(2, ());
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

        inflight.register(1, ());
        inflight.complete(1, 4);
        inflight.abandon(1);
        assert!(inflight.is_idle(1));
    }

    #[test]
    fn an_abandoned_request_is_released_with_nothing_written() {
        let inflight: InFlight<4> = InFlight::new();
        let waker = Waker::noop();

        inflight.register(0, ());
        inflight.register(2, ());
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
    fn a_dropped_request_on_a_discarded_queue_frees_its_identifier() {
        let inflight: InFlight<4> = InFlight::new();

        inflight.register(1, ());
        inflight.abandon(1);
        assert!(!inflight.is_idle(1), "the device still owns the chain");

        // The reset device owns nothing, and its completion never comes.
        inflight.abandon_pending();

        assert!(inflight.is_idle(1));
        assert_eq!(inflight.take(1), None, "nobody collects a dropped request");
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

    /// The completion has been drained and the ring has recycled the
    /// head descriptor, yet a second request cannot take it: the first
    /// request's reply is still being read out of the control slot the
    /// head names. A queue of one descriptor makes the head the only
    /// one there is.
    #[test]
    fn a_reply_holds_its_identifier_until_it_is_dropped() {
        let transport = FakeTransport::confined(FakeTransportConfig {
            device_type: DeviceType::Entropy,
            offered_features: VirtioFeatures::VERSION_1.bits(),
            queue_size: 1,
            supports_queue_reset: false,
            absent_queues: &[],
        });
        let features = negotiate(&transport, RING_FEATURES).expect("features are negotiated");
        let queue = AsyncMutex::new(
            VirtQueue::new(&transport, 0, 1, 1, features).expect("the queue is built"),
        );
        let slots = control_slots(
            &transport,
            &queue.try_lock().expect("nothing holds the queue lock"),
            0,
            16,
        )
        .expect("the slots are carved");
        let inflight: InFlight<1> = InFlight::new();
        let interrupts = Notify::new();
        let parts = [Part::Response(16)];

        let first = block_on(submit_control(
            &inflight, &queue, &transport, &slots, &parts,
        ))
        .expect("the first request is placed");
        let head = first.token;
        queue
            .try_lock()
            .expect("nothing holds the queue lock")
            .device_complete(head, 16);
        let reply = block_on(await_completion(&queue, first, || interrupts.notified()))
            .expect("the first request completes");
        assert_eq!(reply.written(), 16);
        assert!(
            !inflight.is_idle(head),
            "the identifier stays held while the reply is alive"
        );

        let mut second = pin!(submit_control(
            &inflight, &queue, &transport, &slots, &parts,
        ));
        assert!(
            block_on(poll_once(second.as_mut())).is_none(),
            "a second request cannot take the head while the first reply is alive"
        );

        drop(reply);
        assert!(inflight.is_idle(head), "dropping the reply frees it");
        let second = block_on(poll_once(second.as_mut()))
            .expect("the request that was waiting for the head is placed")
            .expect("the second request is placed");
        assert_eq!(second.token, head);
    }
}
