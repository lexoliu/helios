//! Typed error types for the inspector protocol.
//!
//! The protocol crate is a library; AGENTS §3 forbids `anyhow` here. Errors
//! are encoded as structured enums so callers (inspector, CLI, debugger guest)
//! can dispatch on a stable contract instead of opaque strings.
//!
//! Test-only wRPC compatibility shims still adapt to upstream
//! `anyhow::Result`, but the production transport API never exposes that
//! boundary.

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

    /// The guest's kernel printed a panic report on the console the
    /// frame scanner reads.
    ///
    /// A panicked guest answers no further frame, so without this the
    /// call in flight blocks until the caller's outer deadline: the
    /// benchmark lane that found it spent 2.5 h of a shared runner
    /// waiting on a kernel that had been dead for two seconds.
    #[error("guest kernel panicked: {report}")]
    GuestPanicked { report: String },
}

#[cfg(feature = "host")]
impl TransportError {
    pub(crate) fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }

    /// The guest's panic report when this fault is a dead guest rather
    /// than a transport of its own.
    ///
    /// The walk is explicit because `io::Error` does not expose a
    /// custom payload through [`std::error::Error::source`], so an
    /// error chain alone cannot find a panic that crossed the framing
    /// layer.
    pub fn guest_panic(&self) -> Option<&str> {
        match self {
            Self::GuestPanicked { report } => Some(report),
            Self::Io { source, .. } => source
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<Self>())
                .and_then(Self::guest_panic),
            Self::Closed
            | Self::Rejected(_)
            | Self::UnexpectedReply
            | Self::HandoffClosed { .. } => None,
        }
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

impl RpcError {
    /// The guest's panic report when this call failed because the guest
    /// kernel died rather than because the call itself was refused.
    ///
    /// A caller that drives many calls in a row — the workload bench —
    /// uses it to stop instead of asking a dead guest the next question.
    pub fn guest_panic(&self) -> Option<&str> {
        match self {
            #[cfg(feature = "host")]
            Self::Invoke { source, .. } => source.guest_panic(),
            Self::Encode { .. } | Self::Decode { .. } => None,
        }
    }
}

/// Server-side dispatch error used by the in-debugger guest implementation.
///
/// Not gated on the `guest` feature: the RPC server loop that raises it
/// is generic over its dispatcher, and the host build tests that loop.
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

impl DispatchError {
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
        }
    }
}
