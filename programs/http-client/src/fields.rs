//! Field-section helpers shared by the request and response halves.
//!
//! Field values are byte strings, not text: RFC 9110 allows values that are not
//! valid UTF-8 and implementations are expected to pass them through. So the
//! wire form is built by appending bytes rather than by rendering a text
//! template, and parsed values stay `Vec<u8>` all the way into `wasi:http`'s
//! `fields`.

use std::string::String;
use std::vec::Vec;

use helios_api::http::{ErrorCode, Fields, HeaderError};

/// HTTP line terminator.
pub const CRLF: &[u8] = b"\r\n";

/// Connection-management fields that belong to a single hop.
///
/// RFC 9110 §7.6.1 forbids forwarding these, and `wasi:http`'s `fields`
/// rejects them outright, so a response carrying them can only be translated
/// into a `fields` after they are removed. `connection` itself may also name
/// further per-hop fields; see [`connection_options`].
pub const HOP_BY_HOP_FIELD_NAMES: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
    "host",
    "http2-settings",
];

/// Which field section a failure belongs to, so it maps to the right code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Section {
    Headers,
    Trailers,
}

/// Append one `name: value` line.
pub fn push_field(out: &mut Vec<u8>, name: &str, value: &[u8]) {
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b": ");
    out.extend_from_slice(value);
    out.extend_from_slice(CRLF);
}

/// Whether `value` is an RFC 9110 token.
pub fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Field names the peer listed in its `connection` header.
pub fn connection_options(headers: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut options = Vec::new();
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("connection") {
            continue;
        }
        let Ok(value) = core::str::from_utf8(value) else {
            continue;
        };
        for option in value.split(',') {
            let option = option.trim();
            if !option.is_empty() {
                options.push(option.to_ascii_lowercase());
            }
        }
    }
    options
}

/// Whether `name` describes this connection rather than the message.
pub fn is_hop_by_hop(name: &str, connection_options: &[String]) -> bool {
    HOP_BY_HOP_FIELD_NAMES
        .iter()
        .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
        || connection_options
            .iter()
            .any(|option| name.eq_ignore_ascii_case(option))
}

/// Build a `wasi:http` field section out of parsed wire fields.
///
/// `wasi:http` owns the size limits, so a section that is too large is
/// reported with the code for the section it came from rather than truncated.
pub fn build_fields(entries: &[(String, Vec<u8>)], section: Section) -> Result<Fields, ErrorCode> {
    Fields::from_list(entries).map_err(|error| match (error, section) {
        (HeaderError::SizeExceeded, Section::Headers) => {
            ErrorCode::HttpResponseHeaderSectionSize(None)
        }
        (HeaderError::SizeExceeded, Section::Trailers) => {
            ErrorCode::HttpResponseTrailerSectionSize(None)
        }
        (HeaderError::Other(detail), _) => ErrorCode::InternalError(detail),
        (HeaderError::InvalidSyntax | HeaderError::Forbidden | HeaderError::Immutable, _) => {
            ErrorCode::HttpProtocolError
        }
    })
}
