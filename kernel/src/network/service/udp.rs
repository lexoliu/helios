use super::*;

#[derive(Clone, Copy)]
pub(super) struct UdpSocketState {
    pub(super) stack_socket: helios_netstack::UdpSocketId,
    pub(super) binding: UdpSocketBinding,
}

impl<CpuImpl, Runtime, DeviceImpl> NetworkService<CpuImpl, Runtime, DeviceImpl>
where
    CpuImpl: Cpu + Clone,
    Runtime: ComponentRuntimeState + Sync,
    DeviceImpl: NetworkDevice,
{
    pub async fn udp_bind(&self, local_port: u16) -> Result<UdpBinding<UdpSocketId>, UdpError> {
        self.execute_udp_bind(local_port).await
    }

    pub fn udp_connect(
        &self,
        socket: UdpSocketId,
        remote_address: NetworkIpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        self.execute_udp_connect(socket, remote_address, port)
    }

    pub fn udp_disconnect(&self, socket: UdpSocketId) -> Result<(), UdpError> {
        self.execute_udp_disconnect(socket)
    }

    pub async fn udp_send(
        &self,
        socket: UdpSocketId,
        host: &str,
        port: u16,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<u64, UdpError> {
        self.execute_udp_send(socket, host, port, bytes, timeout_nanos)
            .await
    }

    pub async fn udp_send_address(
        &self,
        socket: UdpSocketId,
        remote_address: NetworkIpAddress,
        port: u16,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<u64, UdpError> {
        self.execute_udp_send_address(socket, remote_address, port, bytes, timeout_nanos)
            .await
    }

    pub async fn udp_receive(
        &self,
        socket: UdpSocketId,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Result<Option<UdpDatagram>, UdpError> {
        self.execute_udp_receive(socket, max_bytes, timeout_nanos)
            .await
    }

    /// Probe a bound socket's receive queue without consuming a datagram.
    ///
    /// Like the TCP probe this drives the device first: a datagram that has
    /// not been demuxed into the stack yet is invisible to the queue check.
    /// A bound UDP socket is always writable — sends are not window-limited.
    pub async fn udp_readiness(&self, socket: UdpSocketId) -> Result<SocketReadiness, UdpError> {
        self.drive_udp().await?;
        self.inner.state.with_handle(socket, |state| {
            let stack_socket = state.udp_socket(socket)?.stack_socket;
            let readable = state
                .stack
                .udp_receive_pending(stack_socket)
                .map_err(|_| UdpError {
                    kind: UdpErrorKind::Unavailable,
                    detail: NetworkErrorDetail::UdpReceiveFailed,
                })?;
            Ok(SocketReadiness {
                readable,
                writable: true,
                hangup: false,
            })
        })
    }

    pub async fn udp_join_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        self.execute_udp_join_multicast_v4(group, interface).await
    }

    pub async fn udp_leave_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        self.execute_udp_leave_multicast_v4(group, interface).await
    }

    pub async fn udp_close(&self, socket: UdpSocketId) {
        self.inner.state.with_handle(socket, |state| {
            state.remove_udp_socket(socket);
        });
    }

    pub(super) async fn execute_udp_bind(
        &self,
        local_port: u16,
    ) -> Result<UdpBinding<UdpSocketId>, UdpError> {
        // local_port == 0 means "allocate ephemeral"; the binding
        // should live on the current processor's shard so its
        // freshly stride-allocated port demuxes RX traffic back
        // here. A non-zero `local_port` is fixed by the caller, so
        // route by the port's stride owner instead.
        if local_port == 0 {
            let processor = self.inner.cpu.current_processor();
            self.inner
                .state
                .with_processor(processor, |state| state.start_udp_bind(local_port))
        } else {
            self.inner
                .state
                .with_local_port(local_port, |state| state.start_udp_bind(local_port))
        }
    }

    pub(super) fn execute_udp_connect(
        &self,
        socket: UdpSocketId,
        remote_address: NetworkIpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        self.synchronize_control_plane();
        let destination = map_network_ip_address(remote_address);
        self.inner.state.with_handle(socket, |state| {
            state.connect_udp_socket(socket, destination, port)
        })
    }

    pub(super) fn execute_udp_disconnect(&self, socket: UdpSocketId) -> Result<(), UdpError> {
        self.inner
            .state
            .with_handle(socket, |state| state.disconnect_udp_socket(socket))
    }

    pub(super) async fn execute_udp_send(
        &self,
        socket: UdpSocketId,
        host: &str,
        port: u16,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<u64, UdpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        let destination = self.resolve_host_udp(host, deadline_nanos).await?;
        self.execute_udp_send_ipv4(socket, destination, port, bytes, deadline_nanos)
            .await
    }

    pub(super) async fn execute_udp_send_address(
        &self,
        socket: UdpSocketId,
        remote_address: NetworkIpAddress,
        port: u16,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<u64, UdpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        match remote_address {
            NetworkIpAddress::Ipv4(destination) => {
                self.execute_udp_send_ipv4(
                    socket,
                    map_kernel_ipv4_address(destination),
                    port,
                    bytes,
                    deadline_nanos,
                )
                .await
            }
            NetworkIpAddress::Ipv6(destination) => {
                self.execute_udp_send_ip(socket, IpAddress::Ipv6(destination), port, bytes)
                    .await
            }
        }
    }

    pub(super) async fn execute_udp_send_ipv4(
        &self,
        socket: UdpSocketId,
        destination: Ipv4Address,
        port: u16,
        bytes: &[u8],
        deadline_nanos: u64,
    ) -> Result<u64, UdpError> {
        self.wait_for_ipv4_udp(deadline_nanos).await?;
        self.execute_udp_send_ip(socket, IpAddress::Ipv4(destination), port, bytes)
            .await
    }

    pub(super) async fn execute_udp_send_ip(
        &self,
        socket: UdpSocketId,
        destination: IpAddress,
        port: u16,
        bytes: &[u8],
    ) -> Result<u64, UdpError> {
        self.synchronize_control_plane();
        let now = StackInstant::from_nanos(self.now_nanos());
        let written = self.inner.state.with_handle(socket, |state| {
            state.try_send_udp(socket, destination, port, bytes, now)
        })?;
        Ok(u64::try_from(written).unwrap_or_else(|_| panic!("udp write length exceeds u64")))
    }

    pub(super) async fn execute_udp_receive(
        &self,
        socket: UdpSocketId,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Result<Option<UdpDatagram>, UdpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        loop {
            self.drive_udp().await?;
            let received = self.inner.state.with_handle(socket, |state| {
                state.poll_udp_receive(socket, max_bytes as usize)
            })?;
            match received {
                Some(datagram) => return Ok(Some(datagram)),
                None => {
                    if self.now_nanos() >= deadline_nanos {
                        return Err(UdpError {
                            kind: UdpErrorKind::Timeout,
                            detail: NetworkErrorDetail::UdpReceiveTimeout,
                        });
                    }
                    self.wait_for_progress(NETWORK_PROGRESS_WAIT).await;
                }
            }
        }
    }

    pub(super) async fn execute_udp_join_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        self.inner
            .state
            .with_mut(|state| state.join_multicast_v4(group, interface))
    }

    pub(super) async fn execute_udp_leave_multicast_v4(
        &self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        self.inner
            .state
            .with_mut(|state| state.leave_multicast_v4(group, interface))
    }

    pub(super) async fn wait_for_ipv4_udp(&self, deadline_nanos: u64) -> Result<(), UdpError> {
        self.wait_for_ipv4_configured(
            deadline_nanos,
            udp_configuration_timeout,
            udp_configuration_error,
        )
        .await
    }

    pub(super) async fn resolve_host_udp(
        &self,
        host: &str,
        deadline_nanos: u64,
    ) -> Result<Ipv4Address, UdpError> {
        if let Some(address) = parse_ipv4(host) {
            return Ok(address);
        }
        let timeout_nanos = deadline_nanos.saturating_sub(self.now_nanos());
        let addresses = self
            .execute_dns_resolve(host, timeout_nanos)
            .await
            .map_err(|error| UdpError {
                kind: match error.kind {
                    DnsErrorKind::Timeout => UdpErrorKind::Timeout,
                    DnsErrorKind::Unavailable | DnsErrorKind::Internal => UdpErrorKind::Unavailable,
                    DnsErrorKind::UnresolvedHost => UdpErrorKind::UnresolvedHost,
                },
                detail: error.detail,
            })?;
        addresses
            .into_iter()
            .next()
            .map(map_kernel_ipv4_address)
            .ok_or(UdpError {
                kind: UdpErrorKind::UnresolvedHost,
                detail: NetworkErrorDetail::DnsNoIpv4Address,
            })
    }

    pub(super) async fn drive_udp(&self) -> Result<(), UdpError> {
        self.drive_network(NetworkPollSource::Udp)
            .await
            .map_err(|error| UdpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))
    }
}

impl NetworkShard {
    pub(super) fn start_udp_bind(
        &mut self,
        local_port: u16,
    ) -> Result<UdpBinding<UdpSocketId>, UdpError> {
        let local_port = if local_port == 0 {
            self.allocate_udp_local_port()?
        } else if self
            .stack
            .udp_binding_free(UdpSocketBinding::wildcard(local_port))
        {
            local_port
        } else {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpPortInUse,
            });
        };
        let binding = UdpSocketBinding::wildcard(local_port);
        let stack_socket = self.stack.open_udp(binding).map_err(map_udp_bind_error)?;
        Ok(UdpBinding {
            socket: self.insert_udp_socket(stack_socket, binding),
            local_port,
        })
    }

    pub(super) fn connect_udp_socket(
        &mut self,
        socket: UdpSocketId,
        destination: IpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        let slot = self.decode_handle_slot(socket.0.get());
        let state = *self.udp_socket(socket)?;
        let local = self
            .udp_local_endpoint(state.binding.local_port, destination)
            .map_err(map_udp_connect_error)?;
        let remote = UdpEndpoint {
            address: destination,
            port,
        };
        let binding = UdpSocketBinding::connected(local, remote);
        self.stack
            .rebind_udp(state.stack_socket, binding)
            .map_err(map_udp_connect_error)?;
        let state = self
            .udp_sockets
            .get_mut(slot)
            .unwrap_or_else(|| panic!("UDP socket disappeared during connect"));
        state.binding = binding;
        Ok(())
    }

    pub(super) fn disconnect_udp_socket(&mut self, socket: UdpSocketId) -> Result<(), UdpError> {
        let slot = self.decode_handle_slot(socket.0.get());
        let state = *self.udp_socket(socket)?;
        if state.binding.remote.is_none() {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpDisconnectFailed,
            });
        }
        let binding = UdpSocketBinding::wildcard(state.binding.local_port);
        self.stack
            .rebind_udp(state.stack_socket, binding)
            .map_err(map_udp_disconnect_error)?;
        let state = self
            .udp_sockets
            .get_mut(slot)
            .unwrap_or_else(|| panic!("UDP socket disappeared during disconnect"));
        state.binding = binding;
        Ok(())
    }

    pub(super) fn try_send_udp(
        &mut self,
        socket: UdpSocketId,
        destination: IpAddress,
        port: u16,
        bytes: &[u8],
        now: StackInstant,
    ) -> Result<usize, UdpError> {
        let stack_socket = self.udp_socket(socket)?.stack_socket;
        if let Some(error) = self
            .stack
            .take_udp_error(stack_socket)
            .map_err(|_| UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UnknownUdpSocket,
            })?
        {
            return Err(map_udp_socket_error(error));
        }
        self.stack
            .send_udp(
                stack_socket,
                destination,
                port,
                bytes,
                socket.0.get() as u16,
                now,
            )
            .map_err(|error| {
                tracing::debug!(?error, "failed to queue UDP datagram");
                map_udp_send_error(error)
            })
    }

    pub(super) fn poll_udp_receive(
        &mut self,
        socket: UdpSocketId,
        max_bytes: usize,
    ) -> Result<Option<UdpDatagram>, UdpError> {
        let stack_socket = self.udp_socket(socket)?.stack_socket;
        if let Some(error) = self
            .stack
            .take_udp_error(stack_socket)
            .map_err(|_| UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UnknownUdpSocket,
            })?
        {
            return Err(map_udp_socket_error(error));
        }
        let Some(datagram) = self.stack.take_udp(stack_socket).map_err(|_| UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpReceiveFailed,
        })?
        else {
            return Ok(None);
        };
        Ok(Some(UdpDatagram {
            address: map_ip_address(datagram.source),
            port: datagram.source_port,
            bytes: limit_udp_datagram_bytes(datagram.bytes, max_bytes),
        }))
    }

    pub(super) fn remove_udp_socket(&mut self, socket: UdpSocketId) {
        let slot = self.decode_handle_slot(socket.0.get());
        if let Some(socket) = self.udp_sockets.remove(slot) {
            self.stack
                .remove_udp_socket(socket.stack_socket)
                .unwrap_or_else(|_| panic!("UDP handle referenced an unknown stack socket"));
        }
    }

    pub(super) fn insert_udp_socket(
        &mut self,
        stack_socket: helios_netstack::UdpSocketId,
        binding: UdpSocketBinding,
    ) -> UdpSocketId {
        let slot = self.udp_sockets.insert(UdpSocketState {
            stack_socket,
            binding,
        });
        UdpSocketId(
            NonZeroU32::new(self.encode_handle_id(slot))
                .unwrap_or_else(|| panic!("udp socket ids must never be zero")),
        )
    }

    pub(super) fn udp_socket(&self, socket: UdpSocketId) -> Result<&UdpSocketState, UdpError> {
        let slot = self.decode_handle_slot(socket.0.get());
        self.udp_sockets.get(slot).ok_or_else(|| UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownUdpSocket,
        })
    }

    pub(super) fn internal_udp_socket(
        &self,
        slot: usize,
        expected_port: u16,
    ) -> helios_netstack::UdpSocketId {
        let state = self
            .udp_sockets
            .get(slot)
            .unwrap_or_else(|| panic!("internal UDP socket slot {slot} is not initialized"));
        assert_eq!(
            state.binding.local_port, expected_port,
            "internal UDP socket slot {slot} has unexpected local port"
        );
        state.stack_socket
    }

    pub(super) fn allocate_udp_local_port(&mut self) -> Result<u16, UdpError> {
        for _ in 0..self.ephemeral_port_attempts() {
            let candidate = self.next_udp_local_port;
            self.next_udp_local_port = self.advance_ephemeral_port(self.next_udp_local_port);
            if self.is_udp_local_port_free(candidate) {
                return Ok(candidate);
            }
        }
        Err(UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpNoEphemeralPorts,
        })
    }

    pub(super) fn is_udp_local_port_free(&self, port: u16) -> bool {
        self.stack
            .udp_binding_free(UdpSocketBinding::wildcard(port))
    }

    pub(super) fn udp_local_endpoint(
        &self,
        local_port: u16,
        destination: IpAddress,
    ) -> Result<UdpEndpoint, StackError> {
        let address = match destination {
            IpAddress::Ipv4(destination) => self
                .stack
                .source_ipv4_address_for(destination)
                .map(IpAddress::Ipv4)
                .or_else(|| {
                    self.stack
                        .primary_ipv4_address()
                        .map(|cidr| IpAddress::Ipv4(cidr.address()))
                }),
            IpAddress::Ipv6(destination) => self
                .stack
                .source_ipv6_address_for(destination)
                .map(IpAddress::Ipv6)
                .or_else(|| {
                    self.stack
                        .primary_ipv6_address()
                        .map(|cidr| IpAddress::Ipv6(cidr.address()))
                }),
        }
        .ok_or(StackError::Unroutable)?;
        Ok(UdpEndpoint {
            address,
            port: local_port,
        })
    }

    pub(super) fn join_multicast_v4(
        &mut self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        let group = map_kernel_ipv4_address(group);
        let interface = self.require_multicast_interface(interface)?;
        self.stack
            .join_ipv4_multicast(group, interface)
            .map_err(multicast_join_error)
    }

    pub(super) fn leave_multicast_v4(
        &mut self,
        group: KernelIpv4Address,
        interface: KernelIpv4Address,
    ) -> Result<(), UdpError> {
        let group = map_kernel_ipv4_address(group);
        let interface = self.require_multicast_interface(interface)?;
        self.stack
            .leave_ipv4_multicast(group, interface)
            .map_err(multicast_leave_error)
    }

    pub(super) fn require_multicast_interface(
        &self,
        interface: KernelIpv4Address,
    ) -> Result<Ipv4Address, UdpError> {
        let Some(cidr) = self.stack.primary_ipv4_address() else {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpMulticastInterfaceUnavailable,
            });
        };
        if interface.octets() == [0, 0, 0, 0] {
            return Ok(cidr.address());
        }
        let interface = map_kernel_ipv4_address(interface);
        if cidr.address() != interface {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpMulticastInterfaceUnavailable,
            });
        }
        Ok(interface)
    }
}

pub(super) fn limit_udp_datagram_bytes(bytes: UdpPayload, max_bytes: usize) -> Bytes {
    bytes.into_limited_bytes(max_bytes)
}
