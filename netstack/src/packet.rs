use crate::{
    EthernetAddress, IpAddress, Ipv4Address, Ipv6Address, icmpv6_checksum, internet_checksum,
    ipv4_checksum, tcp_pseudo_header_checksum, tcpv4_checksum, tcpv6_checksum, udp_checksum,
    udp_pseudo_header_checksum,
};

/// How a transport checksum is produced at encode time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportChecksum {
    /// Compute and store the full checksum in software.
    Software,
    /// Seed the checksum field with the folded pseudo-header sum and
    /// let the device finish it from the frame's csum_start/csum_offset.
    DeviceOffload,
}

pub const MAX_TCP_SACK_BLOCKS: usize = 4;

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
    pub identification: u16,
    pub header_len: usize,
    pub fragment_offset_blocks: u16,
    pub more_fragments: bool,
    pub total_len: usize,
    pub payload: &'a [u8],
}

impl<'a> Ipv4Packet<'a> {
    pub const MIN_HEADER_LEN: usize = 20;

    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        Self::parse_with_payload(bytes, Ipv4PayloadLength::Strict, Ipv4Checksum::Validate)
    }

    pub fn parse_without_checksum(bytes: &'a [u8]) -> Option<Self> {
        Self::parse_with_payload(bytes, Ipv4PayloadLength::Strict, Ipv4Checksum::Skip)
    }

    pub fn parse_quoted(bytes: &'a [u8]) -> Option<Self> {
        Self::parse_with_payload(bytes, Ipv4PayloadLength::Truncated, Ipv4Checksum::Validate)
    }

    fn parse_with_payload(
        bytes: &'a [u8],
        payload_length: Ipv4PayloadLength,
        checksum: Ipv4Checksum,
    ) -> Option<Self> {
        if bytes.len() < Self::MIN_HEADER_LEN || bytes[0] >> 4 != 4 {
            return None;
        }
        let header_len = usize::from(bytes[0] & 0x0f) * 4;
        if header_len < Self::MIN_HEADER_LEN || bytes.len() < header_len {
            return None;
        }
        if matches!(checksum, Ipv4Checksum::Validate) && ipv4_checksum(&bytes[..header_len]) != 0 {
            return None;
        }
        let total_len = usize::from(read_u16(bytes, 2)?);
        if total_len < header_len {
            return None;
        }
        let payload_end = match payload_length {
            Ipv4PayloadLength::Strict if total_len > bytes.len() => return None,
            Ipv4PayloadLength::Strict => total_len,
            Ipv4PayloadLength::Truncated => total_len.min(bytes.len()),
        };
        if payload_end < header_len {
            return None;
        }
        let fragment = read_u16(bytes, 6)?;
        Some(Self {
            source: Ipv4Address::new(read_array::<4>(bytes, 12)?),
            destination: Ipv4Address::new(read_array::<4>(bytes, 16)?),
            protocol: IpProtocol::from_number(bytes[9])?,
            hop_limit: bytes[8],
            identification: read_u16(bytes, 4)?,
            header_len,
            fragment_offset_blocks: fragment & 0x1fff,
            more_fragments: fragment & 0x2000 != 0,
            total_len,
            payload: &bytes[header_len..payload_end],
        })
    }

    pub const fn is_fragmented(self) -> bool {
        self.fragment_offset_blocks != 0 || self.more_fragments
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
enum Ipv4PayloadLength {
    Strict,
    Truncated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ipv4Checksum {
    Validate,
    Skip,
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
        Self::parse_with_payload(bytes, Ipv6PayloadLength::Strict)
    }

    pub fn parse_quoted(bytes: &'a [u8]) -> Option<Self> {
        Self::parse_with_payload(bytes, Ipv6PayloadLength::Truncated)
    }

    fn parse_with_payload(bytes: &'a [u8], payload_length: Ipv6PayloadLength) -> Option<Self> {
        if bytes.len() < Self::HEADER_LEN || bytes[0] >> 4 != 6 {
            return None;
        }
        let payload_len = usize::from(read_u16(bytes, 4)?);
        let end = Self::HEADER_LEN.checked_add(payload_len)?;
        let payload_end = match payload_length {
            Ipv6PayloadLength::Strict if bytes.len() < end => return None,
            Ipv6PayloadLength::Strict => end,
            Ipv6PayloadLength::Truncated => end.min(bytes.len()),
        };
        Some(Self {
            source: Ipv6Address::new(read_array::<16>(bytes, 8)?),
            destination: Ipv6Address::new(read_array::<16>(bytes, 24)?),
            next_header: IpProtocol::from_number(bytes[6])?,
            hop_limit: bytes[7],
            payload: &bytes[Self::HEADER_LEN..payload_end],
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
enum Ipv6PayloadLength {
    Strict,
    Truncated,
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

    pub fn parse_ports(bytes: &[u8]) -> Option<TransportPorts> {
        Some(TransportPorts {
            source: read_u16(bytes, 0)?,
            destination: read_u16(bytes, 2)?,
        })
    }

    pub fn encode(
        output: &mut [u8],
        source: IpAddress,
        destination: IpAddress,
        source_port: u16,
        destination_port: u16,
        payload: &[u8],
        checksum: TransportChecksum,
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
        let field = match checksum {
            TransportChecksum::Software => {
                udp_transmitted_checksum(udp_checksum(source, destination, &output[..len]))
            }
            TransportChecksum::DeviceOffload => {
                udp_pseudo_header_checksum(source, destination, len)
            }
        };
        write_u16(output, 6, field)?;
        Some(len)
    }
}

fn udp_transmitted_checksum(checksum: u16) -> u16 {
    if checksum == 0 { u16::MAX } else { checksum }
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
    pub options: TcpOptions<'a>,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpQuotedPacket {
    pub source_port: u16,
    pub destination_port: u16,
    pub sequence: u32,
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
            options: TcpOptions::parse(&bytes[Self::MIN_HEADER_LEN..header_len])?,
            payload: &bytes[header_len..],
        })
    }

    pub fn parse_ports(bytes: &[u8]) -> Option<TransportPorts> {
        Some(TransportPorts {
            source: read_u16(bytes, 0)?,
            destination: read_u16(bytes, 2)?,
        })
    }

    pub fn parse_quoted(bytes: &[u8]) -> Option<TcpQuotedPacket> {
        Some(TcpQuotedPacket {
            source_port: read_u16(bytes, 0)?,
            destination_port: read_u16(bytes, 2)?,
            sequence: read_u32(bytes, 4)?,
        })
    }

    pub fn encode(
        output: &mut [u8],
        source: IpAddress,
        destination: IpAddress,
        header: TcpHeader,
        payload: &[u8],
        checksum: TransportChecksum,
    ) -> Option<usize> {
        Self::encode_with_options(
            output,
            source,
            destination,
            header,
            TcpHeaderOptions::empty(),
            payload,
            checksum,
        )
    }

    /// Encodes only the TCP header and options for a scatter frame whose
    /// payload is transmitted from its own buffer: the payload length is
    /// folded into the pseudo-header checksum seed, so this encoding is
    /// only valid together with transmit checksum offload.
    pub fn encode_headers_scatter(
        output: &mut [u8],
        source: IpAddress,
        destination: IpAddress,
        header: TcpHeader,
        options: TcpHeaderOptions,
        payload_len: usize,
    ) -> Option<usize> {
        let options_len = options.encoded_len();
        let header_len = Self::MIN_HEADER_LEN.checked_add(options_len)?;
        let data_offset_words = u8::try_from(header_len / 4).ok()?;
        if output.len() < header_len {
            return None;
        }
        output[..header_len].fill(0);
        write_u16(output, 0, header.source_port)?;
        write_u16(output, 2, header.destination_port)?;
        write_u32(output, 4, header.sequence)?;
        write_u32(output, 8, header.acknowledgement)?;
        output[12] = data_offset_words << 4;
        output[13] = header.flags.bits() as u8;
        write_u16(output, 14, header.window_size)?;
        options.encode(&mut output[Self::MIN_HEADER_LEN..header_len])?;
        let segment_len = header_len.checked_add(payload_len)?;
        let field = tcp_pseudo_header_checksum(source, destination, segment_len);
        write_u16(output, 16, field)?;
        Some(header_len)
    }

    pub fn encode_with_options(
        output: &mut [u8],
        source: IpAddress,
        destination: IpAddress,
        header: TcpHeader,
        options: TcpHeaderOptions,
        payload: &[u8],
        checksum: TransportChecksum,
    ) -> Option<usize> {
        let options_len = options.encoded_len();
        let header_len = Self::MIN_HEADER_LEN.checked_add(options_len)?;
        let data_offset_words = u8::try_from(header_len / 4).ok()?;
        let len = header_len.checked_add(payload.len())?;
        if output.len() < len {
            return None;
        }
        output[..header_len].fill(0);
        write_u16(output, 0, header.source_port)?;
        write_u16(output, 2, header.destination_port)?;
        write_u32(output, 4, header.sequence)?;
        write_u32(output, 8, header.acknowledgement)?;
        output[12] = data_offset_words << 4;
        output[13] = header.flags.bits() as u8;
        write_u16(output, 14, header.window_size)?;
        options.encode(&mut output[Self::MIN_HEADER_LEN..header_len])?;
        output[header_len..len].copy_from_slice(payload);
        let field = match checksum {
            TransportChecksum::Software => match (source, destination) {
                (IpAddress::Ipv4(source), IpAddress::Ipv4(destination)) => {
                    tcpv4_checksum(source, destination, &output[..len])
                }
                (IpAddress::Ipv6(source), IpAddress::Ipv6(destination)) => {
                    tcpv6_checksum(source, destination, &output[..len])
                }
                _ => panic!("TCP pseudo-header address families must match"),
            },
            TransportChecksum::DeviceOffload => {
                tcp_pseudo_header_checksum(source, destination, len)
            }
        };
        write_u16(output, 16, field)?;
        Some(len)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportPorts {
    pub source: u16,
    pub destination: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpTimestampOption {
    pub value: u32,
    pub echo_reply: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpSackBlock {
    pub left_edge: u32,
    pub right_edge: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpSackBlocks {
    blocks: [TcpSackBlock; MAX_TCP_SACK_BLOCKS],
    len: usize,
}

impl TcpSackBlocks {
    pub const fn empty() -> Self {
        Self {
            blocks: [TcpSackBlock {
                left_edge: 0,
                right_edge: 0,
            }; MAX_TCP_SACK_BLOCKS],
            len: 0,
        }
    }

    pub fn push(&mut self, block: TcpSackBlock) -> Result<(), TcpSackBlock> {
        if self.len == MAX_TCP_SACK_BLOCKS {
            return Err(block);
        }
        self.blocks[self.len] = block;
        self.len += 1;
        Ok(())
    }

    pub const fn len(self) -> usize {
        self.len
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[TcpSackBlock] {
        &self.blocks[..self.len]
    }
}

impl Default for TcpSackBlocks {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpOptions<'a> {
    raw: &'a [u8],
    maximum_segment_size: Option<u16>,
    window_scale: Option<u8>,
    sack_permitted: bool,
    sack_blocks: TcpSackBlocks,
    timestamp: Option<TcpTimestampOption>,
}

impl<'a> TcpOptions<'a> {
    pub const fn empty() -> Self {
        Self {
            raw: &[],
            maximum_segment_size: None,
            window_scale: None,
            sack_permitted: false,
            sack_blocks: TcpSackBlocks::empty(),
            timestamp: None,
        }
    }

    pub fn parse(raw: &'a [u8]) -> Option<Self> {
        let mut options = Self {
            raw,
            ..Self::empty()
        };
        let mut offset = 0;
        while offset < raw.len() {
            match raw[offset] {
                0 => break,
                1 => offset += 1,
                kind => {
                    let len = usize::from(*raw.get(offset + 1)?);
                    if len < 2 || offset.checked_add(len)? > raw.len() {
                        return None;
                    }
                    let data = &raw[offset + 2..offset + len];
                    match kind {
                        2 if data.len() == 2 => {
                            options.maximum_segment_size =
                                Some(u16::from_be_bytes(data.try_into().ok()?));
                        }
                        3 if data.len() == 1 => {
                            options.window_scale = Some(data[0]);
                        }
                        4 if data.is_empty() => {
                            options.sack_permitted = true;
                        }
                        5 if data.len().is_multiple_of(8) => {
                            for block in data.chunks_exact(8) {
                                options
                                    .sack_blocks
                                    .push(TcpSackBlock {
                                        left_edge: u32::from_be_bytes(block[..4].try_into().ok()?),
                                        right_edge: u32::from_be_bytes(block[4..].try_into().ok()?),
                                    })
                                    .ok()?;
                            }
                        }
                        8 if data.len() == 8 => {
                            options.timestamp = Some(TcpTimestampOption {
                                value: u32::from_be_bytes(data[..4].try_into().ok()?),
                                echo_reply: u32::from_be_bytes(data[4..].try_into().ok()?),
                            });
                        }
                        _ => {}
                    }
                    offset += len;
                }
            }
        }
        Some(options)
    }

    pub const fn raw(self) -> &'a [u8] {
        self.raw
    }

    pub const fn maximum_segment_size(self) -> Option<u16> {
        self.maximum_segment_size
    }

    pub const fn window_scale(self) -> Option<u8> {
        self.window_scale
    }

    pub const fn sack_permitted(self) -> bool {
        self.sack_permitted
    }

    pub const fn sack_blocks(self) -> TcpSackBlocks {
        self.sack_blocks
    }

    pub const fn timestamp(self) -> Option<TcpTimestampOption> {
        self.timestamp
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpHeaderOptions {
    maximum_segment_size: Option<u16>,
    window_scale: Option<u8>,
    sack_permitted: bool,
    sack_blocks: TcpSackBlocks,
    timestamp: Option<TcpTimestampOption>,
}

impl TcpHeaderOptions {
    pub const fn empty() -> Self {
        Self {
            maximum_segment_size: None,
            window_scale: None,
            sack_permitted: false,
            sack_blocks: TcpSackBlocks::empty(),
            timestamp: None,
        }
    }

    pub const fn with_maximum_segment_size(mut self, value: u16) -> Self {
        assert!(value != 0, "TCP MSS option must be non-zero");
        self.maximum_segment_size = Some(value);
        self
    }

    pub const fn with_window_scale(mut self, shift: u8) -> Self {
        assert!(shift <= 14, "TCP window scale must be <= 14");
        self.window_scale = Some(shift);
        self
    }

    pub const fn with_sack_permitted(mut self) -> Self {
        self.sack_permitted = true;
        self
    }

    pub const fn with_sack_blocks(mut self, blocks: TcpSackBlocks) -> Self {
        self.sack_blocks = blocks;
        self
    }

    pub const fn sack_blocks(self) -> TcpSackBlocks {
        self.sack_blocks
    }

    pub const fn with_timestamp(mut self, value: TcpTimestampOption) -> Self {
        self.timestamp = Some(value);
        self
    }

    pub const fn timestamp(self) -> Option<TcpTimestampOption> {
        self.timestamp
    }

    pub fn encoded_len(self) -> usize {
        let maximum_segment_size_len = match self.maximum_segment_size {
            Some(_) => 4,
            None => 0,
        };
        let window_scale_len = match self.window_scale {
            Some(_) => 3,
            None => 0,
        };
        let sack_permitted_len = if self.sack_permitted { 2 } else { 0 };
        let sack_blocks_len = if self.sack_blocks.is_empty() {
            0
        } else {
            2 + self.sack_blocks.len() * 8
        };
        let timestamp_len = match self.timestamp {
            Some(_) => 10,
            None => 0,
        };
        align_tcp_options_len(
            maximum_segment_size_len
                + window_scale_len
                + sack_permitted_len
                + sack_blocks_len
                + timestamp_len,
        )
    }

    fn encode(self, output: &mut [u8]) -> Option<()> {
        if output.len() != self.encoded_len() {
            return None;
        }
        output.fill(0);
        let mut offset = 0;
        if let Some(mss) = self.maximum_segment_size {
            output[offset] = 2;
            output[offset + 1] = 4;
            output[offset + 2..offset + 4].copy_from_slice(&mss.to_be_bytes());
            offset += 4;
        }
        if let Some(shift) = self.window_scale {
            output[offset] = 3;
            output[offset + 1] = 3;
            output[offset + 2] = shift;
            offset += 3;
        }
        if self.sack_permitted {
            output[offset] = 4;
            output[offset + 1] = 2;
            offset += 2;
        }
        if !self.sack_blocks.is_empty() {
            output[offset] = 5;
            output[offset + 1] = u8::try_from(2 + self.sack_blocks.len() * 8).ok()?;
            offset += 2;
            for block in self.sack_blocks.as_slice() {
                output[offset..offset + 4].copy_from_slice(&block.left_edge.to_be_bytes());
                output[offset + 4..offset + 8].copy_from_slice(&block.right_edge.to_be_bytes());
                offset += 8;
            }
        }
        if let Some(timestamp) = self.timestamp {
            output[offset] = 8;
            output[offset + 1] = 10;
            output[offset + 2..offset + 6].copy_from_slice(&timestamp.value.to_be_bytes());
            output[offset + 6..offset + 10].copy_from_slice(&timestamp.echo_reply.to_be_bytes());
        }
        Some(())
    }
}

const fn align_tcp_options_len(len: usize) -> usize {
    (len + 3) & !3
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
pub struct Icmpv4DestinationUnreachable<'a> {
    pub code: Icmpv4DestinationUnreachableCode,
    pub original: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icmpv4DestinationUnreachableCode {
    Net,
    Host,
    Protocol,
    Port,
    FragmentationNeeded { next_hop_mtu: u16 },
    SourceRouteFailed,
    NetUnknown,
    HostUnknown,
    SourceHostIsolated,
    NetProhibited,
    HostProhibited,
    NetTos,
    HostTos,
    CommunicationProhibited,
    HostPrecedenceViolation,
    PrecedenceCutoff,
}

impl Icmpv4DestinationUnreachableCode {
    pub const fn raw(self) -> u8 {
        match self {
            Self::Net => 0,
            Self::Host => 1,
            Self::Protocol => 2,
            Self::Port => 3,
            Self::FragmentationNeeded { .. } => 4,
            Self::SourceRouteFailed => 5,
            Self::NetUnknown => 6,
            Self::HostUnknown => 7,
            Self::SourceHostIsolated => 8,
            Self::NetProhibited => 9,
            Self::HostProhibited => 10,
            Self::NetTos => 11,
            Self::HostTos => 12,
            Self::CommunicationProhibited => 13,
            Self::HostPrecedenceViolation => 14,
            Self::PrecedenceCutoff => 15,
        }
    }

    const fn from_raw(code: u8, next_hop_mtu: u16) -> Option<Self> {
        match code {
            0 => Some(Self::Net),
            1 => Some(Self::Host),
            2 => Some(Self::Protocol),
            3 => Some(Self::Port),
            4 => Some(Self::FragmentationNeeded { next_hop_mtu }),
            5 => Some(Self::SourceRouteFailed),
            6 => Some(Self::NetUnknown),
            7 => Some(Self::HostUnknown),
            8 => Some(Self::SourceHostIsolated),
            9 => Some(Self::NetProhibited),
            10 => Some(Self::HostProhibited),
            11 => Some(Self::NetTos),
            12 => Some(Self::HostTos),
            13 => Some(Self::CommunicationProhibited),
            14 => Some(Self::HostPrecedenceViolation),
            15 => Some(Self::PrecedenceCutoff),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icmpv4Packet<'a> {
    EchoRequest(Icmpv4Echo<'a>),
    EchoReply(Icmpv4Echo<'a>),
    DestinationUnreachable(Icmpv4DestinationUnreachable<'a>),
}

impl<'a> Icmpv4Packet<'a> {
    pub const HEADER_LEN: usize = 8;
    pub const DESTINATION_UNREACHABLE_PORT_CODE: Icmpv4DestinationUnreachableCode =
        Icmpv4DestinationUnreachableCode::Port;

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
            (3, code) => Some(Self::DestinationUnreachable(Icmpv4DestinationUnreachable {
                code: Icmpv4DestinationUnreachableCode::from_raw(code, read_u16(bytes, 6)?)?,
                original: &bytes[Self::HEADER_LEN..],
            })),
            _ => None,
        }
    }

    /// Encodes an Echo Request (RFC 792) carrying `payload`.
    pub fn encode_echo_request(
        output: &mut [u8],
        identifier: u16,
        sequence: u16,
        payload: &[u8],
    ) -> Option<usize> {
        let len = Self::HEADER_LEN.checked_add(payload.len())?;
        if output.len() < len {
            return None;
        }
        output[..len].fill(0);
        output[0] = 8;
        write_u16(output, 4, identifier)?;
        write_u16(output, 6, sequence)?;
        output[Self::HEADER_LEN..len].copy_from_slice(payload);
        let checksum = internet_checksum(&output[..len]);
        write_u16(output, 2, checksum)?;
        Some(len)
    }

    pub fn encode_destination_unreachable(
        output: &mut [u8],
        code: Icmpv4DestinationUnreachableCode,
        original: &[u8],
    ) -> Option<usize> {
        let len = Self::HEADER_LEN.checked_add(original.len())?;
        if output.len() < len {
            return None;
        }
        output[..len].fill(0);
        output[0] = 3;
        output[1] = code.raw();
        if let Icmpv4DestinationUnreachableCode::FragmentationNeeded { next_hop_mtu } = code {
            write_u16(output, 6, next_hop_mtu)?;
        }
        output[Self::HEADER_LEN..len].copy_from_slice(original);
        let checksum = internet_checksum(&output[..len]);
        write_u16(output, 2, checksum)?;
        Some(len)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Icmpv6Echo<'a> {
    pub identifier: u16,
    pub sequence: u16,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Icmpv6DestinationUnreachable<'a> {
    pub code: Icmpv6DestinationUnreachableCode,
    pub original: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icmpv6DestinationUnreachableCode {
    NoRoute,
    AdminProhibited,
    BeyondScope,
    AddressUnreachable,
    PortUnreachable,
    SourceAddressFailed,
    RejectRoute,
}

impl Icmpv6DestinationUnreachableCode {
    pub const fn raw(self) -> u8 {
        match self {
            Self::NoRoute => 0,
            Self::AdminProhibited => 1,
            Self::BeyondScope => 2,
            Self::AddressUnreachable => 3,
            Self::PortUnreachable => 4,
            Self::SourceAddressFailed => 5,
            Self::RejectRoute => 6,
        }
    }

    const fn from_raw(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::NoRoute),
            1 => Some(Self::AdminProhibited),
            2 => Some(Self::BeyondScope),
            3 => Some(Self::AddressUnreachable),
            4 => Some(Self::PortUnreachable),
            5 => Some(Self::SourceAddressFailed),
            6 => Some(Self::RejectRoute),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Icmpv6PacketTooBig<'a> {
    pub mtu: u32,
    pub original: &'a [u8],
}

/// One Neighbor Discovery option (RFC 4861 §4.6, RFC 8106 §5).
///
/// Options the stack has no use for are surfaced as [`NdpOption::Other`]
/// rather than dropped so a caller can log or count them; the iterator
/// still skips over their bytes correctly because every NDP option is
/// length-prefixed in units of 8 octets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NdpOption<'a> {
    SourceLinkLayerAddress(EthernetAddress),
    TargetLinkLayerAddress(EthernetAddress),
    PrefixInformation(NdpPrefixInformation),
    Mtu(u32),
    RecursiveDnsServers(NdpRecursiveDnsServers<'a>),
    Other { kind: u8 },
}

/// Prefix Information option (RFC 4861 §4.6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NdpPrefixInformation {
    pub prefix: Ipv6Address,
    pub prefix_len: u8,
    /// L bit: the prefix identifies an on-link subnet.
    pub on_link: bool,
    /// A bit: the prefix may be used for stateless address autoconfiguration.
    pub autonomous: bool,
    pub valid_lifetime_seconds: u32,
    pub preferred_lifetime_seconds: u32,
}

/// Recursive DNS Server option (RFC 8106 §5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NdpRecursiveDnsServers<'a> {
    pub lifetime_seconds: u32,
    addresses: &'a [u8],
}

impl<'a> NdpRecursiveDnsServers<'a> {
    pub fn addresses(self) -> impl Iterator<Item = Ipv6Address> + 'a {
        self.addresses
            .chunks_exact(16)
            .map(|chunk| Ipv6Address::new(chunk.try_into().expect("16-byte chunk")))
    }
}

/// Borrowed NDP option list; iterating it parses lazily.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NdpOptions<'a> {
    bytes: &'a [u8],
}

impl<'a> NdpOptions<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    pub const fn raw(self) -> &'a [u8] {
        self.bytes
    }

    pub fn iter(self) -> NdpOptionIter<'a> {
        NdpOptionIter { bytes: self.bytes }
    }

    /// First Source Link-Layer Address option, if any.
    pub fn source_link_layer_address(self) -> Option<EthernetAddress> {
        self.iter().find_map(|option| match option {
            NdpOption::SourceLinkLayerAddress(mac) => Some(mac),
            _ => None,
        })
    }

    /// First Target Link-Layer Address option, if any.
    pub fn target_link_layer_address(self) -> Option<EthernetAddress> {
        self.iter().find_map(|option| match option {
            NdpOption::TargetLinkLayerAddress(mac) => Some(mac),
            _ => None,
        })
    }

    /// First MTU option, if any.
    pub fn mtu(self) -> Option<u32> {
        self.iter().find_map(|option| match option {
            NdpOption::Mtu(mtu) => Some(mtu),
            _ => None,
        })
    }
}

impl<'a> IntoIterator for NdpOptions<'a> {
    type Item = NdpOption<'a>;
    type IntoIter = NdpOptionIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NdpOptionIter<'a> {
    bytes: &'a [u8],
}

impl<'a> Iterator for NdpOptionIter<'a> {
    type Item = NdpOption<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.bytes.len() < 2 {
                self.bytes = &[];
                return None;
            }
            let kind = self.bytes[0];
            // Length counts 8-octet units and includes the type/length
            // bytes; zero is illegal and would otherwise loop forever.
            let len = usize::from(self.bytes[1]).checked_mul(8)?;
            if len == 0 || len > self.bytes.len() {
                self.bytes = &[];
                return None;
            }
            let (option, rest) = self.bytes.split_at(len);
            self.bytes = rest;
            let body = &option[2..];
            let parsed = match kind {
                1 if body.len() >= 6 => Some(NdpOption::SourceLinkLayerAddress(
                    body[..6].try_into().ok()?,
                )),
                2 if body.len() >= 6 => Some(NdpOption::TargetLinkLayerAddress(
                    body[..6].try_into().ok()?,
                )),
                3 if len == 32 => Some(NdpOption::PrefixInformation(NdpPrefixInformation {
                    prefix: Ipv6Address::new(read_array::<16>(option, 16)?),
                    prefix_len: option[2],
                    on_link: option[3] & 0x80 != 0,
                    autonomous: option[3] & 0x40 != 0,
                    valid_lifetime_seconds: read_u32(option, 4)?,
                    preferred_lifetime_seconds: read_u32(option, 8)?,
                })),
                5 if len == 8 => Some(NdpOption::Mtu(read_u32(option, 4)?)),
                25 if len >= 24 => Some(NdpOption::RecursiveDnsServers(NdpRecursiveDnsServers {
                    lifetime_seconds: read_u32(option, 4)?,
                    addresses: &option[8..],
                })),
                _ => Some(NdpOption::Other { kind }),
            };
            if let Some(parsed) = parsed {
                return Some(parsed);
            }
        }
    }
}

/// Router Advertisement body (RFC 4861 §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Icmpv6RouterAdvertisement<'a> {
    pub current_hop_limit: u8,
    /// M bit: addresses are available through stateful DHCPv6.
    pub managed_address_configuration: bool,
    /// O bit: other configuration is available through stateful DHCPv6.
    pub other_configuration: bool,
    /// Lifetime of this router as a default router; zero means the
    /// sender is not a default router.
    pub router_lifetime_seconds: u16,
    pub reachable_time_millis: u32,
    pub retransmit_timer_millis: u32,
    pub options: NdpOptions<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icmpv6Packet<'a> {
    EchoRequest(Icmpv6Echo<'a>),
    EchoReply(Icmpv6Echo<'a>),
    DestinationUnreachable(Icmpv6DestinationUnreachable<'a>),
    PacketTooBig(Icmpv6PacketTooBig<'a>),
    RouterAdvertisement(Icmpv6RouterAdvertisement<'a>),
    NeighborSolicitation {
        target: Ipv6Address,
        options: NdpOptions<'a>,
    },
    NeighborAdvertisement {
        target: Ipv6Address,
        /// S bit: this advertisement answers a solicitation.
        solicited: bool,
        /// O bit: the advertisement overrides an existing cache entry.
        override_cache: bool,
        options: NdpOptions<'a>,
    },
}

impl<'a> Icmpv6Packet<'a> {
    pub const ECHO_HEADER_LEN: usize = 8;
    pub const DESTINATION_UNREACHABLE_HEADER_LEN: usize = 8;
    pub const DESTINATION_UNREACHABLE_PORT_CODE: Icmpv6DestinationUnreachableCode =
        Icmpv6DestinationUnreachableCode::PortUnreachable;
    pub const PACKET_TOO_BIG_HEADER_LEN: usize = 8;
    pub const NEIGHBOR_MESSAGE_LEN: usize = 32;
    /// Router Advertisement fixed header, before its options.
    pub const ROUTER_ADVERTISEMENT_HEADER_LEN: usize = 16;
    /// Router Solicitation carrying one Source Link-Layer Address option.
    pub const ROUTER_SOLICITATION_LEN: usize = 16;
    /// Router Solicitation sent from the unspecified address, which
    /// RFC 4861 §4.1 forbids from carrying a link-layer address option.
    pub const ROUTER_SOLICITATION_ANONYMOUS_LEN: usize = 8;

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
            1 => Some(Self::DestinationUnreachable(Icmpv6DestinationUnreachable {
                code: Icmpv6DestinationUnreachableCode::from_raw(bytes[1])?,
                original: &bytes[Self::DESTINATION_UNREACHABLE_HEADER_LEN..],
            })),
            2 => Some(Self::PacketTooBig(Icmpv6PacketTooBig {
                mtu: read_u32(bytes, 4)?,
                original: &bytes[Self::PACKET_TOO_BIG_HEADER_LEN..],
            })),
            134 if bytes.len() >= Self::ROUTER_ADVERTISEMENT_HEADER_LEN => {
                Some(Self::RouterAdvertisement(Icmpv6RouterAdvertisement {
                    current_hop_limit: bytes[4],
                    managed_address_configuration: bytes[5] & 0x80 != 0,
                    other_configuration: bytes[5] & 0x40 != 0,
                    router_lifetime_seconds: read_u16(bytes, 6)?,
                    reachable_time_millis: read_u32(bytes, 8)?,
                    retransmit_timer_millis: read_u32(bytes, 12)?,
                    options: NdpOptions::new(&bytes[Self::ROUTER_ADVERTISEMENT_HEADER_LEN..]),
                }))
            }
            135 if bytes.len() >= 24 => Some(Self::NeighborSolicitation {
                target: Ipv6Address::new(read_array::<16>(bytes, 8)?),
                options: NdpOptions::new(&bytes[24..]),
            }),
            136 if bytes.len() >= 24 => Some(Self::NeighborAdvertisement {
                target: Ipv6Address::new(read_array::<16>(bytes, 8)?),
                solicited: bytes[4] & 0x40 != 0,
                override_cache: bytes[4] & 0x20 != 0,
                options: NdpOptions::new(&bytes[24..]),
            }),
            _ => None,
        }
    }

    /// Encodes a Router Solicitation (RFC 4861 §4.1). `source_mac` is
    /// omitted when `source` is the unspecified address, because the
    /// Source Link-Layer Address option must not appear then.
    pub fn encode_router_solicitation(
        output: &mut [u8],
        source: Ipv6Address,
        destination: Ipv6Address,
        source_mac: Option<EthernetAddress>,
    ) -> Option<usize> {
        let len = if source_mac.is_some() {
            Self::ROUTER_SOLICITATION_LEN
        } else {
            Self::ROUTER_SOLICITATION_ANONYMOUS_LEN
        };
        if output.len() < len {
            return None;
        }
        output[..len].fill(0);
        output[0] = 133;
        if let Some(mac) = source_mac {
            output[8] = 1;
            output[9] = 1;
            output[10..16].copy_from_slice(&mac);
        }
        let checksum = icmpv6_checksum(source, destination, &output[..len]);
        write_u16(output, 2, checksum)?;
        Some(len)
    }

    /// Encodes an Echo Request (RFC 4443 §4.1) carrying `payload`.
    pub fn encode_echo_request(
        output: &mut [u8],
        source: Ipv6Address,
        destination: Ipv6Address,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
    ) -> Option<usize> {
        let len = Self::ECHO_HEADER_LEN.checked_add(payload.len())?;
        if output.len() < len {
            return None;
        }
        output[..len].fill(0);
        output[0] = 128;
        write_u16(output, 4, identifier)?;
        write_u16(output, 6, sequence)?;
        output[Self::ECHO_HEADER_LEN..len].copy_from_slice(payload);
        let checksum = icmpv6_checksum(source, destination, &output[..len]);
        write_u16(output, 2, checksum)?;
        Some(len)
    }

    pub fn encode_neighbor_solicitation(
        output: &mut [u8],
        source: Ipv6Address,
        destination: Ipv6Address,
        target: Ipv6Address,
        source_mac: EthernetAddress,
    ) -> Option<usize> {
        if output.len() < Self::NEIGHBOR_MESSAGE_LEN {
            return None;
        }
        output[..Self::NEIGHBOR_MESSAGE_LEN].fill(0);
        output[0] = 135;
        output[8..24].copy_from_slice(&target.octets());
        output[24] = 1;
        output[25] = 1;
        output[26..32].copy_from_slice(&source_mac);
        let checksum = icmpv6_checksum(source, destination, &output[..Self::NEIGHBOR_MESSAGE_LEN]);
        write_u16(output, 2, checksum)?;
        Some(Self::NEIGHBOR_MESSAGE_LEN)
    }

    pub fn encode_neighbor_advertisement(
        output: &mut [u8],
        source: Ipv6Address,
        destination: Ipv6Address,
        target: Ipv6Address,
        target_mac: EthernetAddress,
    ) -> Option<usize> {
        if output.len() < Self::NEIGHBOR_MESSAGE_LEN {
            return None;
        }
        output[..Self::NEIGHBOR_MESSAGE_LEN].fill(0);
        output[0] = 136;
        output[4] = 0x60;
        output[8..24].copy_from_slice(&target.octets());
        output[24] = 2;
        output[25] = 1;
        output[26..32].copy_from_slice(&target_mac);
        let checksum = icmpv6_checksum(source, destination, &output[..Self::NEIGHBOR_MESSAGE_LEN]);
        write_u16(output, 2, checksum)?;
        Some(Self::NEIGHBOR_MESSAGE_LEN)
    }

    pub fn encode_destination_unreachable(
        output: &mut [u8],
        source: Ipv6Address,
        destination: Ipv6Address,
        code: Icmpv6DestinationUnreachableCode,
        original: &[u8],
    ) -> Option<usize> {
        let len = Self::DESTINATION_UNREACHABLE_HEADER_LEN.checked_add(original.len())?;
        if output.len() < len {
            return None;
        }
        output[..len].fill(0);
        output[0] = 1;
        output[1] = code.raw();
        output[Self::DESTINATION_UNREACHABLE_HEADER_LEN..len].copy_from_slice(original);
        let checksum = icmpv6_checksum(source, destination, &output[..len]);
        write_u16(output, 2, checksum)?;
        Some(len)
    }

    pub fn encode_packet_too_big(
        output: &mut [u8],
        source: Ipv6Address,
        destination: Ipv6Address,
        mtu: u32,
        original: &[u8],
    ) -> Option<usize> {
        let len = Self::PACKET_TOO_BIG_HEADER_LEN.checked_add(original.len())?;
        if output.len() < len {
            return None;
        }
        output[..len].fill(0);
        output[0] = 2;
        write_u32(output, 4, mtu)?;
        output[Self::PACKET_TOO_BIG_HEADER_LEN..len].copy_from_slice(original);
        let checksum = icmpv6_checksum(source, destination, &output[..len]);
        write_u16(output, 2, checksum)?;
        Some(len)
    }
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    let end = offset.checked_add(N)?;
    bytes.get(offset..end)?.try_into().ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_options_parse_mss_sack_window_scale_and_timestamp() {
        let raw = [
            2, 4, 0x05, 0xb4, 4, 2, 3, 3, 7, 1, 8, 10, 0, 0, 0, 9, 0, 0, 0, 3, 0, 0, 0, 0,
        ];

        let options = TcpOptions::parse(&raw).expect("TCP options should parse");

        assert_eq!(options.maximum_segment_size(), Some(1460));
        assert_eq!(options.window_scale(), Some(7));
        assert!(options.sack_permitted());
        assert!(options.sack_blocks().is_empty());
        assert_eq!(
            options.timestamp(),
            Some(TcpTimestampOption {
                value: 9,
                echo_reply: 3,
            })
        );
        assert_eq!(options.raw(), raw);
    }

    #[test]
    fn tcp_encode_with_options_roundtrips_header_options_and_payload() {
        let source = IpAddress::Ipv4(Ipv4Address::new([192, 0, 2, 1]));
        let destination = IpAddress::Ipv4(Ipv4Address::new([192, 0, 2, 2]));
        let header = TcpHeader {
            source_port: 49152,
            destination_port: 80,
            sequence: 7,
            acknowledgement: 0,
            flags: TcpFlags::SYN,
            window_size: u16::MAX,
        };
        let options = TcpHeaderOptions::empty()
            .with_maximum_segment_size(1460)
            .with_window_scale(7)
            .with_sack_permitted()
            .with_timestamp(TcpTimestampOption {
                value: 11,
                echo_reply: 5,
            });
        let mut bytes = [0u8; 64];

        let len = TcpPacket::encode_with_options(
            &mut bytes,
            source,
            destination,
            header,
            options,
            b"x",
            TransportChecksum::Software,
        )
        .expect("TCP segment should fit");
        let packet = TcpPacket::parse(&bytes[..len]).expect("encoded TCP packet should parse");

        assert_eq!(packet.source_port, 49152);
        assert_eq!(packet.destination_port, 80);
        assert_eq!(packet.payload, b"x");
        assert_eq!(packet.options.maximum_segment_size(), Some(1460));
        assert_eq!(packet.options.window_scale(), Some(7));
        assert!(packet.options.sack_permitted());
        assert!(packet.options.sack_blocks().is_empty());
        assert_eq!(
            packet.options.timestamp(),
            Some(TcpTimestampOption {
                value: 11,
                echo_reply: 5,
            })
        );
    }

    #[test]
    fn tcp_sack_blocks_roundtrip_in_options() {
        let source = IpAddress::Ipv4(Ipv4Address::new([192, 0, 2, 1]));
        let destination = IpAddress::Ipv4(Ipv4Address::new([192, 0, 2, 2]));
        let header = TcpHeader {
            source_port: 49152,
            destination_port: 80,
            sequence: 7,
            acknowledgement: 101,
            flags: TcpFlags::ACK,
            window_size: u16::MAX,
        };
        let mut blocks = TcpSackBlocks::empty();
        blocks
            .push(TcpSackBlock {
                left_edge: 104,
                right_edge: 107,
            })
            .expect("single SACK block should fit");
        let mut bytes = [0u8; 64];

        let len = TcpPacket::encode_with_options(
            &mut bytes,
            source,
            destination,
            header,
            TcpHeaderOptions::empty().with_sack_blocks(blocks),
            &[],
            TransportChecksum::Software,
        )
        .expect("TCP SACK segment should fit");
        let packet = TcpPacket::parse(&bytes[..len]).expect("encoded SACK should parse");

        assert_eq!(packet.options.sack_blocks().as_slice(), blocks.as_slice());
    }

    #[test]
    fn udp_encode_transmits_zero_ipv4_checksum_as_ffff() {
        let source = IpAddress::Ipv4(Ipv4Address::new([192, 0, 2, 1]));
        let destination = IpAddress::Ipv4(Ipv4Address::new([192, 0, 2, 2]));
        let mut bytes = [0u8; 16];

        let len = UdpPacket::encode(
            &mut bytes,
            source,
            destination,
            49152,
            53,
            &[0xbb, 0xa0],
            TransportChecksum::Software,
        )
        .expect("UDP packet should fit");

        assert_eq!(len, UdpPacket::HEADER_LEN + 2);
        assert_eq!(read_u16(&bytes, 6), Some(u16::MAX));
    }

    #[test]
    fn udp_encode_transmits_zero_ipv6_checksum_as_ffff() {
        let source = IpAddress::Ipv6(Ipv6Address::new([
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ]));
        let destination = IpAddress::Ipv6(Ipv6Address::new([
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2,
        ]));
        let mut bytes = [0u8; 16];

        let len = UdpPacket::encode(
            &mut bytes,
            source,
            destination,
            49152,
            53,
            &[0xe4, 0x2f],
            TransportChecksum::Software,
        )
        .expect("UDP packet should fit");

        assert_eq!(len, UdpPacket::HEADER_LEN + 2);
        assert_eq!(read_u16(&bytes, 6), Some(u16::MAX));
    }

    #[test]
    fn ipv4_parse_exposes_fragment_metadata() {
        let source = Ipv4Address::new([192, 0, 2, 1]);
        let destination = Ipv4Address::new([192, 0, 2, 2]);
        let mut bytes = [0u8; 32];
        let header_len = Ipv4Packet::encode_header(
            &mut bytes,
            source,
            destination,
            IpProtocol::Udp,
            12,
            0x1234,
            64,
        )
        .expect("IPv4 header should fit");
        bytes[6..8].copy_from_slice(&(0x2000u16 | 2).to_be_bytes());
        bytes[10..12].copy_from_slice(&0u16.to_be_bytes());
        let checksum = ipv4_checksum(&bytes[..header_len]);
        bytes[10..12].copy_from_slice(&checksum.to_be_bytes());

        let packet = Ipv4Packet::parse(&bytes[..header_len + 12])
            .expect("fragmented IPv4 packet should parse");

        assert_eq!(packet.source, source);
        assert_eq!(packet.destination, destination);
        assert_eq!(packet.protocol, IpProtocol::Udp);
        assert_eq!(packet.identification, 0x1234);
        assert_eq!(packet.header_len, Ipv4Packet::MIN_HEADER_LEN);
        assert_eq!(packet.fragment_offset_blocks, 2);
        assert!(packet.more_fragments);
        assert!(packet.is_fragmented());
        assert_eq!(packet.payload.len(), 12);
    }
}
