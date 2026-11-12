//! A driver's record of what its device has acknowledged.
//!
//! A device that is reset loses the state its commands built, so a driver
//! keeps a record of that state and replays it. The record has to hold
//! exactly what the device has *acknowledged* at the moment the machine
//! is frozen, and two things make that hard when a command is a request
//! some task awaits:
//!
//! * A record written before the command is sent describes state the
//!   device may refuse, and one the device never saw if the task parked
//!   before the command was submitted.
//! * A record written when the waiter wakes misses a command that
//!   completed during the freeze: the device answered it, the quiesce
//!   reaped it, and the waiter has not run.
//!
//! A [`Journal`] closes both. The command is submitted with a response
//! buffer the journal owns, and the change is filed with the token the
//! device was handed, in the same step that publishes the chain. Whoever
//! first sees the answer settles the change, exactly once: the waiter
//! when it wakes, or a quiesce that reaped the completion for it. A
//! change applies only when the answer says the device accepted it, and
//! a command that was never submitted has filed nothing.
//!
//! The command's bytes travel in the control slot of the chain's head
//! descriptor, like every other command on the queue. Its answer does
//! not: a control slot is the request's only while its identifier is
//! held, and a request dropped mid-flight lets its identifier go at the
//! device's completion, after which another request may stage into the
//! slot before a quiesce reads the answer. The answer buffer is device
//! memory the [`Sent`] owns instead, drawn from the device's
//! [`DmaMemory`] when the command is sent, so it outlives the
//! identifier for as long as the journal or the waiter holds the record.
//! The device stops writing it when it completes the chain, and the
//! table keeps a record until a later command is filed under the same
//! identifier, which the completion table allows only once the device has
//! returned it; a waiter whose queue faulted leaves its record filed for
//! the same reason, so a device that broke the ring contract never
//! writes into a buffer that was freed.
//!
//! Concurrency contract: the table is a spin mutex over one slot per
//! descriptor identifier, held for one slot update and never across a
//! queue call or an await. A [`Sent`] is shared by the table and its
//! waiter; its answer is read only after the device completed the chain,
//! the settled flag has a lock of its own, and `apply` runs under neither
//! the table lock nor a queue lock the caller could be holding. Changes to
//! one object (a stream, a resource) apply in the order they are settled,
//! which for commands one task awaits after another is their submission
//! order; two commands on the same object in flight at once from
//! different tasks have no order a driver could preserve, and callers do
//! not issue them.

use core::alloc::Layout;

use alloc::sync::Arc;
use alloc::vec::Vec;

use arrayvec::ArrayVec;
use async_lock::Mutex as AsyncMutex;
use helios_hal::dma::{DmaError, DmaMemory};
use helios_hal::io::{IoError, IoResult};
use helios_hal::iommu::DmaAccess;
use spin::Mutex;

use crate::bus::DeviceBus;
use crate::inflight::{InFlight, Ticket, submit_control};
use crate::queue::{ControlSlots, MAX_CHAIN_BUFFERS, Part, TransportBuffer, VirtQueue};
use crate::transport::VirtioTransport;

/// One submitted command and the change its acknowledgement applies.
pub(crate) struct Sent<C, T: VirtioTransport> {
    token: u16,
    change: C,
    /// Where the device writes its answer, or `None` for a command that
    /// carries none. Owned here rather than by the waiter so that a
    /// quiesce, which reaps the completion on the waiter's behalf, can
    /// read it.
    response: Option<TransportBuffer<T>>,
    /// `None` until settled, then whether the device accepted the
    /// command.
    settled: Mutex<Option<bool>>,
}

impl<C, T: VirtioTransport> Sent<C, T> {
    /// The device's answer. Read only once the device completed the chain.
    fn answer(&self) -> &[u8] {
        self.response.as_ref().map_or(&[], AsRef::as_ref)
    }

    /// Applies the change if the answer in the response buffer is one
    /// `accepted` approves, and answers whether it was. The first caller
    /// decides; later ones get the same answer and apply nothing.
    pub(crate) fn settle(
        &self,
        accepted: impl FnOnce(&[u8]) -> bool,
        apply: impl FnOnce(&C),
    ) -> bool {
        let mut settled = self.settled.lock();
        if let Some(outcome) = *settled {
            return outcome;
        }
        let outcome = accepted(self.answer());
        if outcome {
            apply(&self.change);
        }
        *settled = Some(outcome);
        outcome
    }

    /// A copy of the device's answer.
    pub(crate) fn response(&self) -> Vec<u8> {
        self.answer().to_vec()
    }
}

/// The record filed under one descriptor identifier, if any.
type Filed<C, T> = Option<Arc<Sent<C, T>>>;

/// The commands a driver has submitted whose answers have not been
/// collected, one slot per descriptor identifier of the queue they ride.
pub(crate) struct Journal<C, T: VirtioTransport, const N: usize> {
    sent: Mutex<[Filed<C, T>; N]>,
}

impl<C, T: VirtioTransport, const N: usize> Journal<C, T, N> {
    pub(crate) const fn new() -> Self {
        Self {
            sent: Mutex::new([const { None }; N]),
        }
    }

    /// Submits one command whose answer is `response_bytes` long, and
    /// files `change` under the token the device was handed.
    ///
    /// `inputs` are copied into the chain's control slot. The chain is
    /// published and filed in the same poll, so a freeze finds either both
    /// or neither: a task parked before the chain went out has filed
    /// nothing. Returns the record and the ticket the caller awaits.
    #[expect(
        clippy::too_many_arguments,
        reason = "a submission names the queue's completion table, ring, transport and control \
                  slots as `submit_control` does, and a journaled command adds its bytes, its \
                  answer length and its change; a struct would exist only to carry them here"
    )]
    pub(crate) async fn send<'a>(
        &self,
        inflight: &'a InFlight<N>,
        queue: &AsyncMutex<VirtQueue<T>>,
        transport: &T,
        slots: &'a ControlSlots<TransportBuffer<T>>,
        inputs: &[&[u8]],
        response_bytes: usize,
        change: C,
    ) -> IoResult<(Arc<Sent<C, T>>, Ticket<'a, N>)> {
        let mut response = if response_bytes == 0 {
            None
        } else {
            Some(answer_buffer(transport.bus().dma(), response_bytes).await?)
        };
        // The chain names the buffer's device address, which does not
        // move with the value when the record takes it below.
        let ticket = {
            let mut parts: ArrayVec<Part<'_>, MAX_CHAIN_BUFFERS> = ArrayVec::new();
            for bytes in inputs {
                parts.push(Part::Request(bytes));
            }
            if let Some(response) = response.as_mut() {
                parts.push(Part::Write(&mut response.as_mut()[..response_bytes]));
            }
            submit_control(inflight, queue, transport, slots, &parts).await?
        };
        let sent = Arc::new(Sent {
            token: ticket.token(),
            change,
            response,
            settled: Mutex::new(None),
        });
        self.sent.lock()[usize::from(sent.token)] = Some(sent.clone());
        Ok((sent, ticket))
    }

    /// Settles every command the device has answered and nobody has
    /// collected, for a quiesce that reaped their completions. Only
    /// called once the queue holds no chain, so every command filed here
    /// has been answered.
    pub(crate) fn settle_all(&self, accepted: impl Fn(&[u8]) -> bool, apply: impl Fn(&C)) {
        let pending: Vec<Arc<Sent<C, T>>> = self.sent.lock().iter().flatten().cloned().collect();
        for sent in pending {
            sent.settle(&accepted, &apply);
        }
    }

    /// Forgets `sent`, once its waiter has the answer. A waiter whose
    /// command did not come back leaves it filed: see the module
    /// documentation.
    pub(crate) fn forget(&self, sent: &Arc<Sent<C, T>>) {
        let mut table = self.sent.lock();
        let slot = &mut table[usize::from(sent.token)];
        if slot.as_ref().is_some_and(|held| Arc::ptr_eq(held, sent)) {
            *slot = None;
        }
    }
}

/// Device memory for a command's answer of `bytes`, waited for while the
/// device's memory is short, as [`DmaMemory::want_space`] says.
async fn answer_buffer<P: DmaMemory>(dma: &P, bytes: usize) -> IoResult<P::Buffer> {
    let layout = Layout::array::<u8>(bytes).map_err(|_| IoError::OutOfBounds)?;
    let mut declared = None;
    loop {
        match dma.allocate(layout, DmaAccess::WRITE) {
            Ok(buffer) => return Ok(buffer),
            Err(DmaError::Replenishing { .. } | DmaError::Exhausted { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        match declared.take() {
            // Declared, then asked again below: a refusal after the
            // declaration is the one worth parking on.
            None => declared = Some(dma.want_space()),
            Some(mark) => dma.space_changed(mark).await,
        }
    }
}
