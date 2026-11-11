use arrayvec::ArrayVec;

use crate::{DhcpLease, EthernetAddress, Ipv4Address, Ipv4Cidr, StackInstant};

pub const DHCP_CLIENT_PORT: u16 = 68;
pub const DHCP_SERVER_PORT: u16 = 67;
pub const MAX_DHCP_DNS_SERVERS: usize = 4;

const NANOS_PER_SECOND: u64 = 1_000_000_000;
/// How long a client waits for an answer in SELECTING and REQUESTING
/// before it sends again: one second, the interval this client has
/// always retransmitted at. RFC 2131 §4.1 suggests a randomised backoff
/// that starts at four seconds and doubles to 64.
const RETRANSMIT_NANOS: u64 = NANOS_PER_SECOND;
/// RFC 2131 §4.4.5: a client in RENEWING or REBINDING waits half the
/// time left to the next deadline between requests, "down to a minimum
/// of 60 seconds".
const MIN_RENEWAL_RETRANSMIT_NANOS: u64 = 60 * NANOS_PER_SECOND;
/// How long a client that could not use the lease it was granted waits
/// before it starts over. RFC 2131 §4.4.1 has a client wait "between
/// one and ten seconds" before its first DISCOVER, and §3.1 step 5 has
/// one that gave an address back wait "a minimum of ten seconds"; the
/// client reads no randomness, so it takes the bound the two share.
const RESTART_BACKOFF_NANOS: u64 = 10 * NANOS_PER_SECOND;
/// How soon a renewal whose server's link-layer address was unresolved
/// is asked again: RFC 1122 §2.3.2.1 caps ARP requests at "1 per
/// second per destination", and the retry sends another one if the
/// first went unanswered.
const UNRESOLVED_RETRY_NANOS: u64 = NANOS_PER_SECOND;
/// RFC 2131 §3.3: a lease of 0xffffffff is infinite.
const INFINITE_LEASE_SECONDS: u32 = u32::MAX;

const BOOTP_HEADER_LEN: usize = 236;
const OPTION_RENEWAL_TIME: u8 = 58;
const OPTION_REBINDING_TIME: u8 = 59;
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
    /// `ciaddr`: the address the client already holds, filled in only
    /// while it renews or rebinds it (RFC 2131 §4.3.2), and
    /// [`Ipv4Address::UNSPECIFIED`] otherwise.
    pub client_ip: Ipv4Address,
    pub requested_ip: Option<Ipv4Address>,
    pub server_identifier: Option<Ipv4Address>,
}

impl DhcpClientMessage {
    pub fn discover(transaction_id: u32, client_mac: EthernetAddress) -> Self {
        Self {
            message_type: DhcpMessageType::Discover,
            transaction_id,
            client_mac,
            client_ip: Ipv4Address::UNSPECIFIED,
            requested_ip: None,
            server_identifier: None,
        }
    }

    /// A REQUEST in SELECTING and REQUESTING: it names the offered
    /// address and the server whose offer is taken (RFC 2131 §4.3.2).
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
            client_ip: Ipv4Address::UNSPECIFIED,
            requested_ip: Some(requested_ip),
            server_identifier: Some(server_identifier),
        }
    }

    /// A REQUEST in RENEWING and REBINDING: the address rides in
    /// `ciaddr`, and neither the server identifier nor the requested
    /// address option is filled in (RFC 2131 §4.3.2).
    pub fn renew(transaction_id: u32, client_mac: EthernetAddress, client_ip: Ipv4Address) -> Self {
        Self {
            message_type: DhcpMessageType::Request,
            transaction_id,
            client_mac,
            client_ip,
            requested_ip: None,
            server_identifier: None,
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
        // RFC 2131 §4.1: a client that cannot receive a unicast before
        // it is configured asks for a broadcast reply. One that holds an
        // address can, and its `ciaddr` is where the server answers.
        if self.client_ip.is_unspecified() {
            output[10..12].copy_from_slice(&0x8000u16.to_be_bytes());
        }
        output[12..16].copy_from_slice(&self.client_ip.octets());
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

/// A message from a server, reduced to what the client reads.
///
/// An option whose length is wrong for its type makes the whole message
/// malformed, and a malformed message never reaches this type: it is
/// not a message this client can act on, and reading a defaulted value
/// out of it would be a guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DhcpServerMessage {
    pub message_type: DhcpMessageType,
    pub transaction_id: u32,
    /// `chaddr`: the hardware address the server answers.
    pub client_mac: EthernetAddress,
    pub your_ip: Ipv4Address,
    pub server_identifier: Option<Ipv4Address>,
    pub subnet_mask: Option<Ipv4Address>,
    pub router: Option<Ipv4Address>,
    pub dns_servers: DhcpDnsServers,
    pub lease_seconds: Option<u32>,
    /// Option 58, the time to renewal.
    pub renewal_seconds: Option<u32>,
    /// Option 59, the time to rebinding.
    pub rebinding_seconds: Option<u32>,
}

impl DhcpServerMessage {
    /// Writes the message in the wire format [`DhcpPacket`] reads, the
    /// way a server would send it.
    pub fn encode(&self, output: &mut [u8]) -> Option<usize> {
        if output.len() < BOOTP_HEADER_LEN + DHCP_MAGIC_COOKIE.len() {
            return None;
        }
        output[..BOOTP_HEADER_LEN].fill(0);
        output[0] = 2;
        output[1] = 1;
        output[2] = 6;
        output[4..8].copy_from_slice(&self.transaction_id.to_be_bytes());
        output[16..20].copy_from_slice(&self.your_ip.octets());
        output[28..34].copy_from_slice(&self.client_mac);
        output[BOOTP_HEADER_LEN..BOOTP_HEADER_LEN + 4].copy_from_slice(&DHCP_MAGIC_COOKIE);

        let mut writer = DhcpOptionWriter::new(&mut output[BOOTP_HEADER_LEN + 4..]);
        writer.write_u8(OPTION_MESSAGE_TYPE, self.message_type.code())?;
        let addresses = [
            (OPTION_SERVER_IDENTIFIER, self.server_identifier),
            (OPTION_SUBNET_MASK, self.subnet_mask),
            (OPTION_ROUTER, self.router),
        ];
        for (code, address) in addresses {
            if let Some(address) = address {
                writer.write_bytes(code, &address.octets())?;
            }
        }
        if !self.dns_servers.is_empty() {
            let mut octets = ArrayVec::<u8, { MAX_DHCP_DNS_SERVERS * 4 }>::new();
            for server in &self.dns_servers {
                octets.try_extend_from_slice(&server.octets()).ok()?;
            }
            writer.write_bytes(OPTION_DNS_SERVER, &octets)?;
        }
        let durations = [
            (OPTION_LEASE_TIME, self.lease_seconds),
            (OPTION_RENEWAL_TIME, self.renewal_seconds),
            (OPTION_REBINDING_TIME, self.rebinding_seconds),
        ];
        for (code, seconds) in durations {
            if let Some(seconds) = seconds {
                writer.write_bytes(code, &seconds.to_be_bytes())?;
            }
        }
        let options_len = writer.finish()?;
        Some(BOOTP_HEADER_LEN + 4 + options_len)
    }
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

    /// Reads the message, or `None` when it is malformed: a truncated
    /// option, an unknown message type, an option of a fixed size whose
    /// length is wrong, or an address list (router, DNS servers) whose
    /// length is not a positive multiple of four (RFC 2132 §3.5, §3.8).
    ///
    /// Addresses past [`MAX_DHCP_DNS_SERVERS`] are dropped, whether they
    /// came in one DNS server option or in several.
    pub fn server_message(self) -> Option<DhcpServerMessage> {
        let transaction_id = u32::from_be_bytes(read_array(self.bytes, 4)?);
        let your_ip = Ipv4Address::new(read_array(self.bytes, 16)?);
        let client_mac = read_array(self.bytes, 28)?;
        let mut parser = DhcpOptionParser::new(&self.bytes[BOOTP_HEADER_LEN + 4..]);
        let mut message_type = None;
        let mut server_identifier = None;
        let mut subnet_mask = None;
        let mut router = None;
        let mut dns_servers = DhcpDnsServers::new();
        let mut lease_seconds = None;
        let mut renewal_seconds = None;
        let mut rebinding_seconds = None;

        while let Some(option) = parser.next()? {
            match option.code {
                OPTION_MESSAGE_TYPE => {
                    let [code] = option.value else {
                        return None;
                    };
                    message_type = DhcpMessageType::from_code(*code);
                }
                OPTION_SERVER_IDENTIFIER => server_identifier = Some(address_option(option.value)?),
                OPTION_SUBNET_MASK => subnet_mask = Some(address_option(option.value)?),
                OPTION_ROUTER => {
                    router = Some(address_list_option(option.value)?.next()?);
                }
                OPTION_DNS_SERVER => {
                    let room = dns_servers.remaining_capacity();
                    dns_servers.extend(address_list_option(option.value)?.take(room));
                }
                OPTION_LEASE_TIME => lease_seconds = Some(seconds_option(option.value)?),
                OPTION_RENEWAL_TIME => renewal_seconds = Some(seconds_option(option.value)?),
                OPTION_REBINDING_TIME => rebinding_seconds = Some(seconds_option(option.value)?),
                _ => {}
            }
        }

        Some(DhcpServerMessage {
            message_type: message_type?,
            transaction_id,
            client_mac,
            your_ip,
            server_identifier,
            subnet_mask,
            router,
            dns_servers,
            lease_seconds,
            renewal_seconds,
            rebinding_seconds,
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
    bytes.get(offset..end)?.try_into().ok()
}

fn address_option(value: &[u8]) -> Option<Ipv4Address> {
    Some(Ipv4Address::new(value.try_into().ok()?))
}

/// The addresses of an option that lists them, or `None` when its
/// length is not a positive multiple of four.
fn address_list_option(value: &[u8]) -> Option<impl Iterator<Item = Ipv4Address> + '_> {
    if value.is_empty() || !value.len().is_multiple_of(4) {
        return None;
    }
    Some(
        value
            .chunks_exact(4)
            .map(|chunk| Ipv4Address::new([chunk[0], chunk[1], chunk[2], chunk[3]])),
    )
}

fn seconds_option(value: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(value.try_into().ok()?))
}

/// The identifier of the exchange after `current`, never zero.
pub const fn next_transaction_id(current: u32) -> u32 {
    let next = current.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

/// The prefix length a subnet mask spells, or `None` when the mask is
/// not a run of ones followed by zeros, or has no ones at all.
fn mask_prefix_len(mask: Ipv4Address) -> Option<u8> {
    let raw = u32::from_be_bytes(mask.octets());
    let prefix_len = raw.leading_ones();
    if prefix_len == 0 {
        return None;
    }
    (raw == u32::MAX << (32 - prefix_len)).then_some(prefix_len as u8)
}

/// The prefix length an address's class implies, or `None` for an
/// address no server can lease to a host: `0.0.0.0`, or one of class D
/// or E.
///
/// An ACK without option 1 takes its prefix from here. RFC 2131 does not
/// say what a client does without a mask, and the class is the one
/// prefix the address itself names: RFC 791 §3.2 divides the space by
/// the leading bits of the first octet, `0` for class A with an 8-bit
/// network, `10` for class B with 16 bits and `110` for class C with 24.
/// RFC 1112 §4 gives class D (`1110`) to multicast groups and reserves
/// class E (`1111`, which holds the limited broadcast address).
fn host_class_prefix_len(address: Ipv4Address) -> Option<u8> {
    if address.is_unspecified() {
        return None;
    }
    match address.octets()[0] {
        0..=127 => Some(8),
        128..=191 => Some(16),
        192..=223 => Some(24),
        224..=255 => None,
    }
}

/// Where the client sends a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DhcpDestination {
    /// The limited broadcast address.
    Broadcast,
    /// One server's own address, which only a client that already holds
    /// a lease from it can know.
    Server(Ipv4Address),
}

/// A client message and where it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DhcpTransmit {
    pub message: DhcpClientMessage,
    pub destination: DhcpDestination,
}

/// What the caller owes the client after handing it a message or the
/// time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DhcpAction {
    /// Nothing.
    None,
    /// Put this message on the wire.
    Send(DhcpTransmit),
    /// Configure the interface with `lease`. `replaces` is the lease it
    /// supersedes when a renewal changed the prefix or the router, so
    /// the caller can take back what the new one no longer names. A
    /// caller that cannot configure it tells the client so with
    /// [`DhcpClient::lease_not_installed`].
    Install {
        lease: DhcpLease,
        replaces: Option<DhcpLease>,
    },
    /// The lease ended, by a NAK or by running out: take its address,
    /// its router and its resolvers off the interface.
    Remove(DhcpLease),
}

/// The client's position in RFC 2131 figure 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DhcpState {
    Init,
    Selecting,
    Requesting,
    Bound,
    Renewing,
    Rebinding,
}

/// The three deadlines of a finite lease, RFC 2131 §4.4.5, in the order
/// T1 < T2 < expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LeaseSchedule {
    /// T1: the client starts renewing with the server it leased from.
    renew_at: StackInstant,
    /// T2: the client asks any server.
    rebind_at: StackInstant,
    /// The server may give the address to another client.
    expires_at: StackInstant,
}

/// A lease the client holds and the server that granted it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    lease: DhcpLease,
    server: Ipv4Address,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Offer {
    address: Ipv4Address,
    server: Ipv4Address,
}

/// The requests of RENEWING, or of REBINDING, for a lease the client
/// holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Renewal {
    /// When the state's first request was sent. A lease an ACK grants
    /// runs from "the time at which the original request was sent"
    /// (RFC 2131 §4.4.1), which is never later than the request the ACK
    /// answers.
    requested_at: StackInstant,
    retransmit_at: StackInstant,
    /// Whether this exchange has spent its early retry for a server
    /// whose link-layer address was unresolved.
    resolution_retried: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Init {
        start_at: StackInstant,
    },
    Selecting {
        retransmit_at: StackInstant,
    },
    Requesting {
        offer: Offer,
        /// When the first REQUEST for `offer` was sent, which the lease
        /// an ACK grants runs from (RFC 2131 §4.4.1).
        requested_at: StackInstant,
        retransmit_at: StackInstant,
    },
    Bound {
        binding: Binding,
        /// `None` for an infinite lease, which is never renewed.
        schedule: Option<LeaseSchedule>,
    },
    Renewing {
        binding: Binding,
        schedule: LeaseSchedule,
        renewal: Renewal,
    },
    Rebinding {
        binding: Binding,
        schedule: LeaseSchedule,
        renewal: Renewal,
    },
}

/// The DHCPv4 client of RFC 2131 §4.4: INIT, SELECTING, REQUESTING,
/// BOUND, RENEWING and REBINDING.
///
/// The client does no I/O and reads no clock. It is given the current
/// time and at most one parsed server message, and answers with the one
/// thing the caller must do. After a message, and whenever
/// [`Self::next_deadline`] has passed, the caller drives it with
/// [`Self::poll`] until that answers [`DhcpAction::None`]: a step that
/// moves the client to a state that owes a message leaves that message
/// for the next call.
///
/// A message is acted on only when it is for this interface's hardware
/// address and this exchange's transaction id, and in a state that
/// waits for it (RFC 2131 §4.4.1, table 4): REQUESTING, RENEWING and
/// REBINDING take an ACK or a NAK, SELECTING an OFFER. REQUESTING and
/// RENEWING take them only from the server that was asked, and an
/// ACK or NAK in REBINDING from any server that names itself, because
/// REBINDING asks them all (§4.4.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DhcpClient {
    mac: EthernetAddress,
    transaction_id: u32,
    phase: Phase,
}

impl DhcpClient {
    /// A client in INIT that starts its first exchange under
    /// `transaction_id`.
    pub const fn new(mac: EthernetAddress, transaction_id: u32) -> Self {
        Self {
            mac,
            transaction_id,
            phase: Phase::Init {
                start_at: StackInstant::from_nanos(0),
            },
        }
    }

    /// Returns to INIT under a new transaction id, as after the link
    /// changed. The caller drops whatever the lease had configured.
    pub fn restart(&mut self, transaction_id: u32) {
        *self = Self::new(self.mac, transaction_id);
    }

    pub const fn state(&self) -> DhcpState {
        match self.phase {
            Phase::Init { .. } => DhcpState::Init,
            Phase::Selecting { .. } => DhcpState::Selecting,
            Phase::Requesting { .. } => DhcpState::Requesting,
            Phase::Bound { .. } => DhcpState::Bound,
            Phase::Renewing { .. } => DhcpState::Renewing,
            Phase::Rebinding { .. } => DhcpState::Rebinding,
        }
    }

    pub const fn transaction_id(&self) -> u32 {
        self.transaction_id
    }

    /// The lease the client holds, from the ACK that made it until it
    /// ends.
    pub fn lease(&self) -> Option<&DhcpLease> {
        match &self.phase {
            Phase::Bound { binding, .. }
            | Phase::Renewing { binding, .. }
            | Phase::Rebinding { binding, .. } => Some(&binding.lease),
            Phase::Init { .. } | Phase::Selecting { .. } | Phase::Requesting { .. } => None,
        }
    }

    /// The instant the client next wants [`Self::poll`], or `None` when
    /// it waits on nothing but a message: an infinite lease.
    pub fn next_deadline(&self) -> Option<StackInstant> {
        match &self.phase {
            Phase::Init { start_at } => Some(*start_at),
            Phase::Selecting { retransmit_at } | Phase::Requesting { retransmit_at, .. } => {
                Some(*retransmit_at)
            }
            Phase::Bound { schedule, .. } => schedule.map(|schedule| schedule.renew_at),
            Phase::Renewing {
                schedule, renewal, ..
            } => Some(renewal.retransmit_at.min(schedule.rebind_at)),
            Phase::Rebinding {
                schedule, renewal, ..
            } => Some(renewal.retransmit_at.min(schedule.expires_at)),
        }
    }

    /// Tells the client its last renewal request could not leave,
    /// because the server's link-layer address was still being resolved
    /// and an ARP request went out in its place.
    ///
    /// The first time this happens in an exchange the request is asked
    /// again [`UNRESOLVED_RETRY_NANOS`] from `now`, by when the ARP reply
    /// has normally landed, rather than the minute RFC 2131 §4.4.5 allows
    /// between requests. After that the exchange keeps the RFC's pace,
    /// so a server that never answers ARP costs one early retry per
    /// renewal, not an ARP request every second until T2.
    pub fn retry_unresolved(&mut self, now: StackInstant) {
        if let Phase::Renewing { renewal, .. } = &mut self.phase
            && !renewal.resolution_retried
        {
            renewal.resolution_retried = true;
            renewal.retransmit_at = after(now, UNRESOLVED_RETRY_NANOS);
        }
    }

    /// Tells the client that the lease the last [`DhcpAction::Install`]
    /// handed over could not be configured, and that the caller took
    /// back whatever part of it did go in.
    ///
    /// The client then holds nothing: it returns to INIT under a new
    /// transaction id and starts over [`RESTART_BACKOFF_NANOS`] from
    /// `now`.
    ///
    /// # Panics
    ///
    /// When the client is not BOUND, which is the only state an
    /// `Install` leaves it in: the caller is answering an action the
    /// client never asked for.
    pub fn lease_not_installed(&mut self, now: StackInstant) {
        assert!(
            matches!(self.phase, Phase::Bound { .. }),
            "lease_not_installed while the DHCP client is {:?}, not bound",
            self.state()
        );
        self.phase = self.restart_exchange(after(now, RESTART_BACKOFF_NANOS));
    }

    /// Runs whatever is due at `now`.
    pub fn poll(&mut self, now: StackInstant) -> DhcpAction {
        let phase = core::mem::replace(
            &mut self.phase,
            Phase::Init {
                start_at: StackInstant::from_nanos(0),
            },
        );
        let (phase, action) = self.step(phase, now);
        self.phase = phase;
        action
    }

    fn step(&mut self, phase: Phase, now: StackInstant) -> (Phase, DhcpAction) {
        match phase {
            Phase::Init { start_at } if now >= start_at => self.discover(now),
            Phase::Selecting { retransmit_at } if now >= retransmit_at => self.discover(now),
            Phase::Requesting {
                offer,
                requested_at,
                retransmit_at,
            } if now >= retransmit_at => self.request(offer, requested_at, now),
            Phase::Bound {
                binding,
                schedule: Some(schedule),
            } if now >= schedule.renew_at => {
                // RFC 2131 §4.4.5: at T1 the client starts a new exchange
                // and unicasts a REQUEST to the server it leased from.
                self.transaction_id = next_transaction_id(self.transaction_id);
                let renewal = Renewal {
                    requested_at: now,
                    retransmit_at: now,
                    resolution_retried: false,
                };
                self.step_renewing(binding, schedule, renewal, now)
            }
            Phase::Renewing {
                binding,
                schedule,
                renewal,
            } => self.step_renewing(binding, schedule, renewal, now),
            Phase::Rebinding {
                binding,
                schedule,
                renewal,
            } => self.step_rebinding(binding, schedule, renewal, now),
            phase => (phase, DhcpAction::None),
        }
    }

    fn step_renewing(
        &mut self,
        binding: Binding,
        schedule: LeaseSchedule,
        renewal: Renewal,
        now: StackInstant,
    ) -> (Phase, DhcpAction) {
        if now >= schedule.rebind_at {
            // T2 passed without an answer: the same exchange asks every
            // server.
            let renewal = Renewal {
                requested_at: now,
                retransmit_at: now,
                ..renewal
            };
            return self.step_rebinding(binding, schedule, renewal, now);
        }
        if now < renewal.retransmit_at {
            return (
                Phase::Renewing {
                    binding,
                    schedule,
                    renewal,
                },
                DhcpAction::None,
            );
        }
        let renewal = Renewal {
            retransmit_at: after(now, renewal_interval(now, schedule.rebind_at)),
            ..renewal
        };
        let action = self.renewal_request(&binding, DhcpDestination::Server(binding.server));
        (
            Phase::Renewing {
                binding,
                schedule,
                renewal,
            },
            action,
        )
    }

    fn step_rebinding(
        &mut self,
        binding: Binding,
        schedule: LeaseSchedule,
        renewal: Renewal,
        now: StackInstant,
    ) -> (Phase, DhcpAction) {
        if now >= schedule.expires_at {
            // RFC 2131 §4.4.5: at expiry the client "MUST stop using"
            // the address and "return to the INIT state".
            return (
                self.restart_exchange(now),
                DhcpAction::Remove(binding.lease),
            );
        }
        if now < renewal.retransmit_at {
            return (
                Phase::Rebinding {
                    binding,
                    schedule,
                    renewal,
                },
                DhcpAction::None,
            );
        }
        let renewal = Renewal {
            retransmit_at: after(now, renewal_interval(now, schedule.expires_at)),
            ..renewal
        };
        let action = self.renewal_request(&binding, DhcpDestination::Broadcast);
        (
            Phase::Rebinding {
                binding,
                schedule,
                renewal,
            },
            action,
        )
    }

    /// INIT under a fresh transaction id, starting at `start_at`.
    fn restart_exchange(&mut self, start_at: StackInstant) -> Phase {
        self.transaction_id = next_transaction_id(self.transaction_id);
        Phase::Init { start_at }
    }

    fn discover(&self, now: StackInstant) -> (Phase, DhcpAction) {
        (
            Phase::Selecting {
                retransmit_at: after(now, RETRANSMIT_NANOS),
            },
            broadcast(DhcpClientMessage::discover(self.transaction_id, self.mac)),
        )
    }

    /// Asks for `offer`, in an exchange whose first REQUEST went out at
    /// `requested_at`.
    fn request(
        &self,
        offer: Offer,
        requested_at: StackInstant,
        now: StackInstant,
    ) -> (Phase, DhcpAction) {
        let message =
            DhcpClientMessage::request(self.transaction_id, self.mac, offer.address, offer.server);
        (
            Phase::Requesting {
                offer,
                requested_at,
                retransmit_at: after(now, RETRANSMIT_NANOS),
            },
            broadcast(message),
        )
    }

    fn renewal_request(&self, binding: &Binding, destination: DhcpDestination) -> DhcpAction {
        DhcpAction::Send(DhcpTransmit {
            message: DhcpClientMessage::renew(
                self.transaction_id,
                self.mac,
                binding.lease.address.address(),
            ),
            destination,
        })
    }

    /// Takes one server message at `now`.
    pub fn on_message(&mut self, now: StackInstant, message: &DhcpServerMessage) -> DhcpAction {
        if message.client_mac != self.mac || message.transaction_id != self.transaction_id {
            tracing::debug!(
                transaction_id = message.transaction_id,
                "dropped DHCP message for another client or exchange"
            );
            return DhcpAction::None;
        }
        let phase = core::mem::replace(
            &mut self.phase,
            Phase::Init {
                start_at: StackInstant::from_nanos(0),
            },
        );
        let (phase, action) = self.accept(phase, now, message);
        self.phase = phase;
        action
    }

    fn accept(
        &mut self,
        phase: Phase,
        now: StackInstant,
        message: &DhcpServerMessage,
    ) -> (Phase, DhcpAction) {
        match (phase, message.message_type) {
            (Phase::Selecting { retransmit_at }, DhcpMessageType::Offer) => {
                let Some(server) = message.server_identifier else {
                    tracing::debug!("dropped DHCP offer without a server identifier");
                    return (Phase::Selecting { retransmit_at }, DhcpAction::None);
                };
                if host_class_prefix_len(message.your_ip).is_none() {
                    tracing::debug!(your_ip = ?message.your_ip, "dropped DHCP offer of an address no host can hold");
                    return (Phase::Selecting { retransmit_at }, DhcpAction::None);
                }
                let offer = Offer {
                    address: message.your_ip,
                    server,
                };
                self.request(offer, now, now)
            }
            (
                Phase::Requesting {
                    offer,
                    requested_at,
                    retransmit_at,
                },
                DhcpMessageType::Ack,
            ) if message.server_identifier == Some(offer.server) => {
                match lease_terms(message, requested_at) {
                    Ok((binding, schedule)) => {
                        let lease = binding.lease.clone();
                        (
                            Phase::Bound { binding, schedule },
                            DhcpAction::Install {
                                lease,
                                replaces: None,
                            },
                        )
                    }
                    Err(reason) => {
                        tracing::warn!(reason, "refused a DHCP ack");
                        (
                            Phase::Requesting {
                                offer,
                                requested_at,
                                retransmit_at,
                            },
                            DhcpAction::None,
                        )
                    }
                }
            }
            (Phase::Requesting { offer, .. }, DhcpMessageType::Nak)
                if message.server_identifier == Some(offer.server) =>
            {
                (self.restart_exchange(now), DhcpAction::None)
            }
            (
                Phase::Renewing {
                    binding,
                    schedule,
                    renewal,
                },
                kind @ (DhcpMessageType::Ack | DhcpMessageType::Nak),
            ) if message.server_identifier == Some(binding.server) => self.answer_to_renewal(
                binding,
                kind,
                message,
                now,
                renewal.requested_at,
                |binding| Phase::Renewing {
                    binding,
                    schedule,
                    renewal,
                },
            ),
            (
                Phase::Rebinding {
                    binding,
                    schedule,
                    renewal,
                },
                kind @ (DhcpMessageType::Ack | DhcpMessageType::Nak),
            ) if message.server_identifier.is_some() => self.answer_to_renewal(
                binding,
                kind,
                message,
                now,
                renewal.requested_at,
                |binding| Phase::Rebinding {
                    binding,
                    schedule,
                    renewal,
                },
            ),
            (phase, message_type) => {
                tracing::debug!(
                    ?message_type,
                    "dropped DHCP message that is not a reply to the outstanding exchange"
                );
                (phase, DhcpAction::None)
            }
        }
    }

    /// An ACK or NAK in RENEWING or REBINDING, from a server that may
    /// answer, to requests the first of which went out at
    /// `requested_at`. `stay` rebuilds the phase the client is in, for
    /// an ACK that has to be refused.
    fn answer_to_renewal(
        &mut self,
        binding: Binding,
        kind: DhcpMessageType,
        message: &DhcpServerMessage,
        now: StackInstant,
        requested_at: StackInstant,
        stay: impl FnOnce(Binding) -> Phase,
    ) -> (Phase, DhcpAction) {
        if kind == DhcpMessageType::Nak {
            // RFC 2131 §4.4.5: the lease is gone, and the client
            // "SHOULD ... return to the INIT state".
            let phase = self.restart_exchange(now);
            return (phase, DhcpAction::Remove(binding.lease));
        }
        let (renewed, schedule) = match lease_terms(message, requested_at) {
            Ok((renewed, schedule))
                if renewed.lease.address.address() == binding.lease.address.address() =>
            {
                (renewed, schedule)
            }
            Ok(_) => {
                tracing::warn!("refused a DHCP ack that changes the leased address");
                return (stay(binding), DhcpAction::None);
            }
            Err(reason) => {
                tracing::warn!(reason, "refused a DHCP ack");
                return (stay(binding), DhcpAction::None);
            }
        };
        let superseded = binding.lease.address != renewed.lease.address
            || binding.lease.router != renewed.lease.router;
        let lease = renewed.lease.clone();
        (
            Phase::Bound {
                binding: renewed,
                schedule,
            },
            DhcpAction::Install {
                lease,
                replaces: superseded.then_some(binding.lease),
            },
        )
    }
}

fn broadcast(message: DhcpClientMessage) -> DhcpAction {
    DhcpAction::Send(DhcpTransmit {
        message,
        destination: DhcpDestination::Broadcast,
    })
}

fn after(now: StackInstant, nanos: u64) -> StackInstant {
    StackInstant::from_nanos(now.nanos().saturating_add(nanos))
}

/// RFC 2131 §4.4.5: "wait one-half of the remaining time until T2 (in
/// RENEWING state) and one-half of the remaining lease time (in
/// REBINDING state), down to a minimum of 60 seconds".
fn renewal_interval(now: StackInstant, until: StackInstant) -> u64 {
    (until.nanos().saturating_sub(now.nanos()) / 2).max(MIN_RENEWAL_RETRANSMIT_NANOS)
}

/// Reads the lease an ACK grants, running from `requested_at`, or the
/// reason it grants none: the ACK is then refused for this exchange.
fn lease_terms(
    message: &DhcpServerMessage,
    requested_at: StackInstant,
) -> Result<(Binding, Option<LeaseSchedule>), &'static str> {
    let server = message.server_identifier.ok_or("no server identifier")?;
    let class_prefix_len =
        host_class_prefix_len(message.your_ip).ok_or("your-address is no host's address")?;
    let lease_seconds = message.lease_seconds.ok_or("no lease time")?;
    if lease_seconds == 0 {
        return Err("zero lease time");
    }
    let prefix_len = match message.subnet_mask {
        Some(mask) => mask_prefix_len(mask).ok_or("subnet mask is not a prefix")?,
        None => {
            tracing::debug!(
                address = ?message.your_ip,
                prefix_len = class_prefix_len,
                "DHCP ack without a subnet mask: prefix taken from the address class"
            );
            class_prefix_len
        }
    };
    let schedule = lease_schedule(message, lease_seconds, requested_at);
    let binding = Binding {
        lease: DhcpLease {
            address: Ipv4Cidr::new(message.your_ip, prefix_len),
            router: message.router,
            dns_servers: message.dns_servers.clone(),
            expires_at: schedule.map(|schedule| schedule.expires_at),
        },
        server,
    };
    Ok((binding, schedule))
}

/// T1, T2 and expiry for a lease of `lease_seconds` that runs from
/// `requested_at`, or `None` for an infinite lease.
///
/// T1 and T2 are options 58 and 59 when the server sent them and they
/// keep T1 < T2 < lease, and otherwise RFC 2131 §4.4.5's defaults of
/// 0.5 and 0.875 of the lease. A zero T1 or T2 counts as absent. This is
/// the order systemd-networkd's client checks them in: T2 first against
/// the lease, then T1 against T2, and T2 back to its default if T1 had
/// to take its own. Times are kept in nanoseconds so a one-second lease
/// still renews half a second in, rather than at once and again on
/// every ACK.
fn lease_schedule(
    message: &DhcpServerMessage,
    lease_seconds: u32,
    requested_at: StackInstant,
) -> Option<LeaseSchedule> {
    if lease_seconds == INFINITE_LEASE_SECONDS {
        return None;
    }
    // At most `u32::MAX - 1` seconds, which fits in nanoseconds.
    let lease = u64::from(lease_seconds) * NANOS_PER_SECOND;
    let default_renew = lease / 2;
    // Seven eighths, without the overflow `lease * 7` would risk.
    let default_rebind = lease - lease / 8;
    let given = |seconds: Option<u32>| {
        seconds
            .filter(|&seconds| seconds != 0)
            .map(|seconds| u64::from(seconds) * NANOS_PER_SECOND)
    };
    let given_renew = given(message.renewal_seconds);
    let given_rebind = given(message.rebinding_seconds);
    let rebind = given_rebind
        .filter(|&rebind| rebind < lease)
        .unwrap_or(default_rebind);
    let renew = given_renew
        .filter(|&renew| renew < rebind)
        .unwrap_or(default_renew);
    let rebind = if renew < rebind {
        rebind
    } else {
        default_rebind
    };
    if given_renew.is_some_and(|given| given != renew)
        || given_rebind.is_some_and(|given| given != rebind)
    {
        tracing::warn!(
            lease_seconds,
            renewal_seconds = message.renewal_seconds,
            rebinding_seconds = message.rebinding_seconds,
            "DHCP lease timers are not T1 < T2 < lease: using the RFC 2131 defaults"
        );
    }
    let at = |offset: u64| after(requested_at, offset);
    Some(LeaseSchedule {
        renew_at: at(renew),
        rebind_at: at(rebind),
        expires_at: at(lease),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: EthernetAddress = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const OTHER_MAC: EthernetAddress = [0x52, 0x54, 0x00, 0x65, 0x43, 0x21];
    const SERVER: Ipv4Address = Ipv4Address::new([192, 0, 2, 1]);
    const OTHER_SERVER: Ipv4Address = Ipv4Address::new([192, 0, 2, 2]);
    const ADDRESS: Ipv4Address = Ipv4Address::new([192, 0, 2, 100]);
    const ROUTER: Ipv4Address = Ipv4Address::new([192, 0, 2, 254]);
    const DNS: Ipv4Address = Ipv4Address::new([192, 0, 2, 53]);
    const MASK: Ipv4Address = Ipv4Address::new([255, 255, 255, 0]);
    const TRANSACTION: u32 = 7;
    const LEASE_SECONDS: u32 = 3600;

    fn at(seconds: u64) -> StackInstant {
        StackInstant::from_nanos(seconds * NANOS_PER_SECOND)
    }

    fn reply(
        client: &DhcpClient,
        message_type: DhcpMessageType,
        server: Ipv4Address,
    ) -> DhcpServerMessage {
        let mut dns_servers = DhcpDnsServers::new();
        dns_servers.push(DNS);
        DhcpServerMessage {
            message_type,
            transaction_id: client.transaction_id(),
            client_mac: MAC,
            your_ip: if message_type == DhcpMessageType::Nak {
                Ipv4Address::UNSPECIFIED
            } else {
                ADDRESS
            },
            server_identifier: Some(server),
            subnet_mask: (message_type != DhcpMessageType::Nak).then_some(MASK),
            router: (message_type != DhcpMessageType::Nak).then_some(ROUTER),
            dns_servers,
            lease_seconds: (message_type != DhcpMessageType::Nak).then_some(LEASE_SECONDS),
            renewal_seconds: None,
            rebinding_seconds: None,
        }
    }

    fn expected_lease(expires_at: Option<StackInstant>) -> DhcpLease {
        let mut dns_servers = DhcpDnsServers::new();
        dns_servers.push(DNS);
        DhcpLease {
            address: Ipv4Cidr::new(ADDRESS, 24),
            router: Some(ROUTER),
            dns_servers,
            expires_at,
        }
    }

    fn sent(action: DhcpAction) -> DhcpTransmit {
        match action {
            DhcpAction::Send(transmit) => transmit,
            other => panic!("expected a message to send, got {other:?}"),
        }
    }

    fn installed(action: DhcpAction) -> (DhcpLease, Option<DhcpLease>) {
        match action {
            DhcpAction::Install { lease, replaces } => (lease, replaces),
            other => panic!("expected a lease to install, got {other:?}"),
        }
    }

    /// A client that sent its DISCOVER at 0 s and was offered `address`
    /// by [`SERVER`].
    fn requesting_for(address: Ipv4Address) -> DhcpClient {
        let mut client = DhcpClient::new(MAC, TRANSACTION);
        sent(client.poll(at(0)));
        let mut offer = reply(&client, DhcpMessageType::Offer, SERVER);
        offer.your_ip = address;
        sent(client.on_message(at(0), &offer));
        assert_eq!(client.state(), DhcpState::Requesting);
        client
    }

    /// A client that sent its DISCOVER at 0 s and was offered
    /// [`ADDRESS`] by [`SERVER`].
    fn requesting() -> DhcpClient {
        requesting_for(ADDRESS)
    }

    /// A client whose ACK arrived at 0 s.
    fn bound(ack: impl FnOnce(&mut DhcpServerMessage)) -> DhcpClient {
        let mut client = requesting();
        let mut message = reply(&client, DhcpMessageType::Ack, SERVER);
        ack(&mut message);
        installed(client.on_message(at(0), &message));
        assert_eq!(client.state(), DhcpState::Bound);
        client
    }

    fn renewing() -> DhcpClient {
        let mut client = bound(|_| {});
        sent(client.poll(at(1800)));
        assert_eq!(client.state(), DhcpState::Renewing);
        client
    }

    fn rebinding() -> DhcpClient {
        let mut client = renewing();
        sent(client.poll(at(3150)));
        assert_eq!(client.state(), DhcpState::Rebinding);
        client
    }

    #[test]
    fn a_full_exchange_binds_the_offered_address() {
        let mut client = DhcpClient::new(MAC, TRANSACTION);
        assert_eq!(client.state(), DhcpState::Init);

        let discover = sent(client.poll(at(0)));
        assert_eq!(discover.destination, DhcpDestination::Broadcast);
        assert_eq!(
            discover.message,
            DhcpClientMessage::discover(TRANSACTION, MAC)
        );
        assert_eq!(client.state(), DhcpState::Selecting);
        assert_eq!(client.poll(at(0)), DhcpAction::None);

        let offer = reply(&client, DhcpMessageType::Offer, SERVER);
        let request = sent(client.on_message(at(0), &offer));
        assert_eq!(request.destination, DhcpDestination::Broadcast);
        assert_eq!(
            request.message,
            DhcpClientMessage::request(TRANSACTION, MAC, ADDRESS, SERVER)
        );
        assert_eq!(client.state(), DhcpState::Requesting);

        let ack = reply(&client, DhcpMessageType::Ack, SERVER);
        let (lease, replaces) = installed(client.on_message(at(0), &ack));
        assert_eq!(lease, expected_lease(Some(at(3600))));
        assert_eq!(replaces, None);
        assert_eq!(client.state(), DhcpState::Bound);
        assert_eq!(client.lease(), Some(&lease));
        assert_eq!(client.next_deadline(), Some(at(1800)));
    }

    #[test]
    fn discover_and_request_are_retransmitted_every_second() {
        let mut client = DhcpClient::new(MAC, TRANSACTION);
        sent(client.poll(at(0)));
        assert_eq!(client.next_deadline(), Some(at(1)));
        assert_eq!(
            client.poll(StackInstant::from_nanos(999_999_999)),
            DhcpAction::None
        );
        let again = sent(client.poll(at(1)));
        assert_eq!(again.message.message_type, DhcpMessageType::Discover);

        let offer = reply(&client, DhcpMessageType::Offer, SERVER);
        sent(client.on_message(at(2), &offer));
        assert_eq!(client.poll(at(2)), DhcpAction::None);
        let again = sent(client.poll(at(3)));
        assert_eq!(again.message.message_type, DhcpMessageType::Request);
        assert_eq!(again.message.requested_ip, Some(ADDRESS));
        assert_eq!(again.message.server_identifier, Some(SERVER));
    }

    /// RFC 2131 §4.4.1: the lease runs from when the first REQUEST was
    /// sent, not from when its ACK was read.
    #[test]
    fn a_lease_runs_from_the_first_request_of_the_exchange() {
        let mut client = requesting();
        sent(client.poll(at(1)));
        let ack = reply(&client, DhcpMessageType::Ack, SERVER);

        let (lease, _) = installed(client.on_message(at(5), &ack));

        assert_eq!(lease.expires_at, Some(at(3600)));
        assert_eq!(client.next_deadline(), Some(at(1800)));
    }

    #[test]
    fn an_offer_without_a_server_identifier_is_ignored() {
        let mut client = DhcpClient::new(MAC, TRANSACTION);
        sent(client.poll(at(0)));
        let before = client.clone();
        let mut offer = reply(&client, DhcpMessageType::Offer, SERVER);
        offer.server_identifier = None;

        assert_eq!(client.on_message(at(0), &offer), DhcpAction::None);

        assert_eq!(client, before);
    }

    #[test]
    fn an_offer_of_an_address_no_host_can_hold_is_ignored() {
        for address in [
            Ipv4Address::UNSPECIFIED,
            Ipv4Address::new([224, 0, 0, 1]),
            Ipv4Address::new([240, 0, 0, 1]),
            Ipv4Address::BROADCAST,
        ] {
            let mut client = DhcpClient::new(MAC, TRANSACTION);
            sent(client.poll(at(0)));
            let before = client.clone();
            let mut offer = reply(&client, DhcpMessageType::Offer, SERVER);
            offer.your_ip = address;

            assert_eq!(client.on_message(at(0), &offer), DhcpAction::None);

            assert_eq!(client, before, "{address:?}");
        }
    }

    #[test]
    fn the_lease_is_renewed_at_t1_by_unicast_to_its_server() {
        let mut client = bound(|_| {});
        assert_eq!(client.poll(at(1799)), DhcpAction::None);

        let request = sent(client.poll(at(1800)));

        assert_eq!(request.destination, DhcpDestination::Server(SERVER));
        assert_eq!(
            request.message,
            DhcpClientMessage::renew(client.transaction_id(), MAC, ADDRESS)
        );
        assert_ne!(client.transaction_id(), TRANSACTION);
        assert_eq!(client.state(), DhcpState::Renewing);
        // The address is still configured while the renewal is asked.
        assert!(client.lease().is_some());

        let mut ack = reply(&client, DhcpMessageType::Ack, SERVER);
        ack.lease_seconds = Some(7200);
        let (lease, replaces) = installed(client.on_message(at(1801), &ack));
        // The renewed lease runs from the request at 1800 s.
        assert_eq!(lease, expected_lease(Some(at(1800 + 7200))));
        assert_eq!(replaces, None);
        assert_eq!(client.state(), DhcpState::Bound);
        assert_eq!(client.next_deadline(), Some(at(1800 + 3600)));
    }

    #[test]
    fn a_renewal_is_retransmitted_after_half_the_time_to_t2() {
        let mut client = bound(|_| {});
        sent(client.poll(at(1800)));
        // T2 is 3150 s: half of the 1350 s left is 675 s.
        assert_eq!(client.next_deadline(), Some(at(1800 + 675)));
        assert_eq!(client.poll(at(2474)), DhcpAction::None);
        let again = sent(client.poll(at(2475)));
        assert_eq!(again.destination, DhcpDestination::Server(SERVER));
        // 3150 - 2475 = 675 s left, so 337.5 s.
        assert_eq!(
            client.next_deadline(),
            Some(StackInstant::from_nanos(
                2475 * NANOS_PER_SECOND + 337_500_000_000
            ))
        );
    }

    #[test]
    fn a_retransmission_interval_never_drops_below_a_minute() {
        let now = at(100);
        assert_eq!(renewal_interval(now, at(110)), 60 * NANOS_PER_SECOND);
        assert_eq!(renewal_interval(now, at(100)), 60 * NANOS_PER_SECOND);
        assert_eq!(renewal_interval(now, at(1100)), 500 * NANOS_PER_SECOND);
    }

    #[test]
    fn an_unresolved_renewal_is_asked_again_soon_once_per_exchange() {
        let mut client = bound(|_| {});
        sent(client.poll(at(1800)));

        client.retry_unresolved(at(1800));

        assert_eq!(client.next_deadline(), Some(at(1801)));
        let again = sent(client.poll(at(1801)));
        assert_eq!(again.destination, DhcpDestination::Server(SERVER));

        // The server still did not answer ARP: the exchange goes back
        // to RFC 2131's pace, half of the 1349 s left to T2.
        client.retry_unresolved(at(1801));

        assert_eq!(
            client.next_deadline(),
            Some(StackInstant::from_nanos(
                1801 * NANOS_PER_SECOND + 674_500_000_000
            ))
        );
    }

    #[test]
    fn the_lease_is_rebound_at_t2_by_broadcast_in_the_same_exchange() {
        let mut client = renewing();
        let transaction = client.transaction_id();

        let request = sent(client.poll(at(3150)));

        assert_eq!(request.destination, DhcpDestination::Broadcast);
        assert_eq!(
            request.message,
            DhcpClientMessage::renew(transaction, MAC, ADDRESS)
        );
        assert_eq!(client.state(), DhcpState::Rebinding);
        assert_eq!(client.transaction_id(), transaction);

        // Any server may answer, and the lease is then theirs to renew.
        let ack = reply(&client, DhcpMessageType::Ack, OTHER_SERVER);
        let (lease, replaces) = installed(client.on_message(at(3151), &ack));
        assert_eq!(lease.address, Ipv4Cidr::new(ADDRESS, 24));
        // It runs from the first rebinding request, at 3150 s.
        assert_eq!(lease.expires_at, Some(at(3150 + 3600)));
        assert_eq!(replaces, None);
        assert_eq!(client.state(), DhcpState::Bound);
        let next = sent(client.poll(at(3150 + 1800)));
        assert_eq!(next.destination, DhcpDestination::Server(OTHER_SERVER));
    }

    #[test]
    fn a_rebinding_request_is_retransmitted_after_half_the_remaining_lease() {
        let mut client = rebinding();
        // 3600 - 3150 = 450 s left.
        assert_eq!(client.next_deadline(), Some(at(3150 + 225)));
        let again = sent(client.poll(at(3150 + 225)));
        assert_eq!(again.destination, DhcpDestination::Broadcast);
    }

    #[test]
    fn a_lease_that_runs_out_is_removed_and_the_client_starts_over() {
        let mut client = rebinding();
        let transaction = client.transaction_id();
        // The rebinding request goes out again at 3375 s, and the lease
        // is still the client's until 3600 s.
        let again = sent(client.poll(at(3599)));
        assert_eq!(again.destination, DhcpDestination::Broadcast);
        assert_eq!(client.state(), DhcpState::Rebinding);

        let action = client.poll(at(3600));

        assert_eq!(action, DhcpAction::Remove(expected_lease(Some(at(3600)))));
        assert_eq!(client.state(), DhcpState::Init);
        assert_eq!(client.lease(), None);
        assert_ne!(client.transaction_id(), transaction);
        let discover = sent(client.poll(at(3600)));
        assert_eq!(discover.message.message_type, DhcpMessageType::Discover);
        assert_eq!(client.state(), DhcpState::Selecting);
    }

    #[test]
    fn a_lease_found_expired_in_bound_is_removed() {
        let mut client = bound(|_| {});

        let action = client.poll(at(4000));

        assert_eq!(action, DhcpAction::Remove(expected_lease(Some(at(3600)))));
        assert_eq!(client.state(), DhcpState::Init);
    }

    #[test]
    fn a_nak_while_renewing_removes_the_lease() {
        let mut client = renewing();
        let transaction = client.transaction_id();

        // Only the server that was asked may refuse the renewal.
        let stranger = reply(&client, DhcpMessageType::Nak, OTHER_SERVER);
        assert_eq!(client.on_message(at(1900), &stranger), DhcpAction::None);
        assert_eq!(client.state(), DhcpState::Renewing);

        let nak = reply(&client, DhcpMessageType::Nak, SERVER);
        let action = client.on_message(at(1900), &nak);

        assert_eq!(action, DhcpAction::Remove(expected_lease(Some(at(3600)))));
        assert_eq!(client.state(), DhcpState::Init);
        assert_ne!(client.transaction_id(), transaction);
        assert_eq!(client.next_deadline(), Some(at(1900)));
    }

    #[test]
    fn a_nak_while_rebinding_removes_the_lease_whichever_server_sends_it() {
        let mut client = rebinding();
        let nak = reply(&client, DhcpMessageType::Nak, OTHER_SERVER);

        let action = client.on_message(at(3200), &nak);

        assert_eq!(action, DhcpAction::Remove(expected_lease(Some(at(3600)))));
        assert_eq!(client.state(), DhcpState::Init);
    }

    #[test]
    fn a_nak_while_requesting_restarts_discovery_with_nothing_to_remove() {
        let mut client = requesting();
        let nak = reply(&client, DhcpMessageType::Nak, SERVER);

        assert_eq!(client.on_message(at(1), &nak), DhcpAction::None);

        assert_eq!(client.state(), DhcpState::Init);
        assert_ne!(client.transaction_id(), TRANSACTION);
        let discover = sent(client.poll(at(1)));
        assert_eq!(discover.message.message_type, DhcpMessageType::Discover);
    }

    /// RFC 2131 §4.4.1 (table 4) and #523: a NAK counts only in a state
    /// that waits for an answer, for that exchange's transaction id,
    /// and from a server that names itself and may answer.
    #[test]
    fn a_nak_is_taken_only_as_an_answer_to_the_outstanding_request() {
        let ignored = |client: DhcpClient, nak: &dyn Fn(&mut DhcpServerMessage)| {
            let mut probe = client.clone();
            let mut message = reply(&probe, DhcpMessageType::Nak, SERVER);
            nak(&mut message);

            assert_eq!(probe.on_message(at(100), &message), DhcpAction::None);

            assert_eq!(probe, client, "{message:?}");
        };
        ignored(requesting(), &|nak| nak.transaction_id = TRANSACTION + 1);
        ignored(requesting(), &|nak| {
            nak.server_identifier = Some(OTHER_SERVER)
        });
        for client in [requesting(), renewing(), rebinding()] {
            ignored(client, &|nak| nak.server_identifier = None);
        }
        let selecting = {
            let mut client = DhcpClient::new(MAC, TRANSACTION);
            sent(client.poll(at(0)));
            client
        };
        for client in [DhcpClient::new(MAC, TRANSACTION), selecting, bound(|_| {})] {
            ignored(client, &|_| {});
        }
    }

    #[test]
    fn a_reply_for_another_hardware_address_is_ignored_in_every_state() {
        let clients = [
            DhcpClient::new(MAC, TRANSACTION),
            {
                let mut client = DhcpClient::new(MAC, TRANSACTION);
                sent(client.poll(at(0)));
                client
            },
            requesting(),
            bound(|_| {}),
            renewing(),
            rebinding(),
        ];
        for client in clients {
            for message_type in [
                DhcpMessageType::Offer,
                DhcpMessageType::Ack,
                DhcpMessageType::Nak,
            ] {
                let mut probe = client.clone();
                let mut message = reply(&probe, message_type, SERVER);
                message.client_mac = OTHER_MAC;

                let action = probe.on_message(at(100), &message);

                assert_eq!(action, DhcpAction::None, "{:?}", client.state());
                assert_eq!(probe, client, "{:?} {message_type:?}", client.state());
            }
        }
    }

    #[test]
    fn a_reply_for_another_exchange_is_ignored() {
        let mut client = requesting();
        let before = client.clone();
        let mut ack = reply(&client, DhcpMessageType::Ack, SERVER);
        ack.transaction_id = TRANSACTION + 1;

        assert_eq!(client.on_message(at(0), &ack), DhcpAction::None);

        assert_eq!(client, before);
    }

    #[test]
    fn an_ack_from_a_server_that_was_not_asked_is_ignored_while_requesting() {
        let mut client = requesting();
        let before = client.clone();
        let ack = reply(&client, DhcpMessageType::Ack, OTHER_SERVER);

        assert_eq!(client.on_message(at(0), &ack), DhcpAction::None);

        assert_eq!(client, before);
    }

    /// RFC 791 §3.2: with no option 1, class A gives /8, class B /16
    /// and class C /24.
    #[test]
    fn an_ack_without_a_subnet_mask_takes_the_prefix_of_the_address_class() {
        for (address, prefix_len) in [
            (Ipv4Address::new([10, 1, 2, 3]), 8),
            (Ipv4Address::new([126, 255, 0, 9]), 8),
            (Ipv4Address::new([128, 0, 4, 5]), 16),
            (Ipv4Address::new([191, 255, 4, 5]), 16),
            (Ipv4Address::new([192, 0, 2, 100]), 24),
            (Ipv4Address::new([223, 255, 255, 1]), 24),
        ] {
            let mut client = requesting_for(address);
            let mut ack = reply(&client, DhcpMessageType::Ack, SERVER);
            ack.your_ip = address;
            ack.subnet_mask = None;

            let (lease, _) = installed(client.on_message(at(0), &ack));

            assert_eq!(lease.address, Ipv4Cidr::new(address, prefix_len));
            assert_eq!(client.state(), DhcpState::Bound);
        }
    }

    #[test]
    fn a_renewal_ack_without_a_subnet_mask_keeps_the_lease_on_the_derived_prefix() {
        for mut client in [renewing(), rebinding()] {
            let mut ack = reply(&client, DhcpMessageType::Ack, SERVER);
            ack.subnet_mask = None;

            let (lease, replaces) = installed(client.on_message(at(3200), &ack));

            // 192.0.2.100 is class C, the /24 the lease already had.
            assert_eq!(lease.address, Ipv4Cidr::new(ADDRESS, 24));
            assert_eq!(replaces, None);
        }
    }

    #[test]
    fn an_ack_without_a_lease_time_is_refused_for_the_exchange() {
        let mut client = requesting();
        let mut ack = reply(&client, DhcpMessageType::Ack, SERVER);
        ack.lease_seconds = None;
        let before = client.clone();

        assert_eq!(client.on_message(at(0), &ack), DhcpAction::None);

        assert_eq!(client, before);
        // The retransmission timer runs on, and a good ACK still binds.
        let again = sent(client.poll(at(1)));
        assert_eq!(again.message.message_type, DhcpMessageType::Request);
        let ack = reply(&client, DhcpMessageType::Ack, SERVER);
        installed(client.on_message(at(1), &ack));
        assert_eq!(client.state(), DhcpState::Bound);
    }

    #[test]
    fn an_ack_with_a_malformed_field_is_refused_for_the_exchange() {
        let malformed: [fn(&mut DhcpServerMessage); 9] = [
            |ack| ack.server_identifier = None,
            |ack| ack.your_ip = Ipv4Address::UNSPECIFIED,
            |ack| ack.your_ip = Ipv4Address::BROADCAST,
            |ack| ack.your_ip = Ipv4Address::new([224, 0, 0, 5]),
            |ack| ack.your_ip = Ipv4Address::new([240, 0, 0, 1]),
            |ack| {
                ack.your_ip = Ipv4Address::new([239, 1, 2, 3]);
                ack.subnet_mask = None;
            },
            |ack| ack.subnet_mask = Some(Ipv4Address::new([255, 0, 255, 0])),
            |ack| ack.subnet_mask = Some(Ipv4Address::UNSPECIFIED),
            |ack| ack.lease_seconds = Some(0),
        ];
        for corrupt in malformed {
            let mut client = requesting();
            let before = client.clone();
            let mut ack = reply(&client, DhcpMessageType::Ack, SERVER);
            corrupt(&mut ack);

            assert_eq!(client.on_message(at(0), &ack), DhcpAction::None);

            assert_eq!(client, before, "{ack:?}");
        }
    }

    #[test]
    fn a_renewal_ack_that_lacks_a_field_leaves_the_lease_running() {
        let faults: [fn(&mut DhcpServerMessage); 3] = [
            |ack| ack.lease_seconds = None,
            |ack| ack.your_ip = Ipv4Address::new([192, 0, 2, 101]),
            |ack| ack.your_ip = Ipv4Address::new([240, 0, 0, 1]),
        ];
        for fault in faults {
            for mut client in [renewing(), rebinding()] {
                let before = client.clone();
                let mut ack = reply(&client, DhcpMessageType::Ack, SERVER);
                fault(&mut ack);

                assert_eq!(client.on_message(at(3200), &ack), DhcpAction::None);

                assert_eq!(client, before);
            }
        }
    }

    #[test]
    fn a_renewal_ack_that_moves_the_router_names_the_lease_it_replaces() {
        let mut client = renewing();
        let mut ack = reply(&client, DhcpMessageType::Ack, SERVER);
        ack.router = Some(Ipv4Address::new([192, 0, 2, 253]));

        let (lease, replaces) = installed(client.on_message(at(1900), &ack));

        assert_eq!(lease.router, Some(Ipv4Address::new([192, 0, 2, 253])));
        assert_eq!(replaces, Some(expected_lease(Some(at(3600)))));
    }

    #[test]
    fn a_lease_the_caller_could_not_install_is_given_up_and_sought_again_after_a_backoff() {
        let mut client = bound(|_| {});

        client.lease_not_installed(at(2));

        assert_eq!(client.state(), DhcpState::Init);
        assert_eq!(client.lease(), None);
        assert_ne!(client.transaction_id(), TRANSACTION);
        assert_eq!(client.next_deadline(), Some(at(12)));
        assert_eq!(client.poll(at(11)), DhcpAction::None);
        let discover = sent(client.poll(at(12)));
        assert_eq!(discover.message.message_type, DhcpMessageType::Discover);
    }

    #[test]
    #[should_panic(expected = "lease_not_installed while the DHCP client is Requesting")]
    fn a_lease_not_installed_without_an_install_is_a_caller_bug() {
        requesting().lease_not_installed(at(0));
    }

    #[test]
    fn t1_and_t2_default_to_half_and_seven_eighths_of_the_lease() {
        let mut client = bound(|ack| ack.lease_seconds = Some(1000));
        assert_eq!(client.next_deadline(), Some(at(500)));
        sent(client.poll(at(500)));
        assert_eq!(client.state(), DhcpState::Renewing);
        // Half of the 375 s to T2 is 187.5 s, and T2 comes first after.
        assert_eq!(
            client.next_deadline(),
            Some(StackInstant::from_nanos(
                500 * NANOS_PER_SECOND + 187_500_000_000
            ))
        );
        sent(client.poll(at(875)));
        assert_eq!(client.state(), DhcpState::Rebinding);
    }

    #[test]
    fn t1_and_t2_follow_the_server_when_it_sends_them() {
        let mut client = bound(|ack| {
            ack.lease_seconds = Some(1000);
            ack.renewal_seconds = Some(100);
            ack.rebinding_seconds = Some(200);
        });
        assert_eq!(client.next_deadline(), Some(at(100)));
        sent(client.poll(at(100)));
        sent(client.poll(at(200)));
        assert_eq!(client.state(), DhcpState::Rebinding);
    }

    /// T1 and T2 out of order, past the lease or zero give way to the
    /// RFC 2131 §4.4.5 defaults instead of refusing an address every
    /// other client would take.
    #[test]
    fn t1_and_t2_that_break_their_order_take_the_defaults() {
        let schedules: [(Option<u32>, Option<u32>, u64, u64); 6] = [
            // T1 past T2: T1 takes 1800 s, and T2 its 3150 s with it.
            (Some(2000), Some(1000), 1800, 3150),
            // T2 past the lease.
            (None, Some(LEASE_SECONDS + 1), 1800, 3150),
            (Some(100), Some(LEASE_SECONDS), 100, 3150),
            // T1 past the lease, with no T2.
            (Some(LEASE_SECONDS), None, 1800, 3150),
            (Some(0), Some(0), 1800, 3150),
            // A T1 that fits keeps its value.
            (Some(100), Some(0), 100, 3150),
        ];
        for (renewal, rebinding, renew_at, rebind_at) in schedules {
            let mut client = bound(|ack| {
                ack.renewal_seconds = renewal;
                ack.rebinding_seconds = rebinding;
            });
            assert_eq!(
                client.next_deadline(),
                Some(at(renew_at)),
                "{renewal:?} {rebinding:?}"
            );
            assert_eq!(client.poll(at(renew_at - 1)), DhcpAction::None);
            sent(client.poll(at(rebind_at)));
            assert_eq!(client.state(), DhcpState::Rebinding);
            assert_eq!(
                client.lease().and_then(|lease| lease.expires_at),
                Some(at(3600))
            );
        }
    }

    /// A lease of one second, or a T1 of zero, must not have the client
    /// renew in the same breath as the ACK it just took: each ACK would
    /// start the next request, as fast as the server answers.
    #[test]
    fn a_one_second_lease_renews_half_a_second_in() {
        let mut client = bound(|ack| ack.lease_seconds = Some(1));

        assert_eq!(client.poll(at(0)), DhcpAction::None);
        assert_eq!(
            client.next_deadline(),
            Some(StackInstant::from_nanos(500_000_000))
        );
        sent(client.poll(StackInstant::from_nanos(500_000_000)));
        assert_eq!(client.state(), DhcpState::Renewing);
    }

    #[test]
    fn an_infinite_lease_never_renews_or_expires() {
        let mut client = bound(|ack| ack.lease_seconds = Some(INFINITE_LEASE_SECONDS));

        assert_eq!(client.next_deadline(), None);
        assert_eq!(
            client.poll(StackInstant::from_nanos(u64::MAX)),
            DhcpAction::None
        );
        assert_eq!(client.state(), DhcpState::Bound);
        assert_eq!(client.lease().map(|lease| lease.expires_at), Some(None));
    }

    #[test]
    fn the_longest_finite_lease_schedules_without_overflow() {
        let client = bound(|ack| ack.lease_seconds = Some(INFINITE_LEASE_SECONDS - 1));

        let lease = u64::from(INFINITE_LEASE_SECONDS - 1) * NANOS_PER_SECOND;
        assert_eq!(
            client.next_deadline(),
            Some(StackInstant::from_nanos(lease / 2))
        );
        assert_eq!(
            client.lease().and_then(|lease| lease.expires_at),
            Some(StackInstant::from_nanos(lease))
        );
    }

    fn server_packet(message: &DhcpServerMessage) -> ([u8; 400], usize) {
        let mut bytes = [0u8; 400];
        let len = message.encode(&mut bytes).expect("the message fits");
        (bytes, len)
    }

    /// Replaces the END that closes `bytes[..len]`'s options with
    /// `option` and a new END, and returns the new length.
    fn append_option(bytes: &mut [u8], len: usize, code: u8, value: &[u8]) -> usize {
        let mut end = len - 1;
        bytes[end] = code;
        end += 1;
        bytes[end] = value.len() as u8;
        end += 1;
        bytes[end..end + value.len()].copy_from_slice(value);
        end += value.len();
        bytes[end] = OPTION_END;
        end + 1
    }

    #[test]
    fn a_server_message_reads_back_what_the_server_wrote() {
        let client = DhcpClient::new(MAC, TRANSACTION);
        let mut message = reply(&client, DhcpMessageType::Ack, SERVER);
        message.renewal_seconds = Some(1800);
        message.rebinding_seconds = Some(3150);
        let (bytes, len) = server_packet(&message);

        let parsed = DhcpPacket::parse(&bytes[..len])
            .and_then(DhcpPacket::server_message)
            .expect("the packet parses");

        assert_eq!(parsed, message);
        assert_eq!(parsed.client_mac, MAC);
    }

    #[test]
    fn a_server_message_with_an_option_of_the_wrong_length_is_malformed() {
        let client = DhcpClient::new(MAC, TRANSACTION);
        let mut message = reply(&client, DhcpMessageType::Ack, SERVER);
        message.subnet_mask = None;
        message.router = None;
        message.dns_servers.clear();
        message.lease_seconds = None;
        let (bytes, len) = server_packet(&message);

        for (code, value) in [
            (OPTION_SUBNET_MASK, &[255, 255, 255][..]),
            (OPTION_LEASE_TIME, &[0, 0, 14][..]),
            (OPTION_RENEWAL_TIME, &[0, 0, 0, 0, 1][..]),
            (OPTION_SERVER_IDENTIFIER, &[192, 0, 2][..]),
            (OPTION_ROUTER, &[][..]),
            (OPTION_ROUTER, &[192, 0, 2, 254, 1][..]),
            (OPTION_DNS_SERVER, &[][..]),
            (OPTION_DNS_SERVER, &[192, 0, 2, 53, 192, 0][..]),
        ] {
            let mut bytes = bytes;
            let len = append_option(&mut bytes, len, code, value);
            let packet = DhcpPacket::parse(&bytes[..len]).expect("the header is intact");

            assert_eq!(packet.server_message(), None, "option {code} {value:?}");
        }
    }

    #[test]
    fn dns_servers_past_the_capacity_are_dropped_across_repeated_options() {
        let client = DhcpClient::new(MAC, TRANSACTION);
        let mut message = reply(&client, DhcpMessageType::Ack, SERVER);
        message.dns_servers.clear();
        let (mut bytes, len) = server_packet(&message);
        let first: [u8; 12] = [10, 0, 0, 1, 10, 0, 0, 2, 10, 0, 0, 3];
        let second: [u8; 12] = [10, 0, 0, 4, 10, 0, 0, 5, 10, 0, 0, 6];
        let len = append_option(&mut bytes, len, OPTION_DNS_SERVER, &first);
        let len = append_option(&mut bytes, len, OPTION_DNS_SERVER, &second);

        let parsed = DhcpPacket::parse(&bytes[..len])
            .and_then(DhcpPacket::server_message)
            .expect("the packet parses");

        let expected: DhcpDnsServers = (1..=4)
            .map(|last| Ipv4Address::new([10, 0, 0, last]))
            .collect();
        assert_eq!(parsed.dns_servers, expected);
    }

    #[test]
    fn a_router_option_that_lists_several_takes_the_first() {
        let client = DhcpClient::new(MAC, TRANSACTION);
        let mut message = reply(&client, DhcpMessageType::Ack, SERVER);
        message.router = None;
        let (mut bytes, len) = server_packet(&message);
        let len = append_option(
            &mut bytes,
            len,
            OPTION_ROUTER,
            &[192, 0, 2, 254, 192, 0, 2, 253],
        );

        let parsed = DhcpPacket::parse(&bytes[..len])
            .and_then(DhcpPacket::server_message)
            .expect("the packet parses");

        assert_eq!(parsed.router, Some(ROUTER));
    }

    #[test]
    fn a_renewal_carries_the_address_in_ciaddr_and_no_broadcast_flag() {
        let mut bytes = [0u8; 400];

        let renew = DhcpClientMessage::renew(TRANSACTION, MAC, ADDRESS);
        let len = renew.encode(&mut bytes).expect("the message fits");
        assert_eq!(&bytes[12..16], &ADDRESS.octets());
        assert_eq!(&bytes[10..12], &[0, 0]);
        let options = &bytes[BOOTP_HEADER_LEN + 4..len];
        let mut parser = DhcpOptionParser::new(options);
        while let Some(option) = parser.next().expect("options are well formed") {
            assert_ne!(option.code, OPTION_REQUESTED_IP);
            assert_ne!(option.code, OPTION_SERVER_IDENTIFIER);
        }

        DhcpClientMessage::discover(TRANSACTION, MAC)
            .encode(&mut bytes)
            .expect("the message fits");
        assert_eq!(&bytes[12..16], &[0; 4]);
        assert_eq!(&bytes[10..12], &[0x80, 0]);
    }

    #[test]
    fn a_mask_is_a_prefix_only_when_its_ones_are_contiguous() {
        assert_eq!(
            mask_prefix_len(Ipv4Address::new([255, 255, 255, 255])),
            Some(32)
        );
        assert_eq!(
            mask_prefix_len(Ipv4Address::new([255, 255, 255, 0])),
            Some(24)
        );
        assert_eq!(mask_prefix_len(Ipv4Address::new([128, 0, 0, 0])), Some(1));
        assert_eq!(mask_prefix_len(Ipv4Address::UNSPECIFIED), None);
        assert_eq!(mask_prefix_len(Ipv4Address::new([255, 0, 255, 0])), None);
        assert_eq!(mask_prefix_len(Ipv4Address::new([0, 255, 255, 255])), None);
    }
}
