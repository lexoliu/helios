//! Serialising the exchange's request onto the connection.
//!
//! Framing is decided by the caller's own `content-length`: when it is present
//! the body is sent with that exact length and a mismatch is an error, and when
//! it is absent the body is chunked, which is also the only framing that leaves
//! room for the request trailers.

use std::format;
use std::string::String;
use std::vec::Vec;

use helios_api::http::types::Method;
use helios_api::http::{ErrorCode, Fields};
use helios_api::stream_closed;
use helios_api::wit_bindgen::rt::async_support::{FutureReader, StreamReader};

use crate::fields::{CRLF, is_token, push_field};
use crate::socket::Socket;

/// Bytes pulled from the caller's body stream in one step.
const BODY_CHUNK_BYTES: usize = 64 * 1024;

/// Field name that decides the outgoing framing.
const CONTENT_LENGTH: &str = "content-length";

/// The request line and the fields that precede the body.
pub struct Head<'a> {
    pub method: &'a Method,
    pub target: &'a str,
    pub authority: &'a str,
    pub headers: &'a [(String, Vec<u8>)],
}

/// How the request body is delimited on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Framing {
    /// Exactly this many bytes follow the head.
    ContentLength(u64),
    /// Chunked transfer coding, terminated by a zero-length chunk.
    Chunked,
}

/// Pick the framing the caller's headers ask for.
pub fn framing(headers: &[(String, Vec<u8>)]) -> Result<Framing, ErrorCode> {
    let mut declared: Option<u64> = None;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case(CONTENT_LENGTH) {
            continue;
        }
        let length = core::str::from_utf8(value)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .ok_or(ErrorCode::HttpProtocolError)?;
        if declared.is_some_and(|previous| previous != length) {
            return Err(ErrorCode::HttpProtocolError);
        }
        declared = Some(length);
    }
    Ok(declared.map_or(Framing::Chunked, Framing::ContentLength))
}

/// Send the head, the body and the trailers.
pub async fn send(
    socket: &Socket,
    head: &Head<'_>,
    framing: Framing,
    contents: StreamReader<u8>,
    trailers: FutureReader<Result<Option<Fields>, ErrorCode>>,
) -> Result<(), ErrorCode> {
    socket.write_all(&encode_head(head, framing)?).await?;
    let sent = send_body(socket, framing, contents).await?;

    // The trailers future resolves only after the body stream closes, and it
    // is also how the caller reports that its body could not be produced.
    let trailers = trailers.await?;

    match framing {
        Framing::ContentLength(expected) => {
            if sent != expected {
                return Err(ErrorCode::HttpRequestBodySize(Some(sent)));
            }
            // Identity framing has nowhere to put a trailer section.
            if trailers.is_some() {
                return Err(ErrorCode::HttpProtocolError);
            }
            Ok(())
        }
        Framing::Chunked => finish_chunked(socket, trailers).await,
    }
}

/// Render the request line plus the field section.
fn encode_head(head: &Head<'_>, framing: Framing) -> Result<Vec<u8>, ErrorCode> {
    let method = method_token(head.method)?;
    if !is_request_target(head.target) {
        return Err(ErrorCode::HttpRequestUriInvalid);
    }

    let mut out = Vec::new();
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(head.target.as_bytes());
    out.extend_from_slice(b" HTTP/1.1");
    out.extend_from_slice(CRLF);

    // `host`, `connection` and `transfer-encoding` are hop-by-hop, so
    // `wasi:http` refuses to carry them and this connection's owner — us —
    // supplies them. One connection per exchange, so it always closes.
    push_field(&mut out, "host", head.authority.as_bytes());
    push_field(&mut out, "connection", b"close");
    if framing == Framing::Chunked {
        push_field(&mut out, "transfer-encoding", b"chunked");
    }
    for (name, value) in head.headers {
        push_field(&mut out, name, value);
    }
    out.extend_from_slice(CRLF);
    Ok(out)
}

/// Stream the caller's body onto the connection, returning the byte count.
async fn send_body(
    socket: &Socket,
    framing: Framing,
    mut contents: StreamReader<u8>,
) -> Result<u64, ErrorCode> {
    let mut sent = 0_u64;
    loop {
        let (result, chunk) = contents.read(Vec::with_capacity(BODY_CHUNK_BYTES)).await;
        if !chunk.is_empty() {
            sent += chunk.len() as u64;
            if let Framing::ContentLength(expected) = framing
                && sent > expected
            {
                return Err(ErrorCode::HttpRequestBodySize(Some(sent)));
            }
            match framing {
                Framing::ContentLength(_) => socket.write_all(&chunk).await?,
                Framing::Chunked => socket.write_all(&encode_chunk(&chunk)).await?,
            }
        }
        if stream_closed(result) {
            return Ok(sent);
        }
    }
}

/// Wrap one body chunk in its chunked-transfer envelope.
fn encode_chunk(chunk: &[u8]) -> Vec<u8> {
    let size = format!("{:x}", chunk.len());
    let mut out = Vec::with_capacity(size.len() + chunk.len() + 4);
    out.extend_from_slice(size.as_bytes());
    out.extend_from_slice(CRLF);
    out.extend_from_slice(chunk);
    out.extend_from_slice(CRLF);
    out
}

/// Write the terminating zero-length chunk and any trailer section.
async fn finish_chunked(socket: &Socket, trailers: Option<Fields>) -> Result<(), ErrorCode> {
    let mut out = Vec::new();
    out.extend_from_slice(b"0");
    out.extend_from_slice(CRLF);
    if let Some(trailers) = trailers {
        for (name, value) in trailers.copy_all() {
            push_field(&mut out, &name, &value);
        }
    }
    out.extend_from_slice(CRLF);
    socket.write_all(&out).await
}

/// The wire token for a method, rejecting anything that is not a token.
fn method_token(method: &Method) -> Result<&str, ErrorCode> {
    Ok(match method {
        Method::Get => "GET",
        Method::Head => "HEAD",
        Method::Post => "POST",
        Method::Put => "PUT",
        Method::Delete => "DELETE",
        Method::Connect => "CONNECT",
        Method::Options => "OPTIONS",
        Method::Trace => "TRACE",
        Method::Patch => "PATCH",
        Method::Other(other) => {
            if !is_token(other) {
                return Err(ErrorCode::HttpRequestMethodInvalid);
            }
            other.as_str()
        }
    })
}

/// Whether `target` can appear in a request line.
fn is_request_target(target: &str) -> bool {
    !target.is_empty()
        && target
            .bytes()
            .all(|byte| byte > b' ' && byte != 0x7f && byte.is_ascii())
}
