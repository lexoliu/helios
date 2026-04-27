mod rpc_server;
mod transport_io;

use thiserror::Error;

#[derive(Debug, Error)]
enum DebuggerError {
    #[error("debug serial capability is missing")]
    MissingSerialCapability,

    #[error("inspector dispatch failed: {0}")]
    Dispatch(#[from] helios_inspector_protocol::DispatchError),
}

#[helios_api::main]
async fn main() -> Result<(), DebuggerError> {
    rpc_server::serve_debugger().await
}

pub(crate) use DebuggerError as Error;
