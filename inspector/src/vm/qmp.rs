//! A minimal QMP client for the knobs the inspector exposes.
//!
//! QEMU's machine protocol is a line-oriented JSON stream: a greeting,
//! a capability handshake, then one response object per command, with
//! asynchronous events interleaved between them. Only the commands the
//! inspector actually drives live here — the point is to spare a
//! developer a hand-written `socat` session, not to grow a second QEMU
//! front end.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

/// How long a single command waits for its response.
///
/// A guest that is handing memory back keeps QEMU's vCPU thread busy
/// discarding pages, and the monitor only runs between those bursts, so
/// a reply can take a while. It is still bounded: a caller that cannot
/// get an answer says so rather than hanging the session.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);

/// Why a QMP command did not produce an answer.
///
/// The socket faults, the protocol faults and QEMU's own refusal are
/// separate variants because they point at different things: the host's
/// socket, this client's framing, and the machine's state.
#[derive(Debug, thiserror::Error)]
pub(crate) enum QmpError {
    #[error("failed to connect to QMP socket {socket}: {source}")]
    Connect {
        socket: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to set a QMP read timeout: {source}")]
    SetTimeout {
        #[source]
        source: std::io::Error,
    },
    #[error("failed to clone QMP socket: {source}")]
    CloneSocket {
        #[source]
        source: std::io::Error,
    },
    #[error("QMP socket opened with {greeting} instead of a greeting")]
    MissingGreeting { greeting: Value },
    #[error("failed to encode a QMP command: {source}")]
    Encode {
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to {step} the QMP command {command}: {source}")]
    Send {
        /// `send` or `flush`, the write step that failed.
        step: &'static str,
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("QMP command {command} failed: {error}")]
    Refused { command: String, error: Value },
    #[error("failed to read from the QMP socket: {source}")]
    Read {
        #[source]
        source: std::io::Error,
    },
    #[error("the QMP socket closed while waiting for a response")]
    Closed,
    #[error("failed to decode the QMP message {line:?}: {source}")]
    DecodeMessage {
        line: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to decode a query-balloon response: {source}")]
    DecodeBalloon {
        #[source]
        source: serde_json::Error,
    },
}

/// Why a size the caller wrote is not one QEMU would accept.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SizeError {
    #[error("{text:?} is not a size QEMU would accept: {source}")]
    NotANumber {
        text: String,
        #[source]
        source: core::num::ParseIntError,
    },
    #[error("the size {text:?} overflows a 64-bit byte count")]
    Overflow { text: String },
}

/// One QMP session over a QEMU monitor socket.
pub(crate) struct QmpClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

/// What `query-balloon` reports.
#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct BalloonInfo {
    /// The guest memory size QEMU currently allows, in bytes. The
    /// balloon holds the difference between this and `-m`.
    pub(crate) actual: u64,
}

impl QmpClient {
    /// Connects to `socket` and completes the capability handshake.
    pub(crate) fn connect(socket: &Path) -> Result<Self, QmpError> {
        let stream = UnixStream::connect(socket).map_err(|source| QmpError::Connect {
            socket: socket.display().to_string(),
            source,
        })?;
        stream
            .set_read_timeout(Some(RESPONSE_TIMEOUT))
            .map_err(|source| QmpError::SetTimeout { source })?;
        let writer = stream
            .try_clone()
            .map_err(|source| QmpError::CloneSocket { source })?;
        let mut client = Self {
            reader: BufReader::new(stream),
            writer,
        };
        // The greeting arrives unprompted and has to be consumed before
        // the handshake, or the handshake's own reply is read as the
        // greeting.
        let greeting = client.read_message()?;
        if greeting.get("QMP").is_none() {
            return Err(QmpError::MissingGreeting { greeting });
        }
        client.execute("qmp_capabilities", Value::Null)?;
        Ok(client)
    }

    /// Sets the balloon target: the guest memory size QEMU asks the
    /// guest to keep, in bytes.
    pub(crate) fn set_balloon(&mut self, bytes: u64) -> Result<(), QmpError> {
        self.execute("balloon", json!({ "value": bytes }))?;
        Ok(())
    }

    /// Reads what the guest has actually given up.
    pub(crate) fn query_balloon(&mut self) -> Result<BalloonInfo, QmpError> {
        let value = self.execute("query-balloon", Value::Null)?;
        serde_json::from_value(value).map_err(|source| QmpError::DecodeBalloon { source })
    }

    fn execute(&mut self, command: &str, arguments: Value) -> Result<Value, QmpError> {
        let mut request = json!({ "execute": command });
        if !arguments.is_null() {
            request["arguments"] = arguments;
        }
        let mut line =
            serde_json::to_vec(&request).map_err(|source| QmpError::Encode { source })?;
        line.push(b'\n');
        let write = |step| {
            move |source| QmpError::Send {
                step,
                command: command.to_owned(),
                source,
            }
        };
        self.writer.write_all(&line).map_err(write("send"))?;
        self.writer.flush().map_err(write("flush"))?;

        loop {
            let message = self.read_message()?;
            if let Some(error) = message.get("error") {
                return Err(QmpError::Refused {
                    command: command.to_owned(),
                    error: error.clone(),
                });
            }
            if let Some(result) = message.get("return") {
                return Ok(result.clone());
            }
            // Anything else is an asynchronous event, which is not this
            // command's answer.
        }
    }

    fn read_message(&mut self) -> Result<Value, QmpError> {
        let mut line = String::new();
        loop {
            line.clear();
            let read = self
                .reader
                .read_line(&mut line)
                .map_err(|source| QmpError::Read { source })?;
            if read == 0 {
                return Err(QmpError::Closed);
            }
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(&line).map_err(|source| QmpError::DecodeMessage {
                line: line.clone(),
                source,
            });
        }
    }
}

/// Parses a QEMU-style size: a decimal count with an optional
/// `K`/`M`/`G` suffix, as `-m` and the monitor's own `balloon` command
/// take.
pub(crate) fn parse_size(text: &str) -> Result<u64, SizeError> {
    let text = text.trim();
    let (digits, scale) = match text.chars().last() {
        Some('K' | 'k') => (&text[..text.len() - 1], 1024_u64),
        Some('M' | 'm') => (&text[..text.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text, 1),
    };
    let value: u64 = digits.parse().map_err(|source| SizeError::NotANumber {
        text: text.to_owned(),
        source,
    })?;
    value.checked_mul(scale).ok_or_else(|| SizeError::Overflow {
        text: text.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn sizes_take_the_suffixes_qemu_takes() {
        assert_eq!(parse_size("1024").expect("bare byte count"), 1024);
        assert_eq!(parse_size("2K").expect("kibibytes"), 2 * 1024);
        assert_eq!(parse_size("1536M").expect("mebibytes"), 1536 * 1024 * 1024);
        assert_eq!(parse_size("2G").expect("gibibytes"), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size(" 4g ").expect("padded"), 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn a_size_that_is_not_a_number_is_rejected() {
        parse_size("plenty").expect_err("a bare word is not a size");
        parse_size("1T").expect_err("only K, M and G are QEMU's suffixes");
    }
}
