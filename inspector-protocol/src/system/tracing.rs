pub use super::bindings::helios::system::tracing::{Event, Field, Filter, Level, MonoNanos, Value};

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::error::{RpcError, TransportError};
    use crate::system::methods::{TRACING_INSTANCE, TRACING_RECENT};
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
}

#[cfg(feature = "host")]
pub use host::*;
