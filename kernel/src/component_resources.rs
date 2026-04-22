use crate::{
    ComponentNetworkService, DynamicNetworkService, RawMutexGuardResource, RawMutexResource,
    RawRwLockReadGuardResource, RawRwLockResource, RawRwLockWriteGuardResource, SerialPortResource,
    TcpStreamResource, UdpSocketResource,
};

pub struct ComponentSerialPort {
    pub _resource: SerialPortResource,
}

pub struct ComponentTcpStream<Backend = ComponentTcpBackend> {
    pub resource: TcpStreamResource<Backend>,
}

impl<Backend> ComponentTcpStream<Backend> {
    pub fn new(backend: Backend) -> Self {
        Self {
            resource: TcpStreamResource::new(backend),
        }
    }
}

pub struct ComponentUdpSocket<Backend = ComponentUdpBackend> {
    pub resource: UdpSocketResource<Backend>,
}

impl<Backend> ComponentUdpSocket<Backend> {
    pub fn new(backend: Backend) -> Self {
        Self {
            resource: UdpSocketResource::new(backend),
        }
    }
}

#[derive(Clone)]
pub struct ComponentTcpBackend<Service = DynamicNetworkService>
where
    Service: ComponentNetworkService,
{
    pub service: Service,
    pub stream: Service::TcpStream,
}

#[derive(Clone)]
pub struct ComponentUdpBackend<Service = DynamicNetworkService>
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
