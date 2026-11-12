//! Preparing the guest for a VMM snapshot, and telling it the snapshot
//! is over.
//!
//! `prepare-snapshot` has the guest return the host share to the VMM,
//! which refuses to snapshot a machine that still has it attached, and
//! answers with what the host needs to decide whether the snapshot is
//! safe to take. `snapshot-finished` ends the hold on the machine that
//! keeps running; a machine restored from the snapshot ends its own hold
//! when its generation advances.

use serde::{Deserialize, Serialize};

/// What the guest tells the host when it has released the share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotReadiness {
    /// The machine has a VM generation ID, so a restored copy can tell
    /// it is a copy. Without one, restoring the snapshot gives the
    /// guest no signal to reseed its entropy or re-attach its share on.
    pub has_generation_id: bool,
    /// Some other invocation on this connection is still running, such
    /// as a program started through `programs.exec`, and the snapshot
    /// would capture it half way.
    pub other_invocations_in_flight: bool,
}

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::debugger::methods::{PREPARE_SNAPSHOT, SNAPSHOT_FINISHED, SNAPSHOT_INSTANCE};
    use crate::error::{RpcError, TransportError};
    use crate::transport::Client;
    use futures_io::{AsyncRead, AsyncWrite};

    /// Has the guest release its host share and hold it released, and
    /// returns once it has. A caller that gives up before the answer
    /// must still send [`snapshot_finished`], or the guest's host-share
    /// operations stay parked.
    pub async fn prepare_snapshot<R, W>(
        client: &Client<R, W>,
    ) -> Result<SnapshotReadiness, RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = client
            .invoke_raw(SNAPSHOT_INSTANCE, PREPARE_SNAPSHOT, Vec::new())
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: SNAPSHOT_INSTANCE,
                func: PREPARE_SNAPSHOT,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: SNAPSHOT_INSTANCE,
            func: PREPARE_SNAPSHOT,
            source,
        })
    }

    /// Tells the guest the snapshot is over and it keeps running, so it
    /// attaches its host share again.
    pub async fn snapshot_finished<R, W>(client: &Client<R, W>) -> Result<(), RpcError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = client
            .invoke_raw(SNAPSHOT_INSTANCE, SNAPSHOT_FINISHED, Vec::new())
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: SNAPSHOT_INSTANCE,
                func: SNAPSHOT_FINISHED,
                source,
            })?;
        postcard::from_bytes(&bytes).map_err(|source| RpcError::Decode {
            instance: SNAPSHOT_INSTANCE,
            func: SNAPSHOT_FINISHED,
            source,
        })
    }
}

#[cfg(feature = "host")]
pub use host::*;

#[cfg(feature = "guest")]
mod guest {
    use super::*;
    use crate::debugger::methods::{PREPARE_SNAPSHOT, SNAPSHOT_FINISHED, SNAPSHOT_INSTANCE};
    use crate::error::DispatchError;
    use crate::system::server::Invocations;
    use helios_api::snapshot;

    pub(crate) fn supports(instance: &str, func: &str) -> bool {
        instance == SNAPSHOT_INSTANCE && matches!(func, PREPARE_SNAPSHOT | SNAPSHOT_FINISHED)
    }

    pub(crate) async fn dispatch(
        invocations: &Invocations,
        func: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, DispatchError> {
        let operation = match func {
            PREPARE_SNAPSHOT => "debugger snapshot.prepare-snapshot",
            SNAPSHOT_FINISHED => "debugger snapshot.snapshot-finished",
            _ => unreachable!("supports must reject unsupported debugger snapshot requests"),
        };
        if !payload.is_empty() {
            return Err(DispatchError::UnexpectedPayload { operation });
        }
        let encoded = match func {
            PREPARE_SNAPSHOT => {
                let prepared = snapshot::prepare().await;
                postcard::to_allocvec(&SnapshotReadiness {
                    has_generation_id: prepared.has_generation_id,
                    other_invocations_in_flight: invocations.others() != 0,
                })
            }
            _ => {
                snapshot::finished();
                postcard::to_allocvec(&())
            }
        };
        encoded.map_err(|source| DispatchError::Encode { operation, source })
    }
}

#[cfg(feature = "guest")]
pub(crate) use guest::{dispatch, supports};
