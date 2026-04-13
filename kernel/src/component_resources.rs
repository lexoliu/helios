use crate::{
    ComponentNetworkService, DynamicNetworkService, RawMutexGuardResource, RawMutexResource,
    RawRwLockReadGuardResource, RawRwLockResource, RawRwLockWriteGuardResource, SerialPortResource,
    TcpStreamResource,
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

#[derive(Clone)]
pub struct ComponentTcpBackend<Service = DynamicNetworkService>
where
    Service: ComponentNetworkService,
{
    pub service: Service,
    pub stream: Service::TcpStream,
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
