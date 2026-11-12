//! IPv6 interface addresses and the lifetimes an autoconfigured one
//! carries (RFC 4862 §5.5.3 and §5.5.4).
//!
//! A manually configured or link-local address has no lifetime, so
//! [`Ipv6AddressOrigin`] gives it none rather than a sentinel value; an
//! autoconfigured address carries its [`AddressLifetimes`].
//!
//! Concurrency: plain values with no interior mutability. A stack owns its
//! table, and the kernel's control plane keeps copies behind a snapshot
//! cell.

use crate::{Ipv6Cidr, StackInstant};

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// The Valid Lifetime that bounds what an unauthenticated advertisement
/// can do to an address that already exists: two hours (RFC 4862
/// §5.5.3 (e)).
pub const TWO_HOURS_SECONDS: u32 = 2 * 60 * 60;

/// The Valid or Preferred Lifetime field value that means "forever"
/// (RFC 4861 §4.6.2).
const INFINITE_LIFETIME_SECONDS: u32 = u32::MAX;

/// The instant a lifetime advertised in seconds runs out, or `None` for
/// the infinite lifetime (RFC 4861 §4.6.2).
pub(crate) fn lifetime_deadline(now: StackInstant, seconds: u32) -> Option<StackInstant> {
    (seconds != INFINITE_LIFETIME_SECONDS).then(|| {
        StackInstant::from_nanos(
            now.nanos()
                .saturating_add(u64::from(seconds) * NANOS_PER_SECOND),
        )
    })
}

/// How long something lasts: until an instant, or forever.
///
/// Ordered by when it ends, so `Infinite` is greater than every
/// `Until`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lifetime {
    Until(StackInstant),
    Infinite,
}

impl Lifetime {
    /// The lifetime a Prefix Information option's field announces when
    /// received at `now`.
    pub fn from_seconds(now: StackInstant, seconds: u32) -> Self {
        lifetime_deadline(now, seconds).map_or(Self::Infinite, Self::Until)
    }

    /// Whether the lifetime has ended at `now`.
    pub fn has_run_out(self, now: StackInstant) -> bool {
        matches!(self, Self::Until(deadline) if deadline <= now)
    }
}

/// Whether an autoconfigured address may start new communication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preference {
    /// Preferred until the lifetime ends.
    Preferred(Lifetime),
    /// The preferred lifetime has ended. Still a valid address (it keeps
    /// answering), but source-address selection avoids it while another
    /// is available (RFC 4862 §5.5.4).
    Deprecated,
}

/// The two lifetimes of an autoconfigured address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddressLifetimes {
    preference: Preference,
    valid: Lifetime,
}

impl AddressLifetimes {
    /// The lifetimes a new address takes from the Prefix Information
    /// option that created it (RFC 4862 §5.5.3 (d)).
    pub fn from_advertised(now: StackInstant, preferred_seconds: u32, valid_seconds: u32) -> Self {
        Self {
            preference: Self::preference_at(now, preferred_seconds),
            valid: Lifetime::from_seconds(now, valid_seconds),
        }
    }

    fn preference_at(now: StackInstant, preferred_seconds: u32) -> Preference {
        match Lifetime::from_seconds(now, preferred_seconds) {
            lifetime if lifetime.has_run_out(now) => Preference::Deprecated,
            lifetime => Preference::Preferred(lifetime),
        }
    }

    pub const fn preference(&self) -> Preference {
        self.preference
    }

    pub const fn valid(&self) -> Lifetime {
        self.valid
    }

    /// Applies a Prefix Information option for an address already
    /// configured by autoconfiguration (RFC 4862 §5.5.3 (e)).
    ///
    /// The preferred lifetime is always reset to the advertised one. The
    /// valid lifetime follows the two-hour rule: it is raised to the
    /// advertised one when that exceeds two hours or what remains, left
    /// alone when what remains is at most two hours, and otherwise cut
    /// to two hours, so a forged advertisement with short lifetimes is
    /// cancelled by the next legitimate one before it takes effect. The
    /// authenticated-advertisement exception does not apply: no Secure
    /// Neighbor Discovery support exists.
    pub fn refresh(&mut self, now: StackInstant, preferred_seconds: u32, valid_seconds: u32) {
        self.preference = Self::preference_at(now, preferred_seconds);
        let advertised = Lifetime::from_seconds(now, valid_seconds);
        let two_hours = Lifetime::from_seconds(now, TWO_HOURS_SECONDS);
        self.valid = if advertised > two_hours || advertised > self.valid {
            advertised
        } else if self.valid <= two_hours {
            self.valid
        } else {
            two_hours
        };
    }

    /// Advances the lifetimes to `now`: `false` once the address is no
    /// longer valid, otherwise `true` with a preferred lifetime that has
    /// ended turned into deprecation.
    fn advance(&mut self, now: StackInstant) -> bool {
        if self.valid.has_run_out(now) {
            return false;
        }
        if let Preference::Preferred(lifetime) = self.preference
            && lifetime.has_run_out(now)
        {
            self.preference = Preference::Deprecated;
        }
        true
    }
}

/// Where an interface address came from, which decides whether it has
/// lifetimes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ipv6AddressOrigin {
    /// Configured by a local decision (the link-local address, a manual
    /// address): never expires and is never deprecated.
    Configured,
    /// Derived from an advertised prefix by stateless autoconfiguration.
    Autoconfigured(AddressLifetimes),
}

/// One address of the interface and its origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv6AddressEntry {
    cidr: Ipv6Cidr,
    origin: Ipv6AddressOrigin,
}

impl Ipv6AddressEntry {
    pub const fn configured(cidr: Ipv6Cidr) -> Self {
        Self {
            cidr,
            origin: Ipv6AddressOrigin::Configured,
        }
    }

    pub const fn autoconfigured(cidr: Ipv6Cidr, lifetimes: AddressLifetimes) -> Self {
        Self {
            cidr,
            origin: Ipv6AddressOrigin::Autoconfigured(lifetimes),
        }
    }

    pub const fn cidr(&self) -> Ipv6Cidr {
        self.cidr
    }

    pub const fn origin(&self) -> Ipv6AddressOrigin {
        self.origin
    }

    pub(crate) fn lifetimes_mut(&mut self) -> Option<&mut AddressLifetimes> {
        match &mut self.origin {
            Ipv6AddressOrigin::Configured => None,
            Ipv6AddressOrigin::Autoconfigured(lifetimes) => Some(lifetimes),
        }
    }

    /// Whether source-address selection should avoid the address while
    /// another is available.
    pub const fn is_deprecated(&self) -> bool {
        matches!(
            self.origin,
            Ipv6AddressOrigin::Autoconfigured(AddressLifetimes {
                preference: Preference::Deprecated,
                ..
            })
        )
    }

    /// Advances the address to `now`; `false` once it is invalid and the
    /// table must drop it (RFC 4862 §5.5.4).
    pub(crate) fn advance(&mut self, now: StackInstant) -> bool {
        match &mut self.origin {
            Ipv6AddressOrigin::Configured => true,
            Ipv6AddressOrigin::Autoconfigured(lifetimes) => lifetimes.advance(now),
        }
    }
}
