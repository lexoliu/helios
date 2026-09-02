use super::*;

pub(super) struct TcpListenerState {
    pub(super) stack_socket: helios_netstack::SocketId,
    pub(super) local_port: u16,
}

#[derive(Debug)]
pub(super) enum TcpConnectProgress {
    Pending,
    Connected,
}

pub(super) enum TcpReadProgress {
    Pending,
    Data(Bytes),
    Eof,
}

impl<CpuImpl, Runtime, DeviceImpl> NetworkService<CpuImpl, Runtime, DeviceImpl>
where
    CpuImpl: Cpu + Clone,
    Runtime: ComponentRuntimeState + Sync,
    DeviceImpl: NetworkDevice,
{
    pub async fn tcp_connect(
        &self,
        host: &str,
        port: u16,
        timeout_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        self.tcp_connect_from(
            host,
            port,
            0,
            helios_netstack::DEFAULT_HOP_LIMIT,
            timeout_nanos,
        )
        .await
    }

    pub async fn tcp_connect_from(
        &self,
        host: &str,
        port: u16,
        local_port: u16,
        hop_limit: u8,
        timeout_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        self.execute_tcp_connect(host, port, local_port, hop_limit, timeout_nanos)
            .await
    }

    pub async fn tcp_connect_address(
        &self,
        remote_address: NetworkIpAddress,
        port: u16,
        local_port: u16,
        hop_limit: u8,
        timeout_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        self.execute_tcp_connect_address(
            map_network_ip_address(remote_address),
            port,
            local_port,
            hop_limit,
            timeout_nanos,
        )
        .await
    }

    pub async fn tcp_listen(
        &self,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: u16,
        hop_limit: u8,
    ) -> Result<TcpListener<TcpListenerId>, TcpError> {
        self.execute_tcp_listen(local_address, local_port, backlog, hop_limit)
            .await
    }

    /// Retargets a connected stream's IPv4 TTL / IPv6 hop limit.
    pub fn tcp_set_hop_limit(&self, stream: TcpStreamId, hop_limit: u8) -> Result<(), TcpError> {
        self.inner
            .state
            .with_handle(stream, |state| state.set_tcp_hop_limit(stream, hop_limit))
    }

    /// Retargets a listener's hop limit. Connections accepted after this
    /// inherit the new value; ones already queued keep the old one.
    pub fn tcp_listener_set_hop_limit(
        &self,
        listener: TcpListenerId,
        hop_limit: u8,
    ) -> Result<(), TcpError> {
        self.inner.state.with_handle(listener, |state| {
            state.set_tcp_listener_hop_limit(listener, hop_limit)
        })
    }

    pub async fn tcp_accept(
        &self,
        listener: TcpListenerId,
        timeout_nanos: u64,
    ) -> Result<TcpAccepted<TcpStreamId>, TcpError> {
        self.execute_tcp_accept(listener, timeout_nanos).await
    }

    pub async fn tcp_write_all(
        &self,
        stream: TcpStreamId,
        bytes: &[u8],
        timeout_nanos: u64,
    ) -> Result<(), TcpError> {
        self.execute_tcp_write_all_bytes(stream, Bytes::copy_from_slice(bytes), timeout_nanos)
            .await
    }

    pub async fn tcp_write_all_bytes(
        &self,
        stream: TcpStreamId,
        bytes: Bytes,
        timeout_nanos: u64,
    ) -> Result<(), TcpError> {
        self.execute_tcp_write_all_bytes(stream, bytes, timeout_nanos)
            .await
    }

    pub async fn tcp_read(
        &self,
        stream: TcpStreamId,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Result<Option<Bytes>, TcpError> {
        self.execute_tcp_read(stream, max_bytes, timeout_nanos)
            .await
    }

    pub async fn tcp_read_into(
        &self,
        stream: TcpStreamId,
        buffer: RegisteredTcpReadBuffer<'_>,
        timeout_nanos: u64,
    ) -> Result<Option<usize>, TcpError> {
        self.execute_tcp_read_into(stream, buffer, timeout_nanos)
            .await
    }

    /// Probe a connected stream's readiness without consuming buffered bytes.
    ///
    /// The stack only advances when the device is driven, so a probe has to
    /// drive once before reading state; otherwise a socket whose data is
    /// still sitting in the RX ring reports "not ready" forever. The read
    /// probe itself is a zero-length `tcp_read`, which reports whether the
    /// receive queue is non-empty (or the peer half-closed) and leaves the
    /// queue intact.
    pub async fn tcp_readiness(&self, stream: TcpStreamId) -> Result<SocketReadiness, TcpError> {
        self.drive_tcp().await?;
        let now = StackInstant::from_nanos(self.now_nanos());
        self.inner.state.with_handle(stream, |state| {
            let socket = state.tcp_socket(stream)?;
            let read = state.stack.tcp_read(socket, 0, now).map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpReceiveFailed,
            })?;
            let (readable, hangup) = match read {
                TcpReadState::Data(_) => (true, false),
                TcpReadState::Eof => (true, true),
                TcpReadState::Pending => (false, false),
            };
            // Writability follows the send side, which outlives the read
            // side: a peer that half-closed still accepts our data. A full
            // transmit queue clears it until ACKs free capacity.
            let writable = state.stack.tcp_send_ready(socket).map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UnknownTcpStream,
            })?;
            Ok(SocketReadiness {
                readable,
                writable,
                hangup,
            })
        })
    }

    /// Probe a listener's accept queue without consuming a connection.
    pub async fn tcp_listener_readiness(
        &self,
        listener: TcpListenerId,
    ) -> Result<SocketReadiness, TcpError> {
        self.drive_tcp().await?;
        self.inner.state.with_handle(listener, |state| {
            let stack_socket = state.tcp_listener(listener)?.stack_socket;
            let readable = state
                .stack
                .tcp_accept_pending(stack_socket)
                .map_err(|_| TcpError {
                    kind: TcpErrorKind::Unavailable,
                    detail: NetworkErrorDetail::TcpListenerClosedUnexpectedly,
                })?;
            Ok(SocketReadiness {
                readable,
                writable: false,
                hangup: false,
            })
        })
    }

    pub async fn tcp_shutdown_send(&self, stream: TcpStreamId) -> Result<(), TcpError> {
        self.inner
            .state
            .with_handle(stream, |state| state.shutdown_tcp_send(stream))?;
        self.drive_tcp().await
    }

    pub async fn tcp_close(&self, stream: TcpStreamId) {
        self.inner.state.with_handle(stream, |state| {
            state.remove_tcp_stream(stream);
        });
    }

    pub(super) async fn execute_tcp_connect(
        &self,
        host: &str,
        port: u16,
        local_port: u16,
        hop_limit: u8,
        timeout_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        let candidates = self.resolve_host_tcp(host, deadline_nanos).await?;
        // Every candidate shares the caller's deadline: a refused or
        // unreachable answer hands whatever time is left to the next
        // address instead of restarting the timeout.
        attempt_each_address(&candidates, move |destination| {
            self.execute_tcp_connect_address_until(
                destination,
                port,
                local_port,
                hop_limit,
                deadline_nanos,
            )
        })
        .await
    }

    pub(super) async fn execute_tcp_connect_address(
        &self,
        destination: IpAddress,
        port: u16,
        local_port: u16,
        hop_limit: u8,
        timeout_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        self.execute_tcp_connect_address_until(
            destination,
            port,
            local_port,
            hop_limit,
            deadline_nanos,
        )
        .await
    }

    pub(super) async fn execute_tcp_connect_address_until(
        &self,
        destination: IpAddress,
        port: u16,
        local_port: u16,
        hop_limit: u8,
        deadline_nanos: u64,
    ) -> Result<TcpStreamId, TcpError> {
        if matches!(destination, IpAddress::Ipv4(_)) {
            self.wait_for_ipv4_tcp(deadline_nanos).await?;
        }
        // A caller-selected local port already has a deterministic
        // shard owner; route there so RX demux and handle routing agree.
        // Port 0 allocates from the current processor's shard.
        let stream = if local_port == 0 {
            let processor = self.inner.cpu.current_processor();
            self.inner.state.with_processor(processor, |state| {
                state.start_tcp_connect(destination, port, local_port, hop_limit)
            })
        } else {
            self.inner.state.with_local_port(local_port, |state| {
                state.start_tcp_connect(destination, port, local_port, hop_limit)
            })
        }?;

        loop {
            self.drive_tcp().await?;
            let now_nanos = self.now_nanos();
            let poll_connect = self.inner.state.with_handle(stream, |state| {
                match state.poll_tcp_connect(stream) {
                    Ok(TcpConnectProgress::Connected) => Ok(TcpConnectProgress::Connected),
                    Ok(TcpConnectProgress::Pending) => {
                        if now_nanos >= deadline_nanos {
                            state.remove_tcp_stream(stream);
                            Err(TcpError {
                                kind: TcpErrorKind::Timeout,
                                detail: NetworkErrorDetail::TcpConnectTimeout,
                            })
                        } else {
                            Ok(TcpConnectProgress::Pending)
                        }
                    }
                    Err(error) => {
                        state.remove_tcp_stream(stream);
                        Err(error)
                    }
                }
            })?;
            if matches!(poll_connect, TcpConnectProgress::Connected) {
                return Ok(stream);
            }
            self.wait_for_tcp_progress(deadline_nanos).await;
        }
    }

    pub(super) async fn execute_tcp_listen(
        &self,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: u16,
        hop_limit: u8,
    ) -> Result<TcpListener<TcpListenerId>, TcpError> {
        let backlog = TcpListenBacklog::try_new(backlog).map_err(|_| TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpListenStartFailed,
        })?;
        // Listener lives on the shard that owns its local port: a
        // well-known port (< EPHEMERAL_PORT_START) goes to shard 0,
        // an explicit ephemeral port stride-maps to its owner. RX
        // demux for the same port routes back here.
        self.inner.state.with_local_port(local_port, |state| {
            state.start_tcp_listen(local_address, local_port, backlog, hop_limit)
        })
    }

    pub(super) async fn execute_tcp_accept(
        &self,
        listener: TcpListenerId,
        timeout_nanos: u64,
    ) -> Result<TcpAccepted<TcpStreamId>, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        loop {
            self.drive_tcp().await?;
            let accepted = self
                .inner
                .state
                .with_handle(listener, |state| state.poll_tcp_accept(listener))?;
            if let Some(accepted) = accepted {
                return Ok(accepted);
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(TcpError {
                    kind: TcpErrorKind::Timeout,
                    detail: NetworkErrorDetail::TcpAcceptTimeout,
                });
            }
            self.wait_for_tcp_progress(deadline_nanos).await;
        }
    }

    pub(super) async fn execute_tcp_write_all_bytes(
        &self,
        stream: TcpStreamId,
        mut bytes: Bytes,
        timeout_nanos: u64,
    ) -> Result<(), TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        while !bytes.is_empty() {
            self.drive_tcp().await?;
            let written = self.inner.state.with_handle(stream, |state| {
                state.try_write_tcp_bytes(stream, &mut bytes)
            })?;
            if written != 0 {
                continue;
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(TcpError {
                    kind: TcpErrorKind::Timeout,
                    detail: NetworkErrorDetail::TcpWriteTimeout,
                });
            }
            self.wait_for_tcp_progress(deadline_nanos).await;
        }
        Ok(())
    }

    pub(super) async fn execute_tcp_read(
        &self,
        stream: TcpStreamId,
        max_bytes: u32,
        timeout_nanos: u64,
    ) -> Result<Option<Bytes>, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        let max_bytes = max_bytes as usize;
        loop {
            match self.poll_tcp_read_once(stream, max_bytes, TcpReadPhasePrefix::Initial)? {
                TcpReadProgress::Data(bytes) => return Ok(Some(bytes)),
                TcpReadProgress::Eof => return Ok(None),
                TcpReadProgress::Pending => {}
            }

            let drive_started = self.profile_start();
            let read = self.drive_tcp_read_burst(stream, max_bytes).await?;
            self.record_network_profile("tcp-read-drive-network", drive_started);
            match read {
                TcpReadProgress::Data(bytes) => return Ok(Some(bytes)),
                TcpReadProgress::Eof => return Ok(None),
                TcpReadProgress::Pending => {}
            }
            match self
                .poll_tcp_read_without_interrupt_sleep(stream, max_bytes, deadline_nanos)
                .await?
            {
                TcpReadProgress::Data(bytes) => return Ok(Some(bytes)),
                TcpReadProgress::Eof => return Ok(None),
                TcpReadProgress::Pending => {}
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(TcpError {
                    kind: TcpErrorKind::Timeout,
                    detail: NetworkErrorDetail::TcpReadTimeout,
                });
            }
            let wait_started = self.profile_start();
            self.wait_for_tcp_progress(deadline_nanos).await;
            self.record_network_profile("tcp-read-wait", wait_started);
        }
    }

    pub(super) async fn execute_tcp_read_into(
        &self,
        stream: TcpStreamId,
        mut buffer: RegisteredTcpReadBuffer<'_>,
        timeout_nanos: u64,
    ) -> Result<Option<usize>, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        let max_bytes = buffer.capacity();
        loop {
            match self.poll_tcp_read_into_once(
                stream,
                &mut buffer,
                TcpReadPhasePrefix::IntoInitial,
            )? {
                TcpReadIntoState::Data(bytes) => return Ok(Some(bytes)),
                TcpReadIntoState::Eof => return Ok(None),
                TcpReadIntoState::Pending => {}
            }

            let drive_started = self.profile_start();
            self.drive_tcp_read_network_burst(max_bytes).await?;
            self.record_network_profile("tcp-read-into-drive-network", drive_started);
            match self.poll_tcp_read_into_once(
                stream,
                &mut buffer,
                TcpReadPhasePrefix::IntoAfterDrive,
            )? {
                TcpReadIntoState::Data(bytes) => return Ok(Some(bytes)),
                TcpReadIntoState::Eof => return Ok(None),
                TcpReadIntoState::Pending => {}
            }
            match self
                .poll_tcp_read_into_without_interrupt_sleep(stream, &mut buffer, deadline_nanos)
                .await?
            {
                TcpReadIntoState::Data(bytes) => return Ok(Some(bytes)),
                TcpReadIntoState::Eof => return Ok(None),
                TcpReadIntoState::Pending => {}
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(TcpError {
                    kind: TcpErrorKind::Timeout,
                    detail: NetworkErrorDetail::TcpReadTimeout,
                });
            }
            let wait_started = self.profile_start();
            self.wait_for_tcp_progress(deadline_nanos).await;
            self.record_network_profile("tcp-read-into-wait", wait_started);
        }
    }

    pub(super) fn poll_tcp_read_once(
        &self,
        stream: TcpStreamId,
        max_bytes: usize,
        profile_prefix: TcpReadPhasePrefix,
    ) -> Result<TcpReadProgress, TcpError> {
        let started = self.profile_start();
        let now = StackInstant::from_nanos(self.now_nanos());
        let read = self
            .inner
            .state
            .with_handle(stream, |state| state.poll_tcp_read(stream, max_bytes, now))?;
        self.record_tcp_read_progress(profile_prefix, started, &read);
        Ok(read)
    }

    pub(super) fn poll_tcp_read_into_once(
        &self,
        stream: TcpStreamId,
        buffer: &mut RegisteredTcpReadBuffer<'_>,
        profile_prefix: TcpReadPhasePrefix,
    ) -> Result<TcpReadIntoState, TcpError> {
        let started = self.profile_start();
        let now = StackInstant::from_nanos(self.now_nanos());
        let read = self.inner.state.with_handle(stream, |state| {
            state.poll_tcp_read_into(stream, buffer, now)
        })?;
        self.record_tcp_read_into_progress(profile_prefix, started, &read);
        Ok(read)
    }

    pub(super) async fn poll_tcp_read_without_interrupt_sleep(
        &self,
        stream: TcpStreamId,
        max_bytes: usize,
        deadline_nanos: u64,
    ) -> Result<TcpReadProgress, TcpError> {
        let capabilities = self.inner.device.capabilities().events;
        if !capabilities.polling || capabilities.interrupts {
            return Ok(TcpReadProgress::Pending);
        }

        for _ in 0..NETWORK_POLLING_TCP_READ_ROUNDS {
            if self.now_nanos() >= deadline_nanos {
                return Ok(TcpReadProgress::Pending);
            }
            let yield_started = self.profile_start();
            crate::yield_now().await;
            self.record_network_profile("tcp-read-polling-yield", yield_started);

            let drive_started = self.profile_start();
            let outcome = self
                .poll_network_once_with_tcp_read(
                    NetworkPollSource::Tcp,
                    Some(NetworkTcpReadProbe {
                        stream,
                        max_bytes,
                        profile_prefix: TcpReadPhasePrefix::Polling,
                    }),
                    true,
                )
                .await
                .map_err(|error| {
                    TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
                })?;
            self.record_network_profile("tcp-read-polling-drive-network", drive_started);
            let read = outcome
                .tcp_read
                .expect("TCP read probe did not produce a read result")?;
            match read {
                ready @ (TcpReadProgress::Data(_) | TcpReadProgress::Eof) => return Ok(ready),
                TcpReadProgress::Pending => {}
            }
            // AArch64/HVF local-loopback profiling at 028b6ef showed fixed
            // polling reads spending 121.5 ms across 8173 network drives while
            // only 115 probes became ready. The cheap Helios syscall path is
            // not the bottleneck there; empty device/stack drives are. Keep
            // spinning only while RX/TX/completion work is actually moving.
            if outcome.progress.is_idle() {
                break;
            }
        }
        Ok(TcpReadProgress::Pending)
    }

    pub(super) async fn poll_tcp_read_into_without_interrupt_sleep(
        &self,
        stream: TcpStreamId,
        buffer: &mut RegisteredTcpReadBuffer<'_>,
        deadline_nanos: u64,
    ) -> Result<TcpReadIntoState, TcpError> {
        let capabilities = self.inner.device.capabilities().events;
        if !capabilities.polling || capabilities.interrupts {
            return Ok(TcpReadIntoState::Pending);
        }

        for _ in 0..NETWORK_POLLING_TCP_READ_ROUNDS {
            if self.now_nanos() >= deadline_nanos {
                return Ok(TcpReadIntoState::Pending);
            }
            let yield_started = self.profile_start();
            crate::yield_now().await;
            self.record_network_profile("tcp-read-into-polling-yield", yield_started);

            let drive_started = self.profile_start();
            let outcome = self
                .poll_network_once(NetworkPollSource::Tcp)
                .await
                .map_err(|error| {
                    TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
                })?;
            self.record_network_profile("tcp-read-into-polling-drive-network", drive_started);
            match self.poll_tcp_read_into_once(stream, buffer, TcpReadPhasePrefix::IntoPolling)? {
                ready @ (TcpReadIntoState::Data(_) | TcpReadIntoState::Eof) => return Ok(ready),
                TcpReadIntoState::Pending => {}
            }
            if outcome.0.is_idle() {
                break;
            }
        }
        Ok(TcpReadIntoState::Pending)
    }

    pub(super) async fn wait_for_ipv4_tcp(&self, deadline_nanos: u64) -> Result<(), TcpError> {
        self.wait_for_ipv4_configured(
            deadline_nanos,
            tcp_configuration_timeout,
            tcp_configuration_error,
        )
        .await
    }

    pub(super) async fn resolve_host_tcp(
        &self,
        host: &str,
        deadline_nanos: u64,
    ) -> Result<ConnectCandidates, TcpError> {
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
            .map_err(|error| TcpError {
                kind: match error.kind {
                    DnsErrorKind::Timeout => TcpErrorKind::Timeout,
                    DnsErrorKind::Unavailable | DnsErrorKind::Internal => TcpErrorKind::Unavailable,
                    DnsErrorKind::UnresolvedHost => TcpErrorKind::UnresolvedHost,
                },
                detail: error.detail,
            })?;
        self.usable_addresses(addresses).ok_or(TcpError {
            kind: TcpErrorKind::UnresolvedHost,
            detail: NetworkErrorDetail::DnsNoIpv4Address,
        })
    }

    pub(super) async fn drive_tcp(&self) -> Result<(), TcpError> {
        self.drive_network(NetworkPollSource::Tcp)
            .await
            .map_err(|error| TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))
    }

    pub(super) async fn drive_tcp_read_burst(
        &self,
        stream: TcpStreamId,
        max_bytes: usize,
    ) -> Result<TcpReadProgress, TcpError> {
        self.drive_tcp_read_network_burst(max_bytes).await?;
        self.poll_tcp_read_once(stream, max_bytes, TcpReadPhasePrefix::AfterDrive)
    }

    pub(super) async fn drive_tcp_read_network_burst(
        &self,
        max_bytes: usize,
    ) -> Result<(), TcpError> {
        let capabilities = self.inner.device.capabilities().events;
        let rounds = if capabilities.polling && max_bytes > self.inner.device.max_frame_len() {
            NETWORK_TCP_READ_BURST_ROUNDS
        } else {
            1
        };
        let outcome = self
            .poll_network_once(NetworkPollSource::Tcp)
            .await
            .map_err(|error| TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))?;
        if rounds > 1 && outcome.0.receive_saturated(outcome.1) {
            let mut deferred_transmit = false;
            for _ in 1..rounds {
                let outcome = self
                    .poll_network_receive_once(NetworkPollSource::Tcp)
                    .await
                    .map_err(|error| {
                        TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
                    })?;
                deferred_transmit |= outcome.0.received_frames != 0;
                if !outcome.0.receive_saturated(outcome.1) {
                    break;
                }
            }
            if deferred_transmit {
                // Receive-only TCP bursts already drove the stack; the hot path
                // only needs to publish generated ACK/window-update frames here.
                // Re-entering a full network poll showed up as extra
                // `tcp-read-drive-network` work in local AArch64/HVF profiles.
                let budget = self.inner.poll.budget();
                let (transmitted, _) = self
                    .submit_network_transmit(NetworkPollSource::Tcp, budget)
                    .await
                    .map_err(|error| {
                        TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed)
                    })?;
                if transmitted != 0 {
                    self.inner.poll.complete(NetworkPollProgress {
                        received_frames: 0,
                        reclaimed_tx: 0,
                        transmitted_frames: transmitted,
                    });
                }
            }
        }
        Ok(())
    }

    pub(super) async fn poll_network_once_with_tcp_read(
        &self,
        source: NetworkPollSource,
        tcp_read_probe: Option<NetworkTcpReadProbe>,
        submit_transmit: bool,
    ) -> Result<NetworkPollOutcome, IoError> {
        self.synchronize_control_plane();
        let budget = self.inner.poll.budget();

        let reclaim_started = self.profile_start();
        let reclaimed = match self
            .inner
            .device
            .reclaim_transmit_completions_immediate(budget.tx_completions)?
        {
            Some(reclaimed) => reclaimed,
            None => {
                self.inner
                    .device
                    .reclaim_transmit_completions(budget.tx_completions)
                    .await?
            }
        };
        if reclaimed != 0 {
            self.record_network_profile_events(
                source.tx_reclaim_phase(),
                reclaim_started,
                reclaimed,
            );
        }

        let mut received = 0usize;
        let mut received_bytes = 0usize;
        let receive_started = self.profile_start();
        // `stack_rx_budget` is fixed at Stack construction so we read
        // it from the cached service-level value (no shard lock).
        // Backpressure remains a per-shard query because it tracks
        // live receive-window state.
        let stack_rx_budget = self.inner.state.with(|state| {
            if state.stack.receive_backpressured() {
                0
            } else {
                self.inner.stack_rx_budget
            }
        });
        loop {
            let remaining_rx_budget = budget
                .rx_frames
                .min(stack_rx_budget)
                .saturating_sub(received);
            if stack_rx_budget == 0 || remaining_rx_budget == 0 {
                break;
            }

            let receive_limit = remaining_rx_budget.min(NETWORK_RX_BATCH_FRAMES);
            let mut frames: [Option<Bytes>; NETWORK_RX_BATCH_FRAMES] =
                core::array::from_fn(|_| None);
            let poll_pair = usize::from(self.inner.cpu.current_processor().id());
            let received_batch = match self
                .inner
                .device
                .try_receive_frames_immediate_on(poll_pair, &mut frames[..receive_limit])?
            {
                Some(received_batch) => received_batch,
                None => {
                    let mut received_batch = 0usize;
                    for frame in &mut frames[..receive_limit] {
                        let Some(received_frame) = self.inner.device.try_receive_frame().await?
                        else {
                            break;
                        };
                        *frame = Some(received_frame);
                        received_batch += 1;
                    }
                    received_batch
                }
            };
            if received_batch == 0 {
                break;
            }

            let mut receive_backpressured = false;
            let received_at = StackInstant::from_nanos(self.now_nanos());
            // Demux each frame to the shard owning its destination
            // port. The previous single-shard path locked
            // `shard_for_default` once per batch; under multi-shard
            // we lock the owning shard per-frame so different
            // ports can be processed in parallel by other CPUs and
            // each shard's Stack only sees the connections it
            // actually owns. Non-IP / non-TCP-UDP frames (ARP,
            // ICMP, malformed) route to shard 0 via
            // `shard_idx_for_port(None, ...)`.
            let shard_count = self.inner.state.shard_count();
            for frame in frames[..received_batch].iter().flatten() {
                if receive_backpressured {
                    break;
                }
                let frame_len = frame.len();
                let port = peek_local_port(frame.as_ref());
                let shard_idx = shard_idx_for_port(port, shard_count);
                let mut shard = self.inner.state.shard_at(shard_idx).lock();
                match shard
                    .stack
                    .receive_frame_bytes_with_backpressure(frame.clone(), received_at)
                {
                    Ok(backpressured) => {
                        received += 1;
                        received_bytes = received_bytes.saturating_add(frame_len);
                        if backpressured {
                            receive_backpressured = true;
                        }
                    }
                    Err(StackError::ReceiveBackpressure) => {
                        receive_backpressured = true;
                    }
                    Err(error) => {
                        tracing::debug!(?error, "dropped malformed network frame");
                        received += 1;
                        received_bytes = received_bytes.saturating_add(frame_len);
                    }
                }
                shard.drain_control_events(&self.inner.control);
            }

            if self
                .inner
                .device
                .repost_rx_frames_immediate(&mut frames[..received_batch])?
                .is_none()
            {
                for frame in &mut frames[..received_batch] {
                    drop(frame.take());
                }
            }

            if receive_backpressured {
                break;
            }
        }
        self.record_network_profile_events_bytes(
            source.rx_drain_phase(),
            receive_started,
            received,
            received_bytes,
        );

        let tcp_started = self.profile_start();
        let now = StackInstant::from_nanos(self.now_nanos());
        let mut tcp_read = None;
        let mut tcp_read_started = None;
        let mut tcp_read_finished = None;
        // Each shard owns its own TCP connections, so the timer
        // drive must hit every shard's Stack.
        self.inner.state.for_each(|state| {
            state
                .stack
                .drive_tcp(now)
                .unwrap_or_else(|error| tracing::debug!(?error, "failed to drive TCP control"));
        });
        let tcp_finished = self.profile_start();
        if let Some(probe) = tcp_read_probe {
            tcp_read_started = self.profile_start();
            tcp_read = Some(self.inner.state.with_handle(probe.stream, |state| {
                state.poll_tcp_read(probe.stream, probe.max_bytes, now)
            }));
            tcp_read_finished = self.profile_start();
        }
        self.record_network_profile_between(source.tcp_drive_phase(), tcp_started, tcp_finished);
        if let (Some(probe), Some(Ok(read))) = (tcp_read_probe, tcp_read.as_ref()) {
            self.record_tcp_read_progress_between(
                probe.profile_prefix,
                tcp_read_started,
                tcp_read_finished,
                read,
            );
        }

        let (transmitted, _) = if submit_transmit {
            self.submit_network_transmit(source, budget).await?
        } else {
            (0, 0)
        };
        let progress = NetworkPollProgress {
            received_frames: received,
            reclaimed_tx: reclaimed,
            transmitted_frames: transmitted,
        };
        self.inner.poll.complete(progress);
        Ok(NetworkPollOutcome {
            progress,
            budget,
            tcp_read,
        })
    }

    pub(super) async fn wait_for_tcp_progress(&self, operation_deadline_nanos: u64) {
        let now_nanos = self.now_nanos();
        if now_nanos >= operation_deadline_nanos {
            return;
        }
        let next_tcp_deadline = self.inner.state.min_tcp_deadline_nanos();
        let next_deadline = next_tcp_deadline
            .unwrap_or(operation_deadline_nanos)
            .min(operation_deadline_nanos);
        let timer_wait = Duration::from_nanos(next_deadline.saturating_sub(now_nanos));
        self.wait_for_progress(self.progress_wait(timer_wait)).await;
    }

    pub(super) fn record_tcp_read_progress(
        &self,
        prefix: TcpReadPhasePrefix,
        start: Option<NetworkPerfStart>,
        read: &TcpReadProgress,
    ) {
        let (phase, bytes) = match read {
            TcpReadProgress::Pending => (
                tcp_read_profile_phase(prefix, TcpReadPhaseOutcome::Pending),
                0,
            ),
            TcpReadProgress::Data(bytes) => (
                tcp_read_profile_phase(prefix, TcpReadPhaseOutcome::Ready),
                bytes.len(),
            ),
            TcpReadProgress::Eof => (tcp_read_profile_phase(prefix, TcpReadPhaseOutcome::Eof), 0),
        };
        self.record_network_profile_events_bytes(phase, start, 1, bytes);
    }

    pub(super) fn record_tcp_read_progress_between(
        &self,
        prefix: TcpReadPhasePrefix,
        start: Option<NetworkPerfStart>,
        end: Option<NetworkPerfStart>,
        read: &TcpReadProgress,
    ) {
        let (phase, bytes) = match read {
            TcpReadProgress::Pending => (
                tcp_read_profile_phase(prefix, TcpReadPhaseOutcome::Pending),
                0,
            ),
            TcpReadProgress::Data(bytes) => (
                tcp_read_profile_phase(prefix, TcpReadPhaseOutcome::Ready),
                bytes.len(),
            ),
            TcpReadProgress::Eof => (tcp_read_profile_phase(prefix, TcpReadPhaseOutcome::Eof), 0),
        };
        self.record_network_profile_events_bytes_between(phase, start, end, 1, bytes);
    }

    pub(super) fn record_tcp_read_into_progress(
        &self,
        prefix: TcpReadPhasePrefix,
        start: Option<NetworkPerfStart>,
        read: &TcpReadIntoState,
    ) {
        let (phase, bytes) = match read {
            TcpReadIntoState::Pending => (
                tcp_read_profile_phase(prefix, TcpReadPhaseOutcome::Pending),
                0,
            ),
            TcpReadIntoState::Data(bytes) => (
                tcp_read_profile_phase(prefix, TcpReadPhaseOutcome::Ready),
                *bytes,
            ),
            TcpReadIntoState::Eof => (tcp_read_profile_phase(prefix, TcpReadPhaseOutcome::Eof), 0),
        };
        self.record_network_profile_events_bytes(phase, start, 1, bytes);
    }
}

impl NetworkShard {
    pub(super) fn start_tcp_connect(
        &mut self,
        destination: IpAddress,
        port: u16,
        local_port: u16,
        hop_limit: u8,
    ) -> Result<TcpStreamId, TcpError> {
        let local_port = if local_port == 0 {
            self.allocate_tcp_local_port()?
        } else if self.is_tcp_local_port_free(local_port) {
            local_port
        } else {
            return Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpConnectStartFailed,
            });
        };
        let local = match destination {
            IpAddress::Ipv4(_) => self
                .stack
                .primary_ipv4_address()
                .map(|cidr| IpAddress::Ipv4(cidr.address())),
            IpAddress::Ipv6(_) => self
                .stack
                .primary_ipv6_address()
                .map(|cidr| IpAddress::Ipv6(cidr.address())),
        };
        let Some(local) = local else {
            return Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::NetworkServiceUnavailable,
            });
        };
        let socket = self
            .stack
            .open_tcp_connect(
                TcpEndpoint {
                    address: local,
                    port: local_port,
                },
                TcpEndpoint {
                    address: destination,
                    port,
                },
                1,
            )
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpConnectStartFailed,
            })?;
        // Applied before the connect loop drives the stack, so the SYN itself
        // already carries the caller's TTL.
        self.stack
            .set_tcp_hop_limit(socket, hop_limit)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpConnectStartFailed,
            })?;
        Ok(self.insert_tcp_stream(socket))
    }

    pub(super) fn start_tcp_listen(
        &mut self,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: TcpListenBacklog,
        hop_limit: u8,
    ) -> Result<TcpListener<TcpListenerId>, TcpError> {
        let local_port = if local_port == 0 {
            self.allocate_tcp_local_port()?
        } else if self.is_tcp_local_port_free(local_port) {
            local_port
        } else {
            return Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpListenStartFailed,
            });
        };
        let stack_socket = self
            .stack
            .open_tcp_listen(
                TcpEndpoint {
                    address: map_network_ip_address(local_address),
                    port: local_port,
                },
                backlog,
            )
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpListenStartFailed,
            })?;
        // Accepted children inherit the listener's hop limit inside the
        // stack, so their SYN-ACK carries it as well.
        self.stack
            .set_tcp_hop_limit(stack_socket, hop_limit)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpListenStartFailed,
            })?;
        Ok(TcpListener {
            listener: self.insert_tcp_listener(stack_socket, local_port),
            local_port,
        })
    }

    pub(super) fn set_tcp_hop_limit(
        &mut self,
        stream: TcpStreamId,
        hop_limit: u8,
    ) -> Result<(), TcpError> {
        let socket = self.tcp_socket(stream)?;
        self.stack
            .set_tcp_hop_limit(socket, hop_limit)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UnknownTcpStream,
            })
    }

    pub(super) fn set_tcp_listener_hop_limit(
        &mut self,
        listener: TcpListenerId,
        hop_limit: u8,
    ) -> Result<(), TcpError> {
        let stack_socket = self.tcp_listener(listener)?.stack_socket;
        self.stack
            .set_tcp_hop_limit(stack_socket, hop_limit)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpListenerClosedUnexpectedly,
            })
    }

    pub(super) fn poll_tcp_accept(
        &mut self,
        listener: TcpListenerId,
    ) -> Result<Option<TcpAccepted<TcpStreamId>>, TcpError> {
        let stack_socket = self.tcp_listener(listener)?.stack_socket;
        let Some(accepted) = self
            .stack
            .take_tcp_accept(stack_socket)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpListenerClosedUnexpectedly,
            })?
        else {
            return Ok(None);
        };
        let stream = self.insert_tcp_stream(accepted.socket);
        Ok(Some(TcpAccepted {
            stream,
            address: map_ip_address(accepted.remote.address),
            port: accepted.remote.port,
        }))
    }

    pub(super) fn poll_tcp_connect(
        &mut self,
        stream: TcpStreamId,
    ) -> Result<TcpConnectProgress, TcpError> {
        let socket = self.tcp_socket(stream)?;
        match self.stack.tcp_connect_state(socket).map_err(|_| TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownTcpStream,
        })? {
            TcpConnectState::Connected => Ok(TcpConnectProgress::Connected),
            TcpConnectState::Pending => Ok(TcpConnectProgress::Pending),
            TcpConnectState::Closed(error) => Err(map_tcp_connect_terminal_error(error)),
        }
    }

    pub(super) fn try_write_tcp_bytes(
        &mut self,
        stream: TcpStreamId,
        bytes: &mut Bytes,
    ) -> Result<usize, TcpError> {
        let socket = self.tcp_socket(stream)?;
        self.stack
            .tcp_send_bytes(socket, bytes)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpWriteQueueFailed,
            })
    }

    pub(super) fn shutdown_tcp_send(&mut self, stream: TcpStreamId) -> Result<(), TcpError> {
        let socket = self.tcp_socket(stream)?;
        self.stack.tcp_shutdown_send(socket).map_err(|_| TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownTcpStream,
        })
    }

    pub(super) fn poll_tcp_read(
        &mut self,
        stream: TcpStreamId,
        max_bytes: usize,
        now: StackInstant,
    ) -> Result<TcpReadProgress, TcpError> {
        let socket = self.tcp_socket(stream)?;
        match self
            .stack
            .tcp_read(socket, max_bytes, now)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpReceiveFailed,
            })? {
            TcpReadState::Pending => Ok(TcpReadProgress::Pending),
            TcpReadState::Data(bytes) => Ok(TcpReadProgress::Data(bytes)),
            TcpReadState::Eof => Ok(TcpReadProgress::Eof),
        }
    }

    pub(super) fn poll_tcp_read_into(
        &mut self,
        stream: TcpStreamId,
        buffer: &mut RegisteredTcpReadBuffer<'_>,
        now: StackInstant,
    ) -> Result<TcpReadIntoState, TcpError> {
        let socket = self.tcp_socket(stream)?;
        self.stack
            .tcp_read_into(socket, buffer, now)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpReceiveFailed,
            })
    }

    pub(super) fn remove_tcp_stream(&mut self, stream: TcpStreamId) {
        let slot = self.decode_handle_slot(stream.0.get());
        if let Some(socket) = self.tcp_streams.remove(slot) {
            self.stack
                .remove_tcp_socket(socket)
                .unwrap_or_else(|_| panic!("TCP stream referenced an unknown stack socket"));
        }
    }

    pub(super) fn insert_tcp_stream(&mut self, socket: helios_netstack::SocketId) -> TcpStreamId {
        let slot = self.tcp_streams.insert(socket);
        TcpStreamId(
            NonZeroU32::new(self.encode_handle_id(slot))
                .unwrap_or_else(|| panic!("tcp stream ids must never be zero")),
        )
    }

    pub(super) fn insert_tcp_listener(
        &mut self,
        stack_socket: helios_netstack::SocketId,
        local_port: u16,
    ) -> TcpListenerId {
        let slot = self.tcp_listeners.insert(TcpListenerState {
            stack_socket,
            local_port,
        });
        TcpListenerId(
            NonZeroU32::new(self.encode_handle_id(slot))
                .unwrap_or_else(|| panic!("tcp listener ids must never be zero")),
        )
    }

    pub(super) fn tcp_socket(
        &self,
        stream: TcpStreamId,
    ) -> Result<helios_netstack::SocketId, TcpError> {
        let slot = self.decode_handle_slot(stream.0.get());
        self.tcp_streams.get(slot).copied().ok_or_else(|| TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownTcpStream,
        })
    }

    pub(super) fn tcp_listener(
        &self,
        listener: TcpListenerId,
    ) -> Result<&TcpListenerState, TcpError> {
        let slot = self.decode_handle_slot(listener.0.get());
        self.tcp_listeners.get(slot).ok_or_else(|| TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpListenerClosedUnexpectedly,
        })
    }

    pub(super) fn allocate_tcp_local_port(&mut self) -> Result<u16, TcpError> {
        for _ in 0..self.ephemeral_port_attempts() {
            let candidate = self.next_tcp_local_port;
            self.next_tcp_local_port = self.advance_ephemeral_port(self.next_tcp_local_port);
            if self.is_tcp_local_port_free(candidate) {
                return Ok(candidate);
            }
        }
        Err(TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpNoEphemeralPorts,
        })
    }

    pub(super) fn is_tcp_local_port_free(&self, port: u16) -> bool {
        self.tcp_listeners
            .iter()
            .all(|state| state.local_port != port)
    }
}
