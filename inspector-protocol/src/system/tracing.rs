pub use super::bindings::helios::system::filters::FilterError;
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

    /// The newest events matching `filter`. `Ok(Err(..))` is the guest
    /// refusing a filter that exceeds its prefix bounds.
    pub async fn recent<R, W>(
        client: &Client<R, W>,
        filter: &Filter,
        limit: u32,
    ) -> Result<Result<Vec<Event>, FilterError>, RpcError>
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

#[cfg(test)]
mod tests {
    use super::{Event, FilterError};

    /// What the guest sends for a refused filter reaches the inspector
    /// with the limit it names intact.
    #[test]
    fn filter_error_round_trips_the_wire() {
        for error in [
            FilterError::TooManyPrefixes(8),
            FilterError::PrefixTooLong(128),
        ] {
            let sent: Result<Vec<Event>, FilterError> = Err(error);
            let bytes = postcard::to_allocvec(&sent).expect("a filter refusal encodes");
            let received: Result<Vec<Event>, FilterError> =
                postcard::from_bytes(&bytes).expect("a filter refusal decodes");
            assert_eq!(format!("{received:?}"), format!("{sent:?}"));
        }
    }
}
