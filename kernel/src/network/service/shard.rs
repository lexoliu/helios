//! Per-processor network shards and the RX demux that feeds them.
//!
//! # SMP contract
//!
//! There is one [`NetworkShard`] per processor and every shard owns an
//! independent `Stack`, socket slab and port allocator. A flow belongs
//! to exactly one shard ([`shard_idx_for_frame`]), so the frames for a
//! socket are always placed in that socket's shard whichever processor
//! drained them off the device.
//!
//! Placement is therefore routinely cross-processor: the shard lock is
//! taken by the draining CPU, not the owning one. That hand-off is
//! completed by [`ShardCell::arrival`] — the draining CPU raises the
//! owning shard's progress signal after it releases the lock, and
//! [`NetworkShardSet::notify_arrivals`] additionally wakes the owning
//! processor when it is not the one that drained, so a parked executor
//! runs the released waiter instead of sleeping to its own deadline.
//!
//! A frame arrival is not the only producer of that signal. A read that
//! relieves a socket's receive backpressure
//! ([`NetworkShardSet::with_handle_receive_drain`]) raises it too: a
//! socket with no room refuses the segments offered to it, the peer is
//! window-blocked, nothing arrives, and the application read that makes
//! room produces no frame of its own for the pump to notice.
//!
//! What a full socket does *not* do is stop the interface. The receive
//! window is that socket's flow control and reaches no further: every
//! other flow, and every ARP, ICMP and DHCP frame, keeps being taken off
//! the device while it is shut (#143).
//!
//! Nothing in this module spins, and no shard lock is ever held across
//! an await point.

use super::*;
use crossbeam_utils::CachePadded;

/// The shard a received frame belongs to.
///
/// This is the one receive-side ownership rule in the kernel, and it is
/// the rule a device's RSS engine reproduces: the flow's Toeplitz hash,
/// modulo the shard count. A device that steers by the same hash
/// delivers the frame on the queue whose processor already owns the
/// socket; a device that cannot steer delivers everything on queue zero
/// and this routes it to the same shard anyway, so only the CPU hop
/// differs.
///
/// Three kinds of frame have no flow the hash can name and belong to
/// [`DEFAULT_SHARD_IDX`]:
///
/// * ARP, ICMP echo and neighbour discovery, which carry no ports;
/// * anything unparseable, which the owning stack will drop;
/// * the DHCP client's exchange, which is broadcast at a moment when
///   the interface has no address to hash a flow with.
///
/// An ICMP error is not one of them: it quotes the packet that provoked
/// it, so the flow it concerns is recoverable and the error is routed to
/// that flow's shard rather than to a shard with no socket to tell.
pub(super) fn shard_idx_for_frame(frame: &RxFrame, shard_count: usize) -> usize {
    // A device that steers has already hashed the frame to pick the
    // queue it delivered on, so taking its number rather than
    // recomputing the same one is the whole benefit of `HASH_REPORT`.
    // It is the same function over the same bytes under the same key,
    // so the answer does not depend on which side produced it.
    if let Some(hash) = frame.offload.flow_hash {
        return hash.bucket(shard_count);
    }
    flow_tuple_for_frame(frame.as_ref())
        .map(|tuple| flow_hash(&tuple).bucket(shard_count))
        .unwrap_or(DEFAULT_SHARD_IDX)
}

/// The received flow a frame belongs to, or `None` when it has none.
fn flow_tuple_for_frame(frame: &[u8]) -> Option<FlowTuple> {
    let ethernet = EthernetFrame::parse(frame)?;
    let (protocol, l4_payload) = match ethernet.protocol {
        EthernetProtocol::Ipv4 => {
            let packet = Ipv4Packet::parse(ethernet.payload)?;
            (packet.protocol, packet.payload)
        }
        EthernetProtocol::Ipv6 => {
            let packet = Ipv6Packet::parse(ethernet.payload)?;
            (packet.next_header, packet.payload)
        }
        _ => return None,
    };
    match protocol {
        IpProtocol::Tcp | IpProtocol::Udp => {
            if is_dhcp_exchange(protocol, l4_payload) {
                return None;
            }
            FlowTuple::from_frame(frame)
        }
        IpProtocol::Icmp => icmpv4_quoted_flow(l4_payload),
        IpProtocol::Icmpv6 => icmpv6_quoted_flow(l4_payload),
    }
}

/// Whether a datagram is part of the DHCP client's exchange.
///
/// It is checked on the ports rather than on the addresses because a
/// reply may be unicast or broadcast, and the client has no address of
/// its own to match against until the exchange has finished.
fn is_dhcp_exchange(protocol: IpProtocol, payload: &[u8]) -> bool {
    if protocol != IpProtocol::Udp {
        return false;
    }
    let Some(packet) = UdpPacket::parse(payload) else {
        return false;
    };
    [packet.source_port, packet.destination_port]
        .iter()
        .any(|port| *port == DHCP_CLIENT_PORT || *port == DHCP_SERVER_PORT)
}

/// The flow an ICMPv4 error concerns, in the direction this interface
/// receives it.
///
/// The quoted header is the packet *we sent*, so its tuple is the
/// outgoing direction and has to be reversed to match how the frames of
/// that flow arrive — which is what the hash is taken over.
fn icmpv4_quoted_flow(bytes: &[u8]) -> Option<FlowTuple> {
    let Icmpv4Packet::DestinationUnreachable(unreachable) = Icmpv4Packet::parse(bytes)? else {
        return None;
    };
    let quoted = Ipv4Packet::parse_quoted(unreachable.original)?;
    let ports = quoted_ports(quoted.protocol, quoted.payload)?;
    Some(FlowTuple::ipv4(quoted.source, ports.0, quoted.destination, ports.1).reversed())
}

/// The flow an ICMPv6 error concerns, in the receive direction.
fn icmpv6_quoted_flow(bytes: &[u8]) -> Option<FlowTuple> {
    let original = match Icmpv6Packet::parse(bytes)? {
        Icmpv6Packet::DestinationUnreachable(unreachable) => unreachable.original,
        Icmpv6Packet::PacketTooBig(packet_too_big) => packet_too_big.original,
        _ => return None,
    };
    let quoted = Ipv6Packet::parse_quoted(original)?;
    let ports = quoted_ports(quoted.next_header, quoted.payload)?;
    Some(FlowTuple::ipv6(quoted.source, ports.0, quoted.destination, ports.1).reversed())
}

/// The port pair of a quoted transport header. An ICMP error only has
/// to quote the first eight bytes of it, which is exactly the ports.
fn quoted_ports(protocol: IpProtocol, payload: &[u8]) -> Option<(u16, u16)> {
    let ports = match protocol {
        IpProtocol::Tcp => TcpPacket::parse_ports(payload)?,
        IpProtocol::Udp => UdpPacket::parse_ports(payload)?,
        _ => return None,
    };
    Some((ports.source, ports.destination))
}

/// Ports per shard when the ephemeral range is split evenly.
///
/// The last shard keeps whatever remainder the division leaves, so
/// every port in the range belongs to exactly one window.
fn ephemeral_window_len(shard_count: usize) -> u16 {
    let total = u32::from(EPHEMERAL_PORT_END - EPHEMERAL_PORT_START) + 1;
    let window = total / (shard_count as u32).max(1);
    u16::try_from(window.max(1)).unwrap_or(u16::MAX)
}

/// The shard that will receive a flow between these endpoints.
///
/// Called before a socket is opened, so the socket is placed where its
/// frames will arrive. The tuple is the receive direction — remote to
/// local — because that is what the demux hashes, and the standard
/// Toeplitz key is not symmetric.
pub(super) fn shard_idx_for_flow(
    local: IpAddress,
    local_port: u16,
    remote: IpAddress,
    remote_port: u16,
    shard_count: usize,
) -> usize {
    FlowTuple::between(remote, remote_port, local, local_port)
        .map(|tuple| flow_hash(&tuple).bucket(shard_count))
        .unwrap_or(DEFAULT_SHARD_IDX)
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

/// One shard's stack and the per-shard words beside it.
///
/// The set holds these behind [`CachePadded`], so two adjacent
/// shards' lock words never share a cache line, nor the 128-byte pair
/// that x86-64's adjacent-line prefetcher and aarch64 cores move as
/// one; without that, every cross-CPU lock operation on one shard
/// would ping-pong the line its neighbour's lock lives on. The
/// single-shard build pays the padding too, to keep the layout
/// invariant.
pub(super) struct ShardCell {
    pub(super) inner: SpinMutex<NetworkShard>,
    /// Frames this shard's stack has taken off the device, and frames
    /// it has handed back to it.
    ///
    /// Counted per shard because that is the distribution the steering
    /// work exists to produce: a device that steers spreads a machine's
    /// flows across these, and one that cannot leaves them all on the
    /// default shard's counter. Relaxed atomics — a statistic that is a
    /// few frames stale is still the right answer.
    pub(super) rx_frames: AtomicU64,
    pub(super) tx_frames: AtomicU64,
    /// Frames the demux had to throw away because this shard's stack
    /// refused them: the segment was already off the ring and there is
    /// nowhere to put it back. A nonzero count on one shard while the
    /// others move frames is the signature of a receiver that is not
    /// keeping up; a nonzero count that keeps climbing while the shard
    /// has no live socket is the signature of a leaked window.
    pub(super) rx_refused_frames: AtomicU64,
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
    /// The owning shard took the frame.
    Delivered { shard_idx: usize },
    /// The owning shard refused the frame because the socket it was for
    /// has no room. The frame is lost — it is already off the ring — and
    /// the shard is named so the loss is counted against the shard that
    /// caused it rather than against the device.
    Backpressured { shard_idx: usize },
    /// Nothing parsed the frame. It is consumed, but no shard changed.
    Malformed,
}

/// A socket handle with the shard that owns it written into it.
///
/// The owner used to be recovered as `(id - 1) % shard_count`, which
/// only worked because a socket was placed on the shard its ephemeral
/// port strided to — the id encoded the *placement rule* rather than
/// the placement. Ownership now follows the flow hash, so there is no
/// rule left to invert and the owner is carried explicitly: the high
/// half of the id is the shard, the low half is that shard's slab slot.
///
/// The low half is stored as `slot + 1` so a handle is never zero, and
/// so slot 0 of shard 0 is still a valid id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ShardHandle(NonZeroU32);

impl ShardHandle {
    const SLOT_BITS: u32 = 16;
    const SLOT_MASK: u32 = (1 << Self::SLOT_BITS) - 1;

    pub(super) fn new(owner: usize, slot: usize) -> Self {
        let owner = u32::try_from(owner)
            .ok()
            .filter(|owner| *owner <= Self::SLOT_MASK)
            .unwrap_or_else(|| panic!("network shard {owner} does not fit a handle's owner half"));
        let encoded_slot = u32::try_from(slot)
            .ok()
            .and_then(|slot| slot.checked_add(1))
            .filter(|slot| *slot <= Self::SLOT_MASK)
            .unwrap_or_else(|| panic!("network handle slot {slot} does not fit a handle"));
        Self(
            NonZeroU32::new((owner << Self::SLOT_BITS) | encoded_slot)
                .unwrap_or_else(|| panic!("network handle slot {slot} encoded to zero")),
        )
    }

    /// The shard that minted this handle and owns the socket behind it.
    pub(super) const fn owner(self) -> usize {
        (self.0.get() >> Self::SLOT_BITS) as usize
    }

    /// The owning shard's slab slot.
    pub(super) const fn slot(self) -> usize {
        ((self.0.get() & Self::SLOT_MASK) - 1) as usize
    }

    pub(super) const fn get(self) -> NonZeroU32 {
        self.0
    }

    /// Rebuilds a handle a component host round-tripped through a raw
    /// integer. A value that never named a slot is a caller bug, not a
    /// recoverable error.
    pub(super) fn from_raw(raw: NonZeroU32) -> Self {
        assert!(
            raw.get() & Self::SLOT_MASK != 0,
            "network handle {raw} carries no slab slot"
        );
        Self(raw)
    }
}

/// A handle for a socket that exists on every shard at once.
///
/// A listener and a wildcard-bound datagram socket cannot belong to one
/// shard: the flow hash of an inbound SYN or of a datagram from an
/// arbitrary peer is not known when the socket is opened, so it lands
/// wherever the hash says. Both are therefore installed on every shard
/// in the *same* slab slot, and the handle names only that slot — the
/// shard is chosen at receive time by the hash rather than written into
/// the id.
///
/// The slot is stored as `slot + 1` so a handle is never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ReplicaHandle(NonZeroU32);

impl ReplicaHandle {
    pub(super) fn new(slot: usize) -> Self {
        let encoded = u32::try_from(slot)
            .ok()
            .and_then(|slot| slot.checked_add(1))
            .unwrap_or_else(|| panic!("replicated handle slot {slot} does not fit a handle"));
        Self(
            NonZeroU32::new(encoded)
                .unwrap_or_else(|| panic!("replicated handle slot {slot} encoded to zero")),
        )
    }

    /// The slab slot this socket occupies on every shard.
    pub(super) const fn slot(self) -> usize {
        (self.0.get() - 1) as usize
    }

    pub(super) const fn get(self) -> NonZeroU32 {
        self.0
    }

    pub(super) const fn from_raw(raw: NonZeroU32) -> Self {
        Self(raw)
    }
}

/// Slab slots reserved across every shard at once.
///
/// A replicated socket has to occupy the same slot in each shard's
/// slab, which the per-shard free lists cannot agree on by themselves.
/// This allocator is the single owner of that decision; the shards then
/// take the slot it hands out.
pub(super) struct ReplicaSlots<const CAPACITY: usize> {
    used: SpinMutex<[bool; CAPACITY]>,
}

impl<const CAPACITY: usize> ReplicaSlots<CAPACITY> {
    /// Builds an allocator whose first `reserved` slots are already
    /// taken, for sockets the shards install at construction.
    pub(super) fn new(reserved: usize) -> Self {
        assert!(
            reserved <= CAPACITY,
            "cannot reserve {reserved} of {CAPACITY} replicated slots"
        );
        let mut used = [false; CAPACITY];
        for slot in &mut used[..reserved] {
            *slot = true;
        }
        Self {
            used: SpinMutex::new(used),
        }
    }

    /// Takes the lowest free slot, or `None` when the table is full.
    pub(super) fn allocate(&self) -> Option<usize> {
        let mut used = self.used.lock();
        let slot = used.iter().position(|taken| !*taken)?;
        used[slot] = true;
        Some(slot)
    }

    /// Returns a slot after every shard has dropped its replica.
    pub(super) fn release(&self, slot: usize) {
        let mut used = self.used.lock();
        assert!(
            core::mem::replace(&mut used[slot], false),
            "replicated slot {slot} was released twice"
        );
    }
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
    pub(super) target: WaitTarget,
    pub(super) mark: ProgressMark,
}

/// Everything a network operation samples before it inspects the state
/// it means to park on.
///
/// Two things can end such a park and both are races rather than polls:
/// a frame another processor placed in the shard, which raises the
/// shard's arrival signal, and the interface's own event, which is what
/// a frame nobody has drained yet raises. A wait that carries only the
/// first arms the second *after* the caller has already looked, and an
/// interrupt in that window is lost — permanently, once a full receive
/// ring stops the device raising any more.
///
/// So the two marks are taken together, in the one call that also
/// decides which shard and which queue pair the operation belongs to,
/// and [`NetworkService::wait_for_shard_progress`] takes nothing else.
#[derive(Clone, Copy)]
pub(super) struct NetworkWait {
    pub(super) shard: ShardWait,
    /// The queue pair this wait watches, fixed when the marks were
    /// taken rather than at the park: a task that migrates between the
    /// two would otherwise park on a pair whose mark it never sampled.
    pub(super) queue_idx: usize,
    pub(super) device: InterfaceEventMark,
}

/// Which arrival signal an operation parks on.
///
/// A socket that lives on one shard waits for that shard. A replicated
/// socket cannot: an inbound connection or a datagram from an arbitrary
/// peer lands on whichever shard its flow hashes to, so the operation
/// waits for the whole set instead.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WaitTarget {
    Shard(usize),
    AnyShard,
}

pub(super) struct NetworkShardSet {
    pub(super) shards: Box<[CachePadded<ShardCell>]>,
    /// Slots for listeners, which exist on every shard because an
    /// inbound connection's hash is not known when the port is opened.
    pub(super) listener_slots: ReplicaSlots<MAX_TCP_LISTENER_HANDLES>,
    /// Slots for bound datagram sockets, for the same reason.
    pub(super) udp_slots: ReplicaSlots<MAX_UDP_SOCKET_HANDLES>,
    /// Raised whenever any shard makes receive-side progress.
    ///
    /// Two kinds of waiter have no single shard to watch. A replicated
    /// socket's accept and datagram-receive calls do not know which
    /// shard their next connection or datagram will hash to, and the
    /// packet pump serves every shard at once. Raising it costs one
    /// atomic increment per receive batch.
    pub(super) any_arrival: ProgressSignal,
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
        let mut shards: Vec<CachePadded<ShardCell>> = Vec::with_capacity(shard_count);
        for index in 0..shard_count {
            shards.push(CachePadded::new(ShardCell {
                inner: SpinMutex::new(factory(index)),
                rx_frames: AtomicU64::new(0),
                tx_frames: AtomicU64::new(0),
                rx_refused_frames: AtomicU64::new(0),
                arrival: ProgressSignal::new(),
            }));
        }
        Self {
            shards: shards.into_boxed_slice(),
            listener_slots: ReplicaSlots::new(0),
            // Slot zero is the DHCP client, which every shard reserves
            // so a replicated bind cannot land on top of it.
            udp_slots: ReplicaSlots::new(INTERNAL_UDP_RESERVED_SLOTS),
            any_arrival: ProgressSignal::new(),
        }
    }

    /// Installs a replicated socket on every shard, or on none.
    ///
    /// A partial install would leave a port taken on some shards and
    /// free on others, and the next bind would then disagree with the
    /// receive path about who owns it, so a shard that refuses unwinds
    /// the ones that already took it.
    pub(super) fn install_replica<E>(
        &self,
        slot: usize,
        mut install: impl FnMut(&mut NetworkShard, usize) -> Result<(), E>,
        mut remove: impl FnMut(&mut NetworkShard, usize),
    ) -> Result<(), E> {
        for (installed, shard) in self.shards.iter().enumerate() {
            let outcome = install(&mut shard.inner.lock(), slot);
            if let Err(error) = outcome {
                for unwound in &self.shards[..installed] {
                    remove(&mut unwound.inner.lock(), slot);
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Runs `f` against each shard's replica in turn, starting at
    /// `start`, and stops at the first one that answers.
    ///
    /// Receiving from a replicated socket works this way: the caller's
    /// own shard is the likeliest to hold something and costs no
    /// cross-processor traffic, and the rest are visited afterwards so
    /// a datagram or a connection the hash placed elsewhere is never
    /// stranded. An error from any replica ends the walk, because a
    /// replicated socket that failed on one shard is not healthy on the
    /// others.
    pub(super) fn find_in_replicas<R, E>(
        &self,
        start: usize,
        mut f: impl FnMut(&mut NetworkShard) -> Result<Option<R>, E>,
    ) -> Result<Option<R>, E> {
        for offset in 0..self.shards.len() {
            let idx = (start + offset) % self.shards.len();
            let mut shard = self.shards[idx].inner.lock();
            if let Some(found) = f(&mut shard)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// Retargets every replica of a socket, in shard order.
    ///
    /// The first shard decides the outcome. Every replica holds the same
    /// binding and reads the same replicated address table, so a later
    /// shard that disagrees means the set has drifted — a kernel bug,
    /// not a caller error — and it is reported as one rather than left
    /// as a half-applied change.
    pub(super) fn for_each_replica<E: core::fmt::Display>(
        &self,
        operation: &'static str,
        mut f: impl FnMut(&mut NetworkShard) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut shards = self.shards.iter();
        let first = shards
            .next()
            .unwrap_or_else(|| panic!("a network shard set is never empty"));
        f(&mut first.inner.lock())?;
        for shard in shards {
            if let Err(error) = f(&mut shard.inner.lock()) {
                panic!(
                    "replicated {operation} succeeded on the first shard but failed on a later one: {error}"
                );
            }
        }
        Ok(())
    }

    pub(super) fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Picks the shard responsible for an unqualified operation
    /// (control-plane queries, admin commands that target every
    /// shard, etc.).
    #[inline]
    pub(super) fn shard_for_default(&self) -> &SpinMutex<NetworkShard> {
        &self.shards[self.default_shard_idx()].inner
    }

    /// Index of the shard [`Self::shard_for_default`] resolves to.
    ///
    /// The same shard that owns every frame the hash cannot name, so a
    /// control-plane operation and an ARP reply meet on one shard.
    #[inline]
    pub(super) const fn default_shard_idx(&self) -> usize {
        DEFAULT_SHARD_IDX
    }

    /// Picks the shard owning a given socket / connection handle.
    #[inline]
    pub(super) fn shard_for_handle<H: Into<ShardHandle>>(
        &self,
        handle: H,
    ) -> &SpinMutex<NetworkShard> {
        &self.shards[self.shard_idx_for_handle(handle)].inner
    }

    /// Index of the shard owning `handle`, as the handle itself records
    /// it.
    #[inline]
    pub(super) fn shard_idx_for_handle<H: Into<ShardHandle>>(&self, handle: H) -> usize {
        let owner = handle.into().owner();
        assert!(
            owner < self.shards.len(),
            "network handle names shard {owner} but the set has {}",
            self.shards.len()
        );
        owner
    }

    /// Records frames this shard took off the device.
    #[inline]
    pub(super) fn record_received(&self, idx: usize, frames: usize) {
        self.shards[idx]
            .rx_frames
            .fetch_add(frames as u64, AtomicOrdering::Relaxed);
    }

    /// Records frames this shard handed to the device.
    #[inline]
    pub(super) fn record_transmitted(&self, idx: usize, frames: usize) {
        self.shards[idx]
            .tx_frames
            .fetch_add(frames as u64, AtomicOrdering::Relaxed);
    }

    /// Records frames this shard would not take because a socket of
    /// its own had no room. They are lost, and the count is the size of
    /// that loss.
    #[inline]
    pub(super) fn record_receive_refused(&self, idx: usize, frames: usize) {
        self.shards[idx]
            .rx_refused_frames
            .fetch_add(frames as u64, AtomicOrdering::Relaxed);
    }

    /// Frames this shard has moved in each direction since boot.
    pub(super) fn frame_counts(&self, idx: usize) -> (u64, u64) {
        (
            self.shards[idx].rx_frames.load(AtomicOrdering::Relaxed),
            self.shards[idx].tx_frames.load(AtomicOrdering::Relaxed),
        )
    }

    /// What this shard's TCP layer has sent, and what it is currently
    /// advertising. Behind the shard lock, so this is a statistics-path
    /// call rather than a hot-path one.
    pub(super) fn tcp_counters(&self, idx: usize) -> TcpStackCounters {
        self.shards[idx].inner.lock().stack.tcp_counters()
    }

    /// Frames this shard refused for want of receive room since boot.
    pub(super) fn refused_frame_count(&self, idx: usize) -> u64 {
        self.shards[idx]
            .rx_refused_frames
            .load(AtomicOrdering::Relaxed)
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
            target: WaitTarget::Shard(idx),
            mark: self.arrival(idx).mark(),
        }
    }

    /// Samples the set-wide arrival signal, for a waiter that belongs
    /// to no single shard: an operation on a socket that lives on every
    /// shard, or the packet pump, which serves them all.
    #[inline]
    pub(super) fn any_shard_wait(&self) -> ShardWait {
        ShardWait {
            target: WaitTarget::AnyShard,
            mark: self.any_arrival.mark(),
        }
    }

    /// The signal a sampled wait belongs to.
    #[inline]
    pub(super) fn arrival_for(&self, target: WaitTarget) -> &ProgressSignal {
        match target {
            WaitTarget::Shard(idx) => self.arrival(idx),
            WaitTarget::AnyShard => &self.any_arrival,
        }
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
        for shard_idx in arrivals.iter() {
            self.raise_shard_progress(shard_idx, cpu);
        }
        // A replicated socket's operations watch the whole set, because
        // the shard their next connection or datagram lands on is not
        // known until its flow is hashed, and so does the packet pump,
        // which produces for every shard.
        self.any_arrival.signal();
    }

    /// Raises one shard's arrival signal and pulls the owning processor
    /// out of its idle park when this is not that processor.
    fn raise_shard_progress<CpuImpl: Cpu>(&self, shard_idx: usize, cpu: &CpuImpl) {
        self.arrival(shard_idx).signal();
        let owner = self.owner_processor(shard_idx);
        if owner != cpu.current_processor() {
            cpu.wake_processor(owner);
        }
    }

    /// Runs `f` against the shard owning `handle`, and signals that
    /// shard when the call relieved its receive backpressure.
    ///
    /// A frame arrival is not the only thing that unblocks a shard. A
    /// socket whose receive queue is full advertises a shut window, the
    /// peer stops sending, and the pump then receives nothing,
    /// transmits nothing and parks — and the only event that can start
    /// the flow again is the application read that makes room. That
    /// read happens on the socket's own task, under this lock, and
    /// produces no frame of its own, so it has to raise the signal
    /// itself: without it the pump sleeps to the next protocol
    /// deadline, which for a pure receiver is the DHCP retransmit
    /// interval a second away, while the peer sits window-blocked
    /// (#107).
    ///
    /// The signal is raised after the lock is released, so a woken
    /// waiter never contends with the drain that woke it.
    pub(super) fn with_handle_receive_drain<H, R, CpuImpl>(
        &self,
        handle: H,
        cpu: &CpuImpl,
        f: impl FnOnce(&mut NetworkShard) -> R,
    ) -> R
    where
        H: Into<ShardHandle>,
        CpuImpl: Cpu,
    {
        let shard_idx = self.shard_idx_for_handle(handle);
        let (result, relieved) = {
            let mut shard = self.shard_at(shard_idx).lock();
            let backpressured = shard.stack.receive_backpressured();
            let result = f(&mut shard);
            (
                result,
                backpressured && !shard.stack.receive_backpressured(),
            )
        };
        if relieved {
            self.raise_shard_progress(shard_idx, cpu);
            self.any_arrival.signal();
        }
        result
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
        let shard_idx = shard_idx_for_frame(frame, self.shard_count());
        let mut shard = self.shards[shard_idx].inner.lock();
        let dispatch = match shard.stack.receive_rx_frame(frame.clone(), received_at) {
            Ok(_) => RxFrameDispatch::Delivered { shard_idx },
            Err(StackError::ReceiveBackpressure) => RxFrameDispatch::Backpressured { shard_idx },
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

    /// The shard a socket created on `processor` should prefer.
    ///
    /// Only a preference: ownership follows the flow hash, and this is
    /// what an allocator aims at so a socket opened on one processor is
    /// also received on it.
    #[inline]
    pub(super) fn shard_idx_for_processor(&self, processor: helios_hal::cpu::ProcessorId) -> usize {
        (processor.id() as usize) % self.shards.len()
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
    pub(super) fn with_handle<H: Into<ShardHandle>, R>(
        &self,
        handle: H,
        f: impl FnOnce(&mut NetworkShard) -> R,
    ) -> R {
        let mut state = self.shard_for_handle(handle).lock();
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
        let initial_ephemeral =
            EPHEMERAL_PORT_START + (shard_idx as u16) * ephemeral_window_len(shard_count);
        let mut stack = Box::new(Stack::new(stack_config));
        let mut udp_sockets = HandleSlab::new();
        // Every shard opens the DHCP client socket, and always in the
        // same slab slot, because a replicated bind occupies one slot
        // index across the whole set and must not land on top of it.
        // Only the default shard ever receives on it — a DHCP exchange
        // is broadcast at a moment when the interface has no address to
        // hash a flow with — but the slot is spent everywhere.
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

    /// Mints a handle for one of this shard's slab slots, stamping this
    /// shard as its owner so an operation arriving with the handle
    /// routes straight back here without a side table.
    pub(super) fn encode_handle_id(&self, slot: usize) -> ShardHandle {
        ShardHandle::new(self.shard_idx, slot)
    }

    /// Reads the slab slot out of a handle this shard owns. Panics if
    /// the handle was minted by a different shard, since a misrouted
    /// handle is a caller-side dispatch bug.
    pub(super) fn decode_handle_slot(&self, handle: ShardHandle) -> usize {
        assert_eq!(
            handle.owner(),
            self.shard_idx,
            "handle routed to shard {} but names shard {}",
            self.shard_idx,
            handle.owner()
        );
        handle.slot()
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

    /// The contiguous slice of the ephemeral range this shard hands
    /// out, as an inclusive `(first, last)` pair.
    ///
    /// A socket's shard follows its flow hash, so the allocator no
    /// longer has to make a port decode back to a shard. What it still
    /// has to do is keep two shards from handing out the same port at
    /// the same moment, which one window each achieves without any
    /// shared state — and unlike the old stride it leaves each shard
    /// walking consecutive numbers instead of skipping `shard_count` at
    /// a time.
    pub(super) fn ephemeral_window(&self) -> (u16, u16) {
        let window = ephemeral_window_len(self.shard_count);
        let first = EPHEMERAL_PORT_START + (self.shard_idx as u16) * window;
        let last = if self.shard_idx + 1 == self.shard_count {
            EPHEMERAL_PORT_END
        } else {
            first + window - 1
        };
        (first, last)
    }

    /// How many candidates an allocation walk may try before it gives
    /// up: every port in this shard's window.
    pub(super) fn ephemeral_port_attempts(&self) -> usize {
        let (first, last) = self.ephemeral_window();
        usize::from(last - first) + 1
    }

    /// Advances the rolling allocator pointer, wrapping at the end of
    /// this shard's window.
    pub(super) fn advance_ephemeral_port(&self, current: u16) -> u16 {
        let (first, last) = self.ephemeral_window();
        if current >= last || current < first {
            first
        } else {
            current + 1
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
