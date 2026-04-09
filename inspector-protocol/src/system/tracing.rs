#[cfg(feature = "host")]
use crate::transport::Client;
#[cfg(feature = "host")]
use anyhow::{Context as _, Result};
#[cfg(feature = "host")]
use futures_io::{AsyncRead, AsyncWrite};

#[cfg(feature = "host")]
use super::methods::{TRACING_INSTANCE, TRACING_RECENT};

pub use super::bindings::helios::system::tracing::{Event, Field, Filter, Level, MonoNanos, Value};

#[cfg(feature = "host")]
pub async fn recent<R, W>(client: &Client<R, W>, filter: &Filter, limit: u32) -> Result<Vec<Event>>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let request = postcard::to_allocvec(&(filter, limit))
        .context("failed to encode remote tracing.recent request")?;
    let bytes = client
        .invoke_raw(TRACING_INSTANCE, TRACING_RECENT, request)
        .await
        .context("failed to invoke remote tracing.recent")?;
    postcard::from_bytes(&bytes).context("failed to decode remote tracing events")
}
