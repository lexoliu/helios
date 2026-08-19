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
    ) -> Result<Vec<KernelIpv4Address>, DnsError> {
        self.execute_dns_resolve(host, timeout_nanos).await
    }

    pub(super) async fn execute_dns_resolve(
        &self,
        host: &str,
        timeout_nanos: u64,
    ) -> Result<Vec<KernelIpv4Address>, DnsError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(vec![map_ipv4_address(address)]);
        }
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        self.wait_for_ipv4_dns(deadline_nanos).await?;
        let query_id = self.inner.state.with_mut(NetworkShard::next_dns_query_id);

        loop {
            self.drive_dns().await?;
            let now = StackInstant::from_nanos(self.now_nanos());
            let query = self.inner.state.with_mut(
                |state| -> Result<Option<Vec<Ipv4Address>>, DnsError> {
                    state.send_dns_query(query_id, host, now)?;
                    state.take_dns_response(query_id)
                },
            )?;
            if let Some(addresses) = query {
                return Ok(addresses.into_iter().map(map_ipv4_address).collect());
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(DnsError {
                    kind: DnsErrorKind::Timeout,
                    detail: NetworkErrorDetail::DnsLookupTimeout,
                });
            }
            self.drive_dns().await?;
            self.wait_for_progress(NETWORK_PROGRESS_WAIT).await;
        }
    }

    pub(super) async fn wait_for_ipv4_dns(&self, deadline_nanos: u64) -> Result<(), DnsError> {
        self.wait_for_ipv4_configured(
            deadline_nanos,
            dns_configuration_timeout,
            dns_configuration_error,
        )
        .await
    }

    pub(super) async fn resolve_host_ping(
        &self,
        host: &str,
        deadline_nanos: u64,
    ) -> Result<Ipv4Address, PingError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(address);
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
        addresses
            .into_iter()
            .next()
            .map(map_kernel_ipv4_address)
            .ok_or(PingError {
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
            self.wait_for_progress(NETWORK_PROGRESS_WAIT).await;
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
    ) -> impl core::future::Future<Output = Result<Vec<KernelIpv4Address>, DnsError>> + Send + 'a
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
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Self::TcpStream, TcpError>> + Send + 'a {
        async move {
            NetworkService::tcp_connect_from(self, host, port, local_port, timeout_nanos).await
        }
    }

    fn tcp_connect_address(
        &self,
        remote_address: NetworkIpAddress,
        port: u16,
        local_port: u16,
        timeout_nanos: u64,
    ) -> impl core::future::Future<Output = Result<Self::TcpStream, TcpError>> + Send + '_ {
        async move {
            NetworkService::tcp_connect_address(
                self,
                remote_address,
                port,
                local_port,
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
    ) -> impl core::future::Future<Output = Result<TcpListener<Self::TcpListener>, TcpError>> + Send + '_
    {
        async move { NetworkService::tcp_listen(self, local_address, local_port, backlog).await }
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
                DHCP_CLIENT_PORT,
                Ipv4Address::BROADCAST,
                DHCP_SERVER_PORT,
                &payload[..len],
                identification,
                now,
            )
            .map(|_| ())
            .map_err(|_| NetworkControlError::BackendFault)
    }

    pub(super) fn next_dns_query_id(&mut self) -> u16 {
        let id = self.next_dns_query_id;
        self.next_dns_query_id = self.next_dns_query_id.wrapping_add(1);
        if self.next_dns_query_id == 0 {
            self.next_dns_query_id = 1;
        }
        id
    }

    pub(super) fn send_dns_query(
        &mut self,
        query_id: u16,
        host: &str,
        now: StackInstant,
    ) -> Result<(), DnsError> {
        let Some(server) = self.dns_servers.first().copied() else {
            return Err(DnsError {
                kind: DnsErrorKind::Unavailable,
                detail: NetworkErrorDetail::DnsServersUnavailable,
            });
        };
        let mut payload = [0u8; 512];
        let len = DnsQuestionWriter::new(&mut payload)
            .write_a_query(query_id, host)
            .ok_or(DnsError {
                kind: DnsErrorKind::UnresolvedHost,
                detail: NetworkErrorDetail::DnsQueryStartFailed,
            })?;
        self.stack
            .send_udp_ipv4(
                INTERNAL_DNS_PORT,
                server,
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

    pub(super) fn take_dns_response(
        &mut self,
        query_id: u16,
    ) -> Result<Option<Vec<Ipv4Address>>, DnsError> {
        let socket = self.internal_udp_socket(INTERNAL_DNS_SOCKET_INDEX, INTERNAL_DNS_PORT);
        while let Some(datagram) = self.stack.take_udp(socket).map_err(|_| DnsError {
            kind: DnsErrorKind::Unavailable,
            detail: NetworkErrorDetail::DnsQueryStartFailed,
        })? {
            let Some(message) = DnsResponse::parse(&datagram.bytes).and_then(DnsResponse::message)
            else {
                continue;
            };
            if message.id != query_id {
                continue;
            }
            if message.addresses.is_empty() {
                return Err(DnsError {
                    kind: DnsErrorKind::UnresolvedHost,
                    detail: NetworkErrorDetail::DnsNoIpv4Address,
                });
            }
            return Ok(Some(message.addresses.into_iter().collect()));
        }
        Ok(None)
    }
}
