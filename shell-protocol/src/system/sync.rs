use anyhow::Result;
use wrpc_transport::{Invoke, ResourceOwn};

use super::imports::helios::system::sync as raw;
use super::resolve_future;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawMutex {
    raw: ResourceOwn<raw::RawMutex>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawMutexGuard {
    raw: ResourceOwn<raw::RawMutexGuard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawRwLock {
    raw: ResourceOwn<raw::RawRwLock>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawRwLockReadGuard {
    raw: ResourceOwn<raw::RawRwLockReadGuard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawRwLockWriteGuard {
    raw: ResourceOwn<raw::RawRwLockWriteGuard>,
}

pub async fn raw_mutex<C>(wrpc: &C) -> Result<RawMutex>
where
    C: Invoke<Context = ()>,
{
    Ok(RawMutex {
        raw: raw::RawMutex::new(wrpc, ()).await?,
    })
}

pub async fn raw_rw_lock<C>(wrpc: &C) -> Result<RawRwLock>
where
    C: Invoke<Context = ()>,
{
    Ok(RawRwLock {
        raw: raw::RawRwLock::new(wrpc, ()).await?,
    })
}

impl RawMutex {
    pub async fn lock<C>(&self, wrpc: &C) -> Result<RawMutexGuard>
    where
        C: Invoke<Context = ()>,
    {
        let borrow = self.raw.as_borrow();
        let (future, driver) = raw::RawMutex::lock(wrpc, (), &borrow).await?;
        Ok(RawMutexGuard {
            raw: resolve_future(future, driver).await?,
        })
    }

    pub async fn drop_remote<C>(self, _wrpc: &C) -> Result<()>
    where
        C: Invoke<Context = ()>,
    {
        drop(self);
        Ok(())
    }
}

impl RawMutexGuard {
    pub async fn drop_remote<C>(self, _wrpc: &C) -> Result<()>
    where
        C: Invoke<Context = ()>,
    {
        drop(self);
        Ok(())
    }
}

impl RawRwLock {
    pub async fn read<C>(&self, wrpc: &C) -> Result<RawRwLockReadGuard>
    where
        C: Invoke<Context = ()>,
    {
        let borrow = self.raw.as_borrow();
        let (future, driver) = raw::RawRwLock::read(wrpc, (), &borrow).await?;
        Ok(RawRwLockReadGuard {
            raw: resolve_future(future, driver).await?,
        })
    }

    pub async fn write<C>(&self, wrpc: &C) -> Result<RawRwLockWriteGuard>
    where
        C: Invoke<Context = ()>,
    {
        let borrow = self.raw.as_borrow();
        let (future, driver) = raw::RawRwLock::write(wrpc, (), &borrow).await?;
        Ok(RawRwLockWriteGuard {
            raw: resolve_future(future, driver).await?,
        })
    }

    pub async fn drop_remote<C>(self, _wrpc: &C) -> Result<()>
    where
        C: Invoke<Context = ()>,
    {
        drop(self);
        Ok(())
    }
}

impl RawRwLockReadGuard {
    pub async fn drop_remote<C>(self, _wrpc: &C) -> Result<()>
    where
        C: Invoke<Context = ()>,
    {
        drop(self);
        Ok(())
    }
}

impl RawRwLockWriteGuard {
    pub async fn drop_remote<C>(self, _wrpc: &C) -> Result<()>
    where
        C: Invoke<Context = ()>,
    {
        drop(self);
        Ok(())
    }
}
