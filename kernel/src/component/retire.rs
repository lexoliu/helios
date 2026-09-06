//! Retirement of network handles whose owner died with no service in
//! hand.
//!
//! Every other owner of a kernel network handle carries the service
//! that minted it and retires the handle in its own `Drop`
//! ([`crate::ComponentTcpBackend`] is the model). One owner cannot: a
//! component's socket resource types are handed to the bindings
//! generator by path, and a path carries no type parameter, so those
//! types stay concrete however generic the host around them is. They
//! name what they own by id, push the ids here when they die, and the
//! store — which is generic over the service — closes them.

use concurrent_queue::{ConcurrentQueue, PopError, PushError};
use triomphe::Arc;

use crate::component::ComponentRuntimeState;

/// One network handle whose owner has died, named the only way an owner
/// without the service can name it.
///
/// The tag is what tells `tcp_close` from `tcp_listener_close` from
/// `udp_close` on the other side: three id spaces share the numeric
/// form and nothing else distinguishes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetiredNetworkHandle {
    TcpStream(u64),
    TcpListener(u64),
    UdpSocket(u64),
}

/// The queue a dying socket pushes its handles to.
///
/// # Concurrency
///
/// Multi-producer, single-consumer and lock-free in both directions.
/// Any task, any `Drop` and any processor may push through a
/// [`SocketRetirementSender`], which is why a sender is all a socket
/// resource holds and why pushing needs no processor index. Draining
/// belongs to the store that owns the queue and happens on that store's
/// own turn: at the entry to every host call and when the store is
/// torn down.
///
/// The queue is unbounded because how many sockets a guest opens is the
/// guest's decision, and a bounded queue with nowhere to put an id
/// would strand a connection in its shard — the #184 failure the
/// ownership rule exists to prevent. Each push is one node from the
/// kernel heap, on a path that runs once per socket death.
pub struct SocketRetirementQueue {
    handles: Arc<ConcurrentQueue<RetiredNetworkHandle>>,
}

impl SocketRetirementQueue {
    pub fn new() -> Self {
        Self {
            handles: Arc::new(ConcurrentQueue::unbounded()),
        }
    }

    /// A push handle for one socket resource.
    pub fn sender(&self) -> SocketRetirementSender {
        SocketRetirementSender {
            handles: self.handles.clone(),
        }
    }

    /// Whether anything is waiting to be retired.
    ///
    /// The prologue of every host call asks this first, so a call on a
    /// store where nothing has died pays one atomic load.
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Hands every queued handle to `retire`, in push order, and
    /// answers how many there were.
    ///
    /// Pushes that arrive while this runs are left for the next drain
    /// rather than extending this one, so a socket dying inside
    /// `retire` cannot hold the drain open.
    pub fn drain(&self, mut retire: impl FnMut(RetiredNetworkHandle)) -> usize {
        let mut retired = 0;
        let mut remaining = self.handles.len();
        while remaining != 0 {
            match self.handles.pop() {
                Ok(handle) => {
                    retire(handle);
                    retired += 1;
                    remaining -= 1;
                }
                Err(PopError::Empty) => break,
                Err(PopError::Closed) => {
                    unreachable!("the socket retirement queue is never closed")
                }
            }
        }
        retired
    }
}

impl Default for SocketRetirementQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// One socket resource's end of the retirement queue.
///
/// Cloning is cheap and unsynchronised, and pushing is safe from any
/// task, any processor and any `Drop`.
#[derive(Clone)]
pub struct SocketRetirementSender {
    handles: Arc<ConcurrentQueue<RetiredNetworkHandle>>,
}

impl SocketRetirementSender {
    pub fn push(&self, handle: RetiredNetworkHandle) {
        match self.handles.push(handle) {
            Ok(()) => {}
            Err(PushError::Full(_)) => {
                unreachable!("the socket retirement queue is unbounded")
            }
            Err(PushError::Closed(_)) => {
                unreachable!("the socket retirement queue is never closed")
            }
        }
    }
}

/// Closes everything `retired` holds through `service`, and wakes the
/// packet pump if anything closed.
///
/// The one place a queued id becomes a close. The pump kick is what
/// carries the FIN or RST those closes queued onto the wire on the next
/// executor turn instead of the next protocol deadline, a second away
/// on a quiet guest (#232); one kick covers the whole batch, because
/// the segments are all in the same outbound queue by the time it is
/// raised.
///
/// Answers how many handles it closed.
pub fn retire_queued_handles<Service>(retired: &SocketRetirementQueue, service: &Service) -> usize
where
    Service: crate::ComponentNetworkService,
{
    let closed = retired.drain(|handle| match handle {
        RetiredNetworkHandle::TcpStream(stream) => {
            service.tcp_close(crate::NetworkHandle::from_raw(stream));
        }
        RetiredNetworkHandle::TcpListener(listener) => {
            service.tcp_listener_close(crate::NetworkHandle::from_raw(listener));
        }
        RetiredNetworkHandle::UdpSocket(socket) => {
            service.udp_close(crate::NetworkHandle::from_raw(socket));
        }
    });
    if closed != 0 {
        service.wake_packet_pump();
    }
    closed
}

/// The store's end of the retirement queue.
///
/// This is what holds the queue and the runtime state that can answer
/// for it together, and it is a field of its own rather than a `Drop`
/// on the store so that ordering is a property of the declaration: a
/// store's resource table is declared before this, so it — and every
/// socket still in it — is torn down first, and the ids those sockets
/// push while dying are already queued when this drains them. A `Drop`
/// written on the store itself would run before its fields and find the
/// queue empty.
pub struct StoreSocketRetirement<RuntimeStateImpl>
where
    RuntimeStateImpl: ComponentRuntimeState,
{
    queue: SocketRetirementQueue,
    runtime_state: RuntimeStateImpl,
}

impl<RuntimeStateImpl> StoreSocketRetirement<RuntimeStateImpl>
where
    RuntimeStateImpl: ComponentRuntimeState,
{
    pub fn new(runtime_state: RuntimeStateImpl) -> Self {
        Self {
            queue: SocketRetirementQueue::new(),
            runtime_state,
        }
    }

    /// A push handle for a socket resource this store is about to
    /// create.
    pub fn sender(&self) -> SocketRetirementSender {
        self.queue.sender()
    }

    /// Retires everything queued since the last drain.
    ///
    /// Called from the prologue of every host call, so the delay
    /// between a socket's death and its connection leaving the netstack
    /// is one host call rather than unbounded.
    pub fn drain(&self) {
        self.runtime_state.retire_network_handles(&self.queue);
    }
}

impl<RuntimeStateImpl> Drop for StoreSocketRetirement<RuntimeStateImpl>
where
    RuntimeStateImpl: ComponentRuntimeState,
{
    fn drop(&mut self) {
        self.drain();
    }
}

#[cfg(all(test, feature = "wasmtime-runtime"))]
mod tests {
    use super::*;
    use crate::test_support::{TestNetworkService, TestRuntimeState};

    /// A store torn down with handles still queued closes every one of
    /// them.
    ///
    /// This is the backstop the whole design rests on. A socket
    /// resource queues what it owns and the store closes it on its next
    /// turn — but a guest that exits holding a socket has no next turn,
    /// which is exactly the case #184 lost. The store's own teardown
    /// drains, so the last host call is never the last chance.
    #[test]
    fn a_store_teardown_retires_everything_still_queued() {
        let service = TestNetworkService::new();
        let streams = service.closed();
        let listeners = service.closed_listeners();
        let sockets = service.closed_udp_sockets();
        let retirement =
            StoreSocketRetirement::new(TestRuntimeState::with_network(service.clone()));

        let sender = retirement.sender();
        sender.push(RetiredNetworkHandle::TcpStream(7));
        sender.push(RetiredNetworkHandle::TcpListener(41));
        sender.push(RetiredNetworkHandle::UdpSocket(9));
        assert_eq!(streams.count(), 0, "nothing closes before the teardown");

        drop(retirement);
        assert_eq!(streams.count(), 1);
        assert_eq!(streams.last(), 7);
        assert_eq!(listeners.count(), 1);
        assert_eq!(listeners.last(), 41);
        assert_eq!(sockets.count(), 1);
        assert_eq!(sockets.last(), 9);
        assert_eq!(
            service.packet_pump_wakes(),
            1,
            "one kick covers the whole batch of segments the closes queued"
        );
    }

    /// A store torn down with an empty queue closes nothing and leaves
    /// the packet pump alone, which is every store that never opened a
    /// socket.
    #[test]
    fn a_store_teardown_with_nothing_queued_costs_nothing() {
        let service = TestNetworkService::new();
        let streams = service.closed();
        let retirement =
            StoreSocketRetirement::new(TestRuntimeState::with_network(service.clone()));

        drop(retirement);
        assert_eq!(streams.count(), 0);
        assert_eq!(service.packet_pump_wakes(), 0);
    }

    /// Handles come back in push order, so a drain closes a socket's
    /// stream before the listener that accepted it.
    #[test]
    fn a_drain_retires_in_the_order_the_sockets_died() {
        let service = TestNetworkService::new();
        let streams = service.closed();
        let queue = SocketRetirementQueue::new();
        let sender = queue.sender();
        sender.push(RetiredNetworkHandle::TcpStream(7));
        sender.push(RetiredNetworkHandle::TcpStream(8));

        assert_eq!(retire_queued_handles(&queue, &service), 2);
        assert_eq!(streams.count(), 2);
        assert_eq!(streams.last(), 8, "the second push is closed second");
        assert!(queue.is_empty(), "a drain empties the queue");
    }
}
