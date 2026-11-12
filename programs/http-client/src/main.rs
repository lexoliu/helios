//! The `http-client` kernel plugin.
//!
//! `wasi:http/client.send` in the kernel is a forwarder: it turns a caller's
//! request into a transport-neutral exchange and hands it to whoever exports
//! `wasi:http/handler`. This component is that exporter. It is an ordinary
//! user-mode wasm program with the ordinary isolation model — the kernel just
//! provisions it from bootfs and keeps one instance alive — so all of HTTP
//! lives out here, and none of it lives in the kernel.
//!
//! The plugin carries no protocol code of its own. It hosts zenwave's native
//! backend (HTTP/1.1 framing, TLS through rustls with the Mozilla roots
//! compiled in) over `helios_api::http_kit::HeliosNet`, which is
//! `http-kit-io` on `wasi:sockets` and the monotonic clock. What is left
//! here is the translation between `wasi:http` and `http` messages, the
//! `request-options` timeouts, and the choice of `wasi:http` error code for
//! each failure.
//!
//! One connection per exchange, closed when the response body ends. There is
//! no pool: the kernel dispatches exchanges concurrently into this one
//! instance, so concurrency comes from having several handlers in flight
//! rather than from reusing sockets.

mod error;
mod fields;
mod request;
mod response;

use std::time::Duration;

use futures_channel::oneshot;
use helios_api::http::handler::Guest;
use helios_api::http::types;
use helios_api::http::{ErrorCode, RequestOptions, Scheme};
use helios_api::http_kit::{HeliosExecutor, HeliosNet};
use helios_api::task::spawn;
use http::StatusCode;
use zenwave::backend::NativeBackend;
use zenwave::transport::Proxy;
use zenwave::{Body, Endpoint, Timeouts, Transport};

use crate::error::code;
use crate::request::BodyOutcome;

mod bindings {
    pub use ::helios_api::bindings::*;
}

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

/// The client's timeouts for one exchange: the connect timeout, the time to
/// the first byte of the response head, and the time between the bytes of
/// its body.
fn timeouts(options: Option<&RequestOptions>) -> Timeouts {
    let nanos = |value: Option<u64>, default: Duration| value.map_or(default, Duration::from_nanos);
    Timeouts::default()
        .with_connect(nanos(
            options.and_then(RequestOptions::get_connect_timeout),
            DEFAULT_CONNECT_TIMEOUT,
        ))
        .with_response_head(nanos(
            options.and_then(RequestOptions::get_first_byte_timeout),
            DEFAULT_FIRST_BYTE_TIMEOUT,
        ))
        .with_body_idle(nanos(
            options.and_then(RequestOptions::get_between_bytes_timeout),
            DEFAULT_BETWEEN_BYTES_TIMEOUT,
        ))
}

async fn handle(request: types::Request) -> Result<types::Response, ErrorCode> {
    // The options resource is a child of the request and must be dropped
    // before the request is consumed.
    let timeouts = {
        let options = request.get_options();
        timeouts(options.as_ref())
    };
    let (mut http, scheme, outcome, verdict) = request::into_http(request)?;
    http.extensions_mut().insert(timeouts);
    // The caller's verdict on its body settles from the body or from the
    // exchange, whichever ends first, so it waits on both in a task of its
    // own.
    let (failed, failure) = oneshot::channel();
    spawn(verdict.settle(failure));
    let exchanged = exchange(http, scheme, &outcome).await;
    if let Err(code) = &exchanged {
        // An error here means the body already decided the verdict.
        let _ = failed.send(code.clone());
    }
    exchanged
}

/// Send the request and turn the response head into the caller's response,
/// leaving its body to stream in a task of its own.
async fn exchange(
    mut http: http::Request<Body>,
    scheme: Scheme,
    outcome: &BodyOutcome,
) -> Result<types::Response, ErrorCode> {
    // No proxy: a guest reaches the network directly.
    let transport = Transport::builder()
        .proxy(Proxy::none())
        .build()
        .map_err(|error| code(&error, outcome))?;
    let mut backend =
        NativeBackend::on_current_thread(transport, HeliosNet, HeliosNet, &HeliosExecutor);
    let response = backend
        .respond(&mut http)
        .await
        .map_err(|error| code(&error, outcome))?;
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        // Nothing in this client asks for an upgrade, so a switch leaves the
        // connection speaking a protocol it cannot read.
        return Err(ErrorCode::HttpUpgradeFailed);
    }

    let (wasi, contents_writer, trailers_writer) = response::to_wasi(&response)?;
    // The body is still on the wire. Draining it has to outlive this call, so
    // it continues in a task the component-model executor keeps polling after
    // `handle` has produced its response.
    spawn(response::pump(
        response.into_body(),
        scheme,
        contents_writer,
        trailers_writer,
    ));
    Ok(wasi)
}
