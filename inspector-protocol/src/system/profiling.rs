pub use super::bindings::helios::system::profiling::{
    Filter, FoldedSample, MetricFilter, MetricSample, MonoNanos, ProfileSection, RawProfileError,
    Scope,
};

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::error::{RpcError, TransportError};
    use crate::system::methods::{
        PROFILING_CLEAR, PROFILING_FOLDED, PROFILING_INSTANCE, PROFILING_METRICS,
        PROFILING_RAW_PROFILE_READ, PROFILING_RAW_PROFILE_SIZE, PROFILING_SET_ENABLED,
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

    pub async fn metrics<R, W>(
        client: &Client<R, W>,
        filter: &MetricFilter,
        limit: u32,
    ) -> Result<Vec<MetricSample>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let request =
            postcard::to_allocvec(&(filter, limit)).map_err(|source| RpcError::Encode {
                instance: PROFILING_INSTANCE,
                func: PROFILING_METRICS,
                source,
            })?;
        let bytes = client
            .invoke_raw(PROFILING_INSTANCE, PROFILING_METRICS, request)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: PROFILING_INSTANCE,
                func: PROFILING_METRICS,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: PROFILING_INSTANCE,
            func: PROFILING_METRICS,
            source,
        })
    }

    /// Asks the guest for the length of its kernel's LLVM raw profile.
    pub async fn raw_profile_size<R, W>(
        client: &Client<R, W>,
    ) -> Result<Result<u64, RawProfileError>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = client
            .invoke_raw(PROFILING_INSTANCE, PROFILING_RAW_PROFILE_SIZE, Vec::new())
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: PROFILING_INSTANCE,
                func: PROFILING_RAW_PROFILE_SIZE,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: PROFILING_INSTANCE,
            func: PROFILING_RAW_PROFILE_SIZE,
            source,
        })
    }

    /// Reads the window of the guest kernel's LLVM raw profile that starts
    /// at `offset`.
    pub async fn raw_profile_read<R, W>(
        client: &Client<R, W>,
        offset: u64,
        length: u32,
    ) -> Result<Result<Vec<u8>, RawProfileError>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let request =
            postcard::to_allocvec(&(offset, length)).map_err(|source| RpcError::Encode {
                instance: PROFILING_INSTANCE,
                func: PROFILING_RAW_PROFILE_READ,
                source,
            })?;
        let bytes = client
            .invoke_raw(PROFILING_INSTANCE, PROFILING_RAW_PROFILE_READ, request)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: PROFILING_INSTANCE,
                func: PROFILING_RAW_PROFILE_READ,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: PROFILING_INSTANCE,
            func: PROFILING_RAW_PROFILE_READ,
            source,
        })
    }
}

#[cfg(feature = "host")]
pub use host::*;
