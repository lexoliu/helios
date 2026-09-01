//! Client half of `wasi:http`: build a [`Request`], [`send`] it, read the
//! [`Response`].
//!
//! Bodies are streams in both directions. An outgoing body is described by the
//! [`RequestBody`] trait so that a caller can hand over bytes it already has,
//! nothing at all, or an arbitrary [`AsyncRead`] whose length is unknown; the
//! trait's `content_length` is what decides whether the request goes out with
//! `content-length` framing or chunked framing. An incoming body is an
//! [`AsyncRead`] plus the trailers future that reports whether the transfer
//! actually completed — the stream reaching EOF on its own does not.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::boxed::Box;
use std::io;
use std::string::{String, ToString};
use std::time::Duration;
use std::vec::Vec;

use futures_io::AsyncRead;
use futures_lite::future::{poll_fn, zip};
use http::Uri;
use thiserror::Error;

use super::types;
use super::{ErrorCode, Fields, HeaderError, Method, RequestOptions, RequestOptionsError, Scheme};
use crate::bindings::wasi::http::client;
use crate::bindings::{wit_future, wit_stream};
use crate::stream_closed;
use crate::wit_bindgen::rt::async_support::{
    FutureReader, FutureWriter, StreamReader, StreamResult, StreamWriter,
};

/// Bytes moved between the caller's reader and the component-model stream in
/// one step.
const BODY_CHUNK_BYTES: usize = 64 * 1024;

/// Field name used to announce a body whose length is known up front.
const CONTENT_LENGTH: &str = "content-length";

/// A URL that cannot be turned into a `wasi:http` request target.
#[derive(Debug, Error)]
pub enum UrlError {
    /// The string is not a URI at all.
    #[error("invalid URL")]
    Invalid(#[from] http::uri::InvalidUri),
    /// A `wasi:http` request needs a scheme to pick a transport.
    #[error("URL is missing a scheme")]
    MissingScheme,
    /// The `http` and `https` schemes always require an authority.
    #[error("URL is missing an authority")]
    MissingAuthority,
}

/// Source of an outgoing request body.
///
/// `content_length` is consulted before the request head is sent: `Some(n)`
/// makes [`send`] announce `content-length: n`, `None` leaves framing to the
/// handler, which uses chunked transfer encoding.
pub trait RequestBody {
    /// Number of bytes [`write`](Self::write) will produce, when that is known
    /// before the body is read.
    fn content_length(&self) -> Option<u64>;

    /// Write the whole body into `writer` and drop it, closing the stream.
    fn write(self, writer: StreamWriter<u8>) -> impl Future<Output = io::Result<()>>;
}

/// The absence of a body: a zero-length content stream.
#[derive(Clone, Copy, Debug, Default)]
pub struct Empty;

/// A body read from an [`AsyncRead`] whose length is not known up front.
#[derive(Clone, Copy, Debug)]
pub struct Streaming<R>(pub R);

impl RequestBody for Empty {
    fn content_length(&self) -> Option<u64> {
        Some(0)
    }

    async fn write(self, writer: StreamWriter<u8>) -> io::Result<()> {
        drop(writer);
        Ok(())
    }
}

impl RequestBody for Vec<u8> {
    fn content_length(&self) -> Option<u64> {
        Some(self.len() as u64)
    }

    async fn write(self, mut writer: StreamWriter<u8>) -> io::Result<()> {
        if !self.is_empty() {
            let remaining = writer.write_all(self).await;
            if !remaining.is_empty() {
                return Err(body_stream_dropped());
            }
        }
        drop(writer);
        Ok(())
    }
}

impl<R> RequestBody for Streaming<R>
where
    R: AsyncRead + Unpin,
{
    fn content_length(&self) -> Option<u64> {
        None
    }

    async fn write(self, mut writer: StreamWriter<u8>) -> io::Result<()> {
        let Self(mut reader) = self;
        let mut buffer = vec![0_u8; BODY_CHUNK_BYTES];
        loop {
            let read = poll_fn(|cx| Pin::new(&mut reader).poll_read(cx, &mut buffer)).await?;
            if read == 0 {
                break;
            }
            let remaining = writer.write_all(buffer[..read].to_vec()).await;
            if !remaining.is_empty() {
                return Err(body_stream_dropped());
            }
        }
        drop(writer);
        Ok(())
    }
}

fn body_stream_dropped() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "request body stream was dropped before the body was written",
    )
}

/// An outgoing request: a head that is validated as it is built, plus a body.
///
/// Headers and timeouts are pushed into the `wasi:http` resources as they are
/// supplied, so a rejected field name or an unsupported timeout surfaces at the
/// call that supplied it rather than at [`send`].
pub struct Request<B> {
    method: Method,
    scheme: Scheme,
    authority: String,
    path_with_query: String,
    headers: Fields,
    trailers: Option<Fields>,
    options: Option<RequestOptions>,
    body: B,
}

impl Request<Empty> {
    /// Build a request for `url` with an empty body.
    pub fn new(method: Method, url: &str) -> Result<Self, UrlError> {
        let uri: Uri = url.parse()?;
        let scheme = match uri.scheme_str() {
            Some("http") => Scheme::Http,
            Some("https") => Scheme::Https,
            Some(other) => Scheme::Other(other.to_string()),
            None => return Err(UrlError::MissingScheme),
        };
        let authority = uri
            .authority()
            .ok_or(UrlError::MissingAuthority)?
            .to_string();
        let path_with_query = uri
            .path_and_query()
            .map_or_else(|| String::from("/"), ToString::to_string);

        Ok(Self {
            method,
            scheme,
            authority,
            path_with_query,
            headers: Fields::new(),
            trailers: None,
            options: None,
            body: Empty,
        })
    }

    /// Build a `GET` request for `url`.
    pub fn get(url: &str) -> Result<Self, UrlError> {
        Self::new(Method::Get, url)
    }

    /// Build a `HEAD` request for `url`.
    pub fn head(url: &str) -> Result<Self, UrlError> {
        Self::new(Method::Head, url)
    }

    /// Build a `POST` request for `url`.
    pub fn post(url: &str) -> Result<Self, UrlError> {
        Self::new(Method::Post, url)
    }

    /// Build a `PUT` request for `url`.
    pub fn put(url: &str) -> Result<Self, UrlError> {
        Self::new(Method::Put, url)
    }

    /// Build a `DELETE` request for `url`.
    pub fn delete(url: &str) -> Result<Self, UrlError> {
        Self::new(Method::Delete, url)
    }
}

impl<B> Request<B> {
    /// Append one header.
    pub fn header(self, name: &str, value: impl AsRef<[u8]>) -> Result<Self, HeaderError> {
        self.headers.append(name, value.as_ref())?;
        Ok(self)
    }

    /// Append one trailer, sent after the body.
    ///
    /// A request with trailers is always sent with chunked framing, because
    /// `content-length` framing leaves nowhere to put them.
    pub fn trailer(mut self, name: &str, value: impl AsRef<[u8]>) -> Result<Self, HeaderError> {
        let trailers = self.trailers.get_or_insert_with(Fields::new);
        trailers.append(name, value.as_ref())?;
        Ok(self)
    }

    /// Set the timeout for establishing the connection.
    pub fn connect_timeout(mut self, timeout: Duration) -> Result<Self, RequestOptionsError> {
        self.options()
            .set_connect_timeout(Some(duration_nanos(timeout)))?;
        Ok(self)
    }

    /// Set the timeout for receiving the first byte of the response body.
    pub fn first_byte_timeout(mut self, timeout: Duration) -> Result<Self, RequestOptionsError> {
        self.options()
            .set_first_byte_timeout(Some(duration_nanos(timeout)))?;
        Ok(self)
    }

    /// Set the timeout for receiving subsequent chunks of the response body.
    pub fn between_bytes_timeout(mut self, timeout: Duration) -> Result<Self, RequestOptionsError> {
        self.options()
            .set_between_bytes_timeout(Some(duration_nanos(timeout)))?;
        Ok(self)
    }

    /// Replace the body.
    pub fn body<N>(self, body: N) -> Request<N>
    where
        N: RequestBody,
    {
        Request {
            method: self.method,
            scheme: self.scheme,
            authority: self.authority,
            path_with_query: self.path_with_query,
            headers: self.headers,
            trailers: self.trailers,
            options: self.options,
            body,
        }
    }

    /// Send this request and await the response head.
    pub async fn send(self) -> Result<Response, ErrorCode>
    where
        B: RequestBody,
    {
        send(self).await
    }

    /// The request options resource, created on first use.
    fn options(&mut self) -> &RequestOptions {
        self.options.get_or_insert_with(RequestOptions::new)
    }
}

fn duration_nanos(duration: Duration) -> u64 {
    duration
        .as_nanos()
        .try_into()
        .expect("duration does not fit into wasi nanoseconds")
}

/// Send `request` through `wasi:http/client` and await its response head.
///
/// The body is written concurrently with the call: the handler needs the head
/// before it can start the exchange, and the body only reaches it once the
/// stream it was handed starts producing.
pub async fn send<B>(request: Request<B>) -> Result<Response, ErrorCode>
where
    B: RequestBody,
{
    let Request {
        method,
        scheme,
        authority,
        path_with_query,
        headers,
        trailers,
        options,
        body,
    } = request;

    // Trailers and `content-length` are mutually exclusive framings; when the
    // caller supplied trailers the handler must use chunked encoding.
    if trailers.is_none()
        && let Some(length) = body.content_length()
        && !headers.has(CONTENT_LENGTH)
    {
        headers
            .append(CONTENT_LENGTH, length.to_string().as_bytes())
            .map_err(header_error_code)?;
    }

    let (contents_writer, contents) = wit_stream::new::<u8>();
    let (trailers_writer, trailers_reader) =
        wit_future::new::<Result<Option<Fields>, ErrorCode>>(|| Ok(None));

    let (raw, _transmitted) =
        types::Request::new(headers, Some(contents), trailers_reader, options);
    raw.set_method(&method)
        .map_err(|()| ErrorCode::HttpRequestMethodInvalid)?;
    raw.set_scheme(Some(&scheme))
        .map_err(|()| ErrorCode::HttpRequestUriInvalid)?;
    raw.set_authority(Some(&authority))
        .map_err(|()| ErrorCode::HttpRequestUriInvalid)?;
    raw.set_path_with_query(Some(&path_with_query))
        .map_err(|()| ErrorCode::HttpRequestUriInvalid)?;

    let (response, ()) = zip(
        client::send(raw),
        write_request_body(body, contents_writer, trailers_writer, trailers),
    )
    .await;

    Ok(Response::from_raw(response?))
}

/// Feed the body into the stream, then resolve the trailers future.
///
/// The trailers future must not resolve before the content stream closes, so
/// the writer is dropped inside [`RequestBody::write`] before anything is
/// written here.
async fn write_request_body<B>(
    body: B,
    writer: StreamWriter<u8>,
    trailers_writer: FutureWriter<Result<Option<Fields>, ErrorCode>>,
    trailers: Option<Fields>,
) where
    B: RequestBody,
{
    let outcome = match body.write(writer).await {
        Ok(()) => Ok(trailers),
        Err(error) => Err(ErrorCode::InternalError(Some(error.to_string()))),
    };
    let _ = trailers_writer.write(outcome).await;
}

fn header_error_code(error: HeaderError) -> ErrorCode {
    match error {
        HeaderError::SizeExceeded => ErrorCode::HttpRequestHeaderSectionSize(None),
        HeaderError::InvalidSyntax | HeaderError::Forbidden | HeaderError::Immutable => {
            ErrorCode::HttpRequestHeaderSize(None)
        }
        HeaderError::Other(detail) => ErrorCode::InternalError(detail),
    }
}

/// A response head plus the body that is still arriving.
pub struct Response {
    status: types::StatusCode,
    headers: Vec<(String, Vec<u8>)>,
    body: ResponseBody,
}

impl Response {
    fn from_raw(raw: types::Response) -> Self {
        let status = raw.get_status_code();
        let headers = raw.get_headers().copy_all();
        let (result_writer, result_reader) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
        let (contents, trailers) = types::Response::consume_body(raw, result_reader);
        Self {
            status,
            headers,
            body: ResponseBody {
                state: BodyState::Idle(contents),
                buffered: Vec::new(),
                cursor: 0,
                trailers,
                result: Some(result_writer),
            },
        }
    }

    /// HTTP status code.
    pub fn status(&self) -> types::StatusCode {
        self.status
    }

    /// All response headers, in the order the origin sent them.
    pub fn headers(&self) -> &[(String, Vec<u8>)] {
        &self.headers
    }

    /// First value of `name`, compared case-insensitively.
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_slice())
    }

    /// The response body reader.
    pub fn body(&mut self) -> &mut ResponseBody {
        &mut self.body
    }

    /// Take the body, dropping the head.
    pub fn into_body(self) -> ResponseBody {
        self.body
    }
}

/// Chunk read in flight, holding the reader until it resolves.
type ChunkRead = Pin<Box<dyn Future<Output = (StreamReader<u8>, StreamResult, Vec<u8>)>>>;

enum BodyState {
    Idle(StreamReader<u8>),
    Reading(ChunkRead),
    Closed,
}

/// The body of a response.
///
/// Reaching EOF through [`AsyncRead`] means the stream closed, not that the
/// body arrived intact; [`trailers`](Self::trailers) is what reports the
/// verdict, and it also carries any trailer fields.
pub struct ResponseBody {
    state: BodyState,
    buffered: Vec<u8>,
    cursor: usize,
    trailers: FutureReader<Result<Option<Fields>, ErrorCode>>,
    result: Option<FutureWriter<Result<(), ErrorCode>>>,
}

impl ResponseBody {
    /// Await the transfer verdict and any trailers.
    ///
    /// Call this after the reader reports EOF. An error here means the body
    /// that was already read is incomplete.
    pub async fn trailers(self) -> Result<Option<Vec<(String, Vec<u8>)>>, ErrorCode> {
        let Self {
            trailers, result, ..
        } = self;
        // Dropping the writer resolves the handling-result future with `Ok`,
        // which is the right verdict for a body nobody rejected.
        drop(result);
        let fields = trailers.await?;
        Ok(fields.map(|fields| fields.copy_all()))
    }

    /// Report that this response could not be handled, closing the transfer.
    pub async fn reject(mut self, code: ErrorCode) {
        if let Some(result) = self.result.take() {
            let _ = result.write(Err(code)).await;
        }
    }
}

async fn read_chunk(mut reader: StreamReader<u8>) -> (StreamReader<u8>, StreamResult, Vec<u8>) {
    let (result, chunk) = reader.read(Vec::with_capacity(BODY_CHUNK_BYTES)).await;
    (reader, result, chunk)
}

impl AsyncRead for ResponseBody {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = self.get_mut();

        loop {
            if this.cursor < this.buffered.len() {
                let count = (this.buffered.len() - this.cursor).min(buf.len());
                buf[..count].copy_from_slice(&this.buffered[this.cursor..this.cursor + count]);
                this.cursor += count;
                return Poll::Ready(Ok(count));
            }

            match core::mem::replace(&mut this.state, BodyState::Closed) {
                BodyState::Closed => return Poll::Ready(Ok(0)),
                BodyState::Idle(reader) => {
                    this.state = BodyState::Reading(Box::pin(read_chunk(reader)));
                }
                BodyState::Reading(mut pending) => match pending.as_mut().poll(cx) {
                    Poll::Pending => {
                        this.state = BodyState::Reading(pending);
                        return Poll::Pending;
                    }
                    Poll::Ready((reader, result, chunk)) => {
                        this.buffered = chunk;
                        this.cursor = 0;
                        this.state = if stream_closed(result) {
                            BodyState::Closed
                        } else {
                            BodyState::Idle(reader)
                        };
                    }
                },
            }
        }
    }
}
