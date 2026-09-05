use super::*;

/// The receive queue pairs a poll visits, in the order it visits them:
/// the pair belonging to the polling processor first, then every other
/// pair once, wrapping.
///
/// Ownership of a pair is a locality preference, not a claim: a frame
/// the host steered onto another processor's pair still has to be
/// drained by whoever is polling, so the sweep covers all of them.
pub(super) fn receive_pair_order(
    local_pair: usize,
    pair_count: usize,
) -> impl Iterator<Item = usize> {
    assert!(pair_count != 0, "an interface has at least one queue pair");
    (0..pair_count).map(move |offset| (local_pair + offset) % pair_count)
}

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

#[derive(Clone, Copy, Debug)]
pub(super) enum TcpReadIntoProgress {
    Pending,
    Data(usize),
    Eof,
}

/// The kernel-side reading of a netstack close kind.
///
/// A gracefully closed connection is end-of-stream and has no error;
/// every other way a connection ends is one the reader has to be told
/// about immediately, instead of waiting out a deadline that will only
/// ever report a timeout that did not happen.
fn tcp_close_error(close: TcpCloseKind) -> Option<TcpError> {
    let (kind, detail) = match close {
        TcpCloseKind::Graceful => return None,
        TcpCloseKind::Reset => (
            TcpErrorKind::ConnectionReset,
            NetworkErrorDetail::TcpConnectionReset,
        ),
        TcpCloseKind::Aborted => (
            TcpErrorKind::ConnectionAborted,
            NetworkErrorDetail::TcpConnectionAborted,
        ),
        TcpCloseKind::Unresponsive => (
            TcpErrorKind::Timeout,
            NetworkErrorDetail::TcpPeerUnresponsive,
        ),
    };
    Some(TcpError { kind, detail })
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
        // Every replica has to answer with the same hop limit, or which
        // shard accepted a connection would change what it sends.
        self.inner
            .state
            .for_each_replica("tcp listener hop limit", |state| {
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
                // A reset reads as ready-and-hung-up just as a clean
                // close does: the read that follows completes at once,
                // with the connection error instead of end-of-stream.
                TcpReadState::Closed(_) => (true, true),
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
        // Readable means "some replica has a connection queued", so the
        // probe walks them the same way `accept` does.
        let readable = self
            .inner
            .state
            .find_in_replicas(self.accepting_shard_idx(), |state| {
                let stack_socket = state.tcp_listener(listener)?.stack_socket;
                state
                    .stack
                    .tcp_accept_pending(stack_socket)
                    .map(|pending| pending.then_some(()))
                    .map_err(|_| TcpError {
                        kind: TcpErrorKind::Unavailable,
                        detail: NetworkErrorDetail::TcpListenerClosedUnexpectedly,
                    })
            })?
            .is_some();
        Ok(SocketReadiness {
            readable,
            writable: false,
            hangup: false,
        })
    }

    /// The shard an accept walk starts at: this processor's own, which
    /// is the one it can drain without touching another CPU's cache.
    fn accepting_shard_idx(&self) -> usize {
        self.inner
            .state
            .shard_idx_for_processor(self.inner.cpu.current_processor())
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
        // The source address is the same on every shard — the control
        // plane republishes it — so the flow can be hashed before a
        // shard is chosen, and the socket is opened on the shard that
        // will receive its segments.
        let local = self.local_address_for(destination).ok_or(TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::NetworkServiceUnavailable,
        })?;
        let shard_count = self.inner.state.shard_count();
        let local_port = if local_port == 0 {
            // Allocating from this processor's own shard, preferring a
            // port whose answers hash back to it, is what keeps a
            // connection opened here from being received somewhere
            // else. Ownership follows the hash either way.
            self.inner
                .state
                .shard_at(self.accepting_shard_idx())
                .lock()
                .allocate_tcp_local_port_for(local, destination, port, shard_count)?
        } else {
            local_port
        };
        let owner = shard_idx_for_flow(local, local_port, destination, port, shard_count);
        let stream = self.inner.state.shard_at(owner).lock().start_tcp_connect(
            local,
            destination,
            port,
            local_port,
            hop_limit,
        )?;

        loop {
            // Sampled before the handshake state is inspected, so a SYN-ACK
            // another processor drains in between resolves the wait.
            let wait = self.shard_wait_for_handle(stream);
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
            self.wait_for_tcp_progress(wait, deadline_nanos).await;
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
        // A listener cannot belong to one shard. The four-tuple of an
        // inbound connection is not known when the port is opened, so
        // its SYN lands on whichever shard the flow hashes to; the
        // listener is therefore installed on every shard, in one slab
        // slot the whole set agrees on, each with its own accept queue.
        let slot = self.inner.state.listener_slots.allocate().ok_or(TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpListenStartFailed,
        })?;
        // The port is chosen once and then bound identically on every
        // replica: a per-shard choice would give the same listener a
        // different port depending on which shard answered.
        let local_port = match local_port {
            0 => self
                .inner
                .state
                .shard_for_default()
                .lock()
                .allocate_tcp_local_port(),
            port => Ok(port),
        };
        let local_port = match local_port {
            Ok(port) => port,
            Err(error) => {
                self.inner.state.listener_slots.release(slot);
                return Err(error);
            }
        };
        let install = self.inner.state.install_replica(
            slot,
            |shard, slot| {
                shard.install_tcp_listener(slot, local_address, local_port, backlog, hop_limit)
            },
            NetworkShard::remove_tcp_listener,
        );
        if let Err(error) = install {
            self.inner.state.listener_slots.release(slot);
            return Err(error);
        }
        Ok(TcpListener {
            listener: TcpListenerId(ReplicaHandle::new(slot).get()),
            local_port,
        })
    }

    pub(super) async fn execute_tcp_accept(
        &self,
        listener: TcpListenerId,
        timeout_nanos: u64,
    ) -> Result<TcpAccepted<TcpStreamId>, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        loop {
            // A listener has an accept queue on every shard, so the walk
            // starts at this processor's own — the cheapest to drain —
            // and then visits the rest, and the wait watches the whole
            // set because the next SYN's shard is not known until its
            // flow is hashed.
            let wait = self.any_shard_wait();
            let start = self.accepting_shard_idx();
            self.drive_tcp().await?;
            let accepted = self
                .inner
                .state
                .find_in_replicas(start, |state| state.poll_tcp_accept(listener))?;
            if let Some(accepted) = accepted {
                return Ok(accepted);
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(TcpError {
                    kind: TcpErrorKind::Timeout,
                    detail: NetworkErrorDetail::TcpAcceptTimeout,
                });
            }
            self.wait_for_tcp_progress(wait, deadline_nanos).await;
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
            // A blocked write is unblocked by the peer's window opening,
            // which arrives as an ACK on this stream's shard.
            let wait = self.shard_wait_for_handle(stream);
            let written = self.inner.state.with_handle(stream, |state| {
                state.try_write_tcp_bytes(stream, &mut bytes)
            })?;
            // Queuing bytes on a socket puts nothing on the wire and
            // raises no signal: the stack has to be driven for them to
            // become a segment, and no arrival, completion or timer
            // says that a socket now has data to send. So the writer
            // publishes what it queued, on its own task, before it
            // returns or parks — the transmit-side counterpart of the
            // receive drain that raises a shard's arrival in #107.
            //
            // Left to the packet pump instead, the segment waits for
            // the pump's next wake, and an idle pump parks for
            // `DHCP_RETRANSMIT_NANOS`. A request/response exchange then
            // pays a second per round trip, which is #158: 5000 echo
            // round trips could not finish inside the benchmark's 180 s
            // iteration deadline.
            //
            // This is the same one poll per iteration the loop always
            // ran, moved behind the write rather than in front of it. A
            // write that found no room still drives, which is what
            // reclaims the peer's ACKs, and the wait it parks on was
            // sampled before this poll, so a drain that made room here
            // releases the park at once instead of being slept through.
            self.drive_tcp().await?;
            if written != 0 {
                continue;
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(TcpError {
                    kind: TcpErrorKind::Timeout,
                    detail: NetworkErrorDetail::TcpWriteTimeout,
                });
            }
            self.wait_for_tcp_progress(wait, deadline_nanos).await;
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
            let wait = self.shard_wait_for_handle(stream);
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
            self.wait_for_tcp_progress(wait, deadline_nanos).await;
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
            let wait = self.shard_wait_for_handle(stream);
            match self.poll_tcp_read_into_once(
                stream,
                &mut buffer,
                TcpReadPhasePrefix::IntoInitial,
            )? {
                TcpReadIntoProgress::Data(bytes) => return Ok(Some(bytes)),
                TcpReadIntoProgress::Eof => return Ok(None),
                TcpReadIntoProgress::Pending => {}
            }

            let drive_started = self.profile_start();
            self.drive_tcp_read_network_burst(max_bytes).await?;
            self.record_network_profile("tcp-read-into-drive-network", drive_started);
            match self.poll_tcp_read_into_once(
                stream,
                &mut buffer,
                TcpReadPhasePrefix::IntoAfterDrive,
            )? {
                TcpReadIntoProgress::Data(bytes) => return Ok(Some(bytes)),
                TcpReadIntoProgress::Eof => return Ok(None),
                TcpReadIntoProgress::Pending => {}
            }
            match self
                .poll_tcp_read_into_without_interrupt_sleep(stream, &mut buffer, deadline_nanos)
                .await?
            {
                TcpReadIntoProgress::Data(bytes) => return Ok(Some(bytes)),
                TcpReadIntoProgress::Eof => return Ok(None),
                TcpReadIntoProgress::Pending => {}
            }
            if self.now_nanos() >= deadline_nanos {
                return Err(TcpError {
                    kind: TcpErrorKind::Timeout,
                    detail: NetworkErrorDetail::TcpReadTimeout,
                });
            }
            let wait_started = self.profile_start();
            self.wait_for_tcp_progress(wait, deadline_nanos).await;
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
        let read =
            self.inner
                .state
                .with_handle_receive_drain(stream, &self.inner.cpu, |state| {
                    state.poll_tcp_read(stream, max_bytes, now)
                })?;
        self.record_tcp_read_progress(profile_prefix, started, &read);
        Ok(read)
    }

    pub(super) fn poll_tcp_read_into_once(
        &self,
        stream: TcpStreamId,
        buffer: &mut RegisteredTcpReadBuffer<'_>,
        profile_prefix: TcpReadPhasePrefix,
    ) -> Result<TcpReadIntoProgress, TcpError> {
        let started = self.profile_start();
        let now = StackInstant::from_nanos(self.now_nanos());
        let read =
            self.inner
                .state
                .with_handle_receive_drain(stream, &self.inner.cpu, |state| {
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
    ) -> Result<TcpReadIntoProgress, TcpError> {
        let capabilities = self.inner.device.capabilities().events;
        if !capabilities.polling || capabilities.interrupts {
            return Ok(TcpReadIntoProgress::Pending);
        }

        for _ in 0..NETWORK_POLLING_TCP_READ_ROUNDS {
            if self.now_nanos() >= deadline_nanos {
                return Ok(TcpReadIntoProgress::Pending);
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
                ready @ (TcpReadIntoProgress::Data(_) | TcpReadIntoProgress::Eof) => {
                    return Ok(ready);
                }
                TcpReadIntoProgress::Pending => {}
            }
            if outcome.0.is_idle() {
                break;
            }
        }
        Ok(TcpReadIntoProgress::Pending)
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

    /// Drains finished transmit descriptors on every queue pair, which
    /// is what frees the scatter payloads the device was reading in
    /// place.
    ///
    /// Every shard submits to its own queue pair, so a reclaim that
    /// visited only pair zero would leave the other pairs' rings full.
    /// A pair another processor currently holds is skipped: that
    /// processor is draining it, and this poll has nothing to add.
    fn reclaim_transmit_completions(&self, budget: usize) -> Result<usize, IoError> {
        let mut reclaimed = 0usize;
        for shard_idx in 0..self.inner.state.shard_count() {
            if reclaimed >= budget {
                break;
            }
            let Some(completed) = self
                .inner
                .device
                .reclaim_transmit_completions_immediate_on(shard_idx, budget - reclaimed)?
            else {
                continue;
            };
            reclaimed += completed;
        }
        Ok(reclaimed)
    }

    /// Drains received frames from every queue pair, which is what puts
    /// a reply into the shard that is waiting for it.
    ///
    /// Every pair has to be visited, for the receive-side reason the
    /// reclaim above visits every pair on the transmit side: the device
    /// delivers a frame on whichever pair *it* steered the flow to, and
    /// that choice is the host's rather than this processor's. A reply
    /// to a broadcast exchange — a DHCP offer, an ARP reply — is hashed
    /// independently of the request that provoked it, so on a
    /// multi-queue backend it routinely arrives on a pair belonging to
    /// a processor that is not polling. Nothing else then drains it: a
    /// packet pump is a backend's own choice to install, and a backend
    /// without one drives the interface entirely from the operation
    /// waiting on it, on that operation's processor.
    ///
    /// The local pair is visited first, so a processor drains its own
    /// ring before it looks at anyone else's, and a pair another
    /// processor already holds is skipped by the device's `try_lock` —
    /// that processor is draining it and this poll has nothing to add.
    /// `Ok(None)` means every pair was held, which is the same "come
    /// back later" a single-pair drain reports.
    fn receive_frames_immediate(
        &self,
        frames: &mut [Option<RxFrame>],
    ) -> Result<Option<usize>, IoError> {
        let pair_count = self.inner.device.queue_pair_count().max(1);
        let local_pair = usize::from(self.inner.cpu.current_processor().id()) % pair_count;
        let mut received = 0usize;
        let mut drained_a_pair = false;
        for pair_idx in receive_pair_order(local_pair, pair_count) {
            if received >= frames.len() {
                break;
            }
            let Some(batch) = self
                .inner
                .device
                .try_receive_frames_immediate_on(pair_idx, &mut frames[received..])?
            else {
                continue;
            };
            drained_a_pair = true;
            received += batch;
        }
        if !drained_a_pair {
            return Ok(None);
        }
        Ok(Some(received))
    }

    pub(super) async fn poll_network_once_with_tcp_read(
        &self,
        source: NetworkPollSource,
        tcp_read_probe: Option<NetworkTcpReadProbe>,
        submit_transmit: bool,
    ) -> Result<NetworkPollOutcome, IoError> {
        // Carrier first: a link that moved invalidates the very
        // configuration the control plane is about to push into the
        // shards.
        self.synchronize_link_state();
        self.synchronize_control_plane();
        let budget = self.inner.poll.budget();

        let reclaim_started = self.profile_start();
        let reclaimed = self.reclaim_transmit_completions(budget.tx_completions)?;
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
        // How many frames one poll may hand the stack is fixed at
        // `Stack` construction, so it is read from the cached
        // service-level value rather than from a shard.
        //
        // Nothing else bounds the drain. Receive backpressure used to:
        // the poll asked one shard — the default one, whichever shard
        // the flows had actually landed on — whether its receive window
        // was open, and took every queue pair off the device when it
        // was not. That made one socket's full receive queue the whole
        // interface's problem. The guest stopped taking frames of any
        // kind, answered no ARP, and the host's neighbour entry for a
        // running guest went stale mid-transfer (#143).
        //
        // A receive window is per-socket flow control and it is already
        // enforced where it belongs: a conforming peer stops sending
        // before the queue fills, and a segment that arrives anyway is
        // outside the advertised window and is dropped by the stack. So
        // the drain runs to its budget, each frame is offered to the
        // shard its flow belongs to, and a shard that will not take one
        // loses that frame and nothing else.
        let stack_rx_budget = self.inner.stack_rx_budget;
        loop {
            let remaining_rx_budget = budget
                .rx_frames
                .min(stack_rx_budget)
                .saturating_sub(received);
            if remaining_rx_budget == 0 {
                break;
            }

            let receive_limit = remaining_rx_budget.min(NETWORK_RX_BATCH_FRAMES);
            let mut frames: [Option<RxFrame>; NETWORK_RX_BATCH_FRAMES] =
                core::array::from_fn(|_| None);
            let received_batch = match self
                .receive_frames_immediate(&mut frames[..receive_limit])?
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

            let received_at = StackInstant::from_nanos(self.now_nanos());
            // Demux each frame to the shard owning its destination
            // port. The previous single-shard path locked
            // `shard_for_default` once per batch; under multi-shard
            // we lock the owning shard per-frame so different
            // ports can be processed in parallel by other CPUs and
            // each shard's Stack only sees the connections it
            // actually owns.
            //
            // The shard that took a frame is remembered rather than
            // signalled here: the signal belongs after the lock is
            // released, and a batch that lands several frames in the
            // same shard should release its waiters once.
            let mut arrivals = ShardArrivals::new();
            for frame in frames[..received_batch].iter().flatten() {
                let frame_len = frame.len();
                match self
                    .inner
                    .state
                    .dispatch_rx_frame(frame, received_at, &self.inner.control)
                {
                    RxFrameDispatch::Delivered { shard_idx } => {
                        arrivals.record(shard_idx);
                        self.inner.state.record_received(shard_idx, 1);
                        received += 1;
                        received_bytes = received_bytes.saturating_add(frame_len);
                    }
                    // The frame is already off the ring and cannot be
                    // put back, so it is lost — which is what a closed
                    // receive window means and what the peer's
                    // retransmission is for. The rest of the batch
                    // belongs to other shards and is delivered
                    // regardless: one saturated flow does not get to
                    // drop another flow's segments, nor an ARP request.
                    RxFrameDispatch::Backpressured { shard_idx } => {
                        self.inner.state.record_receive_refused(shard_idx, 1);
                    }
                    RxFrameDispatch::Malformed => {
                        received += 1;
                        received_bytes = received_bytes.saturating_add(frame_len);
                    }
                }
            }
            // Every shard that took a frame is released here, and the
            // processor that owns it is pulled out of its idle park when
            // this is not that processor. Without this a reply demuxed
            // into a foreign shard would sit there until its waiter's own
            // deadline expired.
            self.inner.state.notify_arrivals(&arrivals, &self.inner.cpu);

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
            tcp_read = Some(self.inner.state.with_handle_receive_drain(
                probe.stream,
                &self.inner.cpu,
                |state| state.poll_tcp_read(probe.stream, probe.max_bytes, now),
            ));
            tcp_read_finished = self.profile_start();
        }
        self.record_network_profile_events_bytes_between(
            source.tcp_drive_phase(),
            tcp_started,
            tcp_finished,
            0,
            0,
        );
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

    /// Parks a TCP operation until its shard makes progress.
    ///
    /// `wait` must have been sampled before the operation inspected its
    /// stream or listener, so a segment another processor drained in
    /// between resolves the wait rather than being slept through. The
    /// timer bound is the sooner of the caller's deadline and the next
    /// protocol deadline any shard owes — a retransmit or a delayed ACK
    /// is driven by nothing but the clock.
    pub(super) async fn wait_for_tcp_progress(
        &self,
        wait: NetworkWait,
        operation_deadline_nanos: u64,
    ) {
        let now_nanos = self.now_nanos();
        if now_nanos >= operation_deadline_nanos {
            return;
        }
        let next_tcp_deadline = self.inner.state.min_tcp_deadline_nanos();
        let next_deadline = next_tcp_deadline
            .unwrap_or(operation_deadline_nanos)
            .min(operation_deadline_nanos);
        let timer_wait = Duration::from_nanos(next_deadline.saturating_sub(now_nanos));
        self.wait_for_shard_progress(wait, self.progress_wait(timer_wait))
            .await;
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
        read: &TcpReadIntoProgress,
    ) {
        let (phase, bytes) = match read {
            TcpReadIntoProgress::Pending => (
                tcp_read_profile_phase(prefix, TcpReadPhaseOutcome::Pending),
                0,
            ),
            TcpReadIntoProgress::Data(bytes) => (
                tcp_read_profile_phase(prefix, TcpReadPhaseOutcome::Ready),
                *bytes,
            ),
            TcpReadIntoProgress::Eof => {
                (tcp_read_profile_phase(prefix, TcpReadPhaseOutcome::Eof), 0)
            }
        };
        self.record_network_profile_events_bytes(phase, start, 1, bytes);
    }
}

impl NetworkShard {
    /// Opens a connection on this shard, which the caller has already
    /// established is the one the flow hashes to.
    pub(super) fn start_tcp_connect(
        &mut self,
        local: IpAddress,
        destination: IpAddress,
        port: u16,
        local_port: u16,
        hop_limit: u8,
    ) -> Result<TcpStreamId, TcpError> {
        if !self.is_tcp_local_port_free(local_port) {
            return Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpConnectStartFailed,
            });
        }
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

    /// Installs this shard's replica of a listener into `slot`.
    ///
    /// The slot and the port are decided once for the whole set, so
    /// every replica of the same listener answers to the same handle
    /// and binds the same port.
    pub(super) fn install_tcp_listener(
        &mut self,
        slot: usize,
        local_address: NetworkIpAddress,
        local_port: u16,
        backlog: TcpListenBacklog,
        hop_limit: u8,
    ) -> Result<(), TcpError> {
        if !self.is_tcp_local_port_free(local_port) {
            return Err(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpListenStartFailed,
            });
        }
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
        self.tcp_listeners.insert_at(
            slot,
            TcpListenerState {
                stack_socket,
                local_port,
            },
        );
        Ok(())
    }

    /// Drops this shard's replica of a listener, used to unwind a
    /// partial install.
    pub(super) fn remove_tcp_listener(&mut self, slot: usize) {
        if let Some(state) = self.tcp_listeners.remove(slot) {
            self.stack
                .remove_tcp_socket(state.stack_socket)
                .unwrap_or_else(|_| panic!("TCP listener referenced an unknown stack socket"));
        }
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
            TcpReadState::Closed(close) => match tcp_close_error(close) {
                Some(error) => Err(error),
                None => Ok(TcpReadProgress::Eof),
            },
        }
    }

    pub(super) fn poll_tcp_read_into(
        &mut self,
        stream: TcpStreamId,
        buffer: &mut RegisteredTcpReadBuffer<'_>,
        now: StackInstant,
    ) -> Result<TcpReadIntoProgress, TcpError> {
        let socket = self.tcp_socket(stream)?;
        match self
            .stack
            .tcp_read_into(socket, buffer, now)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpReceiveFailed,
            })? {
            TcpReadIntoState::Pending => Ok(TcpReadIntoProgress::Pending),
            TcpReadIntoState::Data(bytes) => Ok(TcpReadIntoProgress::Data(bytes)),
            TcpReadIntoState::Closed(close) => match tcp_close_error(close) {
                Some(error) => Err(error),
                None => Ok(TcpReadIntoProgress::Eof),
            },
        }
    }

    pub(super) fn remove_tcp_stream(&mut self, stream: TcpStreamId) {
        let slot = self.decode_handle_slot(stream.into());
        if let Some(socket) = self.tcp_streams.remove(slot) {
            self.stack
                .remove_tcp_socket(socket)
                .unwrap_or_else(|_| panic!("TCP stream referenced an unknown stack socket"));
        }
    }

    pub(super) fn insert_tcp_stream(&mut self, socket: helios_netstack::SocketId) -> TcpStreamId {
        let slot = self.tcp_streams.insert(socket);
        TcpStreamId(self.encode_handle_id(slot).get())
    }

    pub(super) fn tcp_socket(
        &self,
        stream: TcpStreamId,
    ) -> Result<helios_netstack::SocketId, TcpError> {
        let slot = self.decode_handle_slot(stream.into());
        self.tcp_streams.get(slot).copied().ok_or(TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::UnknownTcpStream,
        })
    }

    pub(super) fn tcp_listener(
        &self,
        listener: TcpListenerId,
    ) -> Result<&TcpListenerState, TcpError> {
        self.tcp_listeners
            .get(ReplicaHandle::from(listener).slot())
            .ok_or(TcpError {
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

    /// Allocates an ephemeral port for a flow to a known peer,
    /// preferring one whose answers hash back to this shard.
    ///
    /// Ownership follows the hash whatever port is chosen, so this is
    /// not a correctness requirement — it is what makes steering pay:
    /// a connection opened on this processor is then also received on
    /// it, with no cross-processor hop for its whole life. The walk is
    /// bounded by the shard's own window, and falls back to the first
    /// free port it saw, which the caller then places by its hash.
    pub(super) fn allocate_tcp_local_port_for(
        &mut self,
        local: IpAddress,
        remote: IpAddress,
        remote_port: u16,
        shard_count: usize,
    ) -> Result<u16, TcpError> {
        let mut fallback = None;
        for _ in 0..self.ephemeral_port_attempts() {
            let candidate = self.next_tcp_local_port;
            self.next_tcp_local_port = self.advance_ephemeral_port(self.next_tcp_local_port);
            if !self.is_tcp_local_port_free(candidate) {
                continue;
            }
            if shard_idx_for_flow(local, candidate, remote, remote_port, shard_count)
                == self.shard_idx
            {
                return Ok(candidate);
            }
            fallback.get_or_insert(candidate);
        }
        fallback.ok_or(TcpError {
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
