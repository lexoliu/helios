//! TCP initial sequence numbers (RFC 6528).
//!
//! `ISN = M + F(localip, localport, remoteip, remoteport, secretkey)`
//! where `M` is a clock that ticks once every four microseconds and `F`
//! is a keyed hash (RFC 6528 §3). The active and the passive open both
//! take their sequence number from [`initial_sequence_number`], so a
//! connection's first sequence number is unpredictable to anyone who
//! cannot read the secret, and still advances with time for one
//! four-tuple, which is what keeps a reincarnated connection clear of the
//! old one's sequence space.
//!
//! `F` is SipHash-2-4, the keyed hash Linux uses for the same purpose.
//!
//! Concurrency contract: pure functions over copied values; no state, no
//! locking.

use core::fmt;
use core::hash::Hasher;

use siphasher::sip::SipHasher24;

use crate::{IpAddress, StackInstant, TcpEndpoint};

/// RFC 6528 §3: `M` is a 4 microsecond timer.
const CLOCK_TICK_NANOS: u64 = 4_000;

/// The per-boot key of `F` in RFC 6528 §3.
///
/// The embedding kernel draws it once from its root entropy generator and
/// hands every stack the same value, so a four-tuple hashes identically
/// whichever stack opens the connection. It has no default: a stack cannot
/// be built without one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct IsnSecret([u8; Self::BYTES]);

impl IsnSecret {
    /// SipHash-2-4 takes a 128-bit key.
    pub const BYTES: usize = 16;

    pub const fn from_bytes(bytes: [u8; Self::BYTES]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for IsnSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IsnSecret(..)")
    }
}

/// The initial send sequence number for a connection between `local` and
/// `remote` opened at `now`.
pub fn initial_sequence_number(
    secret: &IsnSecret,
    local: TcpEndpoint,
    remote: TcpEndpoint,
    now: StackInstant,
) -> u32 {
    let mut hasher = SipHasher24::new_with_key(&secret.0);
    write_endpoint(&mut hasher, local);
    write_endpoint(&mut hasher, remote);
    let flow = hasher.finish() as u32;
    let clock = (now.nanos() / CLOCK_TICK_NANOS) as u32;
    clock.wrapping_add(flow)
}

/// Feeds one endpoint as a family tag, the address octets and the port,
/// so an IPv4 and an IPv6 endpoint can never produce the same input.
fn write_endpoint(hasher: &mut SipHasher24, endpoint: TcpEndpoint) {
    match endpoint.address {
        IpAddress::Ipv4(address) => {
            hasher.write_u8(4);
            hasher.write(&address.octets());
        }
        IpAddress::Ipv6(address) => {
            hasher.write_u8(6);
            hasher.write(&address.octets());
        }
    }
    hasher.write_u16(endpoint.port);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ipv4Address, Ipv6Address};

    const SECRET_A: IsnSecret = IsnSecret::from_bytes([0x11; IsnSecret::BYTES]);
    const SECRET_B: IsnSecret = IsnSecret::from_bytes([0x22; IsnSecret::BYTES]);

    fn endpoint(octets: [u8; 4], port: u16) -> TcpEndpoint {
        TcpEndpoint {
            address: IpAddress::Ipv4(Ipv4Address::new(octets)),
            port,
        }
    }

    fn local() -> TcpEndpoint {
        endpoint([192, 0, 2, 10], 49_152)
    }

    fn remote() -> TcpEndpoint {
        endpoint([198, 51, 100, 7], 443)
    }

    #[test]
    fn two_secrets_give_different_numbers_for_one_flow() {
        let now = StackInstant::from_nanos(1_000_000_000);
        assert_ne!(
            initial_sequence_number(&SECRET_A, local(), remote(), now),
            initial_sequence_number(&SECRET_B, local(), remote(), now),
        );
    }

    #[test]
    fn one_secret_gives_different_numbers_for_different_flows() {
        let now = StackInstant::from_nanos(1_000_000_000);
        let base = initial_sequence_number(&SECRET_A, local(), remote(), now);
        let variants = [
            (endpoint([192, 0, 2, 11], 49_152), remote()),
            (endpoint([192, 0, 2, 10], 49_153), remote()),
            (local(), endpoint([198, 51, 100, 8], 443)),
            (local(), endpoint([198, 51, 100, 7], 444)),
            // The direction of the flow matters: the tuple is ordered.
            (remote(), local()),
        ];
        for (variant_local, variant_remote) in variants {
            assert_ne!(
                base,
                initial_sequence_number(&SECRET_A, variant_local, variant_remote, now),
                "{variant_local:?} -> {variant_remote:?} collided with the base flow",
            );
        }
    }

    #[test]
    fn an_ipv4_and_an_ipv6_endpoint_do_not_share_an_input() {
        let now = StackInstant::from_nanos(0);
        let v6 = TcpEndpoint {
            address: IpAddress::Ipv6(Ipv6Address::new([
                0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
            ])),
            port: 49_152,
        };
        assert_ne!(
            initial_sequence_number(&SECRET_A, local(), remote(), now),
            initial_sequence_number(&SECRET_A, v6, remote(), now),
        );
    }

    /// RFC 6528 §3: for one four-tuple the number is the keyed hash plus a
    /// clock that ticks every four microseconds.
    #[test]
    fn the_number_advances_with_the_clock() {
        let at = |nanos| {
            initial_sequence_number(
                &SECRET_A,
                local(),
                remote(),
                StackInstant::from_nanos(nanos),
            )
        };
        let origin = at(0);
        assert_eq!(at(4_000), origin.wrapping_add(1));
        assert_eq!(at(3_999), origin, "M ticks once per four microseconds");
        assert_eq!(at(4_000_000), origin.wrapping_add(1_000));
        assert_eq!(at(1_000_000_000), origin.wrapping_add(250_000));
    }

    #[test]
    fn the_secret_is_not_printed() {
        assert_eq!(alloc::format!("{SECRET_A:?}"), "IsnSecret(..)");
    }
}
