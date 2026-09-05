use crate::{
    ComponentNetworkService, RawMutexGuardResource, RawMutexResource, RawRwLockReadGuardResource,
    RawRwLockResource, RawRwLockWriteGuardResource, SerialPortResource, TcpStreamResource,
    UdpSocketResource,
};

pub struct ComponentSerialPort {
    pub _resource: SerialPortResource,
}

pub struct ComponentTcpStream<Backend> {
    pub resource: TcpStreamResource<Backend>,
}

impl<Backend> ComponentTcpStream<Backend> {
    pub fn new(backend: Backend) -> Self {
        Self {
            resource: TcpStreamResource::new(backend),
        }
    }
}

pub struct ComponentUdpSocket<Backend> {
    pub resource: UdpSocketResource<Backend>,
}

impl<Backend> ComponentUdpSocket<Backend> {
    pub fn new(backend: Backend) -> Self {
        Self {
            resource: UdpSocketResource::new(backend),
        }
    }
}

/// The kernel stream a `helios:system/net` `tcp-stream` resource owns.
///
/// The stream is retired when this backend is dropped, whichever way
/// the resource's life ends: the guest dropping the handle, the store
/// being torn down with the handle still open, or a launch unwinding.
/// Deferring the close to a task the guest's own destructor spawns
/// loses it whenever the instance goes away first, which is how a
/// connection outlived the program that opened it in #184.
pub struct ComponentTcpBackend<Service>
where
    Service: ComponentNetworkService,
{
    pub service: Service,
    stream: Option<Service::TcpStream>,
}

impl<Service> ComponentTcpBackend<Service>
where
    Service: ComponentNetworkService,
{
    pub fn new(service: Service, stream: Service::TcpStream) -> Self {
        Self {
            service,
            stream: Some(stream),
        }
    }

    /// The stream this resource still owns, or `None` once `close` has
    /// retired it.
    pub fn stream(&self) -> Option<Service::TcpStream> {
        self.stream
    }

    /// Retires the stream now. A second call, and the drop that follows
    /// it, do nothing: a handle is retired exactly once, because its
    /// slab slot is reused by the next connection.
    pub fn close(&mut self) {
        if let Some(stream) = self.stream.take() {
            self.service.tcp_close(stream);
        }
    }
}

impl<Service> Drop for ComponentTcpBackend<Service>
where
    Service: ComponentNetworkService,
{
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Clone)]
pub struct ComponentUdpBackend<Service>
where
    Service: ComponentNetworkService,
{
    pub service: Service,
    pub socket: Service::UdpSocket,
}

pub struct ComponentRawMutex {
    pub resource: RawMutexResource,
}

pub struct ComponentRawMutexGuard {
    pub _resource: RawMutexGuardResource,
}

pub struct ComponentRawRwLock {
    pub resource: RawRwLockResource,
}

pub struct ComponentRawRwLockReadGuard {
    pub _resource: RawRwLockReadGuardResource,
}

pub struct ComponentRawRwLockWriteGuard {
    pub _resource: RawRwLockWriteGuardResource,
}

#[cfg(all(test, feature = "wasmtime-runtime"))]
mod tests {
    use super::ComponentTcpBackend;
    use crate::test_support::TestNetworkService;

    /// A `helios:system/net` stream resource retires its kernel stream
    /// when it is dropped.
    ///
    /// The resource destructor the component model runs fires only when
    /// the *guest* drops the handle; a store torn down with the handle
    /// still open runs nothing. Closing from the destructor alone
    /// therefore left the connection behind whenever the instance went
    /// first, which is what #184 saw.
    #[test]
    fn a_system_net_stream_retires_itself_when_its_resource_is_dropped() {
        let service = TestNetworkService::new();
        let closed = service.closed();
        let backend = ComponentTcpBackend::new(service, 5);

        assert_eq!(backend.stream(), Some(5));
        assert_eq!(closed.count(), 0);
        drop(backend);
        assert_eq!(closed.count(), 1, "the backend owns the stream it holds");
        assert_eq!(closed.last(), 5);
    }

    /// `tcp-stream.close` retires the stream once, and the drop that
    /// follows it does nothing: the slab slot a retired handle frees is
    /// handed to the next connection, so a second close would retire
    /// somebody else's stream.
    #[test]
    fn a_closed_system_net_stream_is_not_retired_a_second_time() {
        let service = TestNetworkService::new();
        let closed = service.closed();
        let mut backend = ComponentTcpBackend::new(service, 5);

        backend.close();
        assert_eq!(closed.count(), 1);
        assert_eq!(
            backend.stream(),
            None,
            "a closed stream is gone from the resource"
        );
        backend.close();
        drop(backend);
        assert_eq!(closed.count(), 1, "a stream is retired exactly once");
    }
}
