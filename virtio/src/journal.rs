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
//! Concurrency contract: the table is a spin mutex over one slot per
//! descriptor identifier, held for one slot update and never across a
//! queue call or an await. A [`Sent`] is shared by the table and its
//! waiter; the response buffer and the settled flag each have a lock of
//! their own, and `apply` runs under neither the table lock nor a queue
//! lock the caller could be holding. Changes to one object (a stream, a
//! resource) apply in the order they are settled, which for commands one
//! task awaits after another is their submission order; two commands on
//! the same object in flight at once from different tasks have no order
//! a driver could preserve, and callers do not issue them.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use async_lock::Mutex as AsyncMutex;
use helios_hal::io::IoResult;
use spin::Mutex;

use crate::inflight::{InFlight, submit_chain_with_payload};
use crate::queue::VirtQueue;
use crate::transport::VirtioTransport;

/// One submitted command and the change its acknowledgement applies.
pub(crate) struct Sent<C> {
    token: u16,
    change: C,
    /// Where the device writes its answer. Owned here rather than by the
    /// waiter so that a quiesce, which reaps the completion on the
    /// waiter's behalf, can read it.
    response: Mutex<Box<[u8]>>,
    /// `None` until settled, then whether the device accepted the
    /// command.
    settled: Mutex<Option<bool>>,
}

impl<C> Sent<C> {
    /// The descriptor identifier the command was published under.
    pub(crate) fn token(&self) -> u16 {
        self.token
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
        let outcome = accepted(&self.response.lock());
        if outcome {
            apply(&self.change);
        }
        *settled = Some(outcome);
        outcome
    }

    /// A copy of the device's answer.
    pub(crate) fn response(&self) -> Vec<u8> {
        self.response.lock().to_vec()
    }
}

/// The commands a driver has submitted whose answers have not been
/// collected, one slot per descriptor identifier of the queue they ride.
pub(crate) struct Journal<C, const N: usize> {
    sent: Mutex<[Option<Arc<Sent<C>>>; N]>,
}

impl<C, const N: usize> Journal<C, N> {
    pub(crate) const fn new() -> Self {
        Self {
            sent: Mutex::new([const { None }; N]),
        }
    }

    /// Submits one command whose answer is `response_bytes` long, and
    /// files `change` under the token the device was handed.
    ///
    /// The chain is published and filed in the same poll, so a freeze
    /// finds either both or neither: a task parked before the chain went
    /// out has filed nothing.
    pub(crate) async fn send<T: VirtioTransport>(
        &self,
        inflight: &InFlight<N>,
        queue: &AsyncMutex<VirtQueue<T>>,
        transport: &T,
        inputs: &[&[u8]],
        response_bytes: usize,
        change: C,
    ) -> IoResult<Arc<Sent<C>>> {
        let mut response = vec![0_u8; response_bytes].into_boxed_slice();
        let token = if response.is_empty() {
            submit_chain_with_payload(inflight, queue, transport, inputs, None, &mut []).await?
        } else {
            submit_chain_with_payload(
                inflight,
                queue,
                transport,
                inputs,
                None,
                &mut [&mut response[..]],
            )
            .await?
        };
        let sent = Arc::new(Sent {
            token,
            change,
            response: Mutex::new(response),
            settled: Mutex::new(None),
        });
        self.sent.lock()[usize::from(token)] = Some(sent.clone());
        Ok(sent)
    }

    /// Settles every command the device has answered and nobody has
    /// collected, for a quiesce that reaped their completions. Only
    /// called once the queue holds no chain, so every command filed here
    /// has been answered.
    pub(crate) fn settle_all(&self, accepted: impl Fn(&[u8]) -> bool, apply: impl Fn(&C)) {
        let pending: Vec<Arc<Sent<C>>> = self.sent.lock().iter().flatten().cloned().collect();
        for sent in pending {
            sent.settle(&accepted, &apply);
        }
    }

    /// Forgets `sent`, once its waiter has the answer.
    pub(crate) fn forget(&self, sent: &Arc<Sent<C>>) {
        let mut table = self.sent.lock();
        let slot = &mut table[usize::from(sent.token)];
        if slot.as_ref().is_some_and(|held| Arc::ptr_eq(held, sent)) {
            *slot = None;
        }
    }
}
