//! The `http-client` kernel plugin.
//!
//! `wasi:http/client.send` in the kernel is a forwarder: it turns a caller's
//! request into a transport-neutral exchange and hands it to whoever exports
//! `wasi:http/handler`. This component is that exporter. It is an ordinary
//! user-mode wasm program with the ordinary isolation model — the kernel just
//! provisions it from bootfs and keeps one instance alive — so all of HTTP/1.1
//! lives out here, on top of `helios:system/net` sockets, and none of it lives
//! in the kernel.
//!
//! One connection per exchange, closed when the response body ends. There is
//! no pool: the kernel dispatches exchanges concurrently into this one
//! instance, so concurrency comes from having several handlers in flight
//! rather than from reusing sockets.

mod fields;
mod request;
mod response;
mod socket;

use std::string::String;
use std::time::Duration;

use helios_api::bindings::{wit_future, wit_stream};
use helios_api::http::handler::Guest;
use helios_api::http::types;
use helios_api::http::{ErrorCode, Fields, RequestOptions, Scheme};
use helios_api::net::TcpStream;
use helios_api::task::spawn;

use crate::socket::{Socket, connect_error};

mod bindings {
    pub use ::helios_api::bindings::*;
}

/// Default port for the `http` scheme.
const DEFAULT_PORT: u16 = 80;

/// Applied when the caller supplied no `request-options`.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_BETWEEN_BYTES_TIMEOUT: Duration = Duration::from_secs(600);

struct HttpClient;

::helios_api::bindings::export!(HttpClient);

impl Guest for HttpClient {
    async fn handle(request: types::Request) -> Result<types::Response, ErrorCode> {
        handle(request).await
    }
}

/// Transport-layer timeouts for one exchange.
struct Timeouts {
    connect: Duration,
    first_byte: Duration,
    between_bytes: Duration,
}

impl Timeouts {
    fn from_options(options: Option<&RequestOptions>) -> Self {
        let nanos =
            |value: Option<u64>, default: Duration| value.map_or(default, Duration::from_nanos);
        Self {
            connect: nanos(
                options.and_then(RequestOptions::get_connect_timeout),
                DEFAULT_CONNECT_TIMEOUT,
            ),
            first_byte: nanos(
                options.and_then(RequestOptions::get_first_byte_timeout),
                DEFAULT_FIRST_BYTE_TIMEOUT,
            ),
            between_bytes: nanos(
                options.and_then(RequestOptions::get_between_bytes_timeout),
                DEFAULT_BETWEEN_BYTES_TIMEOUT,
            ),
        }
    }
}

async fn handle(request: types::Request) -> Result<types::Response, ErrorCode> {
    let method = request.get_method();
    match request.get_scheme().unwrap_or(Scheme::Http) {
        Scheme::Http => {}
        // TLS needs a certificate store and a handshake implementation, and
        // this plugin has neither yet.
        Scheme::Https => return Err(ErrorCode::TlsProtocolError),
        Scheme::Other(_) => return Err(ErrorCode::HttpRequestUriInvalid),
    }
    let authority = request
        .get_authority()
        .ok_or(ErrorCode::HttpRequestUriInvalid)?;
    let target = request
        .get_path_with_query()
        .unwrap_or_else(|| String::from("/"));
    let headers = request.get_headers().copy_all();
    let framing = request::framing(&headers)?;

    // The options resource is a child of the request and must be dropped
    // before the request is consumed.
    let timeouts = {
        let options = request.get_options();
        Timeouts::from_options(options.as_ref())
    };

    let (host, port) = split_authority(&authority)?;
    let (body_result, body_result_reader) = wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
    let (contents, contents_trailers) = types::Request::consume_body(request, body_result_reader);

    let mut socket = Socket::new(
        TcpStream::connect_timeout(&host, port, timeouts.connect)
            .await
            .map_err(|error| connect_error(&error))?,
    );

    let head = request::Head {
        method: &method,
        target: &target,
        authority: &authority,
        headers: &headers,
    };
    let sent = request::send(&socket, &head, framing, contents, contents_trailers).await;
    // Whatever happened to the request body, the caller that produced it is
    // waiting on this verdict.
    let verdict = match &sent {
        Ok(()) => Ok(()),
        Err(code) => Err(code.clone()),
    };
    let _ = body_result.write(verdict).await;
    sent?;

    socket.set_timeout(timeouts.first_byte);
    let head = response::read_head(&mut socket).await?;
    let framing = response::framing(&method, &head)?;
    let fields = response::response_fields(&head, framing)?;
    socket.set_timeout(timeouts.between_bytes);

    let (contents_writer, contents) = wit_stream::new::<u8>();
    let (trailers_writer, trailers) =
        wit_future::new::<Result<Option<Fields>, ErrorCode>>(|| Ok(None));
    let (response, _transmitted) = types::Response::new(fields, Some(contents), trailers);
    if response.set_status_code(head.status).is_err() {
        return Err(ErrorCode::HttpProtocolError);
    }

    // The body is still on the wire. Draining it has to outlive this call, so
    // it continues in a task the component-model executor keeps polling after
    // `handle` has produced its response.
    spawn(response::pump(
        socket,
        framing,
        contents_writer,
        trailers_writer,
    ));

    Ok(response)
}

/// Split an authority into the host and port to dial.
fn split_authority(authority: &str) -> Result<(String, u16), ErrorCode> {
    let authority: http::uri::Authority = authority
        .parse()
        .map_err(|_| ErrorCode::HttpRequestUriInvalid)?;
    Ok((
        String::from(authority.host()),
        authority.port_u16().unwrap_or(DEFAULT_PORT),
    ))
}
