//! Per-processor network shards and the RX demux that feeds them.
//!
//! # SMP contract
//!
//! There is one [`NetworkShard`] per processor and every shard owns an
//! independent `Stack`, socket slab and port allocator. A local port
//! belongs to exactly one shard ([`shard_idx_for_port`]), so the frames
//! for a socket are always placed in that socket's shard whichever
//! processor drained them off the device.
//!
//! Placement is therefore routinely cross-processor: the shard lock is
//! taken by the draining CPU, not the owning one. That hand-off is
//! completed by [`PaddedShard::arrival`] — the draining CPU raises the
//! owning shard's progress signal after it releases the lock, and
//! [`NetworkShardSet::notify_arrivals`] additionally wakes the owning
//! processor when it is not the one that drained, so a parked executor
//! runs the released waiter instead of sleeping to its own deadline.
//! Nothing in this module spins, and no shard lock is ever held across
//! an await point.

use super::*;

/// Peek the L4 destination port out of a parsed Ethernet frame.
/// Used by the RX demux to route frames to the shard that owns the
/// local port. Returns `None` for non-IP frames (ARP), non-TCP/UDP
/// IP frames (ICMP), or malformed packets — all of which route to
/// shard 0 in the dispatch path because there is no socket-local
/// owner to identify.
pub(super) fn peek_local_port(frame: &[u8]) -> Option<u16> {
    let ethernet = EthernetFrame::parse(frame)?;
    let payload = ethernet.payload;
    let (protocol, l4_payload) = match ethernet.protocol {
        EthernetProtocol::Ipv4 => {
            let packet = Ipv4Packet::parse(payload)?;
            (packet.protocol, packet.payload)
        }
        EthernetProtocol::Ipv6 => {
            let packet = Ipv6Packet::parse(payload)?;
            (packet.next_header, packet.payload)
        }
        _ => return None,
    };
    match protocol {
        IpProtocol::Tcp => Some(TcpPacket::parse(l4_payload)?.destination_port),
        IpProtocol::Udp => Some(UdpPacket::parse(l4_payload)?.destination_port),
        IpProtocol::Icmp => peek_icmpv4_quoted_local_port(l4_payload),
        IpProtocol::Icmpv6 => peek_icmpv6_quoted_local_port(l4_payload),
    }
}

pub(super) fn peek_icmpv4_quoted_local_port(bytes: &[u8]) -> Option<u16> {
    let Icmpv4Packet::DestinationUnreachable(unreachable) = Icmpv4Packet::parse(bytes)? else {
        return None;
    };
    let quoted = Ipv4Packet::parse_quoted(unreachable.original)?;
    match quoted.protocol {
        IpProtocol::Tcp => Some(TcpPacket::parse_ports(quoted.payload)?.source),
        IpProtocol::Udp => Some(UdpPacket::parse_ports(quoted.payload)?.source),
        _ => None,
    }
}

pub(super) fn peek_icmpv6_quoted_local_port(bytes: &[u8]) -> Option<u16> {
    let original = match Icmpv6Packet::parse(bytes)? {
        Icmpv6Packet::DestinationUnreachable(unreachable) => unreachable.original,
        Icmpv6Packet::PacketTooBig(packet_too_big) => packet_too_big.original,
        _ => return None,
    };
    let quoted = Ipv6Packet::parse_quoted(original)?;
    match quoted.next_header {
        IpProtocol::Tcp => Some(TcpPacket::parse_ports(quoted.payload)?.source),
        IpProtocol::Udp => Some(UdpPacket::parse_ports(quoted.payload)?.source),
        _ => None,
    }
}

/// Maps a local port to the index of the shard that owns it. Server
/// listening ports (anything below `EPHEMERAL_PORT_START`) always
/// route to shard 0; ephemeral ports stride across shards so a
/// freshly-allocated outgoing port `EPHEMERAL_PORT_START + k` lands
/// on shard `k % shard_count`. Frames whose port is `None` (ARP,
/// ICMP, …) or unparseable also map to shard 0.
pub(super) fn shard_idx_for_port(port: Option<u16>, shard_count: usize) -> usize {
    let Some(port) = port else { return 0 };
    if port < EPHEMERAL_PORT_START {
        return 0;
    }
    let stride_idx = port - EPHEMERAL_PORT_START;
    usize::from(stride_idx) % shard_count
}

pub(super) struct NetworkShard {
    pub(super) stack: Box<Stack>,
    /// This shard's index inside the parent `NetworkShardSet`.
    /// Encoded into every public socket id this shard mints so the
    /// inverse mapping `(id - 1) % shard_count == shard_idx` can
    /// route operations back to the owning shard without an extra
    /// table lookup.
    pub(super) shard_idx: usize,
    /// Total number of shards in the parent set. Required for the
    /// stride-based handle encoding.
    pub(super) shard_count: usize,
    pub(super) next_tcp_local_port: u16,
    pub(super) next_udp_local_port: u16,
    pub(super) tcp_streams: HandleSlab<helios_netstack::SocketId, MAX_TCP_STREAM_HANDLES>,
    pub(super) tcp_listeners: HandleSlab<TcpListenerState, MAX_TCP_LISTENER_HANDLES>,
    pub(super) udp_sockets: HandleSlab<UdpSocketState, MAX_UDP_SOCKET_HANDLES>,
    pub(super) dhcp: DhcpClientState,
    pub(super) dns_servers: DhcpDnsServers,
    pub(super) next_dns_query_id: u16,
    /// Identifier stamped on the next ICMP echo request. ICMP frames
    /// carry no port, so every one of them is demultiplexed onto shard
    /// 0 and this counter is that shard's alone.
    pub(super) next_icmp_echo_identifier: u16,
}

/// A set of `NetworkShard` instances laid out per-CPU.
///
/// Cache-line padding around each shard avoids false sharing once
/// multiple shards live in the box: without it, two adjacent
/// `SpinMutex` fields would share a cache line and ping-pong on
/// every cross-CPU lock operation. We pay the padding cost in the
/// single-shard build to keep the layout invariant.
#[repr(align(64))]
pub(super) struct PaddedShard {
    pub(super) inner: SpinMutex<NetworkShard>,
    /// Raised every time a frame is placed in this shard's stack.
    ///
    /// Deliberately outside the `SpinMutex`: the processor that drained
    /// the frame signals after it has released the lock, and a waiter
    /// parks on the signal without taking the lock at all.
    pub(super) arrival: ProgressSignal,
}

/// Shards that took a frame during one receive batch.
///
/// A batch drains at most [`NETWORK_RX_BATCH_FRAMES`] frames and each
/// frame reaches exactly one shard, so the set of shards to signal
/// afterwards is bounded by the batch rather than by the processor
/// count — no allocation, and no cap on how many CPUs the kernel runs
/// on.
pub(super) struct ShardArrivals {
    touched: [u16; NETWORK_RX_BATCH_FRAMES],
    len: usize,
}

/// What the RX demux did with one received frame.
pub(super) enum RxFrameDispatch {
    /// The owning shard took the frame. `backpressured` reports that its
    /// receive window closed as it did.
    Delivered {
        shard_idx: usize,
        backpressured: bool,
    },
    /// The owning shard refused the frame; the caller must stop draining
    /// and let the stack catch up.
    Backpressured,
    /// Nothing parsed the frame. It is consumed, but no shard changed.
    Malformed,
}

/// A shard's arrival signal sampled before the caller inspected that
/// shard, together with the shard it came from.
///
/// Carrying the two as one value is what keeps the wait hard to misuse:
/// an operation cannot park on one shard's signal holding another
/// shard's mark, and taking the mark is the same call that decides which
/// shard the operation belongs to.
#[derive(Clone, Copy)]
pub(super) struct ShardWait {
    pub(super) shard_idx: usize,
    pub(super) mark: ProgressMark,
}

pub(super) struct NetworkShardSet {
    pub(super) shards: Box<[PaddedShard]>,
}

impl ShardArrivals {
    pub(super) const fn new() -> Self {
        Self {
            touched: [0; NETWORK_RX_BATCH_FRAMES],
            len: 0,
        }
    }

    /// Records that `shard_idx` took a frame, ignoring a shard that is
    /// already in the set. The linear scan is over at most the batch
    /// size and beats a bitmap that would have to be sized to the
    /// machine's processor count.
    pub(super) fn record(&mut self, shard_idx: usize) {
        let shard_idx = u16::try_from(shard_idx)
            .unwrap_or_else(|_| panic!("network shard index {shard_idx} exceeds u16 range"));
        if self.touched[..self.len].contains(&shard_idx) {
            return;
        }
        assert!(
            self.len < self.touched.len(),
            "receive batch touched more shards than it drained frames"
        );
        self.touched[self.len] = shard_idx;
        self.len += 1;
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.touched[..self.len].iter().map(|idx| usize::from(*idx))
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl NetworkShardSet {
    /// Builds a shard set sized to `shard_count`. Each shard owns
    /// an independent `NetworkShard`, produced by `factory(i)` so
    /// the ctor can stagger per-shard fields (port allocator base,
    /// DHCP transaction id) across the set.
    pub(super) fn new<F>(shard_count: usize, mut factory: F) -> Self
    where
        F: FnMut(usize) -> NetworkShard,
    {
        assert!(shard_count != 0, "network shard count must be non-zero");
        let mut shards: Vec<PaddedShard> = Vec::with_capacity(shard_count);
        for index in 0..shard_count {
            shards.push(PaddedShard {
                inner: SpinMutex::new(factory(index)),
                arrival: ProgressSignal::new(),
            });
        }
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    pub(super) fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Picks the shard responsible for an unqualified operation
    /// (control-plane queries, admin commands that target every
    /// shard, etc.). Implemented in terms of `shard_for_handle(0)`
    /// so the routing rule lives in exactly one place.
    #[inline]
    pub(super) fn shard_for_default(&self) -> &SpinMutex<NetworkShard> {
        &self.shards[self.default_shard_idx()].inner
    }

    /// Index of the shard [`Self::shard_for_default`] resolves to.
    #[inline]
    pub(super) fn default_shard_idx(&self) -> usize {
        // Handle id 1 is the canonical "shard 0" id under the
        // stride encoding (slot 0 in shard 0 -> id = 1), so
        // shard_for_default and shard_for_handle agree on shard 0.
        self.shard_idx_for_handle(1u64)
    }

    /// Picks the shard owning a given socket / connection handle
    /// using the inverse of `NetworkShard::encode_handle_id`:
    /// `(handle - 1) % shard_count == shard_idx`.
    #[inline]
    pub(super) fn shard_for_handle<H: Into<u64>>(&self, handle: H) -> &SpinMutex<NetworkShard> {
        &self.shards[self.shard_idx_for_handle(handle)].inner
    }

    /// Index of the shard owning `handle`.
    #[inline]
    pub(super) fn shard_idx_for_handle<H: Into<u64>>(&self, handle: H) -> usize {
        let handle: u64 = handle.into();
        let normalized = handle.saturating_sub(1) as usize;
        normalized % self.shards.len()
    }

    /// The shard's cross-processor arrival signal. Raised by whichever
    /// processor placed a frame in the shard; waited on by the
    /// operations the shard owns.
    #[inline]
    pub(super) fn arrival(&self, idx: usize) -> &ProgressSignal {
        &self.shards[idx].arrival
    }

    /// The processor whose executor owns `idx`'s work.
    ///
    /// The inverse of [`Self::shard_idx_for_processor`] for the
    /// `shard_count == processor_count` layout the service builds:
    /// shard `i` belongs to processor `i`.
    #[inline]
    pub(super) fn owner_processor(&self, idx: usize) -> helios_hal::cpu::ProcessorId {
        helios_hal::cpu::ProcessorId::new(
            u16::try_from(idx)
                .unwrap_or_else(|_| panic!("network shard index {idx} exceeds processor id range")),
        )
    }

    /// Samples `idx`'s arrival signal. Callers take this *before* they
    /// inspect the shard, so an arrival that lands between the
    /// inspection and the park is still observed.
    #[inline]
    pub(super) fn shard_wait(&self, idx: usize) -> ShardWait {
        ShardWait {
            shard_idx: idx,
            mark: self.arrival(idx).mark(),
        }
    }

    /// Samples the arrival signal of the shard owning `handle`.
    #[inline]
    pub(super) fn shard_wait_for_handle<H: Into<u64>>(&self, handle: H) -> ShardWait {
        self.shard_wait(self.shard_idx_for_handle(handle))
    }

    /// Samples the arrival signal of the shard owning `port`.
    #[inline]
    pub(super) fn shard_wait_for_local_port(&self, port: u16) -> ShardWait {
        self.shard_wait(self.shard_idx_for_local_port(port))
    }

    /// Samples the arrival signal of the shard that unqualified
    /// operations run on.
    #[inline]
    pub(super) fn default_shard_wait(&self) -> ShardWait {
        self.shard_wait(self.default_shard_idx())
    }

    /// Releases every operation parked on a shard that just took a
    /// frame, and pulls the owning processor out of its idle park when
    /// the frame was drained somewhere else.
    ///
    /// Signalling happens after the per-frame shard locks are released,
    /// so a woken waiter never contends with the drain that woke it.
    pub(super) fn notify_arrivals<CpuImpl: Cpu>(&self, arrivals: &ShardArrivals, cpu: &CpuImpl) {
        if arrivals.is_empty() {
            return;
        }
        let current = cpu.current_processor();
        for shard_idx in arrivals.iter() {
            self.arrival(shard_idx).signal();
            let owner = self.owner_processor(shard_idx);
            if owner != current {
                cpu.wake_processor(owner);
            }
        }
    }

    /// Places one received frame in the shard that owns its local port.
    ///
    /// This is the single RX ownership decision in the kernel: the
    /// destination port picks the shard, the shard's lock is taken for
    /// exactly as long as the stack needs the frame, and the returned
    /// shard index is what the caller signals once it has let go.
    /// Non-IP and non-TCP/UDP frames (ARP, ICMP, malformed) carry no
    /// local port and route to shard 0.
    pub(super) fn dispatch_rx_frame(
        &self,
        frame: &RxFrame,
        received_at: StackInstant,
        control: &NetworkControlPlane,
    ) -> RxFrameDispatch {
        let port = peek_local_port(frame.as_ref());
        let shard_idx = shard_idx_for_port(port, self.shard_count());
        let mut shard = self.shards[shard_idx].inner.lock();
        let dispatch = match shard.stack.receive_rx_frame(frame.clone(), received_at) {
            Ok(backpressured) => RxFrameDispatch::Delivered {
                shard_idx,
                backpressured,
            },
            Err(StackError::ReceiveBackpressure) => RxFrameDispatch::Backpressured,
            Err(error) => {
                tracing::debug!(?error, "dropped malformed network frame");
                RxFrameDispatch::Malformed
            }
        };
        shard.drain_control_events(control);
        dispatch
    }

    /// Locks shard `idx` directly. Used by the RX demux which has
    /// already computed the target shard from the frame's
    /// destination port; bypasses the handle-encoding round-trip.
    #[inline]
    pub(super) fn shard_at(&self, idx: usize) -> &SpinMutex<NetworkShard> {
        &self.shards[idx].inner
    }

    /// Picks the shard that should receive a new socket created on
    /// the given processor. Future RX traffic for the socket's
    /// stride-allocated ephemeral port will demux back to this
    /// shard, so creation must place the slab entry here. Falls
    /// back to shard 0 for processor ids out of the configured
    /// range.
    #[inline]
    pub(super) fn shard_for_processor(
        &self,
        processor: helios_hal::cpu::ProcessorId,
    ) -> &SpinMutex<NetworkShard> {
        &self.shards[self.shard_idx_for_processor(processor)].inner
    }

    /// Index of the shard [`Self::shard_for_processor`] resolves to.
    #[inline]
    pub(super) fn shard_idx_for_processor(&self, processor: helios_hal::cpu::ProcessorId) -> usize {
        (processor.id() as usize) % self.shards.len()
    }

    /// Index of the shard owning a fixed local port.
    #[inline]
    pub(super) fn shard_idx_for_local_port(&self, port: u16) -> usize {
        shard_idx_for_port(Some(port), self.shards.len())
    }

    pub(super) fn with<R>(&self, f: impl FnOnce(&NetworkShard) -> R) -> R {
        let state = self.shard_for_default().lock();
        f(&state)
    }

    pub(super) fn with_mut<R>(&self, f: impl FnOnce(&mut NetworkShard) -> R) -> R {
        let mut state = self.shard_for_default().lock();
        f(&mut state)
    }

    /// Locks the shard owning `handle` and runs the closure against
    /// it under `&mut`. Used by every op that takes a socket /
    /// listener handle so the dispatch decision stays in
    /// `shard_for_handle`. Mutable form is the universal one because
    /// every socket op the caller might run (read drains the socket
    /// receive queue, write enqueues, close removes the slab entry,
    /// etc.) needs interior mutation; a read-only sibling would only
    /// be useful for diagnostic peeks the kernel does not currently
    /// expose.
    pub(super) fn with_handle<H: Into<u64>, R>(
        &self,
        handle: H,
        f: impl FnOnce(&mut NetworkShard) -> R,
    ) -> R {
        let mut state = self.shard_for_handle(handle).lock();
        f(&mut state)
    }

    /// Locks the shard owning the given processor and runs the
    /// closure against it. Used by socket-creation paths so the
    /// new socket lives on the processor that minted it; ephemeral
    /// ports allocated under the stride scheme will demux back to
    /// this shard for incoming traffic.
    pub(super) fn with_processor<R>(
        &self,
        processor: helios_hal::cpu::ProcessorId,
        f: impl FnOnce(&mut NetworkShard) -> R,
    ) -> R {
        let mut state = self.shard_for_processor(processor).lock();
        f(&mut state)
    }

    /// Locks the shard owning a fixed local port (server listener,
    /// explicit UDP bind). Maps via `shard_idx_for_port` so RX
    /// demux for the same port reaches the same shard.
    pub(super) fn with_local_port<R>(
        &self,
        port: u16,
        f: impl FnOnce(&mut NetworkShard) -> R,
    ) -> R {
        let mut state = self.shards[self.shard_idx_for_local_port(port)]
            .inner
            .lock();
        f(&mut state)
    }

    /// Iterates every shard in the set, calling `f` once per shard
    /// under its own lock. Used by control-plane ops that target the
    /// whole stack — clearing routes, listing IPv4 addresses, the
    /// upcoming control task pushing DNS results to all shards, etc.
    pub(super) fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(&mut NetworkShard),
    {
        for shard in &self.shards {
            let mut guard = shard.inner.lock();
            f(&mut guard);
        }
    }

    pub(super) fn min_tcp_deadline_nanos(&self) -> Option<u64> {
        let mut next = None;
        self.for_each(|shard| {
            if let Some(deadline) = shard.stack.next_tcp_deadline().map(StackInstant::nanos) {
                next = Some(next.map_or(deadline, |current: u64| current.min(deadline)));
            }
        });
        next
    }
}

impl NetworkShard {
    /// Builds one shard around a stack configuration the service
    /// derived once from the device's capabilities: every shard drives
    /// the same interface, so they all share the same offload contract.
    pub(super) fn new(
        stack_config: StackConfig,
        transaction_id: u32,
        shard_idx: usize,
        shard_count: usize,
    ) -> Self {
        assert!(shard_count != 0, "network shard count must be non-zero");
        assert!(
            shard_idx < shard_count,
            "shard idx {shard_idx} out of range for {shard_count} shards"
        );
        let initial_ephemeral = EPHEMERAL_PORT_START + shard_idx as u16;
        let mut stack = Box::new(Stack::new(stack_config));
        let mut udp_sockets = HandleSlab::new();
        if shard_idx == shard_idx_for_port(Some(DHCP_CLIENT_PORT), shard_count) {
            let binding = UdpSocketBinding::wildcard(DHCP_CLIENT_PORT);
            let stack_socket = stack
                .open_udp(binding)
                .unwrap_or_else(|error| panic!("failed to open DHCP UDP socket: {error}"));
            let slot = udp_sockets.insert(UdpSocketState {
                stack_socket,
                binding,
            });
            assert_eq!(
                slot, INTERNAL_DHCP_SOCKET_INDEX,
                "DHCP internal UDP socket slot changed"
            );
        }
        if shard_idx == shard_idx_for_port(Some(INTERNAL_DNS_PORT), shard_count) {
            let binding = UdpSocketBinding::wildcard(INTERNAL_DNS_PORT);
            let stack_socket = stack
                .open_udp(binding)
                .unwrap_or_else(|error| panic!("failed to open DNS UDP socket: {error}"));
            let slot = udp_sockets.insert(UdpSocketState {
                stack_socket,
                binding,
            });
            assert_eq!(
                slot, INTERNAL_DNS_SOCKET_INDEX,
                "DNS internal UDP socket slot changed"
            );
        }
        Self {
            stack,
            shard_idx,
            shard_count,
            next_tcp_local_port: initial_ephemeral,
            next_udp_local_port: initial_ephemeral,
            tcp_streams: HandleSlab::new(),
            tcp_listeners: HandleSlab::new(),
            udp_sockets,
            dhcp: DhcpClientState::Init { transaction_id },
            dns_servers: DhcpDnsServers::new(),
            next_dns_query_id: 1,
            next_icmp_echo_identifier: 1,
        }
    }

    /// Encodes a per-shard slab index into a globally unique handle
    /// id whose value satisfies `(id - 1) % shard_count == shard_idx`.
    /// This is the inverse of `NetworkShardSet::shard_for_handle`
    /// so an operation arriving with a handle can route directly to
    /// the shard that minted it without consulting a side table.
    pub(super) fn encode_handle_id(&self, slot: usize) -> u32 {
        let stride = self.shard_count;
        let raw = slot
            .checked_mul(stride)
            .and_then(|product| product.checked_add(self.shard_idx + 1))
            .unwrap_or_else(|| {
                panic!(
                    "network handle slot {slot} overflowed stride {stride} encoding for shard {}",
                    self.shard_idx
                )
            });
        u32::try_from(raw).unwrap_or_else(|_| panic!("encoded handle id {raw} exceeds u32 range"))
    }

    /// Reverses `encode_handle_id`. Panics if the handle was minted
    /// by a different shard, since misrouted handles indicate a
    /// caller-side dispatch bug.
    pub(super) fn decode_handle_slot(&self, raw: u32) -> usize {
        let value = (raw - 1) as usize;
        assert_eq!(
            value % self.shard_count,
            self.shard_idx,
            "handle id {raw} routed to shard {} but encodes shard {}",
            self.shard_idx,
            value % self.shard_count
        );
        value / self.shard_count
    }

    pub(super) fn is_configured(&self) -> bool {
        self.stack.primary_ipv4_address().is_some()
    }

    pub(super) fn drain_control_events(&mut self, control: &NetworkControlPlane) {
        while let Some(event) = self.stack.take_event() {
            match event {
                StackEvent::NeighborUpdated(entry) => {
                    control.update_neighbors(|neighbors| neighbors.learn(entry));
                }
                StackEvent::DhcpConfigured(_) | StackEvent::Ipv6Autoconfigured(_) => {
                    control.publish_from_shard(self)
                }
                StackEvent::UdpDatagram { .. }
                | StackEvent::TcpConnected { .. }
                | StackEvent::TcpReadable { .. }
                | StackEvent::TcpClosed { .. } => {}
            }
        }
    }

    /// Number of ephemeral ports owned by this shard under the
    /// stride-allocation scheme. Equal to the total ephemeral
    /// range divided by the shard count, rounded up.
    pub(super) fn ephemeral_port_attempts(&self) -> usize {
        let total = usize::from(EPHEMERAL_PORT_END - EPHEMERAL_PORT_START) + 1;
        total.div_ceil(self.shard_count)
    }

    /// Advances the rolling allocator pointer by `shard_count`,
    /// wrapping back to the shard's first ephemeral port when we
    /// step past `EPHEMERAL_PORT_END`. The result is always a port
    /// that satisfies `(port - EPHEMERAL_PORT_START) % shard_count
    /// == shard_idx`, matching `shard_idx_for_port` so RX demux
    /// routes back to this shard.
    pub(super) fn advance_ephemeral_port(&self, current: u16) -> u16 {
        let stride = self.shard_count as u16;
        let next = current.checked_add(stride);
        match next {
            Some(value) if value <= EPHEMERAL_PORT_END => value,
            _ => EPHEMERAL_PORT_START + self.shard_idx as u16,
        }
    }

    pub(super) fn add_ipv4_address(
        &mut self,
        cidr: KernelIpv4Cidr,
    ) -> Result<(), NetworkControlError> {
        self.stack.add_ipv4_address(map_kernel_ipv4_cidr(cidr));
        Ok(())
    }

    pub(super) fn remove_ipv4_address(&mut self, cidr: KernelIpv4Cidr) {
        self.stack.remove_ipv4_address(map_kernel_ipv4_cidr(cidr));
    }

    pub(super) fn clear_ipv4_addresses(&mut self) {
        self.stack.clear_ipv4_addresses();
    }

    pub(super) fn list_ipv4_addresses(&self) -> Vec<KernelIpv4Cidr> {
        self.stack.ipv4_addresses().map(map_ipv4_cidr).collect()
    }

    pub(super) fn set_default_ipv4_gateway(
        &mut self,
        gateway: KernelIpv4Address,
    ) -> Result<(), NetworkControlError> {
        self.stack
            .routes_mut()
            .add(Route {
                destination: IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0)),
                gateway: Some(IpAddress::Ipv4(map_kernel_ipv4_address(gateway))),
                expires_at: None,
            })
            .map_err(|_| NetworkControlError::InvalidRoute)
    }

    pub(super) fn add_ipv4_route(
        &mut self,
        route: KernelIpv4Route,
    ) -> Result<(), NetworkControlError> {
        self.stack
            .routes_mut()
            .add(Route {
                destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(route.destination())),
                gateway: Some(IpAddress::Ipv4(map_kernel_ipv4_address(route.gateway()))),
                expires_at: route.expires_at_nanos().map(StackInstant::from_nanos),
            })
            .map_err(|_| NetworkControlError::InvalidRoute)
    }

    pub(super) fn remove_ipv4_route(&mut self, route: KernelIpv4Route) {
        self.stack.routes_mut().remove(Route {
            destination: IpCidr::Ipv4(map_kernel_ipv4_cidr(route.destination())),
            gateway: Some(IpAddress::Ipv4(map_kernel_ipv4_address(route.gateway()))),
            expires_at: route.expires_at_nanos().map(StackInstant::from_nanos),
        });
    }

    pub(super) fn clear_ipv4_routes(&mut self) {
        self.stack.routes_mut().clear_ipv4();
    }

    /// Starts interface configuration over after the link came back.
    ///
    /// The control plane owns addresses, routes, neighbours and
    /// resolvers and has already dropped them; what lives only in the
    /// shard is the configuration state machines, and both have to be
    /// returned to their initial state or nothing would ever ask the new
    /// link for an address.
    pub(super) fn restart_link_configuration(&mut self, transaction_id: u32) {
        self.stack.clear_ipv4_addresses();
        self.clear_ipv4_routes();
        self.stack.restart_ipv6_autoconfig();
        self.dns_servers.clear();
        self.dhcp = DhcpClientState::Init { transaction_id };
    }
}
