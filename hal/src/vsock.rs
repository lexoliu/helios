//! The AF_VSOCK transport contract.
//!
//! vsock is the hypervisor-provided socket family that lets a guest talk
//! to its host without a network: endpoints are `(context id, port)`
//! pairs, the link is point-to-point, and the transport itself carries
//! the connection handshake and the credit-based flow control rather
//! than leaving them to a protocol stack. That contract is defined
//! independently of the device that implements it — virtio-vsock is one
//! transport, VMCI and Hyper-V are others — so the value types and the
//! device trait live here, next to the other hardware contracts, and the
//! concrete driver in the virtio crate encodes them onto its wire
//! format.
//!
//! Concurrency contract: a [`VsockDevice`] is shared. `send` and
//! `receive_into` may be called concurrently from different tasks and
//! from different processors; the implementation serialises access to
//! its rings internally and never blocks the caller's executor.

use core::future::Future;

use crate::io::IoResult;

/// Context id of the host end of every vsock link (virtio 1.2 §5.10.4).
///
/// `0` and `1` are reserved for the hypervisor and for a
/// no-longer-used loopback address, so a guest's own id is always
/// greater than this one.
pub const VSOCK_HOST_CID: u64 = 2;

/// Smallest context id a guest may be given.
///
/// The three ids below it are reserved: `0` for the hypervisor, `1` for
/// the retired loopback address, and `2` for the host itself.
pub const VSOCK_FIRST_GUEST_CID: u64 = 3;

/// One end of a vsock connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VsockAddress {
    /// The context id naming the virtual machine this end lives in.
    pub cid: u64,
    /// The port within that machine.
    pub port: u32,
}

impl VsockAddress {
    pub const fn new(cid: u64, port: u32) -> Self {
        Self { cid, port }
    }

    /// The address of `port` on the host.
    pub const fn host(port: u32) -> Self {
        Self::new(VSOCK_HOST_CID, port)
    }
}

/// What a vsock packet asks its peer to do.
///
/// The operation set is closed: a packet carrying anything else is a
/// protocol violation the receiver answers with [`VsockOp::Reset`],
/// which is why decoding returns an option rather than a catch-all
/// variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VsockOp {
    /// Open a connection to the destination address.
    Request,
    /// Accept a connection the peer requested.
    Response,
    /// Refuse a request, or tear a connection down abruptly.
    Reset,
    /// Close one or both directions of a connection.
    Shutdown,
    /// Carry payload bytes.
    Data,
    /// Report the sender's receive window.
    CreditUpdate,
    /// Ask the peer to report its receive window.
    CreditRequest,
}

impl VsockOp {
    /// The on-the-wire operation number.
    pub const fn as_id(self) -> u16 {
        match self {
            Self::Request => 1,
            Self::Response => 2,
            Self::Reset => 3,
            Self::Shutdown => 4,
            Self::Data => 5,
            Self::CreditUpdate => 6,
            Self::CreditRequest => 7,
        }
    }

    /// Decodes an on-the-wire operation number. `None` for the reserved
    /// `0` and for anything the specification does not define.
    pub const fn from_id(id: u16) -> Option<Self> {
        match id {
            1 => Some(Self::Request),
            2 => Some(Self::Response),
            3 => Some(Self::Reset),
            4 => Some(Self::Shutdown),
            5 => Some(Self::Data),
            6 => Some(Self::CreditUpdate),
            7 => Some(Self::CreditRequest),
            _ => None,
        }
    }
}

/// Which directions of a connection a [`VsockOp::Shutdown`] closes.
///
/// The two flags are independent: a peer that will send no more bytes
/// but still reads its own receive queue announces `send` alone, and
/// only a shutdown of both directions ends the connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VsockShutdown {
    /// The sender will not read any more bytes.
    pub receive: bool,
    /// The sender will not write any more bytes.
    pub send: bool,
}

/// `VIRTIO_VSOCK_SHUTDOWN_F_RECEIVE`.
const SHUTDOWN_RECEIVE: u32 = 1 << 0;
/// `VIRTIO_VSOCK_SHUTDOWN_F_SEND`.
const SHUTDOWN_SEND: u32 = 1 << 1;

impl VsockShutdown {
    /// Neither direction closed.
    pub const fn none() -> Self {
        Self {
            receive: false,
            send: false,
        }
    }

    /// Both directions closed, which ends the connection.
    pub const fn both() -> Self {
        Self {
            receive: true,
            send: true,
        }
    }

    pub const fn from_flags(flags: u32) -> Self {
        Self {
            receive: flags & SHUTDOWN_RECEIVE != 0,
            send: flags & SHUTDOWN_SEND != 0,
        }
    }

    pub const fn as_flags(self) -> u32 {
        let receive = if self.receive { SHUTDOWN_RECEIVE } else { 0 };
        let send = if self.send { SHUTDOWN_SEND } else { 0 };
        receive | send
    }

    /// Whether this shutdown closes the connection outright.
    pub const fn is_full(self) -> bool {
        self.receive && self.send
    }

    /// Folds another peer announcement into this one. Shutdown is
    /// monotonic: a direction the peer has closed never reopens.
    pub const fn merged(self, other: Self) -> Self {
        Self {
            receive: self.receive || other.receive,
            send: self.send || other.send,
        }
    }
}

/// A vsock packet's header: everything about it except the payload.
///
/// `buf_alloc` and `fwd_cnt` ride on *every* packet, not just on credit
/// updates: they are the sender's own receive-window announcement, so a
/// data packet doubles as a credit update for the reverse direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VsockPacketHeader {
    pub source: VsockAddress,
    pub destination: VsockAddress,
    pub op: VsockOp,
    /// Operation-specific flags; the shutdown bits for
    /// [`VsockOp::Shutdown`] and zero for everything else.
    pub flags: u32,
    /// Payload bytes that follow this header.
    pub payload_len: u32,
    /// Size of the sender's own receive buffer.
    pub buf_alloc: u32,
    /// Bytes the sender has consumed from its receive buffer since the
    /// connection opened, counted modulo 2^32.
    pub fwd_cnt: u32,
}

impl VsockPacketHeader {
    /// The shutdown directions this packet announces. Meaningful only
    /// for [`VsockOp::Shutdown`]; every other operation leaves the flag
    /// word zero, which reads as "neither direction".
    pub const fn shutdown(&self) -> VsockShutdown {
        VsockShutdown::from_flags(self.flags)
    }

    /// The header of the reply that returns to this packet's sender.
    ///
    /// Reversing the addresses is what every answer has in common:
    /// responses, resets, credit updates and shutdowns all go back the
    /// way the packet came.
    pub const fn reply(&self, op: VsockOp) -> Self {
        Self {
            source: self.destination,
            destination: self.source,
            op,
            flags: 0,
            payload_len: 0,
            buf_alloc: 0,
            fwd_cnt: 0,
        }
    }
}

/// What one [`VsockDevice::receive_into`] delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VsockReceived {
    pub header: VsockPacketHeader,
    /// Payload bytes written into the caller's buffer. Always equal to
    /// the header's `payload_len`; a device that announces more than it
    /// delivered is a fault, not a short read.
    pub payload_len: usize,
}

/// A device that carries vsock packets between this machine and its
/// host.
///
/// The trait is deliberately packet-level rather than stream-level: the
/// connection table, port allocation and credit accounting are kernel
/// state that no device owns, and a transport that implemented them
/// would have to be reimplemented per device.
pub trait VsockDevice: Send + Sync {
    /// The context id the hypervisor assigned this machine.
    fn guest_cid(&self) -> u64;

    /// Largest payload the device carries in one packet, in both
    /// directions. Callers size their receive buffers by it and split
    /// larger writes across packets.
    fn max_payload_bytes(&self) -> usize;

    /// Sends one packet. `payload` must be no longer than
    /// [`VsockDevice::max_payload_bytes`] and must match the header's
    /// `payload_len`.
    fn send<'a>(
        &'a self,
        header: VsockPacketHeader,
        payload: &'a [u8],
    ) -> impl Future<Output = IoResult<()>> + Send + 'a;

    /// Receives the next packet, copying its payload into `payload`.
    ///
    /// The buffer must hold [`VsockDevice::max_payload_bytes`]; a packet
    /// that does not fit is a device fault rather than a short read,
    /// because vsock has no packet fragmentation for the caller to
    /// reassemble.
    fn receive_into<'a>(
        &'a self,
        payload: &'a mut [u8],
    ) -> impl Future<Output = IoResult<VsockReceived>> + Send + 'a;
}

#[cfg(test)]
mod tests {
    use super::{VsockAddress, VsockOp, VsockPacketHeader, VsockShutdown};

    #[test]
    fn every_defined_operation_round_trips_through_its_wire_number() {
        for op in [
            VsockOp::Request,
            VsockOp::Response,
            VsockOp::Reset,
            VsockOp::Shutdown,
            VsockOp::Data,
            VsockOp::CreditUpdate,
            VsockOp::CreditRequest,
        ] {
            assert_eq!(VsockOp::from_id(op.as_id()), Some(op));
        }
    }

    #[test]
    fn the_reserved_and_unknown_operation_numbers_do_not_decode() {
        assert_eq!(VsockOp::from_id(0), None);
        assert_eq!(VsockOp::from_id(8), None);
        assert_eq!(VsockOp::from_id(u16::MAX), None);
    }

    #[test]
    fn shutdown_directions_round_trip_through_their_flag_word() {
        for shutdown in [
            VsockShutdown::none(),
            VsockShutdown {
                receive: true,
                send: false,
            },
            VsockShutdown {
                receive: false,
                send: true,
            },
            VsockShutdown::both(),
        ] {
            assert_eq!(VsockShutdown::from_flags(shutdown.as_flags()), shutdown);
        }
        assert!(VsockShutdown::both().is_full());
        assert!(
            !VsockShutdown {
                receive: true,
                send: false
            }
            .is_full()
        );
    }

    #[test]
    fn a_shutdown_direction_never_reopens() {
        let announced = VsockShutdown {
            receive: false,
            send: true,
        };
        assert_eq!(
            announced.merged(VsockShutdown {
                receive: true,
                send: false
            }),
            VsockShutdown::both()
        );
        assert_eq!(announced.merged(VsockShutdown::none()), announced);
    }

    #[test]
    fn a_reply_goes_back_the_way_the_packet_came() {
        let packet = VsockPacketHeader {
            source: VsockAddress::new(2, 1024),
            destination: VsockAddress::new(3, 9000),
            op: VsockOp::Request,
            flags: 0,
            payload_len: 0,
            buf_alloc: 4096,
            fwd_cnt: 7,
        };
        let reply = packet.reply(VsockOp::Reset);
        assert_eq!(reply.source, packet.destination);
        assert_eq!(reply.destination, packet.source);
        assert_eq!(reply.op, VsockOp::Reset);
        assert_eq!(reply.payload_len, 0);
    }
}
