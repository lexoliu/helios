//! IPv6 Neighbor Discovery: router solicitation scheduling and the
//! stateless address autoconfiguration (SLAAC) decision made from a
//! received Router Advertisement.
//!
//! # Concurrency contract
//!
//! Every type here is plain owned state with no interior mutability and
//! no synchronisation of its own, exactly like the ARP/neighbor cache it
//! sits beside in [`crate::Stack`]. One [`NeighborDiscovery`] belongs to
//! one `Stack`, and a `Stack` belongs to exactly one network shard,
//! which the kernel keeps behind that shard's lock and pins to a
//! processor. Nothing in this module may be shared between shards: the
//! only cross-shard propagation of what it learns is the kernel control
//! plane republishing the resulting address and route tables to every
//! shard, the same path DHCPv4 leases already take.
//!
//! The wire formats live in [`crate::packet`]; this module is pure
//! state-machine logic so it can be tested without frames, and so the
//! `Stack` keeps a single place where an advertisement turns into
//! address-table and route-table mutations.

use arrayvec::ArrayVec;

use crate::{
    EthernetAddress, Icmpv6RouterAdvertisement, Ipv6Address, Ipv6Cidr, NdpOption, StackInstant,
};

/// Recursive DNS servers retained from a Router Advertisement's RDNSS
/// option (RFC 8106). Three is the RFC 6106 §5.1 recommendation; a
/// fourth slot leaves room for a router that advertises one more.
pub const MAX_IPV6_DNS_SERVERS: usize = 4;

/// Recursive DNS servers learned over IPv6.
pub type Ipv6DnsServers = ArrayVec<Ipv6Address, MAX_IPV6_DNS_SERVERS>;

/// `MAX_RTR_SOLICITATIONS` (RFC 4861 §10).
pub const MAX_ROUTER_SOLICITATIONS: u8 = 3;

/// `RTR_SOLICITATION_INTERVAL` (RFC 4861 §10), in nanoseconds.
pub const ROUTER_SOLICITATION_INTERVAL_NANOS: u64 = 4_000_000_000;

/// Only a /64 prefix can carry a modified EUI-64 interface identifier,
/// so SLAAC autoconfigures from that prefix length alone (RFC 4862 §5.5.3).
pub const SLAAC_PREFIX_LEN: u8 = 64;

/// Router-solicitation scheduling state.
///
/// The stack drives this from [`crate::Stack::drive_ipv6_autoconfig`],
/// which the kernel calls on its configuration poll the same way it
/// calls the DHCPv4 driver. Solicitation stops as soon as a Router
/// Advertisement lands, or after `MAX_ROUTER_SOLICITATIONS` attempts,
/// so a link with no router costs three frames and then nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NeighborDiscovery {
    solicitations_sent: u8,
    next_solicitation_at: Option<StackInstant>,
    configured: bool,
}

impl NeighborDiscovery {
    pub const fn new() -> Self {
        Self {
            solicitations_sent: 0,
            next_solicitation_at: None,
            configured: false,
        }
    }

    /// Whether a Router Advertisement has configured this interface.
    pub const fn is_configured(self) -> bool {
        self.configured
    }

    pub const fn solicitations_sent(self) -> u8 {
        self.solicitations_sent
    }

    /// Returns whether the caller should transmit a Router Solicitation
    /// now, charging the attempt and arming the retransmit deadline when
    /// it does.
    pub fn poll_solicitation(&mut self, now: StackInstant) -> bool {
        if self.configured || self.solicitations_sent >= MAX_ROUTER_SOLICITATIONS {
            return false;
        }
        if let Some(deadline) = self.next_solicitation_at
            && now.nanos() < deadline.nanos()
        {
            return false;
        }
        self.solicitations_sent += 1;
        self.next_solicitation_at = Some(StackInstant::from_nanos(
            now.nanos()
                .saturating_add(ROUTER_SOLICITATION_INTERVAL_NANOS),
        ));
        true
    }

    /// Records that an advertisement configured the interface, which
    /// stops further solicitation.
    pub fn record_advertisement(&mut self) {
        self.configured = true;
        self.next_solicitation_at = None;
    }

    /// Returns to the unconfigured state so solicitation restarts, used
    /// when every autoconfigured address is withdrawn.
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Default for NeighborDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// What a Router Advertisement asks this interface to configure.
///
/// Produced by [`interpret_router_advertisement`] and applied by the
/// stack; separating the two keeps the SLAAC rules testable without
/// building frames and without a live address table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ipv6RouterConfiguration {
    /// Link-local address the advertisement came from.
    pub router: Ipv6Address,
    /// Router's link-layer address, when it sent a Source Link-Layer
    /// Address option.
    pub router_mac: Option<EthernetAddress>,
    /// Lifetime of the router as a default router. Zero withdraws it.
    pub router_lifetime_seconds: u16,
    /// On-link prefixes to install as connected routes.
    pub on_link_prefixes: ArrayVec<Ipv6Cidr, MAX_ADVERTISED_PREFIXES>,
    /// Addresses SLAAC derives from autonomous prefixes.
    pub addresses: ArrayVec<Ipv6Cidr, MAX_ADVERTISED_PREFIXES>,
    /// Link MTU option, when present.
    pub link_mtu: Option<u32>,
    /// `CurHopLimit`, when the router specified one (zero means unspecified).
    pub hop_limit: Option<u8>,
    /// Recursive DNS servers from the RDNSS option.
    pub dns_servers: Ipv6DnsServers,
}

/// Prefixes retained from one advertisement. QEMU's user-mode network
/// advertises a single prefix; four leaves room for a real router
/// advertising a handful without unbounded state.
pub const MAX_ADVERTISED_PREFIXES: usize = 4;

impl Ipv6RouterConfiguration {
    /// Whether the advertisement yielded anything worth installing.
    pub fn configures_interface(&self) -> bool {
        !self.addresses.is_empty()
            || !self.on_link_prefixes.is_empty()
            || self.router_lifetime_seconds != 0
    }
}

/// Applies RFC 4862 §5.5.3 to one advertisement, deriving the addresses
/// and routes it implies for an interface with hardware address `mac`.
///
/// A prefix is autoconfigured when it carries the A bit, is a /64 (the
/// only length a modified EUI-64 identifier fits), has a non-zero valid
/// lifetime, and is not the link-local prefix. On-link (L bit) prefixes
/// are reported separately because a prefix can be on-link without being
/// autonomous.
pub fn interpret_router_advertisement(
    advertisement: Icmpv6RouterAdvertisement<'_>,
    router: Ipv6Address,
    mac: EthernetAddress,
) -> Ipv6RouterConfiguration {
    let mut configuration = Ipv6RouterConfiguration {
        router,
        router_mac: None,
        router_lifetime_seconds: advertisement.router_lifetime_seconds,
        on_link_prefixes: ArrayVec::new(),
        addresses: ArrayVec::new(),
        link_mtu: None,
        hop_limit: (advertisement.current_hop_limit != 0)
            .then_some(advertisement.current_hop_limit),
        dns_servers: ArrayVec::new(),
    };

    for option in advertisement.options {
        match option {
            NdpOption::SourceLinkLayerAddress(router_mac) => {
                configuration.router_mac = Some(router_mac);
            }
            NdpOption::Mtu(mtu) => configuration.link_mtu = Some(mtu),
            NdpOption::RecursiveDnsServers(servers) => {
                if servers.lifetime_seconds != 0 {
                    for address in servers.addresses() {
                        let _ = configuration.dns_servers.try_push(address);
                    }
                }
            }
            NdpOption::PrefixInformation(prefix) => {
                // The prefix length arrives from the wire, so reject an
                // out-of-range one here rather than letting `Ipv6Cidr`'s
                // constructor assert on a hostile advertisement.
                if prefix.prefix_len > 128
                    || prefix.valid_lifetime_seconds == 0
                    || prefix.prefix.is_link_local()
                {
                    continue;
                }
                if prefix.on_link {
                    let _ = configuration
                        .on_link_prefixes
                        .try_push(Ipv6Cidr::new(prefix.prefix, prefix.prefix_len));
                }
                if prefix.autonomous && prefix.prefix_len == SLAAC_PREFIX_LEN {
                    let address = Ipv6Address::from_prefix_and_eui64(prefix.prefix, mac);
                    let _ = configuration
                        .addresses
                        .try_push(Ipv6Cidr::new(address, prefix.prefix_len));
                }
            }
            NdpOption::TargetLinkLayerAddress(_) | NdpOption::Other { .. } => {}
        }
    }

    configuration
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Icmpv6Packet;

    const MAC: EthernetAddress = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const ROUTER: Ipv6Address =
        Ipv6Address::new([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02]);

    /// Builds a Router Advertisement body with a prefix-information
    /// option for `fec0::/64` (QEMU user-mode networking's prefix), a
    /// source link-layer address, an MTU option, and RDNSS at `fec0::3`.
    fn qemu_style_advertisement(buffer: &mut [u8]) -> usize {
        buffer[..16].fill(0);
        buffer[0] = 134;
        buffer[4] = 64; // CurHopLimit
        buffer[6..8].copy_from_slice(&300u16.to_be_bytes()); // router lifetime
        let mut offset = 16;

        // Source link-layer address.
        buffer[offset] = 1;
        buffer[offset + 1] = 1;
        buffer[offset + 2..offset + 8].copy_from_slice(&[0x52, 0x55, 0x0a, 0x00, 0x02, 0x02]);
        offset += 8;

        // Prefix information: fec0::/64, on-link + autonomous.
        buffer[offset..offset + 32].fill(0);
        buffer[offset] = 3;
        buffer[offset + 1] = 4;
        buffer[offset + 2] = 64;
        buffer[offset + 3] = 0xc0;
        buffer[offset + 4..offset + 8].copy_from_slice(&86_400u32.to_be_bytes());
        buffer[offset + 8..offset + 12].copy_from_slice(&14_400u32.to_be_bytes());
        buffer[offset + 16] = 0xfe;
        buffer[offset + 17] = 0xc0;
        offset += 32;

        // MTU.
        buffer[offset..offset + 8].fill(0);
        buffer[offset] = 5;
        buffer[offset + 1] = 1;
        buffer[offset + 4..offset + 8].copy_from_slice(&1500u32.to_be_bytes());
        offset += 8;

        // RDNSS: fec0::3.
        buffer[offset..offset + 24].fill(0);
        buffer[offset] = 25;
        buffer[offset + 1] = 3;
        buffer[offset + 4..offset + 8].copy_from_slice(&600u32.to_be_bytes());
        buffer[offset + 8] = 0xfe;
        buffer[offset + 9] = 0xc0;
        buffer[offset + 23] = 3;
        offset += 24;

        offset
    }

    #[test]
    fn slaac_derives_address_route_and_resolvers_from_advertisement() {
        let mut buffer = [0u8; 128];
        let len = qemu_style_advertisement(&mut buffer);
        let Some(Icmpv6Packet::RouterAdvertisement(advertisement)) =
            Icmpv6Packet::parse(&buffer[..len])
        else {
            panic!("router advertisement should parse");
        };
        let configuration = interpret_router_advertisement(advertisement, ROUTER, MAC);

        assert_eq!(configuration.router, ROUTER);
        assert_eq!(
            configuration.router_mac,
            Some([0x52, 0x55, 0x0a, 0x00, 0x02, 0x02])
        );
        assert_eq!(configuration.router_lifetime_seconds, 300);
        assert_eq!(configuration.link_mtu, Some(1500));
        assert_eq!(configuration.hop_limit, Some(64));
        assert_eq!(
            configuration.addresses.as_slice(),
            &[Ipv6Cidr::new(
                Ipv6Address::new([
                    0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0x50, 0x54, 0x00, 0xff, 0xfe, 0x12, 0x34, 0x56,
                ]),
                64,
            )]
        );
        assert_eq!(
            configuration.on_link_prefixes.as_slice(),
            &[Ipv6Cidr::new(
                Ipv6Address::new([0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                64,
            )]
        );
        assert_eq!(
            configuration.dns_servers.as_slice(),
            &[Ipv6Address::new([
                0xfe, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3
            ])]
        );
        assert!(configuration.configures_interface());
    }

    #[test]
    fn expired_prefix_and_non_autonomous_prefix_do_not_autoconfigure() {
        let mut buffer = [0u8; 128];
        buffer[..16].fill(0);
        buffer[0] = 134;
        let mut offset = 16;
        // Zero valid lifetime: ignored entirely.
        buffer[offset..offset + 32].fill(0);
        buffer[offset] = 3;
        buffer[offset + 1] = 4;
        buffer[offset + 2] = 64;
        buffer[offset + 3] = 0xc0;
        buffer[offset + 16] = 0x20;
        buffer[offset + 17] = 0x01;
        offset += 32;
        // On-link but not autonomous: route only, no address.
        buffer[offset..offset + 32].fill(0);
        buffer[offset] = 3;
        buffer[offset + 1] = 4;
        buffer[offset + 2] = 64;
        buffer[offset + 3] = 0x80;
        buffer[offset + 4..offset + 8].copy_from_slice(&86_400u32.to_be_bytes());
        buffer[offset + 16] = 0x20;
        buffer[offset + 17] = 0x02;
        offset += 32;

        let Some(Icmpv6Packet::RouterAdvertisement(advertisement)) =
            Icmpv6Packet::parse(&buffer[..offset])
        else {
            panic!("router advertisement should parse");
        };
        let configuration = interpret_router_advertisement(advertisement, ROUTER, MAC);
        assert!(configuration.addresses.is_empty());
        assert_eq!(configuration.on_link_prefixes.len(), 1);
        assert_eq!(configuration.router_lifetime_seconds, 0);
    }

    #[test]
    fn out_of_range_prefix_length_is_dropped_rather_than_asserting() {
        let mut buffer = [0u8; 64];
        buffer[..16].fill(0);
        buffer[0] = 134;
        let offset = 16;
        buffer[offset..offset + 32].fill(0);
        buffer[offset] = 3;
        buffer[offset + 1] = 4;
        // A prefix length no IPv6 CIDR can represent.
        buffer[offset + 2] = 200;
        buffer[offset + 3] = 0xc0;
        buffer[offset + 4..offset + 8].copy_from_slice(&86_400u32.to_be_bytes());
        buffer[offset + 16] = 0x20;
        buffer[offset + 17] = 0x01;

        let Some(Icmpv6Packet::RouterAdvertisement(advertisement)) =
            Icmpv6Packet::parse(&buffer[..offset + 32])
        else {
            panic!("router advertisement should parse");
        };
        let configuration = interpret_router_advertisement(advertisement, ROUTER, MAC);
        assert!(configuration.addresses.is_empty());
        assert!(configuration.on_link_prefixes.is_empty());
    }

    #[test]
    fn zero_length_option_terminates_the_option_walk() {
        let mut buffer = [0u8; 32];
        buffer[..16].fill(0);
        buffer[0] = 134;
        // Length 0 is illegal and would otherwise loop forever.
        buffer[16] = 3;
        buffer[17] = 0;

        let Some(Icmpv6Packet::RouterAdvertisement(advertisement)) =
            Icmpv6Packet::parse(&buffer[..24])
        else {
            panic!("router advertisement should parse");
        };
        assert_eq!(advertisement.options.iter().count(), 0);
    }

    #[test]
    fn solicitation_backs_off_and_stops_after_the_rfc_limit() {
        let mut discovery = NeighborDiscovery::new();
        assert!(discovery.poll_solicitation(StackInstant::from_nanos(0)));
        // Inside the retransmit interval: no second frame.
        assert!(!discovery.poll_solicitation(StackInstant::from_nanos(1)));
        assert!(
            discovery
                .poll_solicitation(StackInstant::from_nanos(ROUTER_SOLICITATION_INTERVAL_NANOS))
        );
        assert!(discovery.poll_solicitation(StackInstant::from_nanos(
            2 * ROUTER_SOLICITATION_INTERVAL_NANOS
        )));
        assert_eq!(discovery.solicitations_sent(), MAX_ROUTER_SOLICITATIONS);
        assert!(!discovery.poll_solicitation(StackInstant::from_nanos(
            10 * ROUTER_SOLICITATION_INTERVAL_NANOS
        )));
    }

    #[test]
    fn advertisement_stops_solicitation_and_reset_restarts_it() {
        let mut discovery = NeighborDiscovery::new();
        assert!(discovery.poll_solicitation(StackInstant::from_nanos(0)));
        discovery.record_advertisement();
        assert!(discovery.is_configured());
        assert!(!discovery.poll_solicitation(StackInstant::from_nanos(
            10 * ROUTER_SOLICITATION_INTERVAL_NANOS
        )));
        discovery.reset();
        assert!(!discovery.is_configured());
        assert!(discovery.poll_solicitation(StackInstant::from_nanos(
            10 * ROUTER_SOLICITATION_INTERVAL_NANOS
        )));
    }

    #[test]
    fn link_local_identifier_matches_rfc_4291_appendix_a() {
        assert_eq!(
            Ipv6Address::link_local_from_mac(MAC),
            Ipv6Address::new([
                0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0x50, 0x54, 0x00, 0xff, 0xfe, 0x12, 0x34, 0x56,
            ])
        );
    }
}
