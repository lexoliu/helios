pub use super::bindings::helios::system::profiling::{Filter, FoldedSample, MonoNanos, Scope};

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::error::{RpcError, TransportError};
    use crate::system::methods::{
        PROFILING_CLEAR, PROFILING_FOLDED, PROFILING_INSTANCE, PROFILING_SET_ENABLED,
    };
    use crate::transport::Client;
    use futures_io::{AsyncRead, AsyncWrite};

    pub async fn set_enabled<R, W>(client: &Client<R, W>, enabled: bool) -> Result<(), RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let request = postcard::to_allocvec(&enabled).map_err(|source| RpcError::Encode {
            instance: PROFILING_INSTANCE,
            func: PROFILING_SET_ENABLED,
            source,
        })?;
        client
            .invoke_raw(PROFILING_INSTANCE, PROFILING_SET_ENABLED, request)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: PROFILING_INSTANCE,
                func: PROFILING_SET_ENABLED,
                source,
            })?;
        Ok(())
    }

    pub async fn clear<R, W>(client: &Client<R, W>) -> Result<(), RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        client
            .invoke_raw(PROFILING_INSTANCE, PROFILING_CLEAR, Vec::new())
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: PROFILING_INSTANCE,
                func: PROFILING_CLEAR,
                source,
            })?;
        Ok(())
    }

    pub async fn folded<R, W>(
        client: &Client<R, W>,
        filter: &Filter,
        limit: u32,
    ) -> Result<Vec<FoldedSample>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let request =
            postcard::to_allocvec(&(filter, limit)).map_err(|source| RpcError::Encode {
                instance: PROFILING_INSTANCE,
                func: PROFILING_FOLDED,
                source,
            })?;
        let bytes = client
            .invoke_raw(PROFILING_INSTANCE, PROFILING_FOLDED, request)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: PROFILING_INSTANCE,
                func: PROFILING_FOLDED,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: PROFILING_INSTANCE,
            func: PROFILING_FOLDED,
            source,
        })
    }
}

#[cfg(feature = "host")]
pub use host::*;
