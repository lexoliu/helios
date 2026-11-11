//! The client's response as a `wasi:http` response.
//!
//! The head becomes the `wasi:http` response and its body is streamed into
//! the response's content stream as the client receives it, so the caller
//! sees bytes as they arrive rather than after the whole body has been
//! buffered.

use std::string::{String, ToString};
use std::vec::Vec;

use futures_lite::StreamExt;
use helios_api::bindings::{wit_future, wit_stream};
use helios_api::http::types;
use helios_api::http::{ErrorCode, Fields, Scheme};
use helios_api::wit_bindgen::rt::async_support::{FutureWriter, StreamWriter};
use http::HeaderMap;
use zenwave::{Body, BodyFrame, Response};

use crate::error::body_code;
use crate::fields::{Section, build_fields};

/// The `wasi:http` response for `response`'s head, and the two writers its
/// body and trailers are delivered through.
pub fn to_wasi(
    response: &Response,
) -> Result<
    (
        types::Response,
        StreamWriter<u8>,
        FutureWriter<Result<Option<Fields>, ErrorCode>>,
    ),
    ErrorCode,
> {
    let fields = build_fields(wire_fields(response.headers()), Section::Headers)?;
    let (contents_writer, contents) = wit_stream::new::<u8>();
    let (trailers_writer, trailers) =
        wit_future::new::<Result<Option<Fields>, ErrorCode>>(|| Ok(None));
    let (wasi, _transmitted) = types::Response::new(fields, Some(contents), trailers);
    if wasi.set_status_code(response.status().as_u16()).is_err() {
        return Err(ErrorCode::HttpProtocolError);
    }
    Ok((wasi, contents_writer, trailers_writer))
}

fn wire_fields(headers: &HeaderMap) -> impl Iterator<Item = (String, Vec<u8>)> + '_ {
    headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()))
}

/// Stream the body into the response and then resolve its trailers future.
///
/// Runs after `handle` has returned: the caller only starts reading the body
/// once it holds the response. `scheme` says whether the connection under the
/// body carries TLS.
pub async fn pump(
    mut body: Body,
    scheme: Scheme,
    mut writer: StreamWriter<u8>,
    trailers_writer: FutureWriter<Result<Option<Fields>, ErrorCode>>,
) {
    let outcome = copy(&mut body, matches!(scheme, Scheme::Https), &mut writer).await;
    // `wasi:http` requires the content stream to be closed before the trailers
    // future resolves.
    drop(writer);
    let _ = trailers_writer.write(outcome).await;
}

async fn copy(
    body: &mut Body,
    https: bool,
    writer: &mut StreamWriter<u8>,
) -> Result<Option<Fields>, ErrorCode> {
    let mut trailers = None;
    while let Some(frame) = body.next().await {
        match frame.map_err(|error| body_code(&error, https))? {
            BodyFrame::Data(bytes) => {
                if !writer.write_all(bytes.to_vec()).await.is_empty() {
                    // The caller dropped the body: there is nobody to read the
                    // rest, and dropping the connection ends the exchange.
                    return Ok(None);
                }
            }
            BodyFrame::Trailers(map) => trailers = Some(map),
            _ => {}
        }
    }
    trailers
        .filter(|map| !map.is_empty())
        .map(|map| build_fields(wire_fields(&map), Section::Trailers))
        .transpose()
}
