#[cfg(feature = "host")]
use crate::transport::Client;
#[cfg(feature = "host")]
use anyhow::{Context as _, Result};
#[cfg(feature = "host")]
use futures_io::{AsyncRead, AsyncWrite};

#[cfg(feature = "host")]
use super::methods::{
    PROFILING_CLEAR, PROFILING_FOLDED, PROFILING_INSTANCE, PROFILING_SET_ENABLED,
};

pub use super::bindings::helios::system::profiling::{Filter, FoldedSample, MonoNanos, Scope};

#[cfg(feature = "host")]
pub async fn set_enabled<R, W>(client: &Client<R, W>, enabled: bool) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let request = postcard::to_allocvec(&enabled)
        .context("failed to encode remote profiling.set-enabled request")?;
    client
        .invoke_raw(PROFILING_INSTANCE, PROFILING_SET_ENABLED, request)
        .await
        .context("failed to invoke remote profiling.set-enabled")?;
    Ok(())
}

#[cfg(feature = "host")]
pub async fn clear<R, W>(client: &Client<R, W>) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    client
        .invoke_raw(PROFILING_INSTANCE, PROFILING_CLEAR, Vec::new())
        .await
        .context("failed to invoke remote profiling.clear")?;
    Ok(())
}

#[cfg(feature = "host")]
pub async fn folded<R, W>(
    client: &Client<R, W>,
    filter: &Filter,
    limit: u32,
) -> Result<Vec<FoldedSample>>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let request = postcard::to_allocvec(&(filter, limit))
        .context("failed to encode remote profiling.folded request")?;
    let bytes = client
        .invoke_raw(PROFILING_INSTANCE, PROFILING_FOLDED, request)
        .await
        .context("failed to invoke remote profiling.folded")?;
    postcard::from_bytes(&bytes).context("failed to decode remote profiling folded samples")
}
