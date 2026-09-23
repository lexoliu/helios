use core::fmt;

pub type EthernetAddress = [u8; 6];

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ipv4Address([u8; 4]);

impl Ipv4Address {
    pub const UNSPECIFIED: Self = Self([0, 0, 0, 0]);
    pub const BROADCAST: Self = Self([255, 255, 255, 255]);

    pub const fn new(octets: [u8; 4]) -> Self {
        Self(octets)
    }

    pub const fn octets(self) -> [u8; 4] {
        self.0
    }

    pub const fn is_unspecified(self) -> bool {
        self.0[0] == 0 && self.0[1] == 0 && self.0[2] == 0 && self.0[3] == 0
    }

    pub const fn is_multicast(self) -> bool {
        self.0[0] >= 224 && self.0[0] <= 239
    }
}

impl fmt::Debug for Ipv4Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ipv6Address([u8; 16]);

impl Ipv6Address {
    pub const UNSPECIFIED: Self = Self([0; 16]);
    pub const LOOPBACK: Self = Self([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    pub const ALL_NODES_LINK_LOCAL: Self =
        Self([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    pub const ALL_ROUTERS_LINK_LOCAL: Self =
        Self([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
    /// Prefix length of the link-local prefix `fe80::/64` and of every
    /// prefix SLAAC can build a modified EUI-64 interface identifier for.
    pub const LINK_LOCAL_PREFIX_LEN: u8 = 64;

    pub const fn new(octets: [u8; 16]) -> Self {
        Self(octets)
    }

    /// Builds the modified EUI-64 interface identifier (RFC 4291 §2.5.1,
    /// Appendix A) for a 48-bit Ethernet address: the OUI and the NIC id
    /// with `fffe` spliced between them and the universal/local bit of
    /// the first octet inverted.
    pub const fn eui64_interface_identifier(mac: EthernetAddress) -> [u8; 8] {
        [
            mac[0] ^ 0x02,
            mac[1],
            mac[2],
            0xff,
            0xfe,
            mac[3],
            mac[4],
            mac[5],
        ]
    }

    /// Builds `fe80::/64` + modified EUI-64 for this interface's MAC.
    pub const fn link_local_from_mac(mac: EthernetAddress) -> Self {
        Self::from_prefix_and_eui64(Self([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), mac)
    }

    /// Combines the top 64 bits of `prefix` with the modified EUI-64
    /// interface identifier of `mac`. Only defined for /64 prefixes,
    /// which is the only prefix length SLAAC autoconfigures from.
    pub const fn from_prefix_and_eui64(prefix: Self, mac: EthernetAddress) -> Self {
        let identifier = Self::eui64_interface_identifier(mac);
        let p = prefix.0;
        Self([
            p[0],
            p[1],
            p[2],
            p[3],
            p[4],
            p[5],
            p[6],
            p[7],
            identifier[0],
            identifier[1],
            identifier[2],
            identifier[3],
            identifier[4],
            identifier[5],
            identifier[6],
            identifier[7],
        ])
    }

    pub const fn is_link_local(self) -> bool {
        self.0[0] == 0xfe && (self.0[1] & 0xc0) == 0x80
    }

    pub const fn is_loopback(self) -> bool {
        let mut index = 0;
        while index < 15 {
            if self.0[index] != 0 {
                return false;
            }
            index += 1;
        }
        self.0[15] == 1
    }

    pub const fn octets(self) -> [u8; 16] {
        self.0
    }

    pub const fn is_unspecified(self) -> bool {
        let octets = self.0;
        octets[0] == 0
            && octets[1] == 0
            && octets[2] == 0
            && octets[3] == 0
            && octets[4] == 0
            && octets[5] == 0
            && octets[6] == 0
            && octets[7] == 0
            && octets[8] == 0
            && octets[9] == 0
            && octets[10] == 0
            && octets[11] == 0
            && octets[12] == 0
            && octets[13] == 0
            && octets[14] == 0
            && octets[15] == 0
    }

    pub const fn is_multicast(self) -> bool {
        self.0[0] == 0xff
    }

    pub const fn scope(self) -> Ipv6Scope {
        if self.is_multicast() {
            match self.0[1] & 0x0f {
                0x1 => Ipv6Scope::InterfaceLocal,
                0x2 => Ipv6Scope::LinkLocal,
                0x5 => Ipv6Scope::SiteLocal,
                0xe => Ipv6Scope::Global,
                _ => Ipv6Scope::Other,
            }
        } else if self.0[0] == 0xfe && (self.0[1] & 0xc0) == 0x80 {
            Ipv6Scope::LinkLocal
        } else {
            Ipv6Scope::Global
        }
    }

    pub fn solicited_node_multicast(self) -> Self {
        let mut octets = [0u8; 16];
        octets[0] = 0xff;
        octets[1] = 0x02;
        octets[11] = 0x01;
        octets[12] = 0xff;
        octets[13] = self.0[13];
        octets[14] = self.0[14];
        octets[15] = self.0[15];
        Self(octets)
    }
}

impl fmt::Debug for Ipv6Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, chunk) in self.0.chunks_exact(2).enumerate() {
            if index != 0 {
                f.write_str(":")?;
            }
            write!(f, "{:x}", u16::from_be_bytes([chunk[0], chunk[1]]))?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ipv6Scope {
    InterfaceLocal,
    LinkLocal,
    SiteLocal,
    Global,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IpAddress {
    Ipv4(Ipv4Address),
    Ipv6(Ipv6Address),
}

impl IpAddress {
    pub const fn is_unspecified(self) -> bool {
        match self {
            Self::Ipv4(address) => address.is_unspecified(),
            Self::Ipv6(address) => address.is_unspecified(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ipv4Cidr {
    address: Ipv4Address,
    prefix_len: u8,
}

impl Ipv4Cidr {
    pub const fn new(address: Ipv4Address, prefix_len: u8) -> Self {
        assert!(prefix_len <= 32, "IPv4 CIDR prefix length must be <= 32");
        Self {
            address,
            prefix_len,
        }
    }

    pub const fn address(self) -> Ipv4Address {
        self.address
    }

    pub const fn prefix_len(self) -> u8 {
        self.prefix_len
    }

    pub fn contains(self, address: Ipv4Address) -> bool {
        let mask = if self.prefix_len == 0 {
            0
        } else {
            u32::MAX << (32 - self.prefix_len)
        };
        u32::from_be_bytes(self.address.octets()) & mask
            == u32::from_be_bytes(address.octets()) & mask
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ipv6Cidr {
    address: Ipv6Address,
    prefix_len: u8,
}

impl Ipv6Cidr {
    pub const fn new(address: Ipv6Address, prefix_len: u8) -> Self {
        assert!(prefix_len <= 128, "IPv6 CIDR prefix length must be <= 128");
        Self {
            address,
            prefix_len,
        }
    }

    pub const fn address(self) -> Ipv6Address {
        self.address
    }

    pub const fn prefix_len(self) -> u8 {
        self.prefix_len
    }

    pub fn contains(self, address: Ipv6Address) -> bool {
        let lhs = self.address.octets();
        let rhs = address.octets();
        let full_bytes = usize::from(self.prefix_len / 8);
        if lhs[..full_bytes] != rhs[..full_bytes] {
            return false;
        }
        let remaining = self.prefix_len % 8;
        if remaining == 0 {
            return true;
        }
        let mask = u8::MAX << (8 - remaining);
        lhs[full_bytes] & mask == rhs[full_bytes] & mask
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IpCidr {
    Ipv4(Ipv4Cidr),
    Ipv6(Ipv6Cidr),
}
