//! Receive-side scaling hash.
//!
//! One hash decides which processor owns a flow, and it has to be the
//! same number on both sides of the wire: the device programs its
//! indirection table from it to pick a receive queue, and the kernel
//! demuxes a frame to a shard with it whether or not the device
//! reported one. That is only true if the driver and the device compute
//! the *same* function over the *same* bytes, which is why this is the
//! standard Toeplitz hash over the standard 40-byte key rather than
//! anything cheaper.
//!
//! # SMP contract
//!
//! Everything here is a pure function of its arguments. No state, no
//! locks, no allocation; safe to call on every processor concurrently
//! and from interrupt context.

use crate::packet::{
    EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Packet, Ipv6Packet, TcpPacket, UdpPacket,
};
use crate::types::{IpAddress, Ipv4Address, Ipv6Address};

/// Bytes in the RSS hash key.
///
/// 40 is what `VIRTIO_NET_F_RSS` reports as the key length for a
/// Toeplitz device and what every RSS implementation the kernel talks
/// to expects.
pub const RSS_KEY_BYTES: usize = 40;

/// Entries in the receive-side scaling indirection table.
///
/// A device masks the hash down to a slot in this table and reads the
/// receive queue out of it, so the length must be a power of two. Fixing
/// it here rather than taking whatever a device happens to allow is what
/// lets the driver reproduce the device's decision exactly: both index
/// the same table the same way, for any shard count.
pub const RSS_INDIRECTION_ENTRIES: usize = 128;

/// The queue an indirection-table slot points at.
///
/// The table is not stored anywhere: it is this function, evaluated
/// per slot when the device is programmed and per frame when the
/// software path has to reach the same answer.
pub const fn rss_indirection_entry(slot: usize, buckets: usize) -> usize {
    assert!(
        buckets != 0,
        "an indirection table needs at least one queue"
    );
    slot % buckets
}

/// Largest tuple the hash is computed over: two IPv6 addresses plus two
/// ports.
const MAX_TUPLE_BYTES: usize = 16 + 16 + 2 + 2;

/// The symmetric-free "standard" Toeplitz key published with the
/// Microsoft RSS specification.
///
/// Devices default to it and helios programs it explicitly, so a frame
/// the device hashed and a frame the kernel hashed land on the same
/// shard.
pub const STANDARD_RSS_KEY: [u8; RSS_KEY_BYTES] = [
    0x6d, 0x5a, 0x56, 0xda, 0x25, 0x5b, 0x0e, 0xc2, 0x41, 0x67, 0x25, 0x3d, 0x43, 0xa3, 0x8f, 0xb0,
    0xd0, 0xca, 0x2b, 0xcb, 0xae, 0x7b, 0x30, 0xb4, 0x77, 0xcb, 0x2d, 0xa3, 0x80, 0x30, 0xf2, 0x0c,
    0x6a, 0x42, 0xb7, 0x3b, 0xbe, 0xac, 0x01, 0xfa,
];

/// The flow a frame belongs to, as the bytes RSS hashes.
///
/// Built from a parsed frame or from the endpoints of a socket the
/// kernel is about to open, so placement at open time and demux at
/// receive time cannot disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlowTuple {
    bytes: [u8; MAX_TUPLE_BYTES],
    len: usize,
}

/// A Toeplitz hash value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FlowHash(u32);

impl FlowHash {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    /// The receive queue, and therefore the shard, this flow belongs to.
    ///
    /// This is the one placement rule, and it is deliberately the
    /// two-step mapping a device performs rather than a plain
    /// `hash % buckets`: an RSS engine can only *mask* the hash down to
    /// an indirection-table slot and then read a queue out of the
    /// table, so a driver that divided instead would disagree with it
    /// for every bucket count that is not a power of two. Masking here
    /// too means a device that steers and a device that cannot land the
    /// same flow on the same shard.
    pub const fn bucket(self, buckets: usize) -> usize {
        rss_indirection_entry(self.slot(), buckets)
    }

    /// The indirection-table slot this flow masks down to.
    pub const fn slot(self) -> usize {
        (self.0 as usize) & (RSS_INDIRECTION_ENTRIES - 1)
    }
}

impl FlowTuple {
    /// The tuple for an IPv4 flow.
    pub fn ipv4(
        source: Ipv4Address,
        source_port: u16,
        destination: Ipv4Address,
        destination_port: u16,
    ) -> Self {
        let mut bytes = [0; MAX_TUPLE_BYTES];
        bytes[..4].copy_from_slice(&source.octets());
        bytes[4..8].copy_from_slice(&destination.octets());
        bytes[8..10].copy_from_slice(&source_port.to_be_bytes());
        bytes[10..12].copy_from_slice(&destination_port.to_be_bytes());
        Self { bytes, len: 12 }
    }

    /// The tuple for an IPv6 flow.
    pub fn ipv6(
        source: Ipv6Address,
        source_port: u16,
        destination: Ipv6Address,
        destination_port: u16,
    ) -> Self {
        let mut bytes = [0; MAX_TUPLE_BYTES];
        bytes[..16].copy_from_slice(&source.octets());
        bytes[16..32].copy_from_slice(&destination.octets());
        bytes[32..34].copy_from_slice(&source_port.to_be_bytes());
        bytes[34..36].copy_from_slice(&destination_port.to_be_bytes());
        Self { bytes, len: 36 }
    }

    /// The tuple for a flow between two [`IpAddress`] endpoints.
    ///
    /// A mixed-family pair cannot describe a flow, so it has no tuple.
    pub fn between(
        source: IpAddress,
        source_port: u16,
        destination: IpAddress,
        destination_port: u16,
    ) -> Option<Self> {
        match (source, destination) {
            (IpAddress::Ipv4(source), IpAddress::Ipv4(destination)) => Some(Self::ipv4(
                source,
                source_port,
                destination,
                destination_port,
            )),
            (IpAddress::Ipv6(source), IpAddress::Ipv6(destination)) => Some(Self::ipv6(
                source,
                source_port,
                destination,
                destination_port,
            )),
            _ => None,
        }
    }

    /// The tuple an Ethernet frame carries, if it carries one.
    ///
    /// Only TCP and UDP over IP have a four-tuple. ARP, ICMP and
    /// anything unparseable do not, and the caller places them on the
    /// default shard.
    pub fn from_frame(frame: &[u8]) -> Option<Self> {
        let ethernet = EthernetFrame::parse(frame)?;
        match ethernet.protocol {
            EthernetProtocol::Ipv4 => {
                let packet = Ipv4Packet::parse(ethernet.payload)?;
                let (source_port, destination_port) =
                    transport_ports(packet.protocol, packet.payload)?;
                Some(Self::ipv4(
                    packet.source,
                    source_port,
                    packet.destination,
                    destination_port,
                ))
            }
            EthernetProtocol::Ipv6 => {
                let packet = Ipv6Packet::parse(ethernet.payload)?;
                let (source_port, destination_port) =
                    transport_ports(packet.next_header, packet.payload)?;
                Some(Self::ipv6(
                    packet.source,
                    source_port,
                    packet.destination,
                    destination_port,
                ))
            }
            _ => None,
        }
    }

    /// The same flow seen from the other end.
    ///
    /// The standard Toeplitz key is not symmetric, so the hash of a
    /// flow's outgoing direction differs from its incoming one. A
    /// socket is placed by the hash of the frames it will *receive*,
    /// which is this.
    pub fn reversed(self) -> Self {
        let mut reversed = self;
        match self.len {
            12 => {
                reversed.bytes[..4].copy_from_slice(&self.bytes[4..8]);
                reversed.bytes[4..8].copy_from_slice(&self.bytes[..4]);
                reversed.bytes[8..10].copy_from_slice(&self.bytes[10..12]);
                reversed.bytes[10..12].copy_from_slice(&self.bytes[8..10]);
            }
            36 => {
                reversed.bytes[..16].copy_from_slice(&self.bytes[16..32]);
                reversed.bytes[16..32].copy_from_slice(&self.bytes[..16]);
                reversed.bytes[32..34].copy_from_slice(&self.bytes[34..36]);
                reversed.bytes[34..36].copy_from_slice(&self.bytes[32..34]);
            }
            len => panic!("flow tuple of {len} bytes is neither IPv4 nor IPv6"),
        }
        reversed
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

fn transport_ports(protocol: IpProtocol, payload: &[u8]) -> Option<(u16, u16)> {
    match protocol {
        IpProtocol::Tcp => {
            let packet = TcpPacket::parse(payload)?;
            Some((packet.source_port, packet.destination_port))
        }
        IpProtocol::Udp => {
            let packet = UdpPacket::parse(payload)?;
            Some((packet.source_port, packet.destination_port))
        }
        IpProtocol::Icmp | IpProtocol::Icmpv6 => None,
    }
}

/// The Toeplitz hash of `tuple` under `key`.
///
/// The classic formulation shifts a 32-bit window across the key one
/// bit per input bit and XORs it in wherever the input bit is set. The
/// window is kept in a `u64` so one shift per input byte is all the
/// key access costs.
pub fn toeplitz(key: &[u8; RSS_KEY_BYTES], tuple: &FlowTuple) -> FlowHash {
    let bytes = tuple.as_bytes();
    debug_assert!(
        bytes.len() < RSS_KEY_BYTES,
        "the key must outlast the tuple by at least the 32-bit window"
    );
    let mut result = 0u32;
    // The window holds the next 32 key bits in its high half and the
    // following 32 in its low half, so a byte of input consumes eight
    // single-bit shifts without touching the key again.
    let mut window = u64::from(u32::from_be_bytes([key[0], key[1], key[2], key[3]])) << 32;
    for (index, byte) in bytes.iter().enumerate() {
        let next = key.get(index + 4).copied().unwrap_or(0);
        window |= u64::from(next) << 24;
        let mut byte = *byte;
        for _ in 0..8 {
            if byte & 0x80 != 0 {
                result ^= (window >> 32) as u32;
            }
            byte <<= 1;
            window <<= 1;
        }
    }
    FlowHash(result)
}

/// The hash of a flow under the standard key.
pub fn flow_hash(tuple: &FlowTuple) -> FlowHash {
    toeplitz(&STANDARD_RSS_KEY, tuple)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published Microsoft RSS verification vectors. Both the
    /// two-tuple (addresses only) and four-tuple (addresses and ports)
    /// forms are checked because a device that reports a hash for a
    /// non-TCP/UDP frame computes the former.
    /// One published vector: the two endpoints and the hash they must
    /// produce.
    struct Ipv4Vector {
        source: [u8; 4],
        source_port: u16,
        destination: [u8; 4],
        destination_port: u16,
        hash: u32,
    }

    #[test]
    fn ipv4_vectors_match_the_rss_verification_suite() {
        let cases = [
            Ipv4Vector {
                source: [66, 9, 149, 187],
                source_port: 2794,
                destination: [161, 142, 100, 80],
                destination_port: 1766,
                hash: 0x51cc_c178,
            },
            Ipv4Vector {
                source: [199, 92, 111, 2],
                source_port: 14230,
                destination: [65, 69, 140, 83],
                destination_port: 4739,
                hash: 0xc626_b0ea,
            },
            Ipv4Vector {
                source: [24, 19, 198, 95],
                source_port: 29943,
                destination: [12, 22, 207, 184],
                destination_port: 38024,
                hash: 0x83fd_4533,
            },
            Ipv4Vector {
                source: [38, 27, 205, 30],
                source_port: 40397,
                destination: [209, 142, 163, 6],
                destination_port: 32794,
                hash: 0xa735_e9ee,
            },
            Ipv4Vector {
                source: [153, 39, 163, 191],
                source_port: 8298,
                destination: [202, 188, 127, 2],
                destination_port: 26332,
                hash: 0x56a6_32d0,
            },
        ];
        for case in cases {
            let tuple = FlowTuple::ipv4(
                Ipv4Address::new(case.source),
                case.source_port,
                Ipv4Address::new(case.destination),
                case.destination_port,
            );
            assert_eq!(flow_hash(&tuple).get(), case.hash);
        }
    }

    #[test]
    fn ipv6_vectors_match_the_rss_verification_suite() {
        let source = Ipv6Address::new([
            0x3f, 0xfe, 0x25, 0x01, 0x02, 0x00, 0x1f, 0xff, 0, 0, 0, 0, 0, 0, 0, 0x07,
        ]);
        let destination = Ipv6Address::new([
            0x3f, 0xfe, 0x25, 0x01, 0x02, 0x00, 0x00, 0x03, 0, 0, 0, 0, 0, 0, 0, 0x01,
        ]);
        let tuple = FlowTuple::ipv6(source, 2794, destination, 1766);
        assert_eq!(flow_hash(&tuple).get(), 0x4020_7d3d);
    }

    /// The bucket rule has to be exactly what a device computes:
    /// mask to a table slot, then read the queue out of the table.
    #[test]
    fn a_bucket_is_the_indirection_table_entry_for_the_masked_hash() {
        assert!(RSS_INDIRECTION_ENTRIES.is_power_of_two());
        for raw in [0u32, 1, 127, 128, 129, 0xffff_ffff, 0x8000_0001] {
            let hash = FlowHash::new(raw);
            assert_eq!(hash.slot(), (raw as usize) % RSS_INDIRECTION_ENTRIES);
            for buckets in [1usize, 2, 3, 4, 5, 8, 12] {
                assert_eq!(
                    hash.bucket(buckets),
                    rss_indirection_entry(hash.slot(), buckets)
                );
                assert!(hash.bucket(buckets) < buckets);
            }
        }
    }

    /// Every slot of the table is reachable, so no shard is starved of
    /// the flows that hash to it.
    #[test]
    fn every_bucket_is_reachable_from_some_slot() {
        for buckets in [1usize, 2, 3, 4, 8] {
            for bucket in 0..buckets {
                assert!(
                    (0..RSS_INDIRECTION_ENTRIES)
                        .any(|slot| rss_indirection_entry(slot, buckets) == bucket)
                );
            }
        }
    }

    #[test]
    fn reversing_a_tuple_swaps_both_endpoints() {
        let tuple = FlowTuple::ipv4(
            Ipv4Address::new([192, 0, 2, 1]),
            1234,
            Ipv4Address::new([198, 51, 100, 2]),
            80,
        );
        let reversed = FlowTuple::ipv4(
            Ipv4Address::new([198, 51, 100, 2]),
            80,
            Ipv4Address::new([192, 0, 2, 1]),
            1234,
        );
        assert_eq!(tuple.reversed(), reversed);
        assert_eq!(reversed.reversed(), tuple);
    }
}
