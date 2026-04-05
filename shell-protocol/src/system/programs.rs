#[cfg(feature = "host")]
use crate::transport::Client;
#[cfg(feature = "host")]
use anyhow::{Context as _, Result};
#[cfg(feature = "host")]
use futures_io::{AsyncRead, AsyncWrite};

pub use super::bindings::helios::system::programs::{
    InstanceId, LaunchError, LaunchErrorKind, LaunchRequest,
};

#[cfg(feature = "host")]
pub async fn launch<R, W>(client: &Client<R, W>, request: &LaunchRequest) -> Result<InstanceId>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let bytes = postcard::to_allocvec(request)
        .context("failed to encode remote programs.launch request")?;
    let bytes = client
        .invoke_raw_streaming("helios:system/programs@0.1.0", "launch", bytes)
        .await
        .context("failed to invoke remote programs.launch")?;
    let response = postcard::from_bytes::<Result<InstanceId, LaunchError>>(&bytes)
        .context("failed to decode remote programs.launch response")?;
    response.map_err(|error| anyhow::anyhow!("{:?}: {}", error.kind, error.detail))
}
