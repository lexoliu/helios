//! Transport-neutral HTTP data model.
//!
//! Nothing here knows about a runtime, a component interface, sockets, or a
//! wire format: the types are the values that cross the boundary between a
//! program that wants to make an HTTP request and whatever component actually
//! speaks HTTP. The runtime adapter translates its own resource handles into
//! these types, and the `http-client` kernel plugin turns them into bytes on
//! a socket.
//!
//! Bodies are streamed, never buffered: [`HttpBody`] carries the kernel byte
//! channel the contents flow through, the future the trailers arrive on, and
//! the channel the consumer reports the transmission result back on. That
//! last channel is what lets a program's `request.new` "result of
//! transmission" future resolve with the verdict of whoever consumed the
//! body, one store away.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use bytes::Bytes;
use futures::channel::oneshot;
use thiserror::Error;

/// Field names a guest may neither set nor observe.
///
/// These are all hop-by-hop or connection-management headers: the component
/// that owns the connection decides them, so letting a guest smuggle one
/// through would let it desynchronise the framing of a connection it does not
/// own.
pub const HTTP_FORBIDDEN_FIELD_NAMES: [&str; 9] = [
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

/// Largest single field value a [`HttpFields`] will accept.
pub const HTTP_MAX_FIELD_VALUE_BYTES: usize = 8 * 1024;

/// Largest total size, names plus values, of one field section.
pub const HTTP_MAX_FIELD_SECTION_BYTES: usize = 64 * 1024;

/// Why a mutation of a [`HttpFields`] was refused.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum HttpHeaderError {
    #[error("field name or value is not syntactically valid")]
    InvalidSyntax,
    #[error("field name is forbidden")]
    Forbidden,
    #[error("fields are immutable")]
    Immutable,
    #[error("field would exceed the implementation size limit")]
    SizeExceeded,
}

/// Why a mutation of [`HttpRequestOptions`] was refused.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum HttpRequestOptionsError {
    #[error("request option is not supported")]
    NotSupported,
    #[error("request options are immutable")]
    Immutable,
}

/// The kind of value that failed a syntax check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpSyntaxKind {
    Method,
    Scheme,
    Authority,
    PathWithQuery,
    StatusCode,
}

impl HttpSyntaxKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Method => "method",
            Self::Scheme => "scheme",
            Self::Authority => "authority",
            Self::PathWithQuery => "path-with-query",
            Self::StatusCode => "status code",
        }
    }
}

/// A value handed to a setter was not syntactically valid.
///
/// The corresponding WIT setters return a bare `result`, so the kind is only
/// carried for diagnostics.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("value is not a syntactically valid HTTP {}", .kind.as_str())]
pub struct HttpSyntaxError {
    pub kind: HttpSyntaxKind,
}

impl HttpSyntaxError {
    const fn new(kind: HttpSyntaxKind) -> Self {
        Self { kind }
    }
}

/// Returns true when `byte` is an RFC 9110 `tchar`.
const fn is_field_token_byte(byte: u8) -> bool {
    matches!(byte,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*'
        | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
        | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
}

/// Returns true when `value` is a non-empty RFC 9110 token.
fn is_field_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(is_field_token_byte)
}

/// Returns true when `value` is a valid `field-value`.
///
/// Visible ASCII, `obs-text`, and interior spaces/tabs are allowed; leading or
/// trailing whitespace and any control byte (which would let a value inject a
/// header boundary) are not.
fn is_field_value(value: &[u8]) -> bool {
    if matches!(value.first(), Some(b' ' | b'\t')) || matches!(value.last(), Some(b' ' | b'\t')) {
        return false;
    }
    value
        .iter()
        .all(|byte| matches!(byte, b'\t' | 0x20..=0x7e | 0x80..=0xff))
}

/// A validated HTTP field name, stored in the casing it was given.
///
/// Comparisons are ASCII case-insensitive, as HTTP requires, but the original
/// casing survives so a field section serialises the way the guest wrote it.
#[derive(Clone, Debug, Eq)]
pub struct HttpFieldName(String);

impl HttpFieldName {
    /// Validate `name` as an RFC 9110 token.
    pub fn parse(name: &str) -> Result<Self, HttpHeaderError> {
        if !is_field_token(name) {
            return Err(HttpHeaderError::InvalidSyntax);
        }
        Ok(Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// ASCII case-insensitive comparison against a raw name.
    pub fn matches(&self, name: &str) -> bool {
        self.0.eq_ignore_ascii_case(name)
    }

    /// Whether this name is one a guest may not set or observe.
    pub fn is_forbidden(&self) -> bool {
        HTTP_FORBIDDEN_FIELD_NAMES
            .iter()
            .any(|forbidden| self.matches(forbidden))
    }
}

impl PartialEq for HttpFieldName {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

/// An HTTP field section: headers or trailers.
///
/// A section is either mutable (built by a guest through the constructor,
/// `from-list`, or `clone`) or immutable (handed out by the host, for example
/// `request.get-headers`). Every mutating operation on an immutable section
/// fails with [`HttpHeaderError::Immutable`].
#[derive(Clone, Debug)]
pub struct HttpFields {
    entries: Vec<(HttpFieldName, Bytes)>,
    mutable: bool,
}

impl HttpFields {
    /// An empty, mutable field section.
    pub fn new_mutable() -> Self {
        Self {
            entries: Vec::new(),
            mutable: true,
        }
    }

    /// Build a mutable section from name/value pairs, validating each.
    pub fn from_list(entries: Vec<(String, Bytes)>) -> Result<Self, HttpHeaderError> {
        let mut fields = Self::new_mutable();
        for (name, value) in entries {
            fields.append(&name, value)?;
        }
        Ok(fields)
    }

    /// Build an immutable section from already-validated pairs.
    ///
    /// Forbidden names are dropped rather than rejected: this constructor is
    /// how host-originated sections (a response the plugin parsed off the
    /// wire) reach a guest, and a peer is free to send hop-by-hop headers that
    /// the guest must simply never see.
    pub fn new_immutable(entries: Vec<(HttpFieldName, Bytes)>) -> Self {
        let mut entries = entries;
        entries.retain(|(name, _)| !name.is_forbidden());
        Self {
            entries,
            mutable: false,
        }
    }

    /// Whether this section still accepts mutations.
    pub fn is_mutable(&self) -> bool {
        self.mutable
    }

    /// Freeze this section, as happens when it is handed to the host by value.
    pub fn into_immutable(mut self) -> Self {
        self.mutable = false;
        self
    }

    /// A mutable deep copy, the `fields.clone` semantics.
    pub fn clone_mutable(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            mutable: true,
        }
    }

    /// The name/value pairs in transport order.
    pub fn entries(&self) -> &[(HttpFieldName, Bytes)] {
        &self.entries
    }

    /// Every value stored under `name`; empty when the name is absent or
    /// syntactically invalid.
    pub fn get(&self, name: &str) -> Vec<Bytes> {
        self.entries
            .iter()
            .filter(|(entry, _)| entry.matches(name))
            .map(|(_, value)| value.clone())
            .collect()
    }

    /// Whether `name` is present. A syntactically invalid name is absent.
    pub fn has(&self, name: &str) -> bool {
        self.entries.iter().any(|(entry, _)| entry.matches(name))
    }

    /// Replace every value stored under `name`.
    ///
    /// An empty `values` removes the name. The first existing occurrence keeps
    /// its position in the section so the transport order stays stable.
    pub fn set(&mut self, name: &str, values: Vec<Bytes>) -> Result<(), HttpHeaderError> {
        let name = self.check_mutation(name)?;
        for value in &values {
            Self::check_value(value)?;
        }

        let removed: usize = self
            .entries
            .iter()
            .filter(|(entry, _)| entry == &name)
            .map(|(entry, value)| entry.as_str().len() + value.len())
            .sum();
        let added: usize = values
            .iter()
            .map(|value| name.as_str().len() + value.len())
            .sum();
        self.check_section_size(removed, added)?;

        let position = self
            .entries
            .iter()
            .position(|(entry, _)| entry == &name)
            .unwrap_or(self.entries.len());
        self.entries.retain(|(entry, _)| entry != &name);
        let position = position.min(self.entries.len());
        for (offset, value) in values.into_iter().enumerate() {
            self.entries
                .insert(position + offset, (name.clone(), value));
        }
        Ok(())
    }

    /// Remove every value stored under `name`.
    pub fn delete(&mut self, name: &str) -> Result<(), HttpHeaderError> {
        let name = self.check_mutation(name)?;
        self.entries.retain(|(entry, _)| entry != &name);
        Ok(())
    }

    /// Remove every value stored under `name`, returning what was there.
    pub fn get_and_delete(&mut self, name: &str) -> Result<Vec<Bytes>, HttpHeaderError> {
        let name = self.check_mutation(name)?;
        let mut removed = Vec::new();
        self.entries.retain(|(entry, value)| {
            if entry == &name {
                removed.push(value.clone());
                false
            } else {
                true
            }
        });
        Ok(removed)
    }

    /// Add one more value under `name`, keeping any existing values.
    pub fn append(&mut self, name: &str, value: Bytes) -> Result<(), HttpHeaderError> {
        let name = self.check_mutation(name)?;
        Self::check_value(&value)?;
        self.check_section_size(0, name.as_str().len() + value.len())?;
        self.entries.push((name, value));
        Ok(())
    }

    /// Every name/value pair, in transport order and original casing.
    pub fn copy_all(&self) -> Vec<(String, Bytes)> {
        self.entries
            .iter()
            .map(|(name, value)| (name.as_str().to_string(), value.clone()))
            .collect()
    }

    /// Total accounted size of the section.
    pub fn section_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|(name, value)| name.as_str().len() + value.len())
            .sum()
    }

    fn check_mutation(&self, name: &str) -> Result<HttpFieldName, HttpHeaderError> {
        let name = HttpFieldName::parse(name)?;
        if name.is_forbidden() {
            return Err(HttpHeaderError::Forbidden);
        }
        if !self.mutable {
            return Err(HttpHeaderError::Immutable);
        }
        Ok(name)
    }

    fn check_value(value: &Bytes) -> Result<(), HttpHeaderError> {
        if value.len() > HTTP_MAX_FIELD_VALUE_BYTES {
            return Err(HttpHeaderError::SizeExceeded);
        }
        if !is_field_value(value) {
            return Err(HttpHeaderError::InvalidSyntax);
        }
        Ok(())
    }

    fn check_section_size(&self, removed: usize, added: usize) -> Result<(), HttpHeaderError> {
        let next = self
            .section_bytes()
            .saturating_sub(removed)
            .saturating_add(added);
        if next > HTTP_MAX_FIELD_SECTION_BYTES {
            return Err(HttpHeaderError::SizeExceeded);
        }
        Ok(())
    }
}

/// An HTTP request method.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Head,
    Post,
    Put,
    Delete,
    Connect,
    Options,
    Trace,
    Patch,
    Other(String),
}

impl HttpMethod {
    /// Build a method from a free-form token, folding the nine standard names
    /// back onto their own variants so `get-method` reports them as such.
    pub fn other(value: String) -> Result<Self, HttpSyntaxError> {
        if !is_field_token(&value) {
            return Err(HttpSyntaxError::new(HttpSyntaxKind::Method));
        }
        Ok(match value.as_str() {
            "GET" => Self::Get,
            "HEAD" => Self::Head,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "DELETE" => Self::Delete,
            "CONNECT" => Self::Connect,
            "OPTIONS" => Self::Options,
            "TRACE" => Self::Trace,
            "PATCH" => Self::Patch,
            _ => Self::Other(value),
        })
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Connect => "CONNECT",
            Self::Options => "OPTIONS",
            Self::Trace => "TRACE",
            Self::Patch => "PATCH",
            Self::Other(value) => value,
        }
    }
}

/// The URI scheme a request targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpScheme {
    Http,
    Https,
    Other(String),
}

impl HttpScheme {
    /// Build a scheme from a free-form value, folding `http`/`https` back onto
    /// their own variants regardless of casing.
    pub fn other(value: String) -> Result<Self, HttpSyntaxError> {
        if !is_uri_scheme(&value) {
            return Err(HttpSyntaxError::new(HttpSyntaxKind::Scheme));
        }
        if value.eq_ignore_ascii_case("http") {
            return Ok(Self::Http);
        }
        if value.eq_ignore_ascii_case("https") {
            return Ok(Self::Https);
        }
        Ok(Self::Other(value))
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Other(value) => value,
        }
    }
}

/// RFC 3986 `scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`.
fn is_uri_scheme(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

/// Validate an RFC 3986 authority (`host` with an optional `:port`).
///
/// A colon inside an IPv6 literal such as `[::1]` belongs to the host, so the
/// port is only looked for after any closing bracket.
pub fn validate_http_authority(value: &str) -> Result<(), HttpSyntaxError> {
    let error = HttpSyntaxError::new(HttpSyntaxKind::Authority);
    if value.is_empty() {
        return Err(error);
    }
    let port_start = match value.rfind(']') {
        Some(bracket) => value[bracket..].find(':').map(|offset| bracket + offset),
        None => value.find(':'),
    };
    let (host, port) = match port_start {
        Some(index) => (&value[..index], Some(&value[index + 1..])),
        None => (value, None),
    };
    if host.is_empty() {
        return Err(error);
    }
    let host_ok = if let Some(literal) = host.strip_prefix('[') {
        literal
            .strip_suffix(']')
            .is_some_and(|inner| !inner.is_empty() && inner.bytes().all(is_ipv6_literal_byte))
    } else {
        host.bytes().all(is_reg_name_byte)
    };
    if !host_ok {
        return Err(error);
    }
    let Some(port) = port else {
        return Ok(());
    };
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(error);
    }
    port.parse::<u16>().map(|_| ()).map_err(|_| error)
}

const fn is_ipv6_literal_byte(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' | b':' | b'.' | b'%')
}

const fn is_reg_name_byte(byte: u8) -> bool {
    matches!(byte,
        b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z'
        | b'-' | b'.' | b'_' | b'~' | b'%' | b'!' | b'$' | b'&' | b'\''
        | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=')
}

/// Validate the combined path and query of a request target.
pub fn validate_http_path_with_query(value: &str) -> Result<(), HttpSyntaxError> {
    let error = HttpSyntaxError::new(HttpSyntaxKind::PathWithQuery);
    if value.is_empty() {
        return Ok(());
    }
    if value != "*" && !value.starts_with('/') {
        return Err(error);
    }
    // A fragment never travels on the wire, and anything outside visible ASCII
    // would either split the request line or need percent-encoding first.
    if value
        .bytes()
        .any(|byte| byte == b'#' || !(0x21..=0x7e).contains(&byte))
    {
        return Err(error);
    }
    Ok(())
}

/// Validate an HTTP status code.
pub fn validate_http_status_code(status: u16) -> Result<(), HttpSyntaxError> {
    if (100..=599).contains(&status) {
        Ok(())
    } else {
        Err(HttpSyntaxError::new(HttpSyntaxKind::StatusCode))
    }
}

/// Transport-layer timeouts for one request.
///
/// Like [`HttpFields`], a value is either mutable (the guest built it) or
/// immutable (the host handed it out through `request.get-options`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpRequestOptions {
    connect_timeout_nanos: Option<u64>,
    first_byte_timeout_nanos: Option<u64>,
    between_bytes_timeout_nanos: Option<u64>,
    mutable: bool,
}

impl HttpRequestOptions {
    /// Default options with every timeout unset, mutable.
    pub const fn new_mutable() -> Self {
        Self {
            connect_timeout_nanos: None,
            first_byte_timeout_nanos: None,
            between_bytes_timeout_nanos: None,
            mutable: true,
        }
    }

    pub const fn connect_timeout_nanos(&self) -> Option<u64> {
        self.connect_timeout_nanos
    }

    pub const fn first_byte_timeout_nanos(&self) -> Option<u64> {
        self.first_byte_timeout_nanos
    }

    pub const fn between_bytes_timeout_nanos(&self) -> Option<u64> {
        self.between_bytes_timeout_nanos
    }

    pub const fn is_mutable(&self) -> bool {
        self.mutable
    }

    /// A mutable copy, the `request-options.clone` semantics.
    pub fn clone_mutable(&self) -> Self {
        let mut copy = *self;
        copy.mutable = true;
        copy
    }

    /// An immutable copy, as `request.get-options` hands out.
    pub fn immutable_copy(&self) -> Self {
        let mut copy = *self;
        copy.mutable = false;
        copy
    }

    pub fn set_connect_timeout_nanos(
        &mut self,
        nanos: Option<u64>,
    ) -> Result<(), HttpRequestOptionsError> {
        if !self.mutable {
            return Err(HttpRequestOptionsError::Immutable);
        }
        self.connect_timeout_nanos = nanos;
        Ok(())
    }

    pub fn set_first_byte_timeout_nanos(
        &mut self,
        nanos: Option<u64>,
    ) -> Result<(), HttpRequestOptionsError> {
        if !self.mutable {
            return Err(HttpRequestOptionsError::Immutable);
        }
        self.first_byte_timeout_nanos = nanos;
        Ok(())
    }

    pub fn set_between_bytes_timeout_nanos(
        &mut self,
        nanos: Option<u64>,
    ) -> Result<(), HttpRequestOptionsError> {
        if !self.mutable {
            return Err(HttpRequestOptionsError::Immutable);
        }
        self.between_bytes_timeout_nanos = nanos;
        Ok(())
    }
}

impl Default for HttpRequestOptions {
    fn default() -> Self {
        Self::new_mutable()
    }
}

/// Everything about a request except its body.
#[derive(Clone, Debug)]
pub struct HttpRequestHead {
    pub method: HttpMethod,
    pub scheme: Option<HttpScheme>,
    pub authority: Option<String>,
    pub path_with_query: Option<String>,
    pub headers: HttpFields,
    pub options: Option<HttpRequestOptions>,
}

impl HttpRequestHead {
    /// The `request.new` defaults: `GET`, no target, the supplied headers.
    pub fn new(headers: HttpFields, options: Option<HttpRequestOptions>) -> Self {
        Self {
            method: HttpMethod::Get,
            scheme: None,
            authority: None,
            path_with_query: None,
            headers,
            options,
        }
    }
}

/// Everything about a response except its body.
#[derive(Clone, Debug)]
pub struct HttpResponseHead {
    pub status: u16,
    pub headers: HttpFields,
}

impl HttpResponseHead {
    /// The `response.new` default status of 200.
    pub const DEFAULT_STATUS: u16 = 200;

    pub fn new(headers: HttpFields) -> Self {
        Self {
            status: Self::DEFAULT_STATUS,
            headers,
        }
    }
}

/// Payload of [`HttpErrorCode::DnsError`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpDnsErrorPayload {
    pub rcode: Option<String>,
    pub info_code: Option<u16>,
}

/// Payload of [`HttpErrorCode::TlsAlertReceived`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpTlsAlertReceivedPayload {
    pub alert_id: Option<u8>,
    pub alert_message: Option<String>,
}

/// Payload of the per-field size error codes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpFieldSizePayload {
    pub field_name: Option<String>,
    pub field_size: Option<u32>,
}

/// Every failure an HTTP exchange can report.
///
/// One variant per case of the HTTP error vocabulary the runtime adapter
/// exposes, so a code survives the trip across a store boundary intact.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum HttpErrorCode {
    #[error("DNS lookup timed out")]
    DnsTimeout,
    #[error("DNS lookup failed")]
    DnsError(HttpDnsErrorPayload),
    #[error("destination was not found")]
    DestinationNotFound,
    #[error("destination is unavailable")]
    DestinationUnavailable,
    #[error("destination IP address is prohibited")]
    DestinationIpProhibited,
    #[error("destination IP address is unroutable")]
    DestinationIpUnroutable,
    #[error("connection was refused")]
    ConnectionRefused,
    #[error("connection was terminated")]
    ConnectionTerminated,
    #[error("connection timed out")]
    ConnectionTimeout,
    #[error("connection read timed out")]
    ConnectionReadTimeout,
    #[error("connection write timed out")]
    ConnectionWriteTimeout,
    #[error("connection limit was reached")]
    ConnectionLimitReached,
    #[error("TLS protocol error")]
    TlsProtocolError,
    #[error("TLS certificate error")]
    TlsCertificateError,
    #[error("TLS alert received")]
    TlsAlertReceived(HttpTlsAlertReceivedPayload),
    #[error("HTTP request was denied")]
    HttpRequestDenied,
    #[error("HTTP request requires a content length")]
    HttpRequestLengthRequired,
    #[error("HTTP request body size mismatch")]
    HttpRequestBodySize(Option<u64>),
    #[error("HTTP request method is invalid")]
    HttpRequestMethodInvalid,
    #[error("HTTP request URI is invalid")]
    HttpRequestUriInvalid,
    #[error("HTTP request URI is too long")]
    HttpRequestUriTooLong,
    #[error("HTTP request header section is too large")]
    HttpRequestHeaderSectionSize(Option<u32>),
    #[error("HTTP request header is too large")]
    HttpRequestHeaderSize(Option<HttpFieldSizePayload>),
    #[error("HTTP request trailer section is too large")]
    HttpRequestTrailerSectionSize(Option<u32>),
    #[error("HTTP request trailer is too large")]
    HttpRequestTrailerSize(HttpFieldSizePayload),
    #[error("HTTP response is incomplete")]
    HttpResponseIncomplete,
    #[error("HTTP response header section is too large")]
    HttpResponseHeaderSectionSize(Option<u32>),
    #[error("HTTP response header is too large")]
    HttpResponseHeaderSize(HttpFieldSizePayload),
    #[error("HTTP response body size mismatch")]
    HttpResponseBodySize(Option<u64>),
    #[error("HTTP response trailer section is too large")]
    HttpResponseTrailerSectionSize(Option<u32>),
    #[error("HTTP response trailer is too large")]
    HttpResponseTrailerSize(HttpFieldSizePayload),
    #[error("HTTP response transfer coding is unsupported")]
    HttpResponseTransferCoding(Option<String>),
    #[error("HTTP response content coding is unsupported")]
    HttpResponseContentCoding(Option<String>),
    #[error("HTTP response timed out")]
    HttpResponseTimeout,
    #[error("HTTP upgrade failed")]
    HttpUpgradeFailed,
    #[error("HTTP protocol error")]
    HttpProtocolError,
    #[error("request loop detected")]
    LoopDetected,
    #[error("HTTP configuration error")]
    ConfigurationError,
    #[error("internal HTTP error")]
    InternalError(Option<String>),
}

/// The streamed half of a request or response.
///
/// `contents` carries the body bytes, `trailers` resolves once (with the
/// trailer section, or with nothing when there is none), and `result` is how
/// the consumer tells the producer how transmission went. Dropping `result`
/// without sending is the "nobody is listening" case and resolves the
/// producer's future as a plain success.
pub struct HttpBody {
    pub contents: crate::ByteReader,
    pub trailers: oneshot::Receiver<Result<Option<HttpFields>, HttpErrorCode>>,
    pub result: oneshot::Sender<Result<(), HttpErrorCode>>,
}

/// A complete response travelling back to the caller.
pub struct HttpResponse {
    pub head: HttpResponseHead,
    pub body: HttpBody,
}

/// One request in flight, plus the channel its response comes back on.
pub struct HttpExchange {
    pub head: HttpRequestHead,
    pub body: HttpBody,
    pub response: oneshot::Sender<Result<HttpResponse, HttpErrorCode>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn value(bytes: &str) -> Bytes {
        Bytes::copy_from_slice(bytes.as_bytes())
    }

    #[test]
    fn append_rejects_invalid_names_and_values() {
        let mut fields = HttpFields::new_mutable();
        assert_eq!(
            fields.append("bad name", value("v")),
            Err(HttpHeaderError::InvalidSyntax)
        );
        assert_eq!(
            fields.append("", value("v")),
            Err(HttpHeaderError::InvalidSyntax)
        );
        assert_eq!(
            fields.append("x-test", value(" leading")),
            Err(HttpHeaderError::InvalidSyntax)
        );
        assert_eq!(
            fields.append("x-test", Bytes::from_static(b"a\r\nb")),
            Err(HttpHeaderError::InvalidSyntax)
        );
        assert_eq!(fields.append("x-test", value("ok")), Ok(()));
    }

    #[test]
    fn forbidden_names_are_rejected_on_every_mutation() {
        let mut fields = HttpFields::new_mutable();
        for name in HTTP_FORBIDDEN_FIELD_NAMES {
            assert_eq!(
                fields.append(name, value("v")),
                Err(HttpHeaderError::Forbidden),
                "{name} must be forbidden to append"
            );
            assert_eq!(
                fields.set(name, vec![value("v")]),
                Err(HttpHeaderError::Forbidden)
            );
            assert_eq!(fields.delete(name), Err(HttpHeaderError::Forbidden));
        }
        // The check is case-insensitive, like every other field-name compare.
        assert_eq!(
            fields.append("Transfer-Encoding", value("chunked")),
            Err(HttpHeaderError::Forbidden)
        );
    }

    #[test]
    fn immutable_fields_reject_mutations_but_still_read() {
        let entries = vec![(HttpFieldName::parse("x-test").unwrap(), value("v"))];
        let mut fields = HttpFields::new_immutable(entries);
        assert_eq!(fields.get("X-Test"), vec![value("v")]);
        assert!(fields.has("x-test"));
        assert_eq!(
            fields.append("x-other", value("v")),
            Err(HttpHeaderError::Immutable)
        );
        assert_eq!(
            fields.set("x-other", vec![value("v")]),
            Err(HttpHeaderError::Immutable)
        );
        assert_eq!(fields.delete("x-test"), Err(HttpHeaderError::Immutable));
        assert_eq!(
            fields.get_and_delete("x-test"),
            Err(HttpHeaderError::Immutable)
        );
        assert!(fields.clone_mutable().is_mutable());
    }

    #[test]
    fn immutable_construction_strips_forbidden_names() {
        let entries = vec![
            (HttpFieldName::parse("Connection").unwrap(), value("close")),
            (HttpFieldName::parse("x-keep").unwrap(), value("v")),
        ];
        let fields = HttpFields::new_immutable(entries);
        assert!(!fields.has("connection"));
        assert!(fields.has("x-keep"));
    }

    #[test]
    fn size_limits_are_enforced() {
        let mut fields = HttpFields::new_mutable();
        let oversized = Bytes::from(vec![b'a'; HTTP_MAX_FIELD_VALUE_BYTES + 1]);
        assert_eq!(
            fields.append("x-test", oversized),
            Err(HttpHeaderError::SizeExceeded)
        );

        let chunk = Bytes::from(vec![b'a'; HTTP_MAX_FIELD_VALUE_BYTES]);
        let mut appended = 0;
        loop {
            match fields.append("x-test", chunk.clone()) {
                Ok(()) => appended += 1,
                Err(HttpHeaderError::SizeExceeded) => break,
                Err(error) => panic!("unexpected error: {error}"),
            }
            assert!(appended < 64, "section limit never tripped");
        }
        assert!(fields.section_bytes() <= HTTP_MAX_FIELD_SECTION_BYTES);
    }

    #[test]
    fn set_replaces_values_in_place_and_empty_removes() {
        let mut fields = HttpFields::new_mutable();
        fields.append("a", value("1")).unwrap();
        fields.append("b", value("2")).unwrap();
        fields.append("a", value("3")).unwrap();

        fields.set("A", vec![value("9"), value("10")]).unwrap();
        assert_eq!(
            fields.copy_all(),
            vec![
                ("A".to_string(), value("9")),
                ("A".to_string(), value("10")),
                ("b".to_string(), value("2")),
            ]
        );

        fields.set("a", Vec::new()).unwrap();
        assert!(!fields.has("a"));
        assert_eq!(fields.get_and_delete("b").unwrap(), vec![value("2")]);
        assert!(fields.entries().is_empty());
    }

    #[test]
    fn from_list_validates_every_entry() {
        assert!(
            HttpFields::from_list(vec![("x-a".to_string(), value("1"))])
                .unwrap()
                .has("x-a")
        );
        assert_eq!(
            HttpFields::from_list(vec![("host".to_string(), value("h"))]).unwrap_err(),
            HttpHeaderError::Forbidden
        );
    }

    #[test]
    fn methods_and_schemes_fold_onto_known_variants() {
        assert_eq!(
            HttpMethod::other("GET".to_string()).unwrap(),
            HttpMethod::Get
        );
        assert_eq!(
            HttpMethod::other("QUERY".to_string()).unwrap(),
            HttpMethod::Other("QUERY".to_string())
        );
        assert!(HttpMethod::other("bad method".to_string()).is_err());

        assert_eq!(
            HttpScheme::other("HTTPS".to_string()).unwrap(),
            HttpScheme::Https
        );
        assert_eq!(
            HttpScheme::other("ftp".to_string()).unwrap(),
            HttpScheme::Other("ftp".to_string())
        );
        assert!(HttpScheme::other("1nvalid".to_string()).is_err());
    }

    #[test]
    fn authority_accepts_ipv6_and_validates_ports() {
        for good in [
            "helios.dev",
            "helios.dev:443",
            "127.0.0.1:80",
            "[::1]",
            "[2001:db8::1]",
            "[::1]:443",
        ] {
            assert!(
                validate_http_authority(good).is_ok(),
                "{good} must be accepted"
            );
        }
        for bad in [
            "",
            "helios.dev:",
            "helios.dev:abc",
            "helios.dev:99999",
            "[::1",
        ] {
            assert!(
                validate_http_authority(bad).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn path_with_query_rejects_fragments_and_control_bytes() {
        assert!(validate_http_path_with_query("/a/b?c=d").is_ok());
        assert!(validate_http_path_with_query("*").is_ok());
        assert!(validate_http_path_with_query("").is_ok());
        assert!(validate_http_path_with_query("a/b").is_err());
        assert!(validate_http_path_with_query("/a#frag").is_err());
        assert!(validate_http_path_with_query("/a b").is_err());
    }

    #[test]
    fn request_options_reject_mutation_when_immutable() {
        let mut options = HttpRequestOptions::new_mutable();
        assert_eq!(options.set_connect_timeout_nanos(Some(5)), Ok(()));
        assert_eq!(options.connect_timeout_nanos(), Some(5));

        let mut frozen = options.immutable_copy();
        assert_eq!(
            frozen.set_first_byte_timeout_nanos(Some(1)),
            Err(HttpRequestOptionsError::Immutable)
        );
        assert_eq!(frozen.connect_timeout_nanos(), Some(5));
        assert!(frozen.clone_mutable().is_mutable());
    }

    #[test]
    fn status_codes_outside_the_http_range_are_rejected() {
        assert!(validate_http_status_code(200).is_ok());
        assert!(validate_http_status_code(599).is_ok());
        assert!(validate_http_status_code(99).is_err());
        assert!(validate_http_status_code(600).is_err());
    }
}
