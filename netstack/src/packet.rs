use crate::{
    EthernetAddress, IpAddress, Ipv4Address, Ipv6Address, internet_checksum, ipv4_checksum,
    tcpv4_checksum, tcpv6_checksum, udp_checksum,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EthernetProtocol {
    Ipv4,
    Arp,
    Ipv6,
}

impl EthernetProtocol {
    pub const fn ether_type(self) -> u16 {
        match self {
            Self::Ipv4 => 0x0800,
            Self::Arp => 0x0806,
            Self::Ipv6 => 0x86dd,
        }
    }

    pub const fn from_ether_type(value: u16) -> Option<Self> {
        match value {
            0x0800 => Some(Self::Ipv4),
            0x0806 => Some(Self::Arp),
            0x86dd => Some(Self::Ipv6),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EthernetFrame<'a> {
    pub destination: EthernetAddress,
    pub source: EthernetAddress,
    pub protocol: EthernetProtocol,
    pub payload: &'a [u8],
}

impl<'a> EthernetFrame<'a> {
    pub const HEADER_LEN: usize = 14;

    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < Self::HEADER_LEN {
            return None;
        }
        let protocol = EthernetProtocol::from_ether_type(read_u16(bytes, 12)?)?;
        Some(Self {
            destination: read_array::<6>(bytes, 0)?,
            source: read_array::<6>(bytes, 6)?,
            protocol,
            payload: &bytes[Self::HEADER_LEN..],
        })
    }

    pub fn encode_header(
        output: &mut [u8],
        destination: EthernetAddress,
        source: EthernetAddress,
        protocol: EthernetProtocol,
    ) -> Option<usize> {
        if output.len() < Self::HEADER_LEN {
            return None;
        }
        output[..6].copy_from_slice(&destination);
        output[6..12].copy_from_slice(&source);
        write_u16(output, 12, protocol.ether_type())?;
        Some(Self::HEADER_LEN)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArpOperation {
    Request,
    Reply,
}

impl ArpOperation {
    const fn code(self) -> u16 {
        match self {
            Self::Request => 1,
            Self::Reply => 2,
        }
    }

    const fn from_code(code: u16) -> Option<Self> {
        match code {
            1 => Some(Self::Request),
            2 => Some(Self::Reply),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArpPacket {
    pub operation: ArpOperation,
    pub sender_hardware: EthernetAddress,
    pub sender_protocol: Ipv4Address,
    pub target_hardware: EthernetAddress,
    pub target_protocol: Ipv4Address,
}

impl ArpPacket {
    pub const LEN: usize = 28;

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::LEN {
            return None;
        }
        if read_u16(bytes, 0)? != 1
            || read_u16(bytes, 2)? != EthernetProtocol::Ipv4.ether_type()
            || bytes[4] != 6
            || bytes[5] != 4
        {
            return None;
        }
        Some(Self {
            operation: ArpOperation::from_code(read_u16(bytes, 6)?)?,
            sender_hardware: read_array::<6>(bytes, 8)?,
            sender_protocol: Ipv4Address::new(read_array::<4>(bytes, 14)?),
            target_hardware: read_array::<6>(bytes, 18)?,
            target_protocol: Ipv4Address::new(read_array::<4>(bytes, 24)?),
        })
    }

    pub fn encode(self, output: &mut [u8]) -> Option<usize> {
        if output.len() < Self::LEN {
            return None;
        }
        write_u16(output, 0, 1)?;
        write_u16(output, 2, EthernetProtocol::Ipv4.ether_type())?;
        output[4] = 6;
        output[5] = 4;
        write_u16(output, 6, self.operation.code())?;
        output[8..14].copy_from_slice(&self.sender_hardware);
        output[14..18].copy_from_slice(&self.sender_protocol.octets());
        output[18..24].copy_from_slice(&self.target_hardware);
        output[24..28].copy_from_slice(&self.target_protocol.octets());
        Some(Self::LEN)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpProtocol {
    Icmp,
    Tcp,
    Udp,
    Icmpv6,
}

impl IpProtocol {
    pub const fn number(self) -> u8 {
        match self {
            Self::Icmp => 1,
            Self::Tcp => 6,
            Self::Udp => 17,
            Self::Icmpv6 => 58,
        }
    }

    pub const fn from_number(number: u8) -> Option<Self> {
        match number {
            1 => Some(Self::Icmp),
            6 => Some(Self::Tcp),
            17 => Some(Self::Udp),
            58 => Some(Self::Icmpv6),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Packet<'a> {
    pub source: Ipv4Address,
    pub destination: Ipv4Address,
    pub protocol: IpProtocol,
    pub hop_limit: u8,
    pub payload: &'a [u8],
}

impl<'a> Ipv4Packet<'a> {
    pub const MIN_HEADER_LEN: usize = 20;

    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < Self::MIN_HEADER_LEN || bytes[0] >> 4 != 4 {
            return None;
        }
        let header_len = usize::from(bytes[0] & 0x0f) * 4;
        if header_len < Self::MIN_HEADER_LEN || bytes.len() < header_len {
            return None;
        }
        if ipv4_checksum(&bytes[..header_len]) != 0 {
            return None;
        }
        let total_len = usize::from(read_u16(bytes, 2)?);
        if total_len < header_len || total_len > bytes.len() {
            return None;
        }
        let fragment = read_u16(bytes, 6)?;
        if fragment & 0x3fff != 0 {
            return None;
        }
        Some(Self {
            source: Ipv4Address::new(read_array::<4>(bytes, 12)?),
            destination: Ipv4Address::new(read_array::<4>(bytes, 16)?),
            protocol: IpProtocol::from_number(bytes[9])?,
            hop_limit: bytes[8],
            payload: &bytes[header_len..total_len],
        })
    }

    pub fn encode_header(
        output: &mut [u8],
        source: Ipv4Address,
        destination: Ipv4Address,
        protocol: IpProtocol,
        payload_len: usize,
        identification: u16,
        hop_limit: u8,
    ) -> Option<usize> {
        let total_len = Self::MIN_HEADER_LEN.checked_add(payload_len)?;
        let total_len = u16::try_from(total_len).ok()?;
        if output.len() < Self::MIN_HEADER_LEN {
            return None;
        }
        output[..Self::MIN_HEADER_LEN].fill(0);
        output[0] = 0x45;
        write_u16(output, 2, total_len)?;
        write_u16(output, 4, identification)?;
        write_u16(output, 6, 0x4000)?;
        output[8] = hop_limit;
        output[9] = protocol.number();
        output[12..16].copy_from_slice(&source.octets());
        output[16..20].copy_from_slice(&destination.octets());
        let checksum = ipv4_checksum(&output[..Self::MIN_HEADER_LEN]);
        write_u16(output, 10, checksum)?;
        Some(Self::MIN_HEADER_LEN)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv6Packet<'a> {
    pub source: Ipv6Address,
    pub destination: Ipv6Address,
    pub next_header: IpProtocol,
    pub hop_limit: u8,
    pub payload: &'a [u8],
}

impl<'a> Ipv6Packet<'a> {
    pub const HEADER_LEN: usize = 40;

    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < Self::HEADER_LEN || bytes[0] >> 4 != 6 {
            return None;
        }
        let payload_len = usize::from(read_u16(bytes, 4)?);
        let end = Self::HEADER_LEN.checked_add(payload_len)?;
        if bytes.len() < end {
            return None;
        }
        Some(Self {
            source: Ipv6Address::new(read_array::<16>(bytes, 8)?),
            destination: Ipv6Address::new(read_array::<16>(bytes, 24)?),
            next_header: IpProtocol::from_number(bytes[6])?,
            hop_limit: bytes[7],
            payload: &bytes[Self::HEADER_LEN..end],
        })
    }

    pub fn encode_header(
        output: &mut [u8],
        source: Ipv6Address,
        destination: Ipv6Address,
        next_header: IpProtocol,
        payload_len: usize,
        hop_limit: u8,
    ) -> Option<usize> {
        let payload_len = u16::try_from(payload_len).ok()?;
        if output.len() < Self::HEADER_LEN {
            return None;
        }
        output[..Self::HEADER_LEN].fill(0);
        output[0] = 0x60;
        write_u16(output, 4, payload_len)?;
        output[6] = next_header.number();
        output[7] = hop_limit;
        output[8..24].copy_from_slice(&source.octets());
        output[24..40].copy_from_slice(&destination.octets());
        Some(Self::HEADER_LEN)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpPacket<'a> {
    pub source_port: u16,
    pub destination_port: u16,
    pub payload: &'a [u8],
}

impl<'a> UdpPacket<'a> {
    pub const HEADER_LEN: usize = 8;

    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < Self::HEADER_LEN {
            return None;
        }
        let len = usize::from(read_u16(bytes, 4)?);
        if len < Self::HEADER_LEN || len > bytes.len() {
            return None;
        }
        Some(Self {
            source_port: read_u16(bytes, 0)?,
            destination_port: read_u16(bytes, 2)?,
            payload: &bytes[Self::HEADER_LEN..len],
        })
    }

    pub fn encode(
        output: &mut [u8],
        source: IpAddress,
        destination: IpAddress,
        source_port: u16,
        destination_port: u16,
        payload: &[u8],
    ) -> Option<usize> {
        let len = Self::HEADER_LEN.checked_add(payload.len())?;
        let len_u16 = u16::try_from(len).ok()?;
        if output.len() < len {
            return None;
        }
        write_u16(output, 0, source_port)?;
        write_u16(output, 2, destination_port)?;
        write_u16(output, 4, len_u16)?;
        write_u16(output, 6, 0)?;
        output[Self::HEADER_LEN..len].copy_from_slice(payload);
        let checksum = udp_checksum(source, destination, &output[..len]);
        write_u16(output, 6, checksum)?;
        Some(len)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpFlags(u16);

impl TcpFlags {
    pub const FIN: Self = Self(1 << 0);
    pub const SYN: Self = Self(1 << 1);
    pub const RST: Self = Self(1 << 2);
    pub const PSH: Self = Self(1 << 3);
    pub const ACK: Self = Self(1 << 4);
    pub const URG: Self = Self(1 << 5);
    pub const ECE: Self = Self(1 << 6);
    pub const CWR: Self = Self(1 << 7);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpPacket<'a> {
    pub source_port: u16,
    pub destination_port: u16,
    pub sequence: u32,
    pub acknowledgement: u32,
    pub flags: TcpFlags,
    pub window_size: u16,
    pub payload: &'a [u8],
}

impl<'a> TcpPacket<'a> {
    pub const MIN_HEADER_LEN: usize = 20;

    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < Self::MIN_HEADER_LEN {
            return None;
        }
        let header_len = usize::from(bytes[12] >> 4) * 4;
        if header_len < Self::MIN_HEADER_LEN || header_len > bytes.len() {
            return None;
        }
        Some(Self {
            source_port: read_u16(bytes, 0)?,
            destination_port: read_u16(bytes, 2)?,
            sequence: read_u32(bytes, 4)?,
            acknowledgement: read_u32(bytes, 8)?,
            flags: TcpFlags(u16::from(bytes[13]) | (u16::from(bytes[12] & 0x01) << 8)),
            window_size: read_u16(bytes, 14)?,
            payload: &bytes[header_len..],
        })
    }

    pub fn encode(
        output: &mut [u8],
        source: IpAddress,
        destination: IpAddress,
        header: TcpHeader,
        payload: &[u8],
    ) -> Option<usize> {
        let len = Self::MIN_HEADER_LEN.checked_add(payload.len())?;
        if output.len() < len {
            return None;
        }
        output[..Self::MIN_HEADER_LEN].fill(0);
        write_u16(output, 0, header.source_port)?;
        write_u16(output, 2, header.destination_port)?;
        write_u32(output, 4, header.sequence)?;
        write_u32(output, 8, header.acknowledgement)?;
        output[12] = 5 << 4;
        output[13] = header.flags.bits() as u8;
        write_u16(output, 14, header.window_size)?;
        output[Self::MIN_HEADER_LEN..len].copy_from_slice(payload);
        let checksum = match (source, destination) {
            (IpAddress::Ipv4(source), IpAddress::Ipv4(destination)) => {
                tcpv4_checksum(source, destination, &output[..len])
            }
            (IpAddress::Ipv6(source), IpAddress::Ipv6(destination)) => {
                tcpv6_checksum(source, destination, &output[..len])
            }
            _ => panic!("TCP pseudo-header address families must match"),
        };
        write_u16(output, 16, checksum)?;
        Some(len)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpHeader {
    pub source_port: u16,
    pub destination_port: u16,
    pub sequence: u32,
    pub acknowledgement: u32,
    pub flags: TcpFlags,
    pub window_size: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Icmpv4Echo<'a> {
    pub identifier: u16,
    pub sequence: u16,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icmpv4Packet<'a> {
    EchoRequest(Icmpv4Echo<'a>),
    EchoReply(Icmpv4Echo<'a>),
}

impl<'a> Icmpv4Packet<'a> {
    pub const HEADER_LEN: usize = 8;

    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < Self::HEADER_LEN || internet_checksum(bytes) != 0 {
            return None;
        }
        let echo = Icmpv4Echo {
            identifier: read_u16(bytes, 4)?,
            sequence: read_u16(bytes, 6)?,
            payload: &bytes[Self::HEADER_LEN..],
        };
        match (bytes[0], bytes[1]) {
            (8, 0) => Some(Self::EchoRequest(echo)),
            (0, 0) => Some(Self::EchoReply(echo)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Icmpv6Echo<'a> {
    pub identifier: u16,
    pub sequence: u16,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icmpv6Packet<'a> {
    EchoRequest(Icmpv6Echo<'a>),
    EchoReply(Icmpv6Echo<'a>),
    NeighborSolicitation { target: Ipv6Address },
    NeighborAdvertisement { target: Ipv6Address },
}

impl<'a> Icmpv6Packet<'a> {
    pub const ECHO_HEADER_LEN: usize = 8;

    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < Self::ECHO_HEADER_LEN {
            return None;
        }
        match bytes[0] {
            128 => Some(Self::EchoRequest(Icmpv6Echo {
                identifier: read_u16(bytes, 4)?,
                sequence: read_u16(bytes, 6)?,
                payload: &bytes[Self::ECHO_HEADER_LEN..],
            })),
            129 => Some(Self::EchoReply(Icmpv6Echo {
                identifier: read_u16(bytes, 4)?,
                sequence: read_u16(bytes, 6)?,
                payload: &bytes[Self::ECHO_HEADER_LEN..],
            })),
            135 if bytes.len() >= 24 => Some(Self::NeighborSolicitation {
                target: Ipv6Address::new(read_array::<16>(bytes, 8)?),
            }),
            136 if bytes.len() >= 24 => Some(Self::NeighborAdvertisement {
                target: Ipv6Address::new(read_array::<16>(bytes, 8)?),
            }),
            _ => None,
        }
    }
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    let end = offset.checked_add(N)?;
    Some(bytes.get(offset..end)?.try_into().ok()?)
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(read_array(bytes, offset)?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(read_array(bytes, offset)?))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) -> Option<()> {
    let end = offset.checked_add(2)?;
    bytes
        .get_mut(offset..end)?
        .copy_from_slice(&value.to_be_bytes());
    Some(())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Option<()> {
    let end = offset.checked_add(4)?;
    bytes
        .get_mut(offset..end)?
        .copy_from_slice(&value.to_be_bytes());
    Some(())
}
