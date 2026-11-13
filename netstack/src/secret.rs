//! The per-boot secret of a stack and the independent keys derived from it.
//!
//! The embedding kernel draws one [`StackSecret`] from its root entropy
//! generator and hands every stack the same value. Each keyed function the
//! stack runs (the TCP initial sequence number function of RFC 6528, the
//! socket indexes' hash) takes its own [`SipKey`] from it, derived under a
//! label that names the use, so no two functions share a key and a value
//! one of them leaks says nothing about another.
//!
//! Concurrency contract: pure functions over copied values; no state, no
//! locking.

use core::fmt;
use core::hash::Hasher;

use siphasher::sip128::SipHasher24;

use crate::{IpAddress, TcpEndpoint};

/// The per-boot secret every keyed function of a stack derives its key from.
///
/// It has no default: a stack cannot be built without one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct StackSecret([u8; Self::BYTES]);

impl StackSecret {
    /// SipHash takes a 128-bit key.
    pub const BYTES: usize = 16;

    pub const fn from_bytes(bytes: [u8; Self::BYTES]) -> Self {
        Self(bytes)
    }

    /// The key of the function `domain` names.
    ///
    /// The key is the 128-bit SipHash-2-4 of the domain label under the
    /// secret: SipHash is a pseudorandom function, so keys derived under
    /// different labels are independent of one another and of the secret.
    pub(crate) fn derive(&self, domain: KeyDomain) -> SipKey {
        let key = SipHasher24::new_with_key(&self.0).hash(domain.label());
        SipKey {
            k0: key.h1,
            k1: key.h2,
        }
    }
}

impl fmt::Debug for StackSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StackSecret(..)")
    }
}

/// The functions a key is derived for.
#[derive(Clone, Copy, Debug)]
pub(crate) enum KeyDomain {
    /// `F` of RFC 6528 §3.
    InitialSequenceNumber,
    /// The hash of the socket indexes.
    SocketIndex,
}

impl KeyDomain {
    const fn label(self) -> &'static [u8] {
        match self {
            Self::InitialSequenceNumber => b"helios-netstack/initial-sequence-number",
            Self::SocketIndex => b"helios-netstack/socket-index",
        }
    }
}

/// A 128-bit SipHash key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct SipKey {
    pub(crate) k0: u64,
    pub(crate) k1: u64,
}

impl fmt::Debug for SipKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SipKey(..)")
    }
}

/// Feeds one endpoint as a family tag, the address octets and the port,
/// so an IPv4 and an IPv6 endpoint can never produce the same input.
pub(crate) fn write_endpoint<H: Hasher>(hasher: &mut H, endpoint: TcpEndpoint) {
    write_address(hasher, endpoint.address);
    hasher.write_u16(endpoint.port);
}

/// Feeds one address as a family tag and its octets.
pub(crate) fn write_address<H: Hasher>(hasher: &mut H, address: IpAddress) {
    match address {
        IpAddress::Ipv4(address) => {
            hasher.write_u8(4);
            hasher.write(&address.octets());
        }
        IpAddress::Ipv6(address) => {
            hasher.write_u8(6);
            hasher.write(&address.octets());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET_A: StackSecret = StackSecret::from_bytes([0x11; StackSecret::BYTES]);
    const SECRET_B: StackSecret = StackSecret::from_bytes([0x22; StackSecret::BYTES]);

    #[test]
    fn two_domains_derive_different_keys_from_one_secret() {
        assert_ne!(
            SECRET_A.derive(KeyDomain::InitialSequenceNumber),
            SECRET_A.derive(KeyDomain::SocketIndex),
        );
    }

    #[test]
    fn two_secrets_derive_different_keys_for_one_domain() {
        assert_ne!(
            SECRET_A.derive(KeyDomain::SocketIndex),
            SECRET_B.derive(KeyDomain::SocketIndex),
        );
    }

    #[test]
    fn the_secret_is_not_printed() {
        assert_eq!(alloc::format!("{SECRET_A:?}"), "StackSecret(..)");
        assert_eq!(
            alloc::format!("{:?}", SECRET_A.derive(KeyDomain::SocketIndex)),
            "SipKey(..)"
        );
    }
}
