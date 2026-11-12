//! The `wasi:http` error code each failure of the client stands for.
//!
//! The client reports typed failures; this is the one place that decides
//! which RFC 9209 code a failure is, so the guest sees the same code whether
//! the exchange failed before its response head or while its body streamed.

use std::io::{self, ErrorKind};
use std::string::ToString;

use helios_api::http::{DnsErrorPayload, ErrorCode, TlsAlertReceivedPayload};
use zenwave::{
    Attempt, Cause, ConnectFailure, ConnectPhase, Error, ExchangeFailure, ReceiveError, TimedOut,
    TlsFailure,
};

use crate::request::BodyOutcome;

/// The code for a failure of `respond`.
pub fn code(error: &Error, outcome: &BodyOutcome) -> ErrorCode {
    match error {
        Error::Timeout(TimedOut::Connect {
            during: ConnectPhase::Resolve,
        }) => ErrorCode::DnsTimeout,
        Error::Timeout(TimedOut::Connect {
            during: ConnectPhase::Queued,
        }) => ErrorCode::ConnectionLimitReached,
        Error::Timeout(TimedOut::Connect { .. }) => ErrorCode::ConnectionTimeout,
        Error::Timeout(TimedOut::ResponseHead | TimedOut::Request) => {
            ErrorCode::HttpResponseTimeout
        }
        Error::Timeout(TimedOut::BodyIdle) => ErrorCode::ConnectionReadTimeout,
        Error::Connect(connect) => match &connect.failure {
            ConnectFailure::Resolve(_) => ErrorCode::DnsError(DnsErrorPayload {
                rcode: None,
                info_code: None,
            }),
            ConnectFailure::Tcp(tcp) => tcp_code(&tcp.attempts),
            ConnectFailure::Tls(tls) => tls_code(tls.failure, &tls.cause),
            // No proxy is configured and `wasi:sockets` has no Unix sockets,
            // so these routes are never taken.
            other => internal(other),
        },
        Error::Exchange(exchange) => match exchange.failure {
            ExchangeFailure::Terminated => ErrorCode::ConnectionTerminated,
            ExchangeFailure::Incomplete => ErrorCode::HttpResponseIncomplete,
            ExchangeFailure::HeadTooLarge { limit } => {
                ErrorCode::HttpResponseHeaderSectionSize(u32::try_from(limit).ok())
            }
            ExchangeFailure::TooManyFields { .. } => ErrorCode::HttpResponseHeaderSectionSize(None),
            ExchangeFailure::TransferCoding => ErrorCode::HttpResponseTransferCoding(None),
            ExchangeFailure::Protocol | ExchangeFailure::InvalidRequest => {
                ErrorCode::HttpProtocolError
            }
            ExchangeFailure::TrailersTooLarge { limit } => {
                ErrorCode::HttpResponseTrailerSectionSize(u32::try_from(limit).ok())
            }
            ExchangeFailure::TooManyTrailers { .. } => {
                ErrorCode::HttpResponseTrailerSectionSize(None)
            }
            ExchangeFailure::RequestBodyLength { .. } => {
                ErrorCode::HttpRequestBodySize(Some(outcome.sent()))
            }
            // The caller's own body stream failed: its code is the verdict.
            ExchangeFailure::RequestBody => outcome.failure().unwrap_or_else(|| internal(error)),
            ExchangeFailure::Tls => ErrorCode::TlsProtocolError,
            _ => internal(error),
        },
        Error::InvalidUri(_) => ErrorCode::HttpRequestUriInvalid,
        Error::Config(_) => ErrorCode::ConfigurationError,
        _ => internal(error),
    }
}

/// The code for a failure of the response body, `https` saying whether the
/// connection under it carries TLS: its records fail as `InvalidData`.
pub fn body_code(error: &zenwave::BodyError, https: bool) -> ErrorCode {
    let zenwave::BodyError::Receive(receive) = error else {
        return internal(error);
    };
    match receive {
        ReceiveError::Stalled => ErrorCode::ConnectionReadTimeout,
        ReceiveError::Incomplete => ErrorCode::HttpResponseIncomplete,
        ReceiveError::Framing { .. } => ErrorCode::HttpProtocolError,
        ReceiveError::TrailersTooLarge { limit } => {
            ErrorCode::HttpResponseTrailerSectionSize(u32::try_from(*limit).ok())
        }
        ReceiveError::TooManyTrailers { .. } => ErrorCode::HttpResponseTrailerSectionSize(None),
        ReceiveError::Connection(error) => match error.kind() {
            ErrorKind::InvalidData if https => ErrorCode::TlsProtocolError,
            ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof => ErrorCode::ConnectionTerminated,
            _ => internal(error),
        },
        _ => internal(error),
    }
}

fn internal(error: &impl ToString) -> ErrorCode {
    ErrorCode::InternalError(Some(error.to_string()))
}

/// RFC 8305 races several addresses, so the attempts can differ; the first
/// rule that holds names the failure.
fn tcp_code(attempts: &[Attempt]) -> ErrorCode {
    let all = |kinds: &[ErrorKind]| {
        !attempts.is_empty()
            && attempts
                .iter()
                .all(|attempt| kinds.contains(&attempt.error.kind()))
    };
    let any = |kinds: &[ErrorKind]| {
        attempts
            .iter()
            .any(|attempt| kinds.contains(&attempt.error.kind()))
    };
    if all(&[ErrorKind::HostUnreachable, ErrorKind::NetworkUnreachable]) {
        ErrorCode::DestinationIpUnroutable
    } else if all(&[ErrorKind::PermissionDenied]) {
        ErrorCode::DestinationIpProhibited
    } else if any(&[
        ErrorKind::ConnectionRefused,
        ErrorKind::ConnectionReset,
        ErrorKind::ConnectionAborted,
    ]) {
        ErrorCode::ConnectionRefused
    } else if any(&[ErrorKind::TimedOut]) {
        ErrorCode::ConnectionTimeout
    } else {
        ErrorCode::InternalError(attempts.first().map(|attempt: &Attempt| {
            let error: &io::Error = &attempt.error;
            error.to_string()
        }))
    }
}

fn tls_code(failure: TlsFailure, cause: &Cause) -> ErrorCode {
    match failure {
        TlsFailure::Certificate => ErrorCode::TlsCertificateError,
        TlsFailure::AlertReceived { alert } => {
            ErrorCode::TlsAlertReceived(TlsAlertReceivedPayload {
                alert_id: Some(alert),
                alert_message: Some(cause.to_string()),
            })
        }
        _ => ErrorCode::TlsProtocolError,
    }
}
