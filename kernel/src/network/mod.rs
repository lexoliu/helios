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

/// The established-connection fixture the retirement tests in
/// `wasmtime_adapter` need. Its wiring is private to `network::service`
/// and the owners it exercises are not.
#[cfg(all(test, feature = "wasmtime-runtime"))]
pub(crate) use service::fixture::EstablishedTcpFixture;

/// Bringing a discovered interface online: the one place a backend
/// hands the kernel a network device.
///
/// A backend's job ends at the device. Building the service over it
/// and publishing it are the same two steps on every target, so they
/// live here rather than being repeated — and, as #131 showed,
/// repeated incompletely — in `x86/`, `aarch64/` and `riscv/`.
///
/// What installs nothing here is the packet pump: a queue pair's
/// interrupt is routed to one processor, so the pump that drains the
/// pair is pinned to that processor and is started by its own run
/// loop, which awaits the install this publishes
/// ([`crate::RuntimeState::wait_for_network_service`]).
#[cfg(feature = "wasmtime-runtime")]
impl<CpuImpl, WatchdogImpl> crate::Kernel<CpuImpl, WatchdogImpl>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    /// Installs the network service over `device`.
    ///
    /// The pumps the service drives are not a backend's decision — or
    /// this call's. One pump per queue pair is what advances the
    /// interface when no socket is polling it, and the only waiters
    /// whose park is bounded by a protocol deadline rather than by
    /// some application's timeout — so they are what keeps the guest
    /// answering ARP and acknowledging segments while every socket on
    /// the machine is parked. An interface installed without them
    /// falls silent as soon as its last reader parks, which is exactly
    /// what #131 saw on the x86 tap lane: the host's neighbour entry
    /// for a live guest went `FAILED` in the middle of a transfer.
    /// Publishing the service is what releases them: each pump is
    /// pinned to the processor that owns its pair and was already
    /// parked on this install completing.
    pub fn install_network_interface<ProgramService, HostFsService, DeviceImpl>(
        &self,
        runtime_state: &crate::RuntimeState<
            ProgramService,
            NetworkService<CpuImpl, DeviceImpl>,
            HostFsService,
        >,
        device: DeviceImpl,
    ) where
        ProgramService: Clone + Send + Sync + 'static,
        HostFsService: Clone + Send + Sync + 'static,
        DeviceImpl: crate::NetworkDevice,
    {
        let service = NetworkService::new(
            self.cpu.clone(),
            runtime_state.profiles(),
            runtime_state.uptime_clock(),
            self.timer(),
            device,
        );
        runtime_state.install_network_service(service);
    }
}
