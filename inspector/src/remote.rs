use std::future::Future;
use std::time::Duration;

use helios_inspector_protocol::RpcError;

use crate::runtime;

const REMOTE_TIMEOUT: Duration = Duration::from_secs(180);

/// Why one remote call did not produce an answer.
///
/// The two cases are worth telling apart: a call that timed out reached
/// a guest that stopped answering, while a call that failed got as far
/// as the RPC layer, whose own error says where it stopped.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("timed out waiting for {waiting_for}")]
    TimedOut { waiting_for: &'static str },
    #[error("remote {waiting_for} failed: {source}")]
    Failed {
        waiting_for: &'static str,
        #[source]
        source: RpcError,
    },
}

impl RemoteError {
    /// The guest's panic report when the call failed because the guest
    /// kernel died rather than because the call itself was refused.
    ///
    /// Typed rather than a downcast through an erased chain: only the
    /// RPC layer can carry a panic report, so only that variant is asked
    /// for one.
    pub fn guest_panic(&self) -> Option<&str> {
        match self {
            Self::Failed { source, .. } => source.guest_panic(),
            Self::TimedOut { .. } => None,
        }
    }
}

pub async fn call<T>(
    future: impl Future<Output = core::result::Result<T, RpcError>>,
    waiting_for: &'static str,
) -> Result<T, RemoteError> {
    let value = runtime::timeout(REMOTE_TIMEOUT, future)
        .await
        .ok_or(RemoteError::TimedOut { waiting_for })?;
    value.map_err(|source| RemoteError::Failed {
        waiting_for,
        source,
    })
}
