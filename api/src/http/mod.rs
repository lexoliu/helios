//! HTTP over `wasi:http@0.3.0`.
//!
//! The generated WIT bindings are re-exported as [`types`] for callers that
//! need the resources directly. On top of them this module provides the two
//! ergonomic halves of the interface:
//!
//! * [`Request`]/[`Response`] plus [`send`], a client built on
//!   `wasi:http/client`. Available in every world that imports the client,
//!   which is every world except the one the `http-client` kernel plugin
//!   implements — the plugin *is* the client, so it cannot import one.
//! * [`handler`], the export side, available when the `http-handler` feature
//!   selects the plugin's world.

pub use crate::bindings::wasi::http::types;

pub use types::{
    DnsErrorPayload, ErrorCode, FieldSizePayload, Fields, HeaderError, Method, RequestOptions,
    RequestOptionsError, Scheme, StatusCode, TlsAlertReceivedPayload,
};

#[cfg(not(feature = "http-handler"))]
mod client;

#[cfg(not(feature = "http-handler"))]
pub use client::{Request, RequestBody, Response, ResponseBody, Streaming, UrlError, send};

#[cfg(feature = "http-handler")]
pub mod handler;
