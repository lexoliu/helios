use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DhcpClientState {
    Init {
        transaction_id: u32,
    },
    Selecting {
        transaction_id: u32,
        last_sent: StackInstant,
    },
    Requesting {
        transaction_id: u32,
        requested_ip: Ipv4Address,
        server_identifier: Ipv4Address,
        last_sent: StackInstant,
    },
    Bound,
}

impl<CpuImpl, Runtime, DeviceImpl> NetworkService<CpuImpl, Runtime, DeviceImpl>
where
    CpuImpl: Cpu + Clone,
    Runtime: ComponentRuntimeState + Sync,
    DeviceImpl: NetworkDevice,
{
    pub async fn dns_resolve(
        &self,
        host: &str,
        timeout_nanos: u64,
    ) -> Result<Vec<NetworkIpAddress>, DnsError> {
        self.execute_dns_resolve(host, timeout_nanos).await
    }

    /// Resolves `host` to every address the resolver knows, in both
    /// families.
    ///
    /// One `A` and one `AAAA` question go out together under separate
    /// ids and both answers are collected before returning, because
    /// the guest's name-lookup interface hands it one address stream
    /// covering both families and cannot ask again for the other one.
    /// A resolver that answers only one of the two still yields that
    /// family once the deadline passes rather than failing the lookup.
    pub(super) async fn execute_dns_resolve(
        &self,
        host: &str,
        timeout_nanos: u64,
    ) -> Result<Vec<NetworkIpAddress>, DnsError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(vec![NetworkIpAddress::Ipv4(map_ipv4_address(address))]);
        }
        if let Some(address) = parse_ipv6(host) {
            return Ok(vec![NetworkIpAddress::Ipv6(address)]);
        }
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        self.wait_for_dns_transport(deadline_nanos).await?;
        // The lookup runs on its own connected socket rather than a
        // shared well-known port: a connected socket's four-tuple is
        // known when it is opened, so it is placed on the shard its
        // answers will demux to, and two lookups on two processors no
        // longer share one queue on one shard.
        let resolver = self.resolver_endpoint()?;
        let socket = self.open_resolver_socket(resolver)?;
        let result = self.resolve_through(&socket, host, deadline_nanos).await;
        self.close_resolver_socket(&socket);
        result
    }

    /// Runs one dual-family lookup over an already-open resolver socket.
    async fn resolve_through(
        &self,
        socket: &ResolverSocket,
        host: &str,
        deadline_nanos: u64,
    ) -> Result<Vec<NetworkIpAddress>, DnsError> {
        let queries = self
            .inner
            .state
            .shard_at(socket.shard_idx)
            .lock()
            .next_dns_query_pair();
        let mut answers = DnsAnswers::new();
        loop {
            // The answers land in the socket's own shard, whichever
            // processor drained them off the device. The mark is taken
            // before they are polled, so a response that arrives in
            // between resolves the wait instead of being slept through —
            // the exact case that made `--smp 4` DNS lookups time out
            // while the per-operation wait was a device-event wait.
            let wait = self.shard_wait(socket.shard_idx);
            self.drive_dns().await?;
            let now = StackInstant::from_nanos(self.now_nanos());
            let mut shard = self.inner.state.shard_at(socket.shard_idx).lock();
            shard.send_dns_queries(socket, queries, host, &answers, now)?;
            shard.poll_dns_responses(socket, queries, &mut answers)?;
            drop(shard);
            if answers.complete() {
                return answers.finish();
            }
            if self.now_nanos() >= deadline_nanos {
                if answers.answered_any() {
                    return answers.finish();
                }
                return Err(DnsError {
                    kind: DnsErrorKind::Timeout,
                    detail: NetworkErrorDetail::DnsLookupTimeout,
                });
            }
            self.drive_dns().await?;
            // A dropped query is answered by nothing, so the resolver
            // owns its own retransmission and bounds the wait by it.
            self.wait_for_shard_progress(
                wait,
                self.retransmit_wait(deadline_nanos, NETWORK_RETRANSMIT_WAIT),
            )
            .await;
        }
    }

    /// The resolver this link can actually reach.
    ///
    /// A DHCP-supplied resolver is preferred because the IPv4 path is
    /// always configured first; a Router Advertisement's RDNSS servers
    /// stand in on a link that offers IPv6 only.
    fn resolver_endpoint(&self) -> Result<IpAddress, DnsError> {
        self.inner
            .state
            .with(|state| {
                state
                    .dns_servers
                    .first()
                    .copied()
                    .map(IpAddress::Ipv4)
                    .or_else(|| state.stack.ipv6_dns_servers().next().map(IpAddress::Ipv6))
            })
            .ok_or(DnsError {
                kind: DnsErrorKind::Unavailable,
                detail: NetworkErrorDetail::DnsServersUnavailable,
            })
    }

    /// Opens a connected socket to `resolver` on the shard that will
    /// receive its answers.
    fn open_resolver_socket(&self, resolver: IpAddress) -> Result<ResolverSocket, DnsError> {
        let processor = self.inner.cpu.current_processor();
        let shard_idx = self.inner.state.shard_idx_for_processor(processor);
        let shard_count = self.inner.state.shard_count();
        // The port is picked on this processor's shard, preferring one
        // whose answers hash back here, and the socket is then opened
        // on whichever shard the flow actually hashes to — so the
        // lookup's answers are demultiplexed straight into the queue
        // this call is about to poll.
        let (local, local_port) = {
            let mut shard = self.inner.state.shard_at(shard_idx).lock();
            let local_port = shard
                .allocate_udp_local_port_for(resolver, DNS_PORT, shard_count)
                .map_err(|_| DnsError {
                    kind: DnsErrorKind::Unavailable,
                    detail: NetworkErrorDetail::DnsQueryStartFailed,
                })?;
            let local = shard
                .udp_local_endpoint(local_port, resolver)
                .map_err(|_| DnsError {
                    kind: DnsErrorKind::Unavailable,
                    detail: NetworkErrorDetail::DnsServersUnavailable,
                })?;
            (local, local_port)
        };
        let shard_idx =
            shard_idx_for_flow(local.address, local_port, resolver, DNS_PORT, shard_count);
        let binding = UdpSocketBinding::connected(
            local,
            UdpEndpoint {
                address: resolver,
                port: DNS_PORT,
            },
        );
        let stack_socket = self
            .inner
            .state
            .shard_at(shard_idx)
            .lock()
            .stack
            .open_udp(binding)
            .map_err(|_| DnsError {
                kind: DnsErrorKind::Unavailable,
                detail: NetworkErrorDetail::DnsQueryStartFailed,
            })?;
        Ok(ResolverSocket {
            shard_idx,
            stack_socket,
            resolver,
        })
    }

    /// Releases the socket and its ephemeral port. A lookup that timed
    /// out releases it just like one that answered, so an abandoned
    /// query cannot leak a port or keep matching later datagrams.
    fn close_resolver_socket(&self, socket: &ResolverSocket) {
        self.inner
            .state
            .shard_at(socket.shard_idx)
            .lock()
            .stack
            .remove_udp_socket(socket.stack_socket)
            .unwrap_or_else(|_| panic!("resolver socket referenced an unknown stack socket"));
    }

    pub(super) async fn wait_for_dns_transport(&self, deadline_nanos: u64) -> Result<(), DnsError> {
        self.wait_for_dns_configured(
            deadline_nanos,
            dns_configuration_timeout,
            dns_configuration_error,
        )
        .await
    }

    /// Every address worth an echo request for this destination, in the
    /// order the ping walk should try them.
    ///
    /// Echo works in both families, so a name is resolved the same way
    /// a connect resolves it: the dual-family answer is ordered by what
    /// this link can actually source, and a target that is unreachable
    /// in one family falls through to the next.
    pub(super) async fn resolve_host_ping(
        &self,
        host: &str,
        deadline_nanos: u64,
    ) -> Result<ConnectCandidates, PingError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(ConnectCandidates::literal(IpAddress::Ipv4(address)));
        }
        if let Some(address) = parse_ipv6(host) {
            return Ok(ConnectCandidates::literal(IpAddress::Ipv6(address)));
        }
        let timeout_nanos = deadline_nanos.saturating_sub(self.now_nanos());
        let addresses = self
            .execute_dns_resolve(host, timeout_nanos)
            .await
            .map_err(|error| PingError {
                kind: match error.kind {
                    DnsErrorKind::Timeout => PingErrorKind::Timeout,
                    DnsErrorKind::Unavailable | DnsErrorKind::Internal => {
                        PingErrorKind::Unavailable
                    }
                    DnsErrorKind::UnresolvedHost => PingErrorKind::UnresolvedHost,
                },
                detail: error.detail,
            })?;
        self.usable_addresses(addresses).ok_or(PingError {
            kind: PingErrorKind::UnresolvedHost,
            detail: NetworkErrorDetail::DnsNoIpv4Address,
        })
    }

    pub(super) async fn drive_dns(&self) -> Result<(), DnsError> {
        self.drive_network(NetworkPollSource::Dns)
            .await
            .map_err(|error| DnsError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))
    }

    pub(super) async fn acquire_dhcp_address(&self) -> Result<KernelIpv4Cidr, NetworkControlError> {
        loop {
            // The lease is negotiated on the default shard, which is
            // where the offer demuxes back to.
            let wait = self.default_shard_wait();
            self.drive_network(NetworkPollSource::Configuration)
                .await
                .map_err(|_| NetworkControlError::BackendFault)?;
            let now = StackInstant::from_nanos(self.now_nanos());
            let next = self.inner.state.with_mut(|state| {
                state.drive_dhcp(now)?;
                if state.is_configured() {
                    self.inner.control.publish_from_shard(state);
                }
                Ok(state.stack.primary_ipv4_address().map(map_ipv4_cidr))
            })?;
            if let Some(cidr) = next {
                self.synchronize_control_plane();
                return Ok(cidr);
            }
            self.drive_network(NetworkPollSource::Configuration)
                .await
                .map_err(|_| NetworkControlError::BackendFault)?;
            // This walk has no caller deadline of its own; a dropped
            // DISCOVER is retried at the client's retransmission
            // interval, and an offer that lands sooner wakes it through
            // the shard.
            self.wait_for_shard_progress(
                wait,
                self.progress_wait(Duration::from_nanos(DHCP_RETRANSMIT_NANOS)),
            )
            .await;
        }
    }
}

impl<CpuImpl, Runtime, DeviceImpl> ComponentNetworkService
    for NetworkService<CpuImpl, Runtime, DeviceImpl>
where
    CpuImpl: Cpu + Clone,
    Runtime: ComponentRuntimeState + Sync,
    DeviceImpl: NetworkDevice,
{
    type TcpStream = TcpStreamId;
    type TcpListener = TcpListenerId;
    type UdpSocket = UdpSocketId;

    fn hardware_address(&self) -> [u8; 6] {
        NetworkService::hardware_address(self)
    }

    fn ipv4_cidr(&self) -> impl core::future::Future<Output = Option<crate::Ipv4Cidr>> + Send + '_ {
        async move { NetworkService::ipv4_cidr(self).await }
    }

    fn ping<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<PingReply, PingError>> + Send + 'a {
        async move { NetworkService::ping(self, host, timeout_nanos).await }
    }

    fn dns_resolve<'a>(
        &'a self,
        host: &'a str,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Vec<NetworkIpAddress>, DnsError>> + Send + 'a
    {
        async move { NetworkService::dns_resolve(self, host, timeout_nanos).await }
    }

    fn tcp_connect<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Self::TcpStream, TcpError>> + Send + 'a {
        async move { NetworkService::tcp_connect(self, host, port, timeout_nanos).await }
    }

    fn tcp_connect_from<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        local_port: u16,
        hop_limit: u8,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Self::TcpStream, TcpError>> + Send + 'a {
        async move {
            NetworkService::tcp_connect_from(self, host, port, local_port, hop_limit, timeout_nanos)
                .await
        }
    }

    fn tcp_connect_address(
        &self,
        remote_address: NetworkIpAddress,
        port: u16,
        local_port: u16,
        hop_limit: u8,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Self::TcpStream, TcpError>> + Send + '_ {
        async move {
            NetworkService::tcp_connect_address(
                self,
                remote_address,
                port,
                local_port,
                hop_limit,
                timeout_nanos,
            )
            .await
        }
    }

    fn tcp_listen(
        &self,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: u16,
        hop_limit: u8,
    ) -> impl core::future::Future<Output = Result<TcpListener<Self::TcpListener>, TcpError>> + Send + '_
    {
        async move {
            NetworkService::tcp_listen(self, local_address, local_port, backlog, hop_limit).await
        }
    }

    fn tcp_set_hop_limit(&self, stream: Self::TcpStream, hop_limit: u8) -> Result<(), TcpError> {
        NetworkService::tcp_set_hop_limit(self, stream, hop_limit)
    }

    fn tcp_listener_set_hop_limit(
        &self,
        listener: Self::TcpListener,
        hop_limit: u8,
    ) -> Result<(), TcpError> {
        NetworkService::tcp_listener_set_hop_limit(self, listener, hop_limit)
    }

    fn tcp_accept(
        &self,
        listener: Self::TcpListener,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<TcpAccepted<Self::TcpStream>, TcpError>> + Send + '_
    {
        async move { NetworkService::tcp_accept(self, listener, timeout_nanos).await }
    }

    fn tcp_readiness(
        &self,
        stream: Self::TcpStream,
    ) -> impl core::future::Future<Output = Result<SocketReadiness, TcpError>> + Send + '_ {
        async move { NetworkService::tcp_readiness(self, stream).await }
    }

    fn tcp_listener_readiness(
        &self,
        listener: Self::TcpListener,
    ) -> impl core::future::Future<Output = Result<SocketReadiness, TcpError>> + Send + '_ {
        async move { NetworkService::tcp_listener_readiness(self, listener).await }
    }

    fn udp_readiness(
        &self,
        socket: Self::UdpSocket,
    ) -> impl core::future::Future<Output = Result<SocketReadiness, UdpError>> + Send + '_ {
        async move { NetworkService::udp_readiness(self, socket).await }
    }

    fn tcp_write_all<'a>(
        &'a self,
        stream: Self::TcpStream,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<(), TcpError>> + Send + 'a {
        async move { NetworkService::tcp_write_all(self, stream, bytes, timeout_nanos).await }
    }

    fn tcp_write_all_bytes<'a>(
        &'a self,
        stream: Self::TcpStream,
        bytes: Bytes,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<(), TcpError>> + Send + 'a {
        async move { NetworkService::tcp_write_all_bytes(self, stream, bytes, timeout_nanos).await }
    }

    fn tcp_read<'a>(
        &'a self,
        stream: Self::TcpStream,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Option<Bytes>, TcpError>> + Send + 'a {
        async move { NetworkService::tcp_read(self, stream, max_bytes, timeout_nanos).await }
    }

    fn tcp_read_into<'a>(
        &'a self,
        stream: Self::TcpStream,
        buffer: RegisteredTcpReadBuffer<'a>,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Option<usize>, TcpError>> + Send + 'a {
        async move { NetworkService::tcp_read_into(self, stream, buffer, timeout_nanos).await }
    }

    fn tcp_shutdown_send(
        &self,
        stream: Self::TcpStream,
    ) -> impl core::future::Future<Output = Result<(), TcpError>> + Send + '_ {
        async move { NetworkService::tcp_shutdown_send(self, stream).await }
    }

    fn tcp_close(
        &self,
        stream: Self::TcpStream,
    ) -> impl core::future::Future<Output = ()> + Send + '_ {
        async move { NetworkService::tcp_close(self, stream).await }
    }

    fn udp_bind(
        &self,
        local_port: u16,
    ) -> impl core::future::Future<Output = Result<UdpBinding<Self::UdpSocket>, UdpError>> + Send + '_
    {
        async move { NetworkService::udp_bind(self, local_port).await }
    }

    fn udp_connect(
        &self,
        socket: Self::UdpSocket,
        remote_address: NetworkIpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        NetworkService::udp_connect(self, socket, remote_address, port)
    }

    fn udp_disconnect(&self, socket: Self::UdpSocket) -> Result<(), UdpError> {
        NetworkService::udp_disconnect(self, socket)
    }

    fn udp_set_hop_limit(&self, socket: Self::UdpSocket, hop_limit: u8) -> Result<(), UdpError> {
        NetworkService::udp_set_hop_limit(self, socket, hop_limit)
    }

    fn udp_send<'a>(
        &'a self,
        socket: Self::UdpSocket,
        host: &'a str,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<u64, UdpError>> + Send + 'a {
        async move { NetworkService::udp_send(self, socket, host, port, bytes, timeout_nanos).await }
    }

    fn udp_send_address<'a>(
        &'a self,
        socket: Self::UdpSocket,
        remote_address: NetworkIpAddress,
        port: u16,
        bytes: &'a [u8],
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<u64, UdpError>> + Send + 'a {
        async move {
            NetworkService::udp_send_address(
                self,
                socket,
                remote_address,
                port,
                bytes,
                timeout_nanos,
            )
            .await
        }
    }

    fn udp_receive<'a>(
        &'a self,
        socket: Self::UdpSocket,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Option<UdpDatagram>, UdpError>> + Send + 'a {
        async move { NetworkService::udp_receive(self, socket, max_bytes, timeout_nanos).await }
    }

    fn udp_join_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> impl core::future::Future<Output = Result<(), UdpError>> + Send + '_ {
        async move { NetworkService::udp_join_multicast_v4(self, group, interface).await }
    }

    fn udp_leave_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> impl core::future::Future<Output = Result<(), UdpError>> + Send + '_ {
        async move { NetworkService::udp_leave_multicast_v4(self, group, interface).await }
    }

    fn udp_close(
        &self,
        socket: Self::UdpSocket,
    ) -> impl core::future::Future<Output = ()> + Send + '_ {
        async move { NetworkService::udp_close(self, socket).await }
    }
}

impl<CpuImpl, Runtime, DeviceImpl> NetworkAdminBackend
    for NetworkService<CpuImpl, Runtime, DeviceImpl>
where
    CpuImpl: Cpu + Clone,
    Runtime: ComponentRuntimeState + Sync,
    DeviceImpl: NetworkDevice,
{
    fn network_stats(&self) -> crate::NetworkStats {
        NetworkService::stats(self)
    }

    async fn bridge_port(
        &self,
        port: NetworkPortId,
        _: NetworkBridgeRequest,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        Err(NetworkControlError::BridgeUnavailable)
    }

    async fn unbridge_port(&self, port: NetworkPortId) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        Err(NetworkControlError::BridgeUnavailable)
    }

    async fn acquire_dhcp(
        &self,
        port: NetworkPortId,
    ) -> Result<KernelIpv4Cidr, NetworkControlError> {
        require_local_network_port(port)?;
        self.acquire_dhcp_address().await
    }

    async fn add_address(
        &self,
        port: NetworkPortId,
        address: KernelIpv4Cidr,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.control.update_ipv4_addresses(|addresses| {
            addresses.add(map_kernel_ipv4_cidr(address));
            Ok(())
        })?;
        let connected_route = Route {
            destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(address)),
            gateway: None,
            expires_at: None,
        };
        self.inner.control.update_routes(|routes| {
            routes
                .add(connected_route)
                .map_err(|_| NetworkControlError::InvalidRoute)
        })?;
        let mut result = Ok(());
        self.inner.state.for_each(|state| {
            if result.is_ok() {
                self.inner.control.synchronize_shard(state);
                if let Err(error) = state.add_ipv4_address(address) {
                    result = Err(error);
                }
            }
        });
        result
    }

    async fn remove_address(
        &self,
        port: NetworkPortId,
        address: KernelIpv4Cidr,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.control.update_ipv4_addresses(|addresses| {
            addresses.remove(map_kernel_ipv4_cidr(address));
            Ok(())
        })?;
        self.inner.control.update_routes(|routes| {
            routes.remove(Route {
                destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(address)),
                gateway: None,
                expires_at: None,
            });
            Ok(())
        })?;
        self.inner.state.for_each(|state| {
            self.inner.control.synchronize_shard(state);
            state.remove_ipv4_address(address);
        });
        Ok(())
    }

    async fn clear_addresses(&self, port: NetworkPortId) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.control.update_ipv4_addresses(|addresses| {
            addresses.clear();
            Ok(())
        })?;
        self.inner.control.update_routes(|routes| {
            for cidr in self.inner.state.with(NetworkShard::list_ipv4_addresses) {
                routes.remove(Route {
                    destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(cidr)),
                    gateway: None,
                    expires_at: None,
                });
            }
            Ok(())
        })?;
        self.inner
            .state
            .for_each(NetworkShard::clear_ipv4_addresses);
        Ok(())
    }

    async fn list_addresses(
        &self,
        port: NetworkPortId,
    ) -> Result<Vec<KernelIpv4Cidr>, NetworkControlError> {
        require_local_network_port(port)?;
        Ok(self.inner.control.list_ipv4_addresses())
    }

    async fn mac_address(&self, port: NetworkPortId) -> Result<MacAddress, NetworkControlError> {
        require_local_network_port(port)?;
        Ok(MacAddress::new(self.hardware_address()))
    }

    async fn set_gateway(
        &self,
        port: NetworkPortId,
        gateway: KernelIpv4Address,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.control.update_routes(|routes| {
            routes
                .add(Route {
                    destination: IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0)),
                    gateway: Some(IpAddress::Ipv4(map_kernel_ipv4_address(gateway))),
                    expires_at: None,
                })
                .map_err(|_| NetworkControlError::InvalidRoute)
        })?;
        let mut result = Ok(());
        self.inner.state.for_each(|state| {
            if result.is_ok() {
                self.inner.control.synchronize_shard(state);
                if let Err(error) = state.set_default_ipv4_gateway(gateway) {
                    result = Err(error);
                }
            }
        });
        result
    }

    async fn add_route(
        &self,
        port: NetworkPortId,
        route: KernelIpv4Route,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.control.update_routes(|routes| {
            routes
                .add(Route {
                    destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(route.destination())),
                    gateway: Some(IpAddress::Ipv4(map_kernel_ipv4_address(route.gateway()))),
                    expires_at: route.expires_at_nanos().map(StackInstant::from_nanos),
                })
                .map_err(|_| NetworkControlError::InvalidRoute)
        })?;
        let mut result = Ok(());
        self.inner.state.for_each(|state| {
            if result.is_ok() {
                self.inner.control.synchronize_shard(state);
                if let Err(error) = state.add_ipv4_route(route) {
                    result = Err(error);
                }
            }
        });
        result
    }

    async fn remove_route(
        &self,
        port: NetworkPortId,
        route: KernelIpv4Route,
    ) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.control.update_routes(|routes| {
            routes.remove(Route {
                destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(route.destination())),
                gateway: Some(IpAddress::Ipv4(map_kernel_ipv4_address(route.gateway()))),
                expires_at: route.expires_at_nanos().map(StackInstant::from_nanos),
            });
            Ok(())
        })?;
        self.inner.state.for_each(|state| {
            self.inner.control.synchronize_shard(state);
            state.remove_ipv4_route(route);
        });
        Ok(())
    }

    async fn clear_routes(&self, port: NetworkPortId) -> Result<(), NetworkControlError> {
        require_local_network_port(port)?;
        self.inner.control.update_routes(|routes| {
            routes.clear_ipv4();
            Ok(())
        })?;
        self.inner.state.for_each(NetworkShard::clear_ipv4_routes);
        Ok(())
    }

    async fn list_routes(
        &self,
        port: NetworkPortId,
    ) -> Result<Vec<KernelIpv4Route>, NetworkControlError> {
        require_local_network_port(port)?;
        Ok(self.inner.control.list_ipv4_routes())
    }
}

impl NetworkShard {
    pub(super) fn drive_dhcp(&mut self, now: StackInstant) -> Result<(), NetworkControlError> {
        let socket = self.internal_udp_socket(INTERNAL_DHCP_SOCKET_INDEX, DHCP_CLIENT_PORT);
        while let Some(datagram) = self
            .stack
            .take_udp(socket)
            .unwrap_or_else(|error| panic!("DHCP UDP socket disappeared: {error}"))
        {
            let Some(message) =
                DhcpPacket::parse(&datagram.bytes).and_then(DhcpPacket::server_message)
            else {
                continue;
            };
            match (self.dhcp, message.message_type) {
                (DhcpClientState::Selecting { transaction_id, .. }, DhcpMessageType::Offer)
                    if message.transaction_id == transaction_id =>
                {
                    let server_identifier = message
                        .server_identifier
                        .ok_or(NetworkControlError::BackendFault)?;
                    self.send_dhcp_request(
                        transaction_id,
                        message.your_ip,
                        server_identifier,
                        now,
                    )?;
                    self.dhcp = DhcpClientState::Requesting {
                        transaction_id,
                        requested_ip: message.your_ip,
                        server_identifier,
                        last_sent: now,
                    };
                }
                (DhcpClientState::Requesting { transaction_id, .. }, DhcpMessageType::Ack)
                    if message.transaction_id == transaction_id =>
                {
                    let prefix_len = message
                        .subnet_mask
                        .map(ipv4_mask_prefix_len)
                        .transpose()?
                        .ok_or(NetworkControlError::InvalidAddress)?;
                    self.stack
                        .add_ipv4_address(Ipv4Cidr::new(message.your_ip, prefix_len));
                    if let Some(router) = message.router {
                        self.set_default_ipv4_gateway(map_ipv4_address(router))?;
                    }
                    self.dns_servers.clear();
                    self.dns_servers.extend(message.dns_servers);
                    self.dhcp = DhcpClientState::Bound;
                }
                (_, DhcpMessageType::Nak) => {
                    self.stack.clear_ipv4_addresses();
                    self.dhcp = DhcpClientState::Init {
                        transaction_id: next_transaction_id(message.transaction_id),
                    };
                }
                _ => {}
            }
        }

        if let DhcpClientState::Init { transaction_id } = self.dhcp {
            self.send_dhcp_discover(transaction_id, now)?;
            self.dhcp = DhcpClientState::Selecting {
                transaction_id,
                last_sent: now,
            };
        } else {
            self.retransmit_dhcp(now)?;
        }
        Ok(())
    }

    pub(super) fn send_dhcp_discover(
        &mut self,
        transaction_id: u32,
        now: StackInstant,
    ) -> Result<(), NetworkControlError> {
        let message = DhcpClientMessage::discover(transaction_id, self.stack.config().mac);
        self.send_dhcp_message(message, transaction_id as u16, now)
    }

    pub(super) fn send_dhcp_request(
        &mut self,
        transaction_id: u32,
        requested_ip: Ipv4Address,
        server_identifier: Ipv4Address,
        now: StackInstant,
    ) -> Result<(), NetworkControlError> {
        let message = DhcpClientMessage::request(
            transaction_id,
            self.stack.config().mac,
            requested_ip,
            server_identifier,
        );
        self.send_dhcp_message(message, transaction_id as u16, now)
    }

    pub(super) fn send_dhcp_message(
        &mut self,
        message: DhcpClientMessage,
        identification: u16,
        now: StackInstant,
    ) -> Result<(), NetworkControlError> {
        let mut payload = [0u8; 548];
        let len = message
            .encode(&mut payload)
            .ok_or(NetworkControlError::BackendFault)?;
        self.stack
            .send_udp_ipv4_from(
                Ipv4Address::UNSPECIFIED,
                Ipv4Address::BROADCAST,
                UdpEgress {
                    source_port: DHCP_CLIENT_PORT,
                    destination_port: DHCP_SERVER_PORT,
                    payload: &payload[..len],
                    hop_limit: DEFAULT_HOP_LIMIT,
                },
                identification,
                now,
            )
            .map(|_| ())
            .map_err(|_| NetworkControlError::BackendFault)
    }

    /// Identifier for the next echo exchange this shard starts.
    ///
    /// Replies are matched on it, so an identifier that a previous ping
    /// abandoned must not be handed out again while its late reply may
    /// still arrive; the counter therefore walks the whole 16-bit space
    /// before it repeats, and skips zero the way the DNS query ids do.
    pub(super) fn next_icmp_echo_identifier(&mut self) -> u16 {
        let identifier = self.next_icmp_echo_identifier;
        self.next_icmp_echo_identifier = self.next_icmp_echo_identifier.wrapping_add(1);
        if self.next_icmp_echo_identifier == 0 {
            self.next_icmp_echo_identifier = 1;
        }
        identifier
    }

    /// Queues the echo request `key` describes.
    ///
    /// Reports whether the request reached the transmit queue: a `false`
    /// means the next hop's link-layer address is still being resolved
    /// and an ARP request or neighbour solicitation went out instead, so
    /// the caller repeats the send once the neighbour answers.
    pub(super) fn send_icmp_echo_request(
        &mut self,
        key: IcmpEchoKey,
        payload: &[u8],
        now: StackInstant,
    ) -> Result<bool, PingError> {
        self.stack
            .send_icmp_echo_request(
                key.destination,
                key.identifier,
                key.sequence,
                payload,
                key.sequence,
                now,
            )
            .map(|queued| queued != 0)
            .map_err(|_| PingError {
                kind: PingErrorKind::Unavailable,
                detail: NetworkErrorDetail::IcmpQueueFailed,
            })
    }

    /// Claims the reply to `key`, if one has arrived on this shard.
    pub(super) fn take_icmp_echo_reply(&mut self, key: IcmpEchoKey) -> Option<IcmpEchoReply> {
        self.stack.take_icmp_echo_reply(key)
    }

    fn next_dns_query_id(&mut self) -> u16 {
        let id = self.next_dns_query_id;
        self.next_dns_query_id = self.next_dns_query_id.wrapping_add(1);
        if self.next_dns_query_id == 0 {
            self.next_dns_query_id = 1;
        }
        id
    }

    /// The two question ids one dual-family lookup will carry.
    pub(super) fn next_dns_query_pair(&mut self) -> DnsQueryPair {
        DnsQueryPair {
            a: self.next_dns_query_id(),
            aaaa: self.next_dns_query_id(),
        }
    }

    /// Sends whichever of the pair's questions is still outstanding.
    ///
    /// Called on every poll iteration, so an unanswered question is
    /// retransmitted while its peer's answer is already banked.
    pub(super) fn send_dns_queries(
        &mut self,
        socket: &ResolverSocket,
        queries: DnsQueryPair,
        host: &str,
        answers: &DnsAnswers,
        now: StackInstant,
    ) -> Result<(), DnsError> {
        if !answers.a_answered {
            self.send_dns_query(socket, queries.a, host, DnsRecordType::A, now)?;
        }
        if !answers.aaaa_answered {
            self.send_dns_query(socket, queries.aaaa, host, DnsRecordType::Aaaa, now)?;
        }
        Ok(())
    }

    fn send_dns_query(
        &mut self,
        socket: &ResolverSocket,
        query_id: u16,
        host: &str,
        record: DnsRecordType,
        now: StackInstant,
    ) -> Result<(), DnsError> {
        let mut payload = [0u8; 512];
        let len = DnsQuestionWriter::new(&mut payload)
            .write_query(query_id, host, record)
            .ok_or(DnsError {
                kind: DnsErrorKind::UnresolvedHost,
                detail: NetworkErrorDetail::DnsQueryStartFailed,
            })?;
        self.stack
            .send_udp(
                socket.stack_socket,
                socket.resolver,
                DNS_PORT,
                &payload[..len],
                query_id,
                now,
            )
            .map(|_| ())
            .map_err(|_| DnsError {
                kind: DnsErrorKind::Unavailable,
                detail: NetworkErrorDetail::DnsQueryStartFailed,
            })
    }

    /// Drains this lookup's resolver socket, banking whichever of the
    /// pair's answers have arrived.
    pub(super) fn poll_dns_responses(
        &mut self,
        socket: &ResolverSocket,
        queries: DnsQueryPair,
        answers: &mut DnsAnswers,
    ) -> Result<(), DnsError> {
        let socket = socket.stack_socket;
        while let Some(datagram) = self.stack.take_udp(socket).map_err(|_| DnsError {
            kind: DnsErrorKind::Unavailable,
            detail: NetworkErrorDetail::DnsQueryStartFailed,
        })? {
            let Some(message) = DnsResponse::parse(&datagram.bytes).and_then(DnsResponse::message)
            else {
                continue;
            };
            if message.id == queries.a {
                answers.a_answered = true;
            } else if message.id == queries.aaaa {
                answers.aaaa_answered = true;
            } else {
                continue;
            }
            answers.extend(message.addresses.iter().copied());
        }
        Ok(())
    }

    /// Drives IPv6 stateless address autoconfiguration one step:
    /// installs the link-local address on first call and queues a
    /// Router Solicitation when one is due.
    pub(super) fn drive_ipv6_autoconfig(
        &mut self,
        now: StackInstant,
    ) -> Result<(), NetworkControlError> {
        if self.stack.ipv6_link_local_address().is_none() {
            self.stack.configure_ipv6_link_local();
        }
        self.stack
            .drive_ipv6_autoconfig(now)
            .map_err(|_| NetworkControlError::BackendFault)
    }
}

/// One lookup's connected socket to a resolver.
///
/// Both questions of a dual-family lookup share it, so their answers
/// arrive on one queue on one shard — the shard the socket's four-tuple
/// belongs to, which is where the receive path places them.
pub(super) struct ResolverSocket {
    pub(super) shard_idx: usize,
    pub(super) stack_socket: helios_netstack::UdpSocketId,
    pub(super) resolver: IpAddress,
}

/// The two question ids one dual-family lookup has outstanding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DnsQueryPair {
    pub(super) a: u16,
    pub(super) aaaa: u16,
}

/// Answers accumulated for one dual-family lookup.
///
/// Both questions share the lookup's resolver socket, so answers arrive
/// interleaved with each other; this keeps per-lookup state out of the
/// shard.
pub(super) struct DnsAnswers {
    addresses: Vec<NetworkIpAddress>,
    a_answered: bool,
    aaaa_answered: bool,
}

impl DnsAnswers {
    fn new() -> Self {
        Self {
            addresses: Vec::new(),
            a_answered: false,
            aaaa_answered: false,
        }
    }

    fn extend(&mut self, addresses: impl IntoIterator<Item = IpAddress>) {
        for address in addresses {
            let address = map_ip_address(address);
            if !self.addresses.contains(&address) {
                self.addresses.push(address);
            }
        }
    }

    fn complete(&self) -> bool {
        self.a_answered && self.aaaa_answered
    }

    fn answered_any(&self) -> bool {
        self.a_answered || self.aaaa_answered
    }

    /// A resolver that answered but returned no address for either
    /// family means the name does not resolve, which is distinct from
    /// the resolver never answering at all.
    fn finish(self) -> Result<Vec<NetworkIpAddress>, DnsError> {
        if self.addresses.is_empty() {
            return Err(DnsError {
                kind: DnsErrorKind::UnresolvedHost,
                detail: NetworkErrorDetail::DnsNoIpv4Address,
            });
        }
        Ok(self.addresses)
    }
}
