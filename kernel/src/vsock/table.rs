//! The vsock connection table: every decision the protocol makes.
//!
//! The table is deliberately synchronous and device-free. Everything
//! that depends on time, on the executor, or on the device lives in
//! [`super::VsockService`]; what is left here is a state machine that
//! takes a packet in and hands back the packet that answers it, which is
//! what makes the handshake, the credit arithmetic and the shutdown
//! ordering testable without a virtual machine.
//!
//! Concurrency contract: the table is owned by one spin mutex in the
//! service. Every method is short, allocation-free apart from a
//! connection's one receive ring, and never awaits.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;

use helios_hal::vsock::{VsockAddress, VsockOp, VsockPacketHeader, VsockShutdown};

/// Bytes one connection may hold for a reader that has not collected
/// them yet. This is the window the connection announces as `buf_alloc`,
/// so it is also the most the peer may put in flight towards it.
pub const VSOCK_RECEIVE_WINDOW_BYTES: usize = 64 * 1024;

/// Connections one machine keeps at once.
///
/// The table is a fixed array rather than a map: the kernel's vsock
/// users are the inspector RPC transport and the debugger, a bounded
/// set, and a fixed table keeps connection setup allocation-free apart
/// from the receive ring each connection owns.
pub const MAX_VSOCK_CONNECTIONS: usize = 32;

/// Ports one machine listens on at once.
pub const MAX_VSOCK_LISTENERS: usize = 4;

/// Connections one listener queues before it starts refusing.
pub const MAX_VSOCK_BACKLOG: usize = 8;

/// First port the table hands out for an outbound connection.
///
/// Well-known host services bind low ports, so guest-initiated
/// connections take their local port from the top of the space, as
/// Linux's vsock does.
const EPHEMERAL_PORT_START: u32 = 0xffff_0000;

/// Why a vsock operation could not be carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VsockError {
    #[error("this machine has no vsock device")]
    Unavailable,
    #[error("vsock port {port} is already bound")]
    PortInUse { port: u32 },
    #[error("the peer refused the connection")]
    ConnectionRefused,
    #[error("the connection was reset by the peer")]
    ConnectionReset,
    #[error("the connection is closed")]
    Closed,
    #[error("the operation timed out")]
    Timeout,
    #[error("no free vsock connection slot; at most {MAX_VSOCK_CONNECTIONS} are open at once")]
    ConnectionTableFull,
    #[error("no free vsock listener slot; at most {MAX_VSOCK_LISTENERS} ports are bound at once")]
    ListenerTableFull,
    #[error("no free ephemeral vsock port")]
    NoEphemeralPort,
    #[error("the vsock handle names no live socket")]
    UnknownHandle,
    #[error("the vsock device failed: {0}")]
    Device(helios_hal::io::IoError),
}

/// A live connection's handle.
///
/// The generation half is what makes a stale handle detectable: a slot
/// reused by a later connection answers `UnknownHandle` rather than
/// reading another program's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VsockStreamId(u64);

/// A bound port's handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VsockListenerId(u64);

impl VsockStreamId {
    const fn new(index: usize, generation: u32) -> Self {
        Self(((generation as u64) << 32) | index as u64)
    }

    const fn index(self) -> usize {
        (self.0 & 0xffff_ffff) as usize
    }

    const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }

    /// The opaque value handed to a component-model resource.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub const fn from_u64(raw: u64) -> Self {
        Self(raw)
    }
}

impl VsockListenerId {
    const fn new(index: usize, generation: u32) -> Self {
        Self(((generation as u64) << 32) | index as u64)
    }

    const fn index(self) -> usize {
        (self.0 & 0xffff_ffff) as usize
    }

    const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub const fn from_u64(raw: u64) -> Self {
        Self(raw)
    }
}

/// A fixed-capacity byte ring: a connection's whole receive buffer.
///
/// Its capacity *is* the credit window the connection announces, so the
/// peer can never send more than fits and the table never has to drop
/// bytes it has already acknowledged.
struct ReceiveRing {
    bytes: Box<[u8]>,
    head: usize,
    len: usize,
}

impl ReceiveRing {
    fn new(capacity: usize) -> Self {
        Self {
            bytes: vec![0_u8; capacity].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    fn capacity(&self) -> usize {
        self.bytes.len()
    }

    fn len(&self) -> usize {
        self.len
    }

    fn free(&self) -> usize {
        self.capacity() - self.len
    }

    /// Appends `payload`, which the caller has already checked fits.
    fn push(&mut self, payload: &[u8]) {
        debug_assert!(payload.len() <= self.free());
        let capacity = self.capacity();
        let mut position = (self.head + self.len) % capacity;
        for byte in payload {
            self.bytes[position] = *byte;
            position = (position + 1) % capacity;
        }
        self.len += payload.len();
    }

    /// Copies at most `out.len()` bytes out, returning how many.
    fn drain(&mut self, out: &mut [u8]) -> usize {
        let capacity = self.capacity();
        let count = out.len().min(self.len);
        for (index, slot) in out[..count].iter_mut().enumerate() {
            *slot = self.bytes[(self.head + index) % capacity];
        }
        self.head = (self.head + count) % capacity;
        self.len -= count;
        count
    }
}

/// Where a connection is in its lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionState {
    /// A request went out; the peer has not answered yet.
    Connecting,
    /// Both ends agreed; bytes may flow.
    Established,
    /// The connection is gone, for the recorded reason.
    Closed(VsockError),
}

struct VsockConnection {
    generation: u32,
    local: VsockAddress,
    peer: VsockAddress,
    state: ConnectionState,
    receive: ReceiveRing,
    /// Bytes this end has handed to its reader — the `fwd_cnt` it
    /// announces.
    fwd_cnt: u32,
    /// The `fwd_cnt` value last announced to the peer. The gap between
    /// this and `fwd_cnt` is credit the peer does not know it has.
    announced_fwd_cnt: u32,
    /// The peer's announced receive buffer and consumption counter.
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// Bytes this end has sent, counted the same way the peer counts
    /// what it received.
    tx_cnt: u32,
    /// Directions this end has closed.
    local_shutdown: VsockShutdown,
    /// Directions the peer announced closed.
    peer_shutdown: VsockShutdown,
    /// A writer is mid-packet on this connection. Serialising writers
    /// per connection is what keeps two concurrent writes from
    /// interleaving inside the byte stream they share.
    transmitting: bool,
    /// A credit request is outstanding: the peer has been asked to
    /// re-announce its window and has not answered yet. Asking again
    /// while one is in flight would put a packet on the wire per poll.
    credit_requested: bool,
}

impl VsockConnection {
    /// Credit the peer has granted that this end has not spent.
    fn peer_free(&self) -> u32 {
        let in_flight = self.tx_cnt.wrapping_sub(self.peer_fwd_cnt);
        self.peer_buf_alloc.saturating_sub(in_flight)
    }

    /// This end's own window announcement, carried on every packet.
    fn announce(&mut self) -> (u32, u32) {
        self.announced_fwd_cnt = self.fwd_cnt;
        (
            u32::try_from(self.receive.capacity()).expect("the receive window fits a u32"),
            self.fwd_cnt,
        )
    }

    /// Folds the window the peer announced on an arriving packet.
    fn absorb_peer_credit(&mut self, header: &VsockPacketHeader) {
        self.peer_buf_alloc = header.buf_alloc;
        self.peer_fwd_cnt = header.fwd_cnt;
        self.credit_requested = false;
    }

    fn header(&mut self, op: VsockOp) -> VsockPacketHeader {
        let (buf_alloc, fwd_cnt) = self.announce();
        VsockPacketHeader {
            source: self.local,
            destination: self.peer,
            op,
            flags: 0,
            payload_len: 0,
            buf_alloc,
            fwd_cnt,
        }
    }

    /// Whether the reader has seen everything that will ever arrive.
    fn at_eof(&self) -> bool {
        self.receive.len() == 0 && (self.peer_shutdown.send || self.peer_shutdown.is_full())
    }
}

struct VsockListener {
    generation: u32,
    port: u32,
    backlog: usize,
    /// Connections that completed their handshake and are waiting to be
    /// accepted.
    queued: heapless::Deque<VsockStreamId, MAX_VSOCK_BACKLOG>,
}

/// What one arriving packet asks the service to transmit in reply.
///
/// Only control packets are ever produced here: the table answers a
/// handshake, a reset or a credit request itself, while payload always
/// originates from a writer that already owns the bytes.
pub type VsockReply = VsockPacketHeader;

/// A chunk of a write the table authorised.
///
/// `len` is what the connection's credit allows right now, which may be
/// less than the caller offered; the caller sends that prefix and comes
/// back for the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VsockWriteChunk {
    pub header: VsockPacketHeader,
    pub len: usize,
}

/// The outcome of asking the table to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VsockWriteProgress {
    /// Send this chunk, then report back with [`VsockTable::finish_write`].
    Ready(VsockWriteChunk),
    /// Nothing can go out yet: the peer's window is full, or another
    /// writer holds the connection. The caller waits for progress.
    Blocked,
}

/// The outcome of asking the table to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VsockReadProgress {
    /// Bytes were copied out. A window announcement rides along when
    /// the peer needs to hear that space was freed.
    Ready {
        len: usize,
        credit_update: Option<VsockReply>,
    },
    /// The peer closed its transmit side and the buffer is drained.
    Eof,
    /// Nothing has arrived yet.
    Blocked,
}

/// Every vsock connection and bound port on this machine.
pub struct VsockTable {
    guest_cid: u64,
    /// Largest payload one packet carries, as the device reports it.
    /// A write longer than this is split rather than refused.
    max_payload: usize,
    connections: [Option<VsockConnection>; MAX_VSOCK_CONNECTIONS],
    listeners: [Option<VsockListener>; MAX_VSOCK_LISTENERS],
    next_generation: u32,
    next_ephemeral_port: u32,
}

impl VsockTable {
    pub fn new(guest_cid: u64, max_payload: usize) -> Self {
        assert!(
            max_payload != 0,
            "a vsock device that carries no payload cannot serve a connection"
        );
        Self {
            guest_cid,
            max_payload,
            connections: [const { None }; MAX_VSOCK_CONNECTIONS],
            listeners: [const { None }; MAX_VSOCK_LISTENERS],
            next_generation: 1,
            next_ephemeral_port: EPHEMERAL_PORT_START,
        }
    }

    pub fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    fn take_generation(&mut self) -> u32 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        generation
    }

    /// Binds `port`, or an unused ephemeral port when `port` is zero.
    pub fn listen(&mut self, port: u32, backlog: usize) -> Result<VsockListenerId, VsockError> {
        let port = if port == 0 {
            self.allocate_ephemeral_port()?
        } else {
            if self.port_is_bound(port) {
                return Err(VsockError::PortInUse { port });
            }
            port
        };
        let generation = self.take_generation();
        let index = self
            .listeners
            .iter()
            .position(Option::is_none)
            .ok_or(VsockError::ListenerTableFull)?;
        self.listeners[index] = Some(VsockListener {
            generation,
            port,
            backlog: backlog.clamp(1, MAX_VSOCK_BACKLOG),
            queued: heapless::Deque::new(),
        });
        Ok(VsockListenerId::new(index, generation))
    }

    pub fn listener_port(&self, listener: VsockListenerId) -> Result<u32, VsockError> {
        Ok(self.listener(listener)?.port)
    }

    /// Takes the next connection queued on `listener`, if any.
    pub fn accept(
        &mut self,
        listener: VsockListenerId,
    ) -> Result<Option<VsockStreamId>, VsockError> {
        Ok(self.listener_mut(listener)?.queued.pop_front())
    }

    /// Releases a bound port. Connections it already produced stay open;
    /// they belong to whoever accepted them.
    pub fn close_listener(&mut self, listener: VsockListenerId) -> Result<(), VsockError> {
        let index = listener.index();
        self.listener(listener)?;
        self.listeners[index] = None;
        Ok(())
    }

    /// Opens a connection to `peer`, returning the handle and the
    /// request packet the caller transmits.
    pub fn connect(
        &mut self,
        peer: VsockAddress,
    ) -> Result<(VsockStreamId, VsockReply), VsockError> {
        let local_port = self.allocate_ephemeral_port()?;
        let local = VsockAddress::new(self.guest_cid, local_port);
        let stream = self.open(local, peer, ConnectionState::Connecting)?;
        let connection = self
            .connections
            .get_mut(stream.index())
            .and_then(Option::as_mut)
            .expect("the connection was just installed");
        let request = connection.header(VsockOp::Request);
        Ok((stream, request))
    }

    /// Whether a connection has finished its handshake.
    pub fn connect_progress(&self, stream: VsockStreamId) -> Result<bool, VsockError> {
        let connection = self.connection(stream)?;
        match connection.state {
            ConnectionState::Connecting => Ok(false),
            ConnectionState::Established => Ok(true),
            ConnectionState::Closed(error) => Err(error),
        }
    }

    pub fn peer(&self, stream: VsockStreamId) -> Result<VsockAddress, VsockError> {
        Ok(self.connection(stream)?.peer)
    }

    /// Authorises as much of `len` bytes as the peer's window allows.
    pub fn begin_write(
        &mut self,
        stream: VsockStreamId,
        len: usize,
    ) -> Result<VsockWriteProgress, VsockError> {
        let max_payload = self.max_payload;
        let connection = self.connection_mut(stream)?;
        match connection.state {
            ConnectionState::Closed(error) => return Err(error),
            ConnectionState::Connecting => return Ok(VsockWriteProgress::Blocked),
            ConnectionState::Established => {}
        }
        if connection.local_shutdown.send || connection.peer_shutdown.receive {
            return Err(VsockError::Closed);
        }
        if connection.transmitting {
            return Ok(VsockWriteProgress::Blocked);
        }
        let allowed = usize::try_from(connection.peer_free())
            .expect("a credit window fits a usize")
            .min(max_payload)
            .min(len);
        if allowed == 0 {
            return Ok(VsockWriteProgress::Blocked);
        }
        connection.transmitting = true;
        let mut header = connection.header(VsockOp::Data);
        header.payload_len = u32::try_from(allowed).expect("a bounded chunk fits a u32");
        Ok(VsockWriteProgress::Ready(VsockWriteChunk {
            header,
            len: allowed,
        }))
    }

    /// Records that an authorised chunk reached the device, or did not.
    ///
    /// Credit is spent only on the bytes that were actually handed over:
    /// a failed transmission that still advanced `tx_cnt` would leave
    /// the connection permanently believing the peer's window is
    /// narrower than it is.
    pub fn finish_write(&mut self, stream: VsockStreamId, sent: usize) {
        let Ok(connection) = self.connection_mut(stream) else {
            return;
        };
        connection.transmitting = false;
        connection.tx_cnt = connection
            .tx_cnt
            .wrapping_add(u32::try_from(sent).expect("a bounded chunk fits a u32"));
    }

    /// Asks the peer to re-announce its window, when this end is out of
    /// credit and has not already asked.
    ///
    /// A well-behaved peer announces a freed window on its own, so this
    /// exists for the case where that announcement was the packet that
    /// went missing; asking once per stall is what keeps a blocked
    /// writer from waiting for a peer that thinks it already told us.
    pub fn credit_request(
        &mut self,
        stream: VsockStreamId,
    ) -> Result<Option<VsockReply>, VsockError> {
        let connection = self.connection_mut(stream)?;
        if connection.state != ConnectionState::Established
            || connection.peer_free() != 0
            || connection.credit_requested
        {
            return Ok(None);
        }
        connection.credit_requested = true;
        Ok(Some(connection.header(VsockOp::CreditRequest)))
    }

    /// Copies out at most `out.len()` received bytes.
    pub fn read(
        &mut self,
        stream: VsockStreamId,
        out: &mut [u8],
    ) -> Result<VsockReadProgress, VsockError> {
        let connection = self.connection_mut(stream)?;
        if connection.local_shutdown.receive {
            return Err(VsockError::Closed);
        }
        let len = connection.receive.drain(out);
        if len == 0 {
            // A connection the peer closed cleanly ends in an end of
            // file; one it reset ends in that error, and the reader has
            // to be told which of the two happened.
            return match connection.state {
                ConnectionState::Closed(VsockError::Closed) => Ok(VsockReadProgress::Eof),
                ConnectionState::Closed(error) => Err(error),
                _ if connection.at_eof() => Ok(VsockReadProgress::Eof),
                _ => Ok(VsockReadProgress::Blocked),
            };
        }
        connection.fwd_cnt = connection
            .fwd_cnt
            .wrapping_add(u32::try_from(len).expect("a bounded read fits a u32"));
        // Announce the freed window once it is worth a packet. Telling
        // the peer after every read would put a control packet on the
        // wire per read; never telling it would stall the connection
        // once the window is spent.
        let unannounced = connection
            .fwd_cnt
            .wrapping_sub(connection.announced_fwd_cnt);
        let threshold = u32::try_from(connection.receive.capacity() / 2)
            .expect("half a receive window fits a u32");
        let credit_update =
            (unannounced >= threshold).then(|| connection.header(VsockOp::CreditUpdate));
        Ok(VsockReadProgress::Ready { len, credit_update })
    }

    /// Announces that this end closes `shutdown`'s directions.
    pub fn shutdown(
        &mut self,
        stream: VsockStreamId,
        shutdown: VsockShutdown,
    ) -> Result<Option<VsockReply>, VsockError> {
        let connection = self.connection_mut(stream)?;
        if matches!(connection.state, ConnectionState::Closed(_)) {
            return Ok(None);
        }
        let merged = connection.local_shutdown.merged(shutdown);
        if merged == connection.local_shutdown {
            return Ok(None);
        }
        connection.local_shutdown = merged;
        let mut header = connection.header(VsockOp::Shutdown);
        header.flags = merged.as_flags();
        Ok(Some(header))
    }

    /// Drops a connection, returning the reset that tells the peer.
    ///
    /// A reset rather than a shutdown is what a closed handle means: the
    /// program is gone, so there is nothing left to drain and nothing
    /// that could answer a graceful close.
    pub fn close(&mut self, stream: VsockStreamId) -> Result<Option<VsockReply>, VsockError> {
        let index = stream.index();
        let connection = self.connection_mut(stream)?;
        let reply = match connection.state {
            ConnectionState::Closed(_) => None,
            ConnectionState::Connecting | ConnectionState::Established => {
                Some(connection.header(VsockOp::Reset))
            }
        };
        self.connections[index] = None;
        Ok(reply)
    }

    /// Feeds one arriving packet through the state machine, returning
    /// the packet that answers it.
    pub fn handle_packet(
        &mut self,
        header: &VsockPacketHeader,
        payload: &[u8],
    ) -> Option<VsockReply> {
        if header.destination.cid != self.guest_cid {
            // Not addressed to this machine. Nothing here can answer for
            // a context id the hypervisor did not give us.
            tracing::warn!(
                cid = header.destination.cid,
                guest_cid = self.guest_cid,
                "vsock packet addressed to another context id"
            );
            return Some(header.reply(VsockOp::Reset));
        }
        match header.op {
            VsockOp::Request => self.handle_request(header),
            VsockOp::Response => self.handle_response(header),
            VsockOp::Reset => self.handle_reset(header),
            VsockOp::Shutdown => self.handle_shutdown(header),
            VsockOp::Data => self.handle_data(header, payload),
            VsockOp::CreditUpdate => self.handle_credit_update(header),
            VsockOp::CreditRequest => self.handle_credit_request(header),
        }
    }

    /// Closes every connection, as a transport reset requires.
    pub fn reset_all(&mut self) {
        for connection in self.connections.iter_mut().flatten() {
            connection.state = ConnectionState::Closed(VsockError::ConnectionReset);
        }
        for listener in self.listeners.iter_mut().flatten() {
            listener.queued.clear();
        }
    }

    fn handle_request(&mut self, header: &VsockPacketHeader) -> Option<VsockReply> {
        let local = header.destination;
        let peer = header.source;
        let Some(index) = self.listener_index_for_port(local.port) else {
            return Some(header.reply(VsockOp::Reset));
        };
        let listener = self.listeners[index]
            .as_ref()
            .expect("the listener index was just resolved");
        if listener.queued.len() >= listener.backlog {
            return Some(header.reply(VsockOp::Reset));
        }
        let Ok(stream) = self.open(local, peer, ConnectionState::Established) else {
            return Some(header.reply(VsockOp::Reset));
        };
        let connection = self
            .connections
            .get_mut(stream.index())
            .and_then(Option::as_mut)
            .expect("the connection was just installed");
        connection.absorb_peer_credit(header);
        let response = connection.header(VsockOp::Response);
        let listener = self.listeners[index]
            .as_mut()
            .expect("the listener index was just resolved");
        listener
            .queued
            .push_back(stream)
            .expect("the backlog was checked before the connection was opened");
        Some(response)
    }

    fn handle_response(&mut self, header: &VsockPacketHeader) -> Option<VsockReply> {
        let Some(connection) = self.connection_for_mut(header) else {
            return Some(header.reply(VsockOp::Reset));
        };
        connection.absorb_peer_credit(header);
        match connection.state {
            ConnectionState::Connecting => {
                connection.state = ConnectionState::Established;
                None
            }
            // A response for a connection that is already up is a
            // protocol violation, not a duplicate to be tolerated.
            ConnectionState::Established | ConnectionState::Closed(_) => {
                Some(header.reply(VsockOp::Reset))
            }
        }
    }

    fn handle_reset(&mut self, header: &VsockPacketHeader) -> Option<VsockReply> {
        let connection = self.connection_for_mut(header)?;
        connection.state = match connection.state {
            ConnectionState::Connecting => ConnectionState::Closed(VsockError::ConnectionRefused),
            _ => ConnectionState::Closed(VsockError::ConnectionReset),
        };
        None
    }

    fn handle_shutdown(&mut self, header: &VsockPacketHeader) -> Option<VsockReply> {
        let Some(connection) = self.connection_for_mut(header) else {
            return Some(header.reply(VsockOp::Reset));
        };
        connection.absorb_peer_credit(header);
        connection.peer_shutdown = connection.peer_shutdown.merged(header.shutdown());
        if connection.peer_shutdown.is_full() {
            // Both directions closed ends the connection; the reader
            // still drains what already arrived, and the state records
            // that the close was orderly.
            connection.state = ConnectionState::Closed(VsockError::Closed);
            return Some(header.reply(VsockOp::Reset));
        }
        None
    }

    fn handle_data(&mut self, header: &VsockPacketHeader, payload: &[u8]) -> Option<VsockReply> {
        let Some(connection) = self.connection_for_mut(header) else {
            return Some(header.reply(VsockOp::Reset));
        };
        connection.absorb_peer_credit(header);
        if matches!(connection.state, ConnectionState::Closed(_)) {
            return Some(header.reply(VsockOp::Reset));
        }
        if payload.len() > connection.receive.free() {
            // The peer sent past the window this end announced. There is
            // nowhere to put the bytes and no way to ask for them again.
            tracing::warn!(
                port = connection.local.port,
                bytes = payload.len(),
                free = connection.receive.free(),
                "vsock peer overran the announced receive window"
            );
            connection.state = ConnectionState::Closed(VsockError::ConnectionReset);
            return Some(header.reply(VsockOp::Reset));
        }
        connection.receive.push(payload);
        None
    }

    fn handle_credit_update(&mut self, header: &VsockPacketHeader) -> Option<VsockReply> {
        let connection = self.connection_for_mut(header)?;
        connection.absorb_peer_credit(header);
        None
    }

    fn handle_credit_request(&mut self, header: &VsockPacketHeader) -> Option<VsockReply> {
        let Some(connection) = self.connection_for_mut(header) else {
            return Some(header.reply(VsockOp::Reset));
        };
        connection.absorb_peer_credit(header);
        Some(connection.header(VsockOp::CreditUpdate))
    }

    fn open(
        &mut self,
        local: VsockAddress,
        peer: VsockAddress,
        state: ConnectionState,
    ) -> Result<VsockStreamId, VsockError> {
        if self.connection_index(local, peer).is_some() {
            return Err(VsockError::PortInUse { port: local.port });
        }
        let index = self
            .connections
            .iter()
            .position(Option::is_none)
            .ok_or(VsockError::ConnectionTableFull)?;
        let generation = self.take_generation();
        self.connections[index] = Some(VsockConnection {
            generation,
            local,
            peer,
            state,
            receive: ReceiveRing::new(VSOCK_RECEIVE_WINDOW_BYTES),
            fwd_cnt: 0,
            announced_fwd_cnt: 0,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            tx_cnt: 0,
            local_shutdown: VsockShutdown::none(),
            peer_shutdown: VsockShutdown::none(),
            transmitting: false,
            credit_requested: false,
        });
        Ok(VsockStreamId::new(index, generation))
    }

    fn connection_index(&self, local: VsockAddress, peer: VsockAddress) -> Option<usize> {
        self.connections.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|connection| connection.local == local && connection.peer == peer)
        })
    }

    /// Resolves the connection an arriving packet belongs to.
    fn connection_for_mut(&mut self, header: &VsockPacketHeader) -> Option<&mut VsockConnection> {
        let index = self.connection_index(header.destination, header.source)?;
        self.connections[index].as_mut()
    }

    fn connection(&self, stream: VsockStreamId) -> Result<&VsockConnection, VsockError> {
        self.connections
            .get(stream.index())
            .and_then(Option::as_ref)
            .filter(|connection| connection.generation == stream.generation())
            .ok_or(VsockError::UnknownHandle)
    }

    fn connection_mut(
        &mut self,
        stream: VsockStreamId,
    ) -> Result<&mut VsockConnection, VsockError> {
        self.connections
            .get_mut(stream.index())
            .and_then(Option::as_mut)
            .filter(|connection| connection.generation == stream.generation())
            .ok_or(VsockError::UnknownHandle)
    }

    fn listener(&self, listener: VsockListenerId) -> Result<&VsockListener, VsockError> {
        self.listeners
            .get(listener.index())
            .and_then(Option::as_ref)
            .filter(|bound| bound.generation == listener.generation())
            .ok_or(VsockError::UnknownHandle)
    }

    fn listener_mut(
        &mut self,
        listener: VsockListenerId,
    ) -> Result<&mut VsockListener, VsockError> {
        self.listeners
            .get_mut(listener.index())
            .and_then(Option::as_mut)
            .filter(|bound| bound.generation == listener.generation())
            .ok_or(VsockError::UnknownHandle)
    }

    fn listener_index_for_port(&self, port: u32) -> Option<usize> {
        self.listeners
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|listener| listener.port == port))
    }

    fn port_is_bound(&self, port: u32) -> bool {
        self.listener_index_for_port(port).is_some()
            || self.connections.iter().any(|slot| {
                slot.as_ref()
                    .is_some_and(|connection| connection.local.port == port)
            })
    }

    fn allocate_ephemeral_port(&mut self) -> Result<u32, VsockError> {
        for _ in 0..(u32::MAX - EPHEMERAL_PORT_START) {
            let port = self.next_ephemeral_port;
            self.next_ephemeral_port = if port == u32::MAX {
                EPHEMERAL_PORT_START
            } else {
                port + 1
            };
            if !self.port_is_bound(port) {
                return Ok(port);
            }
        }
        Err(VsockError::NoEphemeralPort)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_VSOCK_CONNECTIONS, VSOCK_RECEIVE_WINDOW_BYTES, VsockError, VsockReadProgress,
        VsockTable, VsockWriteProgress,
    };
    use alloc::vec;
    use helios_hal::vsock::{VsockAddress, VsockOp, VsockPacketHeader, VsockShutdown};

    const GUEST_CID: u64 = 3;
    const HOST_PORT: u32 = 1024;
    const SERVICE_PORT: u32 = 9000;
    /// A payload ceiling a fake device might report; small enough that
    /// the tests can watch a write be split by it.
    const MAX_PAYLOAD: usize = 64;

    fn table() -> VsockTable {
        VsockTable::new(GUEST_CID, MAX_PAYLOAD)
    }

    fn host() -> VsockAddress {
        VsockAddress::host(HOST_PORT)
    }

    fn guest(port: u32) -> VsockAddress {
        VsockAddress::new(GUEST_CID, port)
    }

    /// A packet the host sent to `destination`.
    fn from_host(
        op: VsockOp,
        destination: VsockAddress,
        payload_len: u32,
        buf_alloc: u32,
        fwd_cnt: u32,
    ) -> VsockPacketHeader {
        VsockPacketHeader {
            source: host(),
            destination,
            op,
            flags: 0,
            payload_len,
            buf_alloc,
            fwd_cnt,
        }
    }

    /// Brings a listener up and drives an inbound handshake to
    /// completion, returning the accepted connection.
    fn accepted(table: &mut VsockTable, peer_window: u32) -> super::VsockStreamId {
        let listener = table
            .listen(SERVICE_PORT, 4)
            .expect("the service port is free");
        let response = table
            .handle_packet(
                &from_host(VsockOp::Request, guest(SERVICE_PORT), 0, peer_window, 0),
                &[],
            )
            .expect("a request on a bound port is answered");
        assert_eq!(response.op, VsockOp::Response);
        assert_eq!(response.source, guest(SERVICE_PORT));
        assert_eq!(response.destination, host());
        table
            .accept(listener)
            .expect("the listener is live")
            .expect("the handshake queued a connection")
    }

    #[test]
    fn a_request_on_an_unbound_port_is_reset() {
        let mut table = table();
        let reply = table
            .handle_packet(&from_host(VsockOp::Request, guest(4242), 0, 4096, 0), &[])
            .expect("an unbound port answers");
        assert_eq!(reply.op, VsockOp::Reset);
        assert_eq!(reply.destination, host());
    }

    #[test]
    fn a_packet_addressed_to_another_context_id_is_reset() {
        let mut table = table();
        table.listen(SERVICE_PORT, 4).expect("the port is free");
        let reply = table
            .handle_packet(
                &from_host(
                    VsockOp::Request,
                    VsockAddress::new(GUEST_CID + 1, SERVICE_PORT),
                    0,
                    4096,
                    0,
                ),
                &[],
            )
            .expect("a misaddressed packet answers");
        assert_eq!(reply.op, VsockOp::Reset);
    }

    #[test]
    fn an_inbound_handshake_produces_an_acceptable_connection() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        assert_eq!(table.peer(stream), Ok(host()));
        assert_eq!(table.connect_progress(stream), Ok(true));
    }

    #[test]
    fn a_listener_refuses_once_its_backlog_is_full() {
        let mut table = table();
        table.listen(SERVICE_PORT, 1).expect("the port is free");
        let first = table.handle_packet(
            &from_host(VsockOp::Request, guest(SERVICE_PORT), 0, 4096, 0),
            &[],
        );
        assert_eq!(
            first.expect("the first request is answered").op,
            VsockOp::Response
        );
        // A second request from a different peer port finds the backlog
        // full, and a full backlog refuses rather than queues.
        let mut second_request = from_host(VsockOp::Request, guest(SERVICE_PORT), 0, 4096, 0);
        second_request.source = VsockAddress::host(HOST_PORT + 1);
        let second = table
            .handle_packet(&second_request, &[])
            .expect("the second request is answered");
        assert_eq!(second.op, VsockOp::Reset);
    }

    #[test]
    fn an_outbound_handshake_completes_on_the_peers_response() {
        let mut table = table();
        let (stream, request) = table.connect(host()).expect("a connection can be opened");
        assert_eq!(request.op, VsockOp::Request);
        assert_eq!(request.destination, host());
        assert_eq!(request.source.cid, GUEST_CID);
        assert_eq!(table.connect_progress(stream), Ok(false));

        let local = request.source;
        assert_eq!(
            table.handle_packet(&from_host(VsockOp::Response, local, 0, 8192, 0), &[]),
            None,
            "a response needs no answer"
        );
        assert_eq!(table.connect_progress(stream), Ok(true));
    }

    #[test]
    fn a_reset_during_the_handshake_is_a_refusal() {
        let mut table = table();
        let (stream, request) = table.connect(host()).expect("a connection can be opened");
        table.handle_packet(&from_host(VsockOp::Reset, request.source, 0, 0, 0), &[]);
        assert_eq!(
            table.connect_progress(stream),
            Err(VsockError::ConnectionRefused)
        );
    }

    #[test]
    fn a_reset_on_an_open_connection_is_reported_as_a_reset() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        table.handle_packet(
            &from_host(VsockOp::Reset, guest(SERVICE_PORT), 0, 0, 0),
            &[],
        );
        let mut out = [0_u8; 8];
        assert_eq!(
            table.read(stream, &mut out),
            Err(VsockError::ConnectionReset)
        );
    }

    #[test]
    fn received_bytes_are_handed_to_the_reader_in_order() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        table.handle_packet(
            &from_host(VsockOp::Data, guest(SERVICE_PORT), 5, 4096, 0),
            b"hello",
        );
        table.handle_packet(
            &from_host(VsockOp::Data, guest(SERVICE_PORT), 6, 4096, 0),
            b" world",
        );

        let mut out = [0_u8; 32];
        let progress = table
            .read(stream, &mut out)
            .expect("the connection is open");
        let VsockReadProgress::Ready { len, credit_update } = progress else {
            panic!("bytes have arrived: {progress:?}");
        };
        assert_eq!(&out[..len], b"hello world");
        assert_eq!(
            credit_update, None,
            "eleven bytes out of a 64 KiB window is not worth a packet"
        );
    }

    #[test]
    fn a_peer_that_overruns_the_announced_window_is_reset() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        let payload = vec![0_u8; VSOCK_RECEIVE_WINDOW_BYTES + 1];
        let reply = table
            .handle_packet(
                &from_host(
                    VsockOp::Data,
                    guest(SERVICE_PORT),
                    payload.len() as u32,
                    4096,
                    0,
                ),
                &payload,
            )
            .expect("an overrun is answered");
        assert_eq!(reply.op, VsockOp::Reset);
        let mut out = [0_u8; 8];
        assert_eq!(
            table.read(stream, &mut out),
            Err(VsockError::ConnectionReset)
        );
    }

    #[test]
    fn draining_half_the_window_announces_the_freed_credit() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        let half = VSOCK_RECEIVE_WINDOW_BYTES / 2;
        let payload = vec![7_u8; half];
        table.handle_packet(
            &from_host(VsockOp::Data, guest(SERVICE_PORT), half as u32, 4096, 0),
            &payload,
        );

        let mut out = vec![0_u8; half];
        let progress = table
            .read(stream, &mut out)
            .expect("the connection is open");
        let VsockReadProgress::Ready { len, credit_update } = progress else {
            panic!("bytes have arrived: {progress:?}");
        };
        assert_eq!(len, half);
        let update = credit_update.expect("half a window is worth announcing");
        assert_eq!(update.op, VsockOp::CreditUpdate);
        assert_eq!(update.fwd_cnt, half as u32);
        assert_eq!(update.buf_alloc, VSOCK_RECEIVE_WINDOW_BYTES as u32);
    }

    #[test]
    fn a_write_is_bounded_by_the_peers_window_and_by_one_packet() {
        let mut table = table();
        // The peer announced a window narrower than one packet, so the
        // first chunk is the window and not the payload ceiling.
        let stream = accepted(&mut table, 10);
        let progress = table.begin_write(stream, 1000).expect("the stream is open");
        let VsockWriteProgress::Ready(chunk) = progress else {
            panic!("credit is available: {progress:?}");
        };
        assert_eq!(chunk.len, 10);
        assert_eq!(chunk.header.op, VsockOp::Data);
        assert_eq!(chunk.header.payload_len, 10);
        table.finish_write(stream, chunk.len);

        // The window is now spent; nothing more may go out until the
        // peer says it consumed something.
        assert_eq!(
            table.begin_write(stream, 1000),
            Ok(VsockWriteProgress::Blocked)
        );

        // A credit update reopens exactly what the peer consumed.
        table.handle_packet(
            &from_host(VsockOp::CreditUpdate, guest(SERVICE_PORT), 0, 10, 4),
            &[],
        );
        let progress = table.begin_write(stream, 1000).expect("the stream is open");
        let VsockWriteProgress::Ready(chunk) = progress else {
            panic!("four bytes of credit came back: {progress:?}");
        };
        assert_eq!(chunk.len, 4);
    }

    #[test]
    fn a_write_larger_than_one_packet_is_split_at_the_payload_ceiling() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        let progress = table.begin_write(stream, 4096).expect("the stream is open");
        let VsockWriteProgress::Ready(chunk) = progress else {
            panic!("credit is available: {progress:?}");
        };
        assert_eq!(
            chunk.len, MAX_PAYLOAD,
            "one packet never carries more than the device does"
        );
    }

    #[test]
    fn a_failed_transmission_does_not_spend_credit() {
        let mut table = table();
        let stream = accepted(&mut table, 32);
        let progress = table.begin_write(stream, 32).expect("the stream is open");
        let VsockWriteProgress::Ready(chunk) = progress else {
            panic!("credit is available: {progress:?}");
        };
        assert_eq!(chunk.len, 32);
        table.finish_write(stream, 0);

        let progress = table.begin_write(stream, 32).expect("the stream is open");
        let VsockWriteProgress::Ready(retry) = progress else {
            panic!("no credit was spent: {progress:?}");
        };
        assert_eq!(retry.len, 32);
    }

    #[test]
    fn a_second_writer_waits_for_the_first_to_finish_its_packet() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        let progress = table.begin_write(stream, 8).expect("the stream is open");
        assert!(matches!(progress, VsockWriteProgress::Ready(_)));
        assert_eq!(
            table.begin_write(stream, 8),
            Ok(VsockWriteProgress::Blocked),
            "one writer at a time owns the byte stream"
        );
        table.finish_write(stream, 8);
        assert!(matches!(
            table.begin_write(stream, 8),
            Ok(VsockWriteProgress::Ready(_))
        ));
    }

    #[test]
    fn a_stalled_writer_asks_the_peer_to_re_announce_its_window_once() {
        let mut table = table();
        let stream = accepted(&mut table, 0);
        assert_eq!(
            table.begin_write(stream, 8),
            Ok(VsockWriteProgress::Blocked)
        );
        let request = table
            .credit_request(stream)
            .expect("the stream is open")
            .expect("a stalled connection asks");
        assert_eq!(request.op, VsockOp::CreditRequest);
        assert_eq!(
            table.credit_request(stream),
            Ok(None),
            "a second ask while one is outstanding would be a packet per poll"
        );

        table.handle_packet(
            &from_host(VsockOp::CreditUpdate, guest(SERVICE_PORT), 0, 16, 0),
            &[],
        );
        assert!(matches!(
            table.begin_write(stream, 8),
            Ok(VsockWriteProgress::Ready(_))
        ));
    }

    #[test]
    fn a_credit_request_from_the_peer_is_answered_with_this_ends_window() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        let payload = vec![1_u8; 100];
        table.handle_packet(
            &from_host(VsockOp::Data, guest(SERVICE_PORT), 100, 4096, 0),
            &payload,
        );
        let mut out = [0_u8; 100];
        table.read(stream, &mut out).expect("bytes arrived");

        let update = table
            .handle_packet(
                &from_host(VsockOp::CreditRequest, guest(SERVICE_PORT), 0, 4096, 0),
                &[],
            )
            .expect("a credit request is answered");
        assert_eq!(update.op, VsockOp::CreditUpdate);
        assert_eq!(update.buf_alloc, VSOCK_RECEIVE_WINDOW_BYTES as u32);
        assert_eq!(update.fwd_cnt, 100);
    }

    #[test]
    fn a_peer_that_closes_its_transmit_side_ends_the_readers_stream() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        table.handle_packet(
            &from_host(VsockOp::Data, guest(SERVICE_PORT), 3, 4096, 0),
            b"bye",
        );
        let mut shutdown = from_host(VsockOp::Shutdown, guest(SERVICE_PORT), 0, 4096, 0);
        shutdown.flags = VsockShutdown {
            receive: false,
            send: true,
        }
        .as_flags();
        assert_eq!(
            table.handle_packet(&shutdown, &[]),
            None,
            "half a shutdown does not end the connection"
        );

        // Buffered bytes still reach the reader; only a drained buffer
        // is an end of file.
        let mut out = [0_u8; 8];
        let progress = table
            .read(stream, &mut out)
            .expect("the connection is open");
        let VsockReadProgress::Ready { len, .. } = progress else {
            panic!("the buffered bytes are still there: {progress:?}");
        };
        assert_eq!(&out[..len], b"bye");
        assert_eq!(table.read(stream, &mut out), Ok(VsockReadProgress::Eof));
    }

    #[test]
    fn a_shutdown_of_both_directions_closes_the_connection() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        let mut shutdown = from_host(VsockOp::Shutdown, guest(SERVICE_PORT), 0, 4096, 0);
        shutdown.flags = VsockShutdown::both().as_flags();
        let reply = table
            .handle_packet(&shutdown, &[])
            .expect("a full shutdown is answered");
        assert_eq!(reply.op, VsockOp::Reset);

        let mut out = [0_u8; 8];
        assert_eq!(table.read(stream, &mut out), Ok(VsockReadProgress::Eof));
        assert_eq!(table.begin_write(stream, 8), Err(VsockError::Closed));
    }

    #[test]
    fn this_ends_shutdown_is_announced_once_per_direction() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        let announcement = table
            .shutdown(
                stream,
                VsockShutdown {
                    receive: false,
                    send: true,
                },
            )
            .expect("the stream is open")
            .expect("a new direction is announced");
        assert_eq!(announcement.op, VsockOp::Shutdown);
        assert_eq!(
            announcement.shutdown(),
            VsockShutdown {
                receive: false,
                send: true
            }
        );
        assert_eq!(
            table.shutdown(
                stream,
                VsockShutdown {
                    receive: false,
                    send: true
                }
            ),
            Ok(None),
            "re-announcing a direction already closed puts nothing on the wire"
        );
        assert_eq!(table.begin_write(stream, 8), Err(VsockError::Closed));
    }

    #[test]
    fn closing_a_connection_resets_the_peer_and_retires_the_handle() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        let reset = table
            .close(stream)
            .expect("the stream is open")
            .expect("a live connection is reset on close");
        assert_eq!(reset.op, VsockOp::Reset);
        assert_eq!(reset.destination, host());
        assert_eq!(table.peer(stream), Err(VsockError::UnknownHandle));
    }

    #[test]
    fn a_reused_slot_does_not_answer_the_previous_connections_handle() {
        let mut table = table();
        let listener = table
            .listen(SERVICE_PORT, 4)
            .expect("the service port is free");
        fn handshake(
            table: &mut VsockTable,
            listener: super::VsockListenerId,
            host_port: u32,
        ) -> super::VsockStreamId {
            let mut request = from_host(VsockOp::Request, guest(SERVICE_PORT), 0, 4096, 0);
            request.source = VsockAddress::host(host_port);
            table
                .handle_packet(&request, &[])
                .expect("a bound port answers");
            table
                .accept(listener)
                .expect("the listener is live")
                .expect("the handshake queued a connection")
        }

        let stale = handshake(&mut table, listener, HOST_PORT);
        table.close(stale).expect("the stream is open");
        // The freed slot is the first one the next connection takes, so
        // this is exactly the case a bare index would confuse.
        let fresh = handshake(&mut table, listener, HOST_PORT + 1);
        assert_ne!(stale, fresh);
        assert_eq!(table.peer(stale), Err(VsockError::UnknownHandle));
        assert_eq!(table.peer(fresh), Ok(VsockAddress::host(HOST_PORT + 1)));
    }

    #[test]
    fn binding_a_port_twice_is_refused() {
        let mut table = table();
        table.listen(SERVICE_PORT, 4).expect("the port is free");
        assert_eq!(
            table.listen(SERVICE_PORT, 4),
            Err(VsockError::PortInUse { port: SERVICE_PORT })
        );
    }

    #[test]
    fn outbound_connections_take_distinct_ephemeral_ports() {
        let mut table = table();
        let (_, first) = table.connect(host()).expect("a connection can be opened");
        let (_, second) = table
            .connect(VsockAddress::host(HOST_PORT + 1))
            .expect("a second connection can be opened");
        assert_ne!(first.source.port, second.source.port);
    }

    #[test]
    fn a_full_connection_table_refuses_rather_than_overwrites() {
        let mut table = table();
        for _ in 0..MAX_VSOCK_CONNECTIONS {
            table.connect(host()).expect("the table has room");
        }
        assert_eq!(
            table.connect(host()).map(|(stream, _)| stream),
            Err(VsockError::ConnectionTableFull)
        );
    }

    #[test]
    fn a_transport_reset_closes_every_connection() {
        let mut table = table();
        let stream = accepted(&mut table, 4096);
        table.reset_all();
        let mut out = [0_u8; 8];
        assert_eq!(
            table.read(stream, &mut out),
            Err(VsockError::ConnectionReset)
        );
    }
}
