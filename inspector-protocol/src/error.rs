//! Typed error types for the inspector protocol.
//!
//! The protocol crate is a library; AGENTS §3 forbids `anyhow` here. Errors
//! are encoded as structured enums so callers (inspector, CLI, debugger guest)
//! can dispatch on a stable contract instead of opaque strings.
//!
//! `wrpc_transport::{Invoke, Serve, Index}` are upstream contracts that
//! return `anyhow::Result` and we cannot replace them; the
//! [`crate::transport`] module documents that boundary in detail.

#[cfg(feature = "host")]
use std::io;

use thiserror::Error;

/// Lower-level transport faults raised by the wRPC framing client/server.
#[cfg(feature = "host")]
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport closed unexpectedly")]
    Closed,

    #[error("remote rejected invocation: {0}")]
    Rejected(String),

    #[error("server transport received unexpected reply frame")]
    UnexpectedReply,

    #[error("handler queue for {instance}.{func} was closed")]
    HandoffClosed { instance: String, func: String },

    #[error("transport I/O failed during {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
}

#[cfg(feature = "host")]
impl TransportError {
    pub(crate) fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}

#[cfg(feature = "host")]
impl From<TransportError> for io::Error {
    fn from(error: TransportError) -> Self {
        io::Error::other(error)
    }
}

/// Remote-procedure-call error covering encode, transport, and decode faults.
///
/// Service-level errors that the remote method itself reports (for example
/// `programs::ExecError`) are returned as nested `Result` values from the
/// caller-facing function rather than collapsed into this enum so each call
/// site keeps its typed service contract.
#[derive(Debug, Error)]
pub enum RpcError {
    #[error("failed to encode {instance}.{func} request: {source}")]
    Encode {
        instance: &'static str,
        func: &'static str,
        #[source]
        source: postcard::Error,
    },

    #[cfg(feature = "host")]
    #[error("failed to invoke {instance}.{func}: {source}")]
    Invoke {
        instance: &'static str,
        func: &'static str,
        #[source]
        source: TransportError,
    },

    #[error("failed to decode {instance}.{func} response: {source}")]
    Decode {
        instance: &'static str,
        func: &'static str,
        #[source]
        source: postcard::Error,
    },
}

/// Server-side dispatch error used by the in-debugger guest implementation.
#[cfg(feature = "guest")]
#[derive(Debug, Error)]
pub enum DispatchError {
    #[error("failed to encode {operation} response: {source}")]
    Encode {
        operation: &'static str,
        #[source]
        source: postcard::Error,
    },

    #[error("failed to decode {operation} request payload: {source}")]
    Decode {
        operation: &'static str,
        #[source]
        source: postcard::Error,
    },

    #[error("{operation} expects no request payload")]
    UnexpectedPayload { operation: &'static str },

    #[error("{message}")]
    Protocol { message: String },

    #[error("{0}")]
    Filesystem(String),

    #[error("transport I/O failed during {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(feature = "guest")]
impl DispatchError {
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
        }
    }
}
