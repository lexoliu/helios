use arrayvec::ArrayVec;

use crate::{EthernetAddress, Ipv4Address};

pub const DHCP_CLIENT_PORT: u16 = 68;
pub const DHCP_SERVER_PORT: u16 = 67;
pub const MAX_DHCP_DNS_SERVERS: usize = 4;

const BOOTP_HEADER_LEN: usize = 236;
const DHCP_MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];
const OPTION_SUBNET_MASK: u8 = 1;
const OPTION_ROUTER: u8 = 3;
const OPTION_DNS_SERVER: u8 = 6;
const OPTION_REQUESTED_IP: u8 = 50;
const OPTION_LEASE_TIME: u8 = 51;
const OPTION_MESSAGE_TYPE: u8 = 53;
const OPTION_SERVER_IDENTIFIER: u8 = 54;
const OPTION_PARAMETER_REQUEST_LIST: u8 = 55;
const OPTION_CLIENT_IDENTIFIER: u8 = 61;
const OPTION_END: u8 = 255;

pub type DhcpOptionBuffer = ArrayVec<u8, 312>;
pub type DhcpDnsServers = ArrayVec<Ipv4Address, MAX_DHCP_DNS_SERVERS>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DhcpMessageType {
    Discover,
    Offer,
    Request,
    Decline,
    Ack,
    Nak,
    Release,
    Inform,
}

impl DhcpMessageType {
    const fn code(self) -> u8 {
        match self {
            Self::Discover => 1,
            Self::Offer => 2,
            Self::Request => 3,
            Self::Decline => 4,
            Self::Ack => 5,
            Self::Nak => 6,
            Self::Release => 7,
            Self::Inform => 8,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Discover),
            2 => Some(Self::Offer),
            3 => Some(Self::Request),
            4 => Some(Self::Decline),
            5 => Some(Self::Ack),
            6 => Some(Self::Nak),
            7 => Some(Self::Release),
            8 => Some(Self::Inform),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DhcpClientMessage {
    pub message_type: DhcpMessageType,
    pub transaction_id: u32,
    pub client_mac: EthernetAddress,
    pub requested_ip: Option<Ipv4Address>,
    pub server_identifier: Option<Ipv4Address>,
}

impl DhcpClientMessage {
    pub fn discover(transaction_id: u32, client_mac: EthernetAddress) -> Self {
        Self {
            message_type: DhcpMessageType::Discover,
            transaction_id,
            client_mac,
            requested_ip: None,
            server_identifier: None,
        }
    }

    pub fn request(
        transaction_id: u32,
        client_mac: EthernetAddress,
        requested_ip: Ipv4Address,
        server_identifier: Ipv4Address,
    ) -> Self {
        Self {
            message_type: DhcpMessageType::Request,
            transaction_id,
            client_mac,
            requested_ip: Some(requested_ip),
            server_identifier: Some(server_identifier),
        }
    }

    pub fn encode(self, output: &mut [u8]) -> Option<usize> {
        if output.len() < BOOTP_HEADER_LEN + DHCP_MAGIC_COOKIE.len() {
            return None;
        }
        output[..BOOTP_HEADER_LEN].fill(0);
        output[0] = 1;
        output[1] = 1;
        output[2] = 6;
        output[4..8].copy_from_slice(&self.transaction_id.to_be_bytes());
        output[10..12].copy_from_slice(&0x8000u16.to_be_bytes());
        output[28..34].copy_from_slice(&self.client_mac);
        output[BOOTP_HEADER_LEN..BOOTP_HEADER_LEN + 4].copy_from_slice(&DHCP_MAGIC_COOKIE);

        let mut writer = DhcpOptionWriter::new(&mut output[BOOTP_HEADER_LEN + 4..]);
        writer.write_u8(OPTION_MESSAGE_TYPE, self.message_type.code())?;
        let mut client_identifier = [0u8; 7];
        client_identifier[0] = 1;
        client_identifier[1..].copy_from_slice(&self.client_mac);
        writer.write_bytes(OPTION_CLIENT_IDENTIFIER, &client_identifier)?;
        if let Some(requested_ip) = self.requested_ip {
            writer.write_bytes(OPTION_REQUESTED_IP, &requested_ip.octets())?;
        }
        if let Some(server_identifier) = self.server_identifier {
            writer.write_bytes(OPTION_SERVER_IDENTIFIER, &server_identifier.octets())?;
        }
        writer.write_bytes(
            OPTION_PARAMETER_REQUEST_LIST,
            &[OPTION_SUBNET_MASK, OPTION_ROUTER, OPTION_DNS_SERVER],
        )?;
        let options_len = writer.finish()?;
        Some(BOOTP_HEADER_LEN + 4 + options_len)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DhcpServerMessage {
    pub message_type: DhcpMessageType,
    pub transaction_id: u32,
    pub your_ip: Ipv4Address,
    pub server_identifier: Option<Ipv4Address>,
    pub subnet_mask: Option<Ipv4Address>,
    pub router: Option<Ipv4Address>,
    pub dns_servers: DhcpDnsServers,
    pub lease_seconds: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DhcpPacket<'a> {
    bytes: &'a [u8],
}

impl<'a> DhcpPacket<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < BOOTP_HEADER_LEN + DHCP_MAGIC_COOKIE.len()
            || bytes[0] != 2
            || bytes[1] != 1
            || bytes[2] != 6
            || bytes[BOOTP_HEADER_LEN..BOOTP_HEADER_LEN + 4] != DHCP_MAGIC_COOKIE
        {
            return None;
        }
        Some(Self { bytes })
    }

    pub fn server_message(self) -> Option<DhcpServerMessage> {
        let transaction_id = u32::from_be_bytes(read_array(self.bytes, 4)?);
        let your_ip = Ipv4Address::new(read_array(self.bytes, 16)?);
        let mut parser = DhcpOptionParser::new(&self.bytes[BOOTP_HEADER_LEN + 4..]);
        let mut message_type = None;
        let mut server_identifier = None;
        let mut subnet_mask = None;
        let mut router = None;
        let mut dns_servers = ArrayVec::new();
        let mut lease_seconds = None;

        while let Some(option) = parser.next()? {
            match option.code {
                OPTION_MESSAGE_TYPE if option.value.len() == 1 => {
                    message_type = DhcpMessageType::from_code(option.value[0]);
                }
                OPTION_SERVER_IDENTIFIER if option.value.len() == 4 => {
                    server_identifier = Some(Ipv4Address::new(option.value.try_into().ok()?));
                }
                OPTION_SUBNET_MASK if option.value.len() == 4 => {
                    subnet_mask = Some(Ipv4Address::new(option.value.try_into().ok()?));
                }
                OPTION_ROUTER if option.value.len() >= 4 => {
                    router = Some(Ipv4Address::new(option.value[..4].try_into().ok()?));
                }
                OPTION_DNS_SERVER => {
                    for chunk in option.value.chunks_exact(4).take(MAX_DHCP_DNS_SERVERS) {
                        dns_servers
                            .try_push(Ipv4Address::new(chunk.try_into().ok()?))
                            .ok()?;
                    }
                }
                OPTION_LEASE_TIME if option.value.len() == 4 => {
                    lease_seconds = Some(u32::from_be_bytes(option.value.try_into().ok()?));
                }
                _ => {}
            }
        }

        Some(DhcpServerMessage {
            message_type: message_type?,
            transaction_id,
            your_ip,
            server_identifier,
            subnet_mask,
            router,
            dns_servers,
            lease_seconds,
        })
    }
}

pub struct DhcpOptionWriter<'a> {
    output: &'a mut [u8],
    offset: usize,
}

impl<'a> DhcpOptionWriter<'a> {
    pub fn new(output: &'a mut [u8]) -> Self {
        Self { output, offset: 0 }
    }

    pub fn write_u8(&mut self, code: u8, value: u8) -> Option<()> {
        self.write_bytes(code, &[value])
    }

    pub fn write_bytes(&mut self, code: u8, value: &[u8]) -> Option<()> {
        assert!(
            value.len() <= u8::MAX as usize,
            "DHCP option value is too large"
        );
        let required = 2usize.checked_add(value.len())?;
        let end = self.offset.checked_add(required)?;
        if self.output.len() < end {
            return None;
        }
        self.output[self.offset] = code;
        self.output[self.offset + 1] = value.len() as u8;
        self.output[self.offset + 2..end].copy_from_slice(value);
        self.offset = end;
        Some(())
    }

    pub fn finish(mut self) -> Option<usize> {
        if self.output.len() <= self.offset {
            return None;
        }
        self.output[self.offset] = OPTION_END;
        self.offset += 1;
        Some(self.offset)
    }
}

#[derive(Clone, Copy)]
struct DhcpOption<'a> {
    code: u8,
    value: &'a [u8],
}

struct DhcpOptionParser<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> DhcpOptionParser<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn next(&mut self) -> Option<Option<DhcpOption<'a>>> {
        loop {
            let code = *self.bytes.get(self.offset)?;
            self.offset += 1;
            match code {
                OPTION_END => return Some(None),
                0 => continue,
                _ => {
                    let len = usize::from(*self.bytes.get(self.offset)?);
                    self.offset += 1;
                    let end = self.offset.checked_add(len)?;
                    let value = self.bytes.get(self.offset..end)?;
                    self.offset = end;
                    return Some(Some(DhcpOption { code, value }));
                }
            }
        }
    }
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    let end = offset.checked_add(N)?;
    Some(bytes.get(offset..end)?.try_into().ok()?)
}
