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

use core::hash::Hasher;

use siphasher::sip::SipHasher24;

use crate::secret::{KeyDomain, StackSecret, write_endpoint};
use crate::{StackInstant, TcpEndpoint};

/// RFC 6528 §3: `M` is a 4 microsecond timer.
const CLOCK_TICK_NANOS: u64 = 4_000;

/// The initial send sequence number for a connection between `local` and
/// `remote` opened at `now`.
///
/// `F` is keyed by the key `secret` derives for this function alone, so it
/// shares no key with the socket indexes' hash.
pub fn initial_sequence_number(
    secret: &StackSecret,
    local: TcpEndpoint,
    remote: TcpEndpoint,
    now: StackInstant,
) -> u32 {
    let key = secret.derive(KeyDomain::InitialSequenceNumber);
    let mut hasher = SipHasher24::new_with_keys(key.k0, key.k1);
    write_endpoint(&mut hasher, local);
    write_endpoint(&mut hasher, remote);
    let flow = hasher.finish() as u32;
    let clock = (now.nanos() / CLOCK_TICK_NANOS) as u32;
    clock.wrapping_add(flow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IpAddress, Ipv4Address, Ipv6Address};

    const SECRET_A: StackSecret = StackSecret::from_bytes([0x11; StackSecret::BYTES]);
    const SECRET_B: StackSecret = StackSecret::from_bytes([0x22; StackSecret::BYTES]);

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
}
