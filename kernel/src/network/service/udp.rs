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

    /// Retargets a bound datagram socket's IPv4 TTL / IPv6 hop limit.
    pub fn udp_set_hop_limit(&self, socket: UdpSocketId, hop_limit: u8) -> Result<(), UdpError> {
        self.inner.state.for_each_replica("udp hop limit", |state| {
            state.set_udp_hop_limit(socket, hop_limit)
        })
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
        // Readable means "some replica has a datagram queued", so the
        // probe walks them the same way `receive` does.
        let readable = self
            .inner
            .state
            .find_in_replicas(self.receiving_shard_idx(), |state| {
                let stack_socket = state.udp_socket(socket)?.stack_socket;
                state
                    .stack
                    .udp_receive_pending(stack_socket)
                    .map(|pending| pending.then_some(()))
                    .map_err(|_| UdpError {
                        kind: UdpErrorKind::Unavailable,
                        detail: NetworkErrorDetail::UdpReceiveFailed,
                    })
            })?
            .is_some();
        Ok(SocketReadiness {
            readable,
            writable: true,
            hangup: false,
        })
    }

    /// The shard a receive walk starts at: this processor's own, which
    /// it can drain without touching another CPU's cache.
    fn receiving_shard_idx(&self) -> usize {
        self.inner
            .state
            .shard_idx_for_processor(self.inner.cpu.current_processor())
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
        let slot = ReplicaHandle::from(socket).slot();
        self.inner
            .state
            .for_each_replica("udp close", |state| {
                state.remove_udp_replica(slot);
                Ok::<(), core::convert::Infallible>(())
            })
            .unwrap_or_else(|infallible| match infallible {});
        self.inner.state.udp_slots.release(slot);
    }

    pub(super) async fn execute_udp_bind(
        &self,
        local_port: u16,
    ) -> Result<UdpBinding<UdpSocketId>, UdpError> {
        // A wildcard-bound datagram socket cannot belong to one shard:
        // it accepts datagrams from arbitrary peers, and each of those
        // flows hashes wherever it hashes. The socket is therefore
        // installed on every shard in one slab slot the whole set
        // agrees on, each with its own receive queue.
        let slot = self.inner.state.udp_slots.allocate().ok_or(UdpError {
            kind: UdpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UdpPortInUse,
        })?;
        // The port is chosen once and bound identically on every
        // replica, so which shard answered cannot change the socket's
        // local port.
        let local_port = match local_port {
            0 => self
                .inner
                .state
                .shard_for_default()
                .lock()
                .allocate_udp_local_port(),
            port => Ok(port),
        };
        let local_port = match local_port {
            Ok(port) => port,
            Err(error) => {
                self.inner.state.udp_slots.release(slot);
                return Err(error);
            }
        };
        let install = self.inner.state.install_replica(
            slot,
            |shard, slot| shard.install_udp_bind(slot, local_port),
            NetworkShard::remove_udp_replica,
        );
        if let Err(error) = install {
            self.inner.state.udp_slots.release(slot);
            return Err(error);
        }
        Ok(UdpBinding {
            socket: UdpSocketId(ReplicaHandle::new(slot).get()),
            local_port,
        })
    }

    pub(super) fn execute_udp_connect(
        &self,
        socket: UdpSocketId,
        remote_address: NetworkIpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        self.synchronize_control_plane();
        let destination = map_network_ip_address(remote_address);
        // Every replica is retargeted: a connected socket only accepts
        // datagrams from its peer, and a replica left wildcard-bound
        // would still take one whose hash landed on it.
        self.inner.state.for_each_replica("udp connect", |state| {
            state.connect_udp_socket(socket, destination, port)
        })
    }

    pub(super) fn execute_udp_disconnect(&self, socket: UdpSocketId) -> Result<(), UdpError> {
        self.inner
            .state
            .for_each_replica("udp disconnect", |state| {
                state.disconnect_udp_socket(socket)
            })
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
        let candidates = self.resolve_host_udp(host, deadline_nanos).await?;
        // A name resolved in both families can answer with a family
        // this link cannot source, so the send walks the candidates the
        // same way a connect does, on the same shared deadline.
        attempt_each_address(&candidates, move |destination| async move {
            match destination {
                IpAddress::Ipv4(address) => {
                    self.execute_udp_send_ipv4(socket, address, port, bytes, deadline_nanos)
                        .await
                }
                IpAddress::Ipv6(_) => {
                    self.execute_udp_send_ip(socket, destination, port, bytes)
                        .await
                }
            }
        })
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
        // Every replica carries the same binding, so the frame this
        // produces is identical whichever one sends it; the caller's own
        // shard is the one that costs no cross-processor traffic.
        let written = self
            .inner
            .state
            .shard_at(self.receiving_shard_idx())
            .lock()
            .try_send_udp(socket, destination, port, bytes, now)?;
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
            // A bound socket has a receive queue on every shard, and a
            // datagram lands on whichever one its peer's flow hashes to,
            // so the walk starts at this processor's own and the wait
            // watches the whole set. Nothing here owes a retransmission,
            // so the only other bound is the caller's deadline.
            let wait = self.inner.state.replica_wait();
            let start = self.receiving_shard_idx();
            self.drive_udp().await?;
            let received = self.inner.state.find_in_replicas(start, |state| {
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
                    self.wait_for_shard_progress(wait, self.deadline_wait(deadline_nanos))
                        .await;
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
    ) -> Result<ConnectCandidates, UdpError> {
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
            .map_err(|error| UdpError {
                kind: match error.kind {
                    DnsErrorKind::Timeout => UdpErrorKind::Timeout,
                    DnsErrorKind::Unavailable | DnsErrorKind::Internal => UdpErrorKind::Unavailable,
                    DnsErrorKind::UnresolvedHost => UdpErrorKind::UnresolvedHost,
                },
                detail: error.detail,
            })?;
        self.usable_addresses(addresses).ok_or(UdpError {
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
    /// Installs this shard's replica of a bound datagram socket into
    /// `slot`.
    ///
    /// The slot and the port are decided once for the whole set, so
    /// every replica of the same socket answers to the same handle and
    /// binds the same port.
    pub(super) fn install_udp_bind(
        &mut self,
        slot: usize,
        local_port: u16,
    ) -> Result<(), UdpError> {
        let binding = UdpSocketBinding::wildcard(local_port);
        if !self.stack.udp_binding_free(binding) {
            return Err(UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UdpPortInUse,
            });
        }
        let stack_socket = self.stack.open_udp(binding).map_err(map_udp_bind_error)?;
        self.udp_sockets.insert_at(
            slot,
            UdpSocketState {
                stack_socket,
                binding,
            },
        );
        Ok(())
    }

    pub(super) fn connect_udp_socket(
        &mut self,
        socket: UdpSocketId,
        destination: IpAddress,
        port: u16,
    ) -> Result<(), UdpError> {
        let slot = ReplicaHandle::from(socket).slot();
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

    pub(super) fn set_udp_hop_limit(
        &mut self,
        socket: UdpSocketId,
        hop_limit: u8,
    ) -> Result<(), UdpError> {
        let stack_socket = self.udp_socket(socket)?.stack_socket;
        self.stack
            .set_udp_hop_limit(stack_socket, hop_limit)
            .map_err(|_| UdpError {
                kind: UdpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UnknownUdpSocket,
            })
    }

    pub(super) fn disconnect_udp_socket(&mut self, socket: UdpSocketId) -> Result<(), UdpError> {
        let slot = ReplicaHandle::from(socket).slot();
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

    /// Drops this shard's replica of a bound socket, used both to close
    /// one and to unwind a partial install.
    pub(super) fn remove_udp_replica(&mut self, slot: usize) {
        if let Some(socket) = self.udp_sockets.remove(slot) {
            self.stack
                .remove_udp_socket(socket.stack_socket)
                .unwrap_or_else(|_| panic!("UDP handle referenced an unknown stack socket"));
        }
    }

    pub(super) fn udp_socket(&self, socket: UdpSocketId) -> Result<&UdpSocketState, UdpError> {
        let slot = ReplicaHandle::from(socket).slot();
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
