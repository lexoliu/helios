extern crate alloc;

use alloc::sync::Arc;

use crate::{
    OwnedRawMutexLease, OwnedRawRwLockReadLease, OwnedRawRwLockWriteLease, RawMutex, RawRwLock,
};

pub struct SerialPortResource;

#[derive(Clone)]
pub struct TcpStreamResource<Backend> {
    pub backend: Backend,
}

impl<Backend> TcpStreamResource<Backend> {
    pub const fn new(backend: Backend) -> Self {
        Self { backend }
    }
}

#[derive(Clone)]
pub struct UdpSocketResource<Backend> {
    pub backend: Backend,
}

impl<Backend> UdpSocketResource<Backend> {
    pub const fn new(backend: Backend) -> Self {
        Self { backend }
    }
}

pub struct RawMutexResource {
    pub inner: Arc<RawMutex>,
}

pub struct RawMutexGuardResource {
    pub _lease: OwnedRawMutexLease,
}

pub struct RawRwLockResource {
    pub inner: Arc<RawRwLock>,
}

pub struct RawRwLockReadGuardResource {
    pub _lease: OwnedRawRwLockReadLease,
}

pub struct RawRwLockWriteGuardResource {
    pub _lease: OwnedRawRwLockWriteLease,
}
