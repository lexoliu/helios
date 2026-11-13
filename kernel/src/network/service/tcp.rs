use super::*;

/// The receive queue pairs a poll visits, in the order it visits them:
/// the pair belonging to the polling processor first, then every other
/// pair once, wrapping.
///
/// Ownership of a pair is a locality preference, not a claim: an
/// operation that polls the device itself still visits every pair,
/// because the pump that owns a pair may not be scheduled before this
/// caller's own deadline. The walk is a concrete type so a caller can
/// hold it beside the pump's single-pair walk in one value.
pub(super) fn receive_pair_order(local_pair: usize, pair_count: usize) -> ReceivePairOrder {
    assert!(pair_count != 0, "an interface has at least one queue pair");
    ReceivePairOrder {
        offset: 0,
        local_pair,
        pair_count,
    }
}

/// [`receive_pair_order`]'s iterator, named so [`NetworkPollScope`]
/// can hold it next to a pump's one-pair walk.
pub(super) struct ReceivePairOrder {
    offset: usize,
    local_pair: usize,
    pair_count: usize,
}

impl Iterator for ReceivePairOrder {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.offset >= self.pair_count {
            return None;
        }
        let pair = (self.local_pair + self.offset) % self.pair_count;
        self.offset += 1;
        Some(pair)
    }
}

/// What one receive batch's frames became, gathered by the drain's sink
/// while the device holds each frame's pair and read by the poll once
/// the device has let go.
///
/// Owned by the polling task alone: the sink runs synchronously on the
/// processor that called the drain, so nothing here is shared.
struct RxBatchDelivery {
    /// Shards that took a frame, signalled once the batch is done.
    arrivals: ShardArrivals,
    /// Frames a shard consumed — delivered or unparseable — which is
    /// what the poll's receive budget counts.
    delivered: usize,
    delivered_bytes: usize,
    /// Bytes of every frame the device lent, refused ones included:
    /// the ring phase's byte count.
    lent_bytes: usize,
    /// Time spent demuxing inside the device's hold, summed over the
    /// batch's frames, which the ring phase is reported without.
    dispatch: NetworkPerfSpan,
}

impl RxBatchDelivery {
    fn new() -> Self {
        Self {
            arrivals: ShardArrivals::new(),
            delivered: 0,
            delivered_bytes: 0,
            lent_bytes: 0,
            dispatch: NetworkPerfSpan::default(),
        }
    }
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

#[derive(Clone, Copy, Debug)]
pub(super) enum TcpReadIntoProgress {
    Pending,
    Data(usize),
    Eof,
    /// Bytes are queued, but the page of the buffer they land on next is
    /// swapped out, so the stack handed over none of them.
    PageAbsent,
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

impl<CpuImpl, DeviceImpl> NetworkService<CpuImpl, DeviceImpl>
where
    CpuImpl: Cpu + Clone,
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
        buffer: &mut RegisteredTcpReadBuffer<'_>,
        timeout_nanos: u64,
    ) -> Result<TcpReadInto, TcpError> {
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

    /// What `stream`'s send side does with a write right now.
    ///
    /// The write permit of a socket-backed output stream: `Room` is the
    /// byte count a `write` of at most that many never has to park for,
    /// `Pending` is a full send queue on a live connection, and `Closed`
    /// is a send side that cannot take bytes again — the answer that
    /// resolves a parked `ready` instead of letting it sleep through the
    /// close.
    pub fn tcp_send_room(&self, stream: TcpStreamId) -> Result<TcpWriteProgress, TcpError> {
        self.inner
            .state
            .with_handle(stream, |state| state.tcp_send_room(stream))
    }

    /// Queues what `bytes` gives `stream`'s send queue and publishes it
    /// on the caller's own task: the owning shard's segment production,
    /// then the egress drain onto the rings and the doorbell.
    ///
    /// `Room` is the count the queue took. A queue that took nothing is
    /// classified under the same lock, so the one pass answers what a
    /// `tcp_send_room` probe would take a second pass to say: `Pending`
    /// for a full send window on a live connection, `Closed` for a send
    /// side that cannot send again.
    ///
    /// The drive is the transmit half of a poll. The receive half's
    /// async device read is unreachable from a `write` that may not
    /// block — and unneeded: inbound frames keep landing through the
    /// device's own delivery, and the parked reads and write waits
    /// consume them as they always have.
    pub fn tcp_try_write(
        &self,
        stream: TcpStreamId,
        bytes: &mut Bytes,
    ) -> Result<TcpWriteProgress, TcpError> {
        let budget = self.inner.poll.budget();
        // Completions first, so a ring full of used descriptors frees
        // before the new segment asks it for room — the order the full
        // poll keeps.
        let reclaimed = self
            .reclaim_transmit_completions(budget.tx_completions, NetworkPollScope::Interface)
            .map_err(|error| TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))?;
        let now = StackInstant::from_nanos(self.now_nanos());
        let progress = self.inner.state.with_handle(stream, |state| {
            let progress = state.try_write_tcp_bytes(stream, bytes)?;
            // What was queued becomes a segment on this shard's egress
            // while the lock is already held; nothing else has to run
            // for the write to reach the wire.
            state
                .drive_tcp(now, &self.inner.control)
                .unwrap_or_else(|error| {
                    tracing::debug!(?error, "failed to drive TCP after stream write")
                });
            Ok(progress)
        })?;
        let (transmitted, _) = self
            .submit_network_transmit(NetworkPollSource::Tcp, budget, NetworkPollScope::Interface)
            .map_err(|error| TcpError::from_io(error, NetworkErrorDetail::VirtioAdvanceFailed))?;
        if transmitted != 0 || reclaimed != 0 {
            self.inner.poll.complete(NetworkPollProgress {
                received_frames: 0,
                reclaimed_tx: reclaimed,
                transmitted_frames: transmitted,
            });
        }
        Ok(progress)
    }

    /// Drains up to `max_bytes` of what `stream`'s receive queue already
    /// holds, without parking.
    ///
    /// `with_handle_receive_drain` raises the owning shard's arrival when
    /// the drain relieved its receive backpressure (#107), so a producer
    /// parked on this socket wakes without being asked.
    pub fn tcp_try_read(
        &self,
        stream: TcpStreamId,
        max_bytes: usize,
    ) -> Result<TcpReadProgress, TcpError> {
        self.poll_tcp_read_once(stream, max_bytes, TcpReadPhasePrefix::Initial)
    }

    /// Resolves when `stream`'s send queue has room, when the send side
    /// can no longer send, or when the stream is gone — a socket-backed
    /// output stream's readiness wait.
    ///
    /// `Closed` resolves the wait rather than propagating its error:
    /// the pollable's contract is that a dead send side is ready, and
    /// the accessor that follows reports the state itself. Each round
    /// samples the shard's wait before the room probe, so an ACK — or a
    /// reset — another processor drains between the probe and the park
    /// resolves the wait rather than being slept through. The drive in
    /// between is what consumes the segments that change the answer.
    pub async fn tcp_write_ready(&self, stream: TcpStreamId) -> Result<(), TcpError> {
        loop {
            let wait = self.shard_wait_for_handle(stream);
            if !matches!(self.tcp_send_room(stream)?, TcpWriteProgress::Pending) {
                return Ok(());
            }
            self.drive_tcp().await?;
            if !matches!(self.tcp_send_room(stream)?, TcpWriteProgress::Pending) {
                return Ok(());
            }
            self.wait_for_tcp_progress(wait, u64::MAX).await;
        }
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
            .shard_idx_for_processor(helios_hal::cpu::current_processor())
    }

    pub async fn tcp_shutdown_send(&self, stream: TcpStreamId) -> Result<(), TcpError> {
        self.inner
            .state
            .with_handle(stream, |state| state.shutdown_tcp_send(stream))?;
        self.drive_tcp().await
    }

    /// Retires `stream`, freeing its slab slot and its stack socket.
    ///
    /// Synchronous on purpose: retirement is a shard-lock update with
    /// nothing to await, and the owner that has to run it is a `Drop`.
    /// A future here would mean the only way to end a stream's life is
    /// to spawn a task, and a task spawned from a dying instance is a
    /// task that may never run — which is how a connection outlived the
    /// program that opened it (#184).
    ///
    /// The close ends with a pump kick, and that is the whole of #231:
    /// what a retirement leaves behind is a FIN sequence to run or a
    /// reset on the outbound queue, and neither puts a frame anywhere a
    /// shard's own arrival signal would see. Without the kick the
    /// segment waits for the pump's next park to expire — up to
    /// `CONFIGURATION_POLL_NANOS` on a guest with nothing else to send —
    /// so a peer learns of the close a second after the guest made it.
    /// The kick belongs here rather than at the call sites: #232 put it
    /// on the component host's retirement drain, and the owners that
    /// reach this method directly — a socket resource backend's
    /// `close`, a descriptor table's `Drop` — never got one. No present
    /// or future owner has to remember it now.
    ///
    /// Unconditional, because the only close that queues nothing is one
    /// on a connection that owed its peer nothing, and telling the two
    /// apart costs more than the kick: `TcpCloseOutcome::Reclaimed`
    /// covers both the bare reclamation and the reset, so gating on it
    /// would drop exactly the wake a reset needs. A kick nothing is
    /// waiting for is one atomic increment on the shard set's arrival
    /// signal and a notify that finds no listener; a kick the parked
    /// pump takes costs it one pass over the shards, which is noise
    /// beside the teardown that just ran.
    pub fn tcp_close(&self, stream: TcpStreamId) {
        let now = StackInstant::from_nanos(self.now_nanos());
        self.inner.state.with_handle(stream, |state| {
            state.remove_tcp_stream(stream, now);
        });
        self.wake_packet_pump(self.inner.state.shard_idx_for_handle(stream));
    }

    /// Retires `listener`, freeing its slab slot, its replica on every
    /// shard, and the local port those replicas were holding.
    ///
    /// Synchronous for the same reason `tcp_close` and `udp_close` are:
    /// retirement is a shard-lock update with nothing to await, and the
    /// owner that has to run it is a `Drop`. A future here would mean
    /// the only way to end a listener's life is to spawn a task, and a
    /// task spawned from a dying instance is a task that may never run.
    ///
    /// Connections already accepted are streams of their own and live
    /// on. A connection still queued in a replica's backlog is reset:
    /// nobody will ever accept it, and its peer completed a handshake
    /// with a port that is going away. Until this existed every
    /// listener a program opened stayed in its shard for the rest of
    /// the boot, holding the port that `is_tcp_local_port_free`
    /// consults (#191).
    ///
    /// Those resets are segments on the outbound queue and nothing
    /// else raises a signal for them, so this kicks the pump for the
    /// reason [`NetworkService::tcp_close`] does and on the same terms.
    pub fn tcp_listener_close(&self, listener: TcpListenerId) {
        let slot = ReplicaHandle::from(listener).slot();
        let now = StackInstant::from_nanos(self.now_nanos());
        self.inner
            .state
            .for_each_replica("tcp listener close", |state| {
                state.remove_tcp_listener(slot, now);
                Ok::<(), core::convert::Infallible>(())
            })
            .unwrap_or_else(|infallible| match infallible {});
        self.inner.state.listener_slots.release(slot);
        self.wake_packet_pumps();
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
        let opened_at = StackInstant::from_nanos(self.now_nanos());
        let stream = if local_port == 0 {
            // Choosing the port and opening the connection are one
            // step: a four-tuple a lingering connection still holds
            // (TIME-WAIT included) can only be seen by opening, so the
            // walk moves on at that conflict instead of failing.
            self.inner.state.open_ephemeral_tcp_connect(
                self.accepting_shard_idx(),
                TcpConnectRequest {
                    local,
                    destination,
                    port,
                    hop_limit,
                    opened_at,
                },
            )?
        } else {
            // A program-bound port is one attempt; a conflict is the
            // program's to see.
            let owner = shard_idx_for_flow(local, local_port, destination, port, shard_count);
            self.inner
                .state
                .shard_at(owner)
                .lock()
                .start_tcp_connect(local, destination, port, local_port, hop_limit, opened_at)
                .map_err(TcpConnectStartError::into_tcp_error)?
        };

        loop {
            // Sampled before the handshake state is inspected, so a SYN-ACK
            // another processor drains in between resolves the wait.
            let wait = self.shard_wait_for_handle(stream);
            self.drive_tcp().await?;
            let now_nanos = self.now_nanos();
            let now = StackInstant::from_nanos(now_nanos);
            let poll_connect = self.inner.state.with_handle(stream, |state| {
                match state.poll_tcp_connect(stream) {
                    Ok(TcpConnectProgress::Connected) => Ok(TcpConnectProgress::Connected),
                    Ok(TcpConnectProgress::Pending) => {
                        if now_nanos >= deadline_nanos {
                            state.remove_tcp_stream(stream, now);
                            Err(TcpError {
                                kind: TcpErrorKind::Timeout,
                                detail: NetworkErrorDetail::TcpConnectTimeout,
                            })
                        } else {
                            Ok(TcpConnectProgress::Pending)
                        }
                    }
                    Err(error) => {
                        state.remove_tcp_stream(stream, now);
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
        let now = StackInstant::from_nanos(self.now_nanos());
        let install = self.inner.state.install_replica(
            slot,
            |shard, slot| {
                shard.install_tcp_listener(slot, local_address, local_port, backlog, hop_limit)
            },
            |shard, slot| shard.remove_tcp_listener(slot, now),
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
            let progress = self.inner.state.with_handle(stream, |state| {
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
            // `CONFIGURATION_POLL_NANOS`. A request/response exchange then
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
            match progress {
                // `Room` queued bytes, so retry what is left. `Pending`
                // is a full send queue, which the park below waits out;
                // `Closed` fails the write rather than parking on a send
                // side that cannot send again — the queue pass answered
                // which under the lock it already held.
                TcpWriteProgress::Room(_) => continue,
                TcpWriteProgress::Pending => {}
                TcpWriteProgress::Closed(error) => return Err(error),
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
                self.report_tcp_read_timeout(stream, timeout_nanos);
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

    /// The read behind [`Self::tcp_read_into`].
    ///
    /// Every copy into `buffer` happens inside a synchronous step of this
    /// future's own poll — the caller's host call, a turn of the instance
    /// that owns the buffer — never in a task of the network service. A
    /// page of the buffer that is out is faulted in here, which ends the
    /// turn, and the read starts over without counting the wait against
    /// the deadline: the bytes it is for are already queued.
    pub(super) async fn execute_tcp_read_into(
        &self,
        stream: TcpStreamId,
        buffer: &mut RegisteredTcpReadBuffer<'_>,
        timeout_nanos: u64,
    ) -> Result<TcpReadInto, TcpError> {
        let deadline_nanos = self.now_nanos().saturating_add(timeout_nanos);
        loop {
            let wait = self.shard_wait_for_handle(stream);
            match self
                .poll_tcp_read_into_round(stream, buffer, deadline_nanos)
                .await?
            {
                TcpReadIntoProgress::Data(bytes) => {
                    return self.finish_tcp_read_into(stream, buffer, bytes).await;
                }
                TcpReadIntoProgress::Eof => return Ok(TcpReadInto::Eof),
                TcpReadIntoProgress::PageAbsent => {
                    let fault_started = self.profile_start();
                    let fault = buffer.bring_in_next_page().await;
                    self.record_network_profile("tcp-read-into-page-in", fault_started);
                    match fault {
                        Ok(()) => continue,
                        Err(error) => return Ok(TcpReadInto::PageLost(error)),
                    }
                }
                TcpReadIntoProgress::Pending => {}
            }
            if self.now_nanos() >= deadline_nanos {
                self.report_tcp_read_timeout(stream, timeout_nanos);
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

    /// Completes a read that has delivered `delivered` bytes.
    ///
    /// A copy that stopped in front of a page of the buffer that is out,
    /// with bytes still queued behind it, takes them once the page is
    /// back, the way a read that meets a page fault in place carries on:
    /// the read still never waits for bytes that have not arrived. A page
    /// that cannot come back ends the read with its loss, which traps the
    /// guest whose memory it was.
    async fn finish_tcp_read_into(
        &self,
        stream: TcpStreamId,
        buffer: &mut RegisteredTcpReadBuffer<'_>,
        mut delivered: usize,
    ) -> Result<TcpReadInto, TcpError> {
        while buffer.awaits_page() && self.tcp_receive_queued(stream)? {
            if let Err(error) = buffer.bring_in_next_page().await {
                return Ok(TcpReadInto::PageLost(error));
            }
            match self.poll_tcp_read_into_once(stream, buffer, TcpReadPhasePrefix::IntoInitial)? {
                TcpReadIntoProgress::Data(more) => delivered += more,
                // Taken again before this poll; the loop brings it back.
                TcpReadIntoProgress::PageAbsent => {}
                TcpReadIntoProgress::Pending | TcpReadIntoProgress::Eof => break,
            }
        }
        Ok(TcpReadInto::Data(delivered))
    }

    /// Whether `stream`'s receive queue holds bytes a read would take
    /// now, without taking them or driving the device.
    fn tcp_receive_queued(&self, stream: TcpStreamId) -> Result<bool, TcpError> {
        let now = StackInstant::from_nanos(self.now_nanos());
        self.inner.state.with_handle(stream, |state| {
            let socket = state.tcp_socket(stream)?;
            let read = state.stack.tcp_read(socket, 0, now).map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpReceiveFailed,
            })?;
            Ok(matches!(read, TcpReadState::Data(_)))
        })
    }

    /// One round of [`Self::execute_tcp_read_into`] short of parking: the
    /// receive queue as it stands, then after one drive of the device,
    /// then, on a device with no interrupts, the polling rounds. The
    /// first answer that is not `Pending` ends the round.
    async fn poll_tcp_read_into_round(
        &self,
        stream: TcpStreamId,
        buffer: &mut RegisteredTcpReadBuffer<'_>,
        deadline_nanos: u64,
    ) -> Result<TcpReadIntoProgress, TcpError> {
        let read = self.poll_tcp_read_into_once(stream, buffer, TcpReadPhasePrefix::IntoInitial)?;
        if !matches!(read, TcpReadIntoProgress::Pending) {
            return Ok(read);
        }
        let drive_started = self.profile_start();
        self.drive_tcp_read_network_burst(buffer.capacity()).await?;
        self.record_network_profile("tcp-read-into-drive-network", drive_started);
        let read =
            self.poll_tcp_read_into_once(stream, buffer, TcpReadPhasePrefix::IntoAfterDrive)?;
        if !matches!(read, TcpReadIntoProgress::Pending) {
            return Ok(read);
        }
        self.poll_tcp_read_into_without_interrupt_sleep(stream, buffer, deadline_nanos)
            .await
    }

    /// Names the receive state a read gave up on.
    ///
    /// The per-shard counters are read after the workload exits, when
    /// the socket has already been torn down and its queues are gone, so
    /// a stall reported that way says nothing about the hole that caused
    /// it. This runs while the socket is still alive, once per timeout,
    /// on a path that has already waited out its whole deadline (#166).
    fn report_tcp_read_timeout(&self, stream: TcpStreamId, timeout_nanos: u64) {
        let diagnostics = self
            .inner
            .state
            .with_handle(stream, |shard| shard.tcp_receive_diagnostics(stream));
        match diagnostics {
            Ok(diagnostics) => tracing::warn!(
                stream = u64::from(stream),
                timeout_nanos,
                state = ?diagnostics.state,
                receive_next = diagnostics.receive_next,
                receive_queued_bytes = diagnostics.receive_queued_bytes,
                out_of_order_queued_bytes = diagnostics.out_of_order_queued_bytes,
                out_of_order_segments = diagnostics.out_of_order_segments,
                first_out_of_order = ?diagnostics.first_out_of_order,
                last_out_of_order = ?diagnostics.last_out_of_order,
                advertised_window_bytes = diagnostics.advertised_window_bytes,
                last_ack = ?diagnostics.last_ack,
                last_segment = ?diagnostics.last_segment,
                "TCP read timed out"
            ),
            Err(error) => tracing::warn!(
                stream = u64::from(stream),
                timeout_nanos,
                ?error,
                "TCP read timed out on a stream the shard no longer holds"
            ),
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
                .with_handle_receive_drain(stream, &self.inner.pairs, |state| {
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
                .with_handle_receive_drain(stream, &self.inner.pairs, |state| {
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
                    NetworkPollScope::Interface,
                    &self.inner.poll,
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
                ready @ (TcpReadIntoProgress::Data(_)
                | TcpReadIntoProgress::Eof
                | TcpReadIntoProgress::PageAbsent) => {
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
                    .submit_network_transmit(
                        NetworkPollSource::Tcp,
                        budget,
                        NetworkPollScope::Interface,
                    )
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

    /// Drains finished transmit descriptors on the queue pairs this
    /// poll scope owns, which is what frees the scatter payloads the
    /// device was reading in place.
    ///
    /// An operation sweeps every pair — it is the stack's whole driver
    /// while it runs — while a pump reclaims only the pair it owns:
    /// every other pair has a pump of its own parked on it. The sweep
    /// covers every pair brought up, live or not, so what a pair held
    /// when it stopped being live is still collected. A pair another
    /// processor currently holds is skipped either way: that processor
    /// is draining it, and this poll has nothing to add.
    fn reclaim_transmit_completions(
        &self,
        budget: usize,
        scope: NetworkPollScope,
    ) -> Result<usize, IoError> {
        let mut reclaimed = 0usize;
        let pair_count = self.inner.device.queue_pair_count().max(1);
        let local_pair = usize::from(helios_hal::cpu::current_processor().id()) % pair_count;
        for pair_idx in scope.pairs(local_pair, pair_count) {
            if reclaimed >= budget {
                break;
            }
            let Some(completed) = self
                .inner
                .device
                .reclaim_transmit_completions_immediate_on(pair_idx, budget - reclaimed)?
            else {
                continue;
            };
            reclaimed += completed;
        }
        Ok(reclaimed)
    }

    /// Demuxes one frame the device lent to the shard that owns its
    /// flow, and records what became of it in `batch`.
    ///
    /// This is the receive drain's sink, so it runs with the frame's
    /// queue pair held: the pair's receive state ranks above the shard
    /// lock `dispatch_rx_frame` takes, and nothing here waits for the
    /// pair. The shard that took the frame is remembered rather than
    /// signalled: the signal belongs after every lock is released, and
    /// a batch that lands several frames in the same shard should
    /// release its waiters once.
    ///
    /// Each frame takes its owning shard's lock on its own, so frames
    /// for different flows are processed in parallel by other
    /// processors and each shard's stack only sees the connections it
    /// owns.
    fn deliver_rx_frame(
        &self,
        frame: &RxFrame,
        received_at: StackInstant,
        batch: &mut RxBatchDelivery,
    ) {
        let dispatch_started = self.profile_start();
        let frame_len = frame.len();
        batch.lent_bytes = batch.lent_bytes.saturating_add(frame_len);
        match self
            .inner
            .state
            .dispatch_rx_frame(frame, received_at, &self.inner.control)
        {
            RxFrameDispatch::Delivered { shard_idx } => {
                batch.arrivals.record(shard_idx);
                self.inner.state.record_received(shard_idx, 1);
                batch.delivered += 1;
                batch.delivered_bytes = batch.delivered_bytes.saturating_add(frame_len);
            }
            // The frame is already off the ring and cannot be put back,
            // so it is lost — which is what a closed receive window
            // means and what the peer's retransmission is for. The rest
            // of the batch belongs to other shards and is delivered
            // regardless: one saturated flow does not get to drop
            // another flow's segments, nor an ARP request.
            RxFrameDispatch::Backpressured { shard_idx } => {
                self.inner.state.record_receive_refused(shard_idx, 1);
            }
            RxFrameDispatch::Malformed => {
                batch.delivered += 1;
                batch.delivered_bytes = batch.delivered_bytes.saturating_add(frame_len);
            }
        }
        if let Some(dispatch) = NetworkPerfSpan::between(dispatch_started, self.profile_start()) {
            batch.dispatch.add(dispatch);
        }
    }

    /// Drains received frames from the queue pairs this poll scope
    /// owns, which is what puts a reply into the shard that is waiting
    /// for it.
    ///
    /// An operation sweeps every pair, because the device delivers a
    /// frame on whichever pair *it* steered the flow to and the pump
    /// that owns the pair may not be scheduled before this caller's own
    /// deadline: a reply to a broadcast exchange — a DHCP offer, an ARP
    /// reply — is hashed independently of the request that provoked it.
    /// A pump visits only the pair it owns; every other pair has a pump
    /// of its own parked on it, so a frame is never without a drainer.
    ///
    /// Each frame reaches `sink` while the device still holds the pair
    /// it came from, so frames of one pair are delivered in ring order
    /// however many processors drain it: a second drainer cannot take
    /// the frames behind a batch and deliver them before the batch
    /// lands.
    ///
    /// The local pair is visited first, so a processor drains its own
    /// ring before it looks at anyone else's, and a pair another
    /// processor already holds is skipped by the device's `try_lock` —
    /// that processor is delivering it, and wakes the pair's waiters if
    /// it lets go with frames still on the ring, so this poll has
    /// nothing to add. `None` means every pair the scope covers was
    /// held, which is the same "come back later" a single-pair drain
    /// reports.
    ///
    /// `limit` bounds the frames the whole sweep hands to `sink`, and
    /// with it how long any one pair is held.
    ///
    /// A pair that refuses ends the sweep, and the refusal travels back
    /// beside the count of frames the sweep had already delivered. The
    /// refusal is counted against the pair that produced it here,
    /// because this is the last place that knows which pair that was.
    fn receive_frames_immediate(
        &self,
        pairs: impl Iterator<Item = usize>,
        limit: usize,
        mut sink: impl FnMut(&RxFrame),
    ) -> Option<RxDrain> {
        let mut received = 0usize;
        let mut drained_a_pair = false;
        for pair_idx in pairs {
            if received >= limit {
                break;
            }
            let Some(drain) = self.inner.device.try_receive_frames_immediate_on(
                pair_idx,
                limit - received,
                &mut sink,
            ) else {
                continue;
            };
            drained_a_pair = true;
            received += drain.received;
            if let Some(refusal) = drain.refusal {
                self.inner.state.record_receive_device_refusal(pair_idx);
                return Some(RxDrain::refused(received, refusal));
            }
        }
        if !drained_a_pair {
            return None;
        }
        Some(RxDrain::completed(received))
    }

    pub(super) async fn poll_network_once_with_tcp_read(
        &self,
        source: NetworkPollSource,
        tcp_read_probe: Option<NetworkTcpReadProbe>,
        submit_transmit: bool,
        scope: NetworkPollScope,
        poll: &NetworkPollState,
    ) -> Result<NetworkPollOutcome, IoError> {
        // Carrier first: a link that moved invalidates the very
        // configuration the control plane is about to push into the
        // shards. Both are interface-wide duties — an operation driving
        // the stack itself keeps them, and of the pumps only the one on
        // pair 0 does, since a link or
        // configuration event arrives on no pair. That pump also runs
        // the configuration state machines themselves, between the link
        // check that may have restarted them and the publication of
        // what they learn.
        if scope.interface_wide() {
            self.synchronize_link_state();
            if scope.drives_configuration() {
                self.drive_interface_configuration();
            }
            self.synchronize_control_plane();
        }
        let budget = poll.budget();

        let reclaim_started = self.profile_start();
        let reclaimed = self.reclaim_transmit_completions(budget.tx_completions, scope)?;
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
        let pair_count = self.inner.device.queue_pair_count().max(1);
        let local_pair = usize::from(helios_hal::cpu::current_processor().id()) % pair_count;
        loop {
            let remaining_rx_budget = budget
                .rx_frames
                .min(stack_rx_budget)
                .saturating_sub(received);
            if remaining_rx_budget == 0 {
                break;
            }

            let receive_limit = remaining_rx_budget.min(NETWORK_RX_BATCH_FRAMES);
            let received_at = StackInstant::from_nanos(self.now_nanos());
            let mut batch = RxBatchDelivery::new();
            // Each frame is demuxed to its shard from inside the drain,
            // while the device still holds the pair it came from. That
            // is what keeps two drainers of one pair — its pump and an
            // operation sweeping every pair — from delivering its frames
            // in an order the ring never had. A drain that popped a
            // batch and released the pair before demuxing it let the
            // other drainer pop the frames behind the batch and land
            // them first, and the stack read every overtaken segment as
            // a hole: it queued the earlier segment out of order and
            // owed the peer a duplicate acknowledgement, and the peer
            // fast-retransmitted data that was never lost (#776).
            let ring_started = self.profile_start();
            let drain = self.receive_frames_immediate(
                scope.pairs(local_pair, pair_count),
                receive_limit,
                |frame| self.deliver_rx_frame(frame, received_at, &mut batch),
            );
            let ring_finished = self.profile_start();
            // The hold interleaves the driver's ring work with the demux
            // frame by frame, so the two phases are the hold split by
            // the demux time summed inside it: `rx-dispatch` is that
            // sum, and `rx-ring` the rest of the hold — popping, chain
            // assembly, and posting the pass's buffers back.
            let received_batch = drain.as_ref().map_or(0, |drain| drain.received);
            let hold = NetworkPerfSpan::between(ring_started, ring_finished);
            self.record_network_profile_span(
                source.rx_ring_phase(),
                hold.map(|hold| hold.excluding(batch.dispatch)),
                received_batch,
                batch.lent_bytes,
            );
            if received_batch != 0 {
                self.record_network_profile_span(
                    source.rx_dispatch_phase(),
                    hold.map(|_| batch.dispatch),
                    received_batch,
                    0,
                );
            }
            // `None`: every pair this scope covers is held by somebody
            // else, who is delivering it and wakes the pair's waiters
            // if it lets go with frames left on the ring. The caller
            // parks on the wait it armed before this poll.
            let Some(RxDrain { refusal, .. }) = drain else {
                break;
            };
            received += batch.delivered;
            received_bytes = received_bytes.saturating_add(batch.delivered_bytes);
            // Every shard that took a frame is released here, after the
            // ring and shard locks are gone, so a woken waiter never
            // contends with the drain that woke it. Without this a reply
            // demuxed into a foreign shard would sit there until its
            // waiter's own deadline expired.
            let signal_started = self.profile_start();
            self.inner.state.notify_arrivals(&batch.arrivals);
            self.record_network_profile_events(
                source.rx_signal_phase(),
                signal_started,
                batch.arrivals.len(),
            );

            // The drain that produced this batch was refused. The frames
            // in front of the refusal have reached their shards and
            // their receive slots are back with the device, so the error
            // is what is left to report.
            if let Some(refusal) = refusal {
                return Err(refusal);
            }
            if received_batch == 0 {
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
        // Each shard owns its own TCP connections, so the timer drive
        // hits every shard this scope serves: all of them for an
        // operation driving the interface, the shards that submit on
        // the pump's pair for a pump.
        let drive_tcp = |state: &mut NetworkShard| {
            state
                .drive_tcp(now, &self.inner.control)
                .unwrap_or_else(|error| tracing::debug!(?error, "failed to drive TCP control"));
        };
        match scope {
            NetworkPollScope::Interface => self.inner.state.for_each(drive_tcp),
            NetworkPollScope::Pair { pair, .. } => {
                self.inner
                    .state
                    .for_each_on_pair(pair, self.inner.pairs.layout(), drive_tcp)
            }
        }
        let tcp_finished = self.profile_start();
        if let Some(probe) = tcp_read_probe {
            tcp_read_started = self.profile_start();
            tcp_read = Some(self.inner.state.with_handle_receive_drain(
                probe.stream,
                &self.inner.pairs,
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
            self.submit_network_transmit(source, budget, scope)?
        } else {
            (0, 0)
        };
        let progress = NetworkPollProgress {
            received_frames: received,
            reclaimed_tx: reclaimed,
            transmitted_frames: transmitted,
        };
        poll.complete(progress);
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
            // A read that waits for a page of its buffer moved nothing yet.
            TcpReadIntoProgress::Pending | TcpReadIntoProgress::PageAbsent => (
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

/// Why a connection could not be started on a shard.
///
/// A caller that chooses the local port tells the two apart: only
/// [`Self::PortInUse`] is cured by another port. A program-bound port
/// sees either as the same [`TcpError`].
#[derive(Debug)]
pub(super) enum TcpConnectStartError {
    /// A listener holds the local port, or a non-closed socket
    /// (TIME-WAIT included) holds the four-tuple.
    PortInUse,
    /// Anything another local port would not change.
    Failed(TcpError),
}

impl TcpConnectStartError {
    /// The error a caller that did not choose the port sees.
    pub(super) fn into_tcp_error(self) -> TcpError {
        match self {
            Self::PortInUse => TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpConnectStartFailed,
            },
            Self::Failed(error) => error,
        }
    }
}

/// What a connect without a bound port knows before it has one.
#[derive(Clone, Copy)]
pub(super) struct TcpConnectRequest {
    pub(super) local: IpAddress,
    pub(super) destination: IpAddress,
    pub(super) port: u16,
    pub(super) hop_limit: u8,
    pub(super) opened_at: StackInstant,
}

impl TcpConnectRequest {
    /// The shard each local port's flow hashes to, on `shard_count`
    /// shards.
    pub(super) fn port_owners(&self, shard_count: usize) -> PortOwners {
        PortOwners::new(self.local, self.destination, self.port, shard_count)
    }

    /// Opens the connection from `local_port` on `shard`, which must be
    /// the flow's owner.
    fn start_on(
        &self,
        shard: &mut NetworkShard,
        local_port: u16,
    ) -> Result<TcpStreamId, TcpConnectStartError> {
        shard.start_tcp_connect(
            self.local,
            self.destination,
            self.port,
            local_port,
            self.hop_limit,
            self.opened_at,
        )
    }
}

/// A connection an ephemeral-port connect opened, and the candidate it
/// took: an [`OwnCandidate`] or a [`ForeignCandidate`], which advance
/// different counters of the destination.
#[derive(Clone, Copy)]
pub(super) struct OpenedTcpConnect<Taken> {
    pub(super) stream: TcpStreamId,
    pub(super) taken: Taken,
}

impl NetworkShardSet {
    /// Chooses an ephemeral port and opens the connection from it, as
    /// one step, from the window of shard `accepting`.
    ///
    /// The start depends on the destination (RFC 6056 §3.3.3,
    /// [`ConnectPortSelector`]) and the walk runs over the accepting
    /// shard's own ports in rank order ([`EphemeralCandidates`]): every
    /// own port is first for an equal share of the keyed starts, and
    /// successive connects to one destination take successive own ports,
    /// so a four-tuple a peer may still hold is not drawn again before a
    /// lap of them. A port whose flow hashes back to the accepting shard
    /// is preferred because the connection is then also received on the
    /// processor that opened it, with no cross-processor hop for its
    /// whole life. Only with every own port in use are the ports that
    /// hash elsewhere tried, on their owning shards, in window order
    /// from the keyed start plus the destination's foreign count. Once a
    /// candidate opens, the count of its kind advances past it.
    ///
    /// Locks: the accepting shard's lock is held to read its window, the
    /// key and the destination's counters, released while the window is
    /// classified, taken again for the own half and for recording an own
    /// take, and once more to record a foreign take. Each foreign
    /// candidate is tried under its owner's lock alone; no two shard
    /// locks are ever held, and none across an await.
    ///
    /// Two connects to one destination may read the same counters
    /// before either records its take and so start at the same rank.
    /// The one that tries a port second finds it in use, because the
    /// test and the open both run under the lock of the port's owner,
    /// and moves on to the next; each then advances the count by the
    /// ranks it passed plus one, and since the advances add, the count
    /// ends past both ports. A race only moves a destination's start
    /// further on, never back onto a port just taken.
    ///
    /// A candidate in use ([`TcpConnectStartError::PortInUse`]) moves
    /// on to the next; any other failure ends the search. Only a window
    /// with every candidate in use fails with
    /// [`NetworkErrorDetail::TcpNoEphemeralPorts`].
    pub(super) fn open_ephemeral_tcp_connect(
        &self,
        accepting: usize,
        request: TcpConnectRequest,
    ) -> Result<TcpStreamId, TcpError> {
        let owners = request.port_owners(self.shard_count());
        let (start, slot) = self.shard_at(accepting).lock().connect_start(&request);
        let candidates =
            EphemeralCandidates::classify(start, |port| owners.owner_of(port) == accepting);
        {
            let mut shard = self.shard_at(accepting).lock();
            if let Some(opened) = shard.open_tcp_connect_along_own_flows(&candidates, request)? {
                shard.connect_ports.took_own(slot, opened.taken);
                return Ok(opened.stream);
            }
        }
        let opened = self.open_tcp_connect_along_foreign_flows(&candidates, &owners, request)?;
        self.shard_at(accepting)
            .lock()
            .connect_ports
            .took_foreign(slot, opened.taken);
        Ok(opened.stream)
    }

    /// The second half of [`Self::open_ephemeral_tcp_connect`]: the
    /// ports of `candidates` that hash to a shard other than the
    /// accepting one, each opened on its owner under `owners`.
    pub(super) fn open_tcp_connect_along_foreign_flows(
        &self,
        candidates: &EphemeralCandidates,
        owners: &PortOwners,
        request: TcpConnectRequest,
    ) -> Result<OpenedTcpConnect<ForeignCandidate>, TcpError> {
        for candidate in candidates.foreign_ports() {
            let owner = owners.owner_of(candidate.port);
            match request.start_on(&mut self.shard_at(owner).lock(), candidate.port) {
                Ok(stream) => {
                    return Ok(OpenedTcpConnect {
                        stream,
                        taken: candidate,
                    });
                }
                Err(TcpConnectStartError::PortInUse) => {}
                Err(TcpConnectStartError::Failed(error)) => return Err(error),
            }
        }
        Err(TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpNoEphemeralPorts,
        })
    }
}

impl NetworkShard {
    /// Opens a connection on this shard, which the caller has already
    /// established is the one the flow hashes to.
    ///
    /// A port a listener holds, or a four-tuple a non-closed socket
    /// (TIME-WAIT included) holds, is [`TcpConnectStartError::PortInUse`]:
    /// the only failure a different local port can cure.
    pub(super) fn start_tcp_connect(
        &mut self,
        local: IpAddress,
        destination: IpAddress,
        port: u16,
        local_port: u16,
        hop_limit: u8,
        now: StackInstant,
    ) -> Result<TcpStreamId, TcpConnectStartError> {
        const START_FAILED: TcpError = TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpConnectStartFailed,
        };
        if !self.is_tcp_local_port_free(local_port) {
            return Err(TcpConnectStartError::PortInUse);
        }
        let local = TcpEndpoint {
            address: local,
            port: local_port,
        };
        let remote = TcpEndpoint {
            address: destination,
            port,
        };
        // RFC 6528: the same keyed function a passive open uses.
        let initial_sequence = self.stack.tcp_initial_sequence(local, remote, now);
        let socket = self
            .stack
            .open_tcp_connect(local, remote, initial_sequence)
            .map_err(|error| match error {
                StackError::AddressInUse => TcpConnectStartError::PortInUse,
                other => TcpConnectStartError::Failed(map_tcp_open_error(
                    other,
                    NetworkErrorDetail::TcpConnectStartFailed,
                )),
            })?;
        // Applied before the connect loop drives the stack, so the SYN itself
        // already carries the caller's TTL.
        self.stack
            .set_tcp_hop_limit(socket, hop_limit)
            .map_err(|_| TcpConnectStartError::Failed(START_FAILED))?;
        Ok(self.insert_tcp_stream(socket))
    }

    /// Opens an ephemeral-port connection on this shard from the own
    /// ports of `candidates`, this shard's own window.
    ///
    /// Only a port whose flow hashes to this shard is tried here, which
    /// is what makes a connection opened on this processor also
    /// received on it. A port that is in use is skipped; any other
    /// failure is returned. `None` means no port of this shard opened,
    /// and the caller tries the rest on their owning shards.
    ///
    /// Runs under this shard's lock, which the caller holds.
    pub(super) fn open_tcp_connect_along_own_flows(
        &mut self,
        candidates: &EphemeralCandidates,
        request: TcpConnectRequest,
    ) -> Result<Option<OpenedTcpConnect<OwnCandidate>>, TcpError> {
        for candidate in candidates.own_ports() {
            match request.start_on(self, candidate.port) {
                Ok(stream) => {
                    return Ok(Some(OpenedTcpConnect {
                        stream,
                        taken: candidate,
                    }));
                }
                Err(TcpConnectStartError::PortInUse) => {}
                Err(TcpConnectStartError::Failed(error)) => return Err(error),
            }
        }
        Ok(None)
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
            .map_err(|error| map_tcp_open_error(error, NetworkErrorDetail::TcpListenStartFailed))?;
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

    /// Drops this shard's replica of a listener, used both to close one
    /// and to unwind a partial install.
    ///
    /// The stack resets whatever this replica's backlog was still
    /// holding, so `now` stamps those resets. Connections already
    /// accepted are streams of their own and are untouched.
    pub(super) fn remove_tcp_listener(&mut self, slot: usize, now: StackInstant) {
        if let Some(state) = self.tcp_listeners.remove(slot) {
            self.stack
                .remove_tcp_socket(state.stack_socket, now)
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

    /// What `stream`'s send side does with a write right now.
    ///
    /// The stack's tri-state with the close kind already folded into the
    /// error a writer reports: a reset reads as `ConnectionReset`, the
    /// orderly close as "no longer writable".
    pub(super) fn tcp_send_room(&self, stream: TcpStreamId) -> Result<TcpWriteProgress, TcpError> {
        let socket = self.tcp_socket(stream)?;
        self.stack
            .tcp_send_room(socket)
            .map(|state| match state {
                TcpSendState::Pending => TcpWriteProgress::Pending,
                TcpSendState::Room(room) => TcpWriteProgress::Room(room),
                TcpSendState::Closed(close) => {
                    TcpWriteProgress::Closed(tcp_close_error(close).unwrap_or(TcpError {
                        kind: TcpErrorKind::Unavailable,
                        detail: NetworkErrorDetail::TcpNoLongerWritable,
                    }))
                }
            })
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::UnknownTcpStream,
            })
    }

    /// Queues what `bytes` gives `stream`'s send queue. `Room` is the
    /// count queued; a queue that took nothing asks `tcp_send_room`
    /// while the state the write saw is still held — `Pending` for a
    /// full send window, `Closed` for a send side that cannot send
    /// again, and `Room` only for a write that carried no bytes.
    pub(super) fn try_write_tcp_bytes(
        &mut self,
        stream: TcpStreamId,
        bytes: &mut Bytes,
    ) -> Result<TcpWriteProgress, TcpError> {
        let socket = self.tcp_socket(stream)?;
        let written = self
            .stack
            .tcp_send_bytes(socket, bytes)
            .map_err(|_| TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpWriteQueueFailed,
            })?;
        match written {
            0 => self.tcp_send_room(stream),
            written => Ok(TcpWriteProgress::Room(written)),
        }
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
            // The stack hands over nothing, though bytes are queued, only
            // when the buffer reported no room: full, or its next page out.
            TcpReadIntoState::Data(0) if buffer.awaits_page() => {
                Ok(TcpReadIntoProgress::PageAbsent)
            }
            TcpReadIntoState::Data(bytes) => Ok(TcpReadIntoProgress::Data(bytes)),
            TcpReadIntoState::Closed(close) => match tcp_close_error(close) {
                Some(error) => Err(error),
                None => Ok(TcpReadIntoProgress::Eof),
            },
        }
    }

    /// Retires `stream` and hands what is left of the connection to the
    /// stack.
    ///
    /// The handle is invalid the moment its slot is released, whatever
    /// the stack then does with the socket: a connection still owing
    /// its peer a FIN outlives the handle, under the stack's ownership
    /// and unreachable from here, and a later call on the handle is
    /// `UnknownTcpStream` either way.
    ///
    /// Before #224 this dropped the socket outright, so a program that
    /// exited with a connection open put nothing on the wire and its
    /// peer kept an established connection until its own timeout.
    pub(super) fn remove_tcp_stream(&mut self, stream: TcpStreamId, now: StackInstant) {
        let slot = self.decode_handle_slot(stream.into());
        if let Some(socket) = self.tcp_streams.remove(slot) {
            let outcome = self
                .stack
                .close_tcp_socket(socket, now)
                .unwrap_or_else(|_| panic!("TCP stream referenced an unknown stack socket"));
            tracing::debug!(stream = u64::from(stream), ?outcome, "TCP stream retired");
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

    pub(super) fn tcp_receive_diagnostics(
        &self,
        stream: TcpStreamId,
    ) -> Result<TcpReceiveDiagnostics, TcpError> {
        let socket = self.tcp_socket(stream)?;
        self.stack
            .tcp_receive_diagnostics(socket)
            .map_err(|_| TcpError {
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
        for candidate in self.ephemeral_walk() {
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

/// The zero-copy receive against guest pages swap has taken: bytes go
/// from the stream's receive queue into guest memory once, and only into
/// a page the copying step saw present.
#[cfg(test)]
mod read_into_tests {
    use alloc::vec::Vec;
    use core::future::Future;
    use core::pin::{Pin, pin};
    use core::task::{Context, Poll, Waker};

    use helios_netstack::{TcpFlags, TcpHeader};

    use crate::memory::{GUEST_SWAP_PAGE, GuestSwap, SwapFaultError};
    use crate::network::EstablishedTcpFixture;
    use crate::{RegisteredTcpReadBuffer, TcpReadInto};

    const PAGE: usize = GUEST_SWAP_PAGE;

    /// The bytes of one segment the peer sends.
    const SEGMENT: usize = 1_400;

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Option<F::Output> {
        match future.poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(output) => Some(output),
            Poll::Pending => None,
        }
    }

    /// What a read finished with, if it finished in this poll; the stream
    /// is live, so it never fails.
    fn read_result(
        read: Pin<&mut impl Future<Output = Result<TcpReadInto, crate::TcpError>>>,
    ) -> Option<TcpReadInto> {
        poll_once(read).map(|read| read.expect("the stream is live"))
    }

    /// Text in Chinese, Japanese and Korean, repeated to `len` bytes.
    fn cjk_bytes(len: usize) -> Vec<u8> {
        "数据データ데이터".bytes().cycle().take(len).collect()
    }

    /// The peer sends `payload` as segments of [`SEGMENT`] bytes, in order.
    fn deliver(fixture: &EstablishedTcpFixture, payload: &[u8]) {
        for (index, segment) in payload.chunks(SEGMENT).enumerate() {
            fixture.deliver(
                TcpHeader {
                    source_port: EstablishedTcpFixture::PEER_PORT,
                    destination_port: EstablishedTcpFixture::LOCAL_PORT,
                    // The handshake took one sequence number on each side.
                    sequence: EstablishedTcpFixture::PEER_SEQUENCE + 1 + (index * SEGMENT) as u32,
                    acknowledgement: EstablishedTcpFixture::LOCAL_SEQUENCE + 1,
                    flags: TcpFlags::ACK.union(TcpFlags::PSH),
                    window_size: u16::MAX,
                },
                segment,
            );
        }
    }

    fn buffer<'a>(swap: &GuestSwap, ranges: &'a [(usize, usize)]) -> RegisteredTcpReadBuffer<'a> {
        // SAFETY: every range lies inside the test memory, which outlives
        // the buffer, and nothing else writes it while the read runs.
        unsafe { RegisteredTcpReadBuffer::new(swap.base(), ranges, Some(swap.residency())) }
    }

    /// Bytes that arrived while the page they land on was out stay queued
    /// until the page is back, then land on it intact.
    #[test]
    fn bytes_for_an_evicted_page_land_intact_once_it_is_back() {
        let fixture = EstablishedTcpFixture::new();
        let service = fixture.service();
        let mut swap = GuestSwap::new(2);
        swap.evict(1);
        let payload = cjk_bytes(SEGMENT);
        deliver(&fixture, &payload);

        let ranges = [(PAGE, SEGMENT)];
        let mut buffer = buffer(&swap, &ranges);
        let mut read = pin!(service.tcp_read_into(fixture.stream(), &mut buffer, 0));
        assert!(
            poll_once(read.as_mut()).is_none(),
            "the read waits for the page instead of writing it"
        );
        assert_eq!(swap.queued_faults(), 1, "the page was asked for");
        assert!(swap.is_out(1), "nothing has read the page back yet");

        swap.serve();
        assert_eq!(read_result(read.as_mut()), Some(TcpReadInto::Data(SEGMENT)));
        assert!(swap.bytes()[PAGE..PAGE + SEGMENT] == payload[..]);
    }

    /// One read over two segments whose buffer starts on a present page
    /// and runs onto an evicted one: what fits before the page that is
    /// out lands at once, the rest once that page is back, all in one
    /// read and in order.
    #[test]
    fn a_read_spanning_a_present_and_an_evicted_page_lands_whole() {
        let fixture = EstablishedTcpFixture::new();
        let service = fixture.service();
        let mut swap = GuestSwap::new(2);
        swap.evict(1);
        let payload = cjk_bytes(2 * SEGMENT);
        deliver(&fixture, &payload);

        let head = 1_000;
        let ranges = [(PAGE - head, head), (PAGE, 2 * SEGMENT - head)];
        let mut buffer = buffer(&swap, &ranges);
        let mut read = pin!(service.tcp_read_into(fixture.stream(), &mut buffer, 0));
        assert!(poll_once(read.as_mut()).is_none());
        assert!(
            swap.bytes()[PAGE - head..PAGE] == payload[..head],
            "the bytes for the present page landed in the first poll"
        );
        assert_eq!(swap.queued_faults(), 1);

        swap.serve();
        assert_eq!(
            read_result(read.as_mut()),
            Some(TcpReadInto::Data(2 * SEGMENT))
        );
        assert!(swap.bytes()[PAGE - head..PAGE - head + 2 * SEGMENT] == payload[..]);
    }

    /// A page that cannot come back ends the read with its loss, and the
    /// bytes it was for stay queued on the stream.
    #[test]
    fn a_lost_page_ends_the_read_and_leaves_the_bytes_queued() {
        let fixture = EstablishedTcpFixture::new();
        let service = fixture.service();
        let mut swap = GuestSwap::new(2);
        swap.evict(1);
        swap.fail_reads(1);
        let payload = cjk_bytes(SEGMENT);
        deliver(&fixture, &payload);

        let ranges = [(PAGE, SEGMENT)];
        let mut lost = buffer(&swap, &ranges);
        let mut read = pin!(service.tcp_read_into(fixture.stream(), &mut lost, 0));
        assert!(poll_once(read.as_mut()).is_none());
        swap.serve();
        assert_eq!(
            read_result(read.as_mut()),
            Some(TcpReadInto::PageLost(SwapFaultError::Backend))
        );

        let ranges = [(0, SEGMENT)];
        let mut present = buffer(&swap, &ranges);
        let mut read = pin!(service.tcp_read_into(fixture.stream(), &mut present, 0));
        assert_eq!(read_result(read.as_mut()), Some(TcpReadInto::Data(SEGMENT)));
        assert!(swap.bytes()[..SEGMENT] == payload[..]);
    }
}
