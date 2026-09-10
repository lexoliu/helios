pub use super::bindings::helios::system::instances::{
    Instance, InstanceId, KillError, MonoNanos, Permille,
};

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::error::{RpcError, TransportError};
    use crate::system::methods::{INSTANCES_INSTANCE, INSTANCES_KILL, INSTANCES_SNAPSHOT};
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

    /// Asks the guest to stop the instance `id` names.
    ///
    /// The guest answers as soon as the instance is flagged, not when it
    /// is gone: what proves it went is the instance leaving a later
    /// snapshot, or whatever its supervisor prints on the way back up.
    pub async fn kill<R, W>(
        client: &Client<R, W>,
        id: InstanceId,
    ) -> Result<Result<(), KillError>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let payload = postcard::to_allocvec(&id).map_err(|source| RpcError::Encode {
            instance: INSTANCES_INSTANCE,
            func: INSTANCES_KILL,
            source,
        })?;
        let bytes = client
            .invoke_raw(INSTANCES_INSTANCE, INSTANCES_KILL, payload)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: INSTANCES_INSTANCE,
                func: INSTANCES_KILL,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: INSTANCES_INSTANCE,
            func: INSTANCES_KILL,
            source,
        })
    }
}

#[cfg(feature = "host")]
pub use host::*;
