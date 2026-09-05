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

/// Bringing a discovered interface online: the one place a backend
/// hands the kernel a network device.
///
/// A backend's job ends at the device. Building the service over it,
/// publishing it to the component host and giving it a packet pump are
/// the same three steps on every target, so they live here rather than
/// being repeated — and, as #131 showed, repeated incompletely — in
/// `x86/`, `aarch64/` and `riscv/`.
#[cfg(feature = "wasmtime-runtime")]
impl<CpuImpl, WatchdogImpl> crate::Kernel<CpuImpl, WatchdogImpl>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    /// Installs the network service over `device` and starts its packet
    /// pump.
    ///
    /// The pump is not a backend's decision. It is the only task that
    /// advances the interface when no socket is polling it, and the
    /// only waiter whose park is bounded by a protocol deadline rather
    /// than by some application's timeout — so it is what keeps the
    /// guest answering ARP and acknowledging segments while every
    /// socket on the machine is parked. An interface installed without
    /// one falls silent as soon as its last reader parks, which is
    /// exactly what #131 saw on the x86 tap lane: the host's neighbour
    /// entry for a live guest went `FAILED` in the middle of a
    /// transfer.
    pub fn install_network_interface<ProgramService, HostFsService, DeviceImpl>(
        &self,
        runtime_state: &crate::RuntimeState<
            ProgramService,
            crate::ComponentHostNetworkService,
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
            runtime_state.clone(),
            self.timer(),
            device,
        );
        let pump = service.clone();
        runtime_state
            .install_network_service(crate::ComponentHostNetworkService::from_service(service));
        self.spawn_detached(async move {
            pump.run_packet_pump().await;
        });
    }
}
