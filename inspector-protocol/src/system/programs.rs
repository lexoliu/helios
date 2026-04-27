pub use super::bindings::helios::system::programs::{
    AotHint, AotRequest, AotResult, ExecError, ExecErrorKind, ExecOutput, ExecRequest, ExecResult,
};

#[cfg(feature = "host")]
mod host {
    use super::*;
    use crate::error::{RpcError, TransportError};
    use crate::system::methods::{PROGRAMS_AOT, PROGRAMS_EXEC, PROGRAMS_INSTANCE};
    use crate::transport::Client;
    use futures_io::{AsyncRead, AsyncWrite};

    /// Outcome of `programs.exec`: outer `Result` covers transport / encode /
    /// decode faults, inner `Result` carries the typed service error contract.
    pub type ExecOutcome = Result<Result<ExecResult, ExecError>, RpcError>;

    /// Outcome of `programs.aot`: outer `Result` covers transport / encode /
    /// decode faults, inner `Result` carries the typed service error contract.
    pub type AotOutcome = Result<Result<AotResult, ExecError>, RpcError>;

    pub async fn exec<R, W>(client: &Client<R, W>, request: &ExecRequest) -> ExecOutcome
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = postcard::to_allocvec(request).map_err(|source| RpcError::Encode {
            instance: PROGRAMS_INSTANCE,
            func: PROGRAMS_EXEC,
            source,
        })?;
        let bytes = client
            .invoke_raw_streaming(PROGRAMS_INSTANCE, PROGRAMS_EXEC, bytes)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: PROGRAMS_INSTANCE,
                func: PROGRAMS_EXEC,
                source,
            })?;
        postcard::from_bytes::<Result<ExecResult, ExecError>>(&bytes).map_err(|source| {
            RpcError::Decode {
                instance: PROGRAMS_INSTANCE,
                func: PROGRAMS_EXEC,
                source,
            }
        })
    }

    pub async fn aot<R, W>(client: &Client<R, W>, request: &AotRequest) -> AotOutcome
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let bytes = postcard::to_allocvec(request).map_err(|source| RpcError::Encode {
            instance: PROGRAMS_INSTANCE,
            func: PROGRAMS_AOT,
            source,
        })?;
        let bytes = client
            .invoke_raw_streaming(PROGRAMS_INSTANCE, PROGRAMS_AOT, bytes)
            .await
            .map_err(|source: TransportError| RpcError::Invoke {
                instance: PROGRAMS_INSTANCE,
                func: PROGRAMS_AOT,
                source,
            })?;
        postcard::from_bytes::<Result<AotResult, ExecError>>(&bytes).map_err(|source| {
            RpcError::Decode {
                instance: PROGRAMS_INSTANCE,
                func: PROGRAMS_AOT,
                source,
            }
        })
    }
}

#[cfg(feature = "host")]
pub use host::*;
