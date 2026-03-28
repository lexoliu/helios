use std::sync::Arc;

use tokio::sync::{
    Mutex as AsyncMutex, OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard,
    RwLock as AsyncRwLock,
};
use wasmtime::component::{Accessor, Resource};

use crate::init_bindings::bindings;
use crate::init_program::StoreData;

pub struct HostedRawMutex {
    inner: Arc<AsyncMutex<()>>,
}

pub struct HostedRawMutexGuard {
    _guard: OwnedMutexGuard<()>,
}

pub struct HostedRawRwLock {
    inner: Arc<AsyncRwLock<()>>,
}

pub struct HostedRawRwLockReadGuard {
    _guard: OwnedRwLockReadGuard<()>,
}

pub struct HostedRawRwLockWriteGuard {
    _guard: OwnedRwLockWriteGuard<()>,
}

impl bindings::helios::system::sync::Host for StoreData {}

impl bindings::helios::system::sync::HostRawMutex for StoreData {
    fn new(&mut self) -> wasmtime::Result<Resource<HostedRawMutex>> {
        Ok(self.table.push(HostedRawMutex {
            inner: Arc::new(AsyncMutex::new(())),
        })?)
    }

    fn drop(&mut self, resource: Resource<HostedRawMutex>) -> wasmtime::Result<()> {
        let _ = self.table.delete(resource)?;
        Ok(())
    }
}

impl bindings::helios::system::sync::HostRawMutexWithStore for StoreData {
    async fn lock<T: 'static>(
        accessor: &Accessor<T, Self>,
        resource: Resource<HostedRawMutex>,
    ) -> wasmtime::Result<Resource<HostedRawMutexGuard>> {
        let mutex = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.inner.clone())
        })?;
        let guard = mutex.lock_owned().await;
        accessor.with(|mut access| {
            Ok(access
                .get()
                .table
                .push(HostedRawMutexGuard { _guard: guard })?)
        })
    }
}

impl bindings::helios::system::sync::HostRawMutexGuard for StoreData {
    fn drop(&mut self, resource: Resource<HostedRawMutexGuard>) -> wasmtime::Result<()> {
        let _ = self.table.delete(resource)?;
        Ok(())
    }
}

impl bindings::helios::system::sync::HostRawRwLock for StoreData {
    fn new(&mut self) -> wasmtime::Result<Resource<HostedRawRwLock>> {
        Ok(self.table.push(HostedRawRwLock {
            inner: Arc::new(AsyncRwLock::new(())),
        })?)
    }

    fn drop(&mut self, resource: Resource<HostedRawRwLock>) -> wasmtime::Result<()> {
        let _ = self.table.delete(resource)?;
        Ok(())
    }
}

impl bindings::helios::system::sync::HostRawRwLockReadGuard for StoreData {
    fn drop(&mut self, resource: Resource<HostedRawRwLockReadGuard>) -> wasmtime::Result<()> {
        let _ = self.table.delete(resource)?;
        Ok(())
    }
}

impl bindings::helios::system::sync::HostRawRwLockWriteGuard for StoreData {
    fn drop(&mut self, resource: Resource<HostedRawRwLockWriteGuard>) -> wasmtime::Result<()> {
        let _ = self.table.delete(resource)?;
        Ok(())
    }
}

impl bindings::helios::system::sync::HostRawRwLockWithStore for StoreData {
    async fn read<T: 'static>(
        accessor: &Accessor<T, Self>,
        resource: Resource<HostedRawRwLock>,
    ) -> wasmtime::Result<Resource<HostedRawRwLockReadGuard>> {
        let rwlock = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.inner.clone())
        })?;
        let guard = rwlock.read_owned().await;
        accessor.with(|mut access| {
            Ok(access
                .get()
                .table
                .push(HostedRawRwLockReadGuard { _guard: guard })?)
        })
    }

    async fn write<T: 'static>(
        accessor: &Accessor<T, Self>,
        resource: Resource<HostedRawRwLock>,
    ) -> wasmtime::Result<Resource<HostedRawRwLockWriteGuard>> {
        let rwlock = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.inner.clone())
        })?;
        let guard = rwlock.write_owned().await;
        accessor.with(|mut access| {
            Ok(access
                .get()
                .table
                .push(HostedRawRwLockWriteGuard { _guard: guard })?)
        })
    }
}
