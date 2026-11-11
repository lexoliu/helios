//! The exchange's `wasi:http` request as the `http` request zenwave sends.
//!
//! Method, target and fields are converted here, and the body is adapted: its
//! frames are pulled from the caller's stream as the client writes them. The
//! verdict on the body, which the caller waits on, is settled by
//! [`Verdict::settle`]: from the caller's stream when it ends, or from the
//! exchange when it fails first. Framing is the client's: the caller's own
//! `content-length` decides it, and a body that differs from it fails the
//! exchange.

use std::string::String;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::vec::Vec;

use futures_channel::oneshot;
use futures_lite::{future, stream};
use helios_api::bindings::wit_future;
use helios_api::http::types::{self, Method};
use helios_api::http::{ErrorCode, Fields, Scheme};
use helios_api::stream_closed;
use helios_api::wit_bindgen::rt::async_support::{FutureReader, FutureWriter, StreamReader};
use http::header::{HeaderMap, HeaderName, HeaderValue};
use thiserror::Error;
use zenwave::{Body, BodyError, BodyFrame};

/// Bytes pulled from the caller's body stream in one step.
const BODY_CHUNK_BYTES: usize = 64 * 1024;

/// What happened to the caller's request body, shared between the body the
/// client reads and the code that maps the client's errors.
#[derive(Debug, Default)]
pub struct BodyOutcome {
    sent: AtomicU64,
    failure: OnceLock<ErrorCode>,
}

impl BodyOutcome {
    /// Bytes of the body handed to the client so far.
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// The code the caller's own body failed with, if it did.
    pub fn failure(&self) -> Option<ErrorCode> {
        self.failure.get().cloned()
    }
}

/// The caller's body stream reported a failure instead of its trailers.
#[derive(Debug, Error)]
#[error("the request body failed: {0:?}")]
struct BodyFailed(ErrorCode);

/// The caller's wait for the verdict on its request body, and the report of
/// how the body stream ended.
pub struct Verdict {
    writer: FutureWriter<Result<(), ErrorCode>>,
    body: oneshot::Receiver<Result<(), ErrorCode>>,
}

/// Which of the two reports a verdict waits on came first.
enum First {
    Body(Result<Result<(), ErrorCode>, oneshot::Canceled>),
    Exchange(Result<ErrorCode, oneshot::Canceled>),
}

impl Verdict {
    /// Settle the caller's verdict. The caller's stream ending decides it;
    /// an exchange that fails first decides it with its own code, since the
    /// client then stops reading the body. `exchange` carries that code and
    /// is dropped unsent when the exchange succeeds; a body the client
    /// dropped after a successful exchange was never fully sent.
    pub async fn settle(self, mut exchange: oneshot::Receiver<ErrorCode>) {
        let Self {
            writer,
            body: mut ended,
        } = self;
        let first = future::or(async { First::Body((&mut ended).await) }, async {
            First::Exchange((&mut exchange).await)
        })
        .await;
        let verdict = match first {
            First::Body(Ok(verdict)) => verdict,
            First::Body(Err(oneshot::Canceled)) => {
                Err(exchange.await.unwrap_or(ErrorCode::ConnectionTerminated))
            }
            First::Exchange(Ok(code)) => Err(code),
            First::Exchange(Err(oneshot::Canceled)) => {
                ended.await.unwrap_or(Err(ErrorCode::ConnectionTerminated))
            }
        };
        // An error here means the caller dropped its end and wants no verdict.
        let _ = writer.write(verdict).await;
    }
}

/// Turn a `wasi:http` request into the request the client sends, the
/// outcome of its body and the caller's verdict on it.
///
/// The request is consumed. The scheme is returned beside it because the
/// client takes the request out of its slot.
pub fn into_http(
    request: types::Request,
) -> Result<(http::Request<Body>, Scheme, Arc<BodyOutcome>, Verdict), ErrorCode> {
    let method = convert_method(&request.get_method())?;
    let scheme = request.get_scheme().unwrap_or(Scheme::Http);
    let scheme_text = match &scheme {
        Scheme::Http => "http",
        Scheme::Https => "https",
        Scheme::Other(_) => return Err(ErrorCode::HttpRequestUriInvalid),
    };
    let authority = request
        .get_authority()
        .ok_or(ErrorCode::HttpRequestUriInvalid)?;
    let target = request
        .get_path_with_query()
        .unwrap_or_else(|| String::from("/"));
    let uri = http::Uri::builder()
        .scheme(scheme_text)
        .authority(authority)
        .path_and_query(target)
        .build()
        .map_err(|_| ErrorCode::HttpRequestUriInvalid)?;
    let headers = convert_fields(request.get_headers().copy_all())?;

    // `settle` always writes; the default is only what a caller sees if the
    // plugin's task is torn down first, and nothing was then sent in full.
    let (writer, verdict_reader) =
        wit_future::new::<Result<(), ErrorCode>>(|| Err(ErrorCode::ConnectionTerminated));
    let (contents, trailers) = types::Request::consume_body(request, verdict_reader);
    let outcome = Arc::new(BodyOutcome::default());
    let (end, ended) = oneshot::channel();
    let frames = body_frames(contents, trailers, end, Arc::clone(&outcome));

    let mut http = http::Request::new(Body::from_frames(frames));
    *http.method_mut() = method;
    *http.uri_mut() = uri;
    *http.headers_mut() = headers;
    let verdict = Verdict {
        writer,
        body: ended,
    };
    Ok((http, scheme, outcome, verdict))
}

fn convert_method(method: &Method) -> Result<http::Method, ErrorCode> {
    Ok(match method {
        Method::Get => http::Method::GET,
        Method::Head => http::Method::HEAD,
        Method::Post => http::Method::POST,
        Method::Put => http::Method::PUT,
        Method::Delete => http::Method::DELETE,
        Method::Connect => http::Method::CONNECT,
        Method::Options => http::Method::OPTIONS,
        Method::Trace => http::Method::TRACE,
        Method::Patch => http::Method::PATCH,
        Method::Other(other) => http::Method::from_bytes(other.as_bytes())
            .map_err(|_| ErrorCode::HttpRequestMethodInvalid)?,
    })
}

/// The caller's fields as a header map. `wasi:http` has validated them, so a
/// name or value that does not convert is a protocol violation.
fn convert_fields(fields: Vec<(String, Vec<u8>)>) -> Result<HeaderMap, ErrorCode> {
    let mut map = HeaderMap::with_capacity(fields.len());
    for (name, value) in fields {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| ErrorCode::HttpProtocolError)?,
            HeaderValue::from_bytes(&value).map_err(|_| ErrorCode::HttpProtocolError)?,
        );
    }
    Ok(map)
}

/// What the stream of body frames is driven by.
struct BodyState {
    contents: StreamReader<u8>,
    trailers: FutureReader<Result<Option<Fields>, ErrorCode>>,
    end: oneshot::Sender<Result<(), ErrorCode>>,
    outcome: Arc<BodyOutcome>,
}

/// The caller's body as the frames the client writes: data as it arrives,
/// then the trailers the caller ends with.
///
/// When the stream ends, how it ended is reported through `end`, whether or
/// not the client has written all of it yet: the verdict is about the
/// caller's own stream. A body the client drops before then reports nothing.
fn body_frames(
    contents: StreamReader<u8>,
    trailers: FutureReader<Result<Option<Fields>, ErrorCode>>,
    end: oneshot::Sender<Result<(), ErrorCode>>,
    outcome: Arc<BodyOutcome>,
) -> impl stream::Stream<Item = Result<BodyFrame, BodyError>> + Send + Sync + 'static {
    let state = BodyState {
        contents,
        trailers,
        end,
        outcome,
    };
    stream::unfold(Some(state), |state| async move {
        let mut state = state?;
        loop {
            let (result, chunk) = state
                .contents
                .read(Vec::with_capacity(BODY_CHUNK_BYTES))
                .await;
            if !chunk.is_empty() {
                state
                    .outcome
                    .sent
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                return Some((Ok(BodyFrame::Data(chunk.into())), Some(state)));
            }
            if !stream_closed(result) {
                continue;
            }
            // The trailers future resolves only after the body stream closes,
            // and it is also how the caller reports that its body could not
            // be produced.
            let trailers = state.trailers.await;
            let verdict = trailers.as_ref().map(|_| ()).map_err(Clone::clone);
            // An error here means the verdict was already settled by a
            // failed exchange.
            let _ = state.end.send(verdict);
            return match trailers {
                Ok(Some(fields)) => Some((trailer_frame(&fields), None)),
                Ok(None) => None,
                Err(code) => {
                    let _ = state.outcome.failure.set(code.clone());
                    Some((Err(BodyError::Other(Box::new(BodyFailed(code)))), None))
                }
            };
        }
    })
}

fn trailer_frame(fields: &Fields) -> Result<BodyFrame, BodyError> {
    let trailers = convert_fields(fields.copy_all())
        .map_err(|code| BodyError::Other(Box::new(BodyFailed(code))))?;
    Ok(BodyFrame::Trailers(trailers))
}
