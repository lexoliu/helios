//! `wasi:http/types` and `wasi:http/client` for program and plugin stores.
//!
//! The kernel owns the *data* half of `wasi:http`: fields, requests,
//! responses, and request options are plain kernel values
//! ([`crate::HttpFields`] and friends) wrapped in resource handles. It owns
//! none of the *protocol* half. `client.send` is a forwarder: it turns the
//! caller's request into a transport-neutral [`HttpExchange`], pushes it
//! through the `http_client` provider slot on
//! [`RuntimeState`](crate::RuntimeState), and awaits the response the
//! `http-client` kernel plugin sends back.
//!
//! Exactly one implementation serves both stores. The program's store sees
//! the *import* side (it constructs a request and calls `client.send`); the
//! plugin's store sees the same `types` implementation from the other
//! direction (the runner hands it a request and reads back a response). That
//! symmetry is why a body is modelled as [`WasiBody`] rather than as two
//! separate types: whichever store a body was built in, the other store sees
//! it as a host body over kernel channels.
//!
//! # Body ownership
//!
//! A guest-constructed body holds the guest's `stream<u8>` and trailers
//! `future` plus the channel that answers the guest's "result of
//! transmission" future. [`WasiBody::into_host`] converts it once, when the
//! body leaves the store: contents are piped into a kernel byte channel,
//! trailers into a oneshot, and the result channel travels along inside
//! [`HttpBody`] so that whoever consumes the body on the far side reports the
//! verdict straight back to the producing guest.

use super::*;

use crate::{
    HttpBody, HttpErrorCode, HttpExchange, HttpFields, HttpMethod, HttpRequestHead,
    HttpRequestOptions, HttpRequestOptionsError, HttpResponse, HttpResponseHead, HttpScheme,
    ProviderError, validate_http_authority, validate_http_path_with_query,
    validate_http_status_code,
};

use super::http_bindings::http::client as http_client;
use super::http_bindings::http::types as http_types;

pub type HttpError = TrappableError<http_types::ErrorCode>;
pub type HttpHeaderTrapError = TrappableError<http_types::HeaderError>;
pub type HttpRequestOptionsTrapError = TrappableError<http_types::RequestOptionsError>;

/// A trailers future as the guest sees it.
type GuestTrailers =
    FutureReader<core::result::Result<Option<Resource<HttpFields>>, http_types::ErrorCode>>;
/// A body-transmission-result future as the guest sees it.
type GuestBodyResult = FutureReader<core::result::Result<(), http_types::ErrorCode>>;

/// Something went wrong reading a value out of a resolved component future.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub(crate) enum HttpFutureTrap {
    #[error("component future resolved without a value")]
    MissingValue,
    #[error("component future consumer was polled after it completed")]
    PolledAfterCompletion,
}

// ---------------------------------------------------------------------------
// Value conversions between the kernel data model and the generated WIT types
// ---------------------------------------------------------------------------

fn wit_dns_error(payload: crate::HttpDnsErrorPayload) -> http_types::DnsErrorPayload {
    http_types::DnsErrorPayload {
        rcode: payload.rcode,
        info_code: payload.info_code,
    }
}

fn kernel_dns_error(payload: http_types::DnsErrorPayload) -> crate::HttpDnsErrorPayload {
    crate::HttpDnsErrorPayload {
        rcode: payload.rcode,
        info_code: payload.info_code,
    }
}

fn wit_tls_alert(
    payload: crate::HttpTlsAlertReceivedPayload,
) -> http_types::TlsAlertReceivedPayload {
    http_types::TlsAlertReceivedPayload {
        alert_id: payload.alert_id,
        alert_message: payload.alert_message,
    }
}

fn kernel_tls_alert(
    payload: http_types::TlsAlertReceivedPayload,
) -> crate::HttpTlsAlertReceivedPayload {
    crate::HttpTlsAlertReceivedPayload {
        alert_id: payload.alert_id,
        alert_message: payload.alert_message,
    }
}

fn wit_field_size(payload: crate::HttpFieldSizePayload) -> http_types::FieldSizePayload {
    http_types::FieldSizePayload {
        field_name: payload.field_name,
        field_size: payload.field_size,
    }
}

fn kernel_field_size(payload: http_types::FieldSizePayload) -> crate::HttpFieldSizePayload {
    crate::HttpFieldSizePayload {
        field_name: payload.field_name,
        field_size: payload.field_size,
    }
}

/// Kernel error code to the code the guest sees.
pub(crate) fn wit_error_code(code: HttpErrorCode) -> http_types::ErrorCode {
    use HttpErrorCode as K;
    use http_types::ErrorCode as W;
    match code {
        K::DnsTimeout => W::DnsTimeout,
        K::DnsError(payload) => W::DnsError(wit_dns_error(payload)),
        K::DestinationNotFound => W::DestinationNotFound,
        K::DestinationUnavailable => W::DestinationUnavailable,
        K::DestinationIpProhibited => W::DestinationIpProhibited,
        K::DestinationIpUnroutable => W::DestinationIpUnroutable,
        K::ConnectionRefused => W::ConnectionRefused,
        K::ConnectionTerminated => W::ConnectionTerminated,
        K::ConnectionTimeout => W::ConnectionTimeout,
        K::ConnectionReadTimeout => W::ConnectionReadTimeout,
        K::ConnectionWriteTimeout => W::ConnectionWriteTimeout,
        K::ConnectionLimitReached => W::ConnectionLimitReached,
        K::TlsProtocolError => W::TlsProtocolError,
        K::TlsCertificateError => W::TlsCertificateError,
        K::TlsAlertReceived(payload) => W::TlsAlertReceived(wit_tls_alert(payload)),
        K::HttpRequestDenied => W::HttpRequestDenied,
        K::HttpRequestLengthRequired => W::HttpRequestLengthRequired,
        K::HttpRequestBodySize(size) => W::HttpRequestBodySize(size),
        K::HttpRequestMethodInvalid => W::HttpRequestMethodInvalid,
        K::HttpRequestUriInvalid => W::HttpRequestUriInvalid,
        K::HttpRequestUriTooLong => W::HttpRequestUriTooLong,
        K::HttpRequestHeaderSectionSize(size) => W::HttpRequestHeaderSectionSize(size),
        K::HttpRequestHeaderSize(payload) => W::HttpRequestHeaderSize(payload.map(wit_field_size)),
        K::HttpRequestTrailerSectionSize(size) => W::HttpRequestTrailerSectionSize(size),
        K::HttpRequestTrailerSize(payload) => W::HttpRequestTrailerSize(wit_field_size(payload)),
        K::HttpResponseIncomplete => W::HttpResponseIncomplete,
        K::HttpResponseHeaderSectionSize(size) => W::HttpResponseHeaderSectionSize(size),
        K::HttpResponseHeaderSize(payload) => W::HttpResponseHeaderSize(wit_field_size(payload)),
        K::HttpResponseBodySize(size) => W::HttpResponseBodySize(size),
        K::HttpResponseTrailerSectionSize(size) => W::HttpResponseTrailerSectionSize(size),
        K::HttpResponseTrailerSize(payload) => W::HttpResponseTrailerSize(wit_field_size(payload)),
        K::HttpResponseTransferCoding(coding) => W::HttpResponseTransferCoding(coding),
        K::HttpResponseContentCoding(coding) => W::HttpResponseContentCoding(coding),
        K::HttpResponseTimeout => W::HttpResponseTimeout,
        K::HttpUpgradeFailed => W::HttpUpgradeFailed,
        K::HttpProtocolError => W::HttpProtocolError,
        K::LoopDetected => W::LoopDetected,
        K::ConfigurationError => W::ConfigurationError,
        K::InternalError(message) => W::InternalError(message),
    }
}

/// The code a guest reported, as a kernel error code.
pub(crate) fn kernel_error_code(code: http_types::ErrorCode) -> HttpErrorCode {
    use HttpErrorCode as K;
    use http_types::ErrorCode as W;
    match code {
        W::DnsTimeout => K::DnsTimeout,
        W::DnsError(payload) => K::DnsError(kernel_dns_error(payload)),
        W::DestinationNotFound => K::DestinationNotFound,
        W::DestinationUnavailable => K::DestinationUnavailable,
        W::DestinationIpProhibited => K::DestinationIpProhibited,
        W::DestinationIpUnroutable => K::DestinationIpUnroutable,
        W::ConnectionRefused => K::ConnectionRefused,
        W::ConnectionTerminated => K::ConnectionTerminated,
        W::ConnectionTimeout => K::ConnectionTimeout,
        W::ConnectionReadTimeout => K::ConnectionReadTimeout,
        W::ConnectionWriteTimeout => K::ConnectionWriteTimeout,
        W::ConnectionLimitReached => K::ConnectionLimitReached,
        W::TlsProtocolError => K::TlsProtocolError,
        W::TlsCertificateError => K::TlsCertificateError,
        W::TlsAlertReceived(payload) => K::TlsAlertReceived(kernel_tls_alert(payload)),
        W::HttpRequestDenied => K::HttpRequestDenied,
        W::HttpRequestLengthRequired => K::HttpRequestLengthRequired,
        W::HttpRequestBodySize(size) => K::HttpRequestBodySize(size),
        W::HttpRequestMethodInvalid => K::HttpRequestMethodInvalid,
        W::HttpRequestUriInvalid => K::HttpRequestUriInvalid,
        W::HttpRequestUriTooLong => K::HttpRequestUriTooLong,
        W::HttpRequestHeaderSectionSize(size) => K::HttpRequestHeaderSectionSize(size),
        W::HttpRequestHeaderSize(payload) => {
            K::HttpRequestHeaderSize(payload.map(kernel_field_size))
        }
        W::HttpRequestTrailerSectionSize(size) => K::HttpRequestTrailerSectionSize(size),
        W::HttpRequestTrailerSize(payload) => K::HttpRequestTrailerSize(kernel_field_size(payload)),
        W::HttpResponseIncomplete => K::HttpResponseIncomplete,
        W::HttpResponseHeaderSectionSize(size) => K::HttpResponseHeaderSectionSize(size),
        W::HttpResponseHeaderSize(payload) => K::HttpResponseHeaderSize(kernel_field_size(payload)),
        W::HttpResponseBodySize(size) => K::HttpResponseBodySize(size),
        W::HttpResponseTrailerSectionSize(size) => K::HttpResponseTrailerSectionSize(size),
        W::HttpResponseTrailerSize(payload) => {
            K::HttpResponseTrailerSize(kernel_field_size(payload))
        }
        W::HttpResponseTransferCoding(coding) => K::HttpResponseTransferCoding(coding),
        W::HttpResponseContentCoding(coding) => K::HttpResponseContentCoding(coding),
        W::HttpResponseTimeout => K::HttpResponseTimeout,
        W::HttpUpgradeFailed => K::HttpUpgradeFailed,
        W::HttpProtocolError => K::HttpProtocolError,
        W::LoopDetected => K::LoopDetected,
        W::ConfigurationError => K::ConfigurationError,
        W::InternalError(message) => K::InternalError(message),
    }
}

fn wit_header_error(error: crate::HttpHeaderError) -> http_types::HeaderError {
    match error {
        crate::HttpHeaderError::InvalidSyntax => http_types::HeaderError::InvalidSyntax,
        crate::HttpHeaderError::Forbidden => http_types::HeaderError::Forbidden,
        crate::HttpHeaderError::Immutable => http_types::HeaderError::Immutable,
        crate::HttpHeaderError::SizeExceeded => http_types::HeaderError::SizeExceeded,
    }
}

fn header_error(error: crate::HttpHeaderError) -> HttpHeaderTrapError {
    wit_header_error(error).into()
}

fn request_options_error(error: HttpRequestOptionsError) -> HttpRequestOptionsTrapError {
    match error {
        HttpRequestOptionsError::NotSupported => http_types::RequestOptionsError::NotSupported,
        HttpRequestOptionsError::Immutable => http_types::RequestOptionsError::Immutable,
    }
    .into()
}

fn wit_method(method: &HttpMethod) -> http_types::Method {
    match method {
        HttpMethod::Get => http_types::Method::Get,
        HttpMethod::Head => http_types::Method::Head,
        HttpMethod::Post => http_types::Method::Post,
        HttpMethod::Put => http_types::Method::Put,
        HttpMethod::Delete => http_types::Method::Delete,
        HttpMethod::Connect => http_types::Method::Connect,
        HttpMethod::Options => http_types::Method::Options,
        HttpMethod::Trace => http_types::Method::Trace,
        HttpMethod::Patch => http_types::Method::Patch,
        HttpMethod::Other(value) => http_types::Method::Other(value.clone()),
    }
}

fn kernel_method(method: http_types::Method) -> core::result::Result<HttpMethod, ()> {
    Ok(match method {
        http_types::Method::Get => HttpMethod::Get,
        http_types::Method::Head => HttpMethod::Head,
        http_types::Method::Post => HttpMethod::Post,
        http_types::Method::Put => HttpMethod::Put,
        http_types::Method::Delete => HttpMethod::Delete,
        http_types::Method::Connect => HttpMethod::Connect,
        http_types::Method::Options => HttpMethod::Options,
        http_types::Method::Trace => HttpMethod::Trace,
        http_types::Method::Patch => HttpMethod::Patch,
        http_types::Method::Other(value) => HttpMethod::other(value).map_err(|_| ())?,
    })
}

fn wit_scheme(scheme: &HttpScheme) -> http_types::Scheme {
    match scheme {
        HttpScheme::Http => http_types::Scheme::Http,
        HttpScheme::Https => http_types::Scheme::Https,
        HttpScheme::Other(value) => http_types::Scheme::Other(value.clone()),
    }
}

fn kernel_scheme(scheme: http_types::Scheme) -> core::result::Result<HttpScheme, ()> {
    Ok(match scheme {
        http_types::Scheme::Http => HttpScheme::Http,
        http_types::Scheme::Https => HttpScheme::Https,
        http_types::Scheme::Other(value) => HttpScheme::other(value).map_err(|_| ())?,
    })
}

// ---------------------------------------------------------------------------
// Body plumbing
// ---------------------------------------------------------------------------

/// Forwards a guest trailers future into a kernel oneshot.
///
/// The guest resolves its trailers future with an owned `trailers` resource;
/// this deletes that resource from the store table and hands the fields to
/// whoever is reading the body on the far side.
pub(crate) struct TrailersConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
    sender: Option<oneshot::Sender<core::result::Result<Option<HttpFields>, HttpErrorCode>>>,
}

impl<T, CpuImpl, HostFs> TrailersConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new(
        getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
        sender: oneshot::Sender<core::result::Result<Option<HttpFields>, HttpErrorCode>>,
    ) -> Self {
        Self {
            getter,
            sender: Some(sender),
        }
    }
}

impl<T: 'static, CpuImpl, HostFs> FutureConsumer<T> for TrailersConsumer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    type Item = core::result::Result<Option<Resource<HttpFields>>, http_types::ErrorCode>;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<()>> {
        let this = self.get_mut();
        let mut item = None;
        source.read(&mut store, &mut item)?;
        let item = item.ok_or_else(|| wasmtime::Error::new(HttpFutureTrap::MissingValue))?;
        let sender = this
            .sender
            .take()
            .ok_or_else(|| wasmtime::Error::new(HttpFutureTrap::PolledAfterCompletion))?;
        let value = match item {
            Ok(Some(resource)) => {
                let fields = (this.getter)(store.data_mut()).table.delete(resource)?;
                Ok(Some(fields.into_immutable()))
            }
            Ok(None) => Ok(None),
            Err(code) => Err(kernel_error_code(code)),
        };
        let _ = sender.send(value);
        Poll::Ready(Ok(()))
    }
}

/// Forwards a guest body-transmission-result future into a kernel oneshot.
pub(crate) struct BodyResultConsumer {
    sender: Option<oneshot::Sender<core::result::Result<(), HttpErrorCode>>>,
}

impl BodyResultConsumer {
    fn new(sender: oneshot::Sender<core::result::Result<(), HttpErrorCode>>) -> Self {
        Self {
            sender: Some(sender),
        }
    }
}

impl<T: 'static> FutureConsumer<T> for BodyResultConsumer {
    type Item = core::result::Result<(), http_types::ErrorCode>;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<()>> {
        let this = self.get_mut();
        let mut item = None;
        source.read(&mut store, &mut item)?;
        let item = item.ok_or_else(|| wasmtime::Error::new(HttpFutureTrap::MissingValue))?;
        let sender = this
            .sender
            .take()
            .ok_or_else(|| wasmtime::Error::new(HttpFutureTrap::PolledAfterCompletion))?;
        let _ = sender.send(item.map_err(kernel_error_code));
        Poll::Ready(Ok(()))
    }
}

/// Produces a guest trailers future from a kernel oneshot.
///
/// Pushing the fields into the store table needs store access, which is why
/// this is a [`FutureProducer`] rather than a plain async block.
pub(crate) struct TrailersProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
    receiver: oneshot::Receiver<core::result::Result<Option<HttpFields>, HttpErrorCode>>,
}

impl<T, CpuImpl, HostFs> TrailersProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new(
        getter: fn(&mut T) -> &mut StoreData<CpuImpl, HostFs>,
        receiver: oneshot::Receiver<core::result::Result<Option<HttpFields>, HttpErrorCode>>,
    ) -> Self {
        Self { getter, receiver }
    }
}

impl<T: 'static, CpuImpl, HostFs> FutureProducer<T> for TrailersProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    type Item = core::result::Result<Option<Resource<HttpFields>>, http_types::ErrorCode>;

    fn poll_produce(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'_, T>,
        finish: bool,
    ) -> Poll<Result<Option<Self::Item>>> {
        let this = self.get_mut();
        let resolved = match Pin::new(&mut this.receiver).poll(cx) {
            Poll::Ready(Ok(value)) => value,
            // The producing side went away without ever sending: that is a
            // body which simply carried no trailers.
            Poll::Ready(Err(oneshot::Canceled)) => Ok(None),
            Poll::Pending if finish => return Poll::Ready(Ok(None)),
            Poll::Pending => return Poll::Pending,
        };
        let item = match resolved {
            Ok(Some(fields)) => {
                let resource = (this.getter)(store.data_mut()).table.push(fields)?;
                Ok(Some(resource))
            }
            Ok(None) => Ok(None),
            Err(code) => Err(wit_error_code(code)),
        };
        Poll::Ready(Ok(Some(item)))
    }
}

/// The body of a request or response, from whichever side built it.
pub(crate) enum WasiBody {
    /// Built by the guest in this store: the guest still owns the stream and
    /// the trailers future, and is waiting on `result`.
    Guest {
        contents: Option<StreamReader<u8>>,
        trailers: GuestTrailers,
        result: oneshot::Sender<core::result::Result<(), HttpErrorCode>>,
    },
    /// Built by the kernel: already sitting on kernel channels, ready to be
    /// streamed into whichever store asks for it.
    Host { body: HttpBody },
}

impl WasiBody {
    /// Wrap the stream and trailers future a guest passed to `request.new` or
    /// `response.new`, returning the future that reports transmission back.
    fn new_guest<T, CpuImpl, HostFs>(
        access: &mut Access<'_, T, HasSelf<StoreData<CpuImpl, HostFs>>>,
        contents: Option<StreamReader<u8>>,
        trailers: GuestTrailers,
    ) -> Result<(Self, GuestBodyResult)>
    where
        T: 'static,
        CpuImpl: Cpu + Clone,
        HostFs: crate::HostFileSystem,
    {
        let (result, result_rx) = oneshot::channel();
        let body = Self::Guest {
            contents,
            trailers,
            result,
        };
        let future = FutureReader::new(access, async move {
            // A dropped sender means nobody ever reported a failure, which is
            // the same as a clean transmission.
            let outcome = result_rx.await.unwrap_or(Ok(()));
            Ok::<_, wasmtime::Error>(outcome.map_err(wit_error_code))
        })?;
        Ok((body, future))
    }

    /// Move this body out of its store and onto kernel channels.
    ///
    /// A guest body is piped into a fresh byte channel and a trailers oneshot;
    /// a host body is already in that form and passes straight through.
    fn into_host<T, CpuImpl, HostFs>(
        self,
        access: &mut Access<'_, T, HasSelf<StoreData<CpuImpl, HostFs>>>,
    ) -> Result<HttpBody>
    where
        T: 'static,
        CpuImpl: Cpu + Clone,
        HostFs: crate::HostFileSystem,
    {
        match self {
            Self::Host { body } => Ok(body),
            Self::Guest {
                contents,
                trailers,
                result,
            } => {
                let (writer, reader) = crate::byte_channel();
                match contents {
                    Some(contents) => {
                        contents.pipe(&mut *access, ChannelStreamConsumer::detached(writer))?;
                    }
                    // No contents at all: dropping the only writer closes the
                    // channel, so the reader sees an immediate end of stream.
                    None => drop(writer),
                }
                let (trailers_tx, trailers_rx) = oneshot::channel();
                let getter = access.getter();
                trailers.pipe(&mut *access, TrailersConsumer::new(getter, trailers_tx))?;
                Ok(HttpBody {
                    contents: reader,
                    trailers: trailers_rx,
                    result,
                })
            }
        }
    }

    /// `consume-body`, shared by requests and responses.
    ///
    /// `fut` is how the caller will report back how the body was handled; it
    /// is wired to whichever result channel this body carries.
    fn consume<T, CpuImpl, HostFs>(
        self,
        access: &mut Access<'_, T, HasSelf<StoreData<CpuImpl, HostFs>>>,
        fut: GuestBodyResult,
    ) -> Result<(StreamReader<u8>, GuestTrailers)>
    where
        T: 'static,
        CpuImpl: Cpu + Clone,
        HostFs: crate::HostFileSystem,
    {
        match self {
            Self::Guest {
                contents,
                trailers,
                result,
            } => {
                let stream = match contents {
                    Some(stream) => stream,
                    None => StreamReader::new(&mut *access, Vec::<u8>::new())?,
                };
                fut.pipe(&mut *access, BodyResultConsumer::new(result))?;
                Ok((stream, trailers))
            }
            Self::Host { body } => {
                let HttpBody {
                    contents,
                    trailers,
                    result,
                } = body;
                fut.pipe(&mut *access, BodyResultConsumer::new(result))?;
                let stream = StreamReader::new(&mut *access, ChannelStreamProducer::new(contents))?;
                let getter = access.getter();
                let future =
                    FutureReader::new(&mut *access, TrailersProducer::new(getter, trailers))?;
                Ok((stream, future))
            }
        }
    }

    /// Release everything this body still owns in the store.
    fn close<T, CpuImpl, HostFs>(
        self,
        access: &mut Access<'_, T, HasSelf<StoreData<CpuImpl, HostFs>>>,
    ) -> Result<()>
    where
        T: 'static,
        CpuImpl: Cpu + Clone,
        HostFs: crate::HostFileSystem,
    {
        if let Self::Guest {
            contents,
            mut trailers,
            ..
        } = self
        {
            if let Some(mut contents) = contents {
                contents.close(&mut *access)?;
            }
            trailers.close(&mut *access)?;
        }
        Ok(())
    }
}

/// The concrete type behind a `wasi:http/types.request` resource.
pub struct WasiRequest {
    pub(crate) head: HttpRequestHead,
    pub(crate) body: WasiBody,
}

/// The concrete type behind a `wasi:http/types.response` resource.
pub struct WasiResponse {
    pub(crate) head: HttpResponseHead,
    pub(crate) body: WasiBody,
}

impl WasiRequest {
    /// Wrap a kernel-side exchange as the request the plugin will read.
    pub(crate) fn from_host(head: HttpRequestHead, body: HttpBody) -> Self {
        Self {
            head,
            body: WasiBody::Host { body },
        }
    }
}

impl WasiResponse {
    /// Wrap a kernel-side response as the response the caller will read.
    pub(crate) fn from_host(head: HttpResponseHead, body: HttpBody) -> Self {
        Self {
            head,
            body: WasiBody::Host { body },
        }
    }

    /// Move a response the plugin just produced out of the plugin's store and
    /// onto kernel channels, ready to travel back to the calling program.
    pub(crate) fn into_host_response<T, CpuImpl, HostFs>(
        self,
        access: &mut Access<'_, T, HasSelf<StoreData<CpuImpl, HostFs>>>,
    ) -> Result<HttpResponse>
    where
        T: 'static,
        CpuImpl: Cpu + Clone,
        HostFs: crate::HostFileSystem,
    {
        let Self { head, body } = self;
        Ok(HttpResponse {
            head,
            body: body.into_host(access)?,
        })
    }
}

// ---------------------------------------------------------------------------
// `wasi:http/types`
// ---------------------------------------------------------------------------

fn header_trap(error: impl Into<wasmtime::Error>) -> HttpHeaderTrapError {
    TrappableError::trap(error)
}

fn options_trap(error: impl Into<wasmtime::Error>) -> HttpRequestOptionsTrapError {
    TrappableError::trap(error)
}

/// The error a caller sees when the plugin that was serving its exchange went
/// away before answering.
fn plugin_unavailable() -> HttpError {
    http_types::ErrorCode::InternalError(Some(String::from("http plugin restarted"))).into()
}

impl<CpuImpl, HostFs> http_types::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn convert_error_code(&mut self, error: HttpError) -> Result<http_types::ErrorCode> {
        error.downcast()
    }

    fn convert_header_error(
        &mut self,
        error: HttpHeaderTrapError,
    ) -> Result<http_types::HeaderError> {
        error.downcast()
    }

    fn convert_request_options_error(
        &mut self,
        error: HttpRequestOptionsTrapError,
    ) -> Result<http_types::RequestOptionsError> {
        error.downcast()
    }
}

impl<CpuImpl, HostFs> http_types::HostFields for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new(&mut self) -> Result<Resource<HttpFields>> {
        Ok(self.table.push(HttpFields::new_mutable())?)
    }

    fn from_list(
        &mut self,
        entries: Vec<(String, Vec<u8>)>,
    ) -> core::result::Result<Resource<HttpFields>, HttpHeaderTrapError> {
        let entries = entries
            .into_iter()
            .map(|(name, value)| (name, Bytes::from(value)))
            .collect();
        let fields = HttpFields::from_list(entries).map_err(header_error)?;
        self.table.push(fields).map_err(header_trap)
    }

    fn get(&mut self, fields: Resource<HttpFields>, name: String) -> Result<Vec<Vec<u8>>> {
        let fields = self.table.get(&fields)?;
        Ok(fields
            .get(&name)
            .into_iter()
            .map(|value| value.to_vec())
            .collect())
    }

    fn has(&mut self, fields: Resource<HttpFields>, name: String) -> Result<bool> {
        Ok(self.table.get(&fields)?.has(&name))
    }

    fn set(
        &mut self,
        fields: Resource<HttpFields>,
        name: String,
        values: Vec<Vec<u8>>,
    ) -> core::result::Result<(), HttpHeaderTrapError> {
        let values = values.into_iter().map(Bytes::from).collect();
        self.table
            .get_mut(&fields)
            .map_err(header_trap)?
            .set(&name, values)
            .map_err(header_error)
    }

    fn delete(
        &mut self,
        fields: Resource<HttpFields>,
        name: String,
    ) -> core::result::Result<(), HttpHeaderTrapError> {
        self.table
            .get_mut(&fields)
            .map_err(header_trap)?
            .delete(&name)
            .map_err(header_error)
    }

    fn get_and_delete(
        &mut self,
        fields: Resource<HttpFields>,
        name: String,
    ) -> core::result::Result<Vec<Vec<u8>>, HttpHeaderTrapError> {
        let removed = self
            .table
            .get_mut(&fields)
            .map_err(header_trap)?
            .get_and_delete(&name)
            .map_err(header_error)?;
        Ok(removed.into_iter().map(|value| value.to_vec()).collect())
    }

    fn append(
        &mut self,
        fields: Resource<HttpFields>,
        name: String,
        value: Vec<u8>,
    ) -> core::result::Result<(), HttpHeaderTrapError> {
        self.table
            .get_mut(&fields)
            .map_err(header_trap)?
            .append(&name, Bytes::from(value))
            .map_err(header_error)
    }

    fn copy_all(&mut self, fields: Resource<HttpFields>) -> Result<Vec<(String, Vec<u8>)>> {
        Ok(self
            .table
            .get(&fields)?
            .copy_all()
            .into_iter()
            .map(|(name, value)| (name, value.to_vec()))
            .collect())
    }

    fn clone(&mut self, fields: Resource<HttpFields>) -> Result<Resource<HttpFields>> {
        let clone = self.table.get(&fields)?.clone_mutable();
        Ok(self.table.push(clone)?)
    }

    fn drop(&mut self, fields: Resource<HttpFields>) -> Result<()> {
        self.table.delete(fields)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> http_types::HostRequestOptions for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new(&mut self) -> Result<Resource<HttpRequestOptions>> {
        Ok(self.table.push(HttpRequestOptions::new_mutable())?)
    }

    fn get_connect_timeout(
        &mut self,
        options: Resource<HttpRequestOptions>,
    ) -> Result<Option<http_types::Duration>> {
        Ok(self.table.get(&options)?.connect_timeout_nanos())
    }

    fn set_connect_timeout(
        &mut self,
        options: Resource<HttpRequestOptions>,
        duration: Option<http_types::Duration>,
    ) -> core::result::Result<(), HttpRequestOptionsTrapError> {
        self.table
            .get_mut(&options)
            .map_err(options_trap)?
            .set_connect_timeout_nanos(duration)
            .map_err(request_options_error)
    }

    fn get_first_byte_timeout(
        &mut self,
        options: Resource<HttpRequestOptions>,
    ) -> Result<Option<http_types::Duration>> {
        Ok(self.table.get(&options)?.first_byte_timeout_nanos())
    }

    fn set_first_byte_timeout(
        &mut self,
        options: Resource<HttpRequestOptions>,
        duration: Option<http_types::Duration>,
    ) -> core::result::Result<(), HttpRequestOptionsTrapError> {
        self.table
            .get_mut(&options)
            .map_err(options_trap)?
            .set_first_byte_timeout_nanos(duration)
            .map_err(request_options_error)
    }

    fn get_between_bytes_timeout(
        &mut self,
        options: Resource<HttpRequestOptions>,
    ) -> Result<Option<http_types::Duration>> {
        Ok(self.table.get(&options)?.between_bytes_timeout_nanos())
    }

    fn set_between_bytes_timeout(
        &mut self,
        options: Resource<HttpRequestOptions>,
        duration: Option<http_types::Duration>,
    ) -> core::result::Result<(), HttpRequestOptionsTrapError> {
        self.table
            .get_mut(&options)
            .map_err(options_trap)?
            .set_between_bytes_timeout_nanos(duration)
            .map_err(request_options_error)
    }

    fn clone(
        &mut self,
        options: Resource<HttpRequestOptions>,
    ) -> Result<Resource<HttpRequestOptions>> {
        let clone = self.table.get(&options)?.clone_mutable();
        Ok(self.table.push(clone)?)
    }

    fn drop(&mut self, options: Resource<HttpRequestOptions>) -> Result<()> {
        self.table.delete(options)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs> http_types::HostRequest for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_method(&mut self, request: Resource<WasiRequest>) -> Result<http_types::Method> {
        Ok(wit_method(&self.table.get(&request)?.head.method))
    }

    fn set_method(
        &mut self,
        request: Resource<WasiRequest>,
        method: http_types::Method,
    ) -> Result<core::result::Result<(), ()>> {
        let Ok(method) = kernel_method(method) else {
            return Ok(Err(()));
        };
        self.table.get_mut(&request)?.head.method = method;
        Ok(Ok(()))
    }

    fn get_path_with_query(&mut self, request: Resource<WasiRequest>) -> Result<Option<String>> {
        Ok(self.table.get(&request)?.head.path_with_query.clone())
    }

    fn set_path_with_query(
        &mut self,
        request: Resource<WasiRequest>,
        path_with_query: Option<String>,
    ) -> Result<core::result::Result<(), ()>> {
        if let Some(path_with_query) = path_with_query.as_deref()
            && validate_http_path_with_query(path_with_query).is_err()
        {
            return Ok(Err(()));
        }
        self.table.get_mut(&request)?.head.path_with_query = path_with_query;
        Ok(Ok(()))
    }

    fn get_scheme(&mut self, request: Resource<WasiRequest>) -> Result<Option<http_types::Scheme>> {
        Ok(self
            .table
            .get(&request)?
            .head
            .scheme
            .as_ref()
            .map(wit_scheme))
    }

    fn set_scheme(
        &mut self,
        request: Resource<WasiRequest>,
        scheme: Option<http_types::Scheme>,
    ) -> Result<core::result::Result<(), ()>> {
        let scheme = match scheme.map(kernel_scheme).transpose() {
            Ok(scheme) => scheme,
            Err(()) => return Ok(Err(())),
        };
        self.table.get_mut(&request)?.head.scheme = scheme;
        Ok(Ok(()))
    }

    fn get_authority(&mut self, request: Resource<WasiRequest>) -> Result<Option<String>> {
        Ok(self.table.get(&request)?.head.authority.clone())
    }

    fn set_authority(
        &mut self,
        request: Resource<WasiRequest>,
        authority: Option<String>,
    ) -> Result<core::result::Result<(), ()>> {
        if let Some(authority) = authority.as_deref()
            && validate_http_authority(authority).is_err()
        {
            return Ok(Err(()));
        }
        self.table.get_mut(&request)?.head.authority = authority;
        Ok(Ok(()))
    }

    fn get_options(
        &mut self,
        request: Resource<WasiRequest>,
    ) -> Result<Option<Resource<HttpRequestOptions>>> {
        let Some(options) = self.table.get(&request)?.head.options.as_ref() else {
            return Ok(None);
        };
        let options = options.immutable_copy();
        Ok(Some(self.table.push(options)?))
    }

    fn get_headers(&mut self, request: Resource<WasiRequest>) -> Result<Resource<HttpFields>> {
        let headers = self.table.get(&request)?.head.headers.clone();
        Ok(self.table.push(headers)?)
    }
}

impl<CpuImpl, HostFs> http_types::HostResponse for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn get_status_code(&mut self, response: Resource<WasiResponse>) -> Result<u16> {
        Ok(self.table.get(&response)?.head.status)
    }

    fn set_status_code(
        &mut self,
        response: Resource<WasiResponse>,
        status_code: u16,
    ) -> Result<core::result::Result<(), ()>> {
        if validate_http_status_code(status_code).is_err() {
            return Ok(Err(()));
        }
        self.table.get_mut(&response)?.head.status = status_code;
        Ok(Ok(()))
    }

    fn get_headers(&mut self, response: Resource<WasiResponse>) -> Result<Resource<HttpFields>> {
        let headers = self.table.get(&response)?.head.headers.clone();
        Ok(self.table.push(headers)?)
    }
}

impl<CpuImpl, HostFs, U> http_types::HostRequestWithStore<U> for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new(
        mut access: Access<'_, U, Self>,
        headers: Resource<HttpFields>,
        contents: Option<StreamReader<u8>>,
        trailers: GuestTrailers,
        options: Option<Resource<HttpRequestOptions>>,
    ) -> Result<(Resource<WasiRequest>, GuestBodyResult)> {
        let (body, result) = WasiBody::new_guest(&mut access, contents, trailers)?;
        let store = access.get();
        // Fields and options handed to the host by value become immutable, so
        // reading them back out of the request never yields a mutable view.
        let headers = store.table.delete(headers)?.into_immutable();
        let options = match options {
            Some(options) => Some(store.table.delete(options)?.immutable_copy()),
            None => None,
        };
        let request = store.table.push(WasiRequest {
            head: HttpRequestHead::new(headers, options),
            body,
        })?;
        Ok((request, result))
    }

    fn consume_body(
        mut access: Access<'_, U, Self>,
        request: Resource<WasiRequest>,
        fut: GuestBodyResult,
    ) -> Result<(StreamReader<u8>, GuestTrailers)> {
        let WasiRequest { body, .. } = access.get().table.delete(request)?;
        body.consume(&mut access, fut)
    }

    fn drop(mut access: Access<'_, U, Self>, request: Resource<WasiRequest>) -> Result<()> {
        let WasiRequest { body, .. } = access.get().table.delete(request)?;
        body.close(&mut access)
    }
}

impl<CpuImpl, HostFs, U> http_types::HostResponseWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    fn new(
        mut access: Access<'_, U, Self>,
        headers: Resource<HttpFields>,
        contents: Option<StreamReader<u8>>,
        trailers: GuestTrailers,
    ) -> Result<(Resource<WasiResponse>, GuestBodyResult)> {
        let (body, result) = WasiBody::new_guest(&mut access, contents, trailers)?;
        let store = access.get();
        let headers = store.table.delete(headers)?.into_immutable();
        let response = store.table.push(WasiResponse {
            head: HttpResponseHead::new(headers),
            body,
        })?;
        Ok((response, result))
    }

    fn consume_body(
        mut access: Access<'_, U, Self>,
        response: Resource<WasiResponse>,
        fut: GuestBodyResult,
    ) -> Result<(StreamReader<u8>, GuestTrailers)> {
        let WasiResponse { body, .. } = access.get().table.delete(response)?;
        body.consume(&mut access, fut)
    }

    fn drop(mut access: Access<'_, U, Self>, response: Resource<WasiResponse>) -> Result<()> {
        let WasiResponse { body, .. } = access.get().table.delete(response)?;
        body.close(&mut access)
    }
}

// ---------------------------------------------------------------------------
// `wasi:http/client`
// ---------------------------------------------------------------------------

impl<CpuImpl, HostFs> http_client::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs, U> http_client::HostWithStore<U> for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    /// Forward the request to the `http-client` kernel plugin and wait.
    ///
    /// Nothing here parses or transmits HTTP: the request head travels as
    /// kernel values and the body as a kernel byte channel, so a plugin
    /// restart is visible only as a dropped response channel.
    async fn send(
        accessor: &Accessor<U, Self>,
        request: Resource<WasiRequest>,
    ) -> core::result::Result<Resource<WasiResponse>, HttpError> {
        let (exchange, response_rx, runtime_state) = accessor.with(|mut access| {
            let WasiRequest { head, body } = access
                .get()
                .table
                .delete(request)
                .map_err(TrappableError::trap)?;
            let body = body.into_host(&mut access).map_err(TrappableError::trap)?;
            let (response, response_rx) = oneshot::channel();
            let runtime_state = access.get().runtime_state.clone();
            Ok::<_, HttpError>((
                HttpExchange {
                    head,
                    body,
                    response,
                },
                response_rx,
                runtime_state,
            ))
        })?;

        match runtime_state.http_client().send(exchange).await {
            Ok(()) => {}
            // No plugin was provisioned in this kernel image at all.
            Err(ProviderError::Unavailable) => {
                return Err(http_types::ErrorCode::ConfigurationError.into());
            }
            Err(ProviderError::Closed) => return Err(plugin_unavailable()),
        }

        let response = match response_rx.await {
            Ok(Ok(response)) => response,
            Ok(Err(code)) => return Err(wit_error_code(code).into()),
            // The plugin died with this exchange in flight.
            Err(oneshot::Canceled) => return Err(plugin_unavailable()),
        };

        accessor.with(|mut access| {
            let HttpResponse { head, body } = response;
            access
                .get()
                .table
                .push(WasiResponse::from_host(head, body))
                .map_err(TrappableError::trap)
        })
    }
}

/// Link `wasi:http` into a store, for the interfaces the component imports.
///
/// `types` is linked whenever either interface is present: `client` is defined
/// in terms of the `types` resources, so linking it alone would leave those
/// resource types undefined.
pub(crate) fn add_to_linker<CpuImpl, HostFs>(
    linker: &mut wasmtime::component::Linker<StoreData<CpuImpl, HostFs>>,
    imports: &WasiImportSet,
) -> Result<()>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    let wants_types = imports.has("wasi:http/types", "0.3");
    let wants_client = imports.has("wasi:http/client", "0.3");
    if wants_types || wants_client {
        http_types::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| state)?;
    }
    if wants_client {
        http_client::add_to_linker::<_, HasSelf<StoreData<CpuImpl, HostFs>>>(linker, |state| {
            state
        })?;
    }
    Ok(())
}

/// Interfaces [`add_to_linker`] serves. `wasi:http/handler` is not among them:
/// the kernel calls it as an export on the plugin's store rather than serving
/// it as an import.
#[cfg(test)]
pub(crate) const LINKED_INTERFACES: &[&str] = &["wasi:http/types", "wasi:http/client"];
