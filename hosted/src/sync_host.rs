use std::sync::Arc;

use helios_kernel::{OwnedRawMutexLease, OwnedRawRwLockReadLease, OwnedRawRwLockWriteLease};
use wasmtime::component::{Accessor, HasSelf, Resource};

use crate::init_program::StoreData;
use crate::program_bindings::bindings;

pub struct HostedRawMutex {
    inner: Arc<helios_kernel::RawMutex>,
}

pub struct HostedRawMutexGuard {
    _lease: OwnedRawMutexLease,
}

pub struct HostedRawRwLock {
    inner: Arc<helios_kernel::RawRwLock>,
}

pub struct HostedRawRwLockReadGuard {
    _lease: OwnedRawRwLockReadLease,
}

pub struct HostedRawRwLockWriteGuard {
    _lease: OwnedRawRwLockWriteLease,
}

impl bindings::helios::system::sync::Host for StoreData {}

impl bindings::helios::system::sync::HostRawMutex for StoreData {
    async fn new(&mut self) -> wasmtime::Result<Resource<HostedRawMutex>> {
        Ok(self.table.push(HostedRawMutex {
            inner: Arc::new(helios_kernel::RawMutex::new()),
        })?)
    }

    async fn drop(&mut self, resource: Resource<HostedRawMutex>) -> wasmtime::Result<()> {
        let _ = self.table.delete(resource)?;
        Ok(())
    }
}

impl bindings::helios::system::sync::HostRawMutexWithStore for HasSelf<StoreData> {
    async fn lock<T: 'static>(
        accessor: &Accessor<T, Self>,
        resource: Resource<HostedRawMutex>,
    ) -> wasmtime::Result<Resource<HostedRawMutexGuard>> {
        let mutex = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.inner.clone())
        })?;
        let lease = mutex.lock_owned().await;
        accessor.with(|mut access| {
            Ok(access
                .get()
                .table
                .push(HostedRawMutexGuard { _lease: lease })?)
        })
    }
}

impl bindings::helios::system::sync::HostRawMutexGuard for StoreData {
    async fn drop(&mut self, resource: Resource<HostedRawMutexGuard>) -> wasmtime::Result<()> {
        let _ = self.table.delete(resource)?;
        Ok(())
    }
}

impl bindings::helios::system::sync::HostRawRwLock for StoreData {
    async fn new(&mut self) -> wasmtime::Result<Resource<HostedRawRwLock>> {
        Ok(self.table.push(HostedRawRwLock {
            inner: Arc::new(helios_kernel::RawRwLock::new()),
        })?)
    }

    async fn drop(&mut self, resource: Resource<HostedRawRwLock>) -> wasmtime::Result<()> {
        let _ = self.table.delete(resource)?;
        Ok(())
    }
}

impl bindings::helios::system::sync::HostRawRwLockReadGuard for StoreData {
    async fn drop(&mut self, resource: Resource<HostedRawRwLockReadGuard>) -> wasmtime::Result<()> {
        let _ = self.table.delete(resource)?;
        Ok(())
    }
}

impl bindings::helios::system::sync::HostRawRwLockWriteGuard for StoreData {
    async fn drop(
        &mut self,
        resource: Resource<HostedRawRwLockWriteGuard>,
    ) -> wasmtime::Result<()> {
        let _ = self.table.delete(resource)?;
        Ok(())
    }
}

impl bindings::helios::system::sync::HostRawRwLockWithStore for HasSelf<StoreData> {
    async fn read<T: 'static>(
        accessor: &Accessor<T, Self>,
        resource: Resource<HostedRawRwLock>,
    ) -> wasmtime::Result<Resource<HostedRawRwLockReadGuard>> {
        let rwlock = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.inner.clone())
        })?;
        let lease = rwlock.read_owned().await;
        accessor.with(|mut access| {
            Ok(access
                .get()
                .table
                .push(HostedRawRwLockReadGuard { _lease: lease })?)
        })
    }

    async fn write<T: 'static>(
        accessor: &Accessor<T, Self>,
        resource: Resource<HostedRawRwLock>,
    ) -> wasmtime::Result<Resource<HostedRawRwLockWriteGuard>> {
        let rwlock = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&resource)?.inner.clone())
        })?;
        let lease = rwlock.write_owned().await;
        accessor.with(|mut access| {
            Ok(access
                .get()
                .table
                .push(HostedRawRwLockWriteGuard { _lease: lease })?)
        })
    }
}
