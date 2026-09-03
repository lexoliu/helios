//! In-kernel network service.
//!
//! `service` hosts the `NetworkService` that wraps `helios-netstack`
//! for component-host TCP/UDP/DNS access. `control` exposes the
//! capability-checked admin API used by privileged components.
//! `socket_stack` provides per-task socket lifecycle bookkeeping.
//! `http` holds the transport-neutral HTTP value types that cross the
//! boundary between a program and the `http-client` kernel plugin.

mod control;
mod http;
mod service;
mod socket_stack;

pub use control::{
    Ipv4Cidr, Ipv4Route, MacAddress, NetworkAdminBackend, NetworkBridgeRequest,
    NetworkBridgeSecurity, NetworkControl, NetworkControlError, NetworkPortId,
};
pub use http::{
    HTTP_FORBIDDEN_FIELD_NAMES, HTTP_MAX_FIELD_SECTION_BYTES, HTTP_MAX_FIELD_VALUE_BYTES, HttpBody,
    HttpDnsErrorPayload, HttpErrorCode, HttpExchange, HttpFieldName, HttpFieldSizePayload,
    HttpFields, HttpHeaderError, HttpMethod, HttpRequestHead, HttpRequestOptions,
    HttpRequestOptionsError, HttpResponse, HttpResponseHead, HttpScheme, HttpSyntaxError,
    HttpSyntaxKind, HttpTlsAlertReceivedPayload, validate_http_authority,
    validate_http_path_with_query, validate_http_status_code,
};
pub use service::{
    NetworkQueueStats, NetworkService, NetworkStats, TcpListenerId, TcpStreamId, UdpSocketId,
};
pub use socket_stack::SocketStack;
