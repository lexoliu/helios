pub use super::bindings::helios::system::stats::{
    BlockDevice, HostShareCache, Iommu, IommuEndpoint, Memory, MemoryPressure, MonoNanos, Network,
    NetworkQueue, Permille, Processor, Processors, Sample,
};

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::error::{RpcError, TransportError};
    use crate::system::methods::{STATS_INSTANCE, STATS_SNAPSHOT};
    use crate::transport::Client;
    use futures_io::{AsyncRead, AsyncWrite};

    pub async fn snapshot<R, W>(client: &Client<R, W>) -> Result<Sample, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = client
            .invoke_raw(STATS_INSTANCE, STATS_SNAPSHOT, Vec::new())
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: STATS_INSTANCE,
                func: STATS_SNAPSHOT,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: STATS_INSTANCE,
            func: STATS_SNAPSHOT,
            source,
        })
    }
}

#[cfg(feature = "host")]
pub use host::*;
