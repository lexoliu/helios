//! Typed hand-off slot for an interface the kernel forwards to a plugin.
//!
//! Some capabilities the kernel exposes through WIT are not implemented by the
//! kernel at all: it validates the request, turns it into a transport-neutral
//! message, and hands it to a user-mode wasm component that owns the protocol.
//! `wasi:http/client` is the first such interface; `wasi:keyvalue`,
//! `wasi:config`, and TLS termination fit the same shape.
//!
//! A [`ProviderSlot`] is the meeting point. The supervisor that owns the
//! plugin installs the sending half of a bounded queue exactly once during
//! startup; every host call afterwards sends a message through it and awaits
//! the reply on a channel the message itself carried along. Because the slot
//! is written once and read many times, a send costs one atomic load rather
//! than a lock, and a missing provider is a typed error rather than a panic —
//! a kernel image built without the plugin must still answer the WIT call.
//!
//! # Concurrency contract
//!
//! [`ProviderSlot::install`] is racy-safe single-writer: the first caller wins
//! and every later caller gets [`ProviderAlreadyInstalled`]. Installing twice
//! is a wiring bug, so callers treat that error as fatal.
//! [`ProviderSlot::send`] is lock-free apart from the queue's own
//! backpressure and may be called concurrently from any processor; producers
//! park on a [`Notify`] rather than spinning, and the single consumer parks the
//! same way when the queue is empty. Both notifications are permit-based, so a
//! wakeup published before the peer starts awaiting is remembered rather than
//! lost.
//!
//! `futures::channel::mpsc` is deliberately not used here: it lives behind
//! `futures-channel`'s `std` feature, which a `#![no_std]` kernel cannot
//! enable. The queue below is the same `concurrent-queue` + `Notify` pairing
//! the kernel's byte channels already use, made generic over the message type.

extern crate alloc;

use core::future::Future;
use core::sync::atomic::{AtomicBool, Ordering};

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use spin::Once;
use thiserror::Error;
use triomphe::Arc;

use crate::Notify;

/// A provider was installed into a slot that already had one.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("a provider is already installed in this slot")]
pub struct ProviderAlreadyInstalled;

/// Why a message could not be handed to the provider.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ProviderError {
    /// No provider was ever installed — the plugin is not provisioned in this
    /// kernel image.
    #[error("no provider installed")]
    Unavailable,
    /// The provider was installed but has since stopped receiving.
    #[error("provider stopped")]
    Closed,
}

/// Shared state of one provider queue.
struct ProviderChannel<M> {
    queue: ConcurrentQueue<M>,
    /// One permit per queued message, for the single consumer.
    ready: Notify,
    /// One permit per freed slot, for however many producers are parked.
    room: Notify,
}

/// Sending half of a provider queue. Cloneable and callable from any
/// processor.
pub struct ProviderSender<M> {
    channel: Arc<ProviderChannel<M>>,
}

impl<M> Clone for ProviderSender<M> {
    fn clone(&self) -> Self {
        Self {
            channel: self.channel.clone(),
        }
    }
}

/// Receiving half of a provider queue, owned by the plugin supervisor.
///
/// Dropping it closes the queue so every parked and future producer observes
/// [`ProviderError::Closed`] instead of waiting for a consumer that is gone.
pub struct ProviderReceiver<M> {
    channel: Arc<ProviderChannel<M>>,
}

/// Create a bounded provider queue holding at most `capacity` messages.
///
/// # Panics
///
/// Panics when `capacity` is zero: a queue that can never hold a message would
/// deadlock every producer.
pub fn provider_channel<M>(capacity: usize) -> (ProviderSender<M>, ProviderReceiver<M>) {
    assert!(capacity > 0, "provider queue capacity must be non-zero");
    let channel = Arc::new(ProviderChannel {
        queue: ConcurrentQueue::bounded(capacity),
        ready: Notify::new(),
        room: Notify::new(),
    });
    (
        ProviderSender {
            channel: channel.clone(),
        },
        ProviderReceiver { channel },
    )
}

impl<M> ProviderSender<M> {
    /// Queue `message`, parking while the queue is full.
    pub async fn send(&self, message: M) -> Result<(), ProviderError> {
        let mut message = message;
        loop {
            match self.channel.queue.push(message) {
                Ok(()) => {
                    self.channel.ready.notify_one();
                    return Ok(());
                }
                Err(PushError::Full(returned)) => {
                    message = returned;
                    self.channel.room.notified().await;
                }
                Err(PushError::Closed(_)) => return Err(ProviderError::Closed),
            }
        }
    }
}

impl<M> ProviderReceiver<M> {
    /// Await the next message, or `None` once the queue is closed and drained.
    pub async fn recv(&self) -> Option<M> {
        loop {
            match self.channel.queue.pop() {
                Ok(message) => {
                    self.channel.room.notify_one();
                    return Some(message);
                }
                Err(PopError::Empty) => self.channel.ready.notified().await,
                Err(PopError::Closed) => return None,
            }
        }
    }
}

impl<M> Drop for ProviderReceiver<M> {
    fn drop(&mut self) {
        self.channel.queue.close();
        self.channel.room.notify_all();
    }
}

/// Write-once slot holding the sending half of a provider's work queue.
pub struct ProviderSlot<M> {
    installed: AtomicBool,
    sender: Once<ProviderSender<M>>,
}

impl<M> ProviderSlot<M> {
    /// An empty slot. Every `send` reports [`ProviderError::Unavailable`]
    /// until a provider is installed.
    pub const fn new() -> Self {
        Self {
            installed: AtomicBool::new(false),
            sender: Once::new(),
        }
    }

    /// Whether a provider has been installed.
    pub fn is_installed(&self) -> bool {
        self.sender.get().is_some()
    }

    /// Claim the slot for `sender`.
    ///
    /// Returns [`ProviderAlreadyInstalled`] when the slot is already taken;
    /// two supervisors for one interface is a wiring bug the caller should
    /// treat as fatal.
    pub fn install(&self, sender: ProviderSender<M>) -> Result<(), ProviderAlreadyInstalled> {
        if self.installed.swap(true, Ordering::AcqRel) {
            return Err(ProviderAlreadyInstalled);
        }
        self.sender.call_once(|| sender);
        Ok(())
    }

    /// Hand `message` to the provider, waiting for room in its queue.
    ///
    /// Resolves as soon as the message is queued; the reply travels on
    /// whatever channel the message itself carries. An uninstalled slot fails
    /// immediately rather than parking forever.
    pub fn send(&self, message: M) -> impl Future<Output = Result<(), ProviderError>> + Send + '_
    where
        M: Send,
    {
        let sender = self.sender.get();
        async move {
            let sender = sender.ok_or(ProviderError::Unavailable)?;
            sender.send(message).await
        }
    }
}

impl<M> Default for ProviderSlot<M> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::future::block_on;

    #[test]
    fn an_empty_slot_reports_unavailable_without_parking() {
        let slot = ProviderSlot::<u32>::new();
        assert!(!slot.is_installed());
        assert_eq!(block_on(slot.send(7)), Err(ProviderError::Unavailable));
    }

    #[test]
    fn an_installed_slot_delivers_messages_in_order() {
        let slot = ProviderSlot::<u32>::new();
        let (sender, receiver) = provider_channel(4);
        slot.install(sender).expect("first install must win");
        assert!(slot.is_installed());

        block_on(async {
            slot.send(1).await.expect("send must reach the provider");
            slot.send(2).await.expect("send must reach the provider");
            assert_eq!(receiver.recv().await, Some(1));
            assert_eq!(receiver.recv().await, Some(2));
        });
    }

    #[test]
    fn installing_twice_is_reported_as_a_wiring_bug() {
        let slot = ProviderSlot::<u32>::new();
        let (first, _first_rx) = provider_channel(1);
        let (second, _second_rx) = provider_channel(1);
        assert_eq!(slot.install(first), Ok(()));
        assert_eq!(slot.install(second), Err(ProviderAlreadyInstalled));
    }

    #[test]
    fn a_dropped_receiver_reports_the_provider_as_closed() {
        let slot = ProviderSlot::<u32>::new();
        let (sender, receiver) = provider_channel(1);
        slot.install(sender).expect("first install must win");
        drop(receiver);
        assert_eq!(block_on(slot.send(1)), Err(ProviderError::Closed));
    }

    #[test]
    fn a_full_queue_parks_the_producer_until_the_consumer_drains_it() {
        let (sender, receiver) = provider_channel::<u32>(1);
        block_on(async {
            sender.send(1).await.expect("first message fits");
            // The queue is full now: this send only completes because the
            // consumer, driven by the same single-threaded executor, frees a
            // slot and publishes a `room` permit.
            let produce = async {
                sender.send(2).await.expect("second message must fit later");
                sender.send(3).await.expect("third message must fit later");
            };
            let consume = async {
                assert_eq!(receiver.recv().await, Some(1));
                assert_eq!(receiver.recv().await, Some(2));
                assert_eq!(receiver.recv().await, Some(3));
            };
            futures::future::join(produce, consume).await;
        });
    }

    #[test]
    fn queued_messages_survive_the_last_sender() {
        let (sender, receiver) = provider_channel::<u32>(2);
        block_on(async {
            sender.send(1).await.expect("message fits");
            drop(sender);
            // Only the receiver closes the queue: a slot keeps its sender for
            // the kernel's lifetime, so the supervisor parks on an idle queue
            // rather than treating "no senders" as shutdown.
            assert_eq!(receiver.recv().await, Some(1));
        });
    }
}
