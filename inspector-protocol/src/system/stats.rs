#[cfg(feature = "host")]
use crate::transport::Client;
#[cfg(feature = "host")]
use anyhow::{Context as _, Result};
#[cfg(feature = "host")]
use futures_io::{AsyncRead, AsyncWrite};


pub use super::bindings::helios::system::stats::{
    Memory, MemoryPressure, MonoNanos, Permille, Processor, Processors, Sample,
};

#[cfg(feature = "host")]
pub async fn snapshot<R, W>(client: &Client<R, W>) -> Result<Sample>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let bytes = client
        .invoke_raw(STATS_INSTANCE, STATS_SNAPSHOT, Vec::new())
        .await
        .context("failed to invoke remote stats.snapshot")?;
    postcard::from_bytes(&bytes).context("failed to decode remote stats snapshot")
}
