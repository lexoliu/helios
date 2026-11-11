pub use super::bindings::helios::system::tracing::{
    Event, Field, Filter, Level, MonoNanos, TargetError, Value,
};

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::error::{RpcError, TransportError};
    use crate::system::methods::{TRACING_INSTANCE, TRACING_RECENT, TRACING_SET_TARGET_ENABLED};
    use crate::transport::Client;
    use futures_io::{AsyncRead, AsyncWrite};

    pub async fn recent<R, W>(
        client: &Client<R, W>,
        filter: &Filter,
        limit: u32,
    ) -> Result<Vec<Event>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let request =
            postcard::to_allocvec(&(filter, limit)).map_err(|source| RpcError::Encode {
                instance: TRACING_INSTANCE,
                func: TRACING_RECENT,
                source,
            })?;
        let bytes = client
            .invoke_raw(TRACING_INSTANCE, TRACING_RECENT, request)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: TRACING_INSTANCE,
                func: TRACING_RECENT,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: TRACING_INSTANCE,
            func: TRACING_RECENT,
            source,
        })
    }

    /// Turns a kernel diagnostic target on or off for the rest of the
    /// boot. `Ok(Err(..))` is the guest refusing a name nothing
    /// registered.
    pub async fn set_target_enabled<R, W>(
        client: &Client<R, W>,
        target: &str,
        enabled: bool,
    ) -> Result<Result<(), TargetError>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let request =
            postcard::to_allocvec(&(target, enabled)).map_err(|source| RpcError::Encode {
                instance: TRACING_INSTANCE,
                func: TRACING_SET_TARGET_ENABLED,
                source,
            })?;
        let bytes = client
            .invoke_raw(TRACING_INSTANCE, TRACING_SET_TARGET_ENABLED, request)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: TRACING_INSTANCE,
                func: TRACING_SET_TARGET_ENABLED,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: TRACING_INSTANCE,
            func: TRACING_SET_TARGET_ENABLED,
            source,
        })
    }
}

#[cfg(feature = "host")]
pub use host::*;
