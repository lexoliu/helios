#[cfg(feature = "host")]
use crate::transport::Client;
#[cfg(feature = "host")]
use anyhow::{Context as _, Result};
#[cfg(feature = "host")]
use futures_io::{AsyncRead, AsyncWrite};

pub use super::bindings::helios::system::programs::{
    ExecError, ExecErrorKind, ExecOutput, ExecRequest, ExecResult,
};

#[cfg(feature = "host")]
pub async fn exec<R, W>(client: &Client<R, W>, request: &ExecRequest) -> Result<ExecResult>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let bytes = postcard::to_allocvec(request)
        .context("failed to encode remote programs.exec request")?;
    let bytes = client
        .invoke_raw_streaming("helios:system/programs@0.1.0", "exec", bytes)
        .await
        .context("failed to invoke remote programs.exec")?;
    let response = postcard::from_bytes::<Result<ExecResult, ExecError>>(&bytes)
        .context("failed to decode remote programs.exec response")?;
    response.map_err(|error| anyhow::anyhow!("{:?}: {}", error.kind, error.detail))
}
