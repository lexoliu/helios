//! Buffered reader over the kernel TCP socket.
//!
//! HTTP/1.1 framing is line- and length-oriented while the socket hands back
//! arbitrary chunks, so every reader in this crate works against one buffer
//! that it refills on demand and consumes from by byte count.

use std::string::ToString;
use std::time::Duration;
use std::vec::Vec;

use helios_api::http::ErrorCode;
use helios_api::net::TcpStream;

/// Bytes requested from the socket per read.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Buffered view of the connection, plus the socket itself for writing.
pub struct Socket {
    stream: TcpStream,
    buffer: Vec<u8>,
    cursor: usize,
    eof: bool,
}

impl Socket {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
            cursor: 0,
            eof: false,
        }
    }

    /// Set the timeout applied to subsequent socket operations.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.stream.set_timeout(timeout);
    }

    /// Bytes read from the peer but not yet consumed.
    pub fn buffered(&self) -> &[u8] {
        &self.buffer[self.cursor..]
    }

    /// Mark `count` buffered bytes as handled.
    pub fn consume(&mut self, count: usize) {
        assert!(
            self.cursor + count <= self.buffer.len(),
            "consumed more bytes than the socket buffer holds"
        );
        self.cursor += count;
    }

    /// Pull more bytes from the peer.
    ///
    /// Returns `false` once the peer has closed its side, at which point the
    /// buffer holds everything that will ever arrive.
    pub async fn fill(&mut self) -> Result<bool, ErrorCode> {
        if self.eof {
            return Ok(false);
        }
        if self.cursor > 0 {
            self.buffer.drain(..self.cursor);
            self.cursor = 0;
        }
        match self.stream.read(READ_CHUNK_BYTES).await {
            Ok(Some(bytes)) => {
                self.buffer.extend_from_slice(&bytes);
                Ok(true)
            }
            Ok(None) => {
                self.eof = true;
                Ok(false)
            }
            Err(error) => Err(read_error(&error)),
        }
    }

    /// Write `bytes` to the peer in full.
    pub async fn write_all(&self, bytes: &[u8]) -> Result<(), ErrorCode> {
        self.stream
            .write_all(bytes)
            .await
            .map_err(|error| write_error(&error))
    }
}

/// Socket read failure as the `wasi:http` error the caller sees.
pub fn read_error(error: &std::io::Error) -> ErrorCode {
    match error.kind() {
        std::io::ErrorKind::TimedOut => ErrorCode::HttpResponseTimeout,
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
            ErrorCode::ConnectionTerminated
        }
        _ => ErrorCode::InternalError(Some(error.to_string())),
    }
}

/// Socket write failure as the `wasi:http` error the caller sees.
pub fn write_error(error: &std::io::Error) -> ErrorCode {
    match error.kind() {
        std::io::ErrorKind::TimedOut => ErrorCode::ConnectionWriteTimeout,
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
            ErrorCode::ConnectionTerminated
        }
        _ => ErrorCode::InternalError(Some(error.to_string())),
    }
}

/// Connect failure as the `wasi:http` error the caller sees.
pub fn connect_error(error: &std::io::Error) -> ErrorCode {
    match error.kind() {
        std::io::ErrorKind::TimedOut => ErrorCode::ConnectionTimeout,
        std::io::ErrorKind::NotFound => ErrorCode::DnsError(helios_api::http::DnsErrorPayload {
            rcode: None,
            info_code: None,
        }),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
            ErrorCode::ConnectionRefused
        }
        _ => ErrorCode::InternalError(Some(error.to_string())),
    }
}
