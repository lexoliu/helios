//! `helios:system/processors` over the inspector RPC: the possible and
//! online sets, and a processor taken offline or brought online.
//!
//! The answer types are the ones the WIT generates, so every case the
//! kernel's lifecycle can give reaches the host by its own name. The
//! guest dispatcher converts the guest's bindings to these with an
//! exhaustive match per type: a case added to `processors.wit` fails the
//! guest build until it is mapped, and postcard encodes a variant by its
//! position, so a case appended to the WIT leaves every earlier case's
//! encoding as it was.

pub use super::bindings::helios::system::processors::{
    OfflineError, OfflineRefusal, OnlineError, ProcessorId, ProcessorSets,
};

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::error::{RpcError, TransportError};
    use crate::system::methods::{
        PROCESSORS_INSTANCE, PROCESSORS_OFFLINE, PROCESSORS_ONLINE, PROCESSORS_SETS,
    };
    use crate::transport::Client;
    use futures_io::{AsyncRead, AsyncWrite};
    use serde::de::DeserializeOwned;

    /// The possible and online sets, as the guest kernel holds them now.
    pub async fn sets<R, W>(client: &Client<R, W>) -> Result<ProcessorSets, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = client
            .invoke_raw(PROCESSORS_INSTANCE, PROCESSORS_SETS, Vec::new())
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: PROCESSORS_INSTANCE,
                func: PROCESSORS_SETS,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: PROCESSORS_INSTANCE,
            func: PROCESSORS_SETS,
            source,
        })
    }

    /// Takes `processor` offline. The guest answers once the processor
    /// has stopped, or with the lifecycle's reason it did not.
    pub async fn offline<R, W>(
        client: &Client<R, W>,
        processor: ProcessorId,
    ) -> Result<Result<(), OfflineError>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        transition(client, PROCESSORS_OFFLINE, processor).await
    }

    /// Brings `processor` online. The guest answers once it has joined
    /// the online set, or with the lifecycle's reason it did not.
    pub async fn online<R, W>(
        client: &Client<R, W>,
        processor: ProcessorId,
    ) -> Result<Result<(), OnlineError>, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        transition(client, PROCESSORS_ONLINE, processor).await
    }

    async fn transition<R, W, Answer>(
        client: &Client<R, W>,
        func: &'static str,
        processor: ProcessorId,
    ) -> Result<Answer, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
        Answer: DeserializeOwned,
    {
        let payload = postcard::to_allocvec(&processor).map_err(|source| RpcError::Encode {
            instance: PROCESSORS_INSTANCE,
            func,
            source,
        })?;
        let bytes = client
            .invoke_raw(PROCESSORS_INSTANCE, func, payload)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: PROCESSORS_INSTANCE,
                func,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: PROCESSORS_INSTANCE,
            func,
            source,
        })
    }
}

#[cfg(feature = "host")]
pub use host::*;

#[cfg(test)]
mod tests {
    use super::{OfflineError, OfflineRefusal, OnlineError};

    use serde::Serialize;
    use serde::de::DeserializeOwned;

    /// Encodes `answer`, checks it starts with `prefix`, and checks it
    /// decodes to a value that encodes the same way.
    fn round_trip<T: Serialize + DeserializeOwned>(answer: &T, prefix: &[u8]) {
        let bytes = postcard::to_allocvec(answer).expect("an answer encodes");
        assert_eq!(&bytes[..prefix.len()], prefix);
        let decoded = postcard::from_bytes::<T>(&bytes).expect("an answer decodes");
        assert_eq!(
            postcard::to_allocvec(&decoded).expect("a decoded answer encodes"),
            bytes
        );
    }

    /// Each case decodes back to itself under the position it was
    /// encoded with — `Err`, then the case's index among the variants —
    /// so a case appended to the WIT moves no earlier one.
    #[test]
    fn every_transition_answer_survives_the_wire_in_its_place() {
        round_trip::<Result<(), OfflineError>>(&Ok(()), &[0]);
        let offline = [
            OfflineError::NotPossible(4),
            OfflineError::Bootstrap,
            OfflineError::CurrentProcessor,
            OfflineError::NotOnline,
            OfflineError::Refused(OfflineRefusal::PinnedTasks(3)),
            OfflineError::Denied,
        ];
        for (position, error) in (0_u8..).zip(offline) {
            round_trip::<Result<(), OfflineError>>(&Err(error), &[1, position]);
        }
        // `refused` is case 4; its refusal's own index follows.
        let refusals = [
            OfflineRefusal::PinnedTasks(3),
            OfflineRefusal::NetworkStarting,
        ];
        for (position, refusal) in (0_u8..).zip(refusals) {
            round_trip::<Result<(), OfflineError>>(
                &Err(OfflineError::Refused(refusal)),
                &[1, 4, position],
            );
        }

        let online = [
            OnlineError::NotPossible(4),
            OnlineError::NotOffline,
            OnlineError::Denied,
        ];
        for (position, error) in (0_u8..).zip(online) {
            round_trip::<Result<(), OnlineError>>(&Err(error), &[1, position]);
        }
    }
}
