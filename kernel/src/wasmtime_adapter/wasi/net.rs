use super::*;

pub(crate) fn has_wasi_network_rights(
    authority: &crate::ProcessAuthority,
    rights: crate::NetworkAuthorityRights,
) -> bool {
    authority.network_rights().contains(rights)
}

pub(crate) fn wasi_udp_bind_rights(local_port: u16) -> crate::NetworkAuthorityRights {
    if local_port != 0 && local_port < 1024 {
        crate::NetworkAuthorityRights::UDP | crate::NetworkAuthorityRights::PRIVILEGED_BIND
    } else {
        crate::NetworkAuthorityRights::UDP
    }
}

pub(crate) fn wasi_tcp_bind_rights(local_port: u16) -> crate::NetworkAuthorityRights {
    if local_port != 0 && local_port < 1024 {
        crate::NetworkAuthorityRights::TCP | crate::NetworkAuthorityRights::PRIVILEGED_BIND
    } else {
        crate::NetworkAuthorityRights::TCP
    }
}

pub struct P2Network;
#[derive(Clone)]
pub struct TcpSocket {
    pub(super) inner: Arc<Mutex<TcpSocketState>>,
    pub(super) ready: Arc<crate::Notify>,
}

#[derive(Clone)]
pub struct P2ResolveAddressStream {
    pub(super) inner: Arc<Mutex<P2ResolveAddressStreamState>>,
    pub(super) ready: Arc<crate::Notify>,
}

pub(super) struct P2ResolveAddressStreamState {
    pub(super) result: Option<core::result::Result<Vec<crate::Ipv4Address>, crate::DnsError>>,
    pub(super) cursor: usize,
}

pub(super) struct TcpSocketState {
    pub(super) service: ComponentHostNetworkService,
    pub(super) family: WasiTcpSocketFamily,
    pub(super) stream: Option<u64>,
    pub(super) listener: Option<u64>,
    pub(super) local_address: Option<WasiTcpSocketAddress>,
    pub(super) remote_address: Option<WasiTcpSocketAddress>,
    pub(super) receive_shutdown: bool,
    pub(super) send_shutdown: bool,
    pub(super) connect_in_progress: bool,
    pub(super) connect_result: Option<core::result::Result<(), crate::TcpError>>,
    pub(super) listen_in_progress: bool,
    pub(super) listen_result:
        Option<core::result::Result<crate::TcpListener<u64>, crate::TcpError>>,
    pub(super) listen_backlog: u16,
    pub(super) accept_in_progress: bool,
    pub(super) accept_result:
        Option<core::result::Result<crate::TcpAccepted<u64>, crate::TcpError>>,
    pub(super) keep_alive_enabled: bool,
    pub(super) keep_alive_idle_time: u64,
    pub(super) keep_alive_interval: u64,
    pub(super) keep_alive_count: u32,
    pub(super) hop_limit: u8,
    pub(super) receive_buffer_size: u64,
    pub(super) send_buffer_size: u64,
}

#[derive(Clone)]
pub struct UdpSocket {
    pub(super) inner: Arc<Mutex<UdpSocketState>>,
}

pub struct P2IncomingDatagramStream {
    pub(super) socket: UdpSocket,
    pub(super) remote_address: Option<WasiUdpSocketAddress>,
}

pub struct P2OutgoingDatagramStream {
    pub(super) socket: UdpSocket,
    pub(super) remote_address: Option<WasiUdpSocketAddress>,
    pub(super) check_send_permit_count: u64,
}

pub(super) struct TcpReadStreamProducer {
    pub(super) socket: TcpSocket,
    pub(super) pending: Option<Pin<Box<dyn core::future::Future<Output = TcpReadResult> + Send>>>,
    pub(super) completion:
        Option<oneshot::Sender<core::result::Result<(), socket_types::ErrorCode>>>,
}

impl Unpin for TcpReadStreamProducer {}

pub(super) struct TcpWriteConsumer {
    pub(super) socket: TcpSocket,
    pub(super) pending: Option<Pin<Box<dyn core::future::Future<Output = TcpWriteResult> + Send>>>,
    pub(super) completion:
        Option<oneshot::Sender<core::result::Result<(), socket_types::ErrorCode>>>,
}

impl Unpin for TcpWriteConsumer {}

pub(super) struct TcpListenStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) socket: TcpSocket,
    pub(super) get_store_data: for<'a> fn(&'a mut T) -> &'a mut StoreData<CpuImpl, HostFs>,
    pub(super) spawner: crate::Spawner<CpuImpl>,
    pub(super) waiter: crate::NotifyWaiter,
}

impl<T, CpuImpl, HostFs> Unpin for TcpListenStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum P3TcpListenStreamOperation {
    Listen,
    Accept,
    LocalAddress,
    InvalidState,
}

#[derive(Clone, Debug, Error)]
#[error("preview3 TCP listener stream {operation:?} failed")]
pub(super) struct P3TcpListenStreamError {
    pub(super) operation: P3TcpListenStreamOperation,
    #[source]
    pub(super) source: Option<crate::TcpError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WasiTcpSocketFamily {
    Ipv4,
    Ipv6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WasiTcpIpAddress {
    Ipv4(crate::Ipv4Address),
    Ipv6(Ipv6Address),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WasiTcpSocketAddress {
    pub(super) address: WasiTcpIpAddress,
    pub(super) port: u16,
}

impl WasiTcpSocketFamily {
    pub(super) const fn unspecified_address(self) -> WasiTcpIpAddress {
        match self {
            Self::Ipv4 => WasiTcpIpAddress::Ipv4(crate::Ipv4Address::new([0, 0, 0, 0])),
            Self::Ipv6 => WasiTcpIpAddress::Ipv6(Ipv6Address::UNSPECIFIED),
        }
    }
}

impl WasiTcpIpAddress {
    pub(super) const fn family(self) -> WasiTcpSocketFamily {
        match self {
            Self::Ipv4(_) => WasiTcpSocketFamily::Ipv4,
            Self::Ipv6(_) => WasiTcpSocketFamily::Ipv6,
        }
    }

    pub(super) const fn network_address(self) -> crate::NetworkIpAddress {
        match self {
            Self::Ipv4(address) => crate::NetworkIpAddress::Ipv4(address),
            Self::Ipv6(address) => crate::NetworkIpAddress::Ipv6(address),
        }
    }
}

impl P2ResolveAddressStream {
    pub(crate) fn pending() -> Self {
        Self {
            inner: Arc::new(Mutex::new(P2ResolveAddressStreamState {
                result: None,
                cursor: 0,
            })),
            ready: Arc::new(crate::Notify::new()),
        }
    }

    pub(crate) fn complete(
        &self,
        result: core::result::Result<Vec<crate::Ipv4Address>, crate::DnsError>,
    ) {
        self.inner.lock().result = Some(result);
        self.ready.notify_all();
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.inner.lock().result.is_some()
    }

    pub(crate) async fn wait_ready(&self) {
        loop {
            if self.is_ready() {
                return;
            }
            self.ready.notified().await;
        }
    }

    pub(crate) fn next_address(
        &mut self,
    ) -> core::result::Result<Option<crate::Ipv4Address>, crate::DnsError> {
        let mut state = self.inner.lock();
        let Some(result) = state.result.as_ref() else {
            return Err(crate::DnsError {
                kind: crate::DnsErrorKind::Unavailable,
                detail: crate::NetworkErrorDetail::DnsResultNotReady,
            });
        };
        let addresses = result.as_ref().map_err(Clone::clone)?;
        let address = addresses.get(state.cursor).copied();
        if address.is_some() {
            state.cursor += 1;
        }
        Ok(address)
    }
}

impl TcpSocket {
    pub(super) fn new(service: ComponentHostNetworkService, family: WasiTcpSocketFamily) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TcpSocketState {
                service,
                family,
                stream: None,
                listener: None,
                local_address: None,
                remote_address: None,
                receive_shutdown: false,
                send_shutdown: false,
                connect_in_progress: false,
                connect_result: None,
                listen_in_progress: false,
                listen_result: None,
                listen_backlog: DEFAULT_WASI_TCP_LISTEN_BACKLOG,
                accept_in_progress: false,
                accept_result: None,
                keep_alive_enabled: false,
                keep_alive_idle_time: DEFAULT_WASI_TCP_KEEP_ALIVE_IDLE_NANOS,
                keep_alive_interval: DEFAULT_WASI_TCP_KEEP_ALIVE_INTERVAL_NANOS,
                keep_alive_count: DEFAULT_WASI_TCP_KEEP_ALIVE_COUNT,
                hop_limit: DEFAULT_WASI_TCP_HOP_LIMIT,
                receive_buffer_size: WASI_TCP_RECEIVE_BUFFER_BYTES,
                send_buffer_size: WASI_TCP_SEND_BUFFER_BYTES,
            })),
            ready: Arc::new(crate::Notify::new()),
        }
    }

    pub(super) fn accepted(
        service: ComponentHostNetworkService,
        family: WasiTcpSocketFamily,
        stream: u64,
        local_address: WasiTcpSocketAddress,
        remote_address: WasiTcpSocketAddress,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TcpSocketState {
                service,
                family,
                stream: Some(stream),
                listener: None,
                local_address: Some(local_address),
                remote_address: Some(remote_address),
                receive_shutdown: false,
                send_shutdown: false,
                connect_in_progress: false,
                connect_result: None,
                listen_in_progress: false,
                listen_result: None,
                listen_backlog: DEFAULT_WASI_TCP_LISTEN_BACKLOG,
                accept_in_progress: false,
                accept_result: None,
                keep_alive_enabled: false,
                keep_alive_idle_time: DEFAULT_WASI_TCP_KEEP_ALIVE_IDLE_NANOS,
                keep_alive_interval: DEFAULT_WASI_TCP_KEEP_ALIVE_INTERVAL_NANOS,
                keep_alive_count: DEFAULT_WASI_TCP_KEEP_ALIVE_COUNT,
                hop_limit: DEFAULT_WASI_TCP_HOP_LIMIT,
                receive_buffer_size: WASI_TCP_RECEIVE_BUFFER_BYTES,
                send_buffer_size: WASI_TCP_SEND_BUFFER_BYTES,
            })),
            ready: Arc::new(crate::Notify::new()),
        }
    }

    pub(super) fn family(&self) -> WasiTcpSocketFamily {
        self.inner.lock().family
    }

    pub(super) fn address_family(&self) -> socket_types::IpAddressFamily {
        match self.family() {
            WasiTcpSocketFamily::Ipv4 => socket_types::IpAddressFamily::Ipv4,
            WasiTcpSocketFamily::Ipv6 => socket_types::IpAddressFamily::Ipv6,
        }
    }

    pub(super) fn remote_address(
        &self,
    ) -> core::result::Result<WasiTcpSocketAddress, socket_types::ErrorCode> {
        self.inner
            .lock()
            .remote_address
            .ok_or(socket_types::ErrorCode::InvalidState)
    }

    pub(super) fn local_address(
        &self,
    ) -> core::result::Result<WasiTcpSocketAddress, socket_types::ErrorCode> {
        self.inner
            .lock()
            .local_address
            .ok_or(socket_types::ErrorCode::InvalidState)
    }

    pub(super) fn bind_local(
        &self,
        local_address: WasiTcpSocketAddress,
    ) -> core::result::Result<(), socket_types::ErrorCode> {
        let mut state = self.inner.lock();
        if state.stream.is_some()
            || state.listener.is_some()
            || state.connect_in_progress
            || state.listen_in_progress
            || state.accept_in_progress
        {
            return Err(socket_types::ErrorCode::InvalidState);
        }
        state.local_address = Some(local_address);
        Ok(())
    }

    pub(super) fn is_listening(&self) -> bool {
        self.inner.lock().listener.is_some()
    }

    #[cfg(test)]
    pub(super) fn listen_backlog(&self) -> u16 {
        self.inner.lock().listen_backlog
    }

    pub(super) fn set_listen_backlog_size(
        &self,
        value: u64,
    ) -> core::result::Result<(), socket_types::ErrorCode> {
        let backlog = u16::try_from(value).map_err(|_| socket_types::ErrorCode::InvalidArgument)?;
        if backlog == 0 {
            return Err(socket_types::ErrorCode::InvalidArgument);
        }
        let mut state = self.inner.lock();
        if state.stream.is_some()
            || state.connect_in_progress
            || state.connect_result.is_some()
            || state.listener.is_some()
            || state.listen_in_progress
            || state.listen_result.is_some()
            || state.accept_in_progress
        {
            return Err(socket_types::ErrorCode::InvalidState);
        }
        state.listen_backlog = backlog;
        Ok(())
    }

    pub(super) fn keep_alive_enabled(&self) -> core::result::Result<bool, socket_types::ErrorCode> {
        Ok(self.inner.lock().keep_alive_enabled)
    }

    /// Enabling keepalive is rejected rather than recorded.
    ///
    /// The netstack has no keepalive timer, so accepting `true` would make a
    /// guest believe idle connections are being probed and dead peers
    /// detected when neither happens. Disabling it is the state the stack is
    /// already in, so that direction succeeds.
    pub(super) fn set_keep_alive_enabled(
        &self,
        value: bool,
    ) -> core::result::Result<(), socket_types::ErrorCode> {
        if value {
            return Err(socket_types::ErrorCode::NotSupported);
        }
        self.inner.lock().keep_alive_enabled = value;
        Ok(())
    }

    pub(super) fn keep_alive_idle_time(
        &self,
    ) -> core::result::Result<u64, socket_types::ErrorCode> {
        Ok(self.inner.lock().keep_alive_idle_time)
    }

    pub(super) fn set_keep_alive_idle_time(
        &self,
        value: u64,
    ) -> core::result::Result<(), socket_types::ErrorCode> {
        if value == 0 {
            return Err(socket_types::ErrorCode::InvalidArgument);
        }
        self.inner.lock().keep_alive_idle_time = value;
        Ok(())
    }

    pub(super) fn keep_alive_interval(&self) -> core::result::Result<u64, socket_types::ErrorCode> {
        Ok(self.inner.lock().keep_alive_interval)
    }

    pub(super) fn set_keep_alive_interval(
        &self,
        value: u64,
    ) -> core::result::Result<(), socket_types::ErrorCode> {
        if value == 0 {
            return Err(socket_types::ErrorCode::InvalidArgument);
        }
        self.inner.lock().keep_alive_interval = value;
        Ok(())
    }

    pub(super) fn keep_alive_count(&self) -> core::result::Result<u32, socket_types::ErrorCode> {
        Ok(self.inner.lock().keep_alive_count)
    }

    pub(super) fn set_keep_alive_count(
        &self,
        value: u32,
    ) -> core::result::Result<(), socket_types::ErrorCode> {
        if value == 0 {
            return Err(socket_types::ErrorCode::InvalidArgument);
        }
        self.inner.lock().keep_alive_count = value;
        Ok(())
    }

    pub(super) fn receive_buffer_size(&self) -> core::result::Result<u64, socket_types::ErrorCode> {
        Ok(self.inner.lock().receive_buffer_size)
    }

    /// Records a receive-buffer request clamped to what the stack reserves.
    ///
    /// `wasi:sockets` allows an implementation to clamp this hint, and reading
    /// the option back is documented to return the effective value. The
    /// netstack's per-socket receive window is fixed, so the effective value
    /// is that window and the getter reports it instead of echoing whatever
    /// the guest asked for.
    pub(super) fn set_receive_buffer_size(
        &self,
        value: u64,
    ) -> core::result::Result<(), socket_types::ErrorCode> {
        if value == 0 {
            return Err(socket_types::ErrorCode::InvalidArgument);
        }
        self.inner.lock().receive_buffer_size = value.min(WASI_TCP_RECEIVE_BUFFER_BYTES);
        Ok(())
    }

    pub(super) fn send_buffer_size(&self) -> core::result::Result<u64, socket_types::ErrorCode> {
        Ok(self.inner.lock().send_buffer_size)
    }

    /// Records a send-buffer request clamped to the stack's transmit buffer.
    pub(super) fn set_send_buffer_size(
        &self,
        value: u64,
    ) -> core::result::Result<(), socket_types::ErrorCode> {
        if value == 0 {
            return Err(socket_types::ErrorCode::InvalidArgument);
        }
        self.inner.lock().send_buffer_size = value.min(WASI_TCP_SEND_BUFFER_BYTES);
        Ok(())
    }

    pub(super) fn hop_limit(&self) -> core::result::Result<u8, socket_types::ErrorCode> {
        Ok(self.inner.lock().hop_limit)
    }

    /// Sets the IPv4 TTL / IPv6 hop limit and pushes it into the netstack.
    ///
    /// An unbound socket has nothing to push to yet, so the value is held on
    /// the descriptor and handed to `connect`/`listen`, which apply it before
    /// the first packet leaves. Once a stream or listener exists the change
    /// takes effect on the live socket.
    pub(super) fn set_hop_limit(
        &self,
        value: u8,
    ) -> core::result::Result<(), socket_types::ErrorCode> {
        if value == 0 {
            return Err(socket_types::ErrorCode::InvalidArgument);
        }
        let (service, stream, listener) = {
            let mut state = self.inner.lock();
            state.hop_limit = value;
            (state.service.clone(), state.stream, state.listener)
        };
        if let Some(stream) = stream {
            service
                .tcp_set_hop_limit(stream, value)
                .map_err(map_p3_tcp_error)?;
        }
        if let Some(listener) = listener {
            service
                .tcp_listener_set_hop_limit(listener, value)
                .map_err(map_p3_tcp_error)?;
        }
        Ok(())
    }

    pub(super) async fn connect(
        &self,
        remote_address: WasiTcpSocketAddress,
    ) -> core::result::Result<(), socket_types::ErrorCode> {
        let (service, local_port, hop_limit) = {
            let state = self.inner.lock();
            if state.stream.is_some() {
                return Err(socket_types::ErrorCode::InvalidState);
            }
            (
                state.service.clone(),
                state.local_address.map_or(0, |address| address.port),
                state.hop_limit,
            )
        };
        let stream = service
            .tcp_connect_address(
                remote_address.address.network_address(),
                remote_address.port,
                local_port,
                hop_limit,
                u64::MAX,
            )
            .await
            .map_err(map_p3_tcp_error)?;
        let mut state = self.inner.lock();
        state.stream = Some(stream);
        state.remote_address = Some(remote_address);
        state.receive_shutdown = false;
        state.send_shutdown = false;
        Ok(())
    }

    pub(super) async fn write_all_bytes(
        &self,
        bytes: Bytes,
    ) -> core::result::Result<(), socket_types::ErrorCode> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.inner.lock().send_shutdown {
            return Err(socket_types::ErrorCode::InvalidState);
        }
        let (service, stream) = self.connected_stream()?;
        service
            .tcp_write_all_bytes(stream, bytes, u64::MAX)
            .await
            .map_err(map_p3_tcp_error)
    }

    pub(super) async fn read(
        &self,
        max_bytes: u32,
    ) -> core::result::Result<Option<Bytes>, socket_types::ErrorCode> {
        if self.inner.lock().receive_shutdown {
            return Ok(None);
        }
        let (service, stream) = self.connected_stream()?;
        service
            .tcp_read(stream, max_bytes, u64::MAX)
            .await
            .map_err(map_p3_tcp_error)
    }

    pub(super) fn connected_stream(
        &self,
    ) -> core::result::Result<(ComponentHostNetworkService, u64), socket_types::ErrorCode> {
        let state = self.inner.lock();
        let stream = state.stream.ok_or(socket_types::ErrorCode::InvalidState)?;
        Ok((state.service.clone(), stream))
    }

    pub(super) fn take_connected_stream(&self) -> Option<(ComponentHostNetworkService, u64)> {
        let mut state = self.inner.lock();
        state
            .stream
            .take()
            .map(|stream| (state.service.clone(), stream))
    }

    pub(super) fn shutdown_receive(&self) -> core::result::Result<(), socket_types::ErrorCode> {
        let mut state = self.inner.lock();
        if state.stream.is_none() {
            return Err(socket_types::ErrorCode::InvalidState);
        }
        state.receive_shutdown = true;
        Ok(())
    }

    pub(super) fn shutdown_send_state(
        &self,
    ) -> core::result::Result<(ComponentHostNetworkService, u64), socket_types::ErrorCode> {
        let mut state = self.inner.lock();
        let stream = state.stream.ok_or(socket_types::ErrorCode::InvalidState)?;
        if state.send_shutdown {
            return Ok((state.service.clone(), stream));
        }
        state.send_shutdown = true;
        Ok((state.service.clone(), stream))
    }
}

impl<T, CpuImpl, HostFs> TcpListenStreamProducer<T, CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    pub(super) fn new(
        socket: TcpSocket,
        get_store_data: for<'a> fn(&'a mut T) -> &'a mut StoreData<CpuImpl, HostFs>,
        spawner: crate::Spawner<CpuImpl>,
    ) -> Self {
        let waiter = socket.ready.waiter();
        Self {
            socket,
            get_store_data,
            spawner,
            waiter,
        }
    }
}

impl<T, CpuImpl, HostFs> StreamProducer<T> for TcpListenStreamProducer<T, CpuImpl, HostFs>
where
    T: 'static,
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    type Item = Resource<TcpSocket>;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, T>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        if destination.remaining(&mut store) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let this = self.as_mut().get_mut();

        loop {
            let mut start_accept = None;
            let mut accepted_socket = None;
            let mut wait_for_socket = false;
            {
                let mut state = this.socket.inner.lock();
                if let Some(result) = state.listen_result.take() {
                    match result {
                        Ok(listener) => {
                            state.listener = Some(listener.listener);
                            let family = state.family;
                            state.local_address.get_or_insert(WasiTcpSocketAddress {
                                address: family.unspecified_address(),
                                port: listener.local_port,
                            });
                        }
                        Err(error) => {
                            return Poll::Ready(Err(wasmtime::Error::new(
                                P3TcpListenStreamError {
                                    operation: P3TcpListenStreamOperation::Listen,
                                    source: Some(error),
                                },
                            )));
                        }
                    }
                }

                if state.listen_in_progress {
                    wait_for_socket = true;
                } else if let Some(result) = state.accept_result.take() {
                    match result {
                        Ok(accepted) => {
                            let Some(local_address) = state.local_address else {
                                return Poll::Ready(Err(wasmtime::Error::new(
                                    P3TcpListenStreamError {
                                        operation: P3TcpListenStreamOperation::LocalAddress,
                                        source: None,
                                    },
                                )));
                            };
                            let remote_address = match accepted.address {
                                crate::NetworkIpAddress::Ipv4(address) => {
                                    WasiTcpIpAddress::Ipv4(address)
                                }
                                crate::NetworkIpAddress::Ipv6(address) => {
                                    WasiTcpIpAddress::Ipv6(address)
                                }
                            };
                            assert_eq!(
                                remote_address.family(),
                                state.family,
                                "tcp accept returned a peer address for the wrong socket family"
                            );
                            accepted_socket = Some(TcpSocket::accepted(
                                state.service.clone(),
                                state.family,
                                accepted.stream,
                                local_address,
                                WasiTcpSocketAddress {
                                    address: remote_address,
                                    port: accepted.port,
                                },
                            ));
                        }
                        Err(error) => {
                            return Poll::Ready(Err(wasmtime::Error::new(
                                P3TcpListenStreamError {
                                    operation: P3TcpListenStreamOperation::Accept,
                                    source: Some(error),
                                },
                            )));
                        }
                    }
                } else if state.accept_in_progress {
                    wait_for_socket = true;
                } else if let Some(listener) = state.listener {
                    state.accept_in_progress = true;
                    start_accept = Some((
                        state.service.clone(),
                        this.socket.inner.clone(),
                        this.socket.ready.clone(),
                        listener,
                    ));
                } else {
                    return Poll::Ready(Err(wasmtime::Error::new(P3TcpListenStreamError {
                        operation: P3TcpListenStreamOperation::InvalidState,
                        source: None,
                    })));
                }
            }

            if let Some(accepted_socket) = accepted_socket {
                let store_data = (this.get_store_data)(store.data_mut());
                let resource = store_data.table.push(accepted_socket)?;
                destination.set_buffer(Some(resource));
                return Poll::Ready(Ok(StreamResult::Completed));
            }

            if let Some((service, inner, ready, listener)) = start_accept {
                this.spawner.spawn_detached(async move {
                    let result = service.tcp_accept(listener, u64::MAX).await;
                    let mut state = inner.lock();
                    state.accept_in_progress = false;
                    state.accept_result = Some(result);
                    ready.notify_all();
                });
                wait_for_socket = true;
            }

            if wait_for_socket {
                match this.socket.ready.poll_notified(cx, &mut this.waiter) {
                    Poll::Ready(()) => continue,
                    Poll::Pending => return Poll::Pending,
                }
            }
        }
    }
}

impl TcpReadStreamProducer {
    pub(super) fn new(
        socket: TcpSocket,
        completion: oneshot::Sender<core::result::Result<(), socket_types::ErrorCode>>,
    ) -> Self {
        Self {
            socket,
            pending: None,
            completion: Some(completion),
        }
    }

    pub(super) fn complete(&mut self, result: core::result::Result<(), socket_types::ErrorCode>) {
        if let Some(tx) = self.completion.take() {
            let _ = tx.send(result);
        }
    }
}

impl Drop for TcpReadStreamProducer {
    fn drop(&mut self) {
        self.complete(Ok(()));
    }
}

impl<T: 'static> StreamProducer<T> for TcpReadStreamProducer {
    type Item = u8;
    type Buffer = BytesStreamBuffer;

    fn poll_produce(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut destination: Destination<'_, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<Result<StreamResult>> {
        if finish {
            self.complete(Ok(()));
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }

        loop {
            if self.pending.is_none() {
                let capacity = destination.remaining(&mut store);
                if capacity == Some(0) {
                    return Poll::Ready(Ok(StreamResult::Completed));
                }
                let socket = self.socket.clone();
                let capacity = capacity.unwrap_or(FILE_READ_CHUNK_BYTES);
                let max_bytes = u32::try_from(capacity).unwrap_or(u32::MAX);
                self.pending = Some(Box::pin(async move { socket.read(max_bytes).await }));
            }

            let pending = self
                .pending
                .as_mut()
                .expect("tcp read future must be present before polling");
            match pending.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(Some(bytes))) if bytes.is_empty() => {
                    self.pending = None;
                    continue;
                }
                Poll::Ready(Ok(Some(bytes))) => {
                    self.pending = None;
                    destination.set_buffer(BytesStreamBuffer::new(bytes));
                    return Poll::Ready(Ok(StreamResult::Completed));
                }
                Poll::Ready(Ok(None)) => {
                    self.pending = None;
                    self.complete(Ok(()));
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
                Poll::Ready(Err(error)) => {
                    self.pending = None;
                    self.complete(Err(error));
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
            }
        }
    }
}

impl TcpWriteConsumer {
    pub(super) fn new(
        socket: TcpSocket,
        completion: oneshot::Sender<core::result::Result<(), socket_types::ErrorCode>>,
    ) -> Self {
        Self {
            socket,
            pending: None,
            completion: Some(completion),
        }
    }

    pub(super) fn complete(&mut self, result: core::result::Result<(), socket_types::ErrorCode>) {
        if let Some(tx) = self.completion.take() {
            let _ = tx.send(result);
        }
    }
}

impl Drop for TcpWriteConsumer {
    fn drop(&mut self) {
        self.complete(Ok(()));
    }
}

impl<T: 'static> StreamConsumer<T> for TcpWriteConsumer {
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        loop {
            if self.pending.is_none() {
                let available = source.remaining(&mut store);
                if available == 0 {
                    self.complete(Ok(()));
                    return Poll::Ready(Ok(StreamResult::Completed));
                }
                let mut bytes = Vec::with_capacity(available);
                source.read(&mut store, &mut bytes)?;
                let socket = self.socket.clone();
                self.pending = Some(Box::pin(async move {
                    socket.write_all_bytes(Bytes::from(bytes)).await
                }));
            }

            let pending = self
                .pending
                .as_mut()
                .expect("tcp write future must be present before polling");
            match pending.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(())) => {
                    self.pending = None;
                    continue;
                }
                Poll::Ready(Err(error)) => {
                    self.pending = None;
                    self.complete(Err(error));
                    return Poll::Ready(Ok(StreamResult::Dropped));
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WasiUdpSocketFamily {
    Ipv4,
    Ipv6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WasiUdpSocketAddress {
    pub(super) address: crate::NetworkIpAddress,
    pub(super) port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BoundUdpSocket {
    pub(super) socket: u64,
    pub(super) local_port: u16,
}

pub(super) struct UdpSocketState {
    pub(super) service: ComponentHostNetworkService,
    pub(super) family: WasiUdpSocketFamily,
    pub(super) bound: Option<BoundUdpSocket>,
    pub(super) pending_bind: Option<BoundUdpSocket>,
    pub(super) remote_address: Option<WasiUdpSocketAddress>,
    pub(super) hop_limit: u8,
    pub(super) receive_buffer_size: u64,
    pub(super) send_buffer_size: u64,
    pub(super) open_stream_handles: usize,
}

#[derive(Debug)]
pub(super) enum WasiUdpSocketError {
    NotSupported,
    InvalidArgument,
    InvalidState,
    NotInProgress,
    AddressNotBindable,
    DatagramTooLarge,
    WouldBlock,
    Backend(crate::UdpError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WasiUdpSocketDatagram {
    pub(super) bytes: Bytes,
    pub(super) remote_address: WasiUdpSocketAddress,
}

impl UdpSocket {
    pub(super) fn new(service: ComponentHostNetworkService, family: WasiUdpSocketFamily) -> Self {
        Self {
            inner: Arc::new(Mutex::new(UdpSocketState {
                service,
                family,
                bound: None,
                pending_bind: None,
                remote_address: None,
                hop_limit: DEFAULT_WASI_UDP_HOP_LIMIT,
                receive_buffer_size: DEFAULT_WASI_UDP_BUFFER_BYTES,
                send_buffer_size: DEFAULT_WASI_UDP_BUFFER_BYTES,
                open_stream_handles: 0,
            })),
        }
    }

    pub(super) fn family(&self) -> WasiUdpSocketFamily {
        self.inner.lock().family
    }

    pub(super) fn local_address(
        &self,
    ) -> core::result::Result<WasiUdpSocketAddress, WasiUdpSocketError> {
        let state = self.inner.lock();
        let Some(bound) = state.bound else {
            return Err(WasiUdpSocketError::InvalidState);
        };
        Ok(WasiUdpSocketAddress {
            address: match state.family {
                WasiUdpSocketFamily::Ipv4 => {
                    crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([0, 0, 0, 0]))
                }
                WasiUdpSocketFamily::Ipv6 => {
                    crate::NetworkIpAddress::Ipv6(Ipv6Address::UNSPECIFIED)
                }
            },
            port: bound.local_port,
        })
    }

    pub(super) fn remote_address(
        &self,
    ) -> core::result::Result<WasiUdpSocketAddress, WasiUdpSocketError> {
        self.inner
            .lock()
            .remote_address
            .ok_or(WasiUdpSocketError::InvalidState)
    }

    pub(super) fn unicast_hop_limit(&self) -> core::result::Result<u8, WasiUdpSocketError> {
        Ok(self.inner.lock().hop_limit)
    }

    /// Sets the datagram TTL / hop limit and pushes it into the netstack.
    ///
    /// An unbound socket has no stack socket yet, so the value is applied when
    /// the bind completes; a bound socket takes it immediately.
    pub(super) fn set_unicast_hop_limit(
        &self,
        value: u8,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        if value == 0 {
            return Err(WasiUdpSocketError::InvalidArgument);
        }
        let (service, bound) = {
            let mut state = self.inner.lock();
            state.hop_limit = value;
            (state.service.clone(), state.bound.or(state.pending_bind))
        };
        if let Some(bound) = bound {
            service
                .udp_set_hop_limit(bound.socket, value)
                .map_err(WasiUdpSocketError::Backend)?;
        }
        Ok(())
    }

    pub(super) fn receive_buffer_size(&self) -> core::result::Result<u64, WasiUdpSocketError> {
        Ok(self.inner.lock().receive_buffer_size)
    }

    pub(super) fn set_receive_buffer_size(
        &self,
        value: u64,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        if value == 0 {
            return Err(WasiUdpSocketError::InvalidArgument);
        }
        self.inner.lock().receive_buffer_size = value;
        Ok(())
    }

    pub(super) fn send_buffer_size(&self) -> core::result::Result<u64, WasiUdpSocketError> {
        Ok(self.inner.lock().send_buffer_size)
    }

    pub(super) fn set_send_buffer_size(
        &self,
        value: u64,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        if value == 0 {
            return Err(WasiUdpSocketError::InvalidArgument);
        }
        self.inner.lock().send_buffer_size = value;
        Ok(())
    }

    pub(super) async fn bind(
        &self,
        local_address: WasiUdpSocketAddress,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        validate_udp_local_address(self.family(), local_address)?;
        self.bind_backend(local_address.port, false).await
    }

    pub(super) async fn start_bind_p2(
        &self,
        local_address: WasiUdpSocketAddress,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        validate_udp_local_address(self.family(), local_address)?;
        self.bind_backend(local_address.port, true).await
    }

    pub(super) fn finish_bind_p2(&self) -> core::result::Result<(), WasiUdpSocketError> {
        let mut state = self.inner.lock();
        let Some(bound) = state.pending_bind.take() else {
            return Err(WasiUdpSocketError::NotInProgress);
        };
        assert!(
            state.bound.is_none(),
            "udp socket completed a p2 bind while already bound"
        );
        state.bound = Some(bound);
        Ok(())
    }

    pub(super) async fn connect(
        &self,
        remote_address: WasiUdpSocketAddress,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        validate_udp_remote_address(self.family(), remote_address)?;
        let bound = self.ensure_bound(0).await?;
        let service = self.inner.lock().service.clone();
        service
            .udp_connect(bound.socket, remote_address.address, remote_address.port)
            .map_err(WasiUdpSocketError::Backend)?;
        let mut state = self.inner.lock();
        assert_eq!(
            state.bound,
            Some(bound),
            "udp socket binding changed during connect"
        );
        state.remote_address = Some(remote_address);
        Ok(())
    }

    pub(super) fn disconnect(&self) -> core::result::Result<(), WasiUdpSocketError> {
        let mut state = self.inner.lock();
        if state.remote_address.is_none() {
            return Err(WasiUdpSocketError::InvalidState);
        }
        let bound = state.bound.ok_or(WasiUdpSocketError::InvalidState)?;
        let service = state.service.clone();
        service
            .udp_disconnect(bound.socket)
            .map_err(WasiUdpSocketError::Backend)?;
        state.remote_address = None;
        Ok(())
    }

    pub(super) fn open_p2_streams(
        &self,
        remote_address: Option<WasiUdpSocketAddress>,
    ) -> core::result::Result<
        (P2IncomingDatagramStream, P2OutgoingDatagramStream),
        WasiUdpSocketError,
    > {
        if let Some(remote_address) = remote_address {
            validate_udp_remote_address(self.family(), remote_address)?;
        }
        let bound = {
            let state = self.inner.lock();
            if state.pending_bind.is_some() || state.bound.is_none() {
                return Err(WasiUdpSocketError::InvalidState);
            }
            if state.open_stream_handles != 0 {
                return Err(WasiUdpSocketError::InvalidState);
            }
            state.bound.expect("UDP socket bound state checked above")
        };
        let service = self.inner.lock().service.clone();
        if let Some(remote_address) = remote_address {
            service
                .udp_connect(bound.socket, remote_address.address, remote_address.port)
                .map_err(WasiUdpSocketError::Backend)?;
        } else if self.inner.lock().remote_address.is_some() {
            service
                .udp_disconnect(bound.socket)
                .map_err(WasiUdpSocketError::Backend)?;
        }
        let mut state = self.inner.lock();
        assert_eq!(
            state.bound,
            Some(bound),
            "udp socket binding changed while opening streams"
        );
        state.remote_address = remote_address;
        state.open_stream_handles = 2;
        let incoming = P2IncomingDatagramStream {
            socket: self.clone(),
            remote_address,
        };
        let outgoing = P2OutgoingDatagramStream {
            socket: self.clone(),
            remote_address,
            check_send_permit_count: 0,
        };
        Ok((incoming, outgoing))
    }

    pub(super) fn release_stream_handle(&self) {
        let mut state = self.inner.lock();
        if state.open_stream_handles == 0 {
            return;
        }
        state.open_stream_handles -= 1;
        if state.open_stream_handles == 0
            && let (Some(bound), Some(_)) = (state.bound, state.remote_address)
        {
            let service = state.service.clone();
            service
                .udp_disconnect(bound.socket)
                .unwrap_or_else(|error| panic!("failed to disconnect closed UDP streams: {error}"));
            state.remote_address = None;
        }
    }

    pub(super) async fn send_datagram(
        &self,
        bytes: &[u8],
        remote_address: Option<WasiUdpSocketAddress>,
        timeout_nanos: u64,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        if bytes.len() > MAX_WASI_UDP_DATAGRAM_BYTES {
            return Err(WasiUdpSocketError::DatagramTooLarge);
        }
        let target = {
            let state = self.inner.lock();
            resolve_udp_send_target(&state, remote_address)?
        };
        let bound = self.ensure_bound(0).await?;
        let service = self.inner.lock().service.clone();
        let written = service
            .udp_send_address(
                bound.socket,
                target.address,
                target.port,
                bytes,
                timeout_nanos,
            )
            .await
            .map_err(WasiUdpSocketError::Backend)?;
        assert!(
            usize::try_from(written).ok() == Some(bytes.len()),
            "kernel udp service reported a partial datagram write"
        );
        Ok(())
    }

    pub(super) async fn receive_datagram(
        &self,
        remote_address: Option<WasiUdpSocketAddress>,
        timeout_nanos: u64,
    ) -> core::result::Result<WasiUdpSocketDatagram, WasiUdpSocketError> {
        let filter = {
            let state = self.inner.lock();
            if state.pending_bind.is_some() || state.bound.is_none() {
                return Err(WasiUdpSocketError::InvalidState);
            }
            remote_address.or(state.remote_address)
        };
        let bound = self
            .inner
            .lock()
            .bound
            .ok_or(WasiUdpSocketError::InvalidState)?;
        let service = self.inner.lock().service.clone();
        loop {
            let datagram = service
                .udp_receive(
                    bound.socket,
                    MAX_WASI_UDP_DATAGRAM_BYTES as u32,
                    timeout_nanos,
                )
                .await
                .map_err(|error| {
                    if timeout_nanos == 0 && matches!(error.kind, crate::UdpErrorKind::Timeout) {
                        WasiUdpSocketError::WouldBlock
                    } else {
                        WasiUdpSocketError::Backend(error)
                    }
                })?
                .ok_or(WasiUdpSocketError::InvalidState)?;
            let remote = WasiUdpSocketAddress {
                address: datagram.address,
                port: datagram.port,
            };
            if !udp_address_matches_family(self.family(), remote.address) {
                continue;
            }
            if filter.is_some_and(|expected| expected != remote) {
                continue;
            }
            return Ok(WasiUdpSocketDatagram {
                bytes: datagram.bytes,
                remote_address: remote,
            });
        }
    }

    pub(super) async fn bind_backend(
        &self,
        local_port: u16,
        pending_bind: bool,
    ) -> core::result::Result<(), WasiUdpSocketError> {
        let (service, hop_limit) = {
            let state = self.inner.lock();
            if state.pending_bind.is_some() || state.bound.is_some() {
                return Err(WasiUdpSocketError::InvalidState);
            }
            (state.service.clone(), state.hop_limit)
        };
        let binding = service
            .udp_bind(local_port)
            .await
            .map_err(WasiUdpSocketError::Backend)?;
        // A datagram socket cannot emit anything before its bind completes,
        // so applying the descriptor's hop limit here covers every packet it
        // will ever send.
        service
            .udp_set_hop_limit(binding.socket, hop_limit)
            .map_err(WasiUdpSocketError::Backend)?;
        let bound = BoundUdpSocket {
            socket: binding.socket,
            local_port: binding.local_port,
        };
        let mut state = self.inner.lock();
        assert!(
            state.pending_bind.is_none() && state.bound.is_none(),
            "udp bind raced with a concurrent state transition"
        );
        if pending_bind {
            state.pending_bind = Some(bound);
        } else {
            state.bound = Some(bound);
        }
        Ok(())
    }

    pub(super) async fn ensure_bound(
        &self,
        local_port: u16,
    ) -> core::result::Result<BoundUdpSocket, WasiUdpSocketError> {
        {
            let state = self.inner.lock();
            if let Some(bound) = state.bound {
                return Ok(bound);
            }
            if state.pending_bind.is_some() {
                return Err(WasiUdpSocketError::InvalidState);
            }
        }
        self.bind_backend(local_port, false).await?;
        self.inner
            .lock()
            .bound
            .ok_or(WasiUdpSocketError::InvalidState)
    }
}

pub(super) fn resolve_udp_send_target(
    state: &UdpSocketState,
    remote_address: Option<WasiUdpSocketAddress>,
) -> core::result::Result<WasiUdpSocketAddress, WasiUdpSocketError> {
    match (state.remote_address, remote_address) {
        (Some(connected), None) => Ok(connected),
        (Some(connected), Some(provided)) if connected == provided => Ok(connected),
        (None, Some(provided)) => {
            validate_udp_remote_address(state.family, provided)?;
            Ok(provided)
        }
        _ => Err(WasiUdpSocketError::InvalidArgument),
    }
}

pub(super) fn validate_udp_local_address(
    family: WasiUdpSocketFamily,
    local_address: WasiUdpSocketAddress,
) -> core::result::Result<(), WasiUdpSocketError> {
    if !udp_address_matches_family(family, local_address.address) {
        return Err(WasiUdpSocketError::NotSupported);
    }
    if !network_ip_address_is_unspecified(local_address.address) {
        return Err(WasiUdpSocketError::AddressNotBindable);
    }
    Ok(())
}

pub(super) fn validate_udp_remote_address(
    family: WasiUdpSocketFamily,
    remote_address: WasiUdpSocketAddress,
) -> core::result::Result<(), WasiUdpSocketError> {
    if !udp_address_matches_family(family, remote_address.address) {
        return Err(WasiUdpSocketError::NotSupported);
    }
    if network_ip_address_is_unspecified(remote_address.address) || remote_address.port == 0 {
        return Err(WasiUdpSocketError::InvalidArgument);
    }
    Ok(())
}

pub(super) fn udp_address_matches_family(
    family: WasiUdpSocketFamily,
    address: crate::NetworkIpAddress,
) -> bool {
    matches!(
        (family, address),
        (WasiUdpSocketFamily::Ipv4, crate::NetworkIpAddress::Ipv4(_))
            | (WasiUdpSocketFamily::Ipv6, crate::NetworkIpAddress::Ipv6(_))
    )
}

pub(super) fn network_ip_address_is_unspecified(address: crate::NetworkIpAddress) -> bool {
    match address {
        crate::NetworkIpAddress::Ipv4(address) => address.octets() == [0, 0, 0, 0],
        crate::NetworkIpAddress::Ipv6(address) => address.is_unspecified(),
    }
}

impl<CpuImpl, HostFs> wasi::sockets::types::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}
impl<CpuImpl, HostFs> wasi::sockets::types::HostTcpSocket for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn bind(
        &mut self,
        socket: Resource<TcpSocket>,
        local_address: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        let local_address = match parse_p3_tcp_socket_address(local_address, socket.family()) {
            Ok(address) => address,
            Err(error) => return Ok(Err(error)),
        };
        if !has_wasi_network_rights(
            self.process_authority(),
            wasi_tcp_bind_rights(local_address.port),
        ) {
            return Ok(Err(socket_types::ErrorCode::AccessDenied));
        }
        Ok(socket.bind_local(local_address))
    }

    fn create(
        &mut self,
        address_family: socket_types::IpAddressFamily,
    ) -> Result<core::result::Result<Resource<TcpSocket>, socket_types::ErrorCode>> {
        if !has_wasi_network_rights(self.process_authority(), crate::NetworkAuthorityRights::TCP) {
            return Ok(Err(socket_types::ErrorCode::AccessDenied));
        }
        let family = match address_family {
            socket_types::IpAddressFamily::Ipv4 => WasiTcpSocketFamily::Ipv4,
            socket_types::IpAddressFamily::Ipv6 => WasiTcpSocketFamily::Ipv6,
        };
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(socket_types::ErrorCode::Other(None)));
        };
        let resource = self.table.push(TcpSocket::new(service, family))?;
        Ok(Ok(resource))
    }

    fn get_local_address(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.local_address().map(format_p3_tcp_socket_address))
    }

    fn get_remote_address(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.remote_address().map(format_p3_tcp_socket_address))
    }

    fn get_is_listening(&mut self, socket: Resource<TcpSocket>) -> Result<bool> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.is_listening())
    }

    fn get_address_family(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<socket_types::IpAddressFamily> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.address_family())
    }

    fn set_listen_backlog_size(
        &mut self,
        socket: Resource<TcpSocket>,
        value: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.set_listen_backlog_size(value))
    }

    fn get_keep_alive_enabled(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<bool, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.keep_alive_enabled())
    }

    fn set_keep_alive_enabled(
        &mut self,
        socket: Resource<TcpSocket>,
        value: bool,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.set_keep_alive_enabled(value))
    }

    fn get_keep_alive_idle_time(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<wasi::clocks::types::Duration, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.keep_alive_idle_time())
    }

    fn set_keep_alive_idle_time(
        &mut self,
        socket: Resource<TcpSocket>,
        value: wasi::clocks::types::Duration,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.set_keep_alive_idle_time(value))
    }

    fn get_keep_alive_interval(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<wasi::clocks::types::Duration, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.keep_alive_interval())
    }

    fn set_keep_alive_interval(
        &mut self,
        socket: Resource<TcpSocket>,
        value: wasi::clocks::types::Duration,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.set_keep_alive_interval(value))
    }

    fn get_keep_alive_count(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u32, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.keep_alive_count())
    }

    fn set_keep_alive_count(
        &mut self,
        socket: Resource<TcpSocket>,
        value: u32,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.set_keep_alive_count(value))
    }

    fn get_hop_limit(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u8, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.hop_limit())
    }

    fn set_hop_limit(
        &mut self,
        socket: Resource<TcpSocket>,
        value: u8,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.set_hop_limit(value))
    }

    fn get_receive_buffer_size(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.receive_buffer_size())
    }

    fn set_receive_buffer_size(
        &mut self,
        socket: Resource<TcpSocket>,
        value: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.set_receive_buffer_size(value))
    }

    fn get_send_buffer_size(
        &mut self,
        socket: Resource<TcpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.send_buffer_size())
    }

    fn set_send_buffer_size(
        &mut self,
        socket: Resource<TcpSocket>,
        value: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.set_send_buffer_size(value))
    }

    fn drop(&mut self, resource: Resource<TcpSocket>) -> Result<()> {
        let socket = self.table.delete(resource)?;
        if let Some((service, stream)) = socket.take_connected_stream() {
            self.spawner().spawn_detached(async move {
                service.tcp_close(stream).await;
            });
        }
        Ok(())
    }
}

impl<CpuImpl, HostFs> wasi::sockets::types::HostUdpSocket for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn bind(
        &mut self,
        socket: Resource<UdpSocket>,
        local_address: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        if !has_wasi_network_rights(self.process_authority(), crate::NetworkAuthorityRights::UDP) {
            return Ok(Err(socket_types::ErrorCode::AccessDenied));
        }
        let socket = self.table.get(&socket)?.clone();
        let local_address = match parse_p3_udp_socket_address(local_address, socket.family()) {
            Ok(address) => address,
            Err(error) => return Ok(Err(map_p3_udp_socket_error(error))),
        };
        if !has_wasi_network_rights(
            self.process_authority(),
            wasi_udp_bind_rights(local_address.port),
        ) {
            return Ok(Err(socket_types::ErrorCode::AccessDenied));
        }
        let result = socket
            .bind(local_address)
            .await
            .map_err(map_p3_udp_socket_error);
        Ok(result)
    }

    async fn connect(
        &mut self,
        socket: Resource<UdpSocket>,
        remote_address: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        if !has_wasi_network_rights(self.process_authority(), crate::NetworkAuthorityRights::UDP) {
            return Ok(Err(socket_types::ErrorCode::AccessDenied));
        }
        let socket = self.table.get(&socket)?.clone();
        let remote_address = match parse_p3_udp_socket_address(remote_address, socket.family()) {
            Ok(address) => address,
            Err(error) => return Ok(Err(map_p3_udp_socket_error(error))),
        };
        let result = socket
            .connect(remote_address)
            .await
            .map_err(map_p3_udp_socket_error);
        Ok(result)
    }

    fn create(
        &mut self,
        address_family: socket_types::IpAddressFamily,
    ) -> Result<core::result::Result<Resource<UdpSocket>, socket_types::ErrorCode>> {
        if !has_wasi_network_rights(self.process_authority(), crate::NetworkAuthorityRights::UDP) {
            return Ok(Err(socket_types::ErrorCode::AccessDenied));
        }
        let family = match map_p3_udp_family(address_family) {
            Ok(family) => family,
            Err(error) => return Ok(Err(map_p3_udp_socket_error(error))),
        };
        let Some(service) = self.runtime_state.network_service() else {
            return Ok(Err(socket_types::ErrorCode::Other(None)));
        };
        let resource = self.table.push(UdpSocket::new(service, family))?;
        Ok(Ok(resource))
    }

    fn disconnect(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.disconnect().map_err(map_p3_udp_socket_error))
    }

    fn get_local_address(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .local_address()
            .map(format_p3_udp_socket_address)
            .map_err(map_p3_udp_socket_error))
    }

    fn get_remote_address(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<socket_types::IpSocketAddress, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .remote_address()
            .map(format_p3_udp_socket_address)
            .map_err(map_p3_udp_socket_error))
    }

    fn get_address_family(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<socket_types::IpAddressFamily> {
        let socket = self.table.get(&socket)?.clone();
        Ok(format_p3_udp_family(socket.family()))
    }

    fn get_unicast_hop_limit(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u8, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.unicast_hop_limit().map_err(map_p3_udp_socket_error))
    }

    fn set_unicast_hop_limit(
        &mut self,
        socket: Resource<UdpSocket>,
        value: u8,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_unicast_hop_limit(value)
            .map_err(map_p3_udp_socket_error))
    }

    fn get_receive_buffer_size(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .receive_buffer_size()
            .map_err(map_p3_udp_socket_error))
    }

    fn set_receive_buffer_size(
        &mut self,
        socket: Resource<UdpSocket>,
        value: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_receive_buffer_size(value)
            .map_err(map_p3_udp_socket_error))
    }

    fn get_send_buffer_size(
        &mut self,
        socket: Resource<UdpSocket>,
    ) -> Result<core::result::Result<u64, socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket.send_buffer_size().map_err(map_p3_udp_socket_error))
    }

    fn set_send_buffer_size(
        &mut self,
        socket: Resource<UdpSocket>,
        value: u64,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let socket = self.table.get(&socket)?.clone();
        Ok(socket
            .set_send_buffer_size(value)
            .map_err(map_p3_udp_socket_error))
    }

    fn drop(&mut self, resource: Resource<UdpSocket>) -> Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl<CpuImpl, HostFs, U> wasi::sockets::types::HostTcpSocketWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn connect(
        accessor: &Accessor<U, Self>,
        socket: Resource<TcpSocket>,
        remote_address: socket_types::IpSocketAddress,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let (has_tcp_authority, socket) = accessor.with(|mut access| {
            let store = access.get();
            Ok::<_, wasmtime::Error>((
                has_wasi_network_rights(
                    store.process_authority(),
                    crate::NetworkAuthorityRights::TCP,
                ),
                store.table.get(&socket)?.clone(),
            ))
        })?;
        if !has_tcp_authority {
            return Ok(Err(socket_types::ErrorCode::AccessDenied));
        }
        let remote_address = match parse_p3_tcp_socket_address(remote_address, socket.family()) {
            Ok(address) => address,
            Err(error) => return Ok(Err(error)),
        };
        Ok(socket.connect(remote_address).await)
    }

    fn listen(
        mut access: Access<'_, U, Self>,
        socket_resource: Resource<TcpSocket>,
    ) -> Result<core::result::Result<StreamReader<Resource<TcpSocket>>, socket_types::ErrorCode>>
    {
        let socket = access.get().table.get(&socket_resource)?.clone();
        let (local_address, listen_backlog, hop_limit) = {
            let state = socket.inner.lock();
            if state.stream.is_some()
                || state.listener.is_some()
                || state.connect_in_progress
                || state.listen_in_progress
                || state.listen_result.is_some()
            {
                return Ok(Err(socket_types::ErrorCode::InvalidState));
            }
            let local_address = state.local_address.unwrap_or(WasiTcpSocketAddress {
                address: state.family.unspecified_address(),
                port: 0,
            });
            (local_address, state.listen_backlog, state.hop_limit)
        };
        if !has_wasi_network_rights(
            access.get().process_authority(),
            wasi_tcp_bind_rights(local_address.port),
        ) {
            return Ok(Err(socket_types::ErrorCode::AccessDenied));
        }
        let spawner = access.get().spawner().clone();
        let getter = access.getter();
        let stream = StreamReader::new(
            &mut access,
            TcpListenStreamProducer::new(socket.clone(), getter, spawner.clone()),
        )?;
        let (service, inner, ready) = {
            let mut state = socket.inner.lock();
            if state.stream.is_some()
                || state.listener.is_some()
                || state.connect_in_progress
                || state.listen_in_progress
                || state.listen_result.is_some()
            {
                return Ok(Err(socket_types::ErrorCode::InvalidState));
            }
            state.listen_in_progress = true;
            (
                state.service.clone(),
                socket.inner.clone(),
                socket.ready.clone(),
            )
        };
        spawner.spawn_detached(async move {
            let result = service
                .tcp_listen(
                    local_address.address.network_address(),
                    local_address.port,
                    listen_backlog,
                    hop_limit,
                )
                .await;
            let mut state = inner.lock();
            state.listen_in_progress = false;
            state.listen_result = Some(result);
            ready.notify_all();
        });
        Ok(Ok(stream))
    }

    fn send(
        mut access: Access<'_, U, Self>,
        socket: Resource<TcpSocket>,
        mut bytes: StreamReader<u8>,
    ) -> Result<FutureReader<core::result::Result<(), socket_types::ErrorCode>>> {
        if !has_wasi_network_rights(
            access.get().process_authority(),
            crate::NetworkAuthorityRights::TCP,
        ) {
            bytes.close(&mut access)?;
            return FutureReader::new(&mut access, async {
                Ok::<_, wasmtime::Error>(Err::<(), socket_types::ErrorCode>(
                    socket_types::ErrorCode::AccessDenied,
                ))
            });
        }
        let socket = access.get().table.get(&socket)?.clone();
        let (tx, rx) = oneshot::channel();
        bytes.pipe(&mut access, TcpWriteConsumer::new(socket, tx))?;
        FutureReader::new(&mut access, async move {
            match rx.await {
                Ok(result) => Ok::<_, wasmtime::Error>(result),
                Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), socket_types::ErrorCode>(())),
            }
        })
    }

    fn receive(
        mut access: Access<'_, U, Self>,
        socket: Resource<TcpSocket>,
    ) -> Result<(
        StreamReader<u8>,
        FutureReader<core::result::Result<(), socket_types::ErrorCode>>,
    )> {
        if !has_wasi_network_rights(
            access.get().process_authority(),
            crate::NetworkAuthorityRights::TCP,
        ) {
            let stream = StreamReader::new(&mut access, Vec::<u8>::new())?;
            let future = FutureReader::new(&mut access, async {
                Ok::<_, wasmtime::Error>(Err::<(), socket_types::ErrorCode>(
                    socket_types::ErrorCode::AccessDenied,
                ))
            })?;
            return Ok((stream, future));
        }
        let socket = access.get().table.get(&socket)?.clone();
        let (tx, rx) = oneshot::channel();
        let stream = StreamReader::new(&mut access, TcpReadStreamProducer::new(socket, tx))?;
        let future = FutureReader::new(&mut access, async move {
            match rx.await {
                Ok(result) => Ok::<_, wasmtime::Error>(result),
                Err(_) => Ok::<_, wasmtime::Error>(Ok::<(), socket_types::ErrorCode>(())),
            }
        })?;
        Ok((stream, future))
    }
}

impl<CpuImpl, HostFs, U> wasi::sockets::types::HostUdpSocketWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn send(
        accessor: &Accessor<U, Self>,
        socket: Resource<UdpSocket>,
        bytes: Vec<u8>,
        remote_address: Option<socket_types::IpSocketAddress>,
    ) -> Result<core::result::Result<(), socket_types::ErrorCode>> {
        let has_udp_authority = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(has_wasi_network_rights(
                access.get().process_authority(),
                crate::NetworkAuthorityRights::UDP,
            ))
        })?;
        if !has_udp_authority {
            return Ok(Err(socket_types::ErrorCode::AccessDenied));
        }
        let socket = accessor
            .with(|mut access| access.get().table.get(&socket).map(|socket| socket.clone()))?;
        let remote_address = match remote_address {
            Some(address) => match parse_p3_udp_socket_address(address, socket.family()) {
                Ok(address) => Some(address),
                Err(error) => return Ok(Err(map_p3_udp_socket_error(error))),
            },
            None => None,
        };
        let result = socket
            .send_datagram(&bytes, remote_address, u64::MAX)
            .await
            .map_err(map_p3_udp_socket_error);
        Ok(result)
    }

    async fn receive(
        accessor: &Accessor<U, Self>,
        socket: Resource<UdpSocket>,
    ) -> Result<
        core::result::Result<(Vec<u8>, socket_types::IpSocketAddress), socket_types::ErrorCode>,
    > {
        let has_udp_authority = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(has_wasi_network_rights(
                access.get().process_authority(),
                crate::NetworkAuthorityRights::UDP,
            ))
        })?;
        if !has_udp_authority {
            return Ok(Err(socket_types::ErrorCode::AccessDenied));
        }
        let socket = accessor
            .with(|mut access| access.get().table.get(&socket).map(|socket| socket.clone()))?;
        let result = socket
            .receive_datagram(None, u64::MAX)
            .await
            .map(|datagram| {
                (
                    Vec::from(datagram.bytes),
                    format_p3_udp_socket_address(datagram.remote_address),
                )
            })
            .map_err(map_p3_udp_socket_error);
        Ok(result)
    }
}

pub(super) fn parse_p3_tcp_socket_address(
    address: socket_types::IpSocketAddress,
    family: WasiTcpSocketFamily,
) -> core::result::Result<WasiTcpSocketAddress, socket_types::ErrorCode> {
    match (family, address) {
        (WasiTcpSocketFamily::Ipv4, socket_types::IpSocketAddress::Ipv4(address)) => {
            Ok(WasiTcpSocketAddress {
                address: WasiTcpIpAddress::Ipv4(crate::Ipv4Address::new([
                    address.address.0,
                    address.address.1,
                    address.address.2,
                    address.address.3,
                ])),
                port: address.port,
            })
        }
        (WasiTcpSocketFamily::Ipv6, socket_types::IpSocketAddress::Ipv6(address)) => {
            let segments = [
                address.address.0,
                address.address.1,
                address.address.2,
                address.address.3,
                address.address.4,
                address.address.5,
                address.address.6,
                address.address.7,
            ];
            let mut octets = [0; 16];
            for (index, segment) in segments.into_iter().enumerate() {
                let [high, low] = segment.to_be_bytes();
                octets[index * 2] = high;
                octets[index * 2 + 1] = low;
            }
            Ok(WasiTcpSocketAddress {
                address: WasiTcpIpAddress::Ipv6(Ipv6Address::new(octets)),
                port: address.port,
            })
        }
        (WasiTcpSocketFamily::Ipv4, socket_types::IpSocketAddress::Ipv6(_)) => {
            Err(socket_types::ErrorCode::NotSupported)
        }
        (WasiTcpSocketFamily::Ipv6, socket_types::IpSocketAddress::Ipv4(_)) => {
            Err(socket_types::ErrorCode::NotSupported)
        }
    }
}

pub(super) fn format_p3_tcp_socket_address(
    address: WasiTcpSocketAddress,
) -> socket_types::IpSocketAddress {
    match address.address {
        WasiTcpIpAddress::Ipv4(ip) => {
            let [a, b, c, d] = ip.octets();
            socket_types::IpSocketAddress::Ipv4(socket_types::Ipv4SocketAddress {
                port: address.port,
                address: (a, b, c, d),
            })
        }
        WasiTcpIpAddress::Ipv6(ip) => {
            let octets = ip.octets();
            let segment =
                |index: usize| u16::from_be_bytes([octets[index * 2], octets[index * 2 + 1]]);
            socket_types::IpSocketAddress::Ipv6(socket_types::Ipv6SocketAddress {
                port: address.port,
                flow_info: 0,
                address: (
                    segment(0),
                    segment(1),
                    segment(2),
                    segment(3),
                    segment(4),
                    segment(5),
                    segment(6),
                    segment(7),
                ),
                scope_id: 0,
            })
        }
    }
}

pub(super) fn map_p3_tcp_error(error: crate::TcpError) -> socket_types::ErrorCode {
    match error.kind {
        crate::TcpErrorKind::UnresolvedHost | crate::TcpErrorKind::Unavailable => {
            socket_types::ErrorCode::RemoteUnreachable
        }
        crate::TcpErrorKind::Timeout => socket_types::ErrorCode::Timeout,
        crate::TcpErrorKind::PermissionDenied => socket_types::ErrorCode::AccessDenied,
        crate::TcpErrorKind::Internal => socket_types::ErrorCode::Other(None),
    }
}

pub(super) fn map_p3_udp_family(
    family: socket_types::IpAddressFamily,
) -> core::result::Result<WasiUdpSocketFamily, WasiUdpSocketError> {
    match family {
        socket_types::IpAddressFamily::Ipv4 => Ok(WasiUdpSocketFamily::Ipv4),
        socket_types::IpAddressFamily::Ipv6 => Ok(WasiUdpSocketFamily::Ipv6),
    }
}

pub(super) fn format_p3_udp_family(family: WasiUdpSocketFamily) -> socket_types::IpAddressFamily {
    match family {
        WasiUdpSocketFamily::Ipv4 => socket_types::IpAddressFamily::Ipv4,
        WasiUdpSocketFamily::Ipv6 => socket_types::IpAddressFamily::Ipv6,
    }
}

pub(super) fn parse_p3_udp_socket_address(
    address: socket_types::IpSocketAddress,
    family: WasiUdpSocketFamily,
) -> core::result::Result<WasiUdpSocketAddress, WasiUdpSocketError> {
    match (family, address) {
        (WasiUdpSocketFamily::Ipv4, socket_types::IpSocketAddress::Ipv4(address)) => {
            Ok(WasiUdpSocketAddress {
                address: crate::NetworkIpAddress::Ipv4(crate::Ipv4Address::new([
                    address.address.0,
                    address.address.1,
                    address.address.2,
                    address.address.3,
                ])),
                port: address.port,
            })
        }
        (WasiUdpSocketFamily::Ipv6, socket_types::IpSocketAddress::Ipv6(address)) => {
            let segments = [
                address.address.0,
                address.address.1,
                address.address.2,
                address.address.3,
                address.address.4,
                address.address.5,
                address.address.6,
                address.address.7,
            ];
            let mut octets = [0; 16];
            for (index, segment) in segments.into_iter().enumerate() {
                let [high, low] = segment.to_be_bytes();
                octets[index * 2] = high;
                octets[index * 2 + 1] = low;
            }
            Ok(WasiUdpSocketAddress {
                address: crate::NetworkIpAddress::Ipv6(Ipv6Address::new(octets)),
                port: address.port,
            })
        }
        (WasiUdpSocketFamily::Ipv4, socket_types::IpSocketAddress::Ipv6(_)) => {
            Err(WasiUdpSocketError::NotSupported)
        }
        (WasiUdpSocketFamily::Ipv6, socket_types::IpSocketAddress::Ipv4(_)) => {
            Err(WasiUdpSocketError::NotSupported)
        }
    }
}

pub(super) fn format_p3_udp_socket_address(
    address: WasiUdpSocketAddress,
) -> socket_types::IpSocketAddress {
    match address.address {
        crate::NetworkIpAddress::Ipv4(ip) => {
            let [a, b, c, d] = ip.octets();
            socket_types::IpSocketAddress::Ipv4(socket_types::Ipv4SocketAddress {
                port: address.port,
                address: (a, b, c, d),
            })
        }
        crate::NetworkIpAddress::Ipv6(ip) => {
            let octets = ip.octets();
            let segments: [u16; 8] = core::array::from_fn(|index| {
                u16::from_be_bytes([octets[index * 2], octets[index * 2 + 1]])
            });
            socket_types::IpSocketAddress::Ipv6(socket_types::Ipv6SocketAddress {
                port: address.port,
                flow_info: 0,
                address: (
                    segments[0],
                    segments[1],
                    segments[2],
                    segments[3],
                    segments[4],
                    segments[5],
                    segments[6],
                    segments[7],
                ),
                scope_id: 0,
            })
        }
    }
}

pub(super) fn map_p3_udp_socket_error(error: WasiUdpSocketError) -> socket_types::ErrorCode {
    match error {
        WasiUdpSocketError::NotSupported => socket_types::ErrorCode::NotSupported,
        WasiUdpSocketError::InvalidArgument => socket_types::ErrorCode::InvalidArgument,
        WasiUdpSocketError::InvalidState | WasiUdpSocketError::NotInProgress => {
            socket_types::ErrorCode::InvalidState
        }
        WasiUdpSocketError::AddressNotBindable => socket_types::ErrorCode::AddressNotBindable,
        WasiUdpSocketError::DatagramTooLarge => socket_types::ErrorCode::DatagramTooLarge,
        WasiUdpSocketError::WouldBlock
        | WasiUdpSocketError::Backend(crate::UdpError {
            kind: crate::UdpErrorKind::Timeout,
            ..
        }) => socket_types::ErrorCode::Timeout,
        WasiUdpSocketError::Backend(_) => socket_types::ErrorCode::Other(None),
    }
}

impl<CpuImpl, HostFs> wasi::sockets::ip_name_lookup::Host for StoreData<CpuImpl, HostFs>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, HostFs, U> wasi::sockets::ip_name_lookup::HostWithStore<U>
    for HasSelf<StoreData<CpuImpl, HostFs>>
where
    CpuImpl: Cpu + Clone,
    HostFs: crate::HostFileSystem,
{
    async fn resolve_addresses(
        accessor: &Accessor<U, Self>,
        name: String,
    ) -> Result<core::result::Result<Vec<socket_types::IpAddress>, ip_name_lookup::ErrorCode>> {
        let (has_dns_authority, service) = accessor.with(|mut access| {
            let store = access.get();
            Ok::<_, wasmtime::Error>((
                has_wasi_network_rights(
                    store.process_authority(),
                    crate::NetworkAuthorityRights::DNS,
                ),
                store.runtime_state.network_service(),
            ))
        })?;
        if !has_dns_authority {
            return Ok(Err(ip_name_lookup::ErrorCode::AccessDenied));
        }
        let Some(service) = service else {
            return Ok(Err(ip_name_lookup::ErrorCode::TemporaryResolverFailure));
        };
        Ok(service
            .dns_resolve(&name, u64::MAX)
            .await
            .map(|addresses| addresses.into_iter().map(format_p3_ip_address).collect())
            .map_err(map_p3_dns_error))
    }
}

pub(super) fn format_p3_ip_address(address: crate::Ipv4Address) -> socket_types::IpAddress {
    let [a, b, c, d] = address.octets();
    socket_types::IpAddress::Ipv4((a, b, c, d))
}

pub(super) fn map_p3_dns_error(error: crate::DnsError) -> ip_name_lookup::ErrorCode {
    match error.kind {
        crate::DnsErrorKind::UnresolvedHost => ip_name_lookup::ErrorCode::NameUnresolvable,
        crate::DnsErrorKind::Timeout | crate::DnsErrorKind::Unavailable => {
            ip_name_lookup::ErrorCode::TemporaryResolverFailure
        }
        crate::DnsErrorKind::Internal => ip_name_lookup::ErrorCode::Other(None),
    }
}
