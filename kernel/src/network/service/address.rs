use core::future::Future;

use arrayvec::ArrayVec;

use super::*;

/// Upper bound on the addresses one name lookup contributes to a
/// connect walk.
///
/// A dual-family answer lists several records per family. Past this
/// point an extra candidate can only spend deadline that the earlier
/// ones would have used, so the walk stops considering them.
const MAX_CONNECT_CANDIDATES: usize = 8;

/// A non-empty, ordered list of addresses to attempt for one
/// destination name.
///
/// Non-emptiness is an invariant of construction: `new` reports an
/// empty lookup as `None` so the callers that own the "unresolved
/// host" error keep producing it, and the attempt walk never has to
/// describe a run with no attempts in it.
pub(super) struct ConnectCandidates {
    addresses: ArrayVec<IpAddress, MAX_CONNECT_CANDIDATES>,
}

impl ConnectCandidates {
    /// Orders a resolver answer for a link that holds source addresses
    /// in the given families.
    ///
    /// A dual-family lookup routinely returns AAAA records on a link
    /// where nothing configured an IPv6 address, and vice versa, so
    /// addresses of a family this interface holds no source address
    /// for are dropped while any answer of a configured family
    /// remains. When no answer belongs to a configured family the
    /// resolver order stands, leaving the unreachability to surface
    /// from the send path. `None` means the lookup produced nothing to
    /// attempt.
    pub(super) fn new(
        has_ipv4: bool,
        has_ipv6: bool,
        addresses: impl IntoIterator<Item = NetworkIpAddress>,
    ) -> Option<Self> {
        let reachable = |address: &IpAddress| match address {
            IpAddress::Ipv4(_) => has_ipv4,
            IpAddress::Ipv6(_) => has_ipv6,
        };
        let mut addresses: ArrayVec<IpAddress, MAX_CONNECT_CANDIDATES> = addresses
            .into_iter()
            .take(MAX_CONNECT_CANDIDATES)
            .map(map_network_ip_address)
            .collect();
        if addresses.iter().any(reachable) {
            addresses.retain(|address| reachable(address));
        }
        (!addresses.is_empty()).then_some(Self { addresses })
    }

    /// The single candidate a numeric host spells out. No resolver ran,
    /// so there is no other answer to fall back to.
    pub(super) fn literal(address: IpAddress) -> Self {
        let mut addresses = ArrayVec::new();
        addresses.push(address);
        Self { addresses }
    }

    /// The ordered candidate list. Attempt order is the contract this
    /// type exists to carry, so the tests assert on it directly.
    #[cfg(test)]
    pub(super) fn addresses(&self) -> &[IpAddress] {
        &self.addresses
    }

    /// Splits the walk into the attempts whose failure falls through to
    /// the next address and the final attempt whose result is the
    /// reported one.
    fn split_last(&self) -> (&[IpAddress], IpAddress) {
        let (last, rest) = self
            .addresses
            .split_last()
            .unwrap_or_else(|| panic!("connect candidates are never empty"));
        (rest, *last)
    }
}

/// Classifies a failed attempt as belonging to the address it targeted
/// rather than to the request as a whole.
///
/// Sequential fallback only applies to the first kind: a refusal, an
/// unreachable report, a connect that never completed, or a family this
/// link cannot source says the candidate is unusable and the next one
/// deserves whatever deadline remains. Anything else — an unknown
/// socket, an exhausted local port range, an unsupported request —
/// fails identically against every candidate, so it ends the walk at
/// once instead of being retried once per answer.
pub(super) trait AddressAttemptError {
    fn is_address_specific(&self) -> bool;
}

impl AddressAttemptError for TcpError {
    fn is_address_specific(&self) -> bool {
        matches!(
            self.detail,
            NetworkErrorDetail::NetworkConfigurationTimeout
                | NetworkErrorDetail::NetworkServiceUnavailable
                | NetworkErrorDetail::TcpConnectTimeout
                | NetworkErrorDetail::TcpClosedDuringConnect
                | NetworkErrorDetail::TcpRemotePortUnreachable
                | NetworkErrorDetail::TcpRemoteNetworkUnreachable
                | NetworkErrorDetail::TcpRemoteHostUnreachable
                | NetworkErrorDetail::TcpRemoteProtocolUnreachable
                | NetworkErrorDetail::TcpRemoteCommunicationProhibited
        )
    }
}

impl AddressAttemptError for UdpError {
    fn is_address_specific(&self) -> bool {
        matches!(
            self.detail,
            NetworkErrorDetail::NetworkConfigurationTimeout
                | NetworkErrorDetail::NetworkServiceUnavailable
                | NetworkErrorDetail::UdpRemotePortUnreachable
                | NetworkErrorDetail::UdpRemoteNetworkUnreachable
                | NetworkErrorDetail::UdpRemoteHostUnreachable
                | NetworkErrorDetail::UdpRemoteProtocolUnreachable
                | NetworkErrorDetail::UdpRemoteCommunicationProhibited
        )
    }
}

/// Attempts `connect` against each candidate in resolver order until
/// one succeeds, the way RFC 6555 prescribes for a name that resolves
/// in both families.
///
/// The walk is strictly sequential — no attempt starts before the
/// previous one finished — and every attempt draws on the same caller
/// deadline, so falling back never multiplies the timeout the caller
/// asked for. The last candidate's result is returned verbatim, which
/// makes the reported error the one from the final attempt whenever
/// every candidate failed.
///
/// The walk holds no shared state: the candidate list belongs to the
/// calling task, and each attempt independently routes to whichever
/// shard owns the local port it ends up using, so two processors
/// resolving the same name never contend here.
pub(super) async fn attempt_each_address<Handle, Error, Attempt, Fut>(
    candidates: &ConnectCandidates,
    mut connect: Attempt,
) -> Result<Handle, Error>
where
    Error: AddressAttemptError + core::fmt::Display,
    Attempt: FnMut(IpAddress) -> Fut,
    Fut: Future<Output = Result<Handle, Error>>,
{
    let (fallback, last) = candidates.split_last();
    for destination in fallback {
        match connect(*destination).await {
            Ok(handle) => return Ok(handle),
            Err(error) if error.is_address_specific() => {
                tracing::debug!(
                    %error,
                    ?destination,
                    "connect attempt failed, trying the next resolved address"
                );
            }
            Err(error) => return Err(error),
        }
    }
    connect(last).await
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;

    use alloc::vec::Vec;
    use futures_lite::future::block_on;
    use helios_netstack::{IpAddress, Ipv4Address, Ipv6Address};

    use super::{ConnectCandidates, attempt_each_address};
    use crate::{NetworkErrorDetail, NetworkIpAddress, TcpError, TcpErrorKind};

    fn ipv4(last_octet: u8) -> IpAddress {
        IpAddress::Ipv4(Ipv4Address::new([192, 0, 2, last_octet]))
    }

    fn ipv6(last_octet: u8) -> IpAddress {
        IpAddress::Ipv6(Ipv6Address::new([
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, last_octet,
        ]))
    }

    fn network(address: IpAddress) -> NetworkIpAddress {
        super::map_ip_address(address)
    }

    fn refused() -> TcpError {
        TcpError {
            kind: TcpErrorKind::Unavailable,
            detail: NetworkErrorDetail::TcpClosedDuringConnect,
        }
    }

    fn timed_out() -> TcpError {
        TcpError {
            kind: TcpErrorKind::Timeout,
            detail: NetworkErrorDetail::TcpConnectTimeout,
        }
    }

    #[test]
    fn dual_family_answer_keeps_every_reachable_address_in_resolver_order() {
        let candidates = ConnectCandidates::new(
            true,
            true,
            [network(ipv6(2)), network(ipv4(2)), network(ipv4(3))],
        )
        .expect("a dual-family answer resolves");

        assert_eq!(
            candidates.addresses(),
            [ipv6(2), ipv4(2), ipv4(3)].as_slice()
        );
    }

    #[test]
    fn unconfigured_family_is_dropped_while_a_configured_one_answers() {
        let candidates = ConnectCandidates::new(true, false, [network(ipv6(2)), network(ipv4(2))])
            .expect("the IPv4 answer resolves");

        assert_eq!(candidates.addresses(), [ipv4(2)].as_slice());
    }

    #[test]
    fn answer_in_no_configured_family_keeps_resolver_order() {
        let candidates = ConnectCandidates::new(false, false, [network(ipv6(2)), network(ipv4(2))])
            .expect("an unconfigured link still has candidates to attempt");

        assert_eq!(candidates.addresses(), [ipv6(2), ipv4(2)].as_slice());
    }

    #[test]
    fn empty_answer_has_no_candidates() {
        assert!(ConnectCandidates::new(true, true, []).is_none());
    }

    #[test]
    fn refused_ipv6_candidate_falls_back_to_the_ipv4_answer() {
        let candidates = ConnectCandidates::new(true, true, [network(ipv6(2)), network(ipv4(2))])
            .expect("a dual-family answer resolves");
        let attempted = RefCell::new(Vec::new());

        let stream = block_on(attempt_each_address(&candidates, |destination| {
            attempted.borrow_mut().push(destination);
            core::future::ready(match destination {
                IpAddress::Ipv6(_) => Err(refused()),
                IpAddress::Ipv4(_) => Ok(7u32),
            })
        }))
        .expect("the IPv4 candidate connects after the IPv6 one is refused");

        assert_eq!(stream, 7);
        assert_eq!(
            attempted.into_inner().as_slice(),
            [ipv6(2), ipv4(2)].as_slice()
        );
    }

    #[test]
    fn every_candidate_failing_reports_the_last_attempt_error() {
        let candidates = ConnectCandidates::new(true, true, [network(ipv6(2)), network(ipv4(2))])
            .expect("a dual-family answer resolves");
        let attempted = RefCell::new(Vec::new());

        let error = block_on(attempt_each_address(&candidates, |destination| {
            attempted.borrow_mut().push(destination);
            core::future::ready(Err::<u32, _>(match destination {
                IpAddress::Ipv6(_) => refused(),
                IpAddress::Ipv4(_) => timed_out(),
            }))
        }))
        .expect_err("no candidate connects");

        assert_eq!(error.detail, NetworkErrorDetail::TcpConnectTimeout);
        assert_eq!(
            attempted.into_inner().as_slice(),
            [ipv6(2), ipv4(2)].as_slice()
        );
    }

    #[test]
    fn failure_that_every_address_would_share_ends_the_walk() {
        let candidates = ConnectCandidates::new(true, true, [network(ipv6(2)), network(ipv4(2))])
            .expect("a dual-family answer resolves");
        let attempted = RefCell::new(Vec::new());

        let error = block_on(attempt_each_address(&candidates, |destination| {
            attempted.borrow_mut().push(destination);
            core::future::ready(Err::<u32, _>(TcpError {
                kind: TcpErrorKind::Unavailable,
                detail: NetworkErrorDetail::TcpNoEphemeralPorts,
            }))
        }))
        .expect_err("a local port shortage fails the request");

        assert_eq!(error.detail, NetworkErrorDetail::TcpNoEphemeralPorts);
        assert_eq!(attempted.into_inner().as_slice(), [ipv6(2)].as_slice());
    }
}
