//! Host bindings for `wasi:http`.
//!
//! Generated from the `helios:kernel/http-host` world rather than from
//! `wasi:http/service`: the kernel serves exactly two interfaces here
//! (`types` and `client`) and calls exactly one export (`handler`), and a
//! world built on `wasi:cli/imports` would regenerate every interface that
//! [`super::bindings`] already provides.
//!
//! The import half is linked into every program store by
//! [`super::http::add_to_linker`]. The export half is what the `http-client`
//! plugin runner uses to invoke `wasi:http/handler.handle` inside the plugin's
//! own store.

mod generated {
    use wasmtime;

    wasmtime::component::bindgen!({
        path: "../wit",
        world: "helios:kernel/http-host",
        imports: {
            "wasi:http/client.send": store | trappable,
            "wasi:http/types.[static]request.new": store | trappable,
            "wasi:http/types.[static]request.consume-body": store | trappable,
            "wasi:http/types.[drop]request": store | trappable,
            "wasi:http/types.[static]response.new": store | trappable,
            "wasi:http/types.[static]response.consume-body": store | trappable,
            "wasi:http/types.[drop]response": store | trappable,
            default: trappable,
        },
        exports: { default: async | store },
        require_store_data_send: true,
        with: {
            "wasi:http/types.fields": crate::HttpFields,
            "wasi:http/types.request": crate::wasmtime_adapter::wasi::WasiRequest,
            "wasi:http/types.response": crate::wasmtime_adapter::wasi::WasiResponse,
            "wasi:http/types.request-options": crate::HttpRequestOptions,
        },
        trappable_error_type: {
            "wasi:http/types.error-code" => crate::wasmtime_adapter::wasi::HttpError,
            "wasi:http/types.header-error" => crate::wasmtime_adapter::wasi::HttpHeaderTrapError,
            "wasi:http/types.request-options-error" =>
                crate::wasmtime_adapter::wasi::HttpRequestOptionsTrapError,
        },
    });
}

pub(crate) use self::generated::exports;
pub(crate) use self::generated::wasi::http;
pub(crate) use self::generated::{HttpHost, HttpHostPre};
