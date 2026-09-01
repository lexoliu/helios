//! Export side of `wasi:http`, for components that answer requests.
//!
//! A component implements [`Guest`] and hands its type to
//! `helios_api::bindings::export!`. The `http-client` kernel plugin is the
//! in-tree implementor: the kernel calls `handle` once per exchange forwarded
//! from `wasi:http/client`.

pub use crate::bindings::exports::wasi::http::handler::Guest;
