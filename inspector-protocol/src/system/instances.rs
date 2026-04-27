pub use super::bindings::helios::system::instances::{Instance, InstanceId, MonoNanos, Permille};

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::error::{RpcError, TransportError};
    use crate::system::methods::{INSTANCES_INSTANCE, INSTANCES_SNAPSHOT};
    use crate::transport::Client;
    use futures_io::{AsyncRead, AsyncWrite};

    pub async fn snapshot<R, W>(client: &Client<R, W>) -> Result<Vec<Instance>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = client
            .invoke_raw(INSTANCES_INSTANCE, INSTANCES_SNAPSHOT, Vec::new())
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: INSTANCES_INSTANCE,
                func: INSTANCES_SNAPSHOT,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: INSTANCES_INSTANCE,
            func: INSTANCES_SNAPSHOT,
            source,
        })
    }
}

#[cfg(feature = "host")]
pub use host::*;
