#[cfg(feature = "host")]
use crate::transport::Client;
#[cfg(feature = "host")]
use anyhow::{Context as _, Result};
#[cfg(feature = "host")]
use futures_io::{AsyncRead, AsyncWrite};

pub use super::bindings::helios::system::instances::{Instance, InstanceId, MonoNanos, Permille};

#[cfg(feature = "host")]
pub async fn snapshot<R, W>(client: &Client<R, W>) -> Result<Vec<Instance>>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let bytes = client
        .invoke_raw("helios:system/instances@0.1.0", "snapshot", Vec::new())
        .await
        .context("failed to invoke remote instances.snapshot")?;
    postcard::from_bytes(&bytes).context("failed to decode remote instances snapshot")
}
