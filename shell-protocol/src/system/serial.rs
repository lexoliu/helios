use anyhow::Result;
use bytes::Bytes;
use wrpc_transport::{Invoke, ResourceOwn};

use super::imports::helios::system::serial as raw;
use super::resolve_future;

pub use raw::SerialRights;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialPort {
    raw: ResourceOwn<raw::SerialPort>,
}

pub async fn debug_port<C>(wrpc: &C) -> Result<Option<SerialPort>>
where
    C: Invoke<Context = ()>,
{
    Ok(raw::debug_port(wrpc, ())
        .await?
        .map(|raw| SerialPort { raw }))
}

impl SerialPort {
    pub async fn rights<C>(&self, wrpc: &C) -> Result<SerialRights>
    where
        C: Invoke<Context = ()>,
    {
        let borrow = self.raw.as_borrow();
        raw::SerialPort::rights(wrpc, (), &borrow).await
    }

    pub async fn read<C>(&self, wrpc: &C, max_bytes: u32) -> Result<Vec<u8>>
    where
        C: Invoke<Context = ()>,
    {
        let borrow = self.raw.as_borrow();
        let (future, driver) = raw::SerialPort::read(wrpc, (), &borrow, max_bytes).await?;
        Ok(resolve_future(future, driver).await?.to_vec())
    }

    pub async fn write<C>(&self, wrpc: &C, bytes: &[u8]) -> Result<()>
    where
        C: Invoke<Context = ()>,
    {
        let borrow = self.raw.as_borrow();
        let bytes = Bytes::copy_from_slice(bytes);
        let (future, driver) = raw::SerialPort::write(wrpc, (), &borrow, &bytes).await?;
        resolve_future(future, driver).await
    }

    pub async fn flush<C>(&self, wrpc: &C) -> Result<()>
    where
        C: Invoke<Context = ()>,
    {
        let borrow = self.raw.as_borrow();
        let (future, driver) = raw::SerialPort::flush(wrpc, (), &borrow).await?;
        resolve_future(future, driver).await
    }

    pub async fn drop_remote<C>(self, _wrpc: &C) -> Result<()>
    where
        C: Invoke<Context = ()>,
    {
        drop(self);
        Ok(())
    }
}
