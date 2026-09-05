//! Reading the origin's response off the connection.
//!
//! The head is parsed with `httparse`; the body is decoded by whichever framing
//! the head announced and streamed straight into the `wasi:http` response body
//! so the caller sees bytes as they arrive rather than after the whole body has
//! been buffered.

use std::string::{String, ToString};
use std::vec::Vec;

use helios_api::http::types::Method;
use helios_api::http::{ErrorCode, Fields};
use helios_api::wit_bindgen::rt::async_support::{FutureWriter, StreamWriter};

use crate::fields::{Section, build_fields, connection_options, is_hop_by_hop, is_token};
use crate::socket::Socket;

/// Largest response head this client will buffer before giving up.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Largest number of header fields `httparse` is given room for.
const MAX_HEADER_FIELDS: usize = 128;

/// Largest trailer section this client will accept.
const MAX_TRAILER_SECTION_BYTES: usize = 64 * 1024;

/// Largest single chunk-size or trailer line.
const MAX_LINE_BYTES: usize = 16 * 1024;

/// Status code of the only interim response that is not simply skipped.
const SWITCHING_PROTOCOLS: u16 = 101;

/// A head as it comes off the wire: how many bytes it occupied, its status,
/// and its fields.
type ParsedHead = (usize, u16, Vec<(String, Vec<u8>)>);

/// A parsed final response head.
pub struct Head {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
}

/// How the response body is delimited on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Framing {
    /// The message has no body at all, whatever the fields say.
    Empty,
    /// Exactly this many bytes.
    Length(u64),
    /// Chunked transfer coding, possibly followed by trailers.
    Chunked,
    /// Everything until the peer closes the connection.
    UntilClose,
}

/// Read one final response head, skipping any interim `1xx` responses.
pub async fn read_head(socket: &mut Socket) -> Result<Head, ErrorCode> {
    loop {
        let Some((length, status, headers)) = parse_head(socket.buffered())? else {
            if socket.buffered().len() > MAX_HEAD_BYTES {
                return Err(ErrorCode::HttpResponseHeaderSectionSize(Some(
                    MAX_HEAD_BYTES as u32,
                )));
            }
            if !socket.fill().await? {
                return Err(ErrorCode::HttpResponseIncomplete);
            }
            continue;
        };

        socket.consume(length);
        if status == SWITCHING_PROTOCOLS {
            // Nothing in this client asks for an upgrade, so a switch leaves
            // the connection speaking a protocol we cannot read.
            return Err(ErrorCode::HttpUpgradeFailed);
        }
        if status < 200 {
            // Interim response: it carries no body, so the next head starts
            // right here.
            continue;
        }
        return Ok(Head { status, headers });
    }
}

/// Parse a head out of `buffered`, or report that more bytes are needed.
fn parse_head(buffered: &[u8]) -> Result<Option<ParsedHead>, ErrorCode> {
    let mut storage = [httparse::EMPTY_HEADER; MAX_HEADER_FIELDS];
    let mut parsed = httparse::Response::new(&mut storage);
    match parsed.parse(buffered) {
        Ok(httparse::Status::Complete(length)) => {
            let status = parsed.code.ok_or(ErrorCode::HttpProtocolError)?;
            let headers = parsed
                .headers
                .iter()
                .map(|field| (field.name.to_string(), field.value.to_vec()))
                .collect();
            Ok(Some((length, status, headers)))
        }
        Ok(httparse::Status::Partial) => Ok(None),
        Err(httparse::Error::TooManyHeaders) => Err(ErrorCode::HttpResponseHeaderSectionSize(None)),
        Err(_) => Err(ErrorCode::HttpProtocolError),
    }
}

/// Decide how the body that follows `head` is delimited.
pub fn framing(method: &Method, head: &Head) -> Result<Framing, ErrorCode> {
    // RFC 9110 §6.4.1: these responses never carry content, regardless of what
    // their fields claim.
    let no_content = matches!(method, Method::Head)
        || head.status == 204
        || head.status == 304
        || (matches!(method, Method::Connect) && (200..300).contains(&head.status));
    if no_content {
        return Ok(Framing::Empty);
    }

    if let Some(codings) = transfer_codings(&head.headers)? {
        let Some((last, rest)) = codings.split_last() else {
            return Err(ErrorCode::HttpProtocolError);
        };
        if let Some(unsupported) = rest.iter().find(|coding| *coding != "chunked") {
            return Err(ErrorCode::HttpResponseTransferCoding(Some(
                unsupported.clone(),
            )));
        }
        if last != "chunked" {
            return Err(ErrorCode::HttpResponseTransferCoding(Some(last.clone())));
        }
        return Ok(Framing::Chunked);
    }

    match content_length(&head.headers)? {
        Some(length) => Ok(Framing::Length(length)),
        None => Ok(Framing::UntilClose),
    }
}

/// Transfer codings the head announced, in order, or `None` when it announced
/// none.
fn transfer_codings(headers: &[(String, Vec<u8>)]) -> Result<Option<Vec<String>>, ErrorCode> {
    let mut codings: Option<Vec<String>> = None;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("transfer-encoding") {
            continue;
        }
        let value = core::str::from_utf8(value).map_err(|_| ErrorCode::HttpProtocolError)?;
        let list = codings.get_or_insert_with(Vec::new);
        for coding in value.split(',') {
            let coding = coding.trim();
            if coding.is_empty() {
                return Err(ErrorCode::HttpProtocolError);
            }
            list.push(coding.to_ascii_lowercase());
        }
    }
    Ok(codings)
}

/// The single content length the head announced, if any.
fn content_length(headers: &[(String, Vec<u8>)]) -> Result<Option<u64>, ErrorCode> {
    let mut declared: Option<u64> = None;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("content-length") {
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
    Ok(declared)
}

/// Translate the wire head into a `wasi:http` field section.
///
/// Per-hop fields are dropped: `wasi:http`'s `fields` refuses them, and
/// forwarding them would let this connection's framing leak into a message
/// that is about to travel over kernel channels instead. `content-length` goes
/// with them when the body was chunked, because the decoded body no longer has
/// the length the origin declared.
pub fn response_fields(head: &Head, framing: Framing) -> Result<Fields, ErrorCode> {
    let options = connection_options(&head.headers);
    let entries: Vec<(String, Vec<u8>)> = head
        .headers
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name, &options))
        .filter(|(name, _)| {
            framing != Framing::Chunked || !name.eq_ignore_ascii_case("content-length")
        })
        .cloned()
        .collect();
    build_fields(&entries, Section::Headers)
}

/// Stream the body into the response and then resolve its trailers future.
///
/// Runs after `handle` has returned: the caller only starts reading the body
/// once it holds the response.
pub async fn pump(
    mut socket: Socket,
    framing: Framing,
    mut writer: StreamWriter<u8>,
    trailers_writer: FutureWriter<Result<Option<Fields>, ErrorCode>>,
) {
    let outcome = decode(&mut socket, framing, &mut writer).await;
    // `wasi:http` requires the content stream to be closed before the trailers
    // future resolves.
    drop(writer);
    let _ = trailers_writer.write(outcome).await;
}

async fn decode(
    socket: &mut Socket,
    framing: Framing,
    writer: &mut StreamWriter<u8>,
) -> Result<Option<Fields>, ErrorCode> {
    match framing {
        Framing::Empty => Ok(None),
        Framing::Length(length) => {
            copy_exact(socket, length, writer).await?;
            Ok(None)
        }
        Framing::UntilClose => {
            copy_until_close(socket, writer).await?;
            Ok(None)
        }
        Framing::Chunked => decode_chunked(socket, writer).await,
    }
}

/// Move exactly `length` body bytes into the response stream.
async fn copy_exact(
    socket: &mut Socket,
    length: u64,
    writer: &mut StreamWriter<u8>,
) -> Result<(), ErrorCode> {
    let mut remaining = length;
    while remaining > 0 {
        if socket.buffered().is_empty() && !socket.fill().await? {
            return Err(ErrorCode::HttpResponseIncomplete);
        }
        let available = socket.buffered().len() as u64;
        let take = remaining.min(available) as usize;
        let chunk = socket.buffered()[..take].to_vec();
        socket.consume(take);
        remaining -= take as u64;
        if !write_chunk(writer, chunk).await {
            return Ok(());
        }
    }
    Ok(())
}

/// Move everything the peer sends until it closes the connection.
async fn copy_until_close(
    socket: &mut Socket,
    writer: &mut StreamWriter<u8>,
) -> Result<(), ErrorCode> {
    loop {
        if socket.buffered().is_empty() && !socket.fill().await? {
            return Ok(());
        }
        let chunk = socket.buffered().to_vec();
        socket.consume(chunk.len());
        if !write_chunk(writer, chunk).await {
            return Ok(());
        }
    }
}

/// Decode a chunked body and return its trailer section.
async fn decode_chunked(
    socket: &mut Socket,
    writer: &mut StreamWriter<u8>,
) -> Result<Option<Fields>, ErrorCode> {
    loop {
        let line = read_line(socket, MAX_LINE_BYTES).await?;
        let size = parse_chunk_size(&line)?;
        if size == 0 {
            break;
        }
        copy_exact(socket, size, writer).await?;
        if !read_line(socket, MAX_LINE_BYTES).await?.is_empty() {
            return Err(ErrorCode::HttpProtocolError);
        }
    }

    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut section_bytes = 0_usize;
    loop {
        let line = read_line(socket, MAX_LINE_BYTES).await?;
        if line.is_empty() {
            break;
        }
        section_bytes += line.len();
        if section_bytes > MAX_TRAILER_SECTION_BYTES {
            return Err(ErrorCode::HttpResponseTrailerSectionSize(None));
        }
        entries.push(parse_field_line(&line)?);
    }
    if entries.is_empty() {
        return Ok(None);
    }
    let options = connection_options(&entries);
    entries.retain(|(name, _)| !is_hop_by_hop(name, &options));
    Ok(Some(build_fields(&entries, Section::Trailers)?))
}

/// Hand one chunk to the response stream; `false` means the reader is gone.
async fn write_chunk(writer: &mut StreamWriter<u8>, chunk: Vec<u8>) -> bool {
    writer.write_all(chunk).await.is_empty()
}

/// Read one CRLF-terminated line, without its terminator.
async fn read_line(socket: &mut Socket, limit: usize) -> Result<Vec<u8>, ErrorCode> {
    loop {
        let buffered = socket.buffered();
        if let Some(position) = buffered.windows(2).position(|window| window == b"\r\n") {
            let line = buffered[..position].to_vec();
            socket.consume(position + 2);
            return Ok(line);
        }
        if buffered.len() > limit {
            return Err(ErrorCode::HttpProtocolError);
        }
        if !socket.fill().await? {
            return Err(ErrorCode::HttpResponseIncomplete);
        }
    }
}

fn parse_chunk_size(line: &[u8]) -> Result<u64, ErrorCode> {
    let line = core::str::from_utf8(line).map_err(|_| ErrorCode::HttpProtocolError)?;
    // A chunk size may be followed by chunk extensions this client ignores.
    let size = line.split_once(';').map_or(line, |(size, _)| size).trim();
    u64::from_str_radix(size, 16).map_err(|_| ErrorCode::HttpProtocolError)
}

fn parse_field_line(line: &[u8]) -> Result<(String, Vec<u8>), ErrorCode> {
    let separator = line
        .iter()
        .position(|byte| *byte == b':')
        .ok_or(ErrorCode::HttpProtocolError)?;
    let name = core::str::from_utf8(&line[..separator])
        .map_err(|_| ErrorCode::HttpProtocolError)?
        .trim_end();
    if !is_token(name) {
        return Err(ErrorCode::HttpProtocolError);
    }
    let value = trim_optional_whitespace(&line[separator + 1..]);
    Ok((name.to_string(), value.to_vec()))
}

fn trim_optional_whitespace(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t'))
        .map_or(start, |position| position + 1);
    &value[start..end]
}
