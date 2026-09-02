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

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

/// How long a single command waits for its response.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

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
    pub(crate) fn connect(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("failed to connect to QMP socket {}", socket.display()))?;
        stream
            .set_read_timeout(Some(RESPONSE_TIMEOUT))
            .context("failed to set a QMP read timeout")?;
        let writer = stream.try_clone().context("failed to clone QMP socket")?;
        let mut client = Self {
            reader: BufReader::new(stream),
            writer,
        };
        // The greeting arrives unprompted and has to be consumed before
        // the handshake, or the handshake's own reply is read as the
        // greeting.
        let greeting = client.read_message()?;
        if greeting.get("QMP").is_none() {
            bail!("QMP socket opened with {greeting} instead of a greeting");
        }
        client.execute("qmp_capabilities", Value::Null)?;
        Ok(client)
    }

    /// Sets the balloon target: the guest memory size QEMU asks the
    /// guest to keep, in bytes.
    pub(crate) fn set_balloon(&mut self, bytes: u64) -> Result<()> {
        self.execute("balloon", json!({ "value": bytes }))?;
        Ok(())
    }

    /// Reads what the guest has actually given up.
    pub(crate) fn query_balloon(&mut self) -> Result<BalloonInfo> {
        let value = self.execute("query-balloon", Value::Null)?;
        serde_json::from_value(value).context("failed to decode a query-balloon response")
    }

    fn execute(&mut self, command: &str, arguments: Value) -> Result<Value> {
        let mut request = json!({ "execute": command });
        if !arguments.is_null() {
            request["arguments"] = arguments;
        }
        let mut line = serde_json::to_vec(&request).context("failed to encode a QMP command")?;
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .with_context(|| format!("failed to send the QMP command {command}"))?;
        self.writer
            .flush()
            .with_context(|| format!("failed to flush the QMP command {command}"))?;

        loop {
            let message = self.read_message()?;
            if let Some(error) = message.get("error") {
                bail!("QMP command {command} failed: {error}");
            }
            if let Some(result) = message.get("return") {
                return Ok(result.clone());
            }
            // Anything else is an asynchronous event, which is not this
            // command's answer.
        }
    }

    fn read_message(&mut self) -> Result<Value> {
        let mut line = String::new();
        loop {
            line.clear();
            let read = self
                .reader
                .read_line(&mut line)
                .context("failed to read from the QMP socket")?;
            if read == 0 {
                bail!("the QMP socket closed while waiting for a response");
            }
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(&line)
                .with_context(|| format!("failed to decode the QMP message {line:?}"));
        }
    }
}

/// Parses a QEMU-style size: a decimal count with an optional
/// `K`/`M`/`G` suffix, as `-m` and the monitor's own `balloon` command
/// take.
pub(crate) fn parse_size(text: &str) -> Result<u64> {
    let text = text.trim();
    let (digits, scale) = match text.chars().last() {
        Some('K' | 'k') => (&text[..text.len() - 1], 1024_u64),
        Some('M' | 'm') => (&text[..text.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text, 1),
    };
    let value: u64 = digits
        .parse()
        .with_context(|| format!("{text:?} is not a size QEMU would accept"))?;
    value
        .checked_mul(scale)
        .with_context(|| format!("the size {text:?} overflows a 64-bit byte count"))
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
