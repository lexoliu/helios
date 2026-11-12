pub use super::bindings::helios::system::stats::SwapPolicy;
pub use super::bindings::helios::system::swap::PolicyError;

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::error::{RpcError, TransportError};
    use crate::system::methods::{SWAP_INSTANCE, SWAP_SET_POLICY};
    use crate::transport::Client;
    use futures_io::{AsyncRead, AsyncWrite};

    /// Asks the guest to replace every setting of its swap policy.
    ///
    /// The outer error is the call failing; the inner one is the kernel
    /// refusing the policy, which leaves the settings as they were.
    pub async fn set_policy<R, W>(
        client: &Client<R, W>,
        policy: &SwapPolicy,
    ) -> Result<Result<(), PolicyError>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let payload = postcard::to_allocvec(policy).map_err(|source| RpcError::Encode {
            instance: SWAP_INSTANCE,
            func: SWAP_SET_POLICY,
            source,
        })?;
        let bytes = client
            .invoke_raw(SWAP_INSTANCE, SWAP_SET_POLICY, payload)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: SWAP_INSTANCE,
                func: SWAP_SET_POLICY,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: SWAP_INSTANCE,
            func: SWAP_SET_POLICY,
            source,
        })
    }
}

#[cfg(feature = "host")]
pub use host::*;
